//! Pure signal processing: no OS, no `windows-rs`. Compiles and tests on any platform.

mod channel;
mod dsp;
mod mixer;
mod resample;
mod sample;
mod smoothing;
mod spatial;

pub use channel::ChannelMatrix;
pub use dsp::{DspChain, DspParam, DspSpec, DspStage, EqBandSpec, Limiter, ParametricEq};
pub use mixer::{Mixer, MixerCommand, Render, Retired};
pub use resample::{Src, SrcProgress};
pub use sample::{
    ChannelLayout, DomainError, DuckSpec, Format, Gain, GroupId, GroupSpec, OutputId, OutputSpec,
    ResampleRatio, Topology,
};
pub use spatial::{HrirSet, PartitionedConvolver, Spatializer};
