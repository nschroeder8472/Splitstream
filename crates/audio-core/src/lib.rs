//! Pure signal processing: no OS, no `windows-rs`. Compiles and tests on any platform.

mod channel;
mod mixer;
mod resample;
mod sample;

pub use channel::ChannelMatrix;
pub use mixer::{Mixer, MixerCommand};
pub use resample::{Src, SrcProgress};
pub use sample::{
    ChannelLayout, DomainError, Format, Gain, GroupId, GroupSpec, OutputId, OutputSpec,
    ResampleRatio, Topology,
};
