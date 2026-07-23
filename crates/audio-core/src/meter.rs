//! Peak level metering (level-meters.md). Pure, RT-safe: `observe` is
//! alloc-free arithmetic advanced once per frame, same discipline as
//! `mixer.rs`'s `EnvFollower`/`DuckTargetGain` (a per-element step would make
//! the release time scale with channel count — the logged P5 review finding).
//!
//! One `PeakMeter` per group and per output lives inside the [`crate::Mixer`];
//! the engine reads [`PeakMeter::sample`] after each tick and publishes it via
//! atomics, exactly like `duck_depth_db`/`limiter_engaged`. Ballistics that
//! need smoothing live here (domain, unit-testable off-thread); the display-
//! only peak-hold dot stays in the UI.

/// A metering read: the smoothed peak level and whether the raw signal has hit
/// full scale recently. `peak` is **linear** amplitude (>= 0, can exceed 1.0
/// for a pre-limiter group tap) — the dBFS mapping and floor are the meter
/// widget's concern, not the domain's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterLevel {
    pub peak: f32,
    pub clipped: bool,
}

impl MeterLevel {
    pub const SILENT: MeterLevel = MeterLevel { peak: 0.0, clipped: false };
}

/// Meter fall time: how long a peak takes to decay once the signal drops.
/// ~300 ms reads as a natural VU-style fall — long enough to see a transient,
/// short enough to track a busy signal. Attack is instantaneous (a peak meter
/// must catch the peak the moment it happens), so there is no attack constant.
const METER_RELEASE_S: f32 = 0.3;

/// How long the clip indicator stays lit after the last full-scale sample.
/// 1 s is long enough for a glance to catch a brief clip without it flickering.
const CLIP_HOLD_S: f32 = 1.0;

/// Full-scale threshold — a raw frame peak at or above this lights the clip
/// indicator. 1.0 = 0 dBFS.
const CLIP_THRESHOLD: f32 = 1.0;

/// Below this linear level the envelope is snapped to exactly zero. An
/// exponential release never *reaches* zero, and because this meter keeps
/// advancing through silence (unlike `EnvFollower`, which simply stops when a
/// block is empty) the envelope would otherwise decay into subnormal floats
/// within ~30 s of silence and stay there for the process's lifetime —
/// subnormal arithmetic on the RT mixer thread, where flush-to-zero mode isn't
/// set. 1e-9 is −180 dBFS, three orders of magnitude below the meter widget's
/// −60 dBFS floor, so nothing visible is lost.
const SILENCE_EPSILON: f32 = 1.0e-9;

fn one_pole_coeff(time_constant_s: f32, sample_rate: u32) -> f32 {
    (-1.0 / (time_constant_s * sample_rate as f32)).exp()
}

/// Instant-attack, exponential-release peak follower with a clip-hold latch.
/// Instant attack means the envelope jumps straight to any higher frame peak;
/// release decays it back down one frame at a time. Holds no allocation and
/// touches no OS — safe to advance on the RT mixer thread.
pub struct PeakMeter {
    /// Smoothed linear peak envelope, the value [`sample`](Self::sample)
    /// reports. Instant attack, exponential release.
    env: f32,
    release: f32,
    /// Frames remaining before the clip latch clears. Reloaded to
    /// `clip_hold_frames` whenever a raw frame peak reaches full scale.
    clip_hold_left: u32,
    clip_hold_frames: u32,
}

impl PeakMeter {
    pub fn new(sample_rate: u32) -> PeakMeter {
        PeakMeter {
            env: 0.0,
            release: one_pole_coeff(METER_RELEASE_S, sample_rate),
            clip_hold_left: 0,
            clip_hold_frames: (CLIP_HOLD_S * sample_rate as f32) as u32,
        }
    }

    /// Feeds one tick's interleaved block, advancing the envelope once per
    /// frame (peak across `channels`) and reloading the clip latch on any
    /// full-scale frame. Alloc-free. A `channels` of 0 or a partial trailing
    /// frame is ignored via integer division, never a panic.
    pub fn observe(&mut self, block: &[f32], channels: usize) {
        if channels == 0 {
            return;
        }
        let frame_count = block.len() / channels;
        let mut clipped_frames = 0u32;
        for f in 0..frame_count {
            let start = f * channels;
            let frame_peak = block[start..start + channels]
                .iter()
                .fold(0.0f32, |acc, &s| acc.max(s.abs()));
            // Instant attack, exponential release — one step per frame.
            self.env = if frame_peak >= self.env {
                frame_peak
            } else {
                frame_peak + self.release * (self.env - frame_peak)
            };
            if frame_peak >= CLIP_THRESHOLD {
                clipped_frames += 1;
            }
        }
        // Once per block, not once per frame: the hot loop stays branch-free,
        // and flushing at the block boundary caps the envelope's subnormal
        // exposure at the tail of a single block — the next block starts from
        // a clean zero and stays there.
        self.flush_denormal();
        if clipped_frames > 0 {
            self.clip_hold_left = self.clip_hold_frames;
        } else {
            self.clip_hold_left = self.clip_hold_left.saturating_sub(frame_count as u32);
        }
    }

    /// Advances the meter across `frames` of silence without a buffer — the
    /// idle/muted path. Block-level [`observe`](Self::observe) can't decay a
    /// zero-length block (no frames = no time modelled), so a group that goes
    /// silent or an output with nothing summed this tick would freeze its bar
    /// at the last value. The caller feeds this the tick's nominal frame count
    /// instead. Closed-form (`release^frames`), so a whole idle tick is O(1).
    pub fn observe_silence(&mut self, frames: usize) {
        self.env *= self.release.powi(frames as i32);
        self.flush_denormal();
        self.clip_hold_left = self.clip_hold_left.saturating_sub(frames as u32);
    }

    /// Snaps a spent envelope to exactly zero — see [`SILENCE_EPSILON`].
    fn flush_denormal(&mut self) {
        if self.env < SILENCE_EPSILON {
            self.env = 0.0;
        }
    }

    pub fn sample(&self) -> MeterLevel {
        MeterLevel { peak: self.env, clipped: self.clip_hold_left > 0 }
    }

    pub fn reset(&mut self) {
        self.env = 0.0;
        self.clip_hold_left = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// A `frames`-long mono block of constant `value`.
    fn mono(value: f32, frames: usize) -> Vec<f32> {
        vec![value; frames]
    }

    #[test]
    fn a_fresh_meter_reads_silent() {
        let meter = PeakMeter::new(RATE);
        assert_eq!(meter.sample(), MeterLevel::SILENT);
    }

    #[test]
    fn observing_silence_stays_at_the_floor() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(0.0, 256), 1);
        assert_eq!(meter.sample().peak, 0.0);
    }

    #[test]
    fn attack_jumps_straight_to_the_block_peak() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(0.5, 128), 1);
        assert_eq!(meter.sample().peak, 0.5);
    }

    #[test]
    fn the_block_peak_is_the_max_across_channels() {
        let mut meter = PeakMeter::new(RATE);
        // Interleaved stereo: quiet left, loud right.
        let block = vec![0.1, 0.8, 0.1, 0.8];
        meter.observe(&block, 2);
        assert_eq!(meter.sample().peak, 0.8);
    }

    #[test]
    fn magnitude_uses_absolute_value_of_negative_samples() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(-0.7, 64), 1);
        assert_eq!(meter.sample().peak, 0.7);
    }

    #[test]
    fn release_decays_the_envelope_after_the_signal_stops() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(0.5, 64), 1);
        assert_eq!(meter.sample().peak, 0.5);

        meter.observe(&mono(0.0, 4_800), 1); // ~100 ms of silence
        let decayed = meter.sample().peak;
        assert!(decayed < 0.5, "envelope should fall, got {decayed}");
        assert!(decayed > 0.0, "should not have fully decayed yet, got {decayed}");
    }

    #[test]
    fn a_full_scale_sample_lights_the_clip_indicator() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(1.0, 16), 1);
        assert!(meter.sample().clipped);
    }

    #[test]
    fn the_clip_indicator_holds_briefly_then_clears() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(1.0, 16), 1);
        assert!(meter.sample().clipped);

        // Still lit a short while after (well inside the 1 s hold).
        meter.observe(&mono(0.0, 4_800), 1);
        assert!(meter.sample().clipped);

        // Past the full hold window it clears.
        meter.observe(&mono(0.0, RATE as usize + 1), 1);
        assert!(!meter.sample().clipped);
    }

    #[test]
    fn observe_silence_decays_a_frozen_bar() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(0.5, 64), 1);
        assert_eq!(meter.sample().peak, 0.5);

        meter.observe_silence(4_800); // ~100 ms idle tick(s)
        let decayed = meter.sample().peak;
        assert!(decayed < 0.5 && decayed > 0.0, "should decay toward floor, got {decayed}");
    }

    #[test]
    fn observe_silence_matches_observing_a_zero_buffer() {
        // The idle closed-form and feeding real silence must agree, so which
        // path a tick takes never shows a discontinuity in the bar.
        let mut via_silence = PeakMeter::new(RATE);
        let mut via_buffer = PeakMeter::new(RATE);
        via_silence.observe(&mono(0.7, 32), 1);
        via_buffer.observe(&mono(0.7, 32), 1);

        via_silence.observe_silence(480);
        via_buffer.observe(&mono(0.0, 480), 1);

        let a = via_silence.sample().peak;
        let b = via_buffer.sample().peak;
        assert!((a - b).abs() < 1e-6, "silence paths diverged: {a} vs {b}");
    }

    #[test]
    fn observe_silence_clears_a_held_clip() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(1.0, 8), 1);
        assert!(meter.sample().clipped);
        meter.observe_silence(RATE as usize + 1);
        assert!(!meter.sample().clipped);
    }

    #[test]
    fn a_long_idle_stretch_snaps_the_envelope_to_exactly_zero() {
        // An exponential release only *approaches* zero, so without the
        // flush-to-zero guard the envelope would sit in subnormal-float range
        // forever, doing subnormal arithmetic on the RT thread.
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(0.5, 64), 1);
        meter.observe_silence(RATE as usize * 10); // 10 s idle
        assert_eq!(meter.sample().peak, 0.0);
    }

    #[test]
    fn a_long_run_of_silent_blocks_snaps_the_envelope_to_exactly_zero() {
        // Same guarantee down the block-fed path — a source that keeps
        // delivering zero-filled buffers rather than going idle.
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(0.5, 64), 1);
        let silence = mono(0.0, 480);
        for _ in 0..1_000 {
            meter.observe(&silence, 1);
        }
        assert_eq!(meter.sample().peak, 0.0);
    }

    #[test]
    fn reset_returns_to_silent() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&mono(1.0, 64), 1);
        meter.reset();
        assert_eq!(meter.sample(), MeterLevel::SILENT);
    }

    #[test]
    fn a_zero_channel_block_is_ignored_not_a_panic() {
        let mut meter = PeakMeter::new(RATE);
        meter.observe(&[0.5, 0.5], 0);
        assert_eq!(meter.sample(), MeterLevel::SILENT);
    }
}
