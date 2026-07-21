//! Per-group gain, DSP, duck, SRC, and output summing. Everything here runs
//! on the RT mixer thread (`engine::runtime`) — no allocation, no locks, no
//! blocking.

use crate::channel::ChannelMatrix;
use crate::dsp::{db_to_linear, DspChain, DspParam, DspStage, Limiter};
use crate::resample::Src;
use crate::sample::{
    ChannelLayout, DomainError, DuckSpec, Format, Gain, GroupId, GroupSpec, OutputId,
    ResampleRatio, Topology,
};
use crate::smoothing::Smoothed;
use crate::spatial::{HrirSet, Spatializer};

/// Alternative N->2 render stage beside [`ChannelMatrix`], selected per-group
/// (spatial-audio.md). `Spatial` is never an identity transform.
pub enum Render {
    Matrix(ChannelMatrix),
    Spatial(Spatializer),
}

impl Render {
    /// Shared by `build_group` (initial construction) and
    /// `engine::EngineHandle::apply_spatial` (live toggle, off-thread
    /// rebuild) — see spatial-audio.md's "Render::build" decision. Owns the
    /// fallback rule: `spatial` only takes effect when `to` is stereo.
    pub fn build(spatial: bool, from: Format, to: Format, max_block_frames: usize) -> Render {
        if spatial && to.layout == ChannelLayout::STEREO {
            let hrirs = HrirSet::embedded(to.sample_rate);
            Render::Spatial(Spatializer::new(from.layout, &hrirs, max_block_frames))
        } else {
            Render::Matrix(ChannelMatrix::new(from.layout, to.layout))
        }
    }

    /// Matrix-identity only; `Spatial` is always a real transform.
    pub fn is_identity(&self) -> bool {
        match self {
            Render::Matrix(m) => m.is_identity(),
            Render::Spatial(_) => false,
        }
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        match self {
            Render::Matrix(m) => m.process(input, output),
            Render::Spatial(s) => s.process(input, output),
        }
    }
}

/// Manual impl: `Spatializer` holds `Arc<dyn rustfft::Fft<f32>>`, which has
/// no `Debug` — same rationale as `DspChain`'s manual impl in `dsp.rs`.
impl std::fmt::Debug for Render {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Render::Matrix(_) => f.write_str("Render::Matrix"),
            Render::Spatial(_) => f.write_str("Render::Spatial"),
        }
    }
}

/// What a `SwapChain`/`SwapRender` apply hands back for the caller to drop
/// off-RT (notes §7) — widened from a bare `Box<DspChain>` (dsp-pipeline.md)
/// once `SwapRender` needed the same swap-and-retire path.
#[derive(Debug)]
pub enum Retired {
    Chain(Box<DspChain>),
    Render(Box<Render>),
}

#[derive(Debug)]
pub enum MixerCommand {
    SetGroupGain(GroupId, Gain),
    SetMaster(Gain),
    SetFollowMaster(GroupId, bool),
    /// Fans out to every group's `Src` feeding that output — the drift loop
    /// measures fill per output, but each group has its own resampler.
    SetOutputRatio(OutputId, ResampleRatio),
    /// Global output-stage kill, independent of `follow_master` — silences
    /// every group's contribution to every output. Gain/master smoothers
    /// keep running so unmute resumes at the same value with no re-ramp.
    SetMuted(bool),
    /// Param tweak or bypass toggle on a pre-allocated stage — smoothed
    /// internally, never a stepped change (notes §8).
    SetDspParam {
        group: GroupId,
        stage: usize,
        param: DspParam,
    },
    SetDspBypass {
        group: GroupId,
        stage: usize,
        bypassed: bool,
    },
    /// Reconfigures (or clears) a group's duck sidechain. Command-path, not
    /// `Structural`: unlike adding/removing a DSP stage, this never resizes
    /// any RT-owned buffer — just swaps a few scalar fields.
    SetDuck {
        group: GroupId,
        duck: Option<DuckSpec>,
    },
    /// Add/remove-stage change, funnel-classified `Structural` but
    /// implemented as an RT-safe pointer swap (dsp-pipeline.md's revision):
    /// the new chain is built off-thread by the caller, moved in here, and
    /// the retired chain is hg  anded back for the caller to drop off-thread.
    SwapChain {
        group: GroupId,
        chain: Box<DspChain>,
    },
    /// Live spatial-audio toggle (spatial-audio.md): the new `Render` is
    /// built off-thread by the caller (`engine::EngineHandle::apply_spatial`)
    /// against the group's current topology, moved in here, and the retired
    /// `Render` is handed back for the caller to drop off-thread. No epoch
    /// field: unlike `SwapChain`, this touches no stage indices a queued
    /// command could go stale against, and the supervisor is the sole
    /// producer, so a stale swap is simply last-write-wins, harmless.
    SwapRender {
        group: GroupId,
        render: Box<Render>,
    },
}

const GAIN_TIME_CONSTANT_S: f32 = 0.01; // 10ms — inaudible as a ramp, kills zipper noise
/// Envelope-follower detection time, fixed (not user-configurable — `DuckSpec`
/// only exposes the *reaction* attack/release, see `DuckTargetGain`). Fast
/// enough to catch onset without being noise-jittery.
const ENV_FOLLOWER_ATTACK_MS: f32 = 5.0;
const ENV_FOLLOWER_RELEASE_MS: f32 = 100.0;
/// Always-on per-output headroom limiter ceiling (L1 capability 4: shared
/// outputs must never clip). Not user-configurable — this is a safety net,
/// not a mix decision; per-group limiting is what `DspSpec::Limiter` is for.
const OUTPUT_HEADROOM_CEILING_DB: f32 = 0.0;

fn one_pole_coeff(time_constant_s: f32, sample_rate: u32) -> f32 {
    (-1.0 / (time_constant_s * sample_rate as f32)).exp()
}

/// Mixer-level cross-group sidechain follower (L2: not a `DspStage` — it
/// reads one group's post-chain signal to drive gain on a *different*
/// group). One instance per group, always running: any group can become a
/// duck trigger via a live `SetDuck` command, and pre-allocating a follower
/// only for groups referenced at construction would mean growing state at
/// runtime (an RT allocation) the first time a new trigger relationship is
/// configured.
struct EnvFollower {
    env: f32,
    attack: f32,
    release: f32,
}

impl EnvFollower {
    fn new(sample_rate: u32) -> EnvFollower {
        EnvFollower {
            env: 0.0,
            attack: one_pole_coeff(ENV_FOLLOWER_ATTACK_MS / 1000.0, sample_rate),
            release: one_pole_coeff(ENV_FOLLOWER_RELEASE_MS / 1000.0, sample_rate),
        }
    }

    /// Returns the envelope, in dBFS, at the end of `buf` — the per-frame
    /// tracking state carries across calls, but only the final value is
    /// reported (L3 interaction A: one reading per group per tick).
    ///
    /// One update per FRAME (peak across `channels`), not per interleaved
    /// element — `attack`/`release` assume one call per sample-period;
    /// stepping per element would make detection `channels` times faster
    /// than intended (review finding, dsp-pipeline P5).
    fn process_block(&mut self, buf: &[f32], channels: usize) -> f32 {
        let frame_count = buf.len() / channels;
        for f in 0..frame_count {
            let start = f * channels;
            let a = buf[start..start + channels]
                .iter()
                .fold(0.0f32, |acc, &s| acc.max(s.abs()));
            let c = if a > self.env { self.attack } else { self.release };
            self.env = a + c * (self.env - a);
        }
        20.0 * self.env.max(1.0e-6).log10()
    }
}

/// Smoothed linear gain applied to a duck target's buffer — asymmetric
/// attack/release from the group's own `DuckSpec` (the knobs a user tunes),
/// distinct from `EnvFollower`'s fixed detection timing.
struct DuckTargetGain {
    current: f32,
    attack_coeff: f32,
    release_coeff: f32,
}

impl DuckTargetGain {
    fn new(sample_rate: u32, attack_ms: f32, release_ms: f32) -> DuckTargetGain {
        DuckTargetGain {
            current: 1.0,
            attack_coeff: one_pole_coeff(attack_ms / 1000.0, sample_rate),
            release_coeff: one_pole_coeff(release_ms / 1000.0, sample_rate),
        }
    }

    /// Ramps per-frame toward `target` (attack when reducing, release when
    /// recovering) and applies the running gain to every channel in that
    /// frame. One ramp step per frame, not per interleaved element — same
    /// rate bug class as `EnvFollower::process_block` (review finding,
    /// dsp-pipeline P5): `attack_coeff`/`release_coeff` assume one call per
    /// sample-period, so a per-element step would make the group's
    /// configured `attack_ms`/`release_ms` `channels` times faster.
    fn apply(&mut self, buf: &mut [f32], target: f32, channels: usize) {
        let frame_count = buf.len() / channels;
        for f in 0..frame_count {
            let coeff = if target < self.current {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.current = target + coeff * (self.current - target);
            let start = f * channels;
            for s in &mut buf[start..start + channels] {
                *s *= self.current;
            }
        }
    }

    /// Current reduction, in dB (0 = no reduction).
    fn depth_db(&self) -> f32 {
        -20.0 * self.current.max(1.0e-6).log10()
    }
}

struct GroupState {
    id: GroupId,
    output: OutputId,
    channels: usize,
    follow_master: bool,
    gain: Smoothed,
    // Each group carries its own copy of the master ramp rather than sharing
    // one Smoothed across groups: push_group() advances it sample-by-sample,
    // and multiple groups following master in the same tick would otherwise
    // advance shared state multiple times, converging faster than intended.
    // Independent copies at the same time constant drift from each other by
    // a fraction of a sample over a ~10ms ramp — inaudible.
    master: Smoothed,
    /// Converts source layout -> output layout, between the DSP chain and
    /// SRC — either the plain channel matrix or a binaural spatializer.
    /// Boxed at construction (off-RT) so a live `SwapRender` command is a
    /// pointer move, never an RT allocation (notes §7). Skipped entirely
    /// when `is_identity()` (same layout in and out, `Matrix` only).
    render: Box<Render>,
    /// Render output, pre-SRC. Capacity: `max_block_frames * output channels`.
    matrixed: Vec<f32>,
    src: Src,
    /// Gain-applied, then DSP-chain- and duck-processed samples, pre-matrix.
    /// Capacity: `max_block_frames * channels`. Source layout throughout
    /// (notes §17: DSP/duck stay at source layout, not output layout).
    scratch: Vec<f32>,
    /// Valid interleaved sample count in `scratch` this tick — set by
    /// `push_group`, consumed by `mix_tick`'s duck/matrix/SRC/sum phases.
    valid_len: usize,
    /// SRC output. Capacity is generous (see Mixer::new) so a full block's
    /// worth of input is always consumed in one `push_group` call.
    resampled: Vec<f32>,
    input_format: Format,
    /// Boxed at construction (off-RT) so a live `SwapChain` command is a
    /// pointer move, never an RT allocation (notes §7).
    dsp_chain: Box<DspChain>,
    duck: Option<DuckSpec>,
    duck_gain: Option<DuckTargetGain>,
    env_follower: EnvFollower,
    /// This group's own envelope, computed fresh every tick before any
    /// target's duck gain is applied — see `Mixer::mix_tick` phase 1.
    last_env_db: f32,
}

struct OutputState {
    id: OutputId,
    accum: Vec<f32>,
    /// High-water mark of valid samples in `accum` since the last `take_output`.
    filled: usize,
    format: Format,
    /// Always-on headroom limiter (L1 capability 4) — runs after every
    /// group has summed into `accum`, before the render thread reads it.
    limiter: Limiter,
}

pub struct Mixer {
    max_block_frames: usize,
    groups: Vec<GroupState>,
    outputs: Vec<OutputState>,
    muted: bool,
}

/// A `Format`'s `layout` must describe exactly as many speakers as
/// `channels` says — the invariant a raw device mix-format read or a
/// hand-built test `Format` could otherwise violate silently.
fn validate_layout(fmt: &Format) -> Result<(), DomainError> {
    if fmt.layout.count() == fmt.channels {
        Ok(())
    } else {
        Err(DomainError::InvalidLayout {
            channels: fmt.channels,
            layout_count: fmt.layout.count(),
        })
    }
}

fn build_group(
    gspec: &GroupSpec,
    topology: &Topology,
    max_block_frames: usize,
) -> Result<GroupState, DomainError> {
    let out_spec = topology
        .outputs
        .iter()
        .find(|o| o.id == gspec.output)
        .ok_or(DomainError::DanglingOutputRef {
            group: gspec.id,
            output: gspec.output,
        })?;

    if let Some(duck) = &gspec.duck {
        if !topology.groups.iter().any(|gg| gg.id == duck.trigger) {
            return Err(DomainError::DanglingDuckTrigger {
                group: gspec.id,
                trigger: duck.trigger,
            });
        }
    }

    validate_layout(&gspec.input_format)?;
    validate_layout(&out_spec.format)?;

    let channels = gspec.input_format.channels as usize;
    let out_channels = out_spec.format.channels as usize;
    let sample_rate = gspec.input_format.sample_rate;

    let render = Box::new(Render::build(
        gspec.spatial,
        gspec.input_format,
        out_spec.format,
        max_block_frames,
    ));

    // SRC runs post-matrix: both sides already share `out_channels` and
    // `out_spec.format.layout`, so only the sample rate differs between
    // them. This is why `Src::new`'s equal-channel-count check can no
    // longer be hit from a public path — the matrix guarantees it upstream.
    let src_from = Format {
        sample_rate,
        channels: out_spec.format.channels,
        layout: out_spec.format.layout,
    };
    let src = Src::new(src_from, out_spec.format, max_block_frames)?;

    let dsp_chain = Box::new(DspChain::new(&gspec.dsp, gspec.input_format, max_block_frames)?);
    let duck_gain = gspec
        .duck
        .map(|d| DuckTargetGain::new(sample_rate, d.attack_ms, d.release_ms));

    Ok(GroupState {
        id: gspec.id,
        output: gspec.output,
        channels,
        follow_master: gspec.follow_master,
        gain: Smoothed::new(gspec.gain.value(), sample_rate, GAIN_TIME_CONSTANT_S),
        master: Smoothed::new(topology.master.value(), sample_rate, GAIN_TIME_CONSTANT_S),
        render,
        matrixed: vec![0.0; max_block_frames * out_channels],
        src,
        scratch: vec![0.0; max_block_frames * channels],
        valid_len: 0,
        // 8x covers every realistic device sample-rate ratio (worst case
        // in practice is well under 2x) with no per-tick sizing math. Sized
        // by out_channels, not the source channel count — undersizing here
        // for an upmix would only trip the debug_assert below in tests.
        resampled: vec![0.0; max_block_frames * out_channels * 8],
        input_format: gspec.input_format,
        dsp_chain,
        duck: gspec.duck,
        duck_gain,
        env_follower: EnvFollower::new(sample_rate),
        last_env_db: f32::NEG_INFINITY,
    })
}

impl Mixer {
    pub fn new(topology: &Topology, max_block_frames: usize) -> Result<Mixer, DomainError> {
        let max_block_frames = max_block_frames.max(1);

        let mut outputs = Vec::with_capacity(topology.outputs.len());
        for spec in &topology.outputs {
            let cap = max_block_frames * spec.format.channels as usize;
            outputs.push(OutputState {
                id: spec.id,
                accum: vec![0.0; cap],
                filled: 0,
                format: spec.format,
                limiter: Limiter::new(OUTPUT_HEADROOM_CEILING_DB, spec.format, max_block_frames),
            });
        }

        let mut groups = Vec::with_capacity(topology.groups.len());
        for gspec in &topology.groups {
            groups.push(build_group(gspec, topology, max_block_frames)?);
        }

        Ok(Mixer {
            max_block_frames,
            groups,
            outputs,
            muted: false,
        })
    }

    /// Unknown ids are dropped silently: the command ring may still carry a
    /// stale-epoch command past the point its group/output was torn down.
    /// Returns the retired chain/render on `SwapChain`/`SwapRender` — the
    /// caller drops it off-RT (notes §7); every other variant returns `None`.
    pub fn apply(&mut self, cmd: MixerCommand) -> Option<Retired> {
        match cmd {
            MixerCommand::SetGroupGain(id, gain) => {
                if let Some(g) = self.groups.iter_mut().find(|g| g.id == id) {
                    g.gain.set_target(gain.value());
                }
                None
            }
            MixerCommand::SetMaster(gain) => {
                for g in self.groups.iter_mut() {
                    g.master.set_target(gain.value());
                }
                None
            }
            MixerCommand::SetFollowMaster(id, follow) => {
                if let Some(g) = self.groups.iter_mut().find(|g| g.id == id) {
                    g.follow_master = follow;
                }
                None
            }
            MixerCommand::SetOutputRatio(output_id, ratio) => {
                for g in self.groups.iter_mut().filter(|g| g.output == output_id) {
                    g.src.set_ratio(ratio);
                }
                None
            }
            MixerCommand::SetMuted(muted) => {
                self.muted = muted;
                None
            }
            MixerCommand::SetDspParam { group, stage, param } => {
                if let Some(g) = self.groups.iter_mut().find(|g| g.id == group) {
                    g.dsp_chain.set_param(stage, param);
                }
                None
            }
            MixerCommand::SetDspBypass { group, stage, bypassed } => {
                if let Some(g) = self.groups.iter_mut().find(|g| g.id == group) {
                    g.dsp_chain.set_bypass(stage, bypassed);
                }
                None
            }
            MixerCommand::SetDuck { group, duck } => {
                if let Some(g) = self.groups.iter_mut().find(|g| g.id == group) {
                    g.duck_gain = duck.map(|d| {
                        DuckTargetGain::new(g.input_format.sample_rate, d.attack_ms, d.release_ms)
                    });
                    g.duck = duck;
                }
                None
            }
            MixerCommand::SwapChain { group, chain } => self
                .groups
                .iter_mut()
                .find(|g| g.id == group)
                .map(|g| Retired::Chain(std::mem::replace(&mut g.dsp_chain, chain))),
            MixerCommand::SwapRender { group, render } => self
                .groups
                .iter_mut()
                .find(|g| g.id == group)
                .map(|g| Retired::Render(std::mem::replace(&mut g.render, render))),
        }
    }

    /// Applies gain (and master, if bound) per-sample, then the group's DSP
    /// chain — both at source layout. Duck, matrix, SRC, and output summing
    /// happen later in `mix_tick`, once every group has reached this point
    /// (L3 interaction A: duck needs every trigger's post-chain signal
    /// before any target is touched). `frames` is interleaved and truncated
    /// to `max_block_frames`.
    pub fn push_group(&mut self, group: GroupId, frames: &[f32]) {
        let Some(idx) = self.groups.iter().position(|g| g.id == group) else {
            return;
        };
        let g = &mut self.groups[idx];

        debug_assert_eq!(frames.len() % g.channels, 0);
        let frame_count = (frames.len() / g.channels).min(self.max_block_frames);
        let n = frame_count * g.channels;

        for i in 0..frame_count {
            let master = if g.follow_master {
                g.master.next()
            } else {
                1.0
            };
            let gain = g.gain.next() * master;
            for c in 0..g.channels {
                let s = i * g.channels + c;
                g.scratch[s] = frames[s] * gain;
            }
        }

        g.dsp_chain.process(&mut g.scratch[..n], g.input_format);
        g.valid_len = n;
    }

    /// Runs once per tick, after every group's `push_group` call and before
    /// `take_output`. Order per L3 interaction A: every trigger's post-chain
    /// envelope first (phase 1), then duck gain reduction on every target
    /// (phase 2) — no target's matrix/SRC/sum has run yet, so there's no
    /// feedback regardless of how the duck config graph is shaped — then
    /// matrix -> SRC -> sum per group (phase 3), then the always-on
    /// per-output headroom limiter (phase 4).
    pub fn mix_tick(&mut self) {
        for i in 0..self.groups.len() {
            let g = &mut self.groups[i];
            let n = g.valid_len;
            g.last_env_db = g.env_follower.process_block(&g.scratch[..n], g.channels);
        }

        for i in 0..self.groups.len() {
            let Some(duck) = self.groups[i].duck else {
                continue;
            };
            let trigger_env = self
                .groups
                .iter()
                .position(|gr| gr.id == duck.trigger)
                .map(|ti| self.groups[ti].last_env_db)
                .unwrap_or(f32::NEG_INFINITY);
            let reduction = if trigger_env > duck.threshold_db {
                db_to_linear(-duck.amount_db)
            } else {
                1.0
            };
            let n = self.groups[i].valid_len;
            let g = &mut self.groups[i];
            if let Some(dg) = g.duck_gain.as_mut() {
                dg.apply(&mut g.scratch[..n], reduction, g.channels);
            }
        }

        for i in 0..self.groups.len() {
            let n = self.groups[i].valid_len;
            let g = &mut self.groups[i];

            // Render stage: skipped entirely (no copy) when the matrix is
            // identity — the common case should pay nothing extra. `Spatial`
            // is never identity, so it always goes through `render.process`.
            let (matrix_input, matrix_len): (&[f32], usize) = if g.render.is_identity() {
                (&g.scratch[..n], n)
            } else {
                let len = g.render.process(&g.scratch[..n], &mut g.matrixed);
                (&g.matrixed[..len], len)
            };

            let progress = g.src.process(matrix_input, &mut g.resampled);
            debug_assert_eq!(
                progress.consumed, matrix_len,
                "resampled scratch undersized for one block"
            );

            // Output-stage kill: gain/chain/duck/matrix/SRC still ran above
            // (smoothers and resampler state stay warm), only the write into
            // the shared output accumulator is skipped — unmute resumes with
            // no re-ramp or glitch.
            if self.muted {
                continue;
            }

            let output = g.output;
            let produced = progress.produced;
            let Some(out) = self.outputs.iter_mut().find(|o| o.id == output) else {
                continue;
            };
            let write_len = produced.min(out.accum.len());
            for s in 0..write_len {
                out.accum[s] += g.resampled[s];
            }
            out.filled = out.filled.max(write_len);
        }

        for out in self.outputs.iter_mut() {
            let filled = out.filled;
            out.limiter.process(&mut out.accum[..filled], out.format);
        }
    }

    /// Copies this tick's summed output into `buf` (up to its length) and
    /// clears the accumulator for the next tick. Returns samples written;
    /// short reads mean underrun — the caller (render.rs) pads with silence.
    pub fn take_output(&mut self, output: OutputId, buf: &mut [f32]) -> usize {
        let Some(out) = self.outputs.iter_mut().find(|o| o.id == output) else {
            return 0;
        };
        let n = out.filled.min(buf.len());
        buf[..n].copy_from_slice(&out.accum[..n]);
        for s in out.accum[..out.filled].iter_mut() {
            *s = 0.0;
        }
        out.filled = 0;
        n
    }

    /// Telemetry accessor (`EngineStats::limiter_engaged`): whether this
    /// output's always-on headroom limiter reduced gain during the last
    /// `mix_tick`. Unknown id: `false`, same "nothing to report" convention
    /// as every other unknown-id lookup here.
    pub fn output_limiter_engaged(&self, output: OutputId) -> bool {
        self.outputs
            .iter()
            .find(|o| o.id == output)
            .map(|o| o.limiter.engaged())
            .unwrap_or(false)
    }

    /// Telemetry accessor (`EngineStats::duck_depth_db`): this group's
    /// current duck gain reduction, in dB (0 = not reduced / not a duck
    /// target). Unknown id or no duck configured: `0.0`.
    pub fn group_duck_depth_db(&self, group: GroupId) -> f32 {
        self.groups
            .iter()
            .find(|g| g.id == group)
            .and_then(|g| g.duck_gain.as_ref())
            .map(|dg| dg.depth_db())
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::DspSpec;
    use crate::sample::{ChannelLayout, Format, GroupSpec, OutputSpec};

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
            layout: ChannelLayout::STEREO,
        }
    }

    fn five_one(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 6,
            layout: ChannelLayout::SURROUND_5_1,
        }
    }

    fn single_group_topology(gain: f32, follow_master: bool, master: f32) -> Topology {
        Topology {
            master: Gain::new(master).unwrap(),
            groups: vec![GroupSpec {
                id: GroupId(1),
                gain: Gain::new(gain).unwrap(),
                follow_master,
                output: OutputId(1),
                input_format: stereo(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
            }],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        }
    }

    /// Every `Src` is a real chunked resampler now (P2 — see resample.rs module
    /// doc), even at unity ratio: a single push_group/take_output tick doesn't
    /// necessarily yield output immediately (the resampler needs a full input
    /// chunk buffered first). Tests collect output across many ticks and check
    /// the settled tail, rather than asserting on one tick's exact count.
    fn run_ticks(
        mixer: &mut Mixer,
        group: GroupId,
        output: OutputId,
        frames: &[f32],
        ticks: usize,
    ) -> Vec<f32> {
        let mut collected = Vec::new();
        for _ in 0..ticks {
            mixer.push_group(group, frames);
            mixer.mix_tick();
            let mut out = vec![0.0f32; frames.len()];
            let n = mixer.take_output(output, &mut out);
            collected.extend_from_slice(&out[..n]);
        }
        collected
    }

    #[test]
    fn unity_gain_same_format_passes_samples_through() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![0.5f32; 64 * 2];

        let collected = run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 80);

        assert!(
            !collected.is_empty(),
            "resampler never produced output across 80 ticks"
        );
        let tail = &collected[collected.len().saturating_sub(64)..];
        for &s in tail {
            assert!((s - 0.5).abs() < 1e-3, "expected ~0.5, got {s}");
        }
    }

    #[test]
    fn zero_gain_settles_to_silence() {
        let topo = single_group_topology(0.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![1.0f32; 64 * 2];

        let collected = run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 80);

        assert!(
            !collected.is_empty(),
            "resampler never produced output across 80 ticks"
        );
        let tail = &collected[collected.len().saturating_sub(64)..];
        for &s in tail {
            assert!(s.abs() < 1e-2, "expected ~0.0, got {s}");
        }
    }

    #[test]
    fn take_output_resets_accumulator_between_ticks() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![0.3f32; 32 * 2];

        // Push several ticks so the resampler has definitely produced at
        // least one chunk of output at some point.
        run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 32);

        // No push_group this tick — accumulator must read back empty, not stale.
        let mut out = vec![9.0f32; 32 * 2];
        let n = mixer.take_output(OutputId(1), &mut out);
        assert_eq!(n, 0);
    }

    #[test]
    fn unknown_group_and_output_ids_are_ignored_not_panicking() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        assert!(mixer.apply(MixerCommand::SetGroupGain(GroupId(99), Gain::UNITY)).is_none());
        mixer.push_group(GroupId(99), &[0.0; 4]);
        mixer.mix_tick();
        let mut out = [0.0f32; 4];
        assert_eq!(mixer.take_output(OutputId(99), &mut out), 0);
    }

    #[test]
    fn dangling_output_ref_is_rejected_at_construction() {
        let topo = Topology {
            master: Gain::UNITY,
            groups: vec![GroupSpec {
                id: GroupId(1),
                gain: Gain::UNITY,
                follow_master: false,
                output: OutputId(404),
                input_format: stereo(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
            }],
            outputs: vec![],
        };
        assert!(matches!(
            Mixer::new(&topo, 256),
            Err(DomainError::DanglingOutputRef { .. })
        ));
    }

    #[test]
    fn a_bus_output_channel_mismatch_no_longer_hard_fails_construction() {
        // This is the exact scenario that motivated the channel-mixdown
        // feature: an 8/6-channel bus routed to a 2-channel output used to
        // fail at Mixer::new with DomainError::ChannelMismatch.
        let topo = Topology {
            master: Gain::UNITY,
            groups: vec![GroupSpec {
                id: GroupId(1),
                gain: Gain::UNITY,
                follow_master: false,
                output: OutputId(1),
                input_format: five_one(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
            }],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        };
        assert!(Mixer::new(&topo, 256).is_ok());
    }

    #[test]
    fn five_one_bus_downmixed_to_stereo_output_produces_nonzero_audio() {
        let topo = Topology {
            master: Gain::UNITY,
            groups: vec![GroupSpec {
                id: GroupId(1),
                gain: Gain::UNITY,
                follow_master: false,
                output: OutputId(1),
                input_format: five_one(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
            }],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        };
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        // Interleaved 5.1 frames, every channel at 0.5.
        let frames = vec![0.5f32; 64 * 6];

        let collected = run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 80);

        assert!(!collected.is_empty(), "downmixed output never produced any samples");
        assert!(collected.iter().any(|&s| s.abs() > 1e-3), "downmixed output is silent");
    }

    #[test]
    fn set_muted_true_produces_no_output_samples() {
        // follow_master = false here on purpose: mute must silence a group
        // even when it isn't bound to master, per the "global kill" decision.
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        mixer.apply(MixerCommand::SetMuted(true));
        let frames = vec![0.5f32; 64 * 2];

        let collected = run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 80);

        assert!(
            collected.is_empty(),
            "muted mixer must produce zero output samples, got {} samples",
            collected.len()
        );
    }

    #[test]
    fn unmuting_restores_output_at_the_original_gain() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![0.5f32; 64 * 2];

        mixer.apply(MixerCommand::SetMuted(true));
        run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 40);
        mixer.apply(MixerCommand::SetMuted(false));
        let collected = run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 80);

        assert!(!collected.is_empty(), "unmuted mixer produced no output");
        let tail = &collected[collected.len().saturating_sub(64)..];
        for &s in tail {
            assert!((s - 0.5).abs() < 1e-3, "expected ~0.5 after unmute, got {s}");
        }
    }

    #[test]
    fn format_layout_disagreeing_with_channel_count_is_rejected() {
        let bad_format = Format {
            sample_rate: 48_000,
            channels: 2,
            layout: ChannelLayout::SURROUND_5_1, // 6 speakers, claims 2 channels
        };
        let topo = Topology {
            master: Gain::UNITY,
            groups: vec![GroupSpec {
                id: GroupId(1),
                gain: Gain::UNITY,
                follow_master: false,
                output: OutputId(1),
                input_format: bad_format,
                dsp: Vec::new(),
                duck: None,
                spatial: false,
            }],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        };
        assert!(matches!(
            Mixer::new(&topo, 256),
            Err(DomainError::InvalidLayout { .. })
        ));
    }

    #[test]
    fn dangling_duck_trigger_is_rejected_at_construction() {
        let topo = Topology {
            master: Gain::UNITY,
            groups: vec![GroupSpec {
                id: GroupId(1),
                gain: Gain::UNITY,
                follow_master: false,
                output: OutputId(1),
                input_format: stereo(48_000),
                dsp: Vec::new(),
                duck: Some(DuckSpec {
                    trigger: GroupId(99), // does not exist in this topology
                    amount_db: 6.0,
                    threshold_db: -30.0,
                    attack_ms: 5.0,
                    release_ms: 200.0,
                }),
                spatial: false,
            }],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        };
        assert!(matches!(
            Mixer::new(&topo, 256),
            Err(DomainError::DanglingDuckTrigger { .. })
        ));
    }

    fn two_group_topology() -> Topology {
        Topology {
            master: Gain::UNITY,
            groups: vec![
                GroupSpec {
                    id: GroupId(1), // trigger — e.g. voice chat
                    gain: Gain::UNITY,
                    follow_master: false,
                    output: OutputId(1),
                    input_format: stereo(48_000),
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                },
                GroupSpec {
                    id: GroupId(2), // target — e.g. music, ducks under GroupId(1)
                    gain: Gain::UNITY,
                    follow_master: false,
                    output: OutputId(1),
                    input_format: stereo(48_000),
                    dsp: Vec::new(),
                    duck: Some(DuckSpec {
                        trigger: GroupId(1),
                        amount_db: 12.0,
                        threshold_db: -40.0,
                        attack_ms: 5.0,
                        release_ms: 200.0,
                    }),
                    spatial: false,
                },
            ],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        }
    }

    #[test]
    fn loud_trigger_ducks_the_target_group_below_its_dry_level() {
        let topo = two_group_topology();
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let loud_trigger = vec![0.8f32; 64 * 2];
        let target = vec![0.5f32; 64 * 2];

        let mut settled = Vec::new();
        for _ in 0..80 {
            mixer.push_group(GroupId(1), &loud_trigger);
            mixer.push_group(GroupId(2), &target);
            mixer.mix_tick();
            let mut out = vec![0.0f32; 128 * 2];
            let n = mixer.take_output(OutputId(1), &mut out);
            settled = out[..n].to_vec();
        }

        assert!(mixer.group_duck_depth_db(GroupId(2)) > 1.0, "target should be ducked");
        // Summed output is trigger + ducked target; the ducked target alone
        // settles below its dry 0.5 — check via the depth accessor above
        // rather than trying to separate it back out of the sum.
        assert!(!settled.is_empty());
    }

    #[test]
    fn quiet_trigger_does_not_duck_the_target() {
        let topo = two_group_topology();
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let quiet_trigger = vec![0.0001f32; 64 * 2]; // well under threshold_db (-40dB)
        let target = vec![0.5f32; 64 * 2];

        for _ in 0..80 {
            mixer.push_group(GroupId(1), &quiet_trigger);
            mixer.push_group(GroupId(2), &target);
            mixer.mix_tick();
            let mut out = vec![0.0f32; 128 * 2];
            mixer.take_output(OutputId(1), &mut out);
        }

        assert!(
            mixer.group_duck_depth_db(GroupId(2)) < 0.5,
            "target should not be ducked by a quiet trigger"
        );
    }

    #[test]
    fn set_duck_command_reconfigures_a_running_group_without_a_rebuild() {
        let topo = single_group_topology(1.0, false, 1.0); // GroupId(1), no duck at construction
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        assert_eq!(mixer.group_duck_depth_db(GroupId(1)), 0.0);

        mixer.apply(MixerCommand::SetDuck {
            group: GroupId(1),
            duck: Some(DuckSpec {
                trigger: GroupId(1), // trivial self-trigger just to exercise the command path
                amount_db: 6.0,
                threshold_db: -60.0,
                attack_ms: 5.0,
                release_ms: 200.0,
            }),
        });

        let frames = vec![0.5f32; 64 * 2];
        for _ in 0..40 {
            mixer.push_group(GroupId(1), &frames);
            mixer.mix_tick();
            let mut out = vec![0.0f32; 128 * 2];
            mixer.take_output(OutputId(1), &mut out);
        }
        assert!(mixer.group_duck_depth_db(GroupId(1)) > 1.0);
    }

    #[test]
    fn swap_chain_replaces_the_running_chain_and_returns_the_retired_one() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();

        let new_chain = Box::new(
            DspChain::new(
                &[DspSpec::Limiter { ceiling_db: -6.0 }],
                stereo(48_000),
                256,
            )
            .unwrap(),
        );
        let retired = mixer.apply(MixerCommand::SwapChain {
            group: GroupId(1),
            chain: new_chain,
        });
        assert!(retired.is_some(), "swap should hand back the previously-installed chain");
    }

    #[test]
    fn swap_chain_on_unknown_group_is_a_no_op_returning_none() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let chain = Box::new(DspChain::new(&[], stereo(48_000), 256).unwrap());
        assert!(mixer
            .apply(MixerCommand::SwapChain {
                group: GroupId(99),
                chain,
            })
            .is_none());
    }

    #[test]
    fn set_dsp_param_routes_into_the_groups_chain() {
        let topo = Topology {
            master: Gain::UNITY,
            groups: vec![GroupSpec {
                id: GroupId(1),
                gain: Gain::UNITY,
                follow_master: false,
                output: OutputId(1),
                input_format: stereo(48_000),
                dsp: vec![DspSpec::Limiter { ceiling_db: -6.0 }],
                duck: None,
                spatial: false,
            }],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        };
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        // No panic / unknown-id-style silent success is the assertion here —
        // the per-stage EQ/limiter math itself is covered in dsp.rs's own tests.
        mixer.apply(MixerCommand::SetDspParam {
            group: GroupId(1),
            stage: 0,
            param: DspParam::LimiterCeilingDb(-3.0),
        });
        mixer.apply(MixerCommand::SetDspBypass {
            group: GroupId(1),
            stage: 0,
            bypassed: true,
        });
        let frames = vec![1.0f32; 64 * 2];
        let collected = run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 40);
        assert!(!collected.is_empty());
    }

    #[test]
    fn output_headroom_limiter_engages_when_summed_groups_clip() {
        let topo = Topology {
            master: Gain::UNITY,
            groups: vec![
                GroupSpec {
                    id: GroupId(1),
                    gain: Gain::UNITY,
                    follow_master: false,
                    output: OutputId(1),
                    input_format: stereo(48_000),
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                },
                GroupSpec {
                    id: GroupId(2),
                    gain: Gain::UNITY,
                    follow_master: false,
                    output: OutputId(1),
                    input_format: stereo(48_000),
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                },
            ],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        };
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let hot = vec![0.9f32; 64 * 2]; // two groups at 0.9 sum to 1.8 — well over full scale

        let mut settled = Vec::new();
        for _ in 0..80 {
            mixer.push_group(GroupId(1), &hot);
            mixer.push_group(GroupId(2), &hot);
            mixer.mix_tick();
            let mut out = vec![0.0f32; 128 * 2];
            let n = mixer.take_output(OutputId(1), &mut out);
            settled = out[..n].to_vec();
        }

        assert!(mixer.output_limiter_engaged(OutputId(1)));
        for &s in &settled {
            assert!(s <= 1.0 + 1e-2, "expected no clipping above full scale, got {s}");
        }
    }

    #[test]
    fn render_build_selects_spatial_when_spatial_and_output_is_stereo() {
        let render = Render::build(true, five_one(48_000), stereo(48_000), 256);
        assert!(matches!(render, Render::Spatial(_)));
        assert!(!render.is_identity(), "Spatial must never report identity");
    }

    #[test]
    fn render_build_falls_back_to_matrix_when_output_is_not_stereo() {
        // spatial=true but the output is 5.1, not stereo -- the design's
        // documented fallback (mixer owns the rule, not a config error).
        let render = Render::build(true, five_one(48_000), five_one(48_000), 256);
        assert!(matches!(render, Render::Matrix(_)));
        assert!(render.is_identity(), "same layout in/out -> the matrix path is identity");
    }

    #[test]
    fn spatial_group_with_stereo_output_produces_binaural_audio_without_panicking() {
        let topo = Topology {
            master: Gain::UNITY,
            groups: vec![GroupSpec {
                id: GroupId(1),
                gain: Gain::UNITY,
                follow_master: false,
                output: OutputId(1),
                input_format: five_one(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: true,
            }],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        };
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![0.5f32; 64 * 6];

        let collected = run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 80);

        assert!(!collected.is_empty(), "spatialized output never produced any samples");
        assert!(collected.iter().any(|&s| s.abs() > 1e-4), "spatialized output is silent");
    }

    #[test]
    fn swap_render_replaces_the_running_render_and_returns_the_retired_one() {
        let topo = single_group_topology(1.0, false, 1.0); // built non-spatial (Matrix, identity)
        let mut mixer = Mixer::new(&topo, 256).unwrap();

        let new_render = Box::new(Render::build(true, stereo(48_000), stereo(48_000), 256));
        let retired = mixer.apply(MixerCommand::SwapRender {
            group: GroupId(1),
            render: new_render,
        });
        assert!(
            matches!(retired, Some(Retired::Render(_))),
            "swap should hand back the previously-installed render"
        );

        let frames = vec![0.5f32; 64 * 2];
        let collected = run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 80);
        assert!(!collected.is_empty(), "post-swap spatial render produced no output");
    }

    #[test]
    fn swap_render_on_unknown_group_is_a_no_op_returning_none() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let render = Box::new(Render::build(false, stereo(48_000), stereo(48_000), 256));
        assert!(mixer
            .apply(MixerCommand::SwapRender {
                group: GroupId(99),
                render,
            })
            .is_none());
    }
}
