//! Per-group gain, SRC, and output summing. Everything here runs on the RT
//! mixer thread (`engine::runtime`) — no allocation, no locks, no blocking.

use crate::resample::Src;
use crate::sample::{DomainError, Gain, GroupId, GroupSpec, OutputId, Topology};

#[derive(Debug, Clone, Copy)]
pub enum MixerCommand {
    SetGroupGain(GroupId, Gain),
    SetMaster(Gain),
    SetFollowMaster(GroupId, bool),
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
    src: Src,
    /// Gain-applied samples, pre-SRC. Capacity: `max_block_frames * channels`.
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

    let src = Src::new(gspec.input_format, out_spec.format, max_block_frames)?;
    let channels = gspec.input_format.channels as usize;
    let sample_rate = gspec.input_format.sample_rate;

    Ok(GroupState {
        id: gspec.id,
        output: gspec.output,
        channels,
        follow_master: gspec.follow_master,
        gain: Smoothed::new(gspec.gain.value(), sample_rate, GAIN_TIME_CONSTANT_S),
        master: Smoothed::new(topology.master.value(), sample_rate, GAIN_TIME_CONSTANT_S),
        src,
        scratch: vec![0.0; max_block_frames * channels],
        // 8x covers every realistic device sample-rate ratio (worst case
        // in practice is well under 2x) with no per-tick sizing math.
        resampled: vec![0.0; max_block_frames * channels * 8],
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

        let progress = g.src.process(&g.scratch[..n], &mut g.resampled);
        debug_assert_eq!(
            progress.consumed, n,
            "resampled scratch undersized for one block"
        );

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
    use crate::sample::{Format, GroupSpec, OutputSpec};

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
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

    #[test]
    fn unity_gain_same_format_passes_samples_through() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();

        // Push enough blocks for the gain ramp (10ms time constant) to fully settle.
        let frames = vec![0.5f32; 64 * 2];
        for _ in 0..64 {
            mixer.push_group(GroupId(1), &frames);
            let mut out = vec![0.0f32; 64 * 2];
            mixer.take_output(OutputId(1), &mut out);
        }

        mixer.push_group(GroupId(1), &frames);
        let mut out = vec![0.0f32; 64 * 2];
        let n = mixer.take_output(OutputId(1), &mut out);
        assert_eq!(n, frames.len());
        for &s in &out {
            assert!((s - 0.5).abs() < 1e-4, "expected ~0.5, got {s}");
        }
    }

    #[test]
    fn zero_gain_settles_to_silence() {
        let topo = single_group_topology(0.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();

        let frames = vec![1.0f32; 64 * 2];
        for _ in 0..64 {
            mixer.push_group(GroupId(1), &frames);
            let mut out = vec![0.0f32; 64 * 2];
            mixer.take_output(OutputId(1), &mut out);
        }

        mixer.push_group(GroupId(1), &frames);
        let mut out = vec![-1.0f32; 64 * 2];
        mixer.take_output(OutputId(1), &mut out);
        for &s in &out {
            assert!(s.abs() < 1e-3, "expected ~0.0, got {s}");
        }
    }

    #[test]
    fn take_output_resets_accumulator_between_ticks() {
        let topo = single_group_topology(1.0, false, 1.0);
        let mut mixer = Mixer::new(&topo, 256).unwrap();
        let frames = vec![0.3f32; 32 * 2];

        mixer.push_group(GroupId(1), &frames);
        let mut out1 = vec![0.0f32; 32 * 2];
        let n1 = mixer.take_output(OutputId(1), &mut out1);

        // No push_group this tick — accumulator must read back empty, not stale.
        let mut out2 = vec![9.0f32; 32 * 2];
        let n2 = mixer.take_output(OutputId(1), &mut out2);
        assert_eq!(n1, frames.len());
        assert_eq!(n2, 0);
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
}
