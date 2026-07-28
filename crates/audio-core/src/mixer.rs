//! Per-group gain, DSP, duck, SRC, and output summing. Everything here runs
//! on the RT mixer thread (`engine::runtime`) — no allocation, no locks, no
//! blocking.

use crate::channel::ChannelMatrix;
use crate::dsp::{db_to_linear, DspChain, DspParam, DspStage, Limiter};
use crate::meter::{MeterLevel, PeakMeter};
use crate::resample::Src;
use crate::sample::{
    ChannelLayout, DomainError, DuckSpec, Format, Gain, GroupId, GroupSpec, OutputId, OutputSpec,
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
            // `from`'s rate, not `to`'s: the render stage runs BEFORE the SRC
            // (`mix_tick` phase 3), so the samples this convolves are still at
            // the group's input rate. Building the HRIR at the device rate put
            // the interaural delay out by the whole rate ratio — a 48 kHz
            // capture into a 96 kHz DAC got twice the intended ITD.
            let hrirs = HrirSet::embedded(from.sample_rate);
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
    /// Fans out to every group's `Src` feeding that output — each group has its
    /// own resampler, but they must all run the **same** ratio.
    ///
    /// This is a correctness constraint, not a convenience: `mix_tick` sums
    /// every group into one shared accumulator over a single span, so groups on
    /// one output must produce identical frame counts per tick. A per-group
    /// ratio (tried 2026-07-27) makes them diverge, and the shorter group's
    /// tail is silently zero-filled — a level notch at the tick rate, measured
    /// at 1% of output samples dropping to half amplitude.
    ///
    /// Per-output is also the right *physical* granularity. Process-loopback
    /// capture for every app on a machine is driven by one WASAPI engine clock
    /// at one pinned rate, so two groups' capture streams do not drift apart
    /// from each other. The clock that genuinely differs is the DAC's, and
    /// that is shared by every group routed to it.
    SetOutputRatio(OutputId, ResampleRatio),
    /// Global output-stage kill, independent of `follow_master` — silences
    /// every group's contribution to every output. Gain/master smoothers
    /// keep running so unmute resumes at the same value with no re-ramp.
    SetMuted(bool),
    /// Per-group output-stage kill, persisted in config. Same skip point as
    /// `SetMuted` (after matrix/SRC), so smoothers and the resampler stay warm.
    SetGroupMute(GroupId, bool),
    /// Session-only solo, never persisted. Any group soloed puts the whole
    /// mixer in solo mode: non-soloed groups are silenced on every output.
    SetGroupSolo(GroupId, bool),
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
    /// Persisted per-group kill (per-group-mute-solo.md). `GroupSpec.mute`
    /// precedent.
    mute: bool,
    /// Session-only; never sourced from `GroupSpec` -- every rebuild starts
    /// unsoloed (decision 5).
    solo: bool,
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
    /// SRC output, as a FIFO. Capacity is generous (see Mixer::new) so a full
    /// block's worth of input is always consumed in one `push_group` call,
    /// with room left to carry a surplus into the next tick.
    resampled: Vec<f32>,
    /// Interleaved samples of `resampled` produced but not yet summed into the
    /// output. Samples, not frames, matching `SrcProgress::produced` and
    /// `OutputState::filled`; always a whole number of frames because the SRC
    /// only ever produces whole frames.
    ///
    /// Groups sharing an output complete their SRC chunks on *different*
    /// ticks, because their pids deliver packets independently and the mixer
    /// also ticks on render wakes that carry no new capture input. The shared
    /// span must advance at the rate every group can sustain, so whichever
    /// group runs ahead parks its surplus here for a tick instead of having
    /// the other group's silence emitted in its place.
    resampled_samples: usize,
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
    /// Post-fader level meter (level-meters.md): sampled on the group's
    /// post-gain/DSP/duck signal (`scratch`), before matrix/SRC — the audible
    /// contribution this group makes to its output. Independent of the global
    /// mute kill (which happens at the output accumulator, not here), so the
    /// bar reflects what the group is producing even while master is muted;
    /// the output meter is what reads silent under mute.
    meter: PeakMeter,
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
    /// Per-output level meter (level-meters.md): sampled on the final summed +
    /// limited signal, so it reads silent whenever the output is muted or
    /// idle this tick.
    meter: PeakMeter,
}

pub struct Mixer {
    max_block_frames: usize,
    groups: Vec<GroupState>,
    outputs: Vec<OutputState>,
    muted: bool,
}

/// The single effective-silence rule (decision 2: mute wins over solo). Free
/// function, not a method -- it needs one group and one flag, and keeping it
/// callable from both phase 1 and phase 3 of `mix_tick` avoids a second borrow
/// of `self`.
fn silenced(g: &GroupState, solo_active: bool) -> bool {
    g.mute || (solo_active && !g.solo)
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

/// How many frames of THIS output's audio one input block can become — the
/// worst case across every group routed here, since each group resamples from
/// its own input rate to this device's rate. A device nobody feeds still gets
/// a full block so the buffer is never zero-length.
fn output_block_frames(
    topology: &Topology,
    out: &OutputSpec,
    max_block_frames: usize,
) -> usize {
    topology
        .groups
        .iter()
        .filter(|g| g.output == out.id)
        .map(|g| {
            crate::resample::max_output_block_frames(
                max_block_frames,
                g.input_format.sample_rate,
                out.format.sample_rate,
            )
        })
        .max()
        .unwrap_or(max_block_frames)
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
        mute: gspec.mute,
        solo: false,
        gain: Smoothed::new(gspec.gain.value(), sample_rate, GAIN_TIME_CONSTANT_S),
        master: Smoothed::new(topology.master.value(), sample_rate, GAIN_TIME_CONSTANT_S),
        render,
        matrixed: vec![0.0; max_block_frames * out_channels],
        src,
        scratch: vec![0.0; max_block_frames * channels],
        valid_len: 0,
        // SRC output, so sized in OUTPUT frames like every post-SRC buffer
        // (the old flat `max_block_frames * 8` happened to cover a 2x device
        // ratio, but only by accident of the multiplier). Doubled over the
        // worst-case single block so one call can always drain a leftover
        // chunk and resample a fresh one without the caller's input going
        // unconsumed. Sized by out_channels, not the source channel count.
        resampled: vec![
            0.0;
            crate::resample::max_output_block_frames(
                max_block_frames,
                sample_rate,
                out_spec.format.sample_rate,
            ) * out_channels
                * 2
        ],
        resampled_samples: 0,
        input_format: gspec.input_format,
        dsp_chain,
        duck: gspec.duck,
        duck_gain,
        env_follower: EnvFollower::new(sample_rate),
        last_env_db: f32::NEG_INFINITY,
        meter: PeakMeter::new(sample_rate),
    })
}

impl Mixer {
    pub fn new(topology: &Topology, max_block_frames: usize) -> Result<Mixer, DomainError> {
        let max_block_frames = max_block_frames.max(1);

        let mut outputs = Vec::with_capacity(topology.outputs.len());
        for spec in &topology.outputs {
            // OUTPUT frames, not `max_block_frames` — see
            // [`max_output_block_frames`]. `max_block_frames` counts frames at
            // a group's INPUT rate; everything from here on holds frames at
            // this device's rate. Sizing this buffer in input frames truncates
            // `mix_tick`'s `produced.min(accum.len())` by exactly the rate
            // ratio whenever the device runs faster than the capture (48 kHz
            // capture into a 96 kHz DAC discards half of every block).
            let block_frames = output_block_frames(topology, spec, max_block_frames);
            let cap = block_frames * spec.format.channels as usize;
            outputs.push(OutputState {
                id: spec.id,
                accum: vec![0.0; cap],
                filled: 0,
                format: spec.format,
                limiter: Limiter::new(OUTPUT_HEADROOM_CEILING_DB, spec.format, block_frames),
                meter: PeakMeter::new(spec.format.sample_rate),
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

    /// Interleaved samples one tick can leave in this output's accumulator —
    /// the exact size a caller's `take_output` buffer must be. Callers must
    /// not re-derive it from `max_block_frames`: that counts INPUT frames, and
    /// a buffer short of this silently truncates the tick (see
    /// [`crate::max_output_block_frames`]). Unknown id: 0.
    pub fn output_capacity(&self, output: OutputId) -> usize {
        self.outputs
            .iter()
            .find(|o| o.id == output)
            .map(|o| o.accum.len())
            .unwrap_or(0)
    }

    /// Derived once per `mix_tick`, never cached (decision 6: a maintained
    /// counter is state that can desync from the flags it summarizes).
    fn solo_active(&self) -> bool {
        self.groups.iter().any(|g| g.solo)
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
            MixerCommand::SetGroupMute(id, mute) => {
                if let Some(g) = self.groups.iter_mut().find(|g| g.id == id) {
                    g.mute = mute;
                }
                None
            }
            MixerCommand::SetGroupSolo(id, solo) => {
                if let Some(g) = self.groups.iter_mut().find(|g| g.id == id) {
                    g.solo = solo;
                }
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

    /// Drops the sub-chunk of input this group's resampler is still holding,
    /// so it stops gating its output's span (`mix_tick`). The caller
    /// (`engine::runtime::pull_group_inputs`) invokes this the moment a group
    /// has no pids left: nothing can push to it, so nothing will ever complete
    /// that chunk.
    ///
    /// `mix_tick`'s parking-capacity bound would free the span anyway, but only
    /// after the live groups had filled their parking and lost a block of input
    /// to it. This is the exact signal for the case that actually occurs; the
    /// capacity bound is the backstop for causes not enumerated here.
    /// Idempotent — the caller fires it every tick while the group is empty.
    pub fn discard_group_partial_input(&mut self, group: GroupId) {
        if let Some(g) = self.groups.iter_mut().find(|g| g.id == group) {
            g.src.discard_partial_input();
        }
    }

    /// Runs once per tick, after every group's `push_group` call and before
    /// `take_output`. Order per L3 interaction A: every trigger's post-chain
    /// envelope first (phase 1), then duck gain reduction on every target
    /// (phase 2) — no target's matrix/SRC/sum has run yet, so there's no
    /// feedback regardless of how the duck config graph is shaped — then
    /// matrix -> SRC -> sum per group (phase 3), then the always-on
    /// per-output headroom limiter (phase 4).
    pub fn mix_tick(&mut self) {
        self.mix_tick_gated(&[]);
    }

    /// `mix_tick`, with the outputs in `blocked` producing no span this tick.
    ///
    /// Backpressure from the output ring (audio-flow-control). The governor
    /// withholds a group's input pull while its output ring is at threshold,
    /// on the assumption that no input means no output — but a group holding
    /// parked surplus keeps supplying the shared span through a withhold, so
    /// the mixer could offer a ring more than it had room for. Measured on
    /// hardware 2026-07-27: spans of 858–1212 frames offered to a ring with
    /// 705–825 free, the remainder rejected and counted as `output_drops`,
    /// audible as popping whenever two groups shared an output.
    ///
    /// A blocked output computes no span, so every group's surplus stays
    /// parked in its own FIFO and goes out on a tick that has room — which is
    /// what the FIFO is for. Groups still run their gain, DSP, duck, matrix
    /// and SRC stages, so no smoother or resampler goes cold.
    ///
    /// This cannot be done by skipping the flush instead: `take_output` is
    /// what clears the accumulator, so a skipped flush with a live `mix_tick`
    /// would sum the next tick on top of this one.
    ///
    /// **The caller must block emission only while it is also withholding
    /// input for that output** — in the engine both answer to one
    /// `group_may_push` call against one headroom snapshot. Blocking emission
    /// while input still arrives overruns `resampled`, and the input that no
    /// longer fits is dropped without a counter (`a_blocked_output_parks_its_
    /// audio_instead_of_emitting_it` covers the paired case; the unpaired one
    /// trips `mix_tick`'s own "resampled scratch undersized" assert in debug).
    pub fn mix_tick_gated(&mut self, blocked: &[OutputId]) {
        let solo_active = self.solo_active();

        for i in 0..self.groups.len() {
            let g = &mut self.groups[i];
            let n = g.valid_len;
            let env = g.env_follower.process_block(&g.scratch[..n], g.channels);
            // Ballistics always advance (frozen-meter learning, 2026-07-22);
            // only the published trigger env is forced down for a silenced
            // group, so it stops driving any duck target (decision 4).
            g.last_env_db = if silenced(g, solo_active) {
                f32::NEG_INFINITY
            } else {
                env
            };
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

        let max_block_frames = self.max_block_frames;
        for i in 0..self.groups.len() {
            let n = self.groups[i].valid_len;
            let g = &mut self.groups[i];

            // Post-fader meter tap (level-meters.md): the group's fully-faded
            // post-duck signal, at source layout, before matrix/SRC. Empty
            // this tick (no input pushed) → decay across the nominal block so
            // the bar falls instead of freezing at its last peak.
            if n > 0 {
                g.meter.observe(&g.scratch[..n], g.channels);
            } else {
                g.meter.observe_silence(max_block_frames);
            }

            // Render stage: skipped entirely (no copy) when the matrix is
            // identity — the common case should pay nothing extra. `Spatial`
            // is never identity, so it always goes through `render.process`.
            let (matrix_input, matrix_len): (&[f32], usize) = if g.render.is_identity() {
                (&g.scratch[..n], n)
            } else {
                let len = g.render.process(&g.scratch[..n], &mut g.matrixed);
                (&g.matrixed[..len], len)
            };

            // Appends to whatever surplus last tick carried over, so a group
            // that ran ahead of its output's span does not lose those frames.
            let carried = g.resampled_samples;
            let progress = g.src.process(matrix_input, &mut g.resampled[carried..]);
            debug_assert_eq!(
                progress.consumed, matrix_len,
                "resampled scratch undersized for one block"
            );
            g.resampled_samples += progress.produced;

            // Output-stage kill: gain/chain/duck/matrix/SRC still ran above
            // (smoothers and resampler state stay warm), only the write into
            // the shared output accumulator is skipped — unmute resumes with
            // no re-ramp or glitch. Per-group mute/solo silencing rides the
            // same skip point. The FIFO is dropped rather than carried: a
            // silenced group must not gate its output's span (below), and
            // holding frames it will never contribute would only stall it.
            if self.muted || silenced(g, solo_active) {
                g.resampled_samples = 0;
            }
        }

        // How far each output's shared span may advance this tick: the least
        // any group with audio in flight can supply.
        //
        // NOT the most (`filled.max(write_len)`, the pre-2026-07-27 rule).
        // Groups sharing an output cross their SRC chunk boundaries on
        // different ticks, so on the tick where one group completes a chunk
        // the other has produced nothing — and emitting the longer span put
        // that group's *silence* into the output where its audio should have
        // gone. With one group it was invisible (nothing produced meant
        // nothing emitted); with two it spliced a silence block into the other
        // group's stream every time their boundaries fell apart. Measured at
        // 50% of output samples, audible as constant static, and reproduced by
        // `groups_sharing_an_output_never_emit_a_span_another_group_owes`.
        //
        // A group with nothing in flight is genuinely silent for this span and
        // does not gate — otherwise an idle group would stall its output
        // forever.
        //
        // Nor does a group whose in-flight audio is no longer coming. A partial
        // SRC chunk only completes when more input arrives; when a group's last
        // pid goes away none ever does, so it gated at zero forever and every
        // group on that output went permanently silent — measured on hardware
        // 2026-07-27 (MT17), by unassigning the second group's last app.
        //
        // The bound is the parking capacity, not a timeout. Waiting is only
        // free while the groups running ahead can park their surplus, and
        // `resampled` holds two output blocks — one block in flight plus one
        // parked. Once a group is holding a full block it can accept no more,
        // and `src.process` would leave the next block's input unconsumed:
        // waiting past that point discards live audio to keep faith with a
        // group that may never produce again. So at that point in-flight audio
        // stops gating and the span advances without it.
        //
        // In normal operation this never fires: groups are at most a chunk
        // boundary apart, which is well under a block (the surplus that
        // `groups_sharing_an_output_never_emit_a_span_another_group_owes`
        // exercises stays parked for a single tick).
        for out in self.outputs.iter_mut() {
            // No room in this output's ring: emit nothing, park everything.
            // `filled` is already 0 (the previous `take_output` cleared it) —
            // set explicitly so this holds even if a caller skipped a flush.
            if blocked.contains(&out.id) {
                out.filled = 0;
                continue;
            }

            let parking_full = self
                .groups
                .iter()
                .filter(|g| g.output == out.id)
                .any(|g| g.resampled_samples * 2 >= g.resampled.len());

            let span = self
                .groups
                .iter()
                .filter(|g| g.output == out.id)
                .filter(|g| {
                    g.resampled_samples > 0 || (g.src.has_audio_in_flight() && !parking_full)
                })
                .map(|g| g.resampled_samples)
                .min()
                .unwrap_or(0)
                .min(out.accum.len());

            if span == 0 {
                continue;
            }
            for g in self.groups.iter_mut().filter(|g| g.output == out.id) {
                let n = span.min(g.resampled_samples);
                for s in 0..n {
                    out.accum[s] += g.resampled[s];
                }
                // Shift the surplus down for the next tick. At most one block
                // ever survives here, so this copy is bounded by the same block
                // size every other per-tick copy in this function is.
                g.resampled.copy_within(n..g.resampled_samples, 0);
                g.resampled_samples -= n;
            }
            out.filled = span;
        }

        for out in self.outputs.iter_mut() {
            let filled = out.filled;
            out.limiter.process(&mut out.accum[..filled], out.format);

            // Per-output meter tap (level-meters.md): final summed + limited
            // signal. Nothing summed this tick (muted or idle) → decay across
            // the nominal block so the device bar falls to the floor.
            if filled > 0 {
                out.meter.observe(&out.accum[..filled], out.format.channels as usize);
            } else {
                out.meter.observe_silence(max_block_frames);
            }
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

    /// Telemetry accessor (`EngineStats::group_peak`, level-meters.md): this
    /// group's current post-fader meter level. Unknown id: `SILENT`, same
    /// "nothing to report" convention as the accessors above.
    pub fn group_peak(&self, group: GroupId) -> MeterLevel {
        self.groups
            .iter()
            .find(|g| g.id == group)
            .map(|g| g.meter.sample())
            .unwrap_or(MeterLevel::SILENT)
    }

    /// Telemetry accessor (`EngineStats::output_peak`, level-meters.md): this
    /// output device's current post-limiter meter level. Unknown id: `SILENT`.
    pub fn output_peak(&self, output: OutputId) -> MeterLevel {
        self.outputs
            .iter()
            .find(|o| o.id == output)
            .map(|o| o.meter.sample())
            .unwrap_or(MeterLevel::SILENT)
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

    #[test]
    fn a_faster_output_device_than_the_capture_loses_no_frames() {
        // The real-hardware defect: process-loopback capture is fixed at
        // 48 kHz, the user's DAC reports a 96 kHz mix format. `max_block_frames`
        // counts INPUT frames, so an accumulator sized with it held exactly
        // half of what the SRC produced and `mix_tick`'s
        // `produced.min(accum.len())` threw the other half away — every block,
        // silently. Audibly: playback skips forward a few milliseconds at a
        // time while every surviving instant is full-bandwidth.
        let block = 512;
        let topology = Topology {
            master: Gain::UNITY,
            groups: vec![GroupSpec {
                id: GroupId(0),
                gain: Gain::UNITY,
                follow_master: false,
                output: OutputId(0),
                input_format: stereo(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                mute: false,
            }],
            outputs: vec![OutputSpec { id: OutputId(0), format: stereo(96_000) }],
        };
        let mut mixer = Mixer::new(&topology, block).unwrap();

        let capacity = mixer.output_capacity(OutputId(0));
        assert!(
            capacity >= block * 2 * 2,
            "the accumulator must hold a whole block at the OUTPUT rate (2x here), \
             got {capacity} samples for a {block}-frame input block"
        );

        // Conservation across many blocks: at a 2x rate ratio, ~2 output frames
        // must leave for every input frame. The pre-fix code produced ~1.
        let input = vec![0.5f32; block * 2];
        let mut out = vec![0.0f32; capacity];
        let mut total_out = 0usize;
        let blocks = 40;
        for _ in 0..blocks {
            mixer.push_group(GroupId(0), &input);
            mixer.mix_tick();
            total_out += mixer.take_output(OutputId(0), &mut out);
        }

        let total_in = blocks * block * 2;
        let ratio = total_out as f64 / total_in as f64;
        assert!(
            ratio > 1.9,
            "a 48k -> 96k path must emit ~2 output samples per input sample; got {ratio:.3} \
             ({total_out} out / {total_in} in) — the surplus is being truncated"
        );
    }

    #[test]
    fn every_device_rate_conserves_frames_against_the_fixed_48k_capture() {
        // Capture is pinned at 48 kHz (`PROCESS_CAPTURE_FORMAT`), so the rate
        // ratio is whatever the user's DAC reports — anything from a 24 kHz
        // endpoint to a 192 kHz one. Every output-side buffer is sized from
        // `max_output_block_frames`, so this must hold for ALL of them, not
        // just the 2x case that surfaced the bug: an accumulator one frame
        // short of what the SRC emits silently truncates the tick.
        let block = 512;
        for &out_rate in &[24_000u32, 44_100, 48_000, 88_200, 96_000, 128_000, 176_400, 192_000] {
            let topology = Topology {
                master: Gain::UNITY,
                groups: vec![GroupSpec {
                    id: GroupId(0),
                    gain: Gain::UNITY,
                    follow_master: false,
                    output: OutputId(0),
                    input_format: stereo(48_000),
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                    mute: false,
                }],
                outputs: vec![OutputSpec { id: OutputId(0), format: stereo(out_rate) }],
            };
            let mut mixer = Mixer::new(&topology, block).unwrap();
            let capacity = mixer.output_capacity(OutputId(0));

            let input = vec![0.5f32; block * 2];
            let mut out = vec![0.0f32; capacity];
            let mut total_out = 0usize;
            let blocks = 60;
            for _ in 0..blocks {
                mixer.push_group(GroupId(0), &input);
                mixer.mix_tick();
                let n = mixer.take_output(OutputId(0), &mut out);
                // The decisive check: a tick that filled the accumulator to
                // its brim is one the SRC may have overflowed. `take_output`
                // clears the accumulator either way, so a short buffer here is
                // unrecoverable loss, not backpressure.
                assert!(
                    n < capacity,
                    "{out_rate} Hz: a tick produced {n} samples into a {capacity}-sample \
                     accumulator — at the brim, so the SRC's surplus was truncated"
                );
                total_out += n;
            }

            let expected = out_rate as f64 / 48_000.0;
            let actual = total_out as f64 / (blocks * block * 2) as f64;
            assert!(
                (actual - expected).abs() < 0.02,
                "{out_rate} Hz: expected ~{expected:.3} output samples per input sample, \
                 got {actual:.3} — frames are being lost or fabricated"
            );
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
                mute: false,
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

    /// `n` groups sharing one output, ids 1..=n — the shape every span-rule
    /// probe needs to vary group count rather than assume two.
    fn n_groups_one_output_at(n: u16, out_rate: u32) -> Topology {
        let mut t = n_groups_one_output(n);
        t.outputs[0].format = stereo(out_rate);
        t
    }

    fn n_groups_one_output(n: u16) -> Topology {
        Topology {
            master: Gain::new(1.0).unwrap(),
            groups: (1..=n)
                .map(|id| GroupSpec {
                    id: GroupId(id),
                    gain: Gain::new(1.0).unwrap(),
                    follow_master: false,
                    output: OutputId(1),
                    input_format: stereo(48_000),
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                    mute: false,
                })
                .collect(),
            outputs: vec![OutputSpec { id: OutputId(1), format: stereo(48_000) }],
        }
    }

    fn two_groups_one_output() -> Topology {
        let group = |id: u16| GroupSpec {
            id: GroupId(id),
            gain: Gain::new(1.0).unwrap(),
            follow_master: false,
            output: OutputId(1),
            input_format: stereo(48_000),
            dsp: Vec::new(),
            duck: None,
            spatial: false,
            mute: false,
        };
        Topology {
            master: Gain::new(1.0).unwrap(),
            groups: vec![group(1), group(2)],
            outputs: vec![OutputSpec { id: OutputId(1), format: stereo(48_000) }],
        }
    }

    /// Diagnostic, not an assertion — run it with
    /// `cargo test -p audio-core starved_group -- --ignored --nocapture`.
    ///
    /// Oracle for the two-group popping (session-2026-07-27-static.md). Both
    /// groups' producers run at the SAME average rate, one block per tick, but
    /// group 2's arrivals are bursty: nothing on every Nth tick, double on the
    /// tick after. Each group has a capture ring in front of it and the mixer
    /// pops at most one block per tick from it, exactly as `pull_group_inputs`
    /// does — that cap is what is under test, since a group that misses a tick
    /// can only catch up by over-delivering later.
    ///
    /// **Result, 2026-07-27: the span rule is exonerated in steady state.**
    /// Jitter alone produces no notching at any gap frequency — the group
    /// carries a bounded one-block backlog in its ring and emission stays flat
    /// at one block per tick:
    ///
    /// | gap every | samples at half amplitude | max tick | ring B peak |
    /// |---|---|---|---|
    /// | never | 0% | 608 | 1 block |
    /// | 50 ticks | 0% | 608 | 2 blocks |
    /// | 10 ticks | 0% | 608 | 2 blocks |
    ///
    /// Feeding the mixer *directly*, with no ring in front of it, does notch
    /// (2% at a hole every 50 ticks, 10% at every 10) — but that models a
    /// group receiving genuinely less audio, not a group receiving it late,
    /// because `push_group` truncates at one block so the deficit is
    /// permanent. Do not mistake one for the other; this probe originally did.
    #[test]
    #[ignore = "diagnostic oracle for an open defect, not a pass/fail check"]
    fn probe_a_starved_group_loses_a_block_per_missed_tick() {
        let block = 304;
        for gap_every in [0usize, 50, 10] {
            let mut mixer = Mixer::new(&two_groups_one_output(), block).unwrap();
            let (mut ring_a, mut ring_b) = (0usize, 0usize);
            let mut ring_b_peak = 0usize;
            let mut per_tick = Vec::new();
            let mut collected = Vec::new();

            for t in 0..2000 {
                ring_a += block;
                ring_b += if gap_every == 0 {
                    block
                } else if t % gap_every == 0 {
                    0
                } else if t % gap_every == 1 {
                    2 * block
                } else {
                    block
                };
                ring_b_peak = ring_b_peak.max(ring_b);

                let (pop_a, pop_b) = (ring_a.min(block), ring_b.min(block));
                ring_a -= pop_a;
                ring_b -= pop_b;
                mixer.push_group(GroupId(1), &vec![0.5f32; pop_a * 2]);
                mixer.push_group(GroupId(2), &vec![0.5f32; pop_b * 2]);
                mixer.mix_tick();

                let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
                let n = mixer.take_output(OutputId(1), &mut out);
                per_tick.push(n);
                collected.extend_from_slice(&out[..n]);
            }

            // Both groups feed 0.5, so a correct sum is a flat 1.0; anything
            // at 0.5 is one group's contribution missing from that span.
            let tail = &collected[collected.len() / 2..];
            let notched = tail.iter().filter(|s| **s < 0.9).count();
            println!(
                "gap_every={gap_every} notched={notched}/{} ({:.1}%) max_tick={} \
                 zero_ticks={} ring_b_end={ring_b} ring_b_peak={ring_b_peak} block={block}",
                tail.len(),
                100.0 * notched as f64 / tail.len() as f64,
                per_tick.iter().max().unwrap(),
                per_tick.iter().filter(|&&n| n == 0).count(),
            );
        }
    }

    /// Diagnostic, not an assertion — run it with
    /// `cargo test -p audio-core parked_surplus -- --ignored --nocapture`.
    ///
    /// Second oracle for the two-group popping. The first one ruled the span
    /// rule out in steady state; this one models what the span rule's *parked
    /// surplus* does to the output ring.
    ///
    /// The governor (`engine::runtime::group_may_push`) withholds input pulls
    /// while an output ring sits at or above its threshold. It does not gate
    /// emission: a group holding parked surplus keeps feeding the shared span
    /// through a withhold, so the mixer can push into a ring that has no room.
    /// Parking only exists when two groups gate each other, which is exactly
    /// the condition under which `output_drops` was observed climbing on
    /// hardware (3818 → 7172 over a two-group phase, flat at one group).
    ///
    /// Groups are fed a third of a block per tick with group 2 primed half a
    /// block ahead — the same misalignment `groups_sharing_an_output_never_
    /// emit_a_span_another_group_owes` uses, so their chunk boundaries never
    /// line up.
    ///
    /// **Result, 2026-07-27: this hypothesis is dead too.** With the governor
    /// engaged (63 withheld ticks at threshold 0.75), `emitted_while_withheld`
    /// is 0 and nothing is rejected. Parking does not leak through a withhold,
    /// because a withhold pushes every group an empty block, the starved group
    /// gates, and the span collapses to zero — the `min` rule holds the
    /// surplus back precisely when the ring has no room for it.
    #[test]
    #[ignore = "diagnostic oracle for an open defect, not a pass/fail check"]
    fn probe_parked_surplus_pushes_into_a_withheld_ring() {
        let block = 304;
        // Output ring sized and drained like the real one: the governor holds
        // it near GOVERNOR_THRESHOLD_FILL, and the device drains a block per
        // tick on average.
        // Partial blocks, so the two groups' chunk completions land on
        // different ticks and one of them is always parking surplus.
        let feed = block / 3;
        let capacity = block * 2 * 4;
        // Slightly slower than production, so the ring rises to the governor's
        // threshold and sawtooths there — the regime the hardware trace shows
        // (`ring_fill` 0.52–0.81), not the balanced one where it never engages.
        let drain_per_tick = feed * 2 * 97 / 100;

        for threshold in [0.75f64, 0.9] {
            let mut mixer = Mixer::new(&two_groups_one_output(), block).unwrap();
            let mut fill = 0usize;
            let mut rejected = 0usize;
            let mut withheld_ticks = 0usize;
            let mut emitted_while_withheld = 0usize;

            for t in 0..2000 {
                fill = fill.saturating_sub(drain_per_tick);

                let may_push = (fill as f64) < threshold * capacity as f64;
                if may_push {
                    // A block per group per tick, matching the drain. Group 2
                    // is primed half a block ahead at t=0 and stays offset, so
                    // the two never complete their SRC chunks on the same tick.
                    mixer.push_group(GroupId(1), &vec![0.5f32; feed * 2]);
                    let n = if t == 0 { block / 2 + feed } else { feed };
                    mixer.push_group(GroupId(2), &vec![0.5f32; n * 2]);
                } else {
                    withheld_ticks += 1;
                    mixer.push_group(GroupId(1), &[]);
                    mixer.push_group(GroupId(2), &[]);
                }
                mixer.mix_tick();

                let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
                let n = mixer.take_output(OutputId(1), &mut out);
                if !may_push {
                    emitted_while_withheld += n;
                }
                let room = capacity - fill;
                fill += n.min(room);
                rejected += n.saturating_sub(room);
            }

            println!(
                "threshold={threshold} rejected={rejected} withheld_ticks={withheld_ticks} \
                 emitted_while_withheld={emitted_while_withheld} final_fill={fill}/{capacity}"
            );
        }
    }

    /// Diagnostic, not an assertion — run it with
    /// `cargo test -p audio-core group_count -- --ignored --nocapture`.
    ///
    /// How the shared-span design scales past two groups, and what gating
    /// emission on the output ring's headroom (the proposed fix for the
    /// two-group popping) does at each group count. Models the output ring,
    /// the governor's input withhold, and — under `gate_emission` — the same
    /// withhold applied to emission.
    ///
    /// Groups are staggered so their SRC chunk boundaries never coincide,
    /// which is the condition that makes them gate each other at all.
    #[test]
    #[ignore = "diagnostic oracle, not a pass/fail check"]
    fn probe_span_rule_against_group_count() {
        // The reporting machine's pair: capture pinned at 48 kHz into a 96 kHz
        // DAC, so one input block becomes two output blocks and `resampled`
        // (two output blocks) holds only one block of parking.
        let block = 304;
        let out_block = block * 2; // output frames per input block, ratio 2.0
        let capacity = out_block * 2 * 4; // samples, 4 output blocks — as ring_capacity_samples sizes it
        let drain_per_tick = out_block * 2 * 97 / 100;
        let threshold = 0.5f64;

        for gate_emission in [false, true] {
            for n in 2u16..=5 {
                let mut mixer = Mixer::new(&n_groups_one_output_at(n, 96_000), block).unwrap();
                let mut fill = 0usize;
                let (mut rejected, mut zero_spans, mut emitted_total) = (0usize, 0usize, 0usize);
                let mut gated_ticks = 0usize;
                let mut collected = Vec::new();
                // Per-group level scaled so the SUM is 0.5 whatever n is —
                // otherwise n >= 3 sums past the output limiter's headroom and
                // every sample reads as notched.
                let level = 0.5f32 / n as f32;

                for t in 0..3000 {
                    fill = fill.saturating_sub(drain_per_tick);
                    let has_room = (fill as f64) < threshold * capacity as f64;

                    for id in 1..=n {
                        if has_room {
                            // Staggered priming: each group sits at a
                            // different phase within its chunk forever.
                            let extra = if t == 0 { (block / (n as usize + 1)) * id as usize } else { 0 };
                            mixer.push_group(GroupId(id), &vec![level; (block + extra) * 2]);
                        } else {
                            mixer.push_group(GroupId(id), &[]);
                        }
                    }

                    // The fix: emission answers to the same headroom the
                    // input pull does, so parked surplus waits for room
                    // instead of being pushed into a ring that has none.
                    if gate_emission && !has_room {
                        gated_ticks += 1;
                        continue;
                    }
                    mixer.mix_tick();

                    let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
                    let n_out = mixer.take_output(OutputId(1), &mut out);
                    if n_out == 0 {
                        zero_spans += 1;
                    }
                    emitted_total += n_out;
                    collected.extend_from_slice(&out[..n_out]);

                    let room = capacity - fill;
                    fill += n_out.min(room);
                    rejected += n_out.saturating_sub(room);
                }

                // The sum is 0.5 whatever n is, so anything meaningfully below
                // it is some group's contribution missing from that span.
                let tail = &collected[collected.len() / 2..];
                let notched = tail.iter().filter(|s| **s < 0.45).count();
                println!(
                    "gate_emission={gate_emission} n={n} rejected={rejected} \
                     notched={notched}/{} ({:.2}%) zero_spans={zero_spans} \
                     gated_ticks={gated_ticks} emitted={emitted_total}",
                    tail.len(),
                    100.0 * notched as f64 / tail.len().max(1) as f64,
                );
            }
        }
    }

    #[test]
    fn groups_sharing_an_output_never_emit_a_span_another_group_owes() {
        // Regression, 2026-07-27 — the static the user could switch on and off
        // by ASSIGNING an app to a second group, whether or not it played.
        //
        // Groups on one output cross their SRC chunk boundaries on different
        // ticks: their pids deliver packets independently, and the mixer also
        // ticks on render wakes carrying little new capture input. The old
        // span rule (`out.filled = out.filled.max(write_len)`) emitted a block
        // whenever ANY group produced, so on the tick where group 2 completed
        // a chunk and group 1 had not, group 1's *silence* went out in place
        // of the audio still sitting in its resampler. Measured at 49.9% of
        // output samples near zero; with the `min` rule, 0%.
        //
        // Group 2 is fed pure SILENCE here, exactly as the user's second group
        // was: only its chunk TIMING ever mattered.
        let block = 304;
        let mut mixer = Mixer::new(&two_groups_one_output(), block).unwrap();
        let mut collected = Vec::new();
        for t in 0..900 {
            // Partial blocks every tick — both groups always have input in
            // flight, which is what makes each of them gate the shared span.
            mixer.push_group(GroupId(1), &vec![0.5f32; (block / 3) * 2]);
            // Primed half a block ahead, so its chunk boundaries never line up
            // with group 1's.
            let n = if t == 0 { block / 2 + block / 3 } else { block / 3 };
            mixer.push_group(GroupId(2), &vec![0.0f32; n * 2]);
            mixer.mix_tick();
            let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
            let n = mixer.take_output(OutputId(1), &mut out);
            collected.extend_from_slice(&out[..n]);
        }

        let tail = &collected[collected.len() / 2..];
        let near_zero = tail.iter().filter(|s| s.abs() < 0.05).count();
        assert_eq!(
            near_zero,
            0,
            "{near_zero} of {} settled output samples are near silence — group 1 feeds              continuous audio, so any silence in the span is group 2's chunk timing              being emitted in its place",
            tail.len()
        );
    }

    #[test]
    fn a_group_with_nothing_in_flight_does_not_stall_its_output() {
        // The other half of the `min` rule. Gating on every group regardless
        // would let one idle group hold an output at zero forever — the
        // failure mode the rule has to avoid while fixing the notch above.
        let block = 304;
        let mut mixer = Mixer::new(&two_groups_one_output(), block).unwrap();
        let mut collected = Vec::new();
        for _ in 0..200 {
            // Group 2 is never fed at all: no pids, nothing in its resampler.
            mixer.push_group(GroupId(1), &vec![0.5f32; block * 2]);
            mixer.mix_tick();
            let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
            let n = mixer.take_output(OutputId(1), &mut out);
            collected.extend_from_slice(&out[..n]);
        }

        assert!(
            !collected.is_empty(),
            "an output whose second group is idle must still emit — gating on a group              that has no audio at all would stall it permanently"
        );
        let tail = &collected[collected.len() / 2..];
        for &s in tail {
            assert!((s - 0.5).abs() < 1e-2, "expected the live group's ~0.5, got {s}");
        }
    }

    /// Feeds group 1 continuously and group 2 exactly one short block (half a
    /// chunk — `chunk_in` is `max_block_frames`, so it can never complete),
    /// then stops feeding group 2 the way the engine does: an empty push every
    /// tick. Returns, per tick, how many output samples came out.
    fn ticks_after_a_group_stops_being_fed(
        block: usize,
        ticks: usize,
        on_stop: impl FnOnce(&mut Mixer),
    ) -> Vec<usize> {
        let mut mixer = Mixer::new(&two_groups_one_output(), block).unwrap();
        // The partial only reaches the resampler when a tick runs, and the
        // stop loop below pushes group 2 an empty block that would overwrite
        // `valid_len` first — so prime with a tick of its own.
        mixer.push_group(GroupId(1), &vec![0.5f32; block * 2]);
        mixer.push_group(GroupId(2), &vec![0.5f32; (block / 2) * 2]);
        mixer.mix_tick();
        let mut prime = vec![0.0f32; mixer.output_capacity(OutputId(1))];
        mixer.take_output(OutputId(1), &mut prime);
        on_stop(&mut mixer);

        let mut per_tick = Vec::new();
        for _ in 0..ticks {
            mixer.push_group(GroupId(1), &vec![0.5f32; block * 2]);
            mixer.push_group(GroupId(2), &[]);
            mixer.mix_tick();
            let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
            per_tick.push(mixer.take_output(OutputId(1), &mut out));
        }
        per_tick
    }

    #[test]
    fn a_group_that_stops_being_fed_stops_gating_its_output() {
        // Regression, MT17 on hardware 2026-07-27: unassigning the second
        // group's last app killed ALL audio on that output, permanently —
        // `ring_fill` flat at 0.00 while `group_peak` stayed live.
        //
        // A partial SRC chunk only completes when more input arrives. With no
        // pids left none ever does, so `has_audio_in_flight` stayed true and
        // the group gated the shared span at zero for the rest of the session,
        // while the live group's surplus overran its parking capacity and its
        // input went unconsumed. This pins the capacity bound on its own,
        // without the exact signal the engine sends (below).
        let per_tick = ticks_after_a_group_stops_being_fed(304, 20, |_| {});

        assert!(
            per_tick.iter().any(|&n| n > 0),
            "the output never resumed — a group that will never be fed again is still \
             gating its span"
        );
    }

    #[test]
    fn unassigning_a_groups_last_pid_frees_its_outputs_span() {
        // The grace above is the backstop; this is the mechanism. The engine
        // calls `discard_group_partial_input` the tick a group's pids go
        // empty, so the other groups on that output never hear the gap the
        // grace would cost them.
        let per_tick =
            ticks_after_a_group_stops_being_fed(304, 8, |m| m.discard_group_partial_input(GroupId(2)));

        assert!(
            per_tick[..4].iter().any(|&n| n > 0),
            "output stalled after the discard — it should free the span immediately, \
             not wait for the parking capacity to fill"
        );
    }

    #[test]
    fn a_blocked_output_parks_its_audio_instead_of_emitting_it() {
        // Contract for the emission gate, measured on hardware 2026-07-27: the
        // governor withholds a group's input while its ring is at threshold,
        // but a group holding parked surplus kept supplying the shared span
        // through that withhold, so the mixer offered the ring more than it
        // had room for and the remainder was rejected — `output_drops`, heard
        // as popping.
        //
        // NOT an A/B of that fault: it passes with the gate reverted too,
        // because reaching the faulting state offline needs every gating group
        // to hold parked surplus at once, and after any emitting tick `min`
        // leaves at least one of them at zero. Four oracles failed to
        // construct it (see session-2026-07-27-static.md); the fault is only
        // reproducible on hardware so far. What this DOES pin is the contract
        // the gate has to keep: a blocked output emits nothing, and nothing it
        // was holding is lost.
        let block = 304;
        let mut mixer = Mixer::new(&two_groups_one_output(), block).unwrap();

        // One tick that parks: group 1 completes a chunk while group 2 is
        // still mid-chunk, so group 2 gates the span to zero and group 1's
        // block goes into its FIFO. This is the state the fault needs — a
        // group holding surplus when the ring runs out of room.
        mixer.push_group(GroupId(1), &vec![0.5f32; block * 2]);
        mixer.push_group(GroupId(2), &vec![0.5f32; (block / 3) * 2]);
        mixer.mix_tick();
        let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
        assert_eq!(
            mixer.take_output(OutputId(1), &mut out),
            0,
            "group 2 is mid-chunk, so nothing should be emitted yet — without this the \
             rest of the test proves nothing"
        );

        // Blocked. Input is withheld by the SAME predicate in the engine
        // (`group_may_push` decides both), so the groups are pushed empty —
        // that coupling is load-bearing, not incidental: blocking emission
        // while input still flowed would overrun `resampled` and the surplus
        // would become unconsumed input, silently dropped in release.
        let mut emitted_while_blocked = 0;
        for _ in 0..40 {
            mixer.push_group(GroupId(1), &[]);
            mixer.push_group(GroupId(2), &[]);
            mixer.mix_tick_gated(&[OutputId(1)]);
            let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
            emitted_while_blocked += mixer.take_output(OutputId(1), &mut out);
        }
        assert_eq!(emitted_while_blocked, 0, "a blocked output must not emit");

        // Unblocked, with input resumed as the engine resumes pulling it: the
        // parked audio comes out. It was held, not dropped.
        let mut recovered = 0;
        for _ in 0..20 {
            mixer.push_group(GroupId(1), &vec![0.5f32; block * 2]);
            mixer.push_group(GroupId(2), &vec![0.5f32; block * 2]);
            mixer.mix_tick();
            let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
            recovered += mixer.take_output(OutputId(1), &mut out);
        }
        assert!(recovered > 0, "audio parked during the block must still be delivered");
    }

    #[test]
    fn groups_sharing_an_output_stay_frame_aligned() {
        // Regression, 2026-07-27. `mix_tick` sums every group into one shared
        // accumulator over a single span (`out.filled`), so groups on one
        // output MUST produce identical frame counts per tick. A per-group
        // drift ratio made them diverge, and the shorter group's tail was
        // silently zero-filled — a level notch at the tick rate, which
        // measured 1% of output samples at half amplitude and was audible as
        // constant static that scaled with source level.
        //
        // Both groups feed identical DC, so a correct sum is a flat 0.5. The
        // command is per OUTPUT precisely so this cannot happen: whatever
        // ratio the drift loop picks reaches both resamplers.
        let block = 304;
        let mut mixer = Mixer::new(&two_groups_one_output(), block).unwrap();
        mixer.apply(MixerCommand::SetOutputRatio(
            OutputId(1),
            crate::sample::ResampleRatio::new(1.005).unwrap(),
        ));

        let frames = vec![0.25f32; block * 2];
        let mut collected = Vec::new();
        for _ in 0..200 {
            mixer.push_group(GroupId(1), &frames);
            mixer.push_group(GroupId(2), &frames);
            mixer.mix_tick();
            let mut out = vec![0.0f32; mixer.output_capacity(OutputId(1))];
            let n = mixer.take_output(OutputId(1), &mut out);
            collected.extend_from_slice(&out[..n]);
        }

        // Settled tail only — the resampler's start-up transient is not what
        // this pins.
        let tail = &collected[collected.len() / 2..];
        let notched = tail.iter().filter(|s| **s < 0.4).count();
        assert_eq!(
            notched, 0,
            "{notched} of {} output samples dropped below 0.4 — one group's contribution is \
             missing from part of the span, which is the frame-misalignment notch",
            tail.len()
        );
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
    fn group_peak_reflects_the_post_fader_signal() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![0.5f32; 64 * 2];

        mixer.push_group(GroupId(1), &frames);
        mixer.mix_tick();

        // Sampled pre-SRC, so one tick is enough (no resampler warm-up).
        assert!((mixer.group_peak(GroupId(1)).peak - 0.5).abs() < 1e-3);
    }

    #[test]
    fn group_peak_scales_with_the_fader() {
        let topo = single_group_topology(0.5, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![0.8f32; 64 * 2];

        mixer.push_group(GroupId(1), &frames);
        mixer.mix_tick();

        // 0.8 signal * 0.5 fader = 0.4.
        assert!((mixer.group_peak(GroupId(1)).peak - 0.4).abs() < 1e-3);
    }

    #[test]
    fn group_peak_decays_when_the_group_goes_idle() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();

        mixer.push_group(GroupId(1), &vec![0.5f32; 64 * 2]);
        mixer.mix_tick();
        let loud = mixer.group_peak(GroupId(1)).peak;

        // Ticks with nothing pushed — the idle-decay path must lower the bar.
        for _ in 0..50 {
            mixer.push_group(GroupId(1), &[]);
            mixer.mix_tick();
        }
        assert!(mixer.group_peak(GroupId(1)).peak < loud);
    }

    #[test]
    fn output_peak_reflects_the_summed_output() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![0.5f32; 64 * 2];

        // Settle the resampler so the output accumulator is actually filled.
        run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 80);

        assert!(mixer.output_peak(OutputId(1)).peak > 0.4);
    }

    #[test]
    fn unknown_meter_ids_report_silent() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mixer = Mixer::new(&topo, 256).unwrap();
        assert_eq!(mixer.group_peak(GroupId(99)), MeterLevel::SILENT);
        assert_eq!(mixer.output_peak(OutputId(99)), MeterLevel::SILENT);
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
                mute: false,
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
                mute: false,
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
                mute: false,
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
    fn a_muted_group_contributes_nothing_while_other_groups_still_sum() {
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
                    mute: true,
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
                    mute: false,
                },
            ],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        };
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![0.5f32; 64 * 2];

        let mut settled = Vec::new();
        for _ in 0..80 {
            mixer.push_group(GroupId(1), &frames);
            mixer.push_group(GroupId(2), &frames);
            mixer.mix_tick();
            let mut out = vec![0.0f32; 128 * 2];
            let n = mixer.take_output(OutputId(1), &mut out);
            settled = out[..n].to_vec();
        }

        assert!(!settled.is_empty());
        for &s in &settled {
            assert!(
                (s - 0.5).abs() < 1e-2,
                "expected only group 2's 0.5 to sum (group 1 muted), got {s}"
            );
        }
    }

    #[test]
    fn solo_silences_every_non_soloed_group_across_outputs() {
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
                    mute: false,
                },
                GroupSpec {
                    id: GroupId(2),
                    gain: Gain::UNITY,
                    follow_master: false,
                    output: OutputId(2),
                    input_format: stereo(48_000),
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                    mute: false,
                },
            ],
            outputs: vec![
                OutputSpec { id: OutputId(1), format: stereo(48_000) },
                OutputSpec { id: OutputId(2), format: stereo(48_000) },
            ],
        };
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        mixer.apply(MixerCommand::SetGroupSolo(GroupId(1), true));
        let frames = vec![0.5f32; 64 * 2];

        let mut soloed_output_produced_audio = false;
        let mut non_soloed_output_ever_produced_samples = false;
        for _ in 0..80 {
            mixer.push_group(GroupId(1), &frames);
            mixer.push_group(GroupId(2), &frames);
            mixer.mix_tick();
            let mut out1 = vec![0.0f32; 128 * 2];
            let n1 = mixer.take_output(OutputId(1), &mut out1);
            if n1 > 0 {
                soloed_output_produced_audio = true;
            }
            let mut out2 = vec![0.0f32; 128 * 2];
            let n2 = mixer.take_output(OutputId(2), &mut out2);
            if n2 > 0 {
                non_soloed_output_ever_produced_samples = true;
            }
        }

        assert!(soloed_output_produced_audio, "soloed group's output should produce audio");
        assert!(
            !non_soloed_output_ever_produced_samples,
            "non-soloed group's output must never sum any samples across all outputs"
        );
    }

    #[test]
    fn an_explicitly_muted_group_stays_silent_while_soloed() {
        let topo = Topology {
            master: Gain::UNITY,
            groups: vec![GroupSpec {
                id: GroupId(1),
                gain: Gain::UNITY,
                follow_master: false,
                output: OutputId(1),
                input_format: stereo(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                mute: true,
            }],
            outputs: vec![OutputSpec {
                id: OutputId(1),
                format: stereo(48_000),
            }],
        };
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        mixer.apply(MixerCommand::SetGroupSolo(GroupId(1), true));
        let frames = vec![0.5f32; 64 * 2];

        let collected = run_ticks(&mut mixer, GroupId(1), OutputId(1), &frames, 80);

        assert!(
            collected.is_empty(),
            "an explicitly muted group must stay silent even while soloed, got {} samples",
            collected.len()
        );
    }

    #[test]
    fn a_silenced_group_stops_triggering_its_duck_target() {
        let topo = two_group_topology(); // GroupId(1) trigger, GroupId(2) ducks under it
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        mixer.apply(MixerCommand::SetGroupMute(GroupId(1), true));
        let loud_trigger = vec![0.8f32; 64 * 2];
        let target = vec![0.5f32; 64 * 2];

        for _ in 0..80 {
            mixer.push_group(GroupId(1), &loud_trigger);
            mixer.push_group(GroupId(2), &target);
            mixer.mix_tick();
            let mut out = vec![0.0f32; 128 * 2];
            mixer.take_output(OutputId(1), &mut out);
        }

        assert!(
            mixer.group_duck_depth_db(GroupId(2)) < 0.5,
            "a silenced (muted) trigger must not duck its target, got {} dB",
            mixer.group_duck_depth_db(GroupId(2))
        );
    }

    #[test]
    fn a_muted_groups_own_meter_stays_live() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        mixer.apply(MixerCommand::SetGroupMute(GroupId(1), true));
        let frames = vec![0.5f32; 64 * 2];

        mixer.push_group(GroupId(1), &frames);
        mixer.mix_tick();

        // Sampled pre-skip (mixer.rs meter tap), so a muted group's own bar
        // must keep reading its unrouted signal — same invariant as master mute.
        assert!(
            (mixer.group_peak(GroupId(1)).peak - 0.5).abs() < 1e-3,
            "a muted group's own meter must stay live"
        );
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
                mute: false,
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
                mute: false,
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
                    mute: false,
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
                    mute: false,
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
                mute: false,
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
                    mute: false,
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
                    mute: false,
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
                mute: false,
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
