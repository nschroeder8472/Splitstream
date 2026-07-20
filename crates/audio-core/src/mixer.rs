//! Per-group gain, SRC, and output summing. Everything here runs on the RT
//! mixer thread (`engine::runtime`) — no allocation, no locks, no blocking.

use crate::channel::ChannelMatrix;
use crate::resample::Src;
use crate::sample::{
    DomainError, Format, Gain, GroupId, GroupSpec, OutputId, ResampleRatio, Topology,
};

#[derive(Debug, Clone, Copy)]
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
}

const GAIN_TIME_CONSTANT_S: f32 = 0.01; // 10ms — inaudible as a ramp, kills zipper noise

/// One-pole parameter smoother. Every audible parameter ramps toward its
/// target instead of stepping — a stepped gain is an audible "zipper" click.
#[derive(Debug, Clone, Copy)]
struct Smoothed {
    current: f32,
    target: f32,
    coeff: f32,
}

impl Smoothed {
    fn new(initial: f32, sample_rate: u32, time_constant_s: f32) -> Smoothed {
        let coeff = (-1.0 / (time_constant_s * sample_rate as f32)).exp();
        Smoothed {
            current: initial,
            target: initial,
            coeff,
        }
    }

    fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    #[inline]
    fn next(&mut self) -> f32 {
        self.current = self.target + self.coeff * (self.current - self.target);
        self.current
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
    /// Converts source layout -> output layout, between gain and SRC. Skipped
    /// entirely when `is_identity()` (same layout in and out).
    matrix: ChannelMatrix,
    /// Matrix output, pre-SRC. Capacity: `max_block_frames * output channels`.
    matrixed: Vec<f32>,
    src: Src,
    /// Gain-applied samples, pre-matrix. Capacity: `max_block_frames * channels`.
    scratch: Vec<f32>,
    /// SRC output. Capacity is generous (see Mixer::new) so a full block's
    /// worth of input is always consumed in one `push_group` call.
    resampled: Vec<f32>,
}

struct OutputState {
    id: OutputId,
    accum: Vec<f32>,
    /// High-water mark of valid samples in `accum` since the last `take_output`.
    filled: usize,
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

    validate_layout(&gspec.input_format)?;
    validate_layout(&out_spec.format)?;

    let channels = gspec.input_format.channels as usize;
    let out_channels = out_spec.format.channels as usize;
    let sample_rate = gspec.input_format.sample_rate;

    let matrix = ChannelMatrix::new(gspec.input_format.layout, out_spec.format.layout);

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

    Ok(GroupState {
        id: gspec.id,
        output: gspec.output,
        channels,
        follow_master: gspec.follow_master,
        gain: Smoothed::new(gspec.gain.value(), sample_rate, GAIN_TIME_CONSTANT_S),
        master: Smoothed::new(topology.master.value(), sample_rate, GAIN_TIME_CONSTANT_S),
        matrix,
        matrixed: vec![0.0; max_block_frames * out_channels],
        src,
        scratch: vec![0.0; max_block_frames * channels],
        // 8x covers every realistic device sample-rate ratio (worst case
        // in practice is well under 2x) with no per-tick sizing math. Sized
        // by out_channels, not the source channel count — undersizing here
        // for an upmix would only trip the debug_assert below in tests.
        resampled: vec![0.0; max_block_frames * out_channels * 8],
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
    pub fn apply(&mut self, cmd: MixerCommand) {
        match cmd {
            MixerCommand::SetGroupGain(id, gain) => {
                if let Some(g) = self.groups.iter_mut().find(|g| g.id == id) {
                    g.gain.set_target(gain.value());
                }
            }
            MixerCommand::SetMaster(gain) => {
                for g in self.groups.iter_mut() {
                    g.master.set_target(gain.value());
                }
            }
            MixerCommand::SetFollowMaster(id, follow) => {
                if let Some(g) = self.groups.iter_mut().find(|g| g.id == id) {
                    g.follow_master = follow;
                }
            }
            MixerCommand::SetOutputRatio(output_id, ratio) => {
                for g in self.groups.iter_mut().filter(|g| g.output == output_id) {
                    g.src.set_ratio(ratio);
                }
            }
            MixerCommand::SetMuted(muted) => {
                self.muted = muted;
            }
        }
    }

    /// Applies gain (and master, if bound) per-sample, resamples to the
    /// group's target output format, and sums into that output's per-tick
    /// accumulator. `frames` is interleaved and truncated to `max_block_frames`.
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

        // Matrix stage: skipped entirely (no copy) when source and output
        // share a layout — the common case should pay nothing extra.
        let (matrix_input, matrix_len): (&[f32], usize) = if g.matrix.is_identity() {
            (&g.scratch[..n], n)
        } else {
            let len = g.matrix.process(&g.scratch[..n], &mut g.matrixed);
            (&g.matrixed[..len], len)
        };

        let progress = g.src.process(matrix_input, &mut g.resampled);
        debug_assert_eq!(
            progress.consumed, matrix_len,
            "resampled scratch undersized for one block"
        );

        // Output-stage kill: gain/matrix/SRC still ran above (smoothers and
        // resampler state stay warm), only the write into the shared output
        // accumulator is skipped — unmute resumes with no re-ramp or glitch.
        if self.muted {
            return;
        }

        let output = g.output;
        let produced = progress.produced;
        let Some(out) = self.outputs.iter_mut().find(|o| o.id == output) else {
            return;
        };
        let write_len = produced.min(out.accum.len());
        for i in 0..write_len {
            out.accum[i] += g.resampled[i];
        }
        out.filled = out.filled.max(write_len);
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
        mixer.apply(MixerCommand::SetGroupGain(GroupId(99), Gain::UNITY));
        mixer.push_group(GroupId(99), &[0.0; 4]);
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
}
