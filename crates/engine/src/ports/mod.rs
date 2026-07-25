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
    /// The Windows default playback device's master volume (external-controls.md
    /// decision 10) — a distinct, best-effort, possibly-unavailable concern with
    /// its own owner, same `SessionPort` rationale, not folded into this facade
    /// as a plain method. Default body errors so `MockSystem` and future
    /// backends need no change unless they opt in (the `set_bus_match`
    /// precedent) — a device with no volume control is a real, expected case,
    /// not a bug.
    fn open_default_endpoint_volume(&self) -> Result<Box<dyn EndpointVolumePort>, PortError> {
        Err(PortError::Backend("endpoint volume not supported by this backend".into()))
    }
}

/// A volume change the binding did **not** cause — the adapter filters its own
/// writes by `guidEventContext` before ever emitting one (external-controls.md
/// decision 11: that GUID is a COM detail and stays out of this contract).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeEvent {
    /// 0.0..=1.0 slider position, straight from the notification payload —
    /// not dB (decision 14: the payload carries only the scalar, and this
    /// codebase forbids calling back into the API that delivered a
    /// notification, which a dB conversion via `GetVolumeRange` would need).
    pub level: f32,
    pub muted: bool,
}

/// Read/write access to one endpoint's master volume, plus change
/// notifications. Separate from `AudioSystem` for the same reason as
/// `SessionPort`: a distinct, best-effort, possibly-unavailable concern with
/// its own owner, not a call every backend must support.
pub trait EndpointVolumePort: Send {
    fn level(&self) -> Result<f32, PortError>;
    /// Tagged with this port's own GUID on the real backend, so the
    /// resulting notification is filtered out and never re-enters as an
    /// event (decision 11).
    fn set_level(&self, level: f32) -> Result<(), PortError>;
    fn muted(&self) -> Result<bool, PortError>;
    fn set_muted(&self, muted: bool) -> Result<(), PortError>;
    /// Single-consume, same pattern as `SessionPort::take_events`.
    fn take_events(&mut self) -> Receiver<VolumeEvent>;
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

    /// Frames this device will accept *right now* — its buffer capacity minus
    /// current padding (audio-flow-control B1). No default body: a default
    /// that claims space is available is indistinguishable from an infinite
    /// device, which is exactly the condition this method exists to make
    /// observable. Every implementor, mocks included, must answer honestly.
    fn free_frames(&self) -> Result<usize, PortError>;

    /// Returns frames actually accepted. A caller offering more than the last
    /// `free_frames()` reported is a caller bug; any shortfall must be
    /// reported by the caller, never swallowed into a bare `Ok(())` (B1).
    fn write(&mut self, frames: &[f32]) -> Result<usize, PortError>;

    fn format(&self) -> Format;

    /// The device *period* — the audio one `wait_event` wakeup corresponds
    /// to. NOT the total buffer size (B1: those used to be conflated).
    fn period_frames(&self) -> usize;
}
