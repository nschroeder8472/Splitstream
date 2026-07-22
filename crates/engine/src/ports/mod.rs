//! Port traits `win-audio` implements. Defined here, not in `win-audio`,
//! because `win-audio` carries `windows-rs` — this crate (and its graph
//! logic) must compile and unit-test on any platform (spec §6, N5).

use std::sync::mpsc::Receiver;
use std::time::Duration;

use audio_core::Format;

use crate::rules::SessionInfo;

#[cfg(any(test, feature = "test-support"))]
pub mod mock;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EndpointId(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub id: EndpointId,
    pub name: String,
    pub format: Format,
}

#[derive(Debug)]
pub enum PortError {
    DeviceInvalidated,
    NotFound(EndpointId),
    Backend(String),
}

/// `IMMNotificationClient` events, typed and decoupled from COM (drift-and-recovery L4).
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceEvent {
    Added(Endpoint),
    Removed(EndpointId),
    DefaultChanged(EndpointId),
    StateChanged(EndpointId),
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
    /// Per-process loopback capture (process-loopback-capture L4):
    /// `ActivateAudioInterfaceAsync` + `PROCESS_LOOPBACK` in the real
    /// `win-audio` impl — replaces the old per-endpoint `open_capture`.
    /// `include_tree` captures the process's child processes too (same
    /// activation parameter WASAPI exposes).
    fn open_process_capture(&self, pid: u32, include_tree: bool) -> Result<Box<dyn CapturePort>, PortError>;
    fn open_render(&self, id: &EndpointId) -> Result<Box<dyn RenderPort>, PortError>;
    fn promote_rt_thread(&self) -> RtGuard;
    /// Current default render endpoint — the supervisor's fallback target on device removal.
    fn default_output(&self) -> Result<Endpoint, PortError>;
    /// `IMMNotificationClient` wrapper behind a std channel (drift-and-recovery decision:
    /// facade method, not a second port trait — single consumer, the recovery supervisor).
    /// Callable once; a second call replaces the previous subscription in real `win-audio`.
    fn subscribe_device_events(&self) -> Result<Receiver<DeviceEvent>, PortError>;
}

/// `IAudioSessionManager2` notifications (session-routing L4).
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    New(SessionInfo),
    Ended(u32),
}

/// Separate from `AudioSystem` (session-routing decision, deliberate exception
/// to the grow-facade learning): session enumeration is a distinct,
/// best-effort, possibly-unavailable concern — `RoutingCoordinator` owns it
/// behind its own `Box<dyn _>`, not folded into the main facade.
pub trait SessionPort: Send {
    /// Must prime new-session notifications (real `win-audio` impl: calls
    /// `GetSessionEnumerator` at least once — notes §14 gotcha 1).
    fn enumerate(&mut self) -> Result<Vec<SessionInfo>, PortError>;
    /// Single-consume, same pattern as `EngineHandle::take_events`.
    fn take_events(&mut self) -> Receiver<SessionEvent>;
    /// Best-effort — pid not currently found among live sessions (already
    /// exited) is `Ok(())`, not an error (session-mute-on-capture L3 flow E:
    /// failures are isolated, caller logs and moves on, never blocks
    /// reconcile). `&self`, not `&mut self` — same "this is really an OS RPC
    /// call" reasoning as `AudioSystem`'s methods; no persistent state needed.
    fn set_muted(&self, pid: u32, muted: bool) -> Result<(), PortError>;
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
