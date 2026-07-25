//! Two-way sync between the Windows default playback device's master volume
//! and one bound Splitstream target (a group, or master). See
//! `.lattice/context/external-controls.md`. `audio-core` never learns this
//! exists — this module only ever forwards raw [`VolumeEvent`]s outward and
//! applies outbound pushes to the port; deciding *which* config target is
//! bound, translating position to `Gain`, and computing the double-
//! attenuation guard (decision 4) are all app/control-layer knowledge this
//! module never has.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::ports::{AudioSystem, EndpointVolumePort, VolumeEvent};

/// Float-equality tolerance for level comparisons — without it, two
/// notifications carrying the "same" physical position (round-tripped
/// through Windows' own scalar representation) would compare unequal and
/// thrash a redundant sync on every key press.
pub const MIRROR_EPSILON: f32 = 0.005;

const TICK: Duration = Duration::from_millis(50);

/// What [`reconcile`] decided should happen. `PushToEndpoint` carries no
/// `muted` field: mute has no outward-push shape in this design (capability 4
/// — only Windows drives the bound target's mute), so a mute mismatch always
/// resolves as `AdoptFromEndpoint`. Constructed by `reconcile` for the
/// inbound direction; the outward direction (`VolumeBindHandle::push_level`)
/// is a direct call, not routed through this type — see that method's doc.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MirrorAction {
    AdoptFromEndpoint { level: f32, muted: bool },
    PushToEndpoint { level: f32 },
}

/// Pure. `None` = do nothing: suspended (decision 4's double-attenuation
/// guard), or already equal within [`MIRROR_EPSILON`] (the ping-pong guard).
/// Called by the app-layer dispatcher on every inbound [`VolumeEvent`] — it
/// owns both sides' values (the endpoint's fresh reading and the bound
/// target's current gain/mute); this module never learns which target that is.
pub fn reconcile(endpoint: VolumeEvent, target_level: f32, target_muted: bool, suspended: bool) -> Option<MirrorAction> {
    if suspended {
        return None;
    }
    let level_matches = (endpoint.level - target_level).abs() < MIRROR_EPSILON;
    let mute_matches = endpoint.muted == target_muted;
    if level_matches && mute_matches {
        return None;
    }
    Some(MirrorAction::AdoptFromEndpoint { level: endpoint.level, muted: endpoint.muted })
}

enum Command {
    PushLevel(f32),
    PushMuted(bool),
    Rebind,
    SetSuspended(bool),
    Quit,
}

/// Coordinator handle. Owns the outward `VolumeEvent` relay and the
/// background thread that holds the live port (best-effort — a device
/// without volume control, or an open failure, leaves every method here a
/// harmless no-op, capability 7).
pub struct VolumeBindHandle {
    commands: Sender<Command>,
    events: Receiver<VolumeEvent>,
    thread: Option<JoinHandle<()>>,
}

impl VolumeBindHandle {
    /// Raw endpoint events, relayed outward while not suspended — the
    /// dispatcher polls this and calls [`reconcile`] per event. Empty
    /// (never yields) when no port is open or the binding is suspended, same
    /// "stop mirroring both ways" behavior flow D describes: suspended holds
    /// at the transport level, not just as advice to the caller.
    pub fn events(&self) -> &Receiver<VolumeEvent> {
        &self.events
    }

    /// Best-effort outbound push (flow C) — a direct call, not decided by
    /// [`reconcile`]: the dispatcher already knows the bound target's gain
    /// just changed (that's why it's calling this), so there's nothing left
    /// to decide except "am I suspended," which this handle already tracks
    /// and gates on internally. Silently inert when unbound, suspended, or
    /// the port failed to open.
    pub fn push_level(&self, level: f32) {
        let _ = self.commands.send(Command::PushLevel(level));
    }

    pub fn push_muted(&self, muted: bool) {
        let _ = self.commands.send(Command::PushMuted(muted));
    }

    /// Re-opens against the new default device (flow E) — drops the old
    /// port (its `Drop` unregisters) and attempts a fresh
    /// `open_default_endpoint_volume()`. Best-effort: failure leaves the
    /// binding idle, same as never having opened one.
    pub fn rebind(&self) {
        let _ = self.commands.send(Command::Rebind);
    }

    /// Flow D's guard. While suspended, inbound events stop reaching
    /// [`Self::events`] and outbound pushes become no-ops — enforced here,
    /// not left to callers to remember to check.
    pub fn set_suspended(&self, suspended: bool) {
        let _ = self.commands.send(Command::SetSuspended(suspended));
    }

    pub fn shutdown(self) {
        let _ = self.commands.send(Command::Quit);
        if let Some(t) = self.thread {
            let _ = t.join();
        }
    }
}

pub fn start_volume_bind(sys: Arc<dyn AudioSystem>) -> VolumeBindHandle {
    let (commands_tx, commands_rx) = mpsc::channel();
    let (events_tx, events_rx) = mpsc::channel();

    let thread = thread::spawn(move || coordinator_loop(sys, commands_rx, events_tx));

    VolumeBindHandle {
        commands: commands_tx,
        events: events_rx,
        thread: Some(thread),
    }
}

fn coordinator_loop(sys: Arc<dyn AudioSystem>, commands: Receiver<Command>, events: Sender<VolumeEvent>) {
    let mut port = open(&sys);
    let mut port_events = port.as_mut().map(|p| p.take_events());
    // Starts suspended: nothing mirrors until the dispatcher explicitly says
    // otherwise (its first `set_suspended(false)` call then doubles as flow
    // A's "Windows wins" bind-time adopt, via the same leaving-suspended
    // synthetic-event path below — the two are the same mechanism, per flow
    // D's own "re-adopt (flow A)" text).
    let mut suspended = true;

    loop {
        let mut should_quit = false;
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                Command::PushLevel(level) => {
                    if !suspended {
                        if let Some(p) = &port {
                            let _ = p.set_level(level);
                        }
                    }
                }
                Command::PushMuted(muted) => {
                    if !suspended {
                        if let Some(p) = &port {
                            let _ = p.set_muted(muted);
                        }
                    }
                }
                Command::Rebind => {
                    port = open(&sys);
                    port_events = port.as_mut().map(|p| p.take_events());
                    // Flow E: a rebind is itself a fresh "binding engages"
                    // moment (flow A) whenever not suspended, independent of
                    // whether the suspended *value* happens to be unchanged
                    // — the underlying device (and its level) is new either
                    // way, so a value-unchanged suspended flag must not
                    // suppress the re-adopt the way it would for a plain
                    // `SetSuspended` call.
                    if !suspended {
                        adopt_now(&port, &events);
                    }
                }
                Command::SetSuspended(s) => {
                    if suspended && !s {
                        // Leaving suspended (flow D) -- re-adopt the
                        // endpoint's current value (flow A).
                        adopt_now(&port, &events);
                    }
                    suspended = s;
                }
                Command::Quit => should_quit = true,
            }
        }
        if should_quit {
            return;
        }

        if !suspended {
            if let Some(rx) = &port_events {
                while let Ok(evt) = rx.try_recv() {
                    if events.send(evt).is_err() {
                        return; // every consumer gone
                    }
                }
            }
        }

        thread::sleep(TICK);
    }
}

/// Synthesizes one `VolumeEvent` from a direct read of `port`'s current
/// state (flows A/D-E) rather than waiting for an incidental notification
/// that might not arrive for a while. No-op when there's no port, or the
/// read itself fails — both already-inert cases (capability 7).
fn adopt_now(port: &Option<Box<dyn EndpointVolumePort>>, events: &Sender<VolumeEvent>) {
    if let Some(p) = port {
        if let (Ok(level), Ok(muted)) = (p.level(), p.muted()) {
            let _ = events.send(VolumeEvent { level, muted });
        }
    }
}

/// Best-effort open — logged, never fatal (capability 7: a device without
/// volume control, or any open failure, leaves the binding idle).
fn open(sys: &Arc<dyn AudioSystem>) -> Option<Box<dyn EndpointVolumePort>> {
    match sys.open_default_endpoint_volume() {
        Ok(port) => Some(port),
        Err(e) => {
            tracing::info!(?e, "volume_bind: no endpoint volume port available");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::mock::MockSystem;
    use std::time::Duration as StdDuration;

    fn event(level: f32, muted: bool) -> VolumeEvent {
        VolumeEvent { level, muted }
    }

    #[test]
    fn a_suspended_binding_mirrors_nothing() {
        assert_eq!(reconcile(event(0.8, false), 0.2, false, true), None);
    }

    #[test]
    fn equal_within_epsilon_produces_no_action() {
        assert_eq!(reconcile(event(0.5, false), 0.5 + MIRROR_EPSILON / 2.0, false, false), None);
    }

    #[test]
    fn an_endpoint_change_is_adopted() {
        let action = reconcile(event(0.8, false), 0.2, false, false);
        assert_eq!(action, Some(MirrorAction::AdoptFromEndpoint { level: 0.8, muted: false }));
    }

    #[test]
    fn a_mute_change_is_adopted() {
        let action = reconcile(event(0.5, true), 0.5, false, false);
        assert_eq!(action, Some(MirrorAction::AdoptFromEndpoint { level: 0.5, muted: true }));
    }

    #[test]
    fn a_target_change_is_pushed_outward() {
        // "Pushed outward" is VolumeBindHandle::push_level's own direct
        // plumbing to the port (see that method's doc — the outward
        // direction doesn't route through `reconcile`, which only ever
        // decides the inbound-adopt case). This proves a dispatcher calling
        // push_level after a fader move actually reaches the port.
        let sys = Arc::new(MockSystem::new(vec![]));
        let port = sys.enable_endpoint_volume(0.2, false);
        let handle = start_volume_bind(sys as Arc<dyn AudioSystem>);
        handle.set_suspended(false);

        handle.push_level(0.8);
        thread::sleep(StdDuration::from_millis(150));

        assert_eq!(port.set_level_calls(), vec![0.8]);
        handle.shutdown();
    }

    #[test]
    fn an_inbound_endpoint_event_is_relayed_when_not_suspended() {
        let sys = Arc::new(MockSystem::new(vec![]));
        let port = sys.enable_endpoint_volume(0.2, false);
        let handle = start_volume_bind(sys as Arc<dyn AudioSystem>);
        thread::sleep(StdDuration::from_millis(100)); // let the coordinator open + take_events
        handle.set_suspended(false);
        // Leaving suspended synthesizes one adopt event from the port's
        // current (0.2, false) reading (flow A/D) — drain it before the one
        // this test actually cares about.
        let _ = handle.events().recv_timeout(StdDuration::from_secs(1));

        port.emit(event(0.9, false));

        let received = handle.events().recv_timeout(StdDuration::from_secs(1)).expect("expected a relayed event");
        assert_eq!(received, event(0.9, false));
        handle.shutdown();
    }

    #[test]
    fn a_suspended_handle_relays_no_inbound_events_and_pushes_nothing() {
        let sys = Arc::new(MockSystem::new(vec![]));
        let port = sys.enable_endpoint_volume(0.2, false);
        let handle = start_volume_bind(sys as Arc<dyn AudioSystem>);
        thread::sleep(StdDuration::from_millis(100));

        handle.set_suspended(true);
        thread::sleep(StdDuration::from_millis(100));
        port.emit(event(0.9, false));
        handle.push_level(0.5);
        thread::sleep(StdDuration::from_millis(150));

        assert!(handle.events().try_recv().is_err(), "suspended: no inbound relay");
        assert!(port.set_level_calls().is_empty(), "suspended: no outbound push");
        handle.shutdown();
    }

    #[test]
    fn leaving_suspended_synthesizes_one_adopt_event_from_the_ports_current_state() {
        // Flow A/D: binding engages (or re-engages after the guard lifts) by
        // reading the port directly, not by waiting for an incidental
        // notification that might not arrive for a while.
        let sys = Arc::new(MockSystem::new(vec![]));
        sys.enable_endpoint_volume(0.6, true);
        let handle = start_volume_bind(sys as Arc<dyn AudioSystem>);
        thread::sleep(StdDuration::from_millis(100));

        handle.set_suspended(false);

        let received = handle.events().recv_timeout(StdDuration::from_secs(1)).expect("expected a synthesized event");
        assert_eq!(received, event(0.6, true));
        handle.shutdown();
    }

    #[test]
    fn re_suspending_then_unsuspending_again_synthesizes_a_fresh_read_each_time() {
        let sys = Arc::new(MockSystem::new(vec![]));
        let port = sys.enable_endpoint_volume(0.3, false);
        let handle = start_volume_bind(sys as Arc<dyn AudioSystem>);
        thread::sleep(StdDuration::from_millis(100));

        handle.set_suspended(false);
        assert_eq!(
            handle.events().recv_timeout(StdDuration::from_secs(1)).unwrap(),
            event(0.3, false)
        );

        handle.set_suspended(true);
        port.emit(event(0.7, true)); // must not relay while suspended
        thread::sleep(StdDuration::from_millis(100));

        handle.set_suspended(false); // re-adopts whatever the endpoint has now
        assert_eq!(
            handle.events().recv_timeout(StdDuration::from_secs(1)).unwrap(),
            event(0.7, true)
        );
        handle.shutdown();
    }

    #[test]
    fn rebinding_while_not_suspended_re_adopts_even_if_the_suspended_value_is_unchanged() {
        // Regression: a value-unchanged `set_suspended(false)` after a
        // rebind must not suppress the re-adopt just because the coordinator's
        // own transition-detection sees no false->false transition -- the
        // underlying device (and its level) is new either way.
        let sys = Arc::new(MockSystem::new(vec![]));
        sys.enable_endpoint_volume(0.4, false);
        let handle = start_volume_bind(Arc::clone(&sys) as Arc<dyn AudioSystem>);
        thread::sleep(StdDuration::from_millis(100));
        handle.set_suspended(false);
        let _ = handle.events().recv_timeout(StdDuration::from_secs(1)).expect("bind-time adopt");

        handle.rebind();
        handle.set_suspended(false); // same value as before -- must still re-adopt

        let received = handle
            .events()
            .recv_timeout(StdDuration::from_secs(1))
            .expect("expected a re-adopt event after rebind");
        assert_eq!(received, event(0.4, false));
        handle.shutdown();
    }

    #[test]
    fn a_binding_with_no_available_port_is_inert_not_fatal() {
        let sys = Arc::new(MockSystem::new(vec![])); // enable_endpoint_volume never called
        let handle = start_volume_bind(sys as Arc<dyn AudioSystem>);

        handle.push_level(0.5); // must not panic
        handle.set_suspended(false);
        handle.rebind();
        thread::sleep(StdDuration::from_millis(100));
        assert!(handle.events().try_recv().is_err());

        handle.shutdown();
    }
}
