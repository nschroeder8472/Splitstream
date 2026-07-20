//! Splitstream shell binary: single-instance guard, autostart, engine +
//! session-routing startup, config hot-reload, tray + hotkeys + settings
//! window. See `.lattice/context/app-shell.md` (P4).

mod event_pump;
mod hotkeys;
mod lifecycle;
mod tray;
mod ui;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use control::{group_rules, ConfigEdit, ConfigStore};
use engine::ports::{AudioSystem, EndpointId, PolicyPort, SessionPort};
use engine::{ConfigSnapshot, EngineEvent, EngineHandle, RoutingHandle};
use win_audio::WasapiSystem;

use event_pump::{EventPump, UiState};
use lifecycle::{InstanceGuard, InstanceOutcome};

/// Unifies tray/hotkey/UI intents into one dispatch point — see app-shell.md
/// L2. `ToggleMute` isn't in L4's literal enum: it's an implementation-time
/// addition (hotkeys.rs/tray.rs fire it without knowing the current `muted`
/// value; only the dispatcher, which owns the live snapshot, can resolve
/// `!current` into a concrete `ConfigEdit::SetMuted`).
#[derive(Debug, Clone)]
pub enum ShellAction {
    EditParams(Vec<ConfigEdit>),
    EditStructure(Vec<ConfigEdit>),
    ToggleMute,
    ShowSettings,
    Quit,
}

/// No platform-config-directory convention has been decided yet — defaults
/// to the current directory; override with a path argument.
fn config_path() -> PathBuf {
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("splitstream.toml"))
}

/// The bundled virtual driver product is still an open question (spec §15.2),
/// so there's no real "Splitstream Bus" device on a dev machine yet — this
/// lets a real device stand in for one without editing code.
fn bus_name_prefix() -> String {
    std::env::var("SPLITSTREAM_BUS_PREFIX").unwrap_or_else(|_| "Splitstream Bus".to_string())
}

/// Re-resolves group -> bus `EndpointId` from a fresh device enumeration.
/// `EngineHandle` has no accessor for its internally-resolved graph, so
/// routing (a separate subsystem, control-plane only) resolves its own copy
/// the same way `engine::start`/`rebuild` does internally — cheap, non-RT.
fn routing_buses(sys: &dyn AudioSystem, snapshot: &ConfigSnapshot) -> HashMap<audio_core::GroupId, EndpointId> {
    let endpoints = sys.enumerate().unwrap_or_default();
    match engine::graph::resolve(snapshot, &endpoints, &HashSet::new()) {
        Ok(plan) => plan.group_endpoints.into_iter().collect(),
        Err(e) => {
            eprintln!("routing: bus resolution failed, routing table left empty: {e:?}");
            HashMap::new()
        }
    }
}

/// Bundles everything a `ShellAction`/watcher-delivered snapshot needs to
/// react — extracted once the handler functions' parameter lists crossed the
/// too-many-arguments threshold (same idiom already used for
/// `engine::runtime`'s thread-spawning contexts).
struct Dispatcher {
    sys: Arc<dyn AudioSystem>,
    handle: EngineHandle,
    routing: RoutingHandle,
    store: ConfigStore,
    ui: Arc<Mutex<UiState>>,
    default_output: Option<EndpointId>,
    current: ConfigSnapshot,
    shared_ctx: Arc<Mutex<Option<eframe::egui::Context>>>,
}

enum Outcome {
    Continue,
    Quit,
}

impl Dispatcher {
    fn set_current(&mut self, snapshot: ConfigSnapshot) {
        self.ui.lock().unwrap().snapshot = snapshot.clone();
        self.current = snapshot;
    }

    fn focus_window(&self) {
        if let Some(ctx) = self.shared_ctx.lock().unwrap().as_ref() {
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    }

    /// External file edit (flow D) or this store's own echo (app-shell.md's
    /// echo-suppression decision — skip re-applying what the direct path
    /// below already applied).
    fn handle_watcher_snapshot(&mut self, new_snapshot: ConfigSnapshot) {
        if self.store.is_echo(&new_snapshot) {
            self.set_current(new_snapshot);
            return;
        }

        let delta = control::diff(&self.current, &new_snapshot);
        if delta.structural {
            if let Err(e) = self.handle.rebuild(&new_snapshot) {
                eprintln!("rebuild failed: {e:?}");
            }
            let buses = routing_buses(self.sys.as_ref(), &new_snapshot);
            self.routing
                .update_topology(buses, group_rules(&new_snapshot), self.default_output.clone());
        } else {
            if !delta.params.is_empty() {
                if let Err(e) = self.handle.apply_params(&delta.params) {
                    eprintln!("apply_params failed: {e:?}");
                }
            }
            if delta.rules.is_some() {
                self.routing.update_rules(group_rules(&new_snapshot));
            }
        }
        self.set_current(new_snapshot);
    }

    /// Flow C/H: structural edit — funnel-only, rebuild + routing update
    /// paired (app-shell.md's own flow definition).
    fn apply_structural(&mut self, edits: &[ConfigEdit]) {
        match self.store.apply(edits) {
            Ok(new_snapshot) => {
                if let Err(e) = self.handle.rebuild(&new_snapshot) {
                    eprintln!("rebuild failed: {e:?}");
                }
                let buses = routing_buses(self.sys.as_ref(), &new_snapshot);
                self.routing
                    .update_topology(buses, group_rules(&new_snapshot), self.default_output.clone());
                self.set_current(new_snapshot);
            }
            Err(e) => eprintln!("structural edit rejected: {e:?}"),
        }
    }

    /// Flow B/E: param fast path — immediate `MixerCommand` via
    /// `EngineHandle` using the *current* (pre-edit) snapshot's positional
    /// `GroupId`s, plus a debounced comment-preserving config write of the
    /// same value. `SetRules` isn't a mixer param; it re-runs routing's
    /// live-rule-change flow (D) instead.
    fn apply_params(&mut self, edits: &[ConfigEdit]) {
        let commands = edits_to_mixer_commands(edits, &self.current);
        if !commands.is_empty() {
            if let Err(e) = self.handle.apply_params(&commands) {
                eprintln!("apply_params failed: {e:?}");
            }
        }

        let rules_changed = edits.iter().any(|e| matches!(e, ConfigEdit::SetRules(..)));
        match self.store.apply(edits) {
            Ok(new_snapshot) => {
                if rules_changed {
                    self.routing.update_rules(group_rules(&new_snapshot));
                }
                self.set_current(new_snapshot);
            }
            Err(e) => eprintln!("param edit rejected: {e:?}"),
        }
    }

    fn handle_action(&mut self, action: ShellAction) -> Outcome {
        match action {
            ShellAction::EditParams(edits) => self.apply_params(&edits),
            ShellAction::EditStructure(edits) => self.apply_structural(&edits),
            ShellAction::ToggleMute => {
                let muted = !self.current.muted;
                self.apply_params(&[ConfigEdit::SetMuted(muted)]);
            }
            ShellAction::ShowSettings => self.focus_window(),
            ShellAction::Quit => return Outcome::Quit,
        }
        Outcome::Continue
    }
}

/// `ConfigEdit`s with a direct `MixerCommand` equivalent — the fast path
/// (flow B). Structural edits and `SetRules` have no mixer counterpart and
/// are skipped here (handled via `ConfigStore` + routing instead).
fn edits_to_mixer_commands(edits: &[ConfigEdit], current: &ConfigSnapshot) -> Vec<audio_core::MixerCommand> {
    use audio_core::MixerCommand;
    edits
        .iter()
        .filter_map(|edit| match edit {
            ConfigEdit::SetGroupGain(name, gain) => {
                control::group_id_for(current, name).map(|id| MixerCommand::SetGroupGain(id, *gain))
            }
            ConfigEdit::SetMaster(gain) => Some(MixerCommand::SetMaster(*gain)),
            ConfigEdit::SetMuted(muted) => Some(MixerCommand::SetMuted(*muted)),
            ConfigEdit::SetFollowMaster(name, follow) => {
                control::group_id_for(current, name).map(|id| MixerCommand::SetFollowMaster(id, *follow))
            }
            ConfigEdit::SetGroupOutput(..)
            | ConfigEdit::AddGroup(..)
            | ConfigEdit::RemoveGroup(..)
            | ConfigEdit::SetRules(..) => None,
        })
        .collect()
}

/// Everything the main thread needs to run `eframe::run_native` once startup
/// (on the dispatcher thread) has finished.
struct Handoff {
    ui_state: Arc<Mutex<UiState>>,
    routing_reader: engine::RoutingReader,
    actions_tx: mpsc::Sender<ShellAction>,
}

fn main() {
    let outcome = InstanceGuard::acquire(lifecycle::APP_ID).unwrap_or_else(|e| {
        eprintln!("instance check failed: {e:?}");
        std::process::exit(1);
    });
    let (_guard, surface_rx) = match outcome {
        InstanceOutcome::Secondary => {
            println!("Splitstream is already running — surfaced the existing window.");
            return;
        }
        InstanceOutcome::Primary(guard, rx) => (guard, rx),
    };

    let should_quit = Arc::new(AtomicBool::new(false));
    {
        let should_quit = Arc::clone(&should_quit);
        ctrlc::set_handler(move || should_quit.store(true, Ordering::Relaxed))
            .expect("failed to install Ctrl+C handler");
    }
    let shared_ctx: Arc<Mutex<Option<eframe::egui::Context>>> = Arc::new(Mutex::new(None));

    // Startup (config load, engine/routing start, tray/hotkeys) and the
    // dispatcher loop both run here, on a background thread — never on the
    // main thread. `winit`/eframe needs `OleInitialize` (STA) on whichever
    // thread runs `eframe::run_native`; win-audio's WASAPI/COM calls
    // initialize that thread MTA. The two conflict if they ever share a
    // thread — caught via a real smoke run (`OleInitialize failed:
    // RPC_E_CHANGED_MODE`), not by any mock. Main thread stays COM-untouched
    // and reserved for eframe.
    let (handoff_tx, handoff_rx) = mpsc::channel::<Handoff>();
    let dispatcher_quit = Arc::clone(&should_quit);
    let dispatcher_shared_ctx = Arc::clone(&shared_ctx);
    let dispatcher_thread = thread::spawn(move || {
        run_startup_and_dispatch(handoff_tx, surface_rx, dispatcher_quit, dispatcher_shared_ctx);
    });

    let Ok(handoff) = handoff_rx.recv() else {
        // Startup failed before handoff — the dispatcher thread already
        // printed why and is exiting the process on its own.
        let _ = dispatcher_thread.join();
        return;
    };

    // eframe owns the main thread (required on Windows/macOS); the window
    // closing sets `should_quit` so the dispatcher thread runs the real
    // shutdown sequence and exits the process.
    let app = ui::SettingsApp::new(handoff.ui_state, handoff.routing_reader, handoff.actions_tx);
    let _ = eframe::run_native(
        "Splitstream",
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            *shared_ctx.lock().unwrap() = Some(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    );
    should_quit.store(true, Ordering::Relaxed);
    let _ = dispatcher_thread.join();
}

fn run_startup_and_dispatch(
    handoff_tx: mpsc::Sender<Handoff>,
    surface_rx: std::sync::mpsc::Receiver<lifecycle::SurfaceSignal>,
    should_quit: Arc<AtomicBool>,
    shared_ctx: Arc<Mutex<Option<eframe::egui::Context>>>,
) {
    let path = config_path();
    let snapshot = control::load(&path).unwrap_or_else(|e| {
        eprintln!("failed to load {}: {e:?}", path.display());
        std::process::exit(1);
    });

    if let Err(e) = lifecycle::set_autostart(snapshot.app.autostart) {
        eprintln!("autostart registration failed (non-fatal): {e:?}");
    }

    let sys: Arc<dyn AudioSystem> = Arc::new(WasapiSystem::new(bus_name_prefix()));
    let default_output = sys.default_output().ok().map(|ep| ep.id);

    let mut handle = engine::start(&snapshot, Arc::clone(&sys)).unwrap_or_else(|e| {
        eprintln!("failed to start engine: {e:?}");
        std::process::exit(1);
    });

    let (watcher, config_rx) = control::ConfigWatcher::spawn(&path).unwrap_or_else(|e| {
        eprintln!("failed to start config watcher: {e:?}");
        std::process::exit(1);
    });

    let store = ConfigStore::open(&path).unwrap_or_else(|e| {
        eprintln!("failed to open config store: {e:?}");
        std::process::exit(1);
    });

    // Unified engine-event stream: the engine's own `take_events` receiver
    // (single-consume — relayed onward here) plus routing's own notices,
    // both feeding the one `EventPump` app-shell.md specifies.
    let (unified_tx, unified_rx) = mpsc::channel::<EngineEvent>();
    {
        let engine_events = handle.take_events();
        let unified_tx = unified_tx.clone();
        thread::spawn(move || {
            for evt in engine_events {
                if unified_tx.send(evt).is_err() {
                    return;
                }
            }
        });
    }

    let sessions: Box<dyn SessionPort> = Box::new(win_audio::WasapiSessions::new(bus_name_prefix()));
    let policy: Box<dyn PolicyPort> = Box::new(win_audio::PolicyRouter::new());
    let routing = engine::start_routing(
        group_rules(&snapshot),
        routing_buses(sys.as_ref(), &snapshot),
        default_output.clone(),
        sessions,
        policy,
        unified_tx,
    )
    .unwrap_or_else(|e| {
        eprintln!("failed to start session routing: {e:?}");
        std::process::exit(1);
    });

    let routing_reader = routing.reader();
    let ui_state = Arc::new(Mutex::new(UiState {
        snapshot: snapshot.clone(),
        routes: routing.current_routes(),
        stats: handle.stats(),
        routing_degraded: routing.is_degraded(),
    }));

    let (actions_tx, actions_rx) = mpsc::channel::<ShellAction>();
    let (tray_events_tx, tray_events_rx) = mpsc::channel::<EngineEvent>();
    let pump = EventPump::spawn(unified_rx, tray_events_tx, Arc::clone(&ui_state));
    let tray_handle = tray::spawn_tray(actions_tx.clone(), tray_events_rx);
    let hotkey_handle = hotkeys::spawn_hotkeys(&snapshot.app.hotkeys, actions_tx.clone()).unwrap_or_else(|e| {
        eprintln!("hotkey registration failed (non-fatal): {e:?}");
        hotkeys::HotkeyHandle::idle()
    });

    // Second launch (flow G): surface the existing window instead of a
    // second `ShowSettings` round-trip through `actions_tx`, so it works
    // even if the dispatcher loop below is momentarily busy.
    {
        let actions_tx = actions_tx.clone();
        thread::spawn(move || {
            for _ in surface_rx {
                let _ = actions_tx.send(ShellAction::ShowSettings);
            }
        });
    }

    if handoff_tx
        .send(Handoff {
            ui_state: Arc::clone(&ui_state),
            routing_reader,
            actions_tx: actions_tx.clone(),
        })
        .is_err()
    {
        return; // main thread gone already
    }

    let mut dispatcher = Dispatcher {
        sys,
        handle,
        routing,
        store,
        ui: ui_state,
        default_output,
        current: snapshot,
        shared_ctx,
    };
    while !should_quit.load(Ordering::Relaxed) {
        match config_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(new_snapshot) => dispatcher.handle_watcher_snapshot(new_snapshot),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(action) = actions_rx.try_recv() {
            if let Outcome::Quit = dispatcher.handle_action(action) {
                should_quit.store(true, Ordering::Relaxed);
            }
        }
        dispatcher.ui.lock().unwrap().stats = dispatcher.handle.stats();
    }

    drop(watcher);
    tray_handle.shutdown();
    hotkey_handle.shutdown();
    dispatcher.routing.shutdown();
    pump.shutdown();
    if let Err(e) = dispatcher.handle.shutdown() {
        eprintln!("engine shutdown error: {e:?}");
    }
    println!("Splitstream stopped.");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core::{Gain, GroupId, MixerCommand};
    use engine::GroupConfig;

    fn snapshot_with_group(name: &str) -> ConfigSnapshot {
        ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            groups: vec![GroupConfig {
                name: name.into(),
                bus_endpoint: "Bus".into(),
                output_device: "Out".into(),
                gain: Gain::UNITY,
                follow_master: true,
                match_rules: vec![],
            }],
            app: engine::AppConfig::default(),
        }
    }

    #[test]
    fn maps_group_gain_edit_to_a_mixer_command_using_the_positional_group_id() {
        let snapshot = snapshot_with_group("Game");
        let edits = vec![ConfigEdit::SetGroupGain("Game".into(), Gain::new(0.5).unwrap())];

        let commands = edits_to_mixer_commands(&edits, &snapshot);

        assert_eq!(commands.len(), 1);
        assert!(matches!(
            commands[0],
            MixerCommand::SetGroupGain(GroupId(0), g) if g == Gain::new(0.5).unwrap()
        ));
    }

    #[test]
    fn unknown_group_name_is_dropped_not_panicking() {
        let snapshot = snapshot_with_group("Game");
        let edits = vec![ConfigEdit::SetGroupGain(
            "Nonexistent".into(),
            Gain::new(0.5).unwrap(),
        )];

        assert!(edits_to_mixer_commands(&edits, &snapshot).is_empty());
    }

    #[test]
    fn structural_and_rules_edits_have_no_mixer_command_equivalent() {
        let snapshot = snapshot_with_group("Game");
        let edits = vec![
            ConfigEdit::SetRules("Game".into(), vec!["game.exe".into()]),
            ConfigEdit::RemoveGroup("Game".into()),
        ];

        assert!(edits_to_mixer_commands(&edits, &snapshot).is_empty());
    }

    #[test]
    fn master_and_mute_edits_need_no_group_lookup() {
        let snapshot = snapshot_with_group("Game");
        let edits = vec![ConfigEdit::SetMaster(Gain::new(0.7).unwrap()), ConfigEdit::SetMuted(true)];

        let commands = edits_to_mixer_commands(&edits, &snapshot);

        assert_eq!(commands.len(), 2);
        assert!(matches!(commands[0], MixerCommand::SetMaster(g) if g == Gain::new(0.7).unwrap()));
        assert!(matches!(commands[1], MixerCommand::SetMuted(true)));
    }
}
