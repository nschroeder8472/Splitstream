//! Fake `AudioSystem` + ports. This is why the port traits live in `engine`,
//! not `win-audio`: the whole graph runs on any platform against these fakes.

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audio_core::Format;

use super::{
    AudioSystem, CapturePort, DeviceEvent, Endpoint, EndpointId, EndpointKind, PolicyError,
    PolicyPort, PortError, RenderPort, RtGuard, SessionEvent, SessionPort,
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
}

impl MockSystem {
    pub fn new(endpoints: Vec<Endpoint>) -> MockSystem {
        MockSystem {
            endpoints: Mutex::new(endpoints),
            default_output: Mutex::new(None),
            events: Mutex::new(None),
            render_invalidated: Mutex::new(HashMap::new()),
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
}

impl AudioSystem for MockSystem {
    fn enumerate(&self) -> Result<Vec<Endpoint>, PortError> {
        Ok(self.endpoints.lock().unwrap().clone())
    }

    fn open_capture(&self, id: &EndpointId) -> Result<Box<dyn CapturePort>, PortError> {
        let endpoint = self.find(id)?;
        Ok(Box::new(SineCapture::new(440.0, endpoint.format)))
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
            .iter()
            .find(|e| e.kind == EndpointKind::Physical)
            .cloned()
            .ok_or_else(|| PortError::Backend("no physical endpoint configured".into()))
    }

    fn subscribe_device_events(&self) -> Result<Receiver<DeviceEvent>, PortError> {
        let (tx, rx) = mpsc::channel();
        *self.events.lock().unwrap() = Some(tx);
        Ok(rx)
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
}

/// Simulated failure mode for `MockPolicyPort` — reconstructs a fresh
/// `PolicyError` per call rather than storing/cloning one, since `PolicyError`
/// (like `PortError`) intentionally doesn't derive `Clone`.
enum MockFailure {
    Unavailable,
    Failed,
}

#[derive(Default)]
struct PolicyPortState {
    routes: Mutex<HashMap<u32, EndpointId>>,
    visibility: Mutex<HashMap<EndpointId, bool>>,
    default: Mutex<Option<EndpointId>>,
    failing: Mutex<Option<MockFailure>>,
}

/// Fake `PolicyPort`. Records every applied route/visibility/default for
/// test assertions, and `fail_with_*`/`stop_failing` let degradation-path
/// tests (RoutingCoordinator: "first PolicyPort failure degrades") simulate
/// the undocumented surface breaking. `Clone` + `Arc`-backed state — same
/// rationale as `MockSessionPort` above.
#[derive(Clone, Default)]
pub struct MockPolicyPort(Arc<PolicyPortState>);

impl MockPolicyPort {
    pub fn new() -> MockPolicyPort {
        MockPolicyPort::default()
    }

    pub fn fail_with_unavailable(&self) {
        *self.0.failing.lock().unwrap() = Some(MockFailure::Unavailable);
    }

    pub fn fail_with_failed(&self) {
        *self.0.failing.lock().unwrap() = Some(MockFailure::Failed);
    }

    pub fn stop_failing(&self) {
        *self.0.failing.lock().unwrap() = None;
    }

    pub fn routes(&self) -> HashMap<u32, EndpointId> {
        self.0.routes.lock().unwrap().clone()
    }

    pub fn is_visible(&self, endpoint: &EndpointId) -> Option<bool> {
        self.0.visibility.lock().unwrap().get(endpoint).copied()
    }

    pub fn default_endpoint(&self) -> Option<EndpointId> {
        self.0.default.lock().unwrap().clone()
    }

    fn check_failure(&self) -> Result<(), PolicyError> {
        match &*self.0.failing.lock().unwrap() {
            Some(MockFailure::Unavailable) => {
                Err(PolicyError::Unavailable("mock: simulated unavailable".into()))
            }
            Some(MockFailure::Failed) => Err(PolicyError::Failed("mock: simulated failure".into())),
            None => Ok(()),
        }
    }
}

impl PolicyPort for MockPolicyPort {
    fn route(&mut self, pid: u32, bus: &EndpointId) -> Result<(), PolicyError> {
        self.check_failure()?;
        self.0.routes.lock().unwrap().insert(pid, bus.clone());
        Ok(())
    }

    fn clear_route(&mut self, pid: u32) -> Result<(), PolicyError> {
        self.check_failure()?;
        self.0.routes.lock().unwrap().remove(&pid);
        Ok(())
    }

    fn set_visibility(&mut self, endpoint: &EndpointId, visible: bool) -> Result<(), PolicyError> {
        self.check_failure()?;
        self.0.visibility.lock().unwrap().insert(endpoint.clone(), visible);
        Ok(())
    }

    fn set_default(&mut self, endpoint: &EndpointId) -> Result<(), PolicyError> {
        self.check_failure()?;
        *self.0.default.lock().unwrap() = Some(endpoint.clone());
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

    fn endpoint(id: &str, kind: EndpointKind) -> Endpoint {
        Endpoint {
            id: EndpointId(id.to_string()),
            name: id.to_string(),
            kind,
            format: stereo(48_000),
        }
    }

    #[test]
    fn enumerate_returns_configured_endpoints() {
        let sys = MockSystem::new(vec![
            endpoint("bus-1", EndpointKind::Bus),
            endpoint("out-1", EndpointKind::Physical),
        ]);
        let eps = sys.enumerate().unwrap();
        assert_eq!(eps.len(), 2);
    }

    #[test]
    fn open_capture_on_unknown_id_returns_not_found() {
        let sys = MockSystem::new(vec![]);
        let result = sys.open_capture(&EndpointId("missing".into()));
        assert!(matches!(result, Err(PortError::NotFound(_))));
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
    fn default_output_falls_back_to_first_physical_endpoint() {
        let sys = MockSystem::new(vec![
            endpoint("bus-1", EndpointKind::Bus),
            endpoint("out-1", EndpointKind::Physical),
            endpoint("out-2", EndpointKind::Physical),
        ]);
        assert_eq!(sys.default_output().unwrap().id, EndpointId("out-1".into()));
    }

    #[test]
    fn default_output_honors_explicit_override() {
        let sys = MockSystem::new(vec![
            endpoint("out-1", EndpointKind::Physical),
            endpoint("out-2", EndpointKind::Physical),
        ]);
        sys.set_default_output(EndpointId("out-2".into()));
        assert_eq!(sys.default_output().unwrap().id, EndpointId("out-2".into()));
    }

    #[test]
    fn default_output_with_no_physical_endpoint_is_backend_error() {
        let sys = MockSystem::new(vec![endpoint("bus-1", EndpointKind::Bus)]);
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

    #[test]
    fn policy_port_records_applied_route() {
        let mut policy = MockPolicyPort::new();
        let bus = EndpointId("bus-1".into());
        policy.route(1, &bus).unwrap();
        assert_eq!(policy.routes().get(&1), Some(&bus));
    }

    #[test]
    fn policy_port_clear_route_removes_the_recorded_route() {
        let mut policy = MockPolicyPort::new();
        let bus = EndpointId("bus-1".into());
        policy.route(1, &bus).unwrap();
        policy.clear_route(1).unwrap();
        assert!(!policy.routes().contains_key(&1));
    }

    #[test]
    fn policy_port_records_visibility_and_default() {
        let mut policy = MockPolicyPort::new();
        let bus = EndpointId("bus-1".into());
        let out = EndpointId("out-1".into());
        policy.set_visibility(&bus, false).unwrap();
        policy.set_default(&out).unwrap();
        assert_eq!(policy.is_visible(&bus), Some(false));
        assert_eq!(policy.default_endpoint(), Some(out));
    }

    #[test]
    fn policy_port_fails_every_call_once_failing_and_recovers_after_stop_failing() {
        let mut policy = MockPolicyPort::new();
        policy.fail_with_unavailable();
        assert!(matches!(
            policy.route(1, &EndpointId("bus-1".into())),
            Err(PolicyError::Unavailable(_))
        ));

        policy.stop_failing();
        assert!(policy.route(1, &EndpointId("bus-1".into())).is_ok());
    }
}
