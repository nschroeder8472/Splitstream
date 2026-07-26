//! Fake `AudioSystem` + ports. This is why the port traits live in `engine`,
//! not `win-audio`: the whole graph runs on any platform against these fakes.

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use audio_core::Format;

use super::{
    AudioSystem, CapturePort, DeviceEvent, Endpoint, EndpointId, EndpointVolumePort, PortError,
    RenderPort, RtGuard, SessionEvent, SessionPort, VolumeEvent,
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
    /// `None` (the default) simulates a device without volume control —
    /// `open_default_endpoint_volume` errors, matching the real trait's
    /// default body. Set via `enable_endpoint_volume`.
    endpoint_volume: Mutex<Option<MockEndpointVolumePort>>,
    /// Every id `set_default_output` has been asked to install, in order
    /// (double-audio-prevention L4) — lets flows B/C/D round-trip without
    /// hardware.
    default_output_calls: Mutex<Vec<EndpointId>>,
    /// Makes `set_default_output` error, simulating the undocumented COM call
    /// failing (capability 6: surfaced, never panicked).
    failing_set_default_output: AtomicBool,
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
            endpoint_volume: Mutex::new(None),
            default_output_calls: Mutex::new(Vec::new()),
            failing_set_default_output: AtomicBool::new(false),
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

    /// Test hook: seed which endpoint `default_output()` reports (defaults to
    /// the first endpoint in the list). Deliberately *not* named
    /// `set_default_output` — that is now a real `AudioSystem` method, and an
    /// inherent method of the same name would silently shadow it at every
    /// call site.
    pub fn seed_default_output(&self, id: EndpointId) {
        *self.default_output.lock().unwrap() = Some(id);
    }

    /// Test hook: every id `set_default_output` has been asked to install, in
    /// order (double-audio-prevention flows B/C/D).
    pub fn default_output_calls(&self) -> Vec<EndpointId> {
        self.default_output_calls.lock().unwrap().clone()
    }

    /// Test hook: make `set_default_output` error until cleared.
    pub fn fail_set_default_output(&self) {
        self.failing_set_default_output.store(true, Ordering::Relaxed);
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

    /// Test hook: make `open_default_endpoint_volume` succeed, returning a
    /// mock seeded with `level`/`muted`. Returns a cloned handle (same
    /// underlying state) a test uses to simulate Windows-side changes
    /// (`emit`) and inspect outbound calls (`set_level_calls`/`set_muted_calls`)
    /// — same shape as `MockSessionPort`.
    pub fn enable_endpoint_volume(&self, level: f32, muted: bool) -> MockEndpointVolumePort {
        let port = MockEndpointVolumePort::new(level, muted);
        *self.endpoint_volume.lock().unwrap() = Some(port.clone());
        port
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

    fn open_default_endpoint_volume(&self) -> Result<Box<dyn EndpointVolumePort>, PortError> {
        match self.endpoint_volume.lock().unwrap().clone() {
            Some(port) => Ok(Box::new(port)),
            None => Err(PortError::Backend("mock: no endpoint volume port configured".into())),
        }
    }

    /// Opts in to the real method (rather than inheriting the erroring default
    /// body) so double-audio-prevention's take/restore flows round-trip
    /// against `default_output()` with no hardware. An id this mock doesn't
    /// have is rejected — a device that isn't there can't become the default,
    /// and a mock that never says no makes that case untestable.
    fn set_default_output(&self, id: &EndpointId) -> Result<(), PortError> {
        if self.failing_set_default_output.load(Ordering::Relaxed) {
            return Err(PortError::Backend("mock: set_default_output denied".into()));
        }
        self.find(id)?;
        self.default_output_calls.lock().unwrap().push(id.clone());
        *self.default_output.lock().unwrap() = Some(id.clone());
        Ok(())
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

/// Test-side state for a `SineCapture::paced` source (audio-flow-control
/// B17) — frames a `read` may yield are only ever what `CaptureSource::produce`
/// has made available, never the caller's whole buffer unconditionally.
struct CaptureState {
    produced_frames: Mutex<usize>,
}

/// Deterministic signal source: a sine wave at `freq_hz`, same value on every
/// channel. Lets tests assert on exact expected samples instead of "some audio".
/// Two modes, same "new() stays unpaced, paced() purely additive" shape as
/// `SinkRender` (decision 5).
pub struct SineCapture {
    format: Format,
    freq_hz: f32,
    phase: f32,
    paced: Option<Arc<CaptureState>>,
}

impl SineCapture {
    pub fn new(freq_hz: f32, format: Format) -> SineCapture {
        SineCapture {
            format,
            freq_hz,
            phase: 0.0,
            paced: None,
        }
    }

    /// A source that yields at most what `CaptureSource::produce` has made
    /// available, then 0 — never `buf.len()` unconditionally.
    pub fn paced(freq_hz: f32, format: Format) -> (SineCapture, CaptureSource) {
        let state = Arc::new(CaptureState {
            produced_frames: Mutex::new(0),
        });
        let port = SineCapture {
            format,
            freq_hz,
            phase: 0.0,
            paced: Some(Arc::clone(&state)),
        };
        (port, CaptureSource(state))
    }
}

impl CapturePort for SineCapture {
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, PortError> {
        let channels = self.format.channels.max(1) as usize;
        let want_frames = buf.len() / channels;
        let frames = match &self.paced {
            Some(state) => {
                let mut produced = state.produced_frames.lock().unwrap();
                let take = want_frames.min(*produced);
                *produced -= take;
                take
            }
            None => want_frames, // unpaced: fills whatever it's given (decision 5)
        };

        let step = TAU * self.freq_hz / self.format.sample_rate as f32;
        for frame in buf[..frames * channels].chunks_exact_mut(channels) {
            let sample = self.phase.sin();
            frame.fill(sample);
            self.phase = (self.phase + step) % TAU;
        }
        Ok(frames * channels)
    }

    fn format(&self) -> Format {
        self.format
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_millis(5)
    }
}

/// Test-side handle to a paced `SineCapture` (audio-flow-control B17).
/// `Arc`-backed and `Clone`, same idiom as `SinkDevice`/`MockSessionPort`.
#[derive(Clone)]
pub struct CaptureSource(Arc<CaptureState>);

impl CaptureSource {
    /// Makes `frames` more frames available to the next `read` call(s).
    pub fn produce(&self, frames: usize) {
        *self.0.produced_frames.lock().unwrap() += frames;
    }
}

/// Test-side state for a `SinkRender::paced` device — the render sink's own
/// simulated clock. `filled_frames` is freed only by `SinkDevice::drain`,
/// which is also what releases exactly one pending `wait_event` (audio-flow-
/// control B17): nothing about the port advances without the test driving it.
struct SinkState {
    capacity_frames: usize,
    period_frames: usize,
    filled_frames: Mutex<usize>,
    /// Un-consumed `wait_event` releases — `drain` increments, `wait_event`
    /// decrements. A plain counter/condvar pair, not a channel: `wait_event`
    /// needs a bounded timeout wait, which `mpsc::Receiver::recv_timeout`
    /// would also give, but the counter lets `drain` be called any number of
    /// times before a `wait_event` call ever arrives without growing unbounded.
    releases: Mutex<u64>,
    cond: Condvar,
    recorded: Mutex<Vec<f32>>,
}

/// Records every frame written to it, for test assertions (gain applied?
/// groups summed correctly?). Two modes (decision 5, audio-flow-control):
/// `new()` stays the original infinite/immediate sink — zero churn across
/// existing tests that assert on recorded *content*, not flow — `paced()` is
/// purely additive, for tests asserting on flow control itself.
pub struct SinkRender {
    format: Format,
    recorded: Vec<f32>,
    invalidated: Option<Arc<AtomicBool>>,
    paced: Option<Arc<SinkState>>,
}

impl SinkRender {
    pub fn new(format: Format) -> SinkRender {
        SinkRender {
            format,
            recorded: Vec::new(),
            invalidated: None,
            paced: None,
        }
    }

    fn with_invalidation_flag(format: Format, flag: Arc<AtomicBool>) -> SinkRender {
        SinkRender {
            format,
            recorded: Vec::new(),
            invalidated: Some(flag),
            paced: None,
        }
    }

    /// A finite device: `capacity_frames` of buffer, `period_frames` per
    /// event. Returns the port (moved into the spawned `render_loop` thread)
    /// and a cloneable test-side handle to the same underlying state — the
    /// device clock.
    pub fn paced(format: Format, period_frames: usize, capacity_frames: usize) -> (SinkRender, SinkDevice) {
        let state = Arc::new(SinkState {
            capacity_frames,
            period_frames,
            filled_frames: Mutex::new(0),
            releases: Mutex::new(0),
            cond: Condvar::new(),
            recorded: Mutex::new(Vec::new()),
        });
        let port = SinkRender {
            format,
            recorded: Vec::new(),
            invalidated: None,
            paced: Some(Arc::clone(&state)),
        };
        (port, SinkDevice(state))
    }

    /// Unpaced-mode-only accessor — every frame written, in order. Existing
    /// 118-test idiom kept exactly as-is (decision 5); a paced port's content
    /// is read via `SinkDevice::recorded` instead, since the port itself has
    /// moved into a thread by the time a test wants to inspect it.
    pub fn recorded(&self) -> &[f32] {
        &self.recorded
    }
}

impl RenderPort for SinkRender {
    fn wait_event(&mut self, timeout: Duration) -> Result<(), PortError> {
        if let Some(flag) = &self.invalidated {
            // One-shot: consume the flag so this port doesn't fault forever
            // (real WASAPI invalidation is a single terminal event too).
            if flag.swap(false, Ordering::Relaxed) {
                return Err(PortError::DeviceInvalidated);
            }
        }
        let Some(state) = &self.paced else {
            return Ok(()); // unpaced: never blocks (decision 5)
        };
        let deadline = Instant::now() + timeout;
        let mut releases = state.releases.lock().unwrap();
        while *releases == 0 {
            let now = Instant::now();
            if now >= deadline {
                return Err(PortError::Backend("paced sink wait_event timed out".into()));
            }
            let (guard, _timed_out) = state.cond.wait_timeout(releases, deadline - now).unwrap();
            releases = guard;
        }
        *releases -= 1;
        Ok(())
    }

    fn free_frames(&self) -> Result<usize, PortError> {
        match &self.paced {
            Some(state) => {
                let filled = *state.filled_frames.lock().unwrap();
                Ok(state.capacity_frames.saturating_sub(filled))
            }
            // Unpaced: models an infinite device (decision 5) — never the
            // cause of a caller's short write.
            None => Ok(usize::MAX),
        }
    }

    fn write(&mut self, frames: &[f32]) -> Result<usize, PortError> {
        let channels = self.format.channels.max(1) as usize;
        let offered = frames.len() / channels;
        let Some(state) = &self.paced else {
            self.recorded.extend_from_slice(frames);
            return Ok(offered); // unpaced: accepts everything (decision 5)
        };
        let mut filled = state.filled_frames.lock().unwrap();
        let free = state.capacity_frames.saturating_sub(*filled);
        let accepted = offered.min(free);
        *filled += accepted;
        drop(filled);
        if accepted > 0 {
            state
                .recorded
                .lock()
                .unwrap()
                .extend_from_slice(&frames[..accepted * channels]);
        }
        Ok(accepted)
    }

    fn format(&self) -> Format {
        self.format
    }

    fn period_frames(&self) -> usize {
        match &self.paced {
            Some(state) => state.period_frames,
            None => 480,
        }
    }
}

/// Test-side handle to a paced `SinkRender` — the device clock (audio-flow-
/// control B17). `Arc`-backed and `Clone`, same idiom as `MockSessionPort`:
/// the port itself moves into the spawned `render_loop` thread, so a test
/// keeps this cloned handle to drive it afterward.
#[derive(Clone)]
pub struct SinkDevice(Arc<SinkState>);

impl SinkDevice {
    /// Consumes up to `frames` from the simulated device buffer and releases
    /// exactly one `wait_event`. Nothing advances without this call. Returns
    /// frames actually freed (may be less than requested if the buffer holds
    /// less).
    pub fn drain(&self, frames: usize) -> usize {
        let mut filled = self.0.filled_frames.lock().unwrap();
        let n = frames.min(*filled);
        *filled -= n;
        drop(filled);
        *self.0.releases.lock().unwrap() += 1;
        self.0.cond.notify_all();
        n
    }

    pub fn filled_frames(&self) -> usize {
        *self.0.filled_frames.lock().unwrap()
    }

    /// Everything `write` has accepted so far — the paced equivalent of
    /// `SinkRender::recorded`, reachable after the port has moved into a
    /// thread.
    pub fn recorded(&self) -> Vec<f32> {
        self.0.recorded.lock().unwrap().clone()
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

#[derive(Default)]
struct EndpointVolumeState {
    level: Mutex<f32>,
    muted: Mutex<bool>,
    events: Mutex<Option<mpsc::Sender<VolumeEvent>>>,
    set_level_calls: Mutex<Vec<f32>>,
    set_muted_calls: Mutex<Vec<bool>>,
}

/// Fake `EndpointVolumePort`. Same `Arc`-backed `Clone` shape as
/// `MockSessionPort` — `take_events` takes `&mut self`, so the port itself
/// moves into `Box<dyn EndpointVolumePort>`, and a test keeps a cloned handle
/// (same underlying state) to drive it afterward.
#[derive(Clone)]
pub struct MockEndpointVolumePort(Arc<EndpointVolumeState>);

impl MockEndpointVolumePort {
    fn new(level: f32, muted: bool) -> MockEndpointVolumePort {
        let state = EndpointVolumeState {
            level: Mutex::new(level),
            muted: Mutex::new(muted),
            ..Default::default()
        };
        MockEndpointVolumePort(Arc::new(state))
    }

    /// Test hook: simulate Windows changing the volume (keys, OSD, OS mixer)
    /// — updates the mock's own state and, if `take_events` has been called,
    /// delivers the notification. No-op on the notification if nothing has
    /// subscribed yet, same as `MockSystem::emit_device_event`.
    pub fn emit(&self, event: VolumeEvent) {
        *self.0.level.lock().unwrap() = event.level;
        *self.0.muted.lock().unwrap() = event.muted;
        if let Some(tx) = self.0.events.lock().unwrap().as_ref() {
            let _ = tx.send(event);
        }
    }

    /// Test hook: every value `set_level` has been called with, in order.
    pub fn set_level_calls(&self) -> Vec<f32> {
        self.0.set_level_calls.lock().unwrap().clone()
    }

    /// Test hook: every value `set_muted` has been called with, in order.
    pub fn set_muted_calls(&self) -> Vec<bool> {
        self.0.set_muted_calls.lock().unwrap().clone()
    }
}

impl EndpointVolumePort for MockEndpointVolumePort {
    fn level(&self) -> Result<f32, PortError> {
        Ok(*self.0.level.lock().unwrap())
    }

    fn set_level(&self, level: f32) -> Result<(), PortError> {
        self.0.set_level_calls.lock().unwrap().push(level);
        *self.0.level.lock().unwrap() = level;
        Ok(())
    }

    fn muted(&self) -> Result<bool, PortError> {
        Ok(*self.0.muted.lock().unwrap())
    }

    fn set_muted(&self, muted: bool) -> Result<(), PortError> {
        self.0.set_muted_calls.lock().unwrap().push(muted);
        *self.0.muted.lock().unwrap() = muted;
        Ok(())
    }

    fn take_events(&mut self) -> Receiver<VolumeEvent> {
        let (tx, rx) = mpsc::channel();
        *self.0.events.lock().unwrap() = Some(tx);
        rx
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
    fn set_default_output_moves_what_default_output_reports() {
        let sys = MockSystem::new(vec![endpoint("out-1"), endpoint("sink")]);

        sys.set_default_output(&EndpointId("sink".into())).unwrap();

        assert_eq!(sys.default_output().unwrap().id, EndpointId("sink".into()));
        assert_eq!(sys.default_output_calls(), vec![EndpointId("sink".into())]);
    }

    #[test]
    fn set_default_output_failure_is_surfaced_not_panicked() {
        let sys = MockSystem::new(vec![endpoint("out-1"), endpoint("sink")]);
        sys.fail_set_default_output();

        let result = sys.set_default_output(&EndpointId("sink".into()));

        assert!(result.is_err());
        assert_eq!(sys.default_output().unwrap().id, EndpointId("out-1".into()));
    }

    #[test]
    fn set_default_output_rejects_an_endpoint_that_is_not_present() {
        let sys = MockSystem::new(vec![endpoint("out-1")]);

        let result = sys.set_default_output(&EndpointId("sink".into()));

        assert!(matches!(result, Err(PortError::NotFound(_))));
        assert!(sys.default_output_calls().is_empty());
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
        sys.seed_default_output(EndpointId("out-2".into()));
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

    // -- audio-flow-control B17: paced mocks -------------------------------

    #[test]
    fn paced_sink_rejects_more_than_its_free_space() {
        let (mut sink, device) = SinkRender::paced(stereo(48_000), 480, 960);
        let block = vec![0.0f32; 480 * 2]; // 480 frames, well under capacity
        assert_eq!(sink.write(&block).unwrap(), 480);
        assert_eq!(device.filled_frames(), 480);

        // Offer another 480 (would be 960, exactly capacity) then a third
        // that must be rejected outright: only 0 free frames remain.
        assert_eq!(sink.write(&block).unwrap(), 480);
        assert_eq!(device.filled_frames(), 960);
        assert_eq!(
            sink.write(&block).unwrap(),
            0,
            "a full device must accept nothing, never silently drop the excess (B1)"
        );
        assert_eq!(device.filled_frames(), 960);
    }

    #[test]
    fn paced_sink_wait_event_blocks_until_drained() {
        let (mut sink, device) = SinkRender::paced(stereo(48_000), 480, 1920);
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let result = sink.wait_event(Duration::from_secs(5));
            done_tx.send(result.is_ok()).unwrap();
        });

        // Nothing has drained yet — the wait must still be pending.
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "wait_event returned before any drain() released it"
        );

        device.drain(480);
        assert!(
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "wait_event must unblock once drain() releases it"
        );
        handle.join().unwrap();
    }

    #[test]
    fn sine_capture_yields_no_more_than_produced() {
        let (mut capture, source) = SineCapture::paced(440.0, stereo(48_000));
        let mut buf = [0.0f32; 20 * 2]; // room for 20 frames

        assert_eq!(capture.read(&mut buf).unwrap(), 0, "nothing produced yet");

        source.produce(8);
        assert_eq!(
            capture.read(&mut buf).unwrap(),
            8 * 2,
            "must yield exactly what was produced, not the whole buffer"
        );
        assert_eq!(capture.read(&mut buf).unwrap(), 0, "produced frames are consumed, not reusable");
    }
}
