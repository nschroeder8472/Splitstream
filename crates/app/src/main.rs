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

use control::{group_rules, resolve_sink_status, ConfigEdit, ConfigStore, SinkStatus};
use engine::ports::{AudioSystem, EndpointId, SessionPort, VolumeEvent};
use engine::{
    start_volume_bind, AccentChoice, AppConfig, CaptureControl, ConfigSnapshot, EngineEvent, EngineHandle,
    MirrorAction, RoutingHandle, VolumeBindHandle,
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
    /// A device appeared or disappeared (double-audio-prevention flow A/F) —
    /// internal plumbing, forwarded from `EngineEvent::DeviceAvailable`/
    /// `DeviceRemoved`. Carries nothing for the same reason
    /// `DefaultDeviceChanged` doesn't: the dispatcher re-enumerates rather
    /// than trusting a payload that may already be stale.
    DevicesChanged,
    /// The user accepted "make Splitstream's sink the Windows default output"
    /// (flow B). Opt-in and explicit — Splitstream never takes the default
    /// unasked.
    TakeDefaultOutput,
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

/// What prompted a possible (re-)assertion of the sink as the Windows default.
/// The distinction is the whole of flow E: a default change made by the user
/// or by another audio tool is surfaced, never fought — two programs each
/// re-asserting would ping-pong forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssertTrigger {
    Startup,
    /// The user clicked "make the sink the default output" (flow B step 2).
    UserRequest,
    ExternalDefaultChange,
}

/// Everything one takeover of the Windows default involves: the edits that
/// record what is being displaced, and the device to install.
///
/// The two are returned **together** on purpose. Taking the default and
/// writing down what it was are not separate steps a caller may pick from:
/// flow C restores `previous_default` and clears it, so a takeover that skips
/// the recording leaves the *next* quit with nothing to restore and the
/// machine parked on a sink nobody can hear. That is exactly what shipped —
/// the user's one-time click recorded, the every-start re-assertion did not,
/// so run 1 behaved and every run after it ended silent.
///
/// No `PartialEq`: `ConfigEdit` has none, and tests are clearer asserting on
/// the two fields separately anyway.
#[derive(Debug)]
struct SinkTakeover {
    edits: Vec<ConfigEdit>,
    sink: String,
}

/// Flow B (both the opt-in click and step 3's every-start re-assertion) and
/// flow E. `None` means do nothing at all.
///
/// Says yes only for a startup or an explicit user request, only while the
/// user has opted in, and only when the sink exists but isn't already the
/// default. An external default change never says yes — Splitstream does not
/// fight the user or another audio tool for the default device.
fn plan_sink_takeover(
    app: &AppConfig,
    status: &SinkStatus,
    trigger: AssertTrigger,
    current_default: Option<&str>,
) -> Option<SinkTakeover> {
    if trigger == AssertTrigger::ExternalDefaultChange {
        return None;
    }
    if trigger == AssertTrigger::Startup && !app.manage_default {
        return None;
    }
    let SinkStatus::NotDefault { sink, .. } = status else {
        return None;
    };
    Some(SinkTakeover {
        edits: take_default_edits(app, current_default),
        sink: sink.clone(),
    })
}

/// Flow B: the edits recording that Splitstream has taken the default.
///
/// `previous_default` is written **only when the key is empty** (flow B rule
/// 2). A value already sitting there was left by an unclean exit and still
/// names the user's true pre-Splitstream device, so overwriting it — with the
/// sink, most likely — would lose the only way back (flow D).
fn take_default_edits(app: &AppConfig, current_default: Option<&str>) -> Vec<ConfigEdit> {
    let mut edits = vec![ConfigEdit::SetManageDefault(true)];
    let already_recorded = app.previous_default.is_some();
    // Recording the sink as the thing to restore *to* would make a clean quit
    // a no-op and strand the user on a silent device.
    let is_the_sink = current_default.is_some() && current_default == app.sink_device.as_deref();
    if !already_recorded && !is_the_sink {
        if let Some(name) = current_default {
            edits.push(ConfigEdit::SetPreviousDefault(Some(name.to_string())));
        }
    }
    edits
}

/// Flow C: the edits a clean quit writes once the restore attempt has
/// resolved.
///
/// A **failed** restore writes nothing. The recorded device is the user's only
/// remaining route back to their own default — clearing it after a failure
/// leaves the machine pointed at an endpoint nobody can hear, with
/// `manage_default` still true so the next start re-takes the sink, and no
/// record anywhere of what to go back to. That is the worst state this feature
/// can produce, and it is one `if` away.
fn post_restore_edits(restore_succeeded: bool) -> Vec<ConfigEdit> {
    if restore_succeeded {
        vec![ConfigEdit::SetPreviousDefault(None)]
    } else {
        Vec::new()
    }
}

/// The gain and mute a bound target currently holds — what flow G step 5
/// pushes outward so the Windows slider and OSD start from the newly selected
/// group's level rather than the previous one's. `None` when the binding names
/// a group that no longer exists, which is nothing to mirror rather than an
/// error.
fn bound_target_values(snapshot: &ConfigSnapshot, target: &VolumeTarget) -> Option<(audio_core::Gain, bool)> {
    match target {
        VolumeTarget::Master => Some((snapshot.master, snapshot.muted)),
        VolumeTarget::Group(name) => snapshot
            .groups
            .iter()
            .find(|g| &g.name == name)
            .map(|g| (g.gain, g.muted)),
    }
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
        // Flow A: `[app] sink_device` is one of the three inputs, so any
        // snapshot change can move the status — including a hand-edit of the
        // config file.
        self.refresh_sink_status();
    }

    /// Flow A: recompute `SinkStatus` from the three facts that define it and
    /// publish it for the UI. Pure work under the lock; the device list is
    /// whatever `handle_devices_changed` last installed, never re-enumerated
    /// here (`enumerate()` is a blocking COM call, and this runs on every
    /// param edit — including every frame of a fader drag).
    fn refresh_sink_status(&self) {
        let mut ui = self.ui.lock().unwrap();
        let names: Vec<String> = ui.available_devices.iter().map(|e| e.name.clone()).collect();
        ui.sink_status = resolve_sink_status(
            self.current.app.sink_device.as_deref(),
            &names,
            self.default_output_name.as_deref(),
        );
    }

    /// Flow F (and flow A's device-list input): a device appeared or vanished.
    /// `enumerate()` blocks on COM, so it runs *outside* every lock and the
    /// result is assigned under a short one — the recurring "blocking call
    /// under a shared lock" shape this codebase has hit four times.
    fn handle_devices_changed(&mut self) {
        let Ok(endpoints) = self.sys.enumerate() else {
            return; // transient enumeration failure: keep the last known list
        };
        self.ui.lock().unwrap().available_devices = endpoints;
        self.refresh_sink_status();
    }

    /// Flow B: the user accepted taking the default. Records the outgoing
    /// default first (only-if-empty), then installs the sink — that order
    /// means a failure between the two leaves a recoverable config rather
    /// than a moved default nobody remembers the way back from.
    fn handle_take_default_output(&mut self) {
        self.take_sink_as_default(AssertTrigger::UserRequest);
    }

    /// Flow B, both entry points: record what is about to be displaced, then
    /// install the sink. One method rather than two call sites, because the
    /// two halves must never come apart — see [`SinkTakeover`].
    ///
    /// The edits are deliberately **not** rolled back if the install fails:
    /// `previous_default` then names the device that is still the default, so
    /// the quit-time restore is a harmless no-op, while `manage_default`
    /// staying true means the next start retries. The banner keeps reporting
    /// `NotDefault` either way, which is the honest answer (capability 6).
    fn take_sink_as_default(&mut self, trigger: AssertTrigger) {
        let status = self.ui.lock().unwrap().sink_status.clone();
        let Some(takeover) = plan_sink_takeover(
            &self.current.app,
            &status,
            trigger,
            self.default_output_name.as_deref(),
        ) else {
            return;
        };
        tracing::info!(
            sink = takeover.sink,
            displacing = self.default_output_name.as_deref().unwrap_or("<unknown>"),
            ?trigger,
            "taking the Windows default output"
        );
        self.apply_params(&takeover.edits);
        let _took = self.set_default_output_by_name(&takeover.sink);
    }

    /// Installs the named device as the Windows default for all three roles —
    /// used both to take the sink (flow B) and to hand the user's own device
    /// back (flow C). Best-effort and loudly reported (capability 6): never a
    /// panic, never a retry.
    ///
    /// Reports whether it actually worked. Callers that record state around
    /// this **must** branch on it: an unknown device name and a failed COM
    /// call are both real outcomes here, and treating either as success is
    /// what turns a recoverable state into a silent machine.
    fn set_default_output_by_name(&self, device_name: &str) -> bool {
        let Some(id) = self.endpoint_id_for(device_name) else {
            eprintln!("cannot set default output: no device named {device_name:?}");
            return false;
        };
        match self.sys.set_default_output(&id) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("setting the default output to {device_name:?} failed: {e:?}");
                false
            }
        }
    }

    /// Flow C: hand back the default device Splitstream displaced, and clear
    /// the record of it **only if that worked** (see [`post_restore_edits`]).
    /// No-op when Splitstream never took the default.
    fn restore_previous_default(&mut self) {
        let Some(previous) = self.current.app.previous_default.clone() else {
            return;
        };
        tracing::info!(previous, "restoring the default output device");
        let restored = self.set_default_output_by_name(&previous);
        if !restored {
            tracing::warn!(
                previous,
                "could not restore the previous default device — keeping the record so the next \
                 start can still put it back"
            );
        }
        self.apply_params(&post_restore_edits(restored));
    }

    /// Friendly name -> `EndpointId`. Config stores device *names* (the same
    /// vocabulary as every group's `output_device`), while the port speaks ids.
    fn endpoint_id_for(&self, name: &str) -> Option<EndpointId> {
        let ui = self.ui.lock().unwrap();
        ui.available_devices
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.id.clone())
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

        // Double-audio-prevention flow E. Our own `set_default_output` echoes
        // back through this same path; so does a genuine user or OS change.
        // Both are treated identically — recompute and surface, never
        // re-assert. `take_sink_as_default` is called rather than skipped so
        // the rule lives in code at the one place a future reader would be
        // tempted to break it: two tools each re-taking the default would
        // ping-pong, and fighting the user is hostile besides.
        self.refresh_sink_status();
        self.take_sink_as_default(AssertTrigger::ExternalDefaultChange);
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
            self.routing.update_rules(group_rules(&new_snapshot), new_snapshot.app.excluded.clone());
        } else {
            if !delta.params.is_empty() {
                if let Err(e) = self.handle.apply_params(delta.params) {
                    eprintln!("apply_params failed: {e:?}");
                }
            }
            if delta.rules.is_some() || delta.excluded.is_some() {
                self.routing.update_rules(group_rules(&new_snapshot), new_snapshot.app.excluded.clone());
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
                self.routing.update_rules(group_rules(&new_snapshot), new_snapshot.app.excluded.clone());
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

        let routing_changed = edits.iter().any(|e| matches!(e, ConfigEdit::SetRules(..) | ConfigEdit::SetExcluded(..)));
        match self.store.apply(edits) {
            Ok(new_snapshot) => {
                if routing_changed {
                    self.routing.update_rules(group_rules(&new_snapshot), new_snapshot.app.excluded.clone());
                }
                self.set_current(new_snapshot);
                self.push_bound_target_changes(edits);
                if edits.iter().any(|e| matches!(e, ConfigEdit::SetVolumeBind(_))) {
                    self.push_new_bind_outward();
                }
            }
            Err(e) => eprintln!("param edit rejected: {e:?}"),
        }
    }

    /// Flow G step 5: the volume keys now drive a *different* target, so push
    /// that target's current gain and mute outward immediately. Without this
    /// the endpoint slider still sits at the previously-selected group's
    /// level, and the first key press after switching would jump the newly
    /// selected group to it. Runs after `set_current`, so `bound_target()`
    /// already resolves to the new selection.
    fn push_new_bind_outward(&self) {
        let Some(target) = self.bound_target() else { return };
        let Some((gain, muted)) = bound_target_values(&self.current, &target) else {
            return;
        };
        self.push_target_volume_if_bound(&target, gain);
        self.push_target_mute_if_bound(&target, muted);
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
                    self.routing.update_rules(group_rules(&new_snapshot), new_snapshot.app.excluded.clone());
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
            ShellAction::DevicesChanged => self.handle_devices_changed(),
            ShellAction::TakeDefaultOutput => self.handle_take_default_output(),
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
            | ConfigEdit::SetAccent(..)
            | ConfigEdit::SetExcluded(..)
            | ConfigEdit::SetSinkDevice(..)
            | ConfigEdit::SetManageDefault(..)
            | ConfigEdit::SetPreviousDefault(..)
            | ConfigEdit::SetVolumeBind(..) => None,
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
                // Double-audio-prevention flow A/F: the sink is never a group
                // output, so its arrival/removal reaches the app layer only
                // through these two unconditional announcements.
                if matches!(evt, EngineEvent::DeviceAvailable(_) | EngineEvent::DeviceRemoved(_)) {
                    let _ = actions_tx.send(ShellAction::DevicesChanged);
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
        snapshot.app.excluded.clone(),
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
        // Replaced by the dispatcher's first `refresh_sink_status` below,
        // once it exists to compute one.
        sink_status: control::SinkStatus::NotConfigured,
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
    // Double-audio-prevention: publish the first `SinkStatus` (flow A), then
    // re-assert the sink if the user has opted in (flow B step 3). A
    // `previous_default` already sitting in the config at this point was left
    // by an unclean exit — deliberately left untouched (flow D) so the next
    // clean quit still restores the user's true pre-Splitstream device.
    dispatcher.refresh_sink_status();
    dispatcher.take_sink_as_default(AssertTrigger::Startup);
    // Temporary diagnostic (audio-flow-control follow-up): these counters
    // already exist on EngineStats but nothing logs or displays them, so a
    // flow-control regression (drops/xruns) is indistinguishable from a
    // signal-domain one (limiter clipping) without this. Logs only on
    // change — safe to leave running for a full repro session.
    let mut last_flow_stats: Option<(u64, u64, u64, u64, u64)> = None;
    // Opt-in audit trace (`SPLITSTREAM_AUDIT=1`): once a second, the flow
    // state the counters alone can't distinguish — is a ring starving, is the
    // drift controller running away, is a group producing signal at all.
    // Off by default; this is a repro tool, not normal-run logging.
    let audit = std::env::var_os("SPLITSTREAM_AUDIT").is_some();
    let mut last_audit = std::time::Instant::now();
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
        let fresh_stats = dispatcher.handle.stats();
        let limiter_engaged_total: u64 =
            fresh_stats.limiter_engaged.iter().map(|(_, c)| *c).sum();
        let flow_stats = (
            fresh_stats.xruns,
            fresh_stats.output_drops,
            fresh_stats.capture_drops,
            fresh_stats.render_shortfall,
            limiter_engaged_total,
        );
        if last_flow_stats != Some(flow_stats) {
            tracing::info!(
                xruns = flow_stats.0,
                output_drops = flow_stats.1,
                capture_drops = flow_stats.2,
                render_shortfall = flow_stats.3,
                limiter_engaged_total = flow_stats.4,
                "flow-control counters changed"
            );
            last_flow_stats = Some(flow_stats);
        }
        if audit && last_audit.elapsed() >= Duration::from_secs(1) {
            last_audit = std::time::Instant::now();
            let fill: Vec<String> = fresh_stats
                .ring_fill
                .iter()
                .map(|(id, f)| format!("{}:{:.2}", id.0, f))
                .collect();
            let ratio: Vec<String> = fresh_stats
                .applied_ratio
                .iter()
                .map(|(id, r)| format!("{}:{:.5}", id.0, r))
                .collect();
            // Input side of `ring_fill`, per group, and what the drift loop
            // controls against (aggregated to the fullest per output). Smoothed
            // at the source, so a value drifting off 0.5 is a standing rate
            // surplus rather than which part of a poll packet the read landed on.
            let cfill: Vec<String> = fresh_stats
                .capture_fill
                .iter()
                .map(|(id, f)| format!("{}:{:.2}", id.0, f))
                .collect();
            let gpeak: Vec<String> = fresh_stats
                .group_peak
                .iter()
                .map(|(id, m)| format!("{}:{:.4}", id.0, m.peak))
                .collect();
            let opeak: Vec<String> = fresh_stats
                .output_peak
                .iter()
                .map(|(id, m)| format!("{}:{:.4}", id.0, m.peak))
                .collect();
            tracing::info!(
                xruns = fresh_stats.xruns,
                output_drops = fresh_stats.output_drops,
                capture_drops = fresh_stats.capture_drops,
                capture_fill = %cfill.join(","),
                ring_fill = %fill.join(","),
                applied_ratio = %ratio.join(","),
                group_peak = %gpeak.join(","),
                output_peak = %opeak.join(","),
                routes = dispatcher.routing.reader().current_routes().len(),
                "audit"
            );
        }
        dispatcher.ui.lock().unwrap().stats = fresh_stats;
    }

    drop(watcher);
    // Flow C, before anything else is torn down: put the user's own default
    // device back and clear the key, so quitting never leaves the machine
    // pointed at an endpoint nobody can hear. A `previous_default` that is
    // still set on the next start therefore means the exit was unclean (flow
    // D), which is precisely the signal that recovery relies on.
    dispatcher.restore_previous_default();
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

    fn app_with_sink(sink: &str) -> AppConfig {
        AppConfig {
            sink_device: Some(sink.into()),
            manage_default: true,
            ..AppConfig::default()
        }
    }

    // --- double-audio-prevention flows B/D: taking the default -------------

    #[test]
    fn taking_the_default_records_the_previous_one() {
        let app = app_with_sink("CABLE Input");

        let edits = take_default_edits(&app, Some("Headphones"));

        assert!(edits.iter().any(|e| matches!(e, ConfigEdit::SetManageDefault(true))));
        assert!(edits.iter().any(|e| matches!(
            e,
            ConfigEdit::SetPreviousDefault(Some(name)) if name == "Headphones"
        )));
    }

    /// Flow B rule 2 / flow D: a `previous_default` already on disk was left
    /// by an unclean exit and still names the user's true pre-Splitstream
    /// device. Overwriting it — with the sink, most likely — throws away the
    /// only way back.
    #[test]
    fn taking_the_default_twice_does_not_overwrite_the_recorded_previous() {
        let app = AppConfig {
            previous_default: Some("Headphones".into()),
            ..app_with_sink("CABLE Input")
        };

        let edits = take_default_edits(&app, Some("CABLE Input"));

        assert!(!edits
            .iter()
            .any(|e| matches!(e, ConfigEdit::SetPreviousDefault(_))));
    }

    /// A start that inherits the default from an unclean exit must leave the
    /// recorded device intact — same rule as above, viewed from flow D. Pins
    /// the *whole* edit list rather than just the absence of one variant, so
    /// nothing can be added here that touches the record by another route.
    #[test]
    fn a_start_finding_previous_default_already_set_leaves_it_intact() {
        let app = AppConfig {
            previous_default: Some("Speakers".into()),
            ..app_with_sink("CABLE Input")
        };

        let edits = take_default_edits(&app, Some("Headphones"));

        assert_eq!(edits.len(), 1);
        assert!(matches!(edits[0], ConfigEdit::SetManageDefault(true)));
    }

    // --- flow C: what a clean quit does about the displaced default --------

    #[test]
    fn a_successful_restore_clears_the_recorded_previous_default() {
        let edits = post_restore_edits(true);

        assert_eq!(edits.len(), 1);
        assert!(matches!(edits[0], ConfigEdit::SetPreviousDefault(None)));
    }

    /// The record is the user's only remaining route back to their own default
    /// device. Clearing it after a failed restore leaves the machine pointed at
    /// an endpoint nobody can hear, `manage_default` still true so the next
    /// start re-takes the sink, and nothing anywhere naming what to go back to.
    #[test]
    fn a_failed_restore_keeps_the_recorded_previous_default() {
        let edits = post_restore_edits(false);

        assert!(edits.is_empty());
    }

    /// Recording the sink as the device to restore *to* would make a clean
    /// quit a no-op and strand the machine on an endpoint nobody can hear.
    #[test]
    fn taking_the_default_never_records_the_sink_itself_as_the_previous() {
        let app = app_with_sink("CABLE Input");

        let edits = take_default_edits(&app, Some("CABLE Input"));

        assert!(!edits
            .iter()
            .any(|e| matches!(e, ConfigEdit::SetPreviousDefault(_))));
    }

    // --- flow B / flow E: when to take the default, and what it records ---

    fn not_default(sink: &str, current: &str) -> SinkStatus {
        SinkStatus::NotDefault {
            sink: sink.into(),
            current_default: Some(current.into()),
        }
    }

    /// **Regression, found on real hardware 2026-07-26.** The every-start
    /// re-assertion used to install the sink without recording what it
    /// displaced. Flow C clears `previous_default` on each clean quit, so from
    /// the second run onward there was nothing to restore and every quit left
    /// the machine parked on a sink nobody can hear.
    #[test]
    fn a_startup_takeover_records_the_default_it_displaces() {
        let app = app_with_sink("CABLE Input");
        let status = not_default("CABLE Input", "Headphones");

        let takeover = plan_sink_takeover(&app, &status, AssertTrigger::Startup, Some("Headphones"))
            .expect("a start that opted in must take the sink");

        assert_eq!(takeover.sink, "CABLE Input");
        assert!(
            takeover.edits.iter().any(|e| matches!(
                e,
                ConfigEdit::SetPreviousDefault(Some(name)) if name == "Headphones"
            )),
            "taking the default without recording it strands the next quit"
        );
    }

    /// The invariant behind [`SinkTakeover`], stated once so no future call
    /// site can take the default and skip the record: whenever a takeover is
    /// planned and nothing is on file yet, it carries the recording edit.
    #[test]
    fn every_planned_takeover_records_the_previous_default_when_none_is_on_file() {
        let app = app_with_sink("CABLE Input");
        let status = not_default("CABLE Input", "Headphones");

        for trigger in [AssertTrigger::Startup, AssertTrigger::UserRequest] {
            let takeover = plan_sink_takeover(&app, &status, trigger, Some("Headphones"))
                .unwrap_or_else(|| panic!("{trigger:?} should plan a takeover"));

            assert!(
                takeover.edits.iter().any(|e| matches!(e, ConfigEdit::SetPreviousDefault(Some(_)))),
                "{trigger:?} planned a takeover with no way back recorded"
            );
        }
    }

    #[test]
    fn a_user_request_takes_the_sink_even_before_opting_in() {
        let app = AppConfig {
            manage_default: false,
            ..app_with_sink("CABLE Input")
        };
        let status = not_default("CABLE Input", "Headphones");

        let takeover = plan_sink_takeover(&app, &status, AssertTrigger::UserRequest, Some("Headphones"));

        assert!(takeover.is_some(), "the click *is* the opt-in");
    }

    /// Flow E: a default change made by the user, or by another audio tool
    /// doing the same thing, is surfaced and left alone. Re-asserting would
    /// ping-pong forever.
    #[test]
    fn an_external_default_change_is_surfaced_and_never_re_asserted() {
        let app = app_with_sink("CABLE Input");
        let status = not_default("CABLE Input", "Headphones");

        let takeover =
            plan_sink_takeover(&app, &status, AssertTrigger::ExternalDefaultChange, Some("Headphones"));

        assert!(takeover.is_none());
    }

    /// Our own `set_default_output` echoes back through the same
    /// default-changed path. It needs no special casing: the status it
    /// produces is `Active`, and nothing is taken against `Active`.
    #[test]
    fn our_own_default_change_is_suppressed_as_an_echo() {
        let app = app_with_sink("CABLE Input");
        let status = SinkStatus::Active { sink: "CABLE Input".into() };

        for trigger in [
            AssertTrigger::Startup,
            AssertTrigger::UserRequest,
            AssertTrigger::ExternalDefaultChange,
        ] {
            assert!(plan_sink_takeover(&app, &status, trigger, Some("CABLE Input")).is_none());
        }
    }

    #[test]
    fn a_start_does_not_take_the_default_the_user_never_opted_into() {
        let app = AppConfig {
            manage_default: false,
            ..app_with_sink("CABLE Input")
        };
        let status = not_default("CABLE Input", "Headphones");

        assert!(plan_sink_takeover(&app, &status, AssertTrigger::Startup, Some("Headphones")).is_none());
    }

    /// Flow F: a sink that isn't present can't be installed as the default —
    /// the status is surfaced instead, and no COM call is attempted.
    #[test]
    fn sink_removal_mid_session_reports_missing_without_re_taking() {
        let app = app_with_sink("CABLE Input");
        let status = SinkStatus::Missing { configured: "CABLE Input".into() };

        for trigger in [AssertTrigger::Startup, AssertTrigger::UserRequest] {
            assert!(plan_sink_takeover(&app, &status, trigger, Some("Headphones")).is_none());
        }
    }

    // --- flow G step 5: selection re-syncs the endpoint slider -------------

    /// Without this push, the endpoint's slider still sits at the
    /// previously-selected group's level, and the first volume key press
    /// after switching jumps the newly selected group straight to it.
    #[test]
    fn selecting_a_group_pushes_its_current_gain_outward() {
        let mut snapshot = snapshot_with_group("Game");
        let quiet = ui::fader_db_to_gain(-12.0);
        snapshot.groups[0].gain = quiet;
        snapshot.groups[0].muted = true;

        let values = bound_target_values(&snapshot, &VolumeTarget::Group("Game".into()));

        assert_eq!(values, Some((quiet, true)));
    }

    #[test]
    fn selecting_master_pushes_the_master_gain_outward() {
        let mut snapshot = snapshot_with_group("Game");
        snapshot.master = ui::fader_db_to_gain(-6.0);

        let values = bound_target_values(&snapshot, &VolumeTarget::Master);

        assert_eq!(values, Some((snapshot.master, false)));
    }

    #[test]
    fn a_binding_naming_a_missing_group_pushes_nothing() {
        let snapshot = snapshot_with_group("Game");

        let values = bound_target_values(&snapshot, &VolumeTarget::Group("Gone".into()));

        assert_eq!(values, None);
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
