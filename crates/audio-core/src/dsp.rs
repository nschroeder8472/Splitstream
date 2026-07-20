//! Per-group DSP stages — parametric EQ and limiter — cascaded in a
//! `DspChain` that runs after gain, before the channel matrix (notes §17:
//! DSP stays at source layout, not output layout). Pure and pre-allocated at
//! construction: no allocation once built, runs on the RT mixer thread like
//! everything else in `audio-core`.

use crate::sample::{DomainError, Format};
use crate::smoothing::Smoothed;

/// ~10ms smoothing for every audible DSP parameter — bypass wet/dry
/// crossfade, EQ band freq/gain/Q, limiter ceiling (notes §8); an instant
/// step on any of these is an audible click.
const PARAM_TIME_CONSTANT_S: f32 = 0.01;
/// EQ biquad coefficients are recomputed from smoothed parameters once per
/// this many frames, not per sample (notes §8) — trig calls are too costly
/// to repeat every sample, and audible parameter motion doesn't need it.
const EQ_RECOMPUTE_SUB_BLOCK: usize = 32;
/// Denormal flush threshold (notes §9): filter feedback state below this
/// magnitude is treated as silence before it decays into denormal range,
/// which would otherwise spike CPU 10-100x on real hardware.
const DENORMAL_FLOOR: f32 = 1.0e-15;
const LIMITER_ATTACK_MS: f32 = 2.0;
const LIMITER_RELEASE_MS: f32 = 50.0;
/// Floor under a measured peak before dividing by it — avoids a divide-by-
/// (near-)zero producing a huge, meaningless target gain on silence.
const PEAK_FLOOR: f32 = 1.0e-6;

/// One stage in a per-group `DspChain`, or the always-on per-output headroom
/// limiter. `process` runs on the RT mixer thread every tick — implementations
/// must not allocate or block.
pub trait DspStage: Send {
    fn process(&mut self, buf: &mut [f32], fmt: Format);
    /// Sets a new target for a stage-specific parameter; the stage smooths
    /// its own transition internally (never a stepped change).
    fn set_param(&mut self, param: DspParam);
    /// Ramped wet/dry crossfade (~10ms), not an instant switch.
    fn set_bypass(&mut self, bypassed: bool);
    /// Clears internal filter/envelope state (fresh silence in, fresh out).
    fn reset(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqBandSpec {
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

#[derive(Debug, Clone, Copy)]
pub enum DspParam {
    EqBand { band: usize, spec: EqBandSpec },
    LimiterCeilingDb(f32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DspSpec {
    Eq { bands: Vec<EqBandSpec> },
    Limiter { ceiling_db: f32 },
}

pub(crate) fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn validate_eq_band(band: &EqBandSpec, sample_rate: u32) -> Result<(), DomainError> {
    let nyquist = sample_rate as f32 / 2.0;
    if band.freq_hz > 0.0 && band.freq_hz < nyquist && band.q > 0.0 && band.q.is_finite() {
        Ok(())
    } else {
        Err(DomainError::InvalidEqBand {
            freq_hz: band.freq_hz,
            q: band.q,
        })
    }
}

/// Wet/dry crossfade wrapper shared by every stage: `dry` is a pre-allocated
/// scratch copy of the input, taken before processing, so bypass can ramp
/// without an instant click while the stage itself keeps running (state
/// stays warm — same idea as the mixer's mute, notes §8).
struct BypassRamp {
    mix: Smoothed,
    dry: Vec<f32>,
}

impl BypassRamp {
    fn new(sample_rate: u32, max_block_samples: usize) -> BypassRamp {
        BypassRamp {
            mix: Smoothed::new(1.0, sample_rate, PARAM_TIME_CONSTANT_S),
            dry: vec![0.0; max_block_samples],
        }
    }

    fn capture_dry(&mut self, buf: &[f32]) {
        self.dry[..buf.len()].copy_from_slice(buf);
    }

    /// One `mix.next()` step per FRAME, not per interleaved element — `mix`'s
    /// coefficient assumes one call per sample-period; stepping it per
    /// element would make the ramp `channels` times faster than the
    /// documented ~10ms (review finding, dsp-pipeline P5).
    fn crossfade(&mut self, buf: &mut [f32], channels: usize) {
        let frame_count = buf.len() / channels;
        for f in 0..frame_count {
            let m = self.mix.next();
            let start = f * channels;
            for c in 0..channels {
                let i = start + c;
                buf[i] = self.dry[i] + m * (buf[i] - self.dry[i]);
            }
        }
    }

    fn set_bypassed(&mut self, bypassed: bool) {
        self.mix.set_target(if bypassed { 0.0 } else { 1.0 });
    }
}

/// RBJ Audio EQ Cookbook peaking filter, Transposed Direct Form II (best
/// numerical behavior in f32, notes §9). One instance per channel — z-state
/// must never be shared across interleaved channels.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn identity() -> Biquad {
        Biquad {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Coefficients computed in f64, stored f32 (notes §9). At `gain_db ==
    /// 0.0` this reduces to an exact identity filter (A == 1 makes the
    /// numerator and denominator coefficients equal before normalization) —
    /// the property `chain tests` below rely on for a deterministic,
    /// FFT-free "flat band passes through unchanged" case.
    fn set_coeffs_peaking(&mut self, freq_hz: f32, gain_db: f32, q: f32, sample_rate: u32) {
        let sr = sample_rate as f64;
        let w0 = 2.0 * std::f64::consts::PI * freq_hz as f64 / sr;
        let alpha = w0.sin() / (2.0 * q as f64);
        let a = 10f64.powf(gain_db as f64 / 40.0);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * w0.cos();
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * w0.cos();
        let a2 = 1.0 - alpha / a;

        self.b0 = (b0 / a0) as f32;
        self.b1 = (b1 / a0) as f32;
        self.b2 = (b2 / a0) as f32;
        self.a1 = (a1 / a0) as f32;
        self.a2 = (a2 / a0) as f32;
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    #[inline]
    fn flush_denormals(&mut self) {
        if self.z1.abs() < DENORMAL_FLOOR {
            self.z1 = 0.0;
        }
        if self.z2.abs() < DENORMAL_FLOOR {
            self.z2 = 0.0;
        }
    }
}

struct EqBand {
    freq: Smoothed,
    gain_db: Smoothed,
    q: Smoothed,
    /// One filter per channel, sharing the same coefficients (recomputed
    /// identically into each) but never sharing z-state.
    biquads: Vec<Biquad>,
}

impl EqBand {
    fn new(spec: EqBandSpec, sample_rate: u32, channels: usize) -> EqBand {
        EqBand {
            freq: Smoothed::new(spec.freq_hz, sample_rate, PARAM_TIME_CONSTANT_S),
            gain_db: Smoothed::new(spec.gain_db, sample_rate, PARAM_TIME_CONSTANT_S),
            q: Smoothed::new(spec.q, sample_rate, PARAM_TIME_CONSTANT_S),
            biquads: vec![Biquad::identity(); channels],
        }
    }

    /// Advances the smoothers `steps` times (one per real frame this chunk
    /// covers — `Smoothed`'s coefficient assumes one `.next()` per
    /// sample-period) before computing coefficients from wherever they land.
    /// Recomputing coefficients only once per chunk (not per step) is the
    /// intentional cost-saving (notes §8: trig per sub-block, not per
    /// sample) — advancing the *smoother* only once per chunk instead was a
    /// separate bug that made EQ params ramp `steps` times slower than the
    /// documented ~10ms (review finding, dsp-pipeline P5).
    fn recompute(&mut self, sample_rate: u32, steps: usize) {
        let mut freq = self.freq.next();
        let mut gain_db = self.gain_db.next();
        let mut q = self.q.next();
        for _ in 1..steps.max(1) {
            freq = self.freq.next();
            gain_db = self.gain_db.next();
            q = self.q.next();
        }
        for bq in self.biquads.iter_mut() {
            bq.set_coeffs_peaking(freq, gain_db, q, sample_rate);
        }
    }

    fn set_target(&mut self, spec: EqBandSpec) {
        self.freq.set_target(spec.freq_hz);
        self.gain_db.set_target(spec.gain_db);
        self.q.set_target(spec.q);
    }
}

/// Biquad cascade EQ — one `EqBand` per configured band, all in series.
pub struct ParametricEq {
    bands: Vec<EqBand>,
    channels: usize,
    sample_rate: u32,
    bypass: BypassRamp,
}

impl ParametricEq {
    pub fn new(
        bands: &[EqBandSpec],
        fmt: Format,
        max_block_frames: usize,
    ) -> Result<ParametricEq, DomainError> {
        for band in bands {
            validate_eq_band(band, fmt.sample_rate)?;
        }
        let channels = fmt.channels as usize;
        Ok(ParametricEq {
            bands: bands
                .iter()
                .map(|&spec| EqBand::new(spec, fmt.sample_rate, channels))
                .collect(),
            channels,
            sample_rate: fmt.sample_rate,
            bypass: BypassRamp::new(fmt.sample_rate, max_block_frames * channels),
        })
    }
}

impl DspStage for ParametricEq {
    fn process(&mut self, buf: &mut [f32], _fmt: Format) {
        self.bypass.capture_dry(buf);

        let frame_count = buf.len() / self.channels;
        let mut pos = 0;
        while pos < frame_count {
            let chunk_frames = EQ_RECOMPUTE_SUB_BLOCK.min(frame_count - pos);
            for band in self.bands.iter_mut() {
                band.recompute(self.sample_rate, chunk_frames);
            }
            for f in 0..chunk_frames {
                let frame_start = (pos + f) * self.channels;
                for ch in 0..self.channels {
                    let mut x = buf[frame_start + ch];
                    for band in self.bands.iter_mut() {
                        x = band.biquads[ch].process(x);
                    }
                    buf[frame_start + ch] = x;
                }
            }
            pos += chunk_frames;
        }

        for band in self.bands.iter_mut() {
            for bq in band.biquads.iter_mut() {
                bq.flush_denormals();
            }
        }

        self.bypass.crossfade(buf, self.channels);
    }

    fn set_param(&mut self, param: DspParam) {
        if let DspParam::EqBand { band, spec } = param {
            if let Some(b) = self.bands.get_mut(band) {
                b.set_target(spec);
            }
        }
    }

    fn set_bypass(&mut self, bypassed: bool) {
        self.bypass.set_bypassed(bypassed);
    }

    fn reset(&mut self) {
        for band in self.bands.iter_mut() {
            for bq in band.biquads.iter_mut() {
                *bq = Biquad::identity();
            }
        }
    }
}

/// Single-algorithm peak limiter, used in two placements (L2): as an optional
/// per-group `DspStage` inside a `DspChain`, and directly by the mixer as the
/// always-on per-output headroom limiter. Gain reduction is linked across
/// channels (one envelope, driven by the loudest channel each frame) rather
/// than per-channel independent — per-channel reduction would shift the
/// stereo image (same principle as the channel matrix's global normalization,
/// notes §17 GOTCHA 2).
pub struct Limiter {
    ceiling: Smoothed,
    envelope: f32,
    attack_coeff: f32,
    release_coeff: f32,
    channels: usize,
    bypass: BypassRamp,
    engaged: bool,
}

impl Limiter {
    pub fn new(ceiling_db: f32, fmt: Format, max_block_frames: usize) -> Limiter {
        let channels = fmt.channels as usize;
        Limiter {
            ceiling: Smoothed::new(db_to_linear(ceiling_db), fmt.sample_rate, PARAM_TIME_CONSTANT_S),
            envelope: 1.0,
            attack_coeff: (-1.0 / (LIMITER_ATTACK_MS / 1000.0 * fmt.sample_rate as f32)).exp(),
            release_coeff: (-1.0 / (LIMITER_RELEASE_MS / 1000.0 * fmt.sample_rate as f32)).exp(),
            channels,
            bypass: BypassRamp::new(fmt.sample_rate, max_block_frames * channels),
            engaged: false,
        }
    }

    /// Whether the limiter reduced gain at any point during the last `process` call.
    pub fn engaged(&self) -> bool {
        self.engaged
    }
}

impl DspStage for Limiter {
    fn process(&mut self, buf: &mut [f32], _fmt: Format) {
        self.bypass.capture_dry(buf);

        let frame_count = buf.len() / self.channels;
        let mut any_engaged = false;
        for f in 0..frame_count {
            let start = f * self.channels;
            let ceiling = self.ceiling.next();
            let peak = buf[start..start + self.channels]
                .iter()
                .fold(0.0f32, |acc, &s| acc.max(s.abs()));

            let target_gain = if peak > ceiling {
                ceiling / peak.max(PEAK_FLOOR)
            } else {
                1.0
            };
            let coeff = if target_gain < self.envelope {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.envelope = target_gain + coeff * (self.envelope - target_gain);
            if self.envelope < 0.999 {
                any_engaged = true;
            }

            for s in &mut buf[start..start + self.channels] {
                *s *= self.envelope;
            }
        }
        self.engaged = any_engaged;

        self.bypass.crossfade(buf, self.channels);
    }

    fn set_param(&mut self, param: DspParam) {
        if let DspParam::LimiterCeilingDb(db) = param {
            self.ceiling.set_target(db_to_linear(db));
        }
    }

    fn set_bypass(&mut self, bypassed: bool) {
        self.bypass.set_bypassed(bypassed);
    }

    fn reset(&mut self) {
        self.envelope = 1.0;
        self.engaged = false;
    }
}

/// Ordered per-group stages — EQ then limiter, matching `DspSpec` declaration
/// order. Pre-allocated at construction; `process` never allocates.
pub struct DspChain {
    stages: Vec<Box<dyn DspStage>>,
}

impl DspChain {
    pub fn new(
        specs: &[DspSpec],
        fmt: Format,
        max_block_frames: usize,
    ) -> Result<DspChain, DomainError> {
        let mut stages: Vec<Box<dyn DspStage>> = Vec::with_capacity(specs.len());
        for spec in specs {
            let stage: Box<dyn DspStage> = match spec {
                DspSpec::Eq { bands } => {
                    Box::new(ParametricEq::new(bands, fmt, max_block_frames)?)
                }
                DspSpec::Limiter { ceiling_db } => {
                    Box::new(Limiter::new(*ceiling_db, fmt, max_block_frames))
                }
            };
            stages.push(stage);
        }
        Ok(DspChain { stages })
    }

    pub fn process(&mut self, buf: &mut [f32], fmt: Format) {
        for stage in self.stages.iter_mut() {
            stage.process(buf, fmt);
        }
    }

    /// Unknown stage index is a no-op — same "stale command past a rebuild"
    /// tolerance as `Mixer::apply` (notes §7: an epoch-checked command can
    /// still race a chain shape it no longer matches).
    pub fn set_param(&mut self, stage: usize, param: DspParam) {
        if let Some(s) = self.stages.get_mut(stage) {
            s.set_param(param);
        }
    }

    pub fn set_bypass(&mut self, stage: usize, bypassed: bool) {
        if let Some(s) = self.stages.get_mut(stage) {
            s.set_bypass(bypassed);
        }
    }
}

/// Manual impl: `Box<dyn DspStage>` has no automatic `Debug` (the trait
/// doesn't require it — adding it would force every implementor to support
/// it for no real benefit). `MixerCommand::SwapChain` carries a `DspChain`
/// and derives `Debug`, so this only needs to be useful for logging, not exhaustive.
impl std::fmt::Debug for DspChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DspChain").field("stages", &self.stages.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
            layout: crate::sample::ChannelLayout::STEREO,
        }
    }

    fn run_blocks(stage: &mut dyn DspStage, fmt: Format, block: &[f32], blocks: usize) -> Vec<f32> {
        let mut last = block.to_vec();
        for _ in 0..blocks {
            last = block.to_vec();
            stage.process(&mut last, fmt);
        }
        last
    }

    #[test]
    fn zero_gain_eq_band_passes_signal_through_unchanged() {
        // At gain_db == 0.0 the peaking filter's numerator/denominator
        // coefficients are exactly equal before normalization (A == 1) — a
        // deterministic, FFT-free invariant instead of a tolerance check.
        let fmt = stereo(48_000);
        let band = EqBandSpec {
            freq_hz: 1000.0,
            gain_db: 0.0,
            q: 0.7,
        };
        let mut eq = ParametricEq::new(&[band], fmt, 256).unwrap();
        let input = vec![0.3f32, -0.6, 0.1, 0.9, -0.2, 0.4];
        let mut buf = input.clone();
        // Run enough blocks for the freq/gain/q smoothers (already at target
        // since constructed with it) and the bypass ramp (starts at 1.0
        // wet — no ramp needed) to have no remaining transient.
        eq.process(&mut buf, fmt);
        for (out, inp) in buf.iter().zip(input.iter()) {
            assert!((out - inp).abs() < 1e-4, "expected {inp}, got {out}");
        }
    }

    #[test]
    fn eq_rejects_a_band_at_or_above_nyquist() {
        let fmt = stereo(48_000);
        let bad = EqBandSpec {
            freq_hz: 24_000.0,
            gain_db: 3.0,
            q: 0.7,
        };
        assert!(matches!(
            ParametricEq::new(&[bad], fmt, 256),
            Err(DomainError::InvalidEqBand { .. })
        ));
    }

    #[test]
    fn eq_rejects_a_non_positive_q() {
        let fmt = stereo(48_000);
        let bad = EqBandSpec {
            freq_hz: 1000.0,
            gain_db: 3.0,
            q: 0.0,
        };
        assert!(matches!(
            ParametricEq::new(&[bad], fmt, 256),
            Err(DomainError::InvalidEqBand { .. })
        ));
    }

    #[test]
    fn biquad_flush_denormals_zeroes_subnormal_state_but_not_normal_state() {
        let mut bq = Biquad::identity();
        bq.z1 = 1e-16;
        bq.z2 = 0.5;
        bq.flush_denormals();
        assert_eq!(bq.z1, 0.0);
        assert_eq!(bq.z2, 0.5);
    }

    #[test]
    fn limiter_bypass_ramps_back_to_the_unreduced_dry_signal() {
        // Uses the limiter (not the EQ) for the bypass-ramp property: gain
        // reduction on a hot signal is an unambiguous, frequency-independent
        // "processed differs from dry" case to ramp away from.
        let fmt = stereo(48_000);
        let mut limiter = Limiter::new(-6.0, fmt, 256);
        let block = vec![1.0f32; 64 * 2];

        // Let the limiter settle into steady gain reduction first.
        for _ in 0..40 {
            let mut buf = block.clone();
            limiter.process(&mut buf, fmt);
        }
        assert!(limiter.engaged(), "test setup: limiter should be reducing gain here");

        limiter.set_bypass(true);
        let settled = run_blocks(&mut limiter, fmt, &block, 60);

        for (out, inp) in settled.iter().zip(block.iter()) {
            assert!((out - inp).abs() < 1e-2, "expected ~dry {inp}, got {out}");
        }
    }

    #[test]
    fn limiter_does_not_engage_below_ceiling() {
        let fmt = stereo(48_000);
        let mut limiter = Limiter::new(-1.0, fmt, 256); // ceiling well above signal
        let block = vec![0.1f32; 64 * 2];
        let mut buf = block.clone();
        limiter.process(&mut buf, fmt);
        assert!(!limiter.engaged());
        for (out, inp) in buf.iter().zip(block.iter()) {
            assert!((out - inp).abs() < 1e-4, "expected passthrough, got {out}");
        }
    }

    #[test]
    fn bypass_ramp_progress_after_a_fixed_frame_count_is_independent_of_channel_count() {
        // Regression test for a review finding: `BypassRamp::crossfade` used
        // to step its one-pole `mix` once per interleaved SAMPLE instead of
        // once per FRAME, making the ramp `channels` times faster for a
        // multi-channel group than for mono. Same fixed frame count, same
        // starting condition, channel counts far apart (1 vs 8) — how far
        // the ramp has progressed must match, not scale with channel count.
        fn progress_after_one_block(channels: u16) -> f32 {
            let fmt = Format {
                sample_rate: 48_000,
                channels,
                layout: crate::sample::ChannelLayout::default_for_count(channels),
            };
            let mut limiter = Limiter::new(-6.0, fmt, 256);
            let block = vec![1.0f32; 64 * channels as usize];
            for _ in 0..40 {
                let mut buf = block.clone();
                limiter.process(&mut buf, fmt);
            }
            assert!(limiter.engaged(), "test setup: limiter should be reducing gain here");

            limiter.set_bypass(true);
            // One block (64 frames) of ramp — nowhere near fully settled;
            // it's the *fraction* of the way back to dry (1.0) that must
            // match across channel counts, not the exact value.
            let mut buf = block.clone();
            limiter.process(&mut buf, fmt);
            buf[0]
        }

        let mono = progress_after_one_block(1);
        let seven_one = progress_after_one_block(8);
        assert!(
            (mono - seven_one).abs() < 0.05,
            "ramp progress after the same frame count shouldn't depend on channel count: mono {mono}, 7.1 {seven_one}"
        );
    }

    #[test]
    fn limiter_holds_output_at_or_below_ceiling_once_settled() {
        let fmt = stereo(48_000);
        let ceiling_db = -3.0;
        let mut limiter = Limiter::new(ceiling_db, fmt, 256);
        let ceiling_linear = db_to_linear(ceiling_db);
        let block = vec![1.0f32; 64 * 2]; // hot signal, well over ceiling

        let mut buf = block.clone();
        for _ in 0..40 {
            buf = block.clone();
            limiter.process(&mut buf, fmt);
        }

        assert!(limiter.engaged());
        for &s in &buf {
            assert!(
                s <= ceiling_linear + 1e-3,
                "expected <= ceiling {ceiling_linear}, got {s}"
            );
        }
    }

    #[test]
    fn dsp_chain_runs_stages_in_declared_order() {
        let fmt = stereo(48_000);
        let specs = vec![
            DspSpec::Eq {
                bands: vec![EqBandSpec {
                    freq_hz: 1000.0,
                    gain_db: 0.0,
                    q: 0.7,
                }],
            },
            DspSpec::Limiter { ceiling_db: 0.0 },
        ];
        let mut chain = DspChain::new(&specs, fmt, 256).unwrap();
        let mut buf = vec![0.2f32; 64 * 2];
        chain.process(&mut buf, fmt);
        // Flat EQ + ceiling above the signal: passthrough end to end.
        for &s in &buf {
            assert!((s - 0.2).abs() < 1e-3, "expected ~0.2, got {s}");
        }
    }

    #[test]
    fn dsp_chain_propagates_invalid_eq_band_as_domain_error() {
        let fmt = stereo(48_000);
        let specs = vec![DspSpec::Eq {
            bands: vec![EqBandSpec {
                freq_hz: -10.0,
                gain_db: 0.0,
                q: 1.0,
            }],
        }];
        assert!(matches!(
            DspChain::new(&specs, fmt, 256),
            Err(DomainError::InvalidEqBand { .. })
        ));
    }

    #[test]
    fn dsp_chain_set_param_on_unknown_stage_index_is_a_no_op_not_a_panic() {
        let fmt = stereo(48_000);
        let mut chain = DspChain::new(&[], fmt, 256).unwrap();
        chain.set_param(5, DspParam::LimiterCeilingDb(-1.0));
        chain.set_bypass(5, true);
    }
}
