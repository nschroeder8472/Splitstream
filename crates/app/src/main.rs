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
mod icons;
mod lifecycle;
mod logging;
mod paths;
mod theme;
mod tray;
mod ui;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

use control::{group_rules, ConfigEdit, ConfigStore};
use engine::ports::{AudioSystem, SessionPort, VolumeEvent};
use engine::{
    start_volume_bind, AccentChoice, CaptureControl, ConfigSnapshot, EngineEvent, EngineHandle, MirrorAction,
    RoutingHandle, VolumeBindHandle,
};
use win_audio::WasapiSystem;

use event_pump::{EventPump, UiState};
use lifecycle::{InstanceGuard, InstanceOutcome};

/// Which gain a volume hotkey or the endpoint binding acts on
/// (external-controls.md). `PartialEq` so a `Dispatcher` can compare it
/// against the configured `[app] volume_bind` target.
#[derive(Debug, Clone, PartialEq)]
pub enum VolumeTarget {
    Master,
    Group(String),
}

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
    /// Session-only per-group solo (per-group-mute-solo.md). Deliberately NOT
    /// an `EditParams` variant: `apply_params` always follows the mixer
    /// command with `store.apply`, and solo must never reach TOML.
    SetSolo(String, bool),
    /// Switch to (or revert to) a named profile — same flow from tray,
    /// hotkey or window (profiles.md L3 flow A/D). Not an `EditParams`/
    /// `EditStructure`/etc. variant: the edit batch a profile switch
    /// produces is mixed, so `Dispatcher::apply_profile_action` partitions
    /// it by `edit_path` itself rather than the call site pre-choosing one
    /// of the four fixed paths.
    ApplyProfile(String),
    /// An inbound endpoint volume change (external-controls.md flow B) —
    /// reconciled against whichever target `[app] volume_bind` currently
    /// names, if any.
    EndpointVolumeChanged(VolumeEvent),
    VolumeStep { target: VolumeTarget, delta_db: f32 },
    ToggleGroupMute(String),
    /// `true` = pressed, `false` = released or max-hold expired (capability 15).
    PushToMute(bool),
    /// The OS default playback device changed (flow E) — internal plumbing,
    /// forwarded from `EngineEvent::DefaultDeviceChanged`, never sent by
    /// tray/hotkeys/UI. Carries nothing: the dispatcher re-queries the
    /// current default itself rather than trust a possibly-stale payload.
    DefaultDeviceChanged,
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

const VOLUME_STEP_DB: f32 = 3.0;
const PUSH_TO_MUTE_MAX_HOLD: Duration = Duration::from_secs(30);
/// Windows tray icons render at 16x16 (visual-identity.md decision 10).
const TRAY_ICON_SIZE: u32 = 16;

/// Clamped 3 dB stepping (decision 6), shared by every volume hotkey.
/// Reuses the fader's own dB<->`Gain` mapping and range (`ui::FADER_MIN_DB`/
/// `ui::FADER_MAX_DB`) so a hotkey step can never produce a value the fader
/// UI would then display differently.
fn step_gain(gain: audio_core::Gain, delta_db: f32) -> audio_core::Gain {
    let db = (ui::gain_to_fader_db(gain) + delta_db).clamp(ui::FADER_MIN_DB, ui::FADER_MAX_DB);
    ui::fader_db_to_gain(db)
}

/// Endpoint position (0.0..=1.0) <-> `Gain`, mapped 1:1 onto fader travel
/// (decision 14: "the Windows slider and the Splitstream fader sit at the
/// same place") — linear across the *whole* fader range including the boost
/// region above unity, not linear in raw gain. Shares the fader's own dB
/// mapping so a bound target's on-screen fader position always agrees with
/// the reported endpoint position.
fn position_to_gain(position: f32) -> audio_core::Gain {
    let span = ui::FADER_MAX_DB - ui::FADER_MIN_DB;
    ui::fader_db_to_gain(ui::FADER_MIN_DB + position.clamp(0.0, 1.0) * span)
}

fn gain_to_position(gain: audio_core::Gain) -> f32 {
    let span = ui::FADER_MAX_DB - ui::FADER_MIN_DB;
    ((ui::gain_to_fader_db(gain) - ui::FADER_MIN_DB) / span).clamp(0.0, 1.0)
}

/// A key-press/release/expiry the push-to-mute state machine reacts to.
/// `Pressed` carries the *actual* current mute state, not a remembered one
/// (decision 15: a second press while already held re-arms from reality,
/// so a missed `Released` self-heals instead of propagating).
enum HoldEvent {
    Pressed { actual_muted: bool },
    Released,
    Expired,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct HoldState {
    held: bool,
    restore_to: bool,
}

/// Pure push-to-mute state machine (capabilities 14/15), so both are
/// testable without threads or a clock. `Some(muted)` in the return value
/// means "apply this mute state now".
fn push_to_mute(state: HoldState, event: HoldEvent) -> (HoldState, Option<bool>) {
    match event {
        HoldEvent::Pressed { actual_muted } => {
            (HoldState { held: true, restore_to: actual_muted }, Some(true))
        }
        HoldEvent::Released | HoldEvent::Expired => {
            if state.held {
                (HoldState::default(), Some(state.restore_to))
            } else {
                (state, None) // stray release/expiry with nothing held
            }
        }
    }
}

/// `(gain, muted)` for whichever target a volume action names — `None` when
/// the target is a group that no longer exists.
fn target_state(snapshot: &ConfigSnapshot, target: &VolumeTarget) -> Option<(audio_core::Gain, bool)> {
    match target {
        VolumeTarget::Master => Some((snapshot.master, snapshot.muted)),
        VolumeTarget::Group(name) => snapshot.groups.iter().find(|g| &g.name == name).map(|g| (g.gain, g.muted)),
    }
}

/// Every hotkey chord a snapshot defines, collapsed to one binding list
/// (external-controls.md decision 16) — `[hotkeys]`'s four global chords,
/// each profile's optional chord, and each group's three optional chords.
/// Hotkeys are only ever read once at startup (existing limitation, unchanged
/// by this feature): a hand-edit or live add of a chord takes effect on the
/// next launch, not immediately.
fn build_hotkey_bindings(snapshot: &ConfigSnapshot) -> Vec<hotkeys::HotkeyBinding> {
    let mut bindings = Vec::new();
    let hk = &snapshot.app.hotkeys;
    if let Some(chord) = hk.mute_master {
        bindings.push(hotkeys::HotkeyBinding { chord, action: hotkeys::HotkeyAction::ToggleMasterMute });
    }
    if let Some(chord) = hk.push_to_mute {
        bindings.push(hotkeys::HotkeyBinding { chord, action: hotkeys::HotkeyAction::PushToMuteMaster });
    }
    if let Some(chord) = hk.master_volume_up {
        bindings.push(hotkeys::HotkeyBinding {
            chord,
            action: hotkeys::HotkeyAction::VolumeUp(VolumeTarget::Master),
        });
    }
    if let Some(chord) = hk.master_volume_down {
        bindings.push(hotkeys::HotkeyBinding {
            chord,
            action: hotkeys::HotkeyAction::VolumeDown(VolumeTarget::Master),
        });
    }
    for profile in &snapshot.profiles {
        if let Some(chord) = profile.hotkey {
            bindings.push(hotkeys::HotkeyBinding {
                chord,
                action: hotkeys::HotkeyAction::ApplyProfile(profile.name.clone()),
            });
        }
    }
    for group in &snapshot.groups {
        if let Some(chord) = group.hotkey_mute {
            bindings.push(hotkeys::HotkeyBinding {
                chord,
                action: hotkeys::HotkeyAction::ToggleGroupMute(group.name.clone()),
            });
        }
        if let Some(chord) = group.hotkey_volume_up {
            bindings.push(hotkeys::HotkeyBinding {
                chord,
                action: hotkeys::HotkeyAction::VolumeUp(VolumeTarget::Group(group.name.clone())),
            });
        }
        if let Some(chord) = group.hotkey_volume_down {
            bindings.push(hotkeys::HotkeyBinding {
                chord,
                action: hotkeys::HotkeyAction::VolumeDown(VolumeTarget::Group(group.name.clone())),
            });
        }
    }
    bindings
}

/// The tray's current view of live state (external-controls.md capability 9)
/// — rebuilt fresh from a snapshot on every dispatcher-observed change (see
/// `Dispatcher::set_current`), never diffed incrementally.
fn build_tray_model(snapshot: &ConfigSnapshot) -> tray::TrayModel {
    tray::TrayModel {
        groups: snapshot
            .groups
            .iter()
            .map(|g| tray::TrayGroup { name: g.name.clone(), muted: g.muted })
            .collect(),
        profiles: snapshot.profiles.iter().map(|p| p.name.clone()).collect(),
        active_profile: snapshot.app.active_profile.clone(),
        master_muted: snapshot.muted,
    }
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
    /// Bumped on every mixer rebuild (per-group-mute-solo.md decision 8) so
    /// the settings window can drop its session-only solo set. Published to
    /// `UiState` in `set_current`, never re-derived by the UI from a snapshot
    /// diff or `EngineHandle::epoch()` (that also bumps on DSP chain swaps).
    rebuild_generation: u64,
    /// Held only to re-query `default_output()` on a default-device change
    /// (flow E) and at startup — everything else audio-facing already goes
    /// through `handle`/`routing`/`volume_bind`.
    sys: Arc<dyn AudioSystem>,
    volume_bind: VolumeBindHandle,
    /// Rebuilt from every snapshot change in `set_current` (capability 9 —
    /// the tray must never show a stale group/profile/mute-state list).
    tray_handle: tray::TrayHandle,
    /// Friendly name of the current default playback device — refreshed at
    /// startup and on every `DefaultDeviceChanged`, compared against a bound
    /// group's `output_device` to compute the double-attenuation guard
    /// (decision 4).
    default_output_name: Option<String>,
    push_to_mute_state: HoldState,
    /// When the current hold was armed — `None` while not held. Checked
    /// every dispatcher tick against `PUSH_TO_MUTE_MAX_HOLD` (capability 15).
    push_to_mute_armed_at: Option<Instant>,
    /// `(accent, surface theme)` last pushed to the tray icon (visual-identity.md
    /// decision 9/Flow G) — `refresh_tray_icon` compares against this every
    /// dispatcher tick, so a native icon re-render only happens on an actual
    /// change, not on every 50ms poll. `None` before the first tick, so a
    /// startup render always happens even if the resolved value would
    /// otherwise equal a `Default`.
    last_tray_icon: Option<(AccentChoice, egui::Theme)>,
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
        ui.rebuild_generation = self.rebuild_generation;
        if ui.first_run {
            ui.first_run = needs_onboarding(&snapshot);
        }
        drop(ui);

        // Captured before the overwrite below, compared after: `set_current`
        // runs on every param edit, including every frame of a fader drag
        // (`fader()`'s `response.changed()` fires per-frame, not just on
        // release, same as the pre-existing per-frame config write). A
        // native tray-menu rebuild is real OS-level work, unlike that write
        // — done unconditionally, a drag would tear down and recreate the
        // whole menu dozens of times a second for a gain change the tray
        // doesn't even display. Skipped when nothing tray-relevant moved.
        let old_tray_model = build_tray_model(&self.current);

        self.current = snapshot;
        // Recomputed on every snapshot change, not just when `volume_bind`
        // itself changes: a param edit to a *bound group's* `output_device`,
        // a profile switch, or a hand-edit can each flip the guard (decision
        // 4 — "this can arise at any moment"). Cheap when nothing changed:
        // the coordinator only acts on an actual suspended-state transition.
        let suspended = self.compute_suspended();
        self.volume_bind.set_suspended(suspended);
        let new_tray_model = build_tray_model(&self.current);
        if new_tray_model != old_tray_model {
            self.tray_handle.rebuild(new_tray_model);
        }
    }

    /// `[app] volume_bind` resolved to a target, or `None` when unbound.
    fn bound_target(&self) -> Option<VolumeTarget> {
        match self.current.app.volume_bind.as_deref() {
            None => None,
            Some("master") => Some(VolumeTarget::Master),
            Some(name) => Some(VolumeTarget::Group(name.to_string())),
        }
    }

    /// Decision 4's double-attenuation guard: suspended when the bound
    /// group's own output device *is* the current default (master has no
    /// single output device to coincide with, so it is never suspended);
    /// also suspended when nothing is bound, or the bound group no longer
    /// exists — both cases where there is nothing to safely mirror.
    fn compute_suspended(&self) -> bool {
        match self.bound_target() {
            None => true,
            Some(VolumeTarget::Master) => false,
            Some(VolumeTarget::Group(name)) => {
                let Some(group) = self.current.groups.iter().find(|g| g.name == name) else {
                    return true;
                };
                Some(group.output_device.as_str()) == self.default_output_name.as_deref()
            }
        }
    }

    fn is_bound_to(&self, target: &VolumeTarget) -> bool {
        self.bound_target().as_ref() == Some(target)
    }

    /// Flow C: after any edit changing the bound target's gain, push it
    /// outward. A direct, unconditional-but-suspended-gated call — not
    /// decided by `reconcile`, which only ever resolves the inbound
    /// direction (see that function's doc for why).
    fn push_target_volume_if_bound(&self, target: &VolumeTarget, gain: audio_core::Gain) {
        if self.is_bound_to(target) && !self.compute_suspended() {
            self.volume_bind.push_level(gain_to_position(gain));
        }
    }

    fn push_target_mute_if_bound(&self, target: &VolumeTarget, muted: bool) {
        if self.is_bound_to(target) && !self.compute_suspended() {
            self.volume_bind.push_muted(muted);
        }
    }

    /// Flow C, centralized: scans an *applied* edit batch for anything that
    /// touched the bound target's gain or mute — however it got there (a UI
    /// fader drag, a hotkey step, `ToggleMute`, push-to-mute, or a profile
    /// switch's fast-path edits) — and pushes the new value outward exactly
    /// once. Called with `self.current` already the *post*-edit snapshot, so
    /// `edits` is only consulted for *which* fields changed, not their values.
    fn push_bound_target_changes(&self, edits: &[ConfigEdit]) {
        let Some(target) = self.bound_target() else { return };
        let touches = |name: &str| matches!(&target, VolumeTarget::Group(bound) if bound == name);
        let is_master = target == VolumeTarget::Master;

        let gain_changed = edits.iter().any(|e| match e {
            ConfigEdit::SetMaster(_) => is_master,
            ConfigEdit::SetGroupGain(name, _) => touches(name),
            _ => false,
        });
        let mute_changed = edits.iter().any(|e| match e {
            ConfigEdit::SetMuted(_) => is_master,
            ConfigEdit::SetGroupMute(name, _) => touches(name),
            _ => false,
        });

        let Some((gain, muted)) = target_state(&self.current, &target) else { return };
        if gain_changed {
            self.push_target_volume_if_bound(&target, gain);
        }
        if mute_changed {
            self.push_target_mute_if_bound(&target, muted);
        }
    }

    /// Flow B: an inbound endpoint change, reconciled against whichever
    /// target is currently bound.
    fn handle_endpoint_volume_changed(&mut self, event: VolumeEvent) {
        let Some(target) = self.bound_target() else { return };
        let Some((target_gain, target_muted)) = target_state(&self.current, &target) else { return };
        let suspended = self.compute_suspended();
        let target_level = gain_to_position(target_gain);

        if let Some(MirrorAction::AdoptFromEndpoint { level, muted }) =
            engine::volume_bind::reconcile(event, target_level, target_muted, suspended)
        {
            // `reconcile` always echoes back *both* the endpoint's level and
            // muted together, even when only one actually triggered the
            // mismatch (the other is just "still equal") — re-check each
            // field here so a plain volume-key press doesn't also emit a
            // same-value `SetMuted`/`SetGroupMute`, which `push_bound_target_changes`
            // would then read as "mute changed" and push straight back out to
            // the endpoint on every single key press.
            let level_changed = (level - target_level).abs() >= engine::MIRROR_EPSILON;
            let muted_changed = muted != target_muted;
            let mut edits = Vec::new();
            if level_changed {
                let gain = position_to_gain(level);
                edits.push(match &target {
                    VolumeTarget::Master => ConfigEdit::SetMaster(gain),
                    VolumeTarget::Group(name) => ConfigEdit::SetGroupGain(name.clone(), gain),
                });
            }
            if muted_changed {
                edits.push(match &target {
                    VolumeTarget::Master => ConfigEdit::SetMuted(muted),
                    VolumeTarget::Group(name) => ConfigEdit::SetGroupMute(name.clone(), muted),
                });
            }
            if !edits.is_empty() {
                self.apply_params(&edits);
            }
        }
    }

    fn handle_volume_step(&mut self, target: VolumeTarget, delta_db: f32) {
        let Some((gain, _)) = target_state(&self.current, &target) else { return };
        let new_gain = step_gain(gain, delta_db);
        let edit = match &target {
            VolumeTarget::Master => ConfigEdit::SetMaster(new_gain),
            VolumeTarget::Group(name) => ConfigEdit::SetGroupGain(name.clone(), new_gain),
        };
        self.apply_params(&[edit]);
    }

    fn handle_toggle_group_mute(&mut self, name: &str) {
        let Some(group) = self.current.groups.iter().find(|g| g.name == name) else { return };
        let muted = !group.muted;
        self.apply_params(&[ConfigEdit::SetGroupMute(name.to_string(), muted)]);
    }

    /// Flow G: push-to-mute. `pressed` maps onto the pure state machine's
    /// `Pressed`/`Released`; expiry is checked separately every tick (see
    /// `check_push_to_mute_expiry`), not here.
    fn handle_push_to_mute(&mut self, pressed: bool) {
        let event = if pressed {
            HoldEvent::Pressed { actual_muted: self.current.muted }
        } else {
            HoldEvent::Released
        };
        self.apply_push_to_mute_event(event);
    }

    /// Capability 15's safety net — a missed `Released` auto-restores after
    /// `PUSH_TO_MUTE_MAX_HOLD` rather than stranding the audio muted.
    fn check_push_to_mute_expiry(&mut self) {
        let expired = self
            .push_to_mute_armed_at
            .is_some_and(|armed_at| armed_at.elapsed() >= PUSH_TO_MUTE_MAX_HOLD);
        if expired {
            self.apply_push_to_mute_event(HoldEvent::Expired);
        }
    }

    /// visual-identity.md decision 9/Flow G: the tray mark follows the
    /// *system* theme (independent of the app's own theme preference, since
    /// the taskbar has its own), which only `shared_ctx` can answer — the
    /// tray thread has no window of its own to ask. Called every dispatcher
    /// tick (cheap: one lock plus a tuple comparison), but only renders and
    /// pushes a new icon when the resolved `(accent, theme)` actually
    /// changed, the same call-frequency-vs-cost check as `set_current`'s
    /// tray-rebuild guard. Falls back to `Theme::Dark` before the eframe
    /// window's first frame populates `shared_ctx` — matches egui's own
    /// `fallback_theme` default, corrected the moment the real value is known.
    fn refresh_tray_icon(&mut self) {
        let surface_theme = self
            .shared_ctx
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|ctx| ctx.system_theme())
            .unwrap_or(egui::Theme::Dark);
        let wanted = (self.current.app.accent, surface_theme);
        if self.last_tray_icon != Some(wanted) {
            let rgba = theme::brand_icon_rgba(TRAY_ICON_SIZE, theme::accent(wanted.0), wanted.1);
            self.tray_handle.set_icon(rgba, TRAY_ICON_SIZE);
            self.last_tray_icon = Some(wanted);
        }
    }

    fn apply_push_to_mute_event(&mut self, event: HoldEvent) {
        let (new_state, action) = push_to_mute(self.push_to_mute_state, event);
        self.push_to_mute_state = new_state;
        self.push_to_mute_armed_at = if new_state.held { Some(Instant::now()) } else { None };
        if let Some(muted) = action {
            self.apply_params(&[ConfigEdit::SetMuted(muted)]);
        }
    }

    /// Flow E: re-open against the new default device and recompute the
    /// guard — `set_current`'s own `compute_suspended`/`set_suspended` call
    /// only reacts to *snapshot* changes, so a default-device change (which
    /// touches neither `self.current` nor triggers `set_current`) needs its
    /// own explicit recompute here.
    fn handle_default_device_changed(&mut self) {
        self.default_output_name = self.sys.default_output().ok().map(|e| e.name);
        self.volume_bind.rebind();
        let suspended = self.compute_suspended();
        self.volume_bind.set_suspended(suspended);
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
            self.rebuild_generation += 1;
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
                self.rebuild_generation += 1;
                self.routing.update_rules(group_rules(&new_snapshot));
                self.set_current(new_snapshot);
            }
            Err(e) => eprintln!("structural edit rejected: {e:?}"),
        }
    }

    /// Flow B (solo): session-only, resolved against the *current* snapshot's
    /// positional `GroupId` like `apply_params`, but with no `store.apply`
    /// call at all — solo must never reach TOML (decision 1).
    fn apply_solo(&mut self, name: &str, on: bool) {
        if let Some(id) = control::group_id_for(&self.current, name) {
            if let Err(e) = self.handle.apply_params(vec![audio_core::MixerCommand::SetGroupSolo(id, on)]) {
                eprintln!("apply_params (solo) failed: {e:?}");
            }
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
                self.push_bound_target_changes(edits);
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
                ConfigEdit::AddDspStage(name, _)
                | ConfigEdit::RemoveDspStage(name, _)
                | ConfigEdit::SetEqBands(name, ..) => Some(name.as_str()),
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

    /// Flow A/D: applies (or reverts to) a named profile. Partitions the
    /// resulting batch by `edit_path` (profiles.md decision 12) rather than
    /// calling `apply_structural`/`apply_params`/etc. directly — each of
    /// those does its own `store.apply`, and a profile batch must write the
    /// store exactly once. A `Structural` edit (only `SetGroupOutput` can
    /// appear here — profiles never add/remove groups) always rebuilds,
    /// which subsumes any `DspChain`/`Spatial` edits in the same batch too
    /// (`rebuild` reads the whole new snapshot fresh); only when nothing in
    /// the batch is structural do the narrower off-RT swap paths run
    /// instead, so a gain-only profile never rebuilds.
    fn apply_profile_action(&mut self, name: &str) {
        let mut edits = control::profiles::apply_profile(&self.current, name);
        edits.push(ConfigEdit::SetActiveProfile(Some(name.to_string())));

        let structural = edits.iter().any(|e| control::edit_path(e) == control::EditPath::Structural);

        match self.store.apply(&edits) {
            Ok(new_snapshot) => {
                if structural {
                    if let Err(e) = self.handle.rebuild(&new_snapshot) {
                        eprintln!("rebuild failed: {e:?}");
                    }
                    self.rebuild_generation += 1;
                    self.routing.update_rules(group_rules(&new_snapshot));
                } else {
                    let commands = edits_to_mixer_commands(&edits, &self.current);
                    if !commands.is_empty() {
                        if let Err(e) = self.handle.apply_params(commands) {
                            eprintln!("apply_params failed: {e:?}");
                        }
                    }

                    let dsp_chains: Vec<(audio_core::GroupId, Vec<audio_core::DspSpec>)> = edits
                        .iter()
                        .filter_map(|e| match e {
                            ConfigEdit::SetDspChain(group_name, _) => {
                                let id = control::group_id_for(&new_snapshot, group_name)?;
                                let dsp = new_snapshot
                                    .groups
                                    .iter()
                                    .find(|g| &g.name == group_name)?
                                    .dsp
                                    .iter()
                                    .map(|s| s.spec.clone())
                                    .collect();
                                Some((id, dsp))
                            }
                            _ => None,
                        })
                        .collect();
                    if !dsp_chains.is_empty() {
                        if let Err(e) = self.handle.apply_dsp_chains(dsp_chains) {
                            eprintln!("apply_dsp_chains failed: {e:?}");
                        }
                    }

                    let spatial_changes: Vec<(audio_core::GroupId, bool)> = edits
                        .iter()
                        .filter_map(|e| match e {
                            ConfigEdit::SetSpatial(group_name, on) => {
                                control::group_id_for(&new_snapshot, group_name).map(|id| (id, *on))
                            }
                            _ => None,
                        })
                        .collect();
                    if !spatial_changes.is_empty() {
                        if let Err(e) = self.handle.apply_spatial(&spatial_changes) {
                            eprintln!("apply_spatial failed: {e:?}");
                        }
                    }
                }
                self.set_current(new_snapshot);
                self.push_bound_target_changes(&edits);
            }
            Err(e) => eprintln!("profile apply rejected: {e:?}"),
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
            ShellAction::SetSolo(name, on) => self.apply_solo(&name, on),
            ShellAction::ApplyProfile(name) => self.apply_profile_action(&name),
            ShellAction::EndpointVolumeChanged(event) => self.handle_endpoint_volume_changed(event),
            ShellAction::VolumeStep { target, delta_db } => self.handle_volume_step(target, delta_db),
            ShellAction::ToggleGroupMute(name) => self.handle_toggle_group_mute(&name),
            ShellAction::PushToMute(pressed) => self.handle_push_to_mute(pressed),
            ShellAction::DefaultDeviceChanged => self.handle_default_device_changed(),
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
            ConfigEdit::SetGroupMute(name, muted) => {
                control::group_id_for(current, name).map(|id| MixerCommand::SetGroupMute(id, *muted))
            }
            ConfigEdit::SetEqBand(name, stage, band, spec) => {
                let id = control::group_id_for(current, name)?;
                Some(MixerCommand::SetDspParam {
                    group: id,
                    stage: *stage,
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
            | ConfigEdit::SetEqBands(..)
            | ConfigEdit::SetRules(..)
            | ConfigEdit::SetSpatial(..)
            | ConfigEdit::SetAutostart(..)
            | ConfigEdit::SetDspChain(..)
            | ConfigEdit::SetProfile(..)
            | ConfigEdit::RemoveProfile(..)
            | ConfigEdit::SetActiveProfile(..)
            | ConfigEdit::SetTheme(..)
            | ConfigEdit::SetAccent(..) => None,
        })
        .collect()
}

/// Everything the main thread needs to run `eframe::run_native` once startup
/// (on the dispatcher thread) has finished.
struct Handoff {
    ui_state: Arc<Mutex<UiState>>,
    routing_reader: engine::RoutingReader,
    stats_reader: engine::StatsReader,
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

    // Read before `handoff.ui_state` moves into `SettingsApp::new` below —
    // visual-identity.md capability 11: applied inside the `CreationContext`
    // closure, before the first frame, so there's no flash of default egui
    // styling on launch. `ui::SettingsApp::ui`'s own Flow B compare-and-react
    // handles every change after this one.
    let (initial_theme, initial_accent) = {
        let state = handoff.ui_state.lock().unwrap();
        (state.snapshot.app.theme, state.snapshot.app.accent)
    };

    // eframe owns the main thread (required on Windows/macOS); the window
    // closing sets `should_quit` so the dispatcher thread runs the real
    // shutdown sequence and exits the process.
    let app = ui::SettingsApp::new(
        handoff.ui_state,
        handoff.routing_reader,
        handoff.stats_reader,
        handoff.actions_tx,
    );
    let _ = eframe::run_native(
        "Splitstream",
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx, initial_theme, initial_accent);
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
    let default_output_name = default_output_endpoint.as_ref().map(|e| e.name.clone());

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

    let volume_bind = start_volume_bind(Arc::clone(&sys));

    // Created here (earlier than every other `ShellAction` producer) so the
    // engine-event relay thread right below can forward
    // `DefaultDeviceChanged` onto it directly (external-controls.md flow E)
    // — `Dispatcher` is the only consumer with the knowledge (`[app]
    // volume_bind`) to act on it, and this channel is how tray/hotkeys/UI
    // already reach the dispatcher.
    let (actions_tx, actions_rx) = mpsc::channel::<ShellAction>();

    // Unified engine-event stream: the engine's own `take_events` receiver
    // (single-consume — relayed onward here) plus routing's own notices,
    // both feeding the one `EventPump` app-shell.md specifies.
    let (unified_tx, unified_rx) = mpsc::channel::<EngineEvent>();
    {
        let engine_events = handle.take_events();
        let unified_tx = unified_tx.clone();
        let actions_tx = actions_tx.clone();
        thread::spawn(move || {
            for evt in engine_events {
                if matches!(evt, EngineEvent::DefaultDeviceChanged(_)) {
                    let _ = actions_tx.send(ShellAction::DefaultDeviceChanged);
                }
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
        default_output_name: default_output_name.clone(),
        all_sessions: routing.all_sessions(),
        rebuild_generation: 0,
    }));

    let (tray_events_tx, tray_events_rx) = mpsc::channel::<EngineEvent>();
    let pump = EventPump::spawn(unified_rx, tray_events_tx, Arc::clone(&ui_state));
    // No `egui::Context` exists yet at this point in startup (eframe hasn't
    // run on the main thread), so the real system theme is unknowable —
    // `Theme::Dark` matches egui's own `fallback_theme` default and gets
    // corrected by the dispatcher's first `refresh_tray_icon` tick once
    // `shared_ctx` is populated (visual-identity.md decision 9).
    let initial_icon = theme::brand_icon_rgba(TRAY_ICON_SIZE, theme::accent(snapshot.app.accent), egui::Theme::Dark);
    let tray_handle = tray::spawn_tray(
        actions_tx.clone(),
        tray_events_rx,
        build_tray_model(&snapshot),
        initial_icon,
        TRAY_ICON_SIZE,
    );
    let hotkey_bindings = build_hotkey_bindings(&snapshot);
    let hotkey_handle = hotkeys::spawn_hotkeys(&hotkey_bindings, actions_tx.clone()).unwrap_or_else(|e| {
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
            stats_reader: handle.stats_reader(),
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
        rebuild_generation: 0,
        sys: Arc::clone(&sys),
        volume_bind,
        tray_handle,
        default_output_name,
        push_to_mute_state: HoldState::default(),
        push_to_mute_armed_at: None,
        last_tray_icon: None,
    };
    // Establishes the bind's initial suspended state (flow A's "Windows
    // wins" bind-time adopt fires here if `[app] volume_bind` already names
    // a target at startup) — every later transition is driven by
    // `set_current`/`handle_default_device_changed` instead.
    {
        let suspended = dispatcher.compute_suspended();
        dispatcher.volume_bind.set_suspended(suspended);
    }
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
        while let Ok(event) = dispatcher.volume_bind.events().try_recv() {
            dispatcher.handle_endpoint_volume_changed(event);
        }
        dispatcher.check_push_to_mute_expiry();
        dispatcher.refresh_tray_icon();
        dispatcher.ui.lock().unwrap().stats = dispatcher.handle.stats();
    }

    drop(watcher);
    dispatcher.tray_handle.shutdown();
    hotkey_handle.shutdown();
    dispatcher.volume_bind.shutdown();
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
    use audio_core::{EqBandSpec, Gain, GroupId, MixerCommand};
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
                muted: false,
                hotkey_mute: None,
                hotkey_volume_up: None,
                hotkey_volume_down: None,
            }],
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
        }
    }

    #[test]
    fn step_gain_clamps_at_the_top() {
        let near_top = ui::fader_db_to_gain(ui::FADER_MAX_DB - 1.0);
        let stepped = step_gain(near_top, VOLUME_STEP_DB);
        assert!((ui::gain_to_fader_db(stepped) - ui::FADER_MAX_DB).abs() < 1e-3);
    }

    #[test]
    fn step_gain_clamps_at_the_bottom() {
        let near_bottom = ui::fader_db_to_gain(ui::FADER_MIN_DB + 1.0);
        let stepped = step_gain(near_bottom, -VOLUME_STEP_DB);
        assert_eq!(stepped, Gain::SILENT, "clamped to the fader floor -- true silence");
    }

    #[test]
    fn step_gain_moves_by_exactly_the_step_in_the_middle_of_the_range() {
        let mid = ui::fader_db_to_gain(0.0);
        let stepped = step_gain(mid, VOLUME_STEP_DB);
        assert!((ui::gain_to_fader_db(stepped) - VOLUME_STEP_DB).abs() < 1e-3);
    }

    #[test]
    fn position_and_gain_round_trip_through_the_fader_mapping() {
        for position in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let gain = position_to_gain(position);
            let back = gain_to_position(gain);
            assert!((back - position).abs() < 1e-3, "position {position} -> gain -> {back}");
        }
    }

    #[test]
    fn position_zero_and_one_map_to_the_faders_own_extremes() {
        assert_eq!(position_to_gain(0.0), Gain::SILENT);
        assert!((gain_to_position(ui::fader_db_to_gain(ui::FADER_MAX_DB)) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn push_to_mute_restores_the_prior_muted_state() {
        let (held, applied) = push_to_mute(HoldState::default(), HoldEvent::Pressed { actual_muted: false });
        assert_eq!(applied, Some(true), "press always mutes");
        assert!(held.held);

        let (released, applied) = push_to_mute(held, HoldEvent::Released);
        assert_eq!(applied, Some(false), "restores the state from before the press");
        assert!(!released.held);
    }

    #[test]
    fn push_to_mute_leaves_it_muted_if_it_was_already_muted_before_the_press() {
        let (held, _) = push_to_mute(HoldState::default(), HoldEvent::Pressed { actual_muted: true });
        let (_, applied) = push_to_mute(held, HoldEvent::Released);
        assert_eq!(applied, Some(true), "was already muted -- release must not silently unmute");
    }

    #[test]
    fn push_to_mute_restores_on_expiry_when_release_is_missed() {
        let (held, _) = push_to_mute(HoldState::default(), HoldEvent::Pressed { actual_muted: false });
        let (expired, applied) = push_to_mute(held, HoldEvent::Expired);
        assert_eq!(applied, Some(false));
        assert!(!expired.held);
    }

    #[test]
    fn a_second_press_re_arms_from_actual_state_not_remembered_state() {
        // A missed Released left `restore_to` stale (still false), but a
        // fresh press must re-arm from the *actual* current state (true) --
        // decision 15's self-healing property.
        let (held, _) = push_to_mute(HoldState::default(), HoldEvent::Pressed { actual_muted: false });
        let (re_armed, applied) = push_to_mute(held, HoldEvent::Pressed { actual_muted: true });
        assert_eq!(applied, Some(true));
        assert!(re_armed.held);

        let (_, applied) = push_to_mute(re_armed, HoldEvent::Released);
        assert_eq!(applied, Some(true), "re-armed from actual_muted=true, not the stale remembered false");
    }

    #[test]
    fn a_stray_release_with_nothing_held_is_a_no_op() {
        let (state, applied) = push_to_mute(HoldState::default(), HoldEvent::Released);
        assert_eq!(applied, None);
        assert_eq!(state, HoldState::default());
    }

    #[test]
    fn needs_onboarding_is_true_when_there_are_no_groups() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            groups: vec![],
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
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
    fn profile_and_dsp_chain_edits_have_no_mixer_command_equivalent() {
        // Both funnel through their own off-RT swap paths in
        // apply_profile_action (SetDspChain -> apply_dsp_chains) or a plain
        // store write with no engine call at all (SetProfile/RemoveProfile/
        // SetActiveProfile) -- neither has a MixerCommand equivalent.
        let snapshot = snapshot_with_group("Game");
        let edits = vec![
            ConfigEdit::SetDspChain("Game".into(), vec![]),
            ConfigEdit::SetProfile(engine::ProfileConfig {
                name: "Gaming".into(),
                hotkey: None,
                master: Gain::UNITY,
                muted: false,
                groups: vec![],
            }),
            ConfigEdit::RemoveProfile("Gaming".into()),
            ConfigEdit::SetActiveProfile(None),
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

    #[test]
    fn edits_to_mixer_commands_maps_set_group_mute() {
        let snapshot = snapshot_with_group("Game");
        let edits = vec![ConfigEdit::SetGroupMute("Game".into(), true)];

        let commands = edits_to_mixer_commands(&edits, &snapshot);

        assert_eq!(commands.len(), 1);
        assert!(matches!(commands[0], MixerCommand::SetGroupMute(GroupId(0), true)));
    }

    #[test]
    fn edits_to_mixer_commands_maps_set_eq_band_using_its_own_stage_index() {
        // Regression for graphical-eq.md decision 13: the mapping must use
        // the edit's own stage index directly, not search the group's DSP
        // chain for the first Eq-shaped stage (which would silently target
        // the wrong stage under a second Eq stage).
        let snapshot = snapshot_with_group("Game");
        let spec = EqBandSpec { freq_hz: 1000.0, gain_db: 3.0, q: 1.0 };
        let edits = vec![ConfigEdit::SetEqBand("Game".into(), 1, 0, spec)];

        let commands = edits_to_mixer_commands(&edits, &snapshot);

        assert_eq!(commands.len(), 1);
        assert!(matches!(
            commands[0],
            MixerCommand::SetDspParam { group: GroupId(0), stage: 1, param: audio_core::DspParam::EqBand { band: 0, spec: s } }
            if s.freq_hz == 1000.0
        ));
    }

    #[test]
    fn set_eq_bands_is_a_chain_edit_not_a_param() {
        // SetEqBands never has a MixerCommand equivalent -- it always rebuilds
        // the stage via EditDspChains/apply_dsp_chains, even when unchanged
        // in band count (decision 12).
        let snapshot = snapshot_with_group("Game");
        let edits = vec![ConfigEdit::SetEqBands("Game".into(), 0, vec![])];

        let commands = edits_to_mixer_commands(&edits, &snapshot);

        assert!(commands.is_empty());
    }
}
