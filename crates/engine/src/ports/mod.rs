//! Port traits `win-audio` implements. Defined here, not in `win-audio`,
//! because `win-audio` carries `windows-rs` — this crate (and its graph
//! logic) must compile and unit-test on any platform (spec §6, N5).

use std::time::Duration;

use audio_core::Format;

#[cfg(any(test, feature = "test-support"))]
pub mod mock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    Bus,
    Physical,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub id: EndpointId,
    pub name: String,
    pub kind: EndpointKind,
    pub format: Format,
}

#[derive(Debug)]
pub enum PortError {
    DeviceInvalidated,
    NotFound(EndpointId),
    Backend(String),
}

/// RAII thread-priority promotion (MMCSS "Pro Audio" in the real `win-audio`
/// impl). Defined here, not in `win-audio`, so `AudioSystem`'s signature
/// doesn't leak a windows-rs type into this crate. The real implementation
/// hands `RtGuard::new` a closure that reverts the promotion; mocks use `noop`.
pub struct RtGuard {
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl RtGuard {
    pub fn noop() -> RtGuard {
        RtGuard { on_drop: None }
    }

    pub fn new(on_drop: impl FnOnce() + Send + 'static) -> RtGuard {
        RtGuard {
            on_drop: Some(Box::new(on_drop)),
        }
    }
}

impl Drop for RtGuard {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

pub trait AudioSystem: Send + Sync {
    fn enumerate(&self) -> Result<Vec<Endpoint>, PortError>;
    fn open_capture(&self, id: &EndpointId) -> Result<Box<dyn CapturePort>, PortError>;
    fn open_render(&self, id: &EndpointId) -> Result<Box<dyn RenderPort>, PortError>;
    fn promote_rt_thread(&self) -> RtGuard;
}

/// Polled ~period/2 (spec Appendix A) — loopback event mode is historically unreliable.
pub trait CapturePort: Send {
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, PortError>;
    fn format(&self) -> Format;
    fn poll_interval(&self) -> Duration;
}

/// Event-driven shared mode — the render side is the pull clock (spec §7.1).
pub trait RenderPort: Send {
    fn wait_event(&mut self, timeout: Duration) -> Result<(), PortError>;
    fn write(&mut self, frames: &[f32]) -> Result<(), PortError>;
    fn format(&self) -> Format;
    fn period_frames(&self) -> usize;
}
