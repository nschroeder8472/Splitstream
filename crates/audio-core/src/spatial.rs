//! Fixed virtual-speaker binaural rendering (Sonar/HeSuVi-style virtual
//! surround) — an alternative N->2 render stage beside [`crate::ChannelMatrix`],
//! selected via [`crate::Render`]. See `.lattice/context/spatial-audio.md`.
//!
//! `HrirSet`'s impulse data is procedurally synthesized, not measured — see
//! `hrir_data`'s module doc and the context doc's 2026-07-20 decision log for
//! why (no network access to fetch a real public-domain HRIR set this
//! session).

mod hrir_data;

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::dsp::db_to_linear;
use crate::sample::{speaker, ChannelLayout};

const LFE_GAIN_DB: f32 = -6.0;

/// Folds an azimuth (degrees, 0 = front, positive = right) to its
/// spherical-head interaural equivalent: the ITD/ILD effect peaks at ±90°
/// (directly to the side) and falls back to zero at both 0° (front) and
/// ±180° (directly behind) — physically correct for a symmetric head model,
/// even without the pinna cues that would normally disambiguate front/back.
fn fold_azimuth(azimuth_deg: f32) -> f32 {
    let a = azimuth_deg.abs();
    if a <= 90.0 {
        azimuth_deg
    } else {
        (180.0 - a) * azimuth_deg.signum()
    }
}

/// Synthesizes one position's (left, right) impulse pair: a unit impulse on
/// the near ear, a delayed/attenuated/HF-shadowed impulse on the far ear.
/// Woodworth-Schlosberg ITD model + a linear ILD ramp — see `hrir_data`'s
/// module doc for why this is procedural, not measured, data.
fn synth_pair(azimuth_deg: f32, sample_rate: u32, taps: usize) -> (Vec<f32>, Vec<f32>) {
    let mut left = vec![0.0f32; taps];
    let mut right = vec![0.0f32; taps];

    let theta_eff = fold_azimuth(azimuth_deg).to_radians();
    let itd_s = (hrir_data::HEAD_RADIUS_M / hrir_data::SPEED_OF_SOUND_MPS)
        * (theta_eff.abs() + theta_eff.abs().sin());
    let itd_samples = (itd_s * sample_rate as f32).round() as usize;
    let shadow_frac = (theta_eff.abs() / std::f32::consts::FRAC_PI_2).min(1.0);
    let far_gain = db_to_linear(-hrir_data::HEAD_SHADOW_MAX_DB * shadow_frac);

    // azimuth >= 0 => source to the right => right ear is near (reference,
    // zero delay, unity gain); left ear is far (delayed, shadowed).
    let (near, far) = if azimuth_deg >= 0.0 {
        (&mut right, &mut left)
    } else {
        (&mut left, &mut right)
    };
    near[0] = 1.0;

    // Spreads the far ear's energy across a short decaying kernel instead of
    // one sample -- a crude stand-in for head-shadow HF rolloff. Wider
    // angles decay faster (more high-frequency loss), never negative.
    let decay = (0.85 - 0.5 * shadow_frac).max(0.1);
    let far_start = itd_samples.min(taps.saturating_sub(1));
    let mut remaining = far_gain;
    for k in 0..hrir_data::SHADOW_KERNEL_TAPS {
        let idx = far_start + k;
        if idx >= taps {
            break;
        }
        far[idx] += remaining;
        remaining *= decay;
    }

    (left, right)
}

/// Immutable value object: per fixed virtual-speaker position, a (left ear,
/// right ear) impulse pair at `sample_rate`. Construct via
/// [`HrirSet::embedded`] — off-RT (graph build time) only.
pub struct HrirSet {
    sample_rate: u32,
    taps: usize,
    /// Parallel to `hrir_data::POSITIONS`.
    pairs: Vec<(Vec<f32>, Vec<f32>)>,
}

impl HrirSet {
    /// Synthesizes every fixed position's impulse pair directly at
    /// `sample_rate` (see this module's doc comment on why this is
    /// procedural synthesis, not a decode-and-resample of measured data).
    /// Infallible, off-RT only.
    pub fn embedded(sample_rate: u32) -> HrirSet {
        let taps = Self::taps_for(sample_rate);
        let pairs = hrir_data::POSITIONS
            .iter()
            .map(|p| synth_pair(p.azimuth_deg, sample_rate, taps))
            .collect();
        HrirSet { sample_rate, taps, pairs }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn taps(&self) -> usize {
        self.taps
    }

    /// The tap count `embedded(sample_rate)` would build, without actually
    /// synthesizing the impulse pairs — a pure formula, cheap enough for
    /// logging/telemetry call sites that only need the number.
    pub fn taps_for(sample_rate: u32) -> usize {
        ((sample_rate as f32) * hrir_data::IMPULSE_DURATION_S).round().max(1.0) as usize
    }

    /// Left/right impulse pair for `speaker_bit`: a direct hit against
    /// `hrir_data::POSITIONS`, else folded via `hrir_data::FOLD`, else `FC`
    /// as a neutral default when no direction is knowable at all (notes:
    /// every construction path here is infallible, matching the rest of
    /// this crate's "never lose audio, never error on an unknown position"
    /// convention — see `channel.rs`'s `fold_targets`).
    fn pair_for(&self, speaker_bit: u32) -> &(Vec<f32>, Vec<f32>) {
        let direct = hrir_data::POSITIONS.iter().position(|p| p.speaker == speaker_bit);
        let resolved = direct
            .or_else(|| {
                hrir_data::FOLD
                    .iter()
                    .find(|(from, _)| *from == speaker_bit)
                    .and_then(|(_, to)| hrir_data::POSITIONS.iter().position(|p| p.speaker == *to))
            })
            .unwrap_or_else(|| {
                hrir_data::POSITIONS
                    .iter()
                    .position(|p| p.speaker == speaker::FC)
                    .expect("FC is always present in hrir_data::POSITIONS")
            });
        &self.pairs[resolved]
    }
}

/// One channel -> one ear: uniformly-partitioned overlap-save FFT
/// convolution (frequency-delay-line accumulation). Pre-planned `rustfft`
/// FFTs and pre-allocated scratch/state at construction; `process` never
/// allocates. Impulse length is arbitrary relative to `partition` (any
/// number of partitions) -- longer future impulse responses (e.g. a BRIR
/// profile) need no engine changes, only a longer `impulse` at construction.
pub struct PartitionedConvolver {
    partition: usize,
    fft_len: usize,
    num_partitions: usize,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    /// Frequency-domain impulse partitions, fixed at construction.
    filters_freq: Vec<Vec<Complex<f32>>>,
    /// Frequency-delay line: `fdl[0]` is this block's input window, `fdl[j]`
    /// is `j` blocks ago's -- shifted (not recomputed) each block.
    fdl: Vec<Vec<Complex<f32>>>,
    scratch: Vec<Complex<f32>>,
    /// Reused `fft_len`-sized buffer: built as `[prev_block, new_block]`,
    /// FFT'd in place, then copied into `fdl[0]`.
    window: Vec<Complex<f32>>,
    accum: Vec<Complex<f32>>,
    /// Previous input block's real samples -- the overlap-save "history"
    /// half of the next window.
    prev_block: Vec<f32>,
    in_buf: Vec<f32>,
    in_len: usize,
    /// Fixed-capacity output ring, primed with `partition` silence samples
    /// at construction so `process` always returns exactly as many samples
    /// as it's given, even before the first block has actually run (notes:
    /// this is the "output delayed one partition, primed with silence"
    /// contract).
    out_buf: Vec<f32>,
    out_head: usize,
    out_len: usize,
}

impl PartitionedConvolver {
    /// `partition`: power of two. `impulse` may be any length (BRIR-ready).
    pub fn new(impulse: &[f32], partition: usize) -> PartitionedConvolver {
        let partition = partition.max(1);
        let fft_len = partition * 2;
        let num_partitions = impulse.len().div_ceil(partition).max(1);

        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_len);
        let ifft = planner.plan_fft_inverse(fft_len);
        let scratch_len = fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len());
        let mut build_scratch = vec![Complex::new(0.0f32, 0.0); scratch_len];

        let filters_freq = (0..num_partitions)
            .map(|k| {
                let start = k * partition;
                let end = impulse.len().min(start + partition);
                let mut buf = vec![Complex::new(0.0f32, 0.0); fft_len];
                if start < end {
                    for (i, &s) in impulse[start..end].iter().enumerate() {
                        buf[i] = Complex::new(s, 0.0);
                    }
                }
                fft.process_with_scratch(&mut buf, &mut build_scratch);
                buf
            })
            .collect();

        // Capacity = 2*partition; proven backlog bound is partition+1. Already
        // zero-initialized, so priming the first `partition` output samples
        // as silence is just starting `out_len` there -- no write needed.
        let out_buf = vec![0.0f32; fft_len];

        PartitionedConvolver {
            partition,
            fft_len,
            num_partitions,
            fft,
            ifft,
            filters_freq,
            fdl: vec![vec![Complex::new(0.0f32, 0.0); fft_len]; num_partitions],
            scratch: vec![Complex::new(0.0f32, 0.0); scratch_len],
            window: vec![Complex::new(0.0f32, 0.0); fft_len],
            accum: vec![Complex::new(0.0f32, 0.0); fft_len],
            prev_block: vec![0.0f32; partition],
            in_buf: vec![0.0f32; partition],
            in_len: 0,
            out_buf,
            out_head: 0,
            out_len: partition,
        }
    }

    /// Mono, in/out same length; internal block buffering, alloc-free.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), output.len());
        let n = input.len().min(output.len());
        for i in 0..n {
            self.in_buf[self.in_len] = input[i];
            self.in_len += 1;
            if self.in_len == self.partition {
                self.run_block();
                self.in_len = 0;
            }
            output[i] = self.pop_output();
        }
    }

    fn run_block(&mut self) {
        for i in 0..self.partition {
            self.window[i] = Complex::new(self.prev_block[i], 0.0);
            self.window[self.partition + i] = Complex::new(self.in_buf[i], 0.0);
        }
        self.fft.process_with_scratch(&mut self.window, &mut self.scratch);

        for j in (1..self.num_partitions).rev() {
            self.fdl.swap(j, j - 1);
        }
        self.fdl[0].copy_from_slice(&self.window);

        for c in self.accum.iter_mut() {
            *c = Complex::new(0.0, 0.0);
        }
        for j in 0..self.num_partitions {
            for (a, (x, h)) in self
                .accum
                .iter_mut()
                .zip(self.fdl[j].iter().zip(self.filters_freq[j].iter()))
            {
                *a += x * h;
            }
        }

        self.ifft.process_with_scratch(&mut self.accum, &mut self.scratch);

        let norm = 1.0 / self.fft_len as f32;
        for i in 0..self.partition {
            let sample = self.accum[self.partition + i].re * norm;
            self.push_output(sample);
        }

        self.prev_block.copy_from_slice(&self.in_buf);
    }

    fn push_output(&mut self, x: f32) {
        let idx = (self.out_head + self.out_len) % self.out_buf.len();
        self.out_buf[idx] = x;
        self.out_len += 1;
    }

    fn pop_output(&mut self) -> f32 {
        let v = self.out_buf[self.out_head];
        self.out_head = (self.out_head + 1) % self.out_buf.len();
        self.out_len -= 1;
        v
    }
}

struct PositionedChannel {
    /// This channel's index into a source-layout interleaved frame.
    index: usize,
    left: PartitionedConvolver,
    right: PartitionedConvolver,
}

/// N->2 binaural renderer: per positioned channel, a convolver pair; LFE
/// mixed flat (no convolver) into both ears. Same `process` call contract as
/// [`crate::ChannelMatrix`].
pub struct Spatializer {
    channels: Vec<PositionedChannel>,
    lfe_index: Option<usize>,
    in_ch: usize,
    mono_in: Vec<f32>,
    conv_left: Vec<f32>,
    conv_right: Vec<f32>,
}

impl Spatializer {
    /// Infallible. Positioned channels get a convolver pair via
    /// `HrirSet::pair_for` (nearest-standard-position fallback baked in
    /// there); LFE is detected and mixed flat, never convolved. Partition =
    /// next power of two >= `max_block_frames`.
    pub fn new(from: ChannelLayout, hrirs: &HrirSet, max_block_frames: usize) -> Spatializer {
        let partition = max_block_frames.max(1).next_power_of_two();
        let max_block_frames = max_block_frames.max(1);
        let speakers = from.speakers();
        let in_ch = speakers.len().max(1);

        let mut channels = Vec::with_capacity(speakers.len());
        let mut lfe_index = None;
        for (index, &spk) in speakers.iter().enumerate() {
            if spk == speaker::LFE {
                lfe_index = Some(index);
                continue;
            }
            let (l, r) = hrirs.pair_for(spk);
            channels.push(PositionedChannel {
                index,
                left: PartitionedConvolver::new(l, partition),
                right: PartitionedConvolver::new(r, partition),
            });
        }

        Spatializer {
            channels,
            lfe_index,
            in_ch,
            mono_in: vec![0.0f32; max_block_frames],
            conv_left: vec![0.0f32; max_block_frames],
            conv_right: vec![0.0f32; max_block_frames],
        }
    }

    /// `input`: whole frames at `from`'s channel count -> `output`: same
    /// frame count at 2 channels (stereo). Returns samples (not frames)
    /// written. Always overwrites `output`, never accumulates into it (same
    /// convention as `ChannelMatrix::process`).
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        debug_assert_eq!(input.len() % self.in_ch, 0);
        debug_assert_eq!(output.len() % 2, 0);
        let frame_count = (input.len() / self.in_ch).min(output.len() / 2);
        for s in output[..frame_count * 2].iter_mut() {
            *s = 0.0;
        }

        let in_ch = self.in_ch;
        let mono_in = &mut self.mono_in;
        let conv_left = &mut self.conv_left;
        let conv_right = &mut self.conv_right;

        for pc in self.channels.iter_mut() {
            for f in 0..frame_count {
                mono_in[f] = input[f * in_ch + pc.index];
            }
            pc.left.process(&mono_in[..frame_count], &mut conv_left[..frame_count]);
            pc.right.process(&mono_in[..frame_count], &mut conv_right[..frame_count]);
            for f in 0..frame_count {
                output[f * 2] += conv_left[f];
                output[f * 2 + 1] += conv_right[f];
            }
        }

        if let Some(lfe_idx) = self.lfe_index {
            let gain = db_to_linear(LFE_GAIN_DB);
            for f in 0..frame_count {
                let s = input[f * in_ch + lfe_idx] * gain;
                output[f * 2] += s;
                output[f * 2 + 1] += s;
            }
        }

        frame_count * 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrir_set_taps_scale_with_sample_rate_at_constant_duration() {
        let at_44k = HrirSet::embedded(44_100);
        let at_48k = HrirSet::embedded(48_000);
        assert_eq!(at_44k.taps(), 128);
        // ~2.9ms at 48kHz -> ~139 taps, not 128 -- duration is held constant,
        // not tap count.
        assert!(at_48k.taps() > at_44k.taps());
    }

    #[test]
    fn front_center_position_has_no_interaural_delay_or_shadow() {
        let hrirs = HrirSet::embedded(48_000);
        let (l, r) = hrirs.pair_for(speaker::FC);
        assert_eq!(l[0], 1.0);
        assert_eq!(r[0], 1.0);
    }

    #[test]
    fn a_right_side_position_delays_and_attenuates_the_left_ear() {
        let hrirs = HrirSet::embedded(48_000);
        let (l, r) = hrirs.pair_for(speaker::FR);
        assert_eq!(r[0], 1.0, "near (right) ear must be the unity reference");
        assert_eq!(l[0], 0.0, "far (left) ear must be delayed, not instant");
        let far_peak = l.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
        assert!(far_peak > 0.0, "far ear must carry some energy");
        assert!(far_peak < 1.0, "far ear's peak must be attenuated relative to the near ear's unity peak");
    }

    #[test]
    fn left_and_right_mirrored_positions_are_symmetric() {
        let hrirs = HrirSet::embedded(48_000);
        let (fl_left, fl_right) = hrirs.pair_for(speaker::FL);
        let (fr_left, fr_right) = hrirs.pair_for(speaker::FR);
        assert_eq!(fl_left, fr_right, "FL's near ear must mirror FR's near ear");
        assert_eq!(fl_right, fr_left, "FL's far ear must mirror FR's far ear");
    }

    #[test]
    fn unknown_speaker_position_falls_back_to_front_center() {
        let hrirs = HrirSet::embedded(48_000);
        let fc = hrirs.pair_for(speaker::FC);
        let unknown = hrirs.pair_for(0x1_0000); // no named/folded rule
        assert_eq!(fc, unknown);
    }

    #[test]
    fn folded_position_resolves_to_its_documented_target() {
        let hrirs = HrirSet::embedded(48_000);
        let flc = hrirs.pair_for(speaker::FLC);
        let fl = hrirs.pair_for(speaker::FL);
        assert_eq!(flc, fl);
    }

    #[test]
    fn partitioned_convolver_with_a_unit_impulse_passes_input_through_delayed_one_partition() {
        // impulse = [1.0]: convolution with a unit impulse is identity, just
        // delayed by the partition's silence-priming.
        let mut conv = PartitionedConvolver::new(&[1.0], 8);
        let input = vec![1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut output = vec![0.0f32; 8];
        conv.process(&input, &mut output);
        assert_eq!(output, vec![0.0; 8], "first partition must be primed silence");

        let mut output2 = vec![0.0f32; 8];
        conv.process(&input, &mut output2);
        for (got, expected) in output2.iter().zip(input.iter()) {
            assert!((got - expected).abs() < 1e-4, "expected {expected}, got {got}");
        }
    }

    #[test]
    fn partitioned_convolver_matches_a_hand_computed_two_tap_average() {
        // impulse = [0.5, 0.5]: a simple 2-tap moving average, single
        // partition (impulse len <= partition). Feed an impulse train and
        // check the known closed-form result once primed.
        let mut conv = PartitionedConvolver::new(&[0.5, 0.5], 4);
        let block1 = vec![1.0f32, 0.0, 0.0, 0.0];
        let mut out1 = vec![0.0f32; 4];
        conv.process(&block1, &mut out1); // primed silence

        let block2 = vec![0.0f32; 4];
        let mut out2 = vec![0.0f32; 4];
        conv.process(&block2, &mut out2);
        // y[n] = 0.5*x[n] + 0.5*x[n-1]; x = [1,0,0,0,0,0,0,0,...]
        // block1 (x[0..4]=[1,0,0,0]) delayed one partition lands in out2:
        // y[0]=0.5*1+0.5*0=0.5, y[1]=0.5*0+0.5*1=0.5, y[2..]=0
        assert!((out2[0] - 0.5).abs() < 1e-4, "y[0]={}", out2[0]);
        assert!((out2[1] - 0.5).abs() < 1e-4, "y[1]={}", out2[1]);
        assert!(out2[2].abs() < 1e-4);
        assert!(out2[3].abs() < 1e-4);
    }

    #[test]
    fn partitioned_convolver_handles_an_impulse_longer_than_one_partition() {
        // impulse length 6 with partition 4 -> 2 partitions (BRIR-ready path).
        let impulse = vec![1.0f32, 0.0, 0.0, 0.0, 0.5, 0.0];
        let mut conv = PartitionedConvolver::new(&impulse, 4);
        let mut collected = Vec::new();
        let silence = vec![0.0f32; 4];
        let mut impulse_in = vec![0.0f32; 4];
        impulse_in[0] = 1.0;

        let mut out = vec![0.0f32; 4];
        conv.process(&impulse_in, &mut out);
        collected.extend_from_slice(&out);
        for _ in 0..3 {
            conv.process(&silence, &mut out);
            collected.extend_from_slice(&out);
        }

        // Expect the impulse response itself (delayed one partition = 4
        // samples of silence, then the impulse taps) to appear in `collected`.
        assert!(collected[4..10].iter().zip(impulse.iter()).all(|(&g, &e)| (g - e).abs() < 1e-3));
    }

    #[test]
    fn stereo_widen_of_a_non_silent_signal_produces_non_silent_output() {
        let hrirs = HrirSet::embedded(48_000);
        let mut spatializer = Spatializer::new(ChannelLayout::STEREO, &hrirs, 64);
        let input = vec![0.5f32; 64 * 2];
        let mut output = vec![0.0f32; 64 * 2];

        // Feed enough blocks to clear the priming silence.
        let mut any_nonzero = false;
        for _ in 0..4 {
            spatializer.process(&input, &mut output);
            if output.iter().any(|&s| s.abs() > 1e-4) {
                any_nonzero = true;
            }
        }
        assert!(any_nonzero, "spatialized stereo output stayed silent");
    }

    #[test]
    fn lfe_channel_mixes_flat_into_both_ears_at_minus_six_db_never_convolved() {
        let hrirs = HrirSet::embedded(48_000);
        let mut spatializer = Spatializer::new(ChannelLayout::SURROUND_5_1, &hrirs, 4);
        // 5.1 order: FL FR FC LFE BL BR -- only LFE hot.
        let mut input = vec![0.0f32; 4 * 6];
        for f in 0..4 {
            input[f * 6 + 3] = 1.0;
        }
        let mut output = vec![0.0f32; 4 * 2];
        let produced = spatializer.process(&input, &mut output);
        assert_eq!(produced, 8);
        let expected = db_to_linear(LFE_GAIN_DB);
        for f in 0..4 {
            assert!((output[f * 2] - expected).abs() < 1e-4, "L[{f}]={}", output[f * 2]);
            assert!((output[f * 2 + 1] - expected).abs() < 1e-4, "R[{f}]={}", output[f * 2 + 1]);
        }
    }
}
