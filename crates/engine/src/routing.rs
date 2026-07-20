//! Per-app session routing (session-routing L3/L4). Control-plane only —
//! never touches the audio path (mixer, rings, RT threads). A background
//! thread (not RT) reconciles desired routing state against session
//! enumeration/notifications and applies it through `PolicyPort`, with the
//! same "one notice, skip further calls, retry only on config reload"
//! degradation posture as the drift-and-recovery supervisor.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use audio_core::GroupId;

use crate::ports::{EndpointId, PolicyError, PolicyPort, SessionEvent, SessionPort};
use crate::rules::{match_session, GroupRules, SessionInfo};
use crate::runtime::{EngineError, EngineEvent};

const RECONCILE_TICK: Duration = Duration::from_millis(100);

enum Command {
    UpdateRules(Vec<GroupRules>),
    UpdateTopology {
        buses: HashMap<GroupId, EndpointId>,
        rules: Vec<GroupRules>,
        default_output: Option<EndpointId>,
    },
}

struct State {
    rules: Vec<GroupRules>,
    buses: HashMap<GroupId, EndpointId>,
    default_output: Option<EndpointId>,
    /// pid -> bus endpoint currently applied. Avoids rewriting Windows-persisted
    /// per-app prefs on every reconcile (flow A) and is the source of truth for
    /// flow D's "re-route changed / clear now-unmatched" comparison.
    applied: HashMap<u32, EndpointId>,
    /// pid -> last known session, tracked from `enumerate()` + `SessionEvent`
    /// so flow D ("re-match all live sessions") doesn't need to re-enumerate.
    live_sessions: HashMap<u32, SessionInfo>,
    degraded: bool,
}

pub struct RoutingHandle {
    commands: Sender<Command>,
    degraded: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RoutingHandle {
    pub fn update_rules(&self, rules: Vec<GroupRules>) {
        let _ = self.commands.send(Command::UpdateRules(rules));
    }

    /// Call after every structural rebuild (flow H): fresh bus map + rules;
    /// coordinator reconciles as in flow A (idempotent — already-correct
    /// persisted routes untouched).
    pub fn update_topology(
        &self,
        buses: HashMap<GroupId, EndpointId>,
        rules: Vec<GroupRules>,
        default_output: Option<EndpointId>,
    ) {
        let _ = self.commands.send(Command::UpdateTopology {
            buses,
            rules,
            default_output,
        });
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    /// Flow F: endpoints stay hidden, no un-routing (Windows persists
    /// per-app prefs; uninstaller restores). Just stops the coordinator thread.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Flow A (run synchronously here, before the coordinator thread starts —
/// same startup-is-synchronous convention as `engine::start` opening ports):
/// hide bus endpoints + set the branded default, `enumerate()` (primes
/// session notifications, §9.2), then route already-running matched sessions.
pub fn start_routing(
    rules: Vec<GroupRules>,
    buses: HashMap<GroupId, EndpointId>,
    default_output: Option<EndpointId>,
    mut session: Box<dyn SessionPort>,
    mut policy: Box<dyn PolicyPort>,
    events: Sender<EngineEvent>,
) -> Result<RoutingHandle, EngineError> {
    let mut state = State {
        rules,
        buses,
        default_output,
        applied: HashMap::new(),
        live_sessions: HashMap::new(),
        degraded: false,
    };

    let initial_sessions = session.enumerate().map_err(EngineError::Port)?;
    for s in &initial_sessions {
        state.live_sessions.insert(s.pid, s.clone());
    }
    full_reconcile(&mut state, &initial_sessions, policy.as_mut(), &events);

    let session_events = session.take_events();
    let degraded = Arc::new(AtomicBool::new(state.degraded));
    let stop = Arc::new(AtomicBool::new(false));
    let (commands_tx, commands_rx) = mpsc::channel();

    let ctx = CoordinatorCtx {
        _session: session,
        policy,
        session_events,
        commands: commands_rx,
        events,
        degraded: Arc::clone(&degraded),
        stop: Arc::clone(&stop),
    };
    let thread = thread::spawn(move || coordinator_loop(state, ctx));

    Ok(RoutingHandle {
        commands: commands_tx,
        degraded,
        stop,
        thread: Some(thread),
    })
}

/// Wiring/plumbing for the coordinator thread, grouped once the plain
/// parameter list crossed the too-many-arguments threshold (operational
/// learnings: extract at that point, not later — same idiom as
/// `runtime::CaptureFaultCtx`/`RenderFaultCtx`).
struct CoordinatorCtx {
    /// Never called again after `take_events` in `start_routing` — held alive
    /// for the whole coordinator lifetime anyway, matching the "keep the
    /// callback object alive" gotcha `win-audio`'s COM notification wrappers
    /// already document (dropping it would silently unregister the real
    /// WASAPI notification callback). Underscore-prefixed: never read after
    /// construction, same idiom as `WasapiCapture::_client`.
    _session: Box<dyn SessionPort>,
    policy: Box<dyn PolicyPort>,
    session_events: Receiver<SessionEvent>,
    commands: Receiver<Command>,
    events: Sender<EngineEvent>,
    degraded: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

fn coordinator_loop(mut state: State, mut ctx: CoordinatorCtx) {
    while !ctx.stop.load(Ordering::Relaxed) {
        while let Ok(cmd) = ctx.commands.try_recv() {
            match cmd {
                Command::UpdateRules(rules) => {
                    state.rules = rules;
                    state.degraded = false; // retry only on config reload
                    let sessions: Vec<SessionInfo> = state.live_sessions.values().cloned().collect();
                    reconcile(&mut state, &sessions, ctx.policy.as_mut(), &ctx.events);
                }
                Command::UpdateTopology {
                    buses,
                    rules,
                    default_output,
                } => {
                    state.buses = buses;
                    state.rules = rules;
                    state.default_output = default_output;
                    state.degraded = false;
                    let sessions: Vec<SessionInfo> = state.live_sessions.values().cloned().collect();
                    full_reconcile(&mut state, &sessions, ctx.policy.as_mut(), &ctx.events);
                }
            }
            ctx.degraded.store(state.degraded, Ordering::Relaxed);
        }

        while let Ok(evt) = ctx.session_events.try_recv() {
            match evt {
                SessionEvent::New(info) => {
                    state.live_sessions.insert(info.pid, info.clone());
                    reconcile(&mut state, std::slice::from_ref(&info), ctx.policy.as_mut(), &ctx.events);
                }
                // Flow C: drop from the map, no un-route — Windows persists the
                // per-app pref, so the same group applies again on relaunch.
                SessionEvent::Ended(pid) => {
                    state.live_sessions.remove(&pid);
                    state.applied.remove(&pid);
                }
            }
            ctx.degraded.store(state.degraded, Ordering::Relaxed);
        }

        thread::sleep(RECONCILE_TICK);
    }
}

/// Flow A/H: hide every bus endpoint, set the branded default, then reconcile
/// routes for `sessions`.
fn full_reconcile(
    state: &mut State,
    sessions: &[SessionInfo],
    policy: &mut dyn PolicyPort,
    events: &Sender<EngineEvent>,
) {
    let bus_ids: Vec<EndpointId> = state.buses.values().cloned().collect();
    for bus in &bus_ids {
        apply_policy(state, events, || policy.set_visibility(bus, false));
    }
    if let Some(default) = state.default_output.clone() {
        apply_policy(state, events, || policy.set_default(&default));
    }
    reconcile(state, sessions, policy, events);
}

/// Routes newly-matched/changed sessions and clears now-unmatched ones back
/// to default (flow D). Also correct for flow A/B/H: a session that was
/// never `applied` and still doesn't match anything hits the `(None, None)`
/// no-op arm, same as "unmatched apps stay normal."
fn reconcile(
    state: &mut State,
    sessions: &[SessionInfo],
    policy: &mut dyn PolicyPort,
    events: &Sender<EngineEvent>,
) {
    for session in sessions {
        let desired = match_session(session, &state.rules).and_then(|g| state.buses.get(&g).cloned());
        let pid = session.pid;
        match (desired, state.applied.get(&pid).cloned()) {
            (Some(bus), Some(current)) if bus == current => {} // already correct
            (Some(bus), _) => {
                let target = bus.clone();
                if apply_policy(state, events, || policy.route(pid, &target)) {
                    state.applied.insert(pid, bus);
                }
            }
            (None, Some(_)) => {
                if apply_policy(state, events, || policy.clear_route(pid)) {
                    state.applied.remove(&pid);
                }
            }
            (None, None) => {}
        }
    }
}

/// Degradation posture (flow E): skip the call entirely once degraded;
/// otherwise attempt it, and on the *first* failure set the flag and send
/// exactly one `RoutingDegraded` notice (later calls skip before reaching
/// this function's `Err` arm again, since `state.degraded` is already true).
/// Returns whether the call actually ran and succeeded — callers use that to
/// decide whether to update `state.applied`.
fn apply_policy(
    state: &mut State,
    events: &Sender<EngineEvent>,
    f: impl FnOnce() -> Result<(), PolicyError>,
) -> bool {
    if state.degraded {
        return false;
    }
    match f() {
        Ok(()) => true,
        Err(e) => {
            state.degraded = true;
            let reason = match e {
                PolicyError::Unavailable(msg) => format!("routing unavailable: {msg}"),
                PolicyError::Failed(msg) => format!("routing failed: {msg}"),
            };
            let _ = events.send(EngineEvent::RoutingDegraded { reason });
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::mock::{MockPolicyPort, MockSessionPort};
    use crate::rules::MatchRule;
    use std::time::Duration as StdDuration;

    fn bus(id: &str) -> EndpointId {
        EndpointId(id.to_string())
    }

    fn session(pid: u32, exe: &str) -> SessionInfo {
        SessionInfo {
            pid,
            process_path: exe.into(),
            display_name: exe.into(),
        }
    }

    fn game_rules() -> Vec<GroupRules> {
        vec![GroupRules {
            group: GroupId(0),
            rules: vec![MatchRule::ExactName("game.exe".into())],
        }]
    }

    fn game_buses() -> HashMap<GroupId, EndpointId> {
        HashMap::from([(GroupId(0), bus("bus-game"))])
    }

    fn recv_degraded(events: &Receiver<EngineEvent>) -> String {
        match events.recv_timeout(StdDuration::from_millis(500)) {
            Ok(EngineEvent::RoutingDegraded { reason }) => reason,
            other => panic!("expected RoutingDegraded, got {other:?}"),
        }
    }

    #[test]
    fn startup_hides_every_bus_and_sets_the_branded_default() {
        let policy = MockPolicyPort::new();
        let sessions = MockSessionPort::new(vec![]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(
            vec![],
            game_buses(),
            Some(bus("out-1")),
            Box::new(sessions),
            Box::new(policy.clone()),
            tx,
        )
        .unwrap();

        assert_eq!(policy.is_visible(&bus("bus-game")), Some(false));
        assert_eq!(policy.default_endpoint(), Some(bus("out-1")));
        handle.shutdown();
    }

    #[test]
    fn startup_routes_an_already_running_matched_session() {
        let policy = MockPolicyPort::new();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(
            game_rules(),
            game_buses(),
            None,
            Box::new(sessions),
            Box::new(policy.clone()),
            tx,
        )
        .unwrap();

        assert_eq!(policy.routes().get(&100), Some(&bus("bus-game")));
        handle.shutdown();
    }

    #[test]
    fn startup_leaves_an_unmatched_session_untouched() {
        let policy = MockPolicyPort::new();
        let sessions = MockSessionPort::new(vec![session(100, "other.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(
            game_rules(),
            game_buses(),
            None,
            Box::new(sessions),
            Box::new(policy.clone()),
            tx,
        )
        .unwrap();

        assert!(!policy.routes().contains_key(&100));
        handle.shutdown();
    }

    #[test]
    fn new_session_event_routes_a_matching_session() {
        let policy = MockPolicyPort::new();
        let sessions = MockSessionPort::new(vec![]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(
            game_rules(),
            game_buses(),
            None,
            Box::new(sessions.clone()),
            Box::new(policy.clone()),
            tx,
        )
        .unwrap();

        sessions.emit_event(SessionEvent::New(session(200, "game.exe")));

        let deadline = std::time::Instant::now() + StdDuration::from_secs(2);
        while !policy.routes().contains_key(&200) && std::time::Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        assert_eq!(policy.routes().get(&200), Some(&bus("bus-game")));
        handle.shutdown();
    }

    #[test]
    fn session_ended_drops_the_applied_entry_without_clearing_the_route() {
        let policy = MockPolicyPort::new();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(
            game_rules(),
            game_buses(),
            None,
            Box::new(sessions.clone()),
            Box::new(policy.clone()),
            tx,
        )
        .unwrap();
        assert_eq!(policy.routes().get(&100), Some(&bus("bus-game")));

        sessions.emit_event(SessionEvent::Ended(100));
        thread::sleep(StdDuration::from_millis(250));

        // No un-route call — the last-applied route stays recorded on the
        // mock (Windows itself would keep the persisted pref in place too).
        assert_eq!(policy.routes().get(&100), Some(&bus("bus-game")));
        handle.shutdown();
    }

    #[test]
    fn live_rule_change_reroutes_a_now_matching_session_and_clears_a_now_unmatched_one() {
        let policy = MockPolicyPort::new();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe"), session(200, "music.exe")]);
        let (tx, _rx) = mpsc::channel();

        // Startup: only "game.exe" matches, "music.exe" stays unmatched.
        let handle = start_routing(
            game_rules(),
            game_buses(),
            None,
            Box::new(sessions),
            Box::new(policy.clone()),
            tx,
        )
        .unwrap();
        assert_eq!(policy.routes().get(&100), Some(&bus("bus-game")));
        assert!(!policy.routes().contains_key(&200));

        // Rule change: "game.exe" no longer matches anything, "music.exe" now does.
        let new_rules = vec![GroupRules {
            group: GroupId(0),
            rules: vec![MatchRule::ExactName("music.exe".into())],
        }];
        handle.update_rules(new_rules);
        thread::sleep(StdDuration::from_millis(250));

        assert!(!policy.routes().contains_key(&100), "game.exe should have been cleared");
        assert_eq!(policy.routes().get(&200), Some(&bus("bus-game")));
        handle.shutdown();
    }

    #[test]
    fn first_policy_failure_degrades_and_sends_exactly_one_notice() {
        let policy = MockPolicyPort::new();
        policy.fail_with_unavailable();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe")]);
        let (tx, rx) = mpsc::channel();

        let handle = start_routing(
            game_rules(),
            game_buses(),
            None,
            Box::new(sessions),
            Box::new(policy.clone()),
            tx,
        )
        .unwrap();

        let reason = recv_degraded(&rx);
        assert!(reason.contains("unavailable"), "reason was: {reason}");
        assert!(handle.is_degraded());
        // Nothing got routed — the very first policy call (hiding the bus) failed.
        assert!(!policy.routes().contains_key(&100));
        // Exactly one notice: nothing else queued within a short window.
        assert!(rx.recv_timeout(StdDuration::from_millis(200)).is_err());
        handle.shutdown();
    }

    #[test]
    fn config_reload_via_update_rules_clears_degraded_and_retries() {
        let policy = MockPolicyPort::new();
        policy.fail_with_unavailable();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe")]);
        let (tx, rx) = mpsc::channel();

        let handle = start_routing(
            game_rules(),
            game_buses(),
            None,
            Box::new(sessions),
            Box::new(policy.clone()),
            tx,
        )
        .unwrap();
        recv_degraded(&rx);
        assert!(handle.is_degraded());

        policy.stop_failing();
        handle.update_rules(game_rules());
        thread::sleep(StdDuration::from_millis(250));

        assert!(!handle.is_degraded());
        assert_eq!(policy.routes().get(&100), Some(&bus("bus-game")));
        handle.shutdown();
    }

    #[test]
    fn structural_rebuild_via_update_topology_hides_the_new_bus_and_reroutes() {
        let policy = MockPolicyPort::new();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(
            game_rules(),
            game_buses(),
            None,
            Box::new(sessions),
            Box::new(policy.clone()),
            tx,
        )
        .unwrap();
        assert_eq!(policy.routes().get(&100), Some(&bus("bus-game")));

        // Rebuild gave the group's bus a fresh EndpointId.
        let new_buses = HashMap::from([(GroupId(0), bus("bus-game-2"))]);
        handle.update_topology(new_buses, game_rules(), Some(bus("out-2")));
        thread::sleep(StdDuration::from_millis(250));

        assert_eq!(policy.is_visible(&bus("bus-game-2")), Some(false));
        assert_eq!(policy.default_endpoint(), Some(bus("out-2")));
        assert_eq!(policy.routes().get(&100), Some(&bus("bus-game-2")));
        handle.shutdown();
    }
}
