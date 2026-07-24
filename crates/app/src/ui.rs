//! Settings window (egui/eframe): master + per-group fader columns (mockup:
//! `RoughAppUI.png`), dropdown-sourced fields where a valid-options source
//! exists, and drag-and-drop assignment of live audio sessions onto group
//! columns (mixer-ui-redesign). Follow-master/spatial/DSP/duck/match-rules
//! fallback/remove-group live behind each column's gear icon, out of the
//! always-visible view.
//!
//! Reads `routes`/`routing_degraded`/`all_sessions` via `RoutingReader`,
//! polled fresh every frame (see event_pump.rs's doc comment for why: no
//! `EngineEvent` variant signals a route change, so polling is strictly more
//! correct than trying to infer one from unrelated event arrival). All edits
//! go out as `ShellAction`s — this module never touches `ConfigStore` or
//! `EngineHandle` directly (app-shell.md constraint: UI mutates config and
//! sends commands, never calls into `win-audio`).
//!
//! Only the match-rules text fallback keeps a per-group **draft** string
//! (fights in-progress typing if re-derived every frame). Output device and
//! duck-trigger are dropdown-backed — a selection is a discrete commit, no
//! in-progress state to protect, so they read the live value fresh every
//! frame like the gain/duck sliders already do.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use audio_core::{db_to_linear, linear_to_db, DspSpec, EqBandSpec, Gain, GroupId, MeterLevel, OutputId};
use control::ConfigEdit;
use engine::ports::Endpoint;
use engine::{DuckSpecConfig, GroupConfig, MatchRule, RoutingReader, SessionInfo, StatsReader};

use crate::event_pump::UiState;
use crate::ShellAction;

/// Drag payload: a session's pid. `Any + Send + Sync` (egui dnd requirement) —
/// a bare `u32` newtype is enough, the receiving drop zone looks the full
/// `SessionInfo` back up from `all_sessions` by pid.
struct DragSession(u32);

/// Column width clamps (responsive-ui-refinement L4) — narrow enough to fit
/// several side-by-side per the mockup (`RoughAppUI.png`), wide enough to
/// stay readable. Below `MIN_COLUMN_WIDTH * column count`, the row scrolls
/// horizontally instead of shrinking further (L3 flow F).
const MIN_COLUMN_WIDTH: f32 = 100.0;
const MAX_COLUMN_WIDTH: f32 = 220.0;

/// Vertical fader length clamps (responsive-ui-refinement L4).
const MIN_FADER_HEIGHT: f32 = 120.0;
const MAX_FADER_HEIGHT: f32 = 400.0;

/// Rough vertical space a column's chrome (name row, mute button or output
/// dropdown, "Routed Apps" label + drop zone) takes up around the fader —
/// subtracted from the row's available height before clamping fader length.
/// Tuned by eye, not exact; egui sizes the surrounding widgets by their own
/// content regardless, this only informs how much length the fader claims.
/// Bumped for the page-level search box (session-search-and-guidance.md
/// decision 6) — it renders above the column row, so its height eats into
/// the same `available.y` this constant is subtracted from.
const COLUMN_CHROME_HEIGHT: f32 = 190.0;

/// Fixed on-screen size for the custom-painted speaker icon — the icon
/// itself doesn't need to scale with the responsive column/fader sizing,
/// only its position (always directly under Master's fader) does. Square,
/// so the volume-arc math below doesn't need separate x/y scale factors.
const SPEAKER_ICON_SIZE: egui::Vec2 = egui::vec2(28.0, 28.0);

/// Column width given the available row width and how many columns (master
/// plus groups, not counting the floating "+") share it — clamped so
/// columns never get unreadably narrow or absurdly wide. Pure
/// (responsive-ui-refinement L4).
fn column_width(available_width: f32, column_count: usize) -> f32 {
    let count = column_count.max(1) as f32;
    (available_width / count).clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
}

/// Vertical fader length given the column's available height — clamped so
/// the fader never vanishes on a short window or stretches absurdly tall.
/// Pure (responsive-ui-refinement L4).
fn fader_height(available_height: f32) -> f32 {
    available_height.clamp(MIN_FADER_HEIGHT, MAX_FADER_HEIGHT)
}

/// Read-only data `group_column` needs besides `self`/`ui`/the group itself —
/// bundled once the plain parameter list crossed clippy's
/// `too_many_arguments` threshold (operational learnings: extract at that
/// point, not later — same idiom as `engine::runtime`'s `CaptureFaultCtx`/
/// `RenderFaultCtx`). All borrows/`Copy` values, so trivially `Copy`.
#[derive(Clone, Copy)]
struct GroupColumnCtx<'a> {
    routes: &'a [(GroupId, Vec<SessionInfo>)],
    all_sessions: &'a [SessionInfo],
    all_groups: &'a [GroupConfig],
    devices: &'a [Endpoint],
    /// Per-group meter readings this frame (level-meters.md), keyed by
    /// `GroupId`, polled from `StatsReader` in `ui()`.
    group_peak: &'a [(GroupId, MeterLevel)],
    /// Responsive sizing (responsive-ui-refinement L4) — computed once per
    /// frame in `ui()` from `ui.available_size()`, shared by every column.
    width: f32,
    fader_height: f32,
    /// This frame's session search filter (session-search-and-guidance.md) —
    /// a clone of `SettingsApp.search`, threaded through rather than borrowed,
    /// so it doesn't hold `self` borrowed across the `&mut self` column calls.
    query: &'a str,
}

/// Read-only data `master_column` needs besides `self`/`ui` — bundled once the
/// meter/device-list params pushed the plain list past clippy's
/// `too_many_arguments` threshold (operational learnings: extract at that
/// point, same idiom as `GroupColumnCtx`). All borrows/`Copy`, so trivially
/// `Copy`.
#[derive(Clone, Copy)]
struct MasterColumnCtx<'a> {
    snapshot: &'a engine::ConfigSnapshot,
    routes: &'a [(GroupId, Vec<SessionInfo>)],
    all_sessions: &'a [SessionInfo],
    output_peak: &'a [(OutputId, MeterLevel)],
    /// Friendly device name per `OutputId` (level-meters.md) — from
    /// `EngineStats::output_names`, so labels track the engine's real output
    /// assignment even when a group is parked.
    output_names: &'a [(OutputId, String)],
    width: f32,
    height: f32,
    /// This frame's session search filter (session-search-and-guidance.md) —
    /// see `GroupColumnCtx.query` for why it's threaded, not borrowed.
    query: &'a str,
}

/// One frame's owned copy of everything the render pass reads out of the
/// shared [`UiState`], taken under a single short lock by
/// [`SettingsApp::take_frame`]. A named struct rather than a tuple: the
/// destructure had reached twelve positional elements, the same creep the
/// `GroupColumnCtx`/`MasterColumnCtx` extractions exist to prevent — and
/// clippy's `too_many_arguments` doesn't police tuples. `stats` is carried
/// whole instead of decomposed into its five separately-cloned fields.
struct Frame {
    snapshot: engine::ConfigSnapshot,
    routes: Vec<(GroupId, Vec<SessionInfo>)>,
    all_sessions: Vec<SessionInfo>,
    degraded: bool,
    stats: engine::EngineStats,
    first_run: bool,
    available_devices: Vec<Endpoint>,
    default_output_name: Option<String>,
    rebuild_generation: u64,
}

/// Which page `SettingsApp` is currently showing (responsive-ui-refinement
/// L4) — replaces the old `advanced_open: HashMap<String, bool>` /
/// `master_advanced_open: bool`. Exactly one page is ever visible, so an
/// enum has no representable invalid state a bool map could (two groups'
/// pages "open" at once).
#[derive(Clone)]
enum Screen {
    Mixer,
    GroupSettings(String),
}

#[derive(Default)]
struct GroupDraft {
    /// Free-text match-rule fallback (advanced panel) — still drafted, same
    /// reasoning as before: fights in-progress typing if re-derived every
    /// frame. `output_device`/`duck_trigger` are gone from this struct
    /// (mixer-ui-redesign L4) — both are dropdown-backed now, so they read
    /// the live value fresh every frame instead, like every other slider.
    match_rules: String,
}

#[derive(Default)]
struct NewGroupDraft {
    name: String,
    output_device: String,
}

/// First-run onboarding panel's in-progress picks (simple-launch.md, revised
/// for process-loopback-capture: no more virtual bus to pick — just an
/// output device). Own draft state, same reasoning as
/// `GroupDraft`/`NewGroupDraft` — a selected device name and a checkbox
/// shouldn't fight in-progress interaction by re-deriving from `UiState`
/// every frame.
struct OnboardingDraft {
    output_device: String,
    autostart: bool,
}

impl Default for OnboardingDraft {
    fn default() -> Self {
        // Autostart default ON (simple-launch.md decision) — matches the
        // seed config template's own `[app] autostart = true`.
        OnboardingDraft { output_device: String::new(), autostart: true }
    }
}

pub struct SettingsApp {
    ui: Arc<Mutex<UiState>>,
    routing: RoutingReader,
    /// Polled every frame for live level meters (level-meters.md), the same
    /// per-frame pull as `routing`. Kept distinct from `UiState.stats` on
    /// purpose: `ui()` writes the fresh pull into `UiState.stats` so the whole
    /// window (meters + xrun/fault footer) reads one consistent snapshot.
    stats: StatsReader,
    actions: Sender<ShellAction>,
    drafts: HashMap<String, GroupDraft>,
    /// Peak-hold marker state per meter, keyed by group name / device name.
    /// UI-only (level-meters.md decision — no domain footprint); decays in
    /// frame time inside `level_meter`.
    holds: HashMap<String, HoldDot>,
    new_group: NewGroupDraft,
    onboarding: OnboardingDraft,
    /// Which page is showing — mixer or one group's full-screen settings
    /// (responsive-ui-refinement L4). Replaces the old
    /// `advanced_open`/`master_advanced_open` bool fields entirely.
    screen: Screen,
    /// Floating "+" toggles the "Create New Audio Source" panel — hidden by
    /// default, matching the mockup's floating button rather than today's
    /// always-inline row.
    show_new_group_panel: bool,
    /// Session-only solo set, keyed by group name (per-group-mute-solo.md
    /// decision 5: UI-owned, not shell-owned). A `HashSet` because multiple
    /// groups may be soloed at once.
    soloed: HashSet<String>,
    /// Last `UiState.rebuild_generation` this UI has reacted to — a jump
    /// clears `soloed` (decision 8).
    seen_generation: u64,
    /// Session chip search filter (session-search-and-guidance.md) — free
    /// text needs draft state, same reasoning as `GroupDraft.match_rules`.
    /// Never persisted; transient UI state only.
    search: String,
}

impl SettingsApp {
    pub fn new(
        ui: Arc<Mutex<UiState>>,
        routing: RoutingReader,
        stats: StatsReader,
        actions: Sender<ShellAction>,
    ) -> SettingsApp {
        SettingsApp {
            ui,
            routing,
            stats,
            actions,
            drafts: HashMap::new(),
            holds: HashMap::new(),
            new_group: NewGroupDraft::default(),
            onboarding: OnboardingDraft::default(),
            screen: Screen::Mixer,
            show_new_group_panel: false,
            soloed: HashSet::new(),
            seen_generation: 0,
            search: String::new(),
        }
    }

    fn send(&self, action: ShellAction) {
        let _ = self.actions.send(action);
    }

    fn draft_for(&mut self, group: &GroupConfig) -> &mut GroupDraft {
        self.drafts.entry(group.name.clone()).or_insert_with(|| GroupDraft {
            match_rules: group.match_rules.join(", "),
        })
    }

    /// Copies this frame's view of the shared state out under one short lock.
    /// Nothing here calls into another subsystem — every cross-thread pull
    /// already happened before the lock was taken (see `ui`).
    fn take_frame(&self) -> Frame {
        let state = self.ui.lock().unwrap();
        Frame {
            snapshot: state.snapshot.clone(),
            routes: state.routes.clone(),
            all_sessions: state.all_sessions.clone(),
            degraded: state.routing_degraded,
            stats: state.stats.clone(),
            first_run: state.first_run,
            available_devices: state.available_devices.clone(),
            default_output_name: state.default_output_name.clone(),
            rebuild_generation: state.rebuild_generation,
        }
    }
}

/// Rebuild-generation jump clears the session-only solo set (decision 8,
/// resolves open question 1). Pure so the clear-on-rebuild rule is
/// unit-testable without an egui frame.
fn clear_solo_on_rebuild(soloed: &mut HashSet<String>, seen: &mut u64, current: u64) {
    if current != *seen {
        soloed.clear();
        *seen = current;
    }
}

/// Mute-excludes-dim precedence for the "silenced by someone else's solo"
/// visual state (per-group-mute-solo.md L1 capability 7). A muted group is
/// already visually distinct via its own lit M button, so it's deliberately
/// excluded here rather than double-marked. Pure, so this precedence is
/// unit-testable without an egui frame — same rationale as
/// `clear_solo_on_rebuild`.
fn is_dimmed_by_other_solo(solo_active: bool, is_soloed: bool, muted: bool) -> bool {
    solo_active && !is_soloed && !muted
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Every per-frame pull happens *before* `self.ui` is locked, never
        // inside the lock scope. Each of these reads takes another mutex one
        // hop down (`RoutingReader`'s, and `StatsReader`'s engine `running`
        // cell) — and `running` is held across blocking WASAPI device opens
        // during a rebuild, so pulling under the UI lock would stall the render
        // thread *while holding the state every other thread needs*. Read
        // unlocked, assign under a short lock (the recurring "blocking call
        // under a shared lock" shape, operational learnings 2026-07-22).
        let fresh_routes = self.routing.current_routes();
        let fresh_degraded = self.routing.is_degraded();
        let fresh_sessions = self.routing.all_sessions();
        // Fresh telemetry every frame (level-meters.md L3 flow C) — feeds both
        // the meters and the xrun/fault footer from one snapshot.
        let fresh_stats = self.stats.stats();
        {
            let mut state = self.ui.lock().unwrap();
            state.routes = fresh_routes;
            state.routing_degraded = fresh_degraded;
            state.all_sessions = fresh_sessions;
            state.stats = fresh_stats;
        }

        let Frame {
            snapshot,
            routes,
            all_sessions,
            degraded,
            stats,
            first_run,
            available_devices,
            default_output_name,
            rebuild_generation,
        } = self.take_frame();
        clear_solo_on_rebuild(&mut self.soloed, &mut self.seen_generation, rebuild_generation);

        if first_run {
            egui::CentralPanel::default().show(ui, |ui| {
                self.onboarding_panel(ui, &available_devices, default_output_name.as_deref());
            });
            return;
        }

        // Safety net (L3 flow D): if the currently-open group settings page
        // named a group that no longer exists (its own "Remove group" click
        // already handles the expected case by resetting screen directly —
        // this also covers an external config edit removing it out from
        // under an open page), fall back to the mixer rather than looking up
        // a missing group below.
        let screen_is_stale = matches!(&self.screen, Screen::GroupSettings(name) if !snapshot.groups.iter().any(|g| &g.name == name));
        if screen_is_stale {
            self.screen = Screen::Mixer;
        }

        egui::CentralPanel::default().show(ui, |ui| match self.screen.clone() {
            Screen::Mixer => {
                if degraded {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 80, 40),
                        "Routing degraded — some app auto-routing may not work.",
                    );
                    ui.separator();
                }

                ui.heading("Splitstream");

                // Search filters every chip zone at once (decision 4: box
                // only appears once something exists to search). Cloned once
                // rather than borrowed, so `self.search` isn't held across
                // the `&mut self` column calls below.
                if !all_sessions.is_empty() {
                    search_box(ui, &mut self.search);
                }
                let query = self.search.clone();

                // Responsive sizing (responsive-ui-refinement L4/L3 flow A) —
                // recomputed fresh every frame from the space actually
                // available, no cached layout state.
                let available = ui.available_size();
                let column_count = 1 + snapshot.groups.len();
                let width = column_width(available.x, column_count);
                let height = fader_height(available.y - COLUMN_CHROME_HEIGHT);

                let group_ctx = GroupColumnCtx {
                    routes: &routes,
                    all_sessions: &all_sessions,
                    all_groups: &snapshot.groups,
                    devices: &available_devices,
                    group_peak: &stats.group_peak,
                    width,
                    fader_height: height,
                    query: &query,
                };
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let master_ctx = MasterColumnCtx {
                            snapshot: &snapshot,
                            routes: &routes,
                            all_sessions: &all_sessions,
                            output_peak: &stats.output_peak,
                            output_names: &stats.output_names,
                            width,
                            query: &query,
                            height,
                        };
                        self.master_column(ui, &master_ctx);
                        for (i, group) in snapshot.groups.iter().enumerate() {
                            self.group_column(ui, group, GroupId(i as u16), &group_ctx);
                        }
                        if ui.button("+").on_hover_text("Create New Audio Source").clicked() {
                            self.show_new_group_panel = !self.show_new_group_panel;
                        }
                    });
                });

                if self.show_new_group_panel {
                    ui.separator();
                    self.add_group_controls(ui, &available_devices);
                }

                ui.separator();
                ui.label(format!(
                    "xruns: {}   group faults: {}",
                    stats.xruns,
                    stats.group_faults.len()
                ));

                // Meters animate only while this screen is visible (L3 flow D):
                // egui idles without input, so ask for a ~60 fps repaint here.
                // The cost exists only when the mixer is open — closed / on the
                // group-settings page there's no meter to drive, so no repaint
                // request, and the tray-only idle footprint (N1) is untouched.
                ui.ctx().request_repaint_after(Duration::from_millis(16));
            }
            Screen::GroupSettings(name) => {
                // `screen_is_stale` above already guarantees this exists.
                if let Some(group) = snapshot.groups.iter().find(|g| g.name == name) {
                    self.group_settings_page(ui, group, &snapshot.groups);
                }
            }
        });
    }
}

impl SettingsApp {
    /// First-run onboarding (simple-launch.md Flow 2, revised for
    /// process-loopback-capture): no virtual bus to pick anymore — just an
    /// output device and the autostart checkbox. Continue creates one
    /// catch-all group (`match_rules: ["*"]`, L1 capability 2 — everything
    /// unmatched still gets a destination) routed to the picked device, sent
    /// bundled with `SetAutostart` as one `EditStructure` batch. Per-app
    /// rules are refined afterward in the main settings view, not here.
    fn onboarding_panel(&mut self, ui: &mut egui::Ui, devices: &[Endpoint], default_output_name: Option<&str>) {
        // Prefill from the system default once, the first time it's known —
        // never overwrites an in-progress pick on later frames.
        if self.onboarding.output_device.is_empty() {
            if let Some(name) = default_output_name {
                self.onboarding.output_device = name.to_string();
            }
        }

        ui.heading("Welcome to Splitstream");
        ui.label("Splitstream routes each app's audio to whichever output device you choose.");
        ui.separator();

        ui.label("Which device should apps play through?");
        if devices.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 80, 40),
                "No render devices detected. Plug one in, then reopen this window.",
            );
        } else {
            output_device_combo(ui, "onboarding-output", &mut self.onboarding.output_device, devices);
        }

        ui.checkbox(&mut self.onboarding.autostart, "Run Splitstream at logon");

        let can_continue = !self.onboarding.output_device.is_empty();
        if ui.add_enabled(can_continue, egui::Button::new("Continue")).clicked() {
            let group = GroupConfig {
                name: "Main".into(),
                output_device: self.onboarding.output_device.clone(),
                gain: Gain::UNITY,
                follow_master: true,
                match_rules: vec!["*".into()],
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                muted: false,
            };
            self.send(ShellAction::EditStructure(vec![
                ConfigEdit::AddGroup(group),
                ConfigEdit::SetAutostart(self.onboarding.autostart),
            ]));
        }
    }

    /// Master column (`RoughAppUI.png`): name (no gear — nothing left to
    /// hide behind one once mute moves here, responsive-ui-refinement
    /// decision), vertical fader sized to `fader_height`, speaker-icon mute
    /// button directly under the fader, "Routed Apps" footer = the
    /// *unassigned*-session pool (mixer-ui-redesign L2 decision — Master's
    /// footer, not a separate strip). Dropping a chip here unassigns it
    /// (drag-assign target `None`).
    fn master_column(&mut self, ui: &mut egui::Ui, ctx: &MasterColumnCtx) {
        let MasterColumnCtx { snapshot, routes, all_sessions, output_peak, output_names, width, height, query } =
            *ctx;
        ui.group(|ui| {
            ui.set_width(width);
            ui.vertical_centered(|ui| {
                ui.strong("Master Volume");

                if let Some(g) = fader(ui, snapshot.master, "master", height) {
                    self.send(ShellAction::EditParams(vec![ConfigEdit::SetMaster(g)]));
                }

                if speaker_mute_button(ui, snapshot.muted) {
                    self.send(ShellAction::EditParams(vec![ConfigEdit::SetMuted(!snapshot.muted)]));
                }

                // Per-output device meters (level-meters.md): one row per
                // distinct output device, labeled by name (positional against
                // `EngineStats::output_peak`). These read silent under master
                // mute — the group meters above stay live (see mixer.rs).
                if !output_peak.is_empty() {
                    ui.separator();
                    ui.label("Outputs");
                    let dt = ui.input(|i| i.stable_dt);
                    for (id, level) in output_peak {
                        let name = output_names
                            .iter()
                            .find(|(oid, _)| oid == id)
                            .map(|(_, n)| n.as_str())
                            .unwrap_or("output");
                        let hold = self.holds.entry(format!("out:{name}")).or_default();
                        output_meter_row(ui, name, *level, width * 0.6, hold, dt);
                    }
                }

                ui.label("Routed Apps");
                let unassigned = unassigned_sessions(all_sessions, routes);
                let zone_ctx = ChipZoneCtx {
                    sessions: &unassigned,
                    query,
                    zone: ZoneKind::Unassigned,
                    current_group: None,
                    groups: &snapshot.groups,
                    any_sessions: !all_sessions.is_empty(),
                };
                match session_drop_zone(ui, &zone_ctx) {
                    Some(ChipAction::Dropped(pid)) => self.handle_drop(pid, None, all_sessions, &snapshot.groups),
                    Some(ChipAction::Assign { pid, target }) => {
                        self.handle_drop(pid, target.as_deref(), all_sessions, &snapshot.groups)
                    }
                    None => {}
                }
            });
        });
    }

    /// One group's column: name + gear (navigates to that group's full
    /// settings page — responsive-ui-refinement L3 flow B, no longer an
    /// inline expand), dropdown-sourced output device, vertical fader sized
    /// to `ctx.fader_height`, "Routed Apps" footer as a drop zone
    /// (mixer-ui-redesign L2/L3).
    fn group_column(&mut self, ui: &mut egui::Ui, group: &GroupConfig, id: GroupId, ctx: &GroupColumnCtx) {
        let GroupColumnCtx { routes, all_sessions, all_groups, devices, group_peak, width, fader_height, query } =
            *ctx;
        let name = group.name.clone();

        // Solo scope is global (decision 3): `self.soloed` is the one set for
        // every column, not per-output. Silenced-by-someone-else's-solo is
        // its own dim state, distinct from this group's own (lit) mute.
        let solo_active = !self.soloed.is_empty();
        let is_soloed = self.soloed.contains(&name);
        let dimmed_by_other_solo = is_dimmed_by_other_solo(solo_active, is_soloed, group.muted);

        ui.group(|ui| {
            ui.set_width(width);
            if dimmed_by_other_solo {
                ui.multiply_opacity(0.5);
            }
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    ui.strong(&group.name);
                    if ui.small_button("⚙").clicked() {
                        self.screen = Screen::GroupSettings(name.clone());
                    }
                });

                let mut output_choice = group.output_device.clone();
                if output_device_combo(ui, &format!("output-{name}"), &mut output_choice, devices) {
                    self.send(ShellAction::EditStructure(vec![ConfigEdit::SetGroupOutput(
                        name.clone(),
                        output_choice,
                    )]));
                }

                // Fader + its post-fader level meter + M/S toggles
                // (per-group-mute-solo.md), side by side (level-meters.md).
                ui.horizontal(|ui| {
                    if let Some(g) = fader(ui, group.gain, &name, fader_height) {
                        self.send(ShellAction::EditParams(vec![ConfigEdit::SetGroupGain(name.clone(), g)]));
                    }
                    let dt = ui.input(|i| i.stable_dt);
                    let level = peak_for(group_peak, id);
                    let hold = self.holds.entry(format!("grp:{name}")).or_default();
                    level_meter(ui, level, fader_height, hold, dt);

                    ui.vertical(|ui| {
                        let mute_color = ui.visuals().error_fg_color;
                        if toggle_button(ui, "M", group.muted, mute_color) {
                            self.send(ShellAction::EditParams(vec![ConfigEdit::SetGroupMute(
                                name.clone(),
                                !group.muted,
                            )]));
                        }
                        let solo_color = ui.visuals().warn_fg_color;
                        if toggle_button(ui, "S", is_soloed, solo_color) {
                            let on = !is_soloed;
                            if on {
                                self.soloed.insert(name.clone());
                            } else {
                                self.soloed.remove(&name);
                            }
                            self.send(ShellAction::SetSolo(name.clone(), on));
                        }
                    });
                });

                ui.label("Routed Apps");
                let sessions = routed_sessions(routes, id);
                let zone_ctx = ChipZoneCtx {
                    sessions: &sessions,
                    query,
                    zone: ZoneKind::Group,
                    current_group: Some(&name),
                    groups: all_groups,
                    any_sessions: !all_sessions.is_empty(),
                };
                match session_drop_zone(ui, &zone_ctx) {
                    Some(ChipAction::Dropped(pid)) => self.handle_drop(pid, Some(&name), all_sessions, all_groups),
                    Some(ChipAction::Assign { pid, target }) => {
                        self.handle_drop(pid, target.as_deref(), all_sessions, all_groups)
                    }
                    None => {}
                }
            });
        });
    }

    /// Resolves a dropped chip's pid to its process file name and sends the
    /// resulting `ConfigEdit::SetRules` batch — shared by the master
    /// (unassign, `target: None`) and every group column (assign) drop zone.
    fn handle_drop(&self, pid: u32, target: Option<&str>, all_sessions: &[SessionInfo], groups: &[GroupConfig]) {
        let Some(info) = all_sessions.iter().find(|s| s.pid == pid) else {
            return;
        };
        let file_name = session_file_name(info);
        let edits = resolve_drag_assign(&file_name, target, groups);
        if !edits.is_empty() {
            self.send(ShellAction::EditParams(edits));
        }
    }

    /// Group settings page (responsive-ui-refinement L4, revised from
    /// mixer-ui-redesign's inline `group_advanced_panel`): follow-master,
    /// spatial, DSP chain, duck sidechain, match-rules text fallback,
    /// remove-group — same content, now a full-width page reached via the
    /// column's gear icon (L3 flow B) instead of an inline expand. Back
    /// button returns to the mixer (L3 flow C).
    fn group_settings_page(&mut self, ui: &mut egui::Ui, group: &GroupConfig, all_groups: &[GroupConfig]) {
        let name = group.name.clone();

        ui.horizontal(|ui| {
            if ui.button("⬅ Back").clicked() {
                self.screen = Screen::Mixer;
            }
            ui.heading(format!("{name} Settings"));
        });
        ui.separator();

        let mut follow = group.follow_master;
        if ui.checkbox(&mut follow, "Follow master").changed() {
            self.send(ShellAction::EditParams(vec![ConfigEdit::SetFollowMaster(name.clone(), follow)]));
        }

        let mut spatial = group.spatial;
        if ui.checkbox(&mut spatial, "Spatial audio").changed() {
            self.send(ShellAction::EditSpatial(vec![ConfigEdit::SetSpatial(name.clone(), spatial)]));
        }

        self.draft_for(group);
        let mut rules_draft = self.drafts[&name].match_rules.clone();
        ui.horizontal(|ui| {
            ui.label("Match rules (fallback, glob patterns):");
            ui.text_edit_singleline(&mut rules_draft);
            if ui.button("Save rules").clicked() {
                let rules = split_rules(&rules_draft);
                self.send(ShellAction::EditParams(vec![ConfigEdit::SetRules(name.clone(), rules)]));
            }
        });
        if let Some(draft) = self.drafts.get_mut(&name) {
            draft.match_rules = rules_draft;
        }

        self.dsp_controls(ui, group);
        self.duck_controls(ui, group, all_groups);

        if ui.button("Remove group").clicked() {
            self.send(ShellAction::EditStructure(vec![ConfigEdit::RemoveGroup(name.clone())]));
            // L3 flow D — navigate back immediately in the same click rather
            // than waiting for next frame's stale-screen fallback to catch it.
            self.screen = Screen::Mixer;
        }
    }

    /// Per-group DSP chain: one row per configured stage (live param
    /// sliders + bypass + remove), plus add-stage buttons. EQ stages start
    /// with a single band on creation — a new band's params, once added,
    /// can only be retuned via `SetEqBand`, not created independently (no
    /// `AddEqBand` edit exists; matches the contract's edit set as written).
    fn dsp_controls(&self, ui: &mut egui::Ui, group: &GroupConfig) {
        let name = group.name.clone();
        ui.collapsing("DSP", |ui| {
            for (stage_idx, stage) in group.dsp.iter().enumerate() {
                ui.horizontal(|ui| {
                    match &stage.spec {
                        DspSpec::Eq { bands } => {
                            ui.label(format!("EQ #{stage_idx}"));
                            if let Some(band) = bands.first() {
                                let mut freq = band.freq_hz;
                                let mut gain_db = band.gain_db;
                                let mut q = band.q;
                                let mut changed = false;
                                changed |= ui
                                    .add(egui::Slider::new(&mut freq, 20.0..=20_000.0).logarithmic(true).text("Hz"))
                                    .changed();
                                changed |= ui.add(egui::Slider::new(&mut gain_db, -24.0..=24.0).text("dB")).changed();
                                changed |= ui.add(egui::Slider::new(&mut q, 0.1..=10.0).text("Q")).changed();
                                if changed {
                                    self.send(ShellAction::EditParams(vec![ConfigEdit::SetEqBand(
                                        name.clone(),
                                        0,
                                        EqBandSpec { freq_hz: freq, gain_db, q },
                                    )]));
                                }
                            }
                        }
                        DspSpec::Limiter { ceiling_db } => {
                            ui.label(format!("Limiter #{stage_idx}"));
                            let mut ceiling = *ceiling_db;
                            if ui.add(egui::Slider::new(&mut ceiling, -24.0..=0.0).text("Ceiling dB")).changed() {
                                self.send(ShellAction::EditParams(vec![ConfigEdit::SetLimiterCeiling(
                                    name.clone(),
                                    ceiling,
                                )]));
                            }
                        }
                    }

                    let mut bypassed = stage.bypassed;
                    if ui.checkbox(&mut bypassed, "Bypass").changed() {
                        self.send(ShellAction::EditParams(vec![ConfigEdit::SetDspBypass(
                            name.clone(),
                            stage_idx,
                            bypassed,
                        )]));
                    }
                    if ui.button("Remove").clicked() {
                        self.send(ShellAction::EditDspChains(vec![ConfigEdit::RemoveDspStage(
                            name.clone(),
                            stage_idx,
                        )]));
                    }
                });
            }

            ui.horizontal(|ui| {
                if ui.button("Add EQ").clicked() {
                    self.send(ShellAction::EditDspChains(vec![ConfigEdit::AddDspStage(
                        name.clone(),
                        DspSpec::Eq {
                            bands: vec![EqBandSpec { freq_hz: 1000.0, gain_db: 0.0, q: 0.7 }],
                        },
                    )]));
                }
                if ui.button("Add Limiter").clicked() {
                    self.send(ShellAction::EditDspChains(vec![ConfigEdit::AddDspStage(
                        name.clone(),
                        DspSpec::Limiter { ceiling_db: -1.0 },
                    )]));
                }
            });
        });
    }

    /// Cross-group sidechain: trigger group picker (dropdown, sourced from
    /// `all_groups` minus this one — mixer-ui-redesign L4) plus
    /// amount/threshold/attack/release sliders, shown only while a duck is
    /// configured. The trigger dropdown commits exactly like the sliders
    /// (part of the same `changed` aggregate) — no separate draft, no "Set"
    /// button, since a selection is a discrete commit with nothing
    /// in-progress to protect. Enabling seeds a fresh `DuckSpecConfig` with
    /// the first other group's name as its trigger (user decision,
    /// mixer-ui-redesign implementation) — the dropdown then lets the user
    /// change it immediately if that default isn't the intended target.
    fn duck_controls(&self, ui: &mut egui::Ui, group: &GroupConfig, all_groups: &[GroupConfig]) {
        let name = group.name.clone();

        ui.collapsing("Duck (sidechain)", |ui| {
            let mut enabled = group.duck.is_some();
            if ui.checkbox(&mut enabled, "Enabled").changed() {
                let duck = enabled.then(|| {
                    group.duck.clone().unwrap_or_else(|| DuckSpecConfig {
                        trigger: default_duck_trigger(all_groups, &name),
                        amount_db: 6.0,
                        threshold_db: -30.0,
                        attack_ms: 5.0,
                        release_ms: 200.0,
                    })
                });
                self.send(ShellAction::EditParams(vec![ConfigEdit::SetDuck(name.clone(), duck)]));
            }

            if let Some(duck) = &group.duck {
                let mut trigger = duck.trigger.clone();
                let mut amount_db = duck.amount_db;
                let mut threshold_db = duck.threshold_db;
                let mut attack_ms = duck.attack_ms;
                let mut release_ms = duck.release_ms;
                let mut changed = false;

                ui.horizontal(|ui| {
                    ui.label("Trigger group:");
                    changed |= duck_trigger_combo(ui, &mut trigger, all_groups, &name);
                });
                changed |= ui.add(egui::Slider::new(&mut amount_db, 0.0..=24.0).text("Amount dB")).changed();
                changed |= ui
                    .add(egui::Slider::new(&mut threshold_db, -60.0..=0.0).text("Threshold dB"))
                    .changed();
                changed |= ui.add(egui::Slider::new(&mut attack_ms, 1.0..=200.0).text("Attack ms")).changed();
                changed |= ui.add(egui::Slider::new(&mut release_ms, 10.0..=1000.0).text("Release ms")).changed();

                if changed {
                    self.send(ShellAction::EditParams(vec![ConfigEdit::SetDuck(
                        name.clone(),
                        Some(DuckSpecConfig { trigger, amount_db, threshold_db, attack_ms, release_ms }),
                    )]));
                }
            }
        });
    }

    /// "Create New Audio Source" panel (mockup: floating "+"). Output is
    /// dropdown-sourced like every existing group's (mixer-ui-redesign L1
    /// capability 4) — otherwise unchanged from before.
    fn add_group_controls(&mut self, ui: &mut egui::Ui, devices: &[Endpoint]) {
        ui.heading("Create New Audio Source");
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.text_edit_singleline(&mut self.new_group.name);
            output_device_combo(ui, "new-group-output", &mut self.new_group.output_device, devices);

            if ui.button("Create").clicked() && !self.new_group.name.trim().is_empty() {
                let group = GroupConfig {
                    name: self.new_group.name.trim().to_string(),
                    output_device: self.new_group.output_device.trim().to_string(),
                    gain: Gain::UNITY,
                    follow_master: true,
                    match_rules: vec![],
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                    muted: false,
                };
                self.send(ShellAction::EditStructure(vec![ConfigEdit::AddGroup(group)]));
                self.new_group = NewGroupDraft::default();
                self.show_new_group_panel = false;
            }
        });
    }
}

/// Fixed on-screen width of a vertical level meter's bar (level-meters.md) —
/// narrow, sits right beside a fader.
const METER_WIDTH: f32 = 12.0;
/// Bottom of the meter's dB scale: signal at or below this reads as an empty
/// bar. −60 dBFS keeps quiet-but-present signal visible without the bar looking
/// alive on pure noise.
const METER_FLOOR_DB: f32 = -60.0;
/// How fast the peak-hold marker falls, in bar-fraction per second. Slow enough
/// to catch a transient by eye, fast enough to track a dropping signal.
const HOLD_FALL_PER_S: f32 = 0.5;

const METER_GREEN: egui::Color32 = egui::Color32::from_rgb(60, 180, 90);
const METER_AMBER: egui::Color32 = egui::Color32::from_rgb(220, 170, 40);
const METER_RED: egui::Color32 = egui::Color32::from_rgb(220, 70, 45);

/// Per-meter peak-hold marker state (level-meters.md) — UI-only, no domain
/// footprint. `value` is the held bar fraction (0..1); it snaps up to a new
/// peak and decays in frame time. `Default` = rest at the floor.
#[derive(Default)]
struct HoldDot {
    value: f32,
}

/// Maps a linear peak to a 0..1 bar fraction on the dBFS scale (floor
/// [`METER_FLOOR_DB`], top 0 dBFS). Pure.
fn meter_fraction(peak: f32) -> f32 {
    if peak <= 1.0e-6 {
        return 0.0;
    }
    let db = 20.0 * peak.log10();
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

/// Fill color by how hot the bar is — green normal, amber approaching, red near
/// full scale. Pure.
fn meter_color(fraction: f32) -> egui::Color32 {
    if fraction > 0.9 {
        METER_RED
    } else if fraction > 0.75 {
        METER_AMBER
    } else {
        METER_GREEN
    }
}

/// Advances a hold marker one frame: snaps up to `fraction`, else falls at
/// [`HOLD_FALL_PER_S`]. Returns the new held value. Pure.
fn advance_hold(previous: f32, fraction: f32, dt: f32) -> f32 {
    (previous - HOLD_FALL_PER_S * dt).max(fraction)
}

/// dBFS label for a meter's hover text, or `-inf` at silence. Pure.
fn peak_db_label(level: MeterLevel) -> String {
    let db = if level.peak <= 1.0e-6 { f32::NEG_INFINITY } else { 20.0 * level.peak.log10() };
    let clip = if level.clipped { " • clip" } else { "" };
    if db.is_finite() {
        format!("{db:.1} dBFS{clip}")
    } else {
        format!("-inf dBFS{clip}")
    }
}

/// This frame's meter reading for `id` (level-meters.md). Unknown / not-yet-
/// reported id reads `SILENT`, so a freshly-added group shows an empty bar
/// rather than nothing.
fn peak_for(peaks: &[(GroupId, MeterLevel)], id: GroupId) -> MeterLevel {
    peaks.iter().find(|(g, _)| *g == id).map(|(_, m)| *m).unwrap_or(MeterLevel::SILENT)
}

/// Bottom of fader travel (db-faders.md). Matches [`METER_FLOOR_DB`], so the
/// fader and the level meter beside it share one scale.
const FADER_MIN_DB: f32 = -60.0;
/// Top of fader travel -- the last 6 dB is boost above unity (decision 2).
const FADER_MAX_DB: f32 = 6.0;

/// Fader position (dB) -> `Gain`. At or below [`FADER_MIN_DB`] the result is
/// `Gain::SILENT`, not −60 dB (decision 3): pulling a fader down means off,
/// not quiet. The input range guarantees a finite non-negative linear value,
/// so `Gain::new` cannot fail here.
fn fader_db_to_gain(db: f32) -> Gain {
    if db <= FADER_MIN_DB {
        Gain::SILENT
    } else {
        Gain::new(db_to_linear(db)).expect("db_to_linear is finite and non-negative")
    }
}

/// `Gain` -> fader position (dB). `Gain::SILENT` maps to `NEG_INFINITY`, which
/// the slider clamps to the bottom of travel while the readout shows `-inf dB`.
/// Deliberately *not* clamped to the fader range here: an out-of-range
/// existing value (e.g. a hand-written `gain = 4.0`, +12 dB) must reach the
/// slider unclamped so `SliderClamping::Edits` can display it truthfully
/// instead of silently squashing it on mere render (db-faders.md decision 10).
fn gain_to_fader_db(gain: Gain) -> f32 {
    linear_to_db(gain.value())
}

/// Value-box text for a dB reading -- one decimal, explicit sign for boost,
/// `-inf dB` at silence.
fn format_fader_db(db: f64) -> String {
    if db <= FADER_MIN_DB as f64 {
        "-inf dB".to_string()
    } else if db > 0.0 {
        format!("+{db:.1} dB")
    } else {
        format!("{db:.1} dB")
    }
}

/// Parses typed dB back to a number. Accepts `-6`, `-6.0`, `+3`, `0`, an
/// optional `dB`/`db` suffix, and `-inf`/`-∞`. `None` on anything else, which
/// egui treats as "keep the previous value".
fn parse_fader_db(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    let without_suffix = trimmed
        .strip_suffix("dB")
        .or_else(|| trimmed.strip_suffix("db"))
        .unwrap_or(trimmed)
        .trim();
    match without_suffix {
        "-inf" | "-∞" => Some(f64::from(FADER_MIN_DB)),
        other => other.parse::<f64>().ok(),
    }
}

/// One fader: dB-scaled vertical slider, editable dB value box, and
/// double-click-to-unity. `Some` only when this frame changed the value.
/// `id_salt` distinguishes the reset overlay per fader (group name, or
/// "master"); `length` is the slider's long axis. Shared by `master_column`
/// and `group_column` so the two cannot drift apart in scale, floor, or
/// rounding (db-faders.md decision 8).
fn fader(ui: &mut egui::Ui, gain: Gain, id_salt: &str, length: f32) -> Option<Gain> {
    let mut db = gain_to_fader_db(gain);
    ui.spacing_mut().slider_width = length;
    let response = ui.add(
        egui::Slider::new(&mut db, FADER_MIN_DB..=FADER_MAX_DB)
            .vertical()
            .clamping(egui::SliderClamping::Edits)
            .custom_formatter(|v, _| format_fader_db(v))
            .custom_parser(parse_fader_db)
            .drag_value_speed(0.1),
    );

    let reset = ui.interact(response.rect, ui.id().with(("fader-reset", id_salt)), egui::Sense::click());
    if reset.double_clicked() {
        return Some(Gain::UNITY);
    }
    if response.changed() {
        return Some(fader_db_to_gain(db));
    }
    None
}

/// Paints a peak meter into an already-allocated `rect` (level-meters.md):
/// dB-scaled fill, zone color, peak-hold marker, red clip cap, dBFS hover.
/// Custom paint (like [`speaker_mute_button`]) so it renders identically in any
/// theme and carries no glyph-font risk. `vertical` picks the fill/marker/cap
/// axis — the *only* thing that differs between the fader meter and the device
/// row; every other rule lives here once so the two can't drift. `hold` is this
/// meter's persistent marker state, `dt` the frame delta driving its fall.
fn paint_meter(
    ui: &egui::Ui,
    rect: egui::Rect,
    response: egui::Response,
    level: MeterLevel,
    hold: &mut HoldDot,
    dt: f32,
    vertical: bool,
) {
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);

    let fraction = meter_fraction(level.peak);
    hold.value = advance_hold(hold.value, fraction, dt);

    if fraction > 0.0 {
        let fill = if vertical {
            let fill_top = rect.max.y - rect.height() * fraction;
            egui::Rect::from_min_max(egui::pos2(rect.min.x, fill_top), rect.max)
        } else {
            let fill_right = rect.min.x + rect.width() * fraction;
            egui::Rect::from_min_max(rect.min, egui::pos2(fill_right, rect.max.y))
        };
        painter.rect_filled(fill, 2.0, meter_color(fraction));
    }

    if hold.value > 0.0 {
        let held = hold.value.min(1.0);
        let stroke = egui::Stroke::new(1.5, ui.visuals().strong_text_color());
        if vertical {
            let y = rect.max.y - rect.height() * held;
            painter.hline(rect.x_range(), y, stroke);
        } else {
            let x = rect.min.x + rect.width() * held;
            painter.vline(x, rect.y_range(), stroke);
        }
    }

    if level.clipped {
        let cap = if vertical {
            egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.min.y + 3.0))
        } else {
            egui::Rect::from_min_max(egui::pos2(rect.max.x - 3.0, rect.min.y), rect.max)
        };
        painter.rect_filled(cap, 1.0, METER_RED);
    }

    response.on_hover_text(peak_db_label(level));
}

/// Vertical peak meter beside a fader (level-meters.md): dB-scaled fill,
/// zone-colored, peak-hold marker, red clip cap. Delegates the paint to
/// [`paint_meter`] (vertical axis); only the fixed narrow width is meter-
/// specific here.
fn level_meter(ui: &mut egui::Ui, level: MeterLevel, height: f32, hold: &mut HoldDot, dt: f32) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(METER_WIDTH, height), egui::Sense::hover());
    paint_meter(ui, rect, response, level, hold, dt, true);
}

/// Height of a horizontal device-row meter bar (level-meters.md).
const OUTPUT_METER_HEIGHT: f32 = 10.0;

/// A `name  [====  ]` row for the master column's per-output device list
/// (level-meters.md): device name label + a horizontal peak bar. Same scale,
/// coloring, hold, and clip semantics as [`level_meter`] — both share
/// [`paint_meter`] (horizontal axis), laid out so a list of named devices reads
/// cleanly.
fn output_meter_row(ui: &mut egui::Ui, name: &str, level: MeterLevel, width: f32, hold: &mut HoldDot, dt: f32) {
    ui.horizontal(|ui| {
        ui.small(name);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, OUTPUT_METER_HEIGHT), egui::Sense::hover());
        paint_meter(ui, rect, response, level, hold, dt, false);
    });
}

/// Custom-painted speaker icon + click sense (responsive-ui-refinement L4)
/// — cone + volume arcs when `muted` is false, cone + diagonal slash when
/// true. A custom paint avoids the tofu-box risk an emoji-range glyph
/// (🔊/🔇) would carry in egui's default font, unlike the already-proven ⚙
/// (operational learnings). Single-purpose: only Master's mute calls this
/// today, not a generic icon-button abstraction. Returns whether clicked
/// this frame — holds no mute state of its own, caller flips its own bool.
fn speaker_mute_button(ui: &mut egui::Ui, muted: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(SPEAKER_ICON_SIZE, egui::Sense::click());
    let color = if response.hovered() { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
    let stroke = egui::Stroke::new(2.0, color);

    // 24x24 design grid scaled to the allocated (square) rect.
    let scale = rect.width() / 24.0;
    let point = |x: f32, y: f32| rect.min + egui::vec2(x, y) * scale;

    // Speaker cone: rectangular body + trapezoid horn, drawn as two separate
    // convex shapes — `Shape::convex_polygon`'s fill is only correct for
    // convex input (epaint's own doc), and the combined 6-point outline has
    // a reflex vertex at the body/horn seam (x=6), so one polygon call would
    // fill incorrectly there.
    let body = vec![point(0.0, 9.0), point(6.0, 9.0), point(6.0, 15.0), point(0.0, 15.0)];
    let horn = vec![point(6.0, 9.0), point(13.0, 2.0), point(13.0, 22.0), point(6.0, 15.0)];
    ui.painter().add(egui::Shape::convex_polygon(body, color, egui::Stroke::NONE));
    ui.painter().add(egui::Shape::convex_polygon(horn, color, egui::Stroke::NONE));

    if muted {
        ui.painter().line_segment([point(15.0, 5.0), point(22.0, 19.0)], stroke);
    } else {
        let horn_tip = point(13.0, 12.0);
        for radius in [4.0_f32, 7.0] {
            let arc: Vec<egui::Pos2> = (0..=6)
                .map(|i| {
                    let angle = -0.6 + (i as f32 / 6.0) * 1.2; // ~ -34deg..+34deg
                    horn_tip + egui::vec2(angle.cos(), angle.sin()) * radius * scale
                })
                .collect();
            ui.painter().add(egui::Shape::line(arc, stroke));
        }
    }

    response.on_hover_text(if muted { "Unmute" } else { "Mute" }).clicked()
}

/// Plain-letter toggle (M / S) — ASCII only, no glyph-font risk unlike an
/// emoji-range icon; `speaker_mute_button`'s custom paint isn't needed for a
/// letter. Returns whether clicked this frame — holds no state of its own,
/// caller flips its own bool/set.
fn toggle_button(ui: &mut egui::Ui, label: &str, active: bool, tint: egui::Color32) -> bool {
    let text = if active {
        egui::RichText::new(label).strong().color(egui::Color32::WHITE)
    } else {
        egui::RichText::new(label).strong()
    };
    let mut button = egui::Button::new(text);
    if active {
        button = button.fill(tint);
    }
    ui.add(button).clicked()
}

/// Reusable device picker — replaces `text_edit_singleline` at every call
/// site (onboarding, existing-group output, new-group output). Returns true
/// on selection change; `id_source` must be unique among all comboboxes
/// rendered in the same frame.
fn output_device_combo(ui: &mut egui::Ui, id_source: &str, current: &mut String, devices: &[Endpoint]) -> bool {
    let mut changed = false;
    let selected_text = if current.is_empty() { "Select a device..." } else { current.as_str() };
    egui::ComboBox::from_id_salt(id_source)
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            for device in devices {
                if ui.selectable_value(current, device.name.clone(), &device.name).changed() {
                    changed = true;
                }
            }
        });
    changed
}

/// Reusable duck-trigger picker — other group names, `exclude` omits the
/// owning group itself from its own trigger choices (a group can't sidechain
/// off itself).
fn duck_trigger_combo(ui: &mut egui::Ui, current: &mut String, groups: &[GroupConfig], exclude: &str) -> bool {
    let mut changed = false;
    let selected_text = if current.is_empty() { "Select a group..." } else { current.as_str() };
    egui::ComboBox::from_id_salt(("duck-trigger", exclude))
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            for g in groups.iter().filter(|g| g.name != exclude) {
                if ui.selectable_value(current, g.name.clone(), &g.name).changed() {
                    changed = true;
                }
            }
        });
    changed
}

/// Default trigger seeded when a duck is first enabled — the first other
/// group in config order, or empty (no valid target yet) if `exclude` is the
/// only group. User decision (mixer-ui-redesign implementation, 2026-07-22).
fn default_duck_trigger(groups: &[GroupConfig], exclude: &str) -> String {
    groups
        .iter()
        .find(|g| g.name != exclude)
        .map(|g| g.name.clone())
        .unwrap_or_default()
}

/// What a chip's context menu produced.
enum AssignChoice {
    To(String),
    Unassign,
}

/// What a chip zone produced this frame. Both variants funnel into
/// `handle_drop` at the call site (decision 7) — the two gestures physically
/// cannot produce different edits.
enum ChipAction {
    /// A chip was released on this zone — target is the zone's own identity.
    Dropped(u32),
    /// A menu choice on a chip — target is explicit and may be any group.
    Assign { pid: u32, target: Option<String> },
}

/// Read-only data a chip zone needs to filter, choose its empty state, and
/// build the assign menu (session-search-and-guidance.md).
#[derive(Clone, Copy)]
struct ChipZoneCtx<'a> {
    sessions: &'a [SessionInfo],
    query: &'a str,
    zone: ZoneKind,
    /// The group this zone belongs to; `None` for the master (unassigned) pool.
    current_group: Option<&'a str>,
    /// Every configured group, for the assign menu.
    groups: &'a [GroupConfig],
    /// Whether any session exists anywhere — drives `NothingPlaying`.
    any_sessions: bool,
}

/// Free-text filter plus clear button; `true` when the query changed this
/// frame. Esc also clears. Caller renders this only when at least one session
/// exists (decision 4) — with nothing playing, a search box is furniture.
fn search_box(ui: &mut egui::Ui, query: &mut String) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(query)
                .id(egui::Id::new("session-search"))
                .hint_text("Search apps…"),
        );
        if response.changed() {
            changed = true;
        }
        // `has_focus()` reads false here, not true: egui's `Memory::begin_frame`
        // clears focus globally on an unclaimed Escape *before* this widget
        // renders (memory/mod.rs), so the focus loss and the Escape keypress
        // land in the same frame. `lost_focus()` is the one that's true on
        // exactly that frame -- confirmed empirically, not assumed.
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            query.clear();
            changed = true;
        }
        if !query.is_empty() && ui.button("✕").clicked() {
            query.clear();
            changed = true;
        }
    });
    changed
}

/// The chip's own click-sensing interaction (decision 3): `dnd_drag_source`'s
/// response senses drag only, and `Response::context_menu` opens on
/// `secondary_clicked()`, which needs click sense — the same dead-gesture
/// trap as the fader's `double_clicked()` in db-faders.md, in a different
/// widget. Built on `rect`, never on the drag-source response itself.
fn chip_context_menu(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id: egui::Id,
    groups: &[GroupConfig],
    current_group: Option<&str>,
) -> Option<AssignChoice> {
    let menu = ui.interact(rect, id.with("menu"), egui::Sense::click());
    let mut choice = None;
    menu.context_menu(|ui| {
        for g in groups {
            if Some(g.name.as_str()) == current_group {
                ui.label(format!("{} (current)", g.name));
            } else if ui.button(&g.name).clicked() {
                choice = Some(AssignChoice::To(g.name.clone()));
            }
        }
        if current_group.is_some() && ui.button("Unassign").clicked() {
            choice = Some(AssignChoice::Unassign);
        }
    });
    choice
}

/// Renders a chip zone: filters `ctx.sessions` against `ctx.query`, shows the
/// matching empty state when nothing remains, and returns whichever gesture —
/// drop or menu assign — fired this frame (decision 7: both collapse onto the
/// same `handle_drop` at the call site, so they cannot diverge).
fn session_drop_zone(ui: &mut egui::Ui, ctx: &ChipZoneCtx) -> Option<ChipAction> {
    let zone_had_chips = !ctx.sessions.is_empty();
    let shown: Vec<&SessionInfo> = ctx.sessions.iter().filter(|s| session_matches(s, ctx.query)).collect();

    let frame = egui::Frame::group(ui.style());
    let mut action = None;
    let (_, dropped) = ui.dnd_drop_zone::<DragSession, ()>(frame, |ui| {
        if shown.is_empty() {
            let reason = empty_reason(ctx.zone, ctx.any_sessions, zone_had_chips, !ctx.query.is_empty());
            ui.weak(empty_message(reason, ctx.query));
        }
        for session in &shown {
            let id = egui::Id::new(("session-chip", session.pid));
            let response = ui
                .dnd_drag_source(id, DragSession(session.pid), |ui| {
                    ui.label(chip_label(session));
                })
                .response;
            if let Some(choice) = chip_context_menu(ui, response.rect, id, ctx.groups, ctx.current_group) {
                let target = match choice {
                    AssignChoice::To(name) => Some(name),
                    AssignChoice::Unassign => None,
                };
                action = Some(ChipAction::Assign { pid: session.pid, target });
            }
        }
    });
    if let Some(payload) = dropped {
        return Some(ChipAction::Dropped(payload.0));
    }
    action
}

/// Pure — every session currently routed to `group`, sorted by chip label
/// (not raw `display_name` — usually empty, see `chip_label`'s doc) for a
/// stable render order (`routes` is grouping-order from a `HashMap`
/// upstream, not display order).
fn routed_sessions(routes: &[(GroupId, Vec<SessionInfo>)], group: GroupId) -> Vec<SessionInfo> {
    let mut sessions: Vec<SessionInfo> = routes
        .iter()
        .find(|(g, _)| *g == group)
        .map(|(_, sessions)| sessions.clone())
        .unwrap_or_default();
    sessions.sort_by_key(chip_label);
    sessions
}

/// Pure — every live session not currently routed to any group (Master's
/// footer, mixer-ui-redesign L3 flow A).
fn unassigned_sessions(all: &[SessionInfo], routes: &[(GroupId, Vec<SessionInfo>)]) -> Vec<SessionInfo> {
    let routed_pids: std::collections::HashSet<u32> =
        routes.iter().flat_map(|(_, sessions)| sessions.iter().map(|s| s.pid)).collect();
    all.iter().filter(|s| !routed_pids.contains(&s.pid)).cloned().collect()
}

/// Pure — the process image file name `match_session` itself matches
/// against, extracted the same way (engine::rules::match_session).
fn session_file_name(info: &SessionInfo) -> String {
    info.process_path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string()
}

/// Pure — the chip's visible label. `SessionInfo.display_name` comes
/// straight from WASAPI's `IAudioSessionControl::GetDisplayName()`
/// (win-audio's `describe_session`), which most real apps never set — it's
/// commonly empty, not just occasionally (caught live: chips rendered
/// draggable but blank for real sessions). Falls back to the process file
/// name, then the bare pid, so a chip is never unlabeled.
fn chip_label(session: &SessionInfo) -> String {
    if !session.display_name.trim().is_empty() {
        return session.display_name.clone();
    }
    let file_name = session_file_name(session);
    if !file_name.is_empty() {
        return file_name;
    }
    session.pid.to_string()
}

/// Which chip zone this is (session-search-and-guidance.md) — decides which
/// empty state applies when the zone's displayed list is empty.
#[derive(Clone, Copy, PartialEq)]
enum ZoneKind {
    Unassigned,
    Group,
}

/// Why a chip zone is showing nothing.
#[derive(Clone, Copy, PartialEq, Debug)]
enum EmptyReason {
    NothingPlaying,
    AllRouted,
    GroupEmpty,
    NoMatches,
}

/// Pure. Called only when the zone's *displayed* list is empty, so it always
/// has an answer. `zone_had_chips` is pre-filter occupancy — it is what
/// separates "the search hid everything" from "this zone was always empty"
/// (decision 8): without it, a group that was already empty would blame the
/// search for an emptiness it did not cause.
fn empty_reason(zone: ZoneKind, any_sessions: bool, zone_had_chips: bool, searching: bool) -> EmptyReason {
    if !any_sessions {
        EmptyReason::NothingPlaying
    } else if searching && zone_had_chips {
        EmptyReason::NoMatches
    } else if zone == ZoneKind::Unassigned {
        EmptyReason::AllRouted
    } else {
        EmptyReason::GroupEmpty
    }
}

/// Text for an empty zone. `query` is only read for `NoMatches`.
fn empty_message(reason: EmptyReason, query: &str) -> String {
    match reason {
        EmptyReason::NothingPlaying => "No apps are playing audio.".to_string(),
        EmptyReason::AllRouted => "All apps are routed.".to_string(),
        EmptyReason::GroupEmpty => "Drag an app here, or right-click an app to assign it.".to_string(),
        EmptyReason::NoMatches => format!("No apps match \"{query}\"."),
    }
}

/// Pure. Case-insensitive substring over the chip label *and* the process
/// file name (capability 2) — so `chrome` finds a chip labelled either
/// `Google Chrome` or `chrome.exe`. An empty query matches everything.
fn session_matches(session: &SessionInfo, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    chip_label(session).to_lowercase().contains(&query) || session_file_name(session).to_lowercase().contains(&query)
}

/// Pure — resolves a drag-drop onto `target` (`Some(group name)` = assign,
/// `None` = drop on Master = unassign) into the minimal `ConfigEdit::SetRules`
/// batch: target gains an `ExactName(session_file_name)` entry if it doesn't
/// already have one, every other group loses any `ExactName` entry equal to
/// it (case-insensitive, matching `match_session`'s own comparison). Glob
/// rules are never touched — only exact assignments are drag-managed.
/// Groups whose rules don't actually change are omitted from the batch.
fn resolve_drag_assign(session_file_name: &str, target: Option<&str>, groups: &[GroupConfig]) -> Vec<ConfigEdit> {
    let mut edits = Vec::new();
    for g in groups {
        let is_target = target == Some(g.name.as_str());
        let has_exact = g.match_rules.iter().any(|r| is_exact_match_for(r, session_file_name));
        match (is_target, has_exact) {
            (true, false) => {
                let mut rules = g.match_rules.clone();
                rules.push(session_file_name.to_string());
                edits.push(ConfigEdit::SetRules(g.name.clone(), rules));
            }
            (false, true) => {
                let rules: Vec<String> =
                    g.match_rules.iter().filter(|r| !is_exact_match_for(r, session_file_name)).cloned().collect();
                edits.push(ConfigEdit::SetRules(g.name.clone(), rules));
            }
            _ => {}
        }
    }
    edits
}

fn is_exact_match_for(rule: &str, session_file_name: &str) -> bool {
    match MatchRule::parse(rule) {
        MatchRule::ExactName(name) => name.eq_ignore_ascii_case(session_file_name),
        MatchRule::Glob(_) => false,
    }
}

/// Pure — comma-separated draft text -> trimmed, non-empty rule strings.
fn split_rules(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pid: u32, name: &str) -> SessionInfo {
        SessionInfo {
            pid,
            process_path: name.into(),
            display_name: name.into(),
        }
    }

    fn group(name: &str, rules: &[&str]) -> GroupConfig {
        GroupConfig {
            name: name.into(),
            output_device: "Out".into(),
            gain: Gain::UNITY,
            follow_master: true,
            match_rules: rules.iter().map(|r| r.to_string()).collect(),
            dsp: Vec::new(),
            duck: None,
            spatial: false,
            muted: false,
        }
    }

    #[test]
    fn routed_sessions_returns_sorted_sessions_for_the_matching_group() {
        let routes = vec![(GroupId(0), vec![session(1, "b.exe"), session(2, "a.exe")])];
        assert_eq!(
            routed_sessions(&routes, GroupId(0)),
            vec![session(2, "a.exe"), session(1, "b.exe")]
        );
    }

    #[test]
    fn routed_sessions_is_empty_for_a_group_with_no_entry() {
        let routes = vec![(GroupId(0), vec![session(1, "a.exe")])];
        assert!(routed_sessions(&routes, GroupId(1)).is_empty());
    }

    #[test]
    fn unassigned_sessions_excludes_every_pid_present_in_any_route() {
        let all = vec![session(1, "a.exe"), session(2, "b.exe"), session(3, "c.exe")];
        let routes = vec![
            (GroupId(0), vec![session(1, "a.exe")]),
            (GroupId(1), vec![session(3, "c.exe")]),
        ];
        assert_eq!(unassigned_sessions(&all, &routes), vec![session(2, "b.exe")]);
    }

    #[test]
    fn unassigned_sessions_is_everything_when_nothing_is_routed() {
        let all = vec![session(1, "a.exe")];
        assert_eq!(unassigned_sessions(&all, &[]), all);
    }

    #[test]
    fn split_rules_trims_and_drops_empty_entries() {
        assert_eq!(
            split_rules("game.exe,  *steam*, , music.exe "),
            vec!["game.exe".to_string(), "*steam*".to_string(), "music.exe".to_string()]
        );
    }

    #[test]
    fn split_rules_of_blank_text_is_empty() {
        assert!(split_rules("   ").is_empty());
    }

    #[test]
    fn column_width_divides_available_space_evenly_within_the_clamp_range() {
        assert_eq!(column_width(600.0, 3), 200.0);
    }

    #[test]
    fn column_width_never_shrinks_below_the_minimum() {
        assert_eq!(column_width(90.0, 3), MIN_COLUMN_WIDTH);
    }

    #[test]
    fn column_width_never_grows_above_the_maximum() {
        assert_eq!(column_width(1000.0, 1), MAX_COLUMN_WIDTH);
    }

    #[test]
    fn column_width_treats_zero_columns_as_one() {
        assert_eq!(column_width(150.0, 0), column_width(150.0, 1));
    }

    #[test]
    fn fader_height_passes_through_within_the_clamp_range() {
        assert_eq!(fader_height(250.0), 250.0);
    }

    #[test]
    fn fader_height_never_shrinks_below_the_minimum() {
        assert_eq!(fader_height(10.0), MIN_FADER_HEIGHT);
    }

    #[test]
    fn fader_height_never_grows_above_the_maximum() {
        assert_eq!(fader_height(10_000.0), MAX_FADER_HEIGHT);
    }

    #[test]
    fn resolve_drag_assign_onto_a_group_with_no_prior_rule_adds_an_exact_name() {
        let groups = vec![group("Game", &[])];
        let edits = resolve_drag_assign("game.exe", Some("Game"), &groups);
        assert_eq!(edits.len(), 1);
        assert!(matches!(&edits[0], ConfigEdit::SetRules(name, rules)
            if name == "Game" && rules == &vec!["game.exe".to_string()]));
    }

    #[test]
    fn resolve_drag_assign_onto_a_group_that_already_has_it_is_a_no_op() {
        let groups = vec![group("Game", &["game.exe"])];
        let edits = resolve_drag_assign("game.exe", Some("Game"), &groups);
        assert!(edits.is_empty());
    }

    #[test]
    fn resolve_drag_assign_moves_the_session_between_two_groups_in_one_batch() {
        let groups = vec![group("Music", &["game.exe"]), group("Game", &[])];
        let mut edits = resolve_drag_assign("game.exe", Some("Game"), &groups);
        edits.sort_by_key(|e| match e {
            ConfigEdit::SetRules(name, _) => name.clone(),
            _ => String::new(),
        });
        assert_eq!(edits.len(), 2);
        assert!(matches!(&edits[0], ConfigEdit::SetRules(name, rules) if name == "Game" && rules == &vec!["game.exe".to_string()]));
        assert!(matches!(&edits[1], ConfigEdit::SetRules(name, rules) if name == "Music" && rules.is_empty()));
    }

    #[test]
    fn resolve_drag_assign_to_master_unassigns_from_whichever_group_holds_it() {
        let groups = vec![group("Game", &["game.exe"])];
        let edits = resolve_drag_assign("game.exe", None, &groups);
        assert_eq!(edits.len(), 1);
        assert!(matches!(&edits[0], ConfigEdit::SetRules(name, rules) if name == "Game" && rules.is_empty()));
    }

    #[test]
    fn resolve_drag_assign_never_touches_glob_rules() {
        // "steam.exe" is currently routed here purely via the glob match —
        // there is no *exact* rule for it. Unassigning (target: None) must
        // have nothing to remove: the glob rule is left standing, not
        // stripped just because the session appears to leave the group.
        let groups = vec![group("Game", &["*steam*"])];
        let edits = resolve_drag_assign("steam.exe", None, &groups);
        assert!(edits.is_empty());
    }

    #[test]
    fn resolve_drag_assign_is_case_insensitive() {
        let groups = vec![group("Game", &["GAME.EXE"])];
        let edits = resolve_drag_assign("game.exe", None, &groups);
        assert_eq!(edits.len(), 1);
        assert!(matches!(&edits[0], ConfigEdit::SetRules(_, rules) if rules.is_empty()));
    }

    #[test]
    fn default_duck_trigger_picks_the_first_other_group() {
        let groups = vec![group("Voice", &[]), group("Music", &[])];
        assert_eq!(default_duck_trigger(&groups, "Music"), "Voice");
    }

    #[test]
    fn default_duck_trigger_is_empty_when_no_other_group_exists() {
        let groups = vec![group("Solo", &[])];
        assert_eq!(default_duck_trigger(&groups, "Solo"), "");
    }

    #[test]
    fn chip_label_prefers_display_name_when_present() {
        let mut s = session(1, "game.exe");
        s.display_name = "My Game".into();
        assert_eq!(chip_label(&s), "My Game");
    }

    #[test]
    fn chip_label_falls_back_to_the_process_file_name_when_display_name_is_blank() {
        // The common case in practice: WASAPI's GetDisplayName() returns an
        // empty string for most real sessions, not just occasionally.
        let mut s = session(1, "game.exe");
        s.display_name = String::new();
        assert_eq!(chip_label(&s), "game.exe");
    }

    #[test]
    fn chip_label_falls_back_to_the_pid_when_both_are_blank() {
        let s = SessionInfo {
            pid: 4242,
            process_path: "".into(),
            display_name: String::new(),
        };
        assert_eq!(chip_label(&s), "4242");
    }

    #[test]
    fn nothing_playing_beats_every_other_empty_reason() {
        assert_eq!(
            empty_reason(ZoneKind::Group, false, true, true),
            EmptyReason::NothingPlaying
        );
    }

    #[test]
    fn an_empty_master_pool_with_sessions_reads_as_all_routed() {
        assert_eq!(
            empty_reason(ZoneKind::Unassigned, true, false, false),
            EmptyReason::AllRouted
        );
    }

    #[test]
    fn an_empty_group_teaches_the_gesture() {
        assert_eq!(empty_reason(ZoneKind::Group, true, false, false), EmptyReason::GroupEmpty);
    }

    #[test]
    fn a_zone_filtered_to_nothing_reports_no_matches() {
        assert_eq!(empty_reason(ZoneKind::Group, true, true, true), EmptyReason::NoMatches);
    }

    #[test]
    fn a_group_that_was_already_empty_does_not_blame_the_search() {
        // The zone_had_chips distinction (decision 8) -- the one case that is
        // easy to get wrong: a group with nothing routed must still teach the
        // gesture while a search is active, not claim the search hid anything.
        assert_eq!(empty_reason(ZoneKind::Group, true, false, true), EmptyReason::GroupEmpty);
    }

    #[test]
    fn session_matches_finds_the_display_label() {
        let mut s = session(1, "unrelated.exe");
        s.display_name = "Google Chrome".into();
        assert!(session_matches(&s, "chrome"));
    }

    #[test]
    fn session_matches_finds_the_exe_file_name() {
        let s = session(1, "chrome.exe");
        assert!(session_matches(&s, "chrome"));
    }

    #[test]
    fn session_matches_ignores_case() {
        let s = session(1, "Chrome.exe");
        assert!(session_matches(&s, "CHROME"));
    }

    #[test]
    fn an_empty_query_matches_every_session() {
        let s = session(1, "anything.exe");
        assert!(session_matches(&s, ""));
    }

    #[test]
    fn escape_clears_a_focused_search_box() {
        // Regression: egui's Memory::begin_frame clears focus globally on an
        // unclaimed Escape *before* the widget renders (memory/mod.rs), so
        // `response.has_focus()` reads false on the very frame Escape fires --
        // a check against it would be silently dead code. `lost_focus()` is
        // the one that's true on that frame; confirmed empirically against
        // the pinned egui 0.35.0, not assumed from the docs.
        let ctx = egui::Context::default();
        ctx.set_fonts(egui::FontDefinitions::empty());
        let mut query = String::from("chrome");
        let id = egui::Id::new("session-search");

        let _ = ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                search_box(ui, &mut query);
                ui.memory_mut(|m| m.request_focus(id));
            });
        });

        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run_ui(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                search_box(ui, &mut query);
            });
        });

        assert_eq!(query, "", "Escape must clear the search box on the frame focus is lost");
    }

    #[test]
    fn meter_fraction_maps_full_scale_to_a_full_bar() {
        assert!((meter_fraction(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn meter_fraction_is_empty_at_and_below_the_floor() {
        assert_eq!(meter_fraction(0.0), 0.0);
        // −60 dBFS = 0.001 linear, exactly the floor → still empty.
        assert!(meter_fraction(0.001) < 1e-3);
    }

    #[test]
    fn meter_fraction_puts_minus_thirty_db_near_the_middle() {
        // −30 dBFS on a −60..0 scale → 0.5.
        let frac = meter_fraction(10f32.powf(-30.0 / 20.0));
        assert!((frac - 0.5).abs() < 1e-3, "got {frac}");
    }

    #[test]
    fn advance_hold_snaps_up_to_a_new_peak() {
        assert_eq!(advance_hold(0.2, 0.8, 0.016), 0.8);
    }

    #[test]
    fn advance_hold_decays_toward_the_floor_when_below() {
        let next = advance_hold(0.8, 0.1, 0.1); // 100 ms
        assert!(next < 0.8 && next > 0.1, "got {next}");
    }

    #[test]
    fn peak_for_returns_silent_for_an_unknown_group() {
        let peaks = vec![(GroupId(0), MeterLevel { peak: 0.5, clipped: false })];
        assert_eq!(peak_for(&peaks, GroupId(9)), MeterLevel::SILENT);
    }

    #[test]
    fn peak_for_finds_the_matching_group() {
        let level = MeterLevel { peak: 0.5, clipped: true };
        let peaks = vec![(GroupId(0), level)];
        assert_eq!(peak_for(&peaks, GroupId(0)), level);
    }

    #[test]
    fn unity_gain_is_zero_db() {
        assert!((gain_to_fader_db(Gain::UNITY) - 0.0).abs() < 1e-3);
    }

    #[test]
    fn the_bottom_of_travel_maps_to_true_silence() {
        assert_eq!(fader_db_to_gain(FADER_MIN_DB), Gain::SILENT);
        assert_eq!(fader_db_to_gain(FADER_MIN_DB - 10.0), Gain::SILENT);
    }

    #[test]
    fn boost_maps_above_zero_db() {
        let gain = fader_db_to_gain(FADER_MAX_DB);
        assert!((gain.value() - 1.995).abs() < 1e-2, "got {}", gain.value());
    }

    #[test]
    fn gain_round_trips_through_the_fader_mapping() {
        for db in [-40.0f32, -6.0, -0.5, 3.0, 6.0] {
            let gain = fader_db_to_gain(db);
            let back = gain_to_fader_db(gain);
            assert!((back - db).abs() < 1e-2, "db {db} -> gain {} -> {back}", gain.value());
        }
    }

    #[test]
    fn silence_formats_as_minus_inf() {
        assert_eq!(format_fader_db(FADER_MIN_DB as f64), "-inf dB");
        assert_eq!(format_fader_db((FADER_MIN_DB - 5.0) as f64), "-inf dB");
    }

    #[test]
    fn format_fader_db_shows_one_decimal_and_signs_boost() {
        assert_eq!(format_fader_db(-6.0), "-6.0 dB");
        assert_eq!(format_fader_db(0.0), "0.0 dB");
        assert_eq!(format_fader_db(3.5), "+3.5 dB");
    }

    #[test]
    fn parse_fader_db_accepts_signed_suffixed_and_infinite_forms() {
        assert_eq!(parse_fader_db("-6"), Some(-6.0));
        assert_eq!(parse_fader_db("-6.0"), Some(-6.0));
        assert_eq!(parse_fader_db("+3"), Some(3.0));
        assert_eq!(parse_fader_db("0"), Some(0.0));
        assert_eq!(parse_fader_db("3 dB"), Some(3.0));
        assert_eq!(parse_fader_db("-inf"), Some(FADER_MIN_DB as f64));
        assert_eq!(parse_fader_db("-∞"), Some(FADER_MIN_DB as f64));
    }

    #[test]
    fn parse_fader_db_rejects_garbage() {
        assert_eq!(parse_fader_db("loud"), None);
        assert_eq!(parse_fader_db(""), None);
    }

    #[test]
    fn gain_to_fader_db_does_not_clamp_an_out_of_range_existing_value() {
        // Regression for db-faders.md decision 10/15: a hand-written
        // `gain = 4.0` (+12 dB, above FADER_MAX_DB) must reach the slider as
        // the true +12 dB, not pre-clamped to +6 dB -- otherwise
        // `SliderClamping::Edits` has nothing left to preserve.
        let over_range = Gain::new(4.0).unwrap();
        let db = gain_to_fader_db(over_range);
        assert!(db > FADER_MAX_DB, "expected an unclamped value above {FADER_MAX_DB}, got {db}");
    }

    #[test]
    fn a_bumped_rebuild_generation_clears_the_solo_set() {
        let mut soloed: HashSet<String> = ["Game".to_string()].into_iter().collect();
        let mut seen = 3;

        clear_solo_on_rebuild(&mut soloed, &mut seen, 4);

        assert!(soloed.is_empty());
        assert_eq!(seen, 4);
    }

    #[test]
    fn an_unchanged_rebuild_generation_leaves_the_solo_set_alone() {
        let mut soloed: HashSet<String> = ["Game".to_string()].into_iter().collect();
        let mut seen = 3;

        clear_solo_on_rebuild(&mut soloed, &mut seen, 3);

        assert_eq!(soloed, ["Game".to_string()].into_iter().collect());
        assert_eq!(seen, 3);
    }

    #[test]
    fn a_non_soloed_unmuted_group_is_dimmed_while_another_group_is_soloed() {
        assert!(is_dimmed_by_other_solo(true, false, false));
    }

    #[test]
    fn the_soloed_group_itself_is_not_dimmed() {
        assert!(!is_dimmed_by_other_solo(true, true, false));
    }

    #[test]
    fn a_muted_group_is_not_dimmed_even_when_silenced_by_another_groups_solo() {
        // Mute wins over solo (per-group-mute-solo.md decision 2) and already
        // shows its own lit M button -- dimming on top would double-mark it.
        assert!(!is_dimmed_by_other_solo(true, false, true));
    }

    #[test]
    fn no_group_is_dimmed_when_solo_is_inactive() {
        assert!(!is_dimmed_by_other_solo(false, false, false));
    }
}
