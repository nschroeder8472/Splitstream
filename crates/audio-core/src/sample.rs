//! Frame/format value types shared across `audio-core`.

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum DomainError {
    #[error("invalid gain {0}: must be finite and non-negative")]
    InvalidGain(f32),
    #[error("group {group:?} targets output {output:?}, which is not in the topology")]
    DanglingOutputRef { group: GroupId, output: OutputId },
    #[error("resampler construction failed: {0}")]
    ResamplerInit(String),
    #[error("mismatched channel counts: {from} -> {to}")]
    ChannelMismatch { from: u16, to: u16 },
    #[error("invalid resample ratio {0}: must be finite and within {MIN_RESAMPLE_RATIO}..={MAX_RESAMPLE_RATIO}")]
    InvalidResampleRatio(f64),
    #[error("format has {channels} channels but its layout describes {layout_count}")]
    InvalidLayout { channels: u16, layout_count: u16 },
    #[error("invalid EQ band: freq {freq_hz}Hz must be in (0, nyquist) and q {q} must be positive")]
    InvalidEqBand { freq_hz: f32, q: f32 },
    #[error("group {group:?}'s duck triggers {trigger:?}, which is not in the topology")]
    DanglingDuckTrigger { group: GroupId, trigger: GroupId },
}

/// WASAPI `SPEAKER_*` bit values (mmreg.h / ksmedia.h) — this is the same
/// bit order `dwChannelMask` uses, so a mask read off a real device can be
/// stored directly as a `ChannelLayout`.
pub(crate) mod speaker {
    pub const FL: u32 = 0x1;
    pub const FR: u32 = 0x2;
    pub const FC: u32 = 0x4;
    pub const LFE: u32 = 0x8;
    pub const BL: u32 = 0x10;
    pub const BR: u32 = 0x20;
    pub const FLC: u32 = 0x40;
    pub const FRC: u32 = 0x80;
    pub const BC: u32 = 0x100;
    pub const SL: u32 = 0x200;
    pub const SR: u32 = 0x400;
}

/// Speaker-position set. Immutable value object — construct via
/// [`ChannelLayout::from_mask`] (real device) or
/// [`ChannelLayout::default_for_count`] (fallback when no mask is known).
/// Never fails to construct: an inconsistent or unrecognized mask falls back
/// to a channel-count default rather than erroring (see
/// `.lattice/context/channel-mixdown.md` — construction-never-fails is the
/// whole point of this feature).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelLayout(u32);

impl ChannelLayout {
    pub const MONO: ChannelLayout = ChannelLayout(speaker::FC);
    pub const STEREO: ChannelLayout = ChannelLayout(speaker::FL | speaker::FR);
    pub const QUAD: ChannelLayout =
        ChannelLayout(speaker::FL | speaker::FR | speaker::BL | speaker::BR);
    pub const SURROUND_5_1: ChannelLayout = ChannelLayout(
        speaker::FL | speaker::FR | speaker::FC | speaker::LFE | speaker::BL | speaker::BR,
    );
    pub const SURROUND_7_1: ChannelLayout =
        ChannelLayout(Self::SURROUND_5_1.0 | speaker::SL | speaker::SR);

    /// `mask == 0` or a set-bit count that disagrees with `channels` (both
    /// happen on real drivers) fall back to [`Self::default_for_count`]
    /// rather than producing a layout that lies about its own channel count.
    pub fn from_mask(mask: u32, channels: u16) -> ChannelLayout {
        if mask != 0 && mask.count_ones() == channels as u32 {
            ChannelLayout(mask)
        } else {
            Self::default_for_count(channels)
        }
    }

    /// Standard layout per channel count (matches common WASAPI mix-format
    /// defaults): 1=mono, 2=stereo, 3=FL/FR/FC, 4=quad, 5=5.0, 6=5.1, 7=6.1,
    /// 8=7.1. Above 8, the first 8 are the known 7.1 positions and the rest
    /// are unknown-position channels (folded into every output — see
    /// [`crate::ChannelMatrix`]), never colliding with a named speaker bit.
    pub fn default_for_count(channels: u16) -> ChannelLayout {
        use speaker::*;
        let mask = match channels {
            1 => FC,
            2 => FL | FR,
            3 => FL | FR | FC,
            4 => FL | FR | BL | BR,
            5 => FL | FR | FC | BL | BR,
            6 => FL | FR | FC | LFE | BL | BR,
            7 => FL | FR | FC | LFE | BL | BR | BC,
            n if n >= 8 => {
                let known = FL | FR | FC | LFE | BL | BR | SL | SR;
                let extra = (n - 8) as u32;
                let unknown_bits: u32 = (0..extra).map(|i| 1u32 << (11 + i)).sum();
                known | unknown_bits
            }
            _ => 0, // 0 channels: degenerate, count() reports 0 to match
        };
        ChannelLayout(mask)
    }

    /// Number of speaker positions in this layout — always matches the
    /// `channels` count it was built from via `from_mask`/`default_for_count`.
    pub fn count(&self) -> u16 {
        self.0.count_ones() as u16
    }

    /// Set bits in ascending order — this is the channel's column/row index
    /// into a [`crate::ChannelMatrix`], and matches WASAPI's own interleave
    /// order (ascending mask-bit order, not the bit number itself).
    pub(crate) fn speakers(&self) -> Vec<u32> {
        (0..32u32)
            .map(|b| 1u32 << b)
            .filter(|&bit| self.0 & bit != 0)
            .collect()
    }
}

/// PCM format. Samples on every internal path are `f32`, interleaved by `channels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
    pub layout: ChannelLayout,
}

/// Linear gain factor. Always finite and non-negative — construct via [`Gain::new`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gain(f32);

impl Gain {
    pub const UNITY: Gain = Gain(1.0);
    pub const SILENT: Gain = Gain(0.0);

    pub fn new(value: f32) -> Result<Gain, DomainError> {
        if value.is_finite() && value >= 0.0 {
            Ok(Gain(value))
        } else {
            Err(DomainError::InvalidGain(value))
        }
    }

    pub fn value(self) -> f32 {
        self.0
    }
}

/// Clamp for [`ResampleRatio`]: drift correction is a small continuous nudge,
/// never a large jump — anything outside this range means something upstream
/// (device format detection, control loop) is wrong, not a legitimate target.
pub const MIN_RESAMPLE_RATIO: f64 = 0.9;
pub const MAX_RESAMPLE_RATIO: f64 = 1.1;

/// Target ratio for [`crate::Src::set_ratio`] drift correction. Always finite
/// and within `0.9..=1.1` — construct via [`ResampleRatio::new`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResampleRatio(f64);

impl ResampleRatio {
    pub const UNITY: ResampleRatio = ResampleRatio(1.0);

    pub fn new(value: f64) -> Result<ResampleRatio, DomainError> {
        if value.is_finite() && (MIN_RESAMPLE_RATIO..=MAX_RESAMPLE_RATIO).contains(&value) {
            Ok(ResampleRatio(value))
        } else {
            Err(DomainError::InvalidResampleRatio(value))
        }
    }

    pub fn value(self) -> f64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub u16);

/// Cross-group sidechain config for the mixer-level `Ducker` (P5) — configured
/// on the **target** group: "this group's level drops when `trigger` carries
/// signal." See `.lattice/context/dsp-pipeline.md`'s ducking-topology decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuckSpec {
    pub trigger: GroupId,
    pub amount_db: f32,
    pub threshold_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

#[derive(Debug, Clone)]
pub struct GroupSpec {
    pub id: GroupId,
    pub gain: Gain,
    pub follow_master: bool,
    pub output: OutputId,
    pub input_format: Format,
    /// Per-group DSP chain (EQ, limiter), pre-allocated at `Mixer::new`/rebuild
    /// time — runs after gain, before the channel matrix (notes §17: DSP stays
    /// at source layout).
    pub dsp: Vec<crate::dsp::DspSpec>,
    pub duck: Option<DuckSpec>,
    /// Per-group virtual-surround/stereo-widen toggle (spatial-audio.md).
    /// `Render::build` falls back to the plain channel matrix automatically
    /// when this group's output isn't stereo.
    pub spatial: bool,
    /// Persisted per-group mute (per-group-mute-solo.md). Deliberately no
    /// `solo` counterpart: solo is session-only, so every rebuild starts each
    /// group unsoloed -- the absence of the field *is* the guarantee.
    pub mute: bool,
}

#[derive(Debug, Clone)]
pub struct OutputSpec {
    pub id: OutputId,
    pub format: Format,
}

#[derive(Debug, Clone)]
pub struct Topology {
    pub master: Gain,
    pub groups: Vec<GroupSpec>,
    pub outputs: Vec<OutputSpec>,
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn default_for_count_matches_named_consts() {
        assert_eq!(ChannelLayout::default_for_count(1), ChannelLayout::MONO);
        assert_eq!(ChannelLayout::default_for_count(2), ChannelLayout::STEREO);
        assert_eq!(ChannelLayout::default_for_count(4), ChannelLayout::QUAD);
        assert_eq!(ChannelLayout::default_for_count(6), ChannelLayout::SURROUND_5_1);
        assert_eq!(ChannelLayout::default_for_count(8), ChannelLayout::SURROUND_7_1);
    }

    #[test]
    fn count_matches_the_channel_count_it_was_built_from() {
        for n in 1..=16u16 {
            assert_eq!(ChannelLayout::default_for_count(n).count(), n, "count mismatch for {n}");
        }
    }

    #[test]
    fn from_mask_falls_back_on_zero_mask() {
        assert_eq!(ChannelLayout::from_mask(0, 6), ChannelLayout::SURROUND_5_1);
    }

    #[test]
    fn from_mask_falls_back_when_popcount_disagrees_with_channels() {
        // Real drivers report this (mask claims 2 channels, device says 6).
        let stereo_mask = ChannelLayout::STEREO.0;
        assert_eq!(
            ChannelLayout::from_mask(stereo_mask, 6),
            ChannelLayout::SURROUND_5_1
        );
    }

    #[test]
    fn from_mask_accepts_a_consistent_mask() {
        let mask = ChannelLayout::QUAD.0;
        assert_eq!(ChannelLayout::from_mask(mask, 4), ChannelLayout::QUAD);
    }

    #[test]
    fn speakers_are_in_ascending_bit_order_not_declaration_order() {
        // 5.1 mask bit order: FL(0x1) FR(0x2) FC(0x4) LFE(0x8) BL(0x10) BR(0x20)
        assert_eq!(
            ChannelLayout::SURROUND_5_1.speakers(),
            vec![
                speaker::FL,
                speaker::FR,
                speaker::FC,
                speaker::LFE,
                speaker::BL,
                speaker::BR
            ]
        );
    }

    #[test]
    fn beyond_seven_one_extra_channels_are_unknown_position_not_colliding_with_named_speakers() {
        let ten = ChannelLayout::default_for_count(10);
        assert_eq!(ten.count(), 10);
        let known = ChannelLayout::SURROUND_7_1;
        // every known 7.1 speaker bit is still set
        for &s in &known.speakers() {
            assert!(ten.speakers().contains(&s));
        }
    }
}
