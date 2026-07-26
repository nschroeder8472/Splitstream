//! Per-app session routing (process-loopback-capture L3/L4). Control-plane
//! side: a background thread (not RT) matches live sessions against config
//! rules and drives capture-source changes directly through `CaptureControl`
//! — deliberately crosses the old P3-era "control-plane only, never touches
//! the audio path" boundary (accepted judgment call, logged in the context
//! doc: `engine::routing` and `engine::runtime` are peer modules in the same
//! crate, and session-matching now directly determines what gets captured).
//!
//! No more `PolicyPort`/buses/`default_output` — the pivot from per-app
//! WASAPI redirect to per-process loopback capture means there is nothing
//! left to hide or redirect; a matched session's audio is captured directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use audio_core::GroupId;

use crate::ports::{SessionEvent, SessionPort};
use crate::rules::{match_session, GroupRules, MatchKind, SessionInfo};
use crate::runtime::{CaptureControl, EngineError, EngineEvent};

const RECONCILE_TICK: Duration = Duration::from_millis(100);

enum Command {
    UpdateRules(Vec<GroupRules>, Vec<String>),
}

/// A routed session paired with *how* it matched (routing-truthfulness.md
/// capability 4) — computed once here, where the decision is actually made,
/// rather than the UI re-deriving catch-all-ness from raw rule strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedSession {
    pub info: SessionInfo,
    pub kind: MatchKind,
}

struct State {
    rules: Vec<GroupRules>,
    /// Process file names no group may claim (routing-truthfulness.md) —
    /// checked ahead of every rule tier in `match_session`. Config
    /// vocabulary, distinct from `self_pid` below (a runtime fact).
    excluded: Vec<String>,
    /// Splitstream's own pid — enforced centrally here (not per-flow) so no
    /// group's match rules, including a `*` catch-all, can ever resolve to
    /// capturing Splitstream's own render output back into itself (L3
    /// "Self-exclusion" safety rule).
    self_pid: u32,
    /// Group -> (pid, MatchKind) set actually confirmed applied last
    /// reconcile (desired minus whatever failed to open that pass) — the
    /// source of truth for `current_routes()`; `CaptureControl` owns the
    /// real running set.
    applied: HashMap<GroupId, Vec<(u32, MatchKind)>>,
    /// pid -> last known session, tracked from `enumerate()` + `SessionEvent`
    /// so reconcile never needs to re-enumerate.
    live_sessions: HashMap<u32, SessionInfo>,
}

/// Grouped by `GroupId`, sorted by it for deterministic reads — a plain
/// `Mutex` (not a lock-free structure) is fine here: read by the UI/tray on
/// a control-thread poll, never from the RT audio path.
type RoutesSnapshot = Arc<Mutex<Vec<(GroupId, Vec<RoutedSession>)>>>;

/// Every live session, matched or not, sorted by pid — same rationale as
/// `RoutesSnapshot` (mixer-ui-redesign L4): the settings window's
/// unassigned-pool source (Master's footer = this minus every pid in
/// `RoutesSnapshot`).
type AllSessionsSnapshot = Arc<Mutex<Vec<SessionInfo>>>;

pub struct RoutingHandle {
    commands: Sender<Command>,
    degraded: Arc<AtomicBool>,
    routes: RoutesSnapshot,
    all_sessions: AllSessionsSnapshot,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Read-only, `Clone`-able view onto `RoutingHandle`'s state — for a
/// consumer (app-shell.md's settings window) that needs to poll routes/
/// degradation every frame but must never call `update_rules`/`shutdown`.
/// Split out because `RoutingHandle` itself can't be `Clone` (it owns the
/// coordinator thread's `JoinHandle`, which isn't).
#[derive(Clone)]
pub struct RoutingReader {
    degraded: Arc<AtomicBool>,
    routes: RoutesSnapshot,
    all_sessions: AllSessionsSnapshot,
}

impl RoutingReader {
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    pub fn current_routes(&self) -> Vec<(GroupId, Vec<RoutedSession>)> {
        self.routes.lock().unwrap().clone()
    }

    /// Every live session, matched or not — the settings window's
    /// unassigned-pool source (mixer-ui-redesign L4).
    pub fn all_sessions(&self) -> Vec<SessionInfo> {
        self.all_sessions.lock().unwrap().clone()
    }
}

impl RoutingHandle {
    /// Single reconcile entrypoint (merges the old P3-era `update_rules`/
    /// `update_topology` split — neither ever needed a `buses`/
    /// `default_output` param anymore, so one method covers both call sites:
    /// a config edit and a structural rebuild both just re-match live
    /// sessions against fresh rules).
    pub fn update_rules(&self, rules: Vec<GroupRules>, excluded: Vec<String>) {
        let _ = self.commands.send(Command::UpdateRules(rules, excluded));
    }

    /// Soft, per-attempt signal (L3 flow E) — reflects only the most recent
    /// reconcile pass's outcome, never a sticky "everything is broken" flag
    /// that lingers once whatever failed stops being requested.
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    /// Snapshot of currently-applied routes, grouped by `GroupId` — the
    /// settings window's live routed-apps list (app-shell.md L1 §2). Reflects
    /// the coordinator's last reconcile, not a live query.
    pub fn current_routes(&self) -> Vec<(GroupId, Vec<RoutedSession>)> {
        self.routes.lock().unwrap().clone()
    }

    /// Every live session, matched or not — the settings window's
    /// unassigned-pool source (mixer-ui-redesign L4). Reflects the
    /// coordinator's last reconcile, not a live query.
    pub fn all_sessions(&self) -> Vec<SessionInfo> {
        self.all_sessions.lock().unwrap().clone()
    }

    /// A cloneable read-only handle for a consumer that only needs
    /// `is_degraded`/`current_routes`/`all_sessions` — see [`RoutingReader`].
    pub fn reader(&self) -> RoutingReader {
        RoutingReader {
            degraded: Arc::clone(&self.degraded),
            routes: Arc::clone(&self.routes),
            all_sessions: Arc::clone(&self.all_sessions),
        }
    }

    /// Stops the coordinator thread. Nothing Windows-side to restore
    /// (process-loopback-capture pivot: no hidden devices, no persisted
    /// redirect) — `CaptureControl`'s own shutdown path (via
    /// `EngineHandle::shutdown`) tears down any still-running capture
    /// threads.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Flow A: `enumerate()` (primes session notifications), then reconcile
/// already-running matched sessions before the coordinator thread starts
/// (same startup-is-synchronous convention as `engine::start` opening ports).
pub fn start_routing(
    rules: Vec<GroupRules>,
    excluded: Vec<String>,
    self_pid: u32,
    mut session: Box<dyn SessionPort>,
    capture: CaptureControl,
    events: Sender<EngineEvent>,
) -> Result<RoutingHandle, EngineError> {
    let mut state = State {
        rules,
        excluded,
        self_pid,
        applied: HashMap::new(),
        live_sessions: HashMap::new(),
    };

    let initial_sessions = session.enumerate().map_err(EngineError::Port)?;
    for s in &initial_sessions {
        state.live_sessions.insert(s.pid, s.clone());
    }
    let initially_degraded = reconcile(&mut state, &capture, &events);

    let session_events = session.take_events();
    let degraded = Arc::new(AtomicBool::new(initially_degraded));
    let routes = Arc::new(Mutex::new(compute_routes(&state)));
    let all_sessions = Arc::new(Mutex::new(compute_all_sessions(&state)));
    let stop = Arc::new(AtomicBool::new(false));
    let (commands_tx, commands_rx) = mpsc::channel();

    let ctx = CoordinatorCtx {
        _session: session,
        capture,
        session_events,
        commands: commands_rx,
        events,
        degraded: Arc::clone(&degraded),
        routes: Arc::clone(&routes),
        all_sessions: Arc::clone(&all_sessions),
        stop: Arc::clone(&stop),
    };
    let thread = thread::spawn(move || coordinator_loop(state, ctx));

    Ok(RoutingHandle {
        commands: commands_tx,
        degraded,
        routes,
        all_sessions,
        stop,
        thread: Some(thread),
    })
}

/// Every session currently matching a group, keyed by `GroupId` — every
/// group named in `rules` gets an entry (possibly empty), so a group that
/// used to have matches but no longer does still gets reconciled down to
/// nothing rather than silently left with its last-applied set.
/// Self-exclusion (L3 safety rule) is enforced here, centrally, once: `self_pid`
/// is filtered out before matching, so no rule — including a `*` catch-all —
/// can ever resolve to it.
fn compute_desired(
    rules: &[GroupRules],
    excluded: &[String],
    live_sessions: &HashMap<u32, SessionInfo>,
    self_pid: u32,
) -> HashMap<GroupId, Vec<(u32, MatchKind)>> {
    let mut desired: HashMap<GroupId, Vec<(u32, MatchKind)>> = HashMap::new();
    for gr in rules {
        desired.entry(gr.group).or_default();
    }
    for (pid, info) in live_sessions {
        if *pid == self_pid {
            continue;
        }
        if let Some(m) = match_session(info, rules, excluded) {
            desired.entry(m.group).or_default().push((*pid, m.kind));
        }
    }
    desired
}

/// Recomputes every group's desired pid set and applies it through
/// `CaptureControl` — `apply_capture_sources` diffs internally, so calling
/// this every tick regardless of whether anything actually changed is cheap
/// (L3 flow E: also exactly what makes a persistently-failing pid retry
/// "every time" rather than needing a separate retry mechanism). Returns
/// whether *any* group had a pid fail to open this pass — the caller stores
/// that as the new (non-sticky) degraded signal.
///
/// Owns no session-mute lifecycle: muting a captured app's session silences
/// the `PROCESS_LOOPBACK` tap itself (measured 2026-07-25, MT1), so
/// session-mute-on-capture's whole mechanism was void, not merely buggy.
/// Double-audio is prevented by pointing the Windows default at an unheard
/// sink instead (double-audio-prevention), which touches nothing here.
fn reconcile(state: &mut State, capture: &CaptureControl, events: &Sender<EngineEvent>) -> bool {
    let desired = compute_desired(&state.rules, &state.excluded, &state.live_sessions, state.self_pid);
    let mut any_failure = false;

    for (group, pids) in desired {
        let pid_list: Vec<u32> = pids.iter().map(|(pid, _)| *pid).collect();
        match capture.apply_capture_sources(group, pid_list) {
            Ok(failed) => {
                if !failed.is_empty() {
                    any_failure = true;
                    tracing::warn!(?group, ?failed, "routing: capture failed for pid(s)");
                    let _ = events.send(EngineEvent::RoutingDegraded {
                        reason: format!("group {group:?}: capture failed for pid(s) {failed:?}"),
                    });
                }
                let applied: Vec<(u32, MatchKind)> =
                    pids.into_iter().filter(|(p, _)| !failed.contains(p)).collect();
                state.applied.insert(group, applied);
            }
            // Engine already stopped — nothing more this pass can do; leave
            // `state.applied` as it was (stale but harmless, no consumer
            // reads it once the engine is down).
            Err(_) => any_failure = true,
        }
    }

    any_failure
}

/// Every live session, matched or not, sorted by pid (mixer-ui-redesign L4)
/// — the settings window's unassigned-pool source. Pure.
fn compute_all_sessions(state: &State) -> Vec<SessionInfo> {
    let mut sessions: Vec<SessionInfo> = state.live_sessions.values().cloned().collect();
    sessions.sort_by_key(|s| s.pid);
    sessions
}

/// Pairs each applied pid with its `SessionInfo`, sorted by `GroupId` for a
/// deterministic read. A pid missing from `live_sessions` (shouldn't happen —
/// `applied` is only ever derived from `live_sessions` in the same reconcile
/// pass) is skipped rather than panicking.
fn compute_routes(state: &State) -> Vec<(GroupId, Vec<RoutedSession>)> {
    let mut routes: Vec<(GroupId, Vec<RoutedSession>)> = state
        .applied
        .iter()
        .filter(|(_, pids)| !pids.is_empty())
        .map(|(group, pids)| {
            let sessions = pids
                .iter()
                .filter_map(|(pid, kind)| {
                    state.live_sessions.get(pid).cloned().map(|info| RoutedSession { info, kind: *kind })
                })
                .collect();
            (*group, sessions)
        })
        .collect();
    routes.sort_by_key(|(g, _)| g.0);
    routes
}

/// Wiring/plumbing for the coordinator thread, grouped once the plain
/// parameter list crossed the too-many-arguments threshold (operational
/// learnings: extract at that point, not later).
struct CoordinatorCtx {
    /// Held alive for the whole coordinator lifetime, and for that reason
    /// only: dropping it would silently unregister the real WASAPI
    /// session-notification callback (the same gotcha `win-audio`'s COM
    /// notification wrappers document), and the `session_events` receiver
    /// below would go quiet. Never called after `take_events` — the
    /// mute/unmute lifecycle that used to call it is gone (see `reconcile`).
    _session: Box<dyn SessionPort>,
    capture: CaptureControl,
    session_events: Receiver<SessionEvent>,
    commands: Receiver<Command>,
    events: Sender<EngineEvent>,
    degraded: Arc<AtomicBool>,
    routes: RoutesSnapshot,
    all_sessions: AllSessionsSnapshot,
    stop: Arc<AtomicBool>,
}

fn coordinator_loop(mut state: State, ctx: CoordinatorCtx) {
    while !ctx.stop.load(Ordering::Relaxed) {
        while let Ok(cmd) = ctx.commands.try_recv() {
            match cmd {
                Command::UpdateRules(rules, excluded) => {
                    state.rules = rules;
                    state.excluded = excluded;
                    tracing::info!(
                        live_sessions = state.live_sessions.len(),
                        "routing: rules updated"
                    );
                }
            }
        }

        while let Ok(evt) = ctx.session_events.try_recv() {
            match evt {
                SessionEvent::New(info) => {
                    tracing::info!(pid = info.pid, path = %info.process_path.display(), "routing: new session");
                    state.live_sessions.insert(info.pid, info);
                }
                // Flow C: drop from the map — the next reconcile below naturally
                // excludes this pid from every group's desired set, and
                // `CaptureControl` stops its capture thread as a normal diff
                // removal. Nothing Windows-side needs restoring (unlike the
                // old policy-routing model).
                SessionEvent::Ended(pid) => {
                    state.live_sessions.remove(&pid);
                }
            }
        }

        let any_failure = reconcile(&mut state, &ctx.capture, &ctx.events);
        ctx.degraded.store(any_failure, Ordering::Relaxed);
        *ctx.routes.lock().unwrap() = compute_routes(&state);
        *ctx.all_sessions.lock().unwrap() = compute_all_sessions(&state);

        thread::sleep(RECONCILE_TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::mock::{MockSessionPort, MockSystem};
    use crate::rules::MatchRule;
    use crate::runtime;
    use std::time::Duration as StdDuration;

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

    fn routed(pid: u32, exe: &str, kind: MatchKind) -> RoutedSession {
        RoutedSession { info: session(pid, exe), kind }
    }

    fn test_capture() -> (CaptureControl, runtime::EngineHandle) {
        let sys: Arc<dyn crate::ports::AudioSystem> = Arc::new(MockSystem::new(vec![]));
        let snapshot = crate::graph::ConfigSnapshot {
            schema_version: 2,
            master: audio_core::Gain::UNITY,
            muted: false,
            app: crate::graph::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![],
        };
        let handle = runtime::start(&snapshot, sys).unwrap();
        let capture = handle.capture_control();
        (capture, handle)
    }

    #[test]
    fn startup_routes_an_already_running_matched_session() {
        let (capture, engine) = test_capture();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(game_rules(), vec![], 0, Box::new(sessions), capture, tx).unwrap();

        assert_eq!(
            handle.current_routes(),
            vec![(GroupId(0), vec![routed(100, "game.exe", MatchKind::Name)])]
        );
        handle.shutdown();
        engine.shutdown().unwrap();
    }

    #[test]
    fn startup_leaves_an_unmatched_session_untouched() {
        let (capture, engine) = test_capture();
        let sessions = MockSessionPort::new(vec![session(100, "other.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(game_rules(), vec![], 0, Box::new(sessions), capture, tx).unwrap();

        assert!(handle.current_routes().is_empty());
        handle.shutdown();
        engine.shutdown().unwrap();
    }

    #[test]
    fn all_sessions_includes_both_matched_and_unmatched_sessions_sorted_by_pid() {
        let (capture, engine) = test_capture();
        let sessions = MockSessionPort::new(vec![session(200, "other.exe"), session(100, "game.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(game_rules(), vec![], 0, Box::new(sessions), capture, tx).unwrap();

        assert_eq!(
            handle.all_sessions(),
            vec![session(100, "game.exe"), session(200, "other.exe")]
        );
        handle.shutdown();
        engine.shutdown().unwrap();
    }

    #[test]
    fn reader_all_sessions_matches_the_owning_handle() {
        let (capture, engine) = test_capture();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(game_rules(), vec![], 0, Box::new(sessions), capture, tx).unwrap();
        let reader = handle.reader();

        assert_eq!(reader.all_sessions(), handle.all_sessions());
        handle.shutdown();
        engine.shutdown().unwrap();
    }

    #[test]
    fn self_pid_is_never_matched_even_by_a_catch_all_rule() {
        let (capture, engine) = test_capture();
        let catch_all = vec![GroupRules {
            group: GroupId(0),
            rules: vec![MatchRule::Glob(crate::rules::GlobPattern::new("*"))],
        }];
        let sessions = MockSessionPort::new(vec![session(999, "splitstream.exe"), session(100, "game.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(catch_all, vec![], 999, Box::new(sessions), capture, tx).unwrap();

        let routes = handle.current_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].1, vec![routed(100, "game.exe", MatchKind::CatchAll)]);
        handle.shutdown();
        engine.shutdown().unwrap();
    }

    #[test]
    fn new_session_event_routes_a_matching_session() {
        let (capture, engine) = test_capture();
        let sessions = MockSessionPort::new(vec![]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(game_rules(), vec![], 0, Box::new(sessions.clone()), capture, tx).unwrap();
        sessions.emit_event(SessionEvent::New(session(200, "game.exe")));

        let deadline = std::time::Instant::now() + StdDuration::from_secs(2);
        let mut routes = handle.current_routes();
        while routes.is_empty() && std::time::Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(20));
            routes = handle.current_routes();
        }
        assert_eq!(routes, vec![(GroupId(0), vec![routed(200, "game.exe", MatchKind::Name)])]);
        handle.shutdown();
        engine.shutdown().unwrap();
    }

    #[test]
    fn session_ended_drops_it_from_routes() {
        let (capture, engine) = test_capture();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(game_rules(), vec![], 0, Box::new(sessions.clone()), capture, tx).unwrap();
        assert!(!handle.current_routes().is_empty());

        sessions.emit_event(SessionEvent::Ended(100));
        let deadline = std::time::Instant::now() + StdDuration::from_secs(2);
        let mut routes = handle.current_routes();
        while !routes.is_empty() && std::time::Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(20));
            routes = handle.current_routes();
        }
        assert!(routes.is_empty());
        handle.shutdown();
        engine.shutdown().unwrap();
    }

    #[test]
    fn live_rule_change_reroutes_a_now_matching_session_and_clears_a_now_unmatched_one() {
        let (capture, engine) = test_capture();
        let sessions = MockSessionPort::new(vec![session(100, "game.exe"), session(200, "music.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(game_rules(), vec![], 0, Box::new(sessions), capture, tx).unwrap();
        assert_eq!(
            handle.current_routes(),
            vec![(GroupId(0), vec![routed(100, "game.exe", MatchKind::Name)])]
        );

        let new_rules = vec![GroupRules {
            group: GroupId(0),
            rules: vec![MatchRule::ExactName("music.exe".into())],
        }];
        handle.update_rules(new_rules, vec![]);

        let deadline = std::time::Instant::now() + StdDuration::from_secs(2);
        let mut routes = handle.current_routes();
        while routes.first().is_some_and(|(_, s)| s.iter().any(|r| r.info.pid == 100))
            && std::time::Instant::now() < deadline
        {
            thread::sleep(StdDuration::from_millis(20));
            routes = handle.current_routes();
        }
        assert_eq!(routes, vec![(GroupId(0), vec![routed(200, "music.exe", MatchKind::Name)])]);
        handle.shutdown();
        engine.shutdown().unwrap();
    }

    #[test]
    fn a_pid_that_fails_to_open_is_reported_degraded_without_blocking_others() {
        let sys_inner = Arc::new(MockSystem::new(vec![]));
        sys_inner.fail_process_capture(100);
        let sys: Arc<dyn crate::ports::AudioSystem> = sys_inner;
        let snapshot = crate::graph::ConfigSnapshot {
            schema_version: 2,
            master: audio_core::Gain::UNITY,
            muted: false,
            app: crate::graph::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![],
        };
        let engine = runtime::start(&snapshot, sys).unwrap();
        let capture = engine.capture_control();

        let rules = vec![GroupRules {
            group: GroupId(0),
            rules: vec![
                MatchRule::ExactName("game.exe".into()),
                MatchRule::ExactName("music.exe".into()),
            ],
        }];
        let sessions = MockSessionPort::new(vec![session(100, "game.exe"), session(200, "music.exe")]);
        let (tx, _rx) = mpsc::channel();

        let handle = start_routing(rules, vec![], 0, Box::new(sessions), capture, tx).unwrap();

        assert!(handle.is_degraded(), "pid 100's activation was made to fail");
        let routes = handle.current_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].1, vec![routed(200, "music.exe", MatchKind::Name)], "pid 200 still applied");
        handle.shutdown();
        engine.shutdown().unwrap();
    }

}
