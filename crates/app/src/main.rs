//! Splitstream shell binary: single-instance guard, autostart, engine +
//! session-routing startup, config hot-reload, tray + hotkeys + settings
//! window. See `.lattice/context/app-shell.md` (P4).
//!
//! `windows_subsystem = "windows"` (release only — dev builds keep the
//! console) voids `eprintln!`/stderr and any panic message once a
//! double-clicked/logon launch has no console attached; `logging::init`
//! (simple-launch.md) is the replacement diagnostic surface and must ship in
//! the same change as this attribute (operational-learnings 2026-07-20).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod event_pump;
mod hotkeys;
mod lifecycle;
mod logging;
mod paths;
mod tray;
mod ui;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use control::{group_rules, ConfigEdit, ConfigStore};
use engine::ports::{AudioSystem, SessionPort};
use engine::{CaptureControl, ConfigSnapshot, EngineEvent, EngineHandle, RoutingHandle};
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
    /// `AddDspStage`/`RemoveDspStage` edits only — RT-safe chain swap
    /// (dsp-pipeline.md), not `EditParams`' plain fast path (the replacement
    /// `DspChain` needs the group's *post-edit* stage list, so the store
    /// write has to happen before the engine call, the opposite order from
    /// `EditParams`) and not `EditStructure`'s full rebuild.
    EditDspChains(Vec<ConfigEdit>),
    /// `SetSpatial` edits only — write to `ConfigStore` first (same order as
    /// `EditDspChains`: the replacement `Render` needs the group's *post-edit*
    /// output routing), then `EngineHandle::apply_spatial`'s off-RT
    /// build-and-swap path (spatial-audio.md). Not `EditParams`' fast path:
    /// building a `Render` needs the group's current topology, not just a
    /// scalar value.
    EditSpatial(Vec<ConfigEdit>),
    ToggleMute,
    ShowSettings,
    Quit,
}

/// True when the config has no groups at all — the onboarding gate
/// (process-loopback-capture pivot: a group only needs `output_device` to
/// resolve, a normal resolve-time concern, not an onboarding one; there's no
/// more virtual-bus classification step to gate on).
fn needs_onboarding(snapshot: &ConfigSnapshot) -> bool {
    snapshot.groups.is_empty()
}

/// Bundles everything a `ShellAction`/watcher-delivered snapshot needs to
/// react — extracted once the handler functions' parameter lists crossed the
/// too-many-arguments threshold (same idiom already used for
/// `engine::runtime`'s thread-spawning contexts).
struct Dispatcher {
    handle: EngineHandle,
    routing: RoutingHandle,
    store: ConfigStore,
    ui: Arc<Mutex<UiState>>,
    current: ConfigSnapshot,
    shared_ctx: Arc<Mutex<Option<eframe::egui::Context>>>,
}

enum Outcome {
    Continue,
    Quit,
}

impl Dispatcher {
    /// Reconciles `[app] autostart` on every snapshot change (simple-launch.md
    /// L4) — not just at startup — so onboarding, a hand-edit of the config
    /// file, and any future write path all keep the HKCU Run key in sync
    /// with the one source of truth. Also clears `UiState.first_run` once at
    /// least one group exists (only re-checked while still in first-run, to
    /// avoid recomputing on every routine param edit once onboarding is done).
    fn set_current(&mut self, snapshot: ConfigSnapshot) {
        if snapshot.app.autostart != self.current.app.autostart {
            if let Err(e) = lifecycle::set_autostart(snapshot.app.autostart) {
                eprintln!("autostart registration failed (non-fatal): {e:?}");
            }
        }

        let mut ui = self.ui.lock().unwrap();
        ui.snapshot = snapshot.clone();
        if ui.first_run {
            ui.first_run = needs_onboarding(&snapshot);
        }
        drop(ui);

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
        let is_echo = self.store.is_echo(&new_snapshot);
        tracing::info!(is_echo, "main: watcher snapshot arrived");
        if is_echo {
            self.set_current(new_snapshot);
            return;
        }

        let delta = control::diff(&self.current, &new_snapshot);
        tracing::info!(structural = delta.structural, "main: watcher snapshot diff");
        if delta.structural {
            if let Err(e) = self.handle.rebuild(&new_snapshot) {
                eprintln!("rebuild failed: {e:?}");
            }
            self.routing.update_rules(group_rules(&new_snapshot));
        } else {
            if !delta.params.is_empty() {
                if let Err(e) = self.handle.apply_params(delta.params) {
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
        tracing::info!("main: apply_structural (direct edit)");
        match self.store.apply(edits) {
            Ok(new_snapshot) => {
                if let Err(e) = self.handle.rebuild(&new_snapshot) {
                    eprintln!("rebuild failed: {e:?}");
                }
                self.routing.update_rules(group_rules(&new_snapshot));
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
            if let Err(e) = self.handle.apply_params(commands) {
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

    /// Add/remove-stage edits: write to `ConfigStore` first (so the group's
    /// dsp list reflects the edit), then hand the *whole new list* per
    /// affected group to `EngineHandle::apply_dsp_chains`, which builds the
    /// replacement `DspChain` off-RT and swaps it in.
    fn apply_dsp_chain_edits(&mut self, edits: &[ConfigEdit]) {
        let affected: Vec<&str> = edits
            .iter()
            .filter_map(|e| match e {
                ConfigEdit::AddDspStage(name, _) | ConfigEdit::RemoveDspStage(name, _) => {
                    Some(name.as_str())
                }
                _ => None,
            })
            .collect();

        match self.store.apply(edits) {
            Ok(new_snapshot) => {
                let chains: Vec<(audio_core::GroupId, Vec<audio_core::DspSpec>)> = affected
                    .iter()
                    .filter_map(|name| {
                        let id = control::group_id_for(&new_snapshot, name)?;
                        let dsp = new_snapshot
                            .groups
                            .iter()
                            .find(|g| g.name == *name)?
                            .dsp
                            .iter()
                            .map(|s| s.spec.clone())
                            .collect();
                        Some((id, dsp))
                    })
                    .collect();
                if !chains.is_empty() {
                    if let Err(e) = self.handle.apply_dsp_chains(chains) {
                        eprintln!("apply_dsp_chains failed: {e:?}");
                    }
                }
                self.set_current(new_snapshot);
            }
            Err(e) => eprintln!("dsp chain edit rejected: {e:?}"),
        }
    }

    /// `SetSpatial` edits: write to `ConfigStore` first (so the group's
    /// `spatial` flag and current `output_device` are both post-edit), then
    /// hand the resolved `(GroupId, bool)` pairs to
    /// `EngineHandle::apply_spatial`, which builds the replacement `Render`
    /// off-RT and swaps it in.
    fn apply_spatial_edits(&mut self, edits: &[ConfigEdit]) {
        match self.store.apply(edits) {
            Ok(new_snapshot) => {
                let changes: Vec<(audio_core::GroupId, bool)> = edits
                    .iter()
                    .filter_map(|e| match e {
                        ConfigEdit::SetSpatial(name, on) => {
                            control::group_id_for(&new_snapshot, name).map(|id| (id, *on))
                        }
                        _ => None,
                    })
                    .collect();
                if !changes.is_empty() {
                    if let Err(e) = self.handle.apply_spatial(&changes) {
                        eprintln!("apply_spatial failed: {e:?}");
                    }
                }
                self.set_current(new_snapshot);
            }
            Err(e) => eprintln!("spatial edit rejected: {e:?}"),
        }
    }

    fn handle_action(&mut self, action: ShellAction) -> Outcome {
        match action {
            ShellAction::EditParams(edits) => self.apply_params(&edits),
            ShellAction::EditStructure(edits) => self.apply_structural(&edits),
            ShellAction::EditDspChains(edits) => self.apply_dsp_chain_edits(&edits),
            ShellAction::EditSpatial(edits) => self.apply_spatial_edits(&edits),
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
            ConfigEdit::SetEqBand(name, band, spec) => {
                let group = current.groups.iter().find(|g| &g.name == name)?;
                let id = control::group_id_for(current, name)?;
                let stage = group
                    .dsp
                    .iter()
                    .position(|s| matches!(s.spec, audio_core::DspSpec::Eq { .. }))?;
                Some(MixerCommand::SetDspParam {
                    group: id,
                    stage,
                    param: audio_core::DspParam::EqBand { band: *band, spec: *spec },
                })
            }
            ConfigEdit::SetLimiterCeiling(name, ceiling_db) => {
                let group = current.groups.iter().find(|g| &g.name == name)?;
                let id = control::group_id_for(current, name)?;
                let stage = group
                    .dsp
                    .iter()
                    .position(|s| matches!(s.spec, audio_core::DspSpec::Limiter { .. }))?;
                Some(MixerCommand::SetDspParam {
                    group: id,
                    stage,
                    param: audio_core::DspParam::LimiterCeilingDb(*ceiling_db),
                })
            }
            ConfigEdit::SetDspBypass(name, stage, bypassed) => control::group_id_for(current, name)
                .map(|id| MixerCommand::SetDspBypass {
                    group: id,
                    stage: *stage,
                    bypassed: *bypassed,
                }),
            ConfigEdit::SetDuck(name, duck) => {
                let id = control::group_id_for(current, name)?;
                let resolved = duck.as_ref().and_then(|d| {
                    control::group_id_for(current, &d.trigger).map(|trigger| audio_core::DuckSpec {
                        trigger,
                        amount_db: d.amount_db,
                        threshold_db: d.threshold_db,
                        attack_ms: d.attack_ms,
                        release_ms: d.release_ms,
                    })
                });
                Some(MixerCommand::SetDuck { group: id, duck: resolved })
            }
            ConfigEdit::SetGroupOutput(..)
            | ConfigEdit::AddGroup(..)
            | ConfigEdit::RemoveGroup(..)
            | ConfigEdit::RemoveDspStage(..)
            | ConfigEdit::AddDspStage(..)
            | ConfigEdit::SetRules(..)
            | ConfigEdit::SetSpatial(..)
            | ConfigEdit::SetAutostart(..) => None,
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
    // Checked first, before the instance guard / eframe / COM touch
    // anything: the Inno uninstaller runs this as the end user
    // (`runasoriginaluser`) purely to deregister the HKCU autostart entry,
    // then exits (simple-launch.md Flow 6).
    if std::env::args().any(|a| a == "--uninstall-cleanup") {
        let _ = lifecycle::set_autostart(false);
        std::process::exit(0);
    }

    // Held for the process lifetime — dropping it stops the log writer
    // thread. Init'd on the main thread, before anything COM-touching, so
    // even an instance-guard failure lands in the log/dialog surface instead
    // of a console nobody sees under the GUI subsystem.
    let _log_guard = logging::init(&paths::log_dir());

    let outcome = InstanceGuard::acquire(lifecycle::APP_ID).unwrap_or_else(|e| {
        logging::fatal_dialog("instance check failed", &format!("{e:?}"));
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
    let path = paths::config_path();
    let snapshot = control::ensure_config(&path).unwrap_or_else(|e| {
        logging::fatal_dialog("failed to load config", &format!("{}: {e:?}", path.display()));
        std::process::exit(1);
    });

    if let Err(e) = lifecycle::set_autostart(snapshot.app.autostart) {
        eprintln!("autostart registration failed (non-fatal): {e:?}");
    }

    let sys: Arc<dyn AudioSystem> = Arc::new(WasapiSystem::new());
    let default_output_endpoint = sys.default_output().ok();

    let mut handle = engine::start(&snapshot, Arc::clone(&sys)).unwrap_or_else(|e| {
        logging::fatal_dialog("failed to start engine", &format!("{e:?}"));
        std::process::exit(1);
    });

    let (watcher, config_rx) = control::ConfigWatcher::spawn(&path).unwrap_or_else(|e| {
        logging::fatal_dialog("failed to start config watcher", &format!("{e:?}"));
        std::process::exit(1);
    });

    let store = ConfigStore::open(&path).unwrap_or_else(|e| {
        logging::fatal_dialog("failed to open config store", &format!("{e:?}"));
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

    let sessions: Box<dyn SessionPort> = Box::new(win_audio::WasapiSessions::new());
    let capture_control: CaptureControl = handle.capture_control();
    let routing = engine::start_routing(
        group_rules(&snapshot),
        std::process::id(),
        sessions,
        capture_control,
        unified_tx,
    )
    .unwrap_or_else(|e| {
        logging::fatal_dialog("failed to start session routing", &format!("{e:?}"));
        std::process::exit(1);
    });

    let endpoints = sys.enumerate().unwrap_or_default();
    let first_run = needs_onboarding(&snapshot);

    let routing_reader = routing.reader();
    let ui_state = Arc::new(Mutex::new(UiState {
        snapshot: snapshot.clone(),
        routes: routing.current_routes(),
        stats: handle.stats(),
        routing_degraded: routing.is_degraded(),
        first_run,
        available_devices: endpoints,
        default_output_name: default_output_endpoint.map(|ep| ep.name),
        all_sessions: routing.all_sessions(),
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
        handle,
        routing,
        store,
        ui: ui_state,
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
    // Must run before `pump.shutdown()`: the pump thread only exits once
    // every sender to `unified_rx` drops, and one of those senders lives
    // inside `EngineHandle`'s internal `Arc<Persistent>` — it doesn't drop
    // until this call completes. Calling `pump.shutdown()` first deadlocks
    // it forever (found live 2026-07-21: a quit that never actually exits
    // leaves the single-instance mutex held, so every relaunch silently
    // no-ops as "already running").
    if let Err(e) = dispatcher.handle.shutdown() {
        eprintln!("engine shutdown error: {e:?}");
    }
    pump.shutdown();
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
                output_device: "Out".into(),
                gain: Gain::UNITY,
                follow_master: true,
                match_rules: vec![],
                dsp: Vec::new(),
                duck: None,
                spatial: false,
            }],
            app: engine::AppConfig::default(),
        }
    }

    #[test]
    fn needs_onboarding_is_true_when_there_are_no_groups() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            groups: vec![],
            app: engine::AppConfig::default(),
        };
        assert!(needs_onboarding(&snapshot));
    }

    #[test]
    fn needs_onboarding_is_false_when_a_group_exists() {
        let snapshot = snapshot_with_group("Game");
        assert!(!needs_onboarding(&snapshot));
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
    fn spatial_edits_have_no_mixer_command_equivalent() {
        // SetSpatial funnels through EngineHandle::apply_spatial (needs the
        // group's current topology to build a Render), not a plain
        // MixerCommand — same reason AddDspStage/RemoveDspStage aren't here.
        let snapshot = snapshot_with_group("Game");
        let edits = vec![ConfigEdit::SetSpatial("Game".into(), true)];

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
