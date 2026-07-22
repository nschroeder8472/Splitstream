//! Fake `AudioSystem` + ports. This is why the port traits live in `engine`,
//! not `win-audio`: the whole graph runs on any platform against these fakes.

use std::collections::{HashMap, HashSet};
use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audio_core::Format;

use super::{
    AudioSystem, CapturePort, DeviceEvent, Endpoint, EndpointId, PortError, RenderPort, RtGuard,
    SessionEvent, SessionPort,
};
use crate::rules::SessionInfo;

pub struct MockSystem {
    /// `Mutex`, not a plain `Vec`: recovery-path tests simulate a device
    /// disappearing/reappearing mid-run via `remove_endpoint`/`add_endpoint`,
    /// observed by the engine's next `enumerate()` call on rebuild.
    endpoints: Mutex<Vec<Endpoint>>,
    default_output: Mutex<Option<EndpointId>>,
    events: Mutex<Option<mpsc::Sender<DeviceEvent>>>,
    /// One flag per currently-open render port, keyed by endpoint id —
    /// `invalidate_render` flips it so the *already-running* `SinkRender`
    /// fails on its next `wait_event`, simulating a real mid-stream format
    /// change (as opposed to a device that's actually gone).
    render_invalidated: Mutex<HashMap<EndpointId, Arc<AtomicBool>>>,
    /// Pids `open_process_capture` should fail for — simulates permission
    /// denied / a protected process (process-loopback-capture L3 flow E:
    /// per-attempt, isolated failure, no global degraded flag).
    failing_pids: Mutex<std::collections::HashSet<u32>>,
    /// Pids whose capture *opens* fine but errors on the very first `read()`
    /// — simulates a runtime failure mid-stream (as opposed to `failing_pids`,
    /// which fails at open time), for testing that a dead capture thread gets
    /// reaped and retried rather than permanently zombied.
    dying_pids: Mutex<std::collections::HashSet<u32>>,
    /// Successful `open_process_capture` calls per pid — lets a test prove a
    /// pid was actually re-opened (not just skipped as "still current").
    open_counts: Mutex<HashMap<u32, usize>>,
}

impl MockSystem {
    pub fn new(endpoints: Vec<Endpoint>) -> MockSystem {
        MockSystem {
            endpoints: Mutex::new(endpoints),
            default_output: Mutex::new(None),
            events: Mutex::new(None),
            render_invalidated: Mutex::new(HashMap::new()),
            failing_pids: Mutex::new(std::collections::HashSet::new()),
            dying_pids: Mutex::new(std::collections::HashSet::new()),
            open_counts: Mutex::new(HashMap::new()),
        }
    }

    fn find(&self, id: &EndpointId) -> Result<Endpoint, PortError> {
        self.endpoints
            .lock()
            .unwrap()
            .iter()
            .find(|e| &e.id == id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(id.clone()))
    }

    /// Test hook: override which endpoint `default_output()` reports (defaults to
    /// the first `Physical` endpoint in the list).
    pub fn set_default_output(&self, id: EndpointId) {
        *self.default_output.lock().unwrap() = Some(id);
    }

    /// Test hook: push a `DeviceEvent` to whatever `Receiver` `subscribe_device_events`
    /// last handed out. No-op if nothing has subscribed yet.
    pub fn emit_device_event(&self, event: DeviceEvent) {
        if let Some(tx) = self.events.lock().unwrap().as_ref() {
            let _ = tx.send(event);
        }
    }

    /// Test hook: simulate a device disappearing — subsequent `enumerate()`
    /// calls (and therefore any rebuild) won't see it.
    pub fn remove_endpoint(&self, id: &EndpointId) {
        self.endpoints.lock().unwrap().retain(|e| &e.id != id);
    }

    /// Test hook: simulate a device (re)appearing.
    pub fn add_endpoint(&self, endpoint: Endpoint) {
        self.endpoints.lock().unwrap().push(endpoint);
    }

    /// Test hook: simulate a format-change stream invalidation on the
    /// *currently open* render port for `id` — the device stays in
    /// `endpoints` (still enumerable), only the live stream faults. No-op if
    /// nothing is currently open for `id`.
    pub fn invalidate_render(&self, id: &EndpointId) {
        if let Some(flag) = self.render_invalidated.lock().unwrap().get(id) {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// Test hook: make `open_process_capture(pid, _)` fail for `pid` until
    /// `unfail_process_capture` is called — simulates a permission-denied or
    /// protected-process activation failure for one pid, isolated from every
    /// other pid (process-loopback-capture L3 flow E).
    pub fn fail_process_capture(&self, pid: u32) {
        self.failing_pids.lock().unwrap().insert(pid);
    }

    pub fn unfail_process_capture(&self, pid: u32) {
        self.failing_pids.lock().unwrap().remove(&pid);
    }

    /// Test hook: `open_process_capture(pid, _)` succeeds for `pid`, but the
    /// returned port's `read()` always errors — simulates a capture stream
    /// that dies mid-run (as opposed to `fail_process_capture`, which fails
    /// at open time).
    pub fn die_on_read(&self, pid: u32) {
        self.dying_pids.lock().unwrap().insert(pid);
    }

    /// Test hook: how many times `open_process_capture` actually succeeded
    /// for `pid` — proves a re-open attempt happened, not just a skip.
    pub fn open_count(&self, pid: u32) -> usize {
        self.open_counts.lock().unwrap().get(&pid).copied().unwrap_or(0)
    }
}

impl AudioSystem for MockSystem {
    fn enumerate(&self) -> Result<Vec<Endpoint>, PortError> {
        Ok(self.endpoints.lock().unwrap().clone())
    }

    fn open_process_capture(&self, pid: u32, _include_tree: bool) -> Result<Box<dyn CapturePort>, PortError> {
        if self.failing_pids.lock().unwrap().contains(&pid) {
            return Err(PortError::Backend(format!("mock: process capture denied for pid {pid}")));
        }
        *self.open_counts.lock().unwrap().entry(pid).or_insert(0) += 1;
        // Fixed default format — process capture streams aren't tied to any
        // configured endpoint, unlike the old per-bus loopback capture.
        let format = Format {
            sample_rate: 48_000,
            channels: 2,
            layout: audio_core::ChannelLayout::STEREO,
        };
        if self.dying_pids.lock().unwrap().contains(&pid) {
            return Ok(Box::new(DyingCapture { format }));
        }
        Ok(Box::new(SineCapture::new(440.0, format)))
    }

    fn open_render(&self, id: &EndpointId) -> Result<Box<dyn RenderPort>, PortError> {
        let endpoint = self.find(id)?;
        let flag = Arc::new(AtomicBool::new(false));
        self.render_invalidated
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::clone(&flag));
        Ok(Box::new(SinkRender::with_invalidation_flag(
            endpoint.format,
            flag,
        )))
    }

    fn promote_rt_thread(&self) -> RtGuard {
        RtGuard::noop()
    }

    fn default_output(&self) -> Result<Endpoint, PortError> {
        if let Some(id) = self.default_output.lock().unwrap().as_ref() {
            return self.find(id);
        }
        self.endpoints
            .lock()
            .unwrap()
            .first()
            .cloned()
            .ok_or_else(|| PortError::Backend("no physical endpoint configured".into()))
    }

    fn subscribe_device_events(&self) -> Result<Receiver<DeviceEvent>, PortError> {
        let (tx, rx) = mpsc::channel();
        *self.events.lock().unwrap() = Some(tx);
        Ok(rx)
    }
}

/// A capture port that opens successfully but errors on every `read()` —
/// simulates a stream that dies mid-run (`MockSystem::die_on_read`).
struct DyingCapture {
    format: Format,
}

impl CapturePort for DyingCapture {
    fn read(&mut self, _buf: &mut [f32]) -> Result<usize, PortError> {
        Err(PortError::Backend("mock: capture died mid-stream".into()))
    }

    fn format(&self) -> Format {
        self.format
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_millis(1)
    }
}

/// Deterministic signal source: a sine wave at `freq_hz`, same value on every
/// channel. Lets tests assert on exact expected samples instead of "some audio".
pub struct SineCapture {
    format: Format,
    freq_hz: f32,
    phase: f32,
}

impl SineCapture {
    pub fn new(freq_hz: f32, format: Format) -> SineCapture {
        SineCapture {
            format,
            freq_hz,
            phase: 0.0,
        }
    }
}

impl CapturePort for SineCapture {
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, PortError> {
        let channels = self.format.channels as usize;
        let step = TAU * self.freq_hz / self.format.sample_rate as f32;
        for frame in buf.chunks_exact_mut(channels.max(1)) {
            let sample = self.phase.sin();
            frame.fill(sample);
            self.phase = (self.phase + step) % TAU;
        }
        Ok(buf.len())
    }

    fn format(&self) -> Format {
        self.format
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_millis(5)
    }
}

/// Records every frame written to it, for test assertions (gain applied?
/// groups summed correctly?). Never blocks `wait_event`; errors only if
/// `invalidated` is set (via `MockSystem::invalidate_render`).
pub struct SinkRender {
    format: Format,
    recorded: Vec<f32>,
    invalidated: Option<Arc<AtomicBool>>,
}

impl SinkRender {
    pub fn new(format: Format) -> SinkRender {
        SinkRender {
            format,
            recorded: Vec::new(),
            invalidated: None,
        }
    }

    fn with_invalidation_flag(format: Format, flag: Arc<AtomicBool>) -> SinkRender {
        SinkRender {
            format,
            recorded: Vec::new(),
            invalidated: Some(flag),
        }
    }

    pub fn recorded(&self) -> &[f32] {
        &self.recorded
    }
}

impl RenderPort for SinkRender {
    fn wait_event(&mut self, _timeout: Duration) -> Result<(), PortError> {
        if let Some(flag) = &self.invalidated {
            // One-shot: consume the flag so this port doesn't fault forever
            // (real WASAPI invalidation is a single terminal event too).
            if flag.swap(false, Ordering::Relaxed) {
                return Err(PortError::DeviceInvalidated);
            }
        }
        Ok(())
    }

    fn write(&mut self, frames: &[f32]) -> Result<(), PortError> {
        self.recorded.extend_from_slice(frames);
        Ok(())
    }

    fn format(&self) -> Format {
        self.format
    }

    fn period_frames(&self) -> usize {
        480
    }
}

#[derive(Default)]
struct SessionPortState {
    sessions: Mutex<Vec<SessionInfo>>,
    events: Mutex<Option<mpsc::Sender<SessionEvent>>>,
    /// Pids `set_muted` has been called with `true` for, per this mock's own
    /// bookkeeping — cleared again on `set_muted(pid, false)`.
    muted: Mutex<HashSet<u32>>,
    /// Pids `set_muted` should fail for (session-mute-on-capture L3 flow E:
    /// isolated, best-effort failure) — same shape as `MockSystem::failing_pids`.
    failing_mute: Mutex<HashSet<u32>>,
}

/// Fake `SessionPort`. Test hooks (`add_session`/`remove_session`/`emit_event`)
/// simulate WASAPI session enumeration/notifications the same way
/// `MockSystem::{add_endpoint, remove_endpoint, emit_device_event}` simulate
/// device changes. `Clone` + `Arc`-backed state (unlike `MockSystem`, which
/// stays a single object shared via `Arc<MockSystem>`): `SessionPort`'s
/// `&mut self` methods mean the trait object gets moved into
/// `Box<dyn SessionPort>` and handed to `start_routing` by value — a test
/// keeps a cloned handle (same underlying state) to drive it afterward.
#[derive(Clone, Default)]
pub struct MockSessionPort(Arc<SessionPortState>);

impl MockSessionPort {
    pub fn new(sessions: Vec<SessionInfo>) -> MockSessionPort {
        let port = MockSessionPort::default();
        *port.0.sessions.lock().unwrap() = sessions;
        port
    }

    pub fn add_session(&self, session: SessionInfo) {
        self.0.sessions.lock().unwrap().push(session);
    }

    pub fn remove_session(&self, pid: u32) {
        self.0.sessions.lock().unwrap().retain(|s| s.pid != pid);
    }

    /// No-op if nothing has called `take_events` yet.
    pub fn emit_event(&self, event: SessionEvent) {
        if let Some(tx) = self.0.events.lock().unwrap().as_ref() {
            let _ = tx.send(event);
        }
    }

    /// Test hook: pids currently muted per this mock's own bookkeeping.
    pub fn muted_pids(&self) -> HashSet<u32> {
        self.0.muted.lock().unwrap().clone()
    }

    /// Test hook: make `set_muted(pid, _)` fail until cleared.
    pub fn fail_mute(&self, pid: u32) {
        self.0.failing_mute.lock().unwrap().insert(pid);
    }
}

impl SessionPort for MockSessionPort {
    fn enumerate(&mut self) -> Result<Vec<SessionInfo>, PortError> {
        Ok(self.0.sessions.lock().unwrap().clone())
    }

    fn take_events(&mut self) -> Receiver<SessionEvent> {
        let (tx, rx) = mpsc::channel();
        *self.0.events.lock().unwrap() = Some(tx);
        rx
    }

    fn set_muted(&self, pid: u32, muted: bool) -> Result<(), PortError> {
        if self.0.failing_mute.lock().unwrap().contains(&pid) {
            return Err(PortError::Backend(format!("mock: set_muted denied for pid {pid}")));
        }
        let mut set = self.0.muted.lock().unwrap();
        if muted {
            set.insert(pid);
        } else {
            set.remove(&pid);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
            layout: audio_core::ChannelLayout::STEREO,
        }
    }

    fn endpoint(id: &str) -> Endpoint {
        Endpoint {
            id: EndpointId(id.to_string()),
            name: id.to_string(),
            format: stereo(48_000),
        }
    }

    #[test]
    fn enumerate_returns_configured_endpoints() {
        let sys = MockSystem::new(vec![endpoint("out-1"), endpoint("out-2")]);
        let eps = sys.enumerate().unwrap();
        assert_eq!(eps.len(), 2);
    }

    #[test]
    fn open_process_capture_succeeds_for_any_pid_by_default() {
        let sys = MockSystem::new(vec![]);
        assert!(sys.open_process_capture(1234, false).is_ok());
    }

    #[test]
    fn open_process_capture_fails_for_a_pid_marked_failing() {
        let sys = MockSystem::new(vec![]);
        sys.fail_process_capture(1234);
        assert!(matches!(
            sys.open_process_capture(1234, false),
            Err(PortError::Backend(_))
        ));
    }

    #[test]
    fn unfail_process_capture_lets_a_previously_failing_pid_succeed() {
        let sys = MockSystem::new(vec![]);
        sys.fail_process_capture(1234);
        sys.unfail_process_capture(1234);
        assert!(sys.open_process_capture(1234, false).is_ok());
    }

    #[test]
    fn sine_capture_fills_buffer_and_is_deterministic_across_captures() {
        let fmt = stereo(48_000);
        let mut a = SineCapture::new(440.0, fmt);
        let mut b = SineCapture::new(440.0, fmt);
        let mut buf_a = [0.0f32; 8];
        let mut buf_b = [0.0f32; 8];
        assert_eq!(a.read(&mut buf_a).unwrap(), 8);
        assert_eq!(b.read(&mut buf_b).unwrap(), 8);
        assert_eq!(buf_a, buf_b);
        // Both channels of a frame carry the same sample.
        assert_eq!(buf_a[0], buf_a[1]);
    }

    #[test]
    fn sink_render_records_every_write() {
        let mut sink = SinkRender::new(stereo(48_000));
        sink.write(&[0.1, 0.2]).unwrap();
        sink.write(&[0.3, 0.4]).unwrap();
        assert_eq!(sink.recorded(), &[0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn default_output_falls_back_to_first_endpoint() {
        let sys = MockSystem::new(vec![endpoint("out-1"), endpoint("out-2")]);
        assert_eq!(sys.default_output().unwrap().id, EndpointId("out-1".into()));
    }

    #[test]
    fn default_output_honors_explicit_override() {
        let sys = MockSystem::new(vec![endpoint("out-1"), endpoint("out-2")]);
        sys.set_default_output(EndpointId("out-2".into()));
        assert_eq!(sys.default_output().unwrap().id, EndpointId("out-2".into()));
    }

    #[test]
    fn default_output_with_no_endpoints_is_backend_error() {
        let sys = MockSystem::new(vec![]);
        assert!(matches!(sys.default_output(), Err(PortError::Backend(_))));
    }

    #[test]
    fn subscribed_events_are_delivered_to_the_receiver() {
        let sys = MockSystem::new(vec![]);
        let rx = sys.subscribe_device_events().unwrap();
        sys.emit_device_event(DeviceEvent::Removed(EndpointId("out-1".into())));
        assert_eq!(
            rx.recv().unwrap(),
            DeviceEvent::Removed(EndpointId("out-1".into()))
        );
    }

    #[test]
    fn emit_before_subscribe_is_dropped_silently() {
        let sys = MockSystem::new(vec![]);
        sys.emit_device_event(DeviceEvent::DefaultChanged(EndpointId("out-1".into()))); // no subscriber yet — must not panic
        let rx = sys.subscribe_device_events().unwrap();
        assert!(rx.try_recv().is_err());
    }

    fn session(pid: u32, path: &str) -> SessionInfo {
        SessionInfo {
            pid,
            process_path: path.into(),
            display_name: path.into(),
        }
    }

    #[test]
    fn session_port_enumerate_returns_configured_sessions() {
        let mut sessions = MockSessionPort::new(vec![session(1, "game.exe")]);
        let result = sessions.enumerate().unwrap();
        assert_eq!(result, vec![session(1, "game.exe")]);
    }

    #[test]
    fn session_port_add_and_remove_change_the_next_enumerate() {
        let mut sessions = MockSessionPort::new(vec![session(1, "game.exe")]);
        sessions.add_session(session(2, "music.exe"));
        sessions.remove_session(1);
        assert_eq!(sessions.enumerate().unwrap(), vec![session(2, "music.exe")]);
    }

    #[test]
    fn session_port_events_are_delivered_to_the_receiver() {
        let mut sessions = MockSessionPort::new(vec![]);
        let rx = sessions.take_events();
        sessions.emit_event(SessionEvent::Ended(1));
        assert_eq!(rx.recv().unwrap(), SessionEvent::Ended(1));
    }

    #[test]
    fn session_port_emit_before_take_events_is_dropped_silently() {
        let sessions = MockSessionPort::new(vec![]);
        sessions.emit_event(SessionEvent::Ended(1)); // no subscriber yet — must not panic
    }

}
