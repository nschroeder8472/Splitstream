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
}

/// PCM format. Samples on every internal path are `f32`, interleaved by `channels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub u16);

#[derive(Debug, Clone)]
pub struct GroupSpec {
    pub id: GroupId,
    pub gain: Gain,
    pub follow_master: bool,
    pub output: OutputId,
    pub input_format: Format,
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
