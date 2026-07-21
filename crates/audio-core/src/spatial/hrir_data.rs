//! Fixed virtual-speaker layout and procedural-HRIR synthesis constants.
//!
//! **Deviation from the original blueprint** (`.lattice/context/spatial-audio.md`
//! Decisions Log, 2026-07-20): this was meant to hold ~7 KB of *measured*
//! 44.1 kHz HRIR tables (MIT KEMAR / SADIE II). The implementation session had
//! no network access to fetch either dataset, so `HrirSet::embedded` instead
//! *synthesizes* each position's ear pair procedurally, directly at the
//! target sample rate — a spherical-head ITD/ILD model (Woodworth-Schlosberg
//! delay + azimuth-scaled contralateral attenuation and HF rolloff), not a
//! measured HRTF. No pinna spectral notches or elevation cues; real
//! interaural time/level differences. This module holds that model's fixed
//! parameters instead of raw impulse arrays.
//!
//! The public shape (`HrirSet::embedded(sample_rate)`) is unchanged, so this
//! is swappable for real measured data later without touching any consumer.

use crate::sample::speaker;

/// One virtual speaker position: its WASAPI speaker bit and azimuth in
/// degrees (0 = front/center, positive = toward the right ear, matching the
/// sign convention `spatial::synth_pair` expects).
pub(crate) struct Position {
    pub speaker: u32,
    pub azimuth_deg: f32,
}

/// The 7 positions this fixed virtual-speaker set covers — every named
/// speaker in [`crate::sample::ChannelLayout::SURROUND_7_1`] except LFE
/// (LFE has no direction; [`crate::spatial::Spatializer`] mixes it flat into
/// both ears instead of looking it up here). Angles are a reasonable,
/// non-standards-body layout for a synthetic set, not a claimed ITU/HeSuVi
/// angle table.
pub(crate) const POSITIONS: [Position; 7] = [
    Position { speaker: speaker::FC, azimuth_deg: 0.0 },
    Position { speaker: speaker::FL, azimuth_deg: -30.0 },
    Position { speaker: speaker::FR, azimuth_deg: 30.0 },
    Position { speaker: speaker::SL, azimuth_deg: -90.0 },
    Position { speaker: speaker::SR, azimuth_deg: 90.0 },
    Position { speaker: speaker::BL, azimuth_deg: -135.0 },
    Position { speaker: speaker::BR, azimuth_deg: 135.0 },
];

/// Speaker bits this set has no direct position for, each folded to the
/// nearest [`POSITIONS`] entry. `BC` (directly behind) is equidistant from
/// `BL`/`BR` — folded to `BL` as an arbitrary, documented tie-break, not a
/// physically motivated choice. Any bit not covered by this table or
/// [`POSITIONS`] (an unknown-position channel beyond the named 11 — see
/// `ChannelLayout::default_for_count`) falls back to `FC`: a neutral center
/// placement when no direction is knowable at all.
pub(crate) const FOLD: [(u32, u32); 3] = [
    (speaker::FLC, speaker::FL),
    (speaker::FRC, speaker::FR),
    (speaker::BC, speaker::BL),
];

/// Average adult head radius, meters — Woodworth-Schlosberg ITD model input.
pub(crate) const HEAD_RADIUS_M: f32 = 0.0875;
pub(crate) const SPEED_OF_SOUND_MPS: f32 = 343.0;

/// Contralateral (far-ear) attenuation at the ±90° peak. Ramps linearly to
/// 0 dB at dead center/directly behind (see `synth::fold_azimuth`).
pub(crate) const HEAD_SHADOW_MAX_DB: f32 = 12.0;

/// Length, in taps, of the short decay kernel spreading the far ear's
/// impulse across a few samples — a crude stand-in for the head's HF
/// low-pass shadowing effect, scaled by [`HEAD_SHADOW_MAX_DB`]'s ramp.
pub(crate) const SHADOW_KERNEL_TAPS: usize = 4;

/// Same duration (~2.9 ms) as the blueprint's original 128-tap @44.1kHz
/// figure, applied at whatever `sample_rate` `HrirSet::embedded` is asked
/// for — keeps the convolution's latency contribution constant across
/// output sample rates rather than tap *count*.
pub(crate) const IMPULSE_DURATION_S: f32 = 128.0 / 44_100.0;
