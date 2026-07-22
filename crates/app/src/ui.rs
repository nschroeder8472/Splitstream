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

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use eframe::egui;

use audio_core::{DspSpec, EqBandSpec, Gain, GroupId};
use control::ConfigEdit;
use engine::ports::Endpoint;
use engine::{DuckSpecConfig, GroupConfig, MatchRule, RoutingReader, SessionInfo};

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
const COLUMN_CHROME_HEIGHT: f32 = 160.0;

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
    /// Responsive sizing (responsive-ui-refinement L4) — computed once per
    /// frame in `ui()` from `ui.available_size()`, shared by every column.
    width: f32,
    fader_height: f32,
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
    actions: Sender<ShellAction>,
    drafts: HashMap<String, GroupDraft>,
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
}

impl SettingsApp {
    pub fn new(ui: Arc<Mutex<UiState>>, routing: RoutingReader, actions: Sender<ShellAction>) -> SettingsApp {
        SettingsApp {
            ui,
            routing,
            actions,
            drafts: HashMap::new(),
            new_group: NewGroupDraft::default(),
            onboarding: OnboardingDraft::default(),
            screen: Screen::Mixer,
            show_new_group_panel: false,
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
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        {
            let mut state = self.ui.lock().unwrap();
            state.routes = self.routing.current_routes();
            state.routing_degraded = self.routing.is_degraded();
            state.all_sessions = self.routing.all_sessions();
        }

        let (snapshot, routes, all_sessions, degraded, xruns, faults, first_run, available_devices, default_output_name) = {
            let state = self.ui.lock().unwrap();
            (
                state.snapshot.clone(),
                state.routes.clone(),
                state.all_sessions.clone(),
                state.routing_degraded,
                state.stats.xruns,
                state.stats.group_faults.len(),
                state.first_run,
                state.available_devices.clone(),
                state.default_output_name.clone(),
            )
        };

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
                    width,
                    fader_height: height,
                };
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        self.master_column(ui, &snapshot, &routes, &all_sessions, width, height);
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
                ui.label(format!("xruns: {xruns}   group faults: {faults}"));
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
    fn master_column(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &engine::ConfigSnapshot,
        routes: &[(GroupId, Vec<SessionInfo>)],
        all_sessions: &[SessionInfo],
        width: f32,
        height: f32,
    ) {
        ui.group(|ui| {
            ui.set_width(width);
            ui.vertical_centered(|ui| {
                ui.strong("Master Volume");

                let mut master = snapshot.master.value();
                ui.spacing_mut().slider_width = height;
                if ui.add(egui::Slider::new(&mut master, 0.0..=1.0).vertical()).changed() {
                    if let Ok(gain) = Gain::new(master) {
                        self.send(ShellAction::EditParams(vec![ConfigEdit::SetMaster(gain)]));
                    }
                }

                if speaker_mute_button(ui, snapshot.muted) {
                    self.send(ShellAction::EditParams(vec![ConfigEdit::SetMuted(!snapshot.muted)]));
                }

                ui.label("Routed Apps");
                let unassigned = unassigned_sessions(all_sessions, routes);
                if let Some(pid) = session_drop_zone(ui, &unassigned) {
                    self.handle_drop(pid, None, all_sessions, &snapshot.groups);
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
        let GroupColumnCtx { routes, all_sessions, all_groups, devices, width, fader_height } = *ctx;
        let name = group.name.clone();

        ui.group(|ui| {
            ui.set_width(width);
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

                let mut gain = group.gain.value();
                ui.spacing_mut().slider_width = fader_height;
                if ui.add(egui::Slider::new(&mut gain, 0.0..=1.0).vertical()).changed() {
                    if let Ok(g) = Gain::new(gain) {
                        self.send(ShellAction::EditParams(vec![ConfigEdit::SetGroupGain(name.clone(), g)]));
                    }
                }

                ui.label("Routed Apps");
                let sessions = routed_sessions(routes, id);
                if let Some(pid) = session_drop_zone(ui, &sessions) {
                    self.handle_drop(pid, Some(&name), all_sessions, all_groups);
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
                };
                self.send(ShellAction::EditStructure(vec![ConfigEdit::AddGroup(group)]));
                self.new_group = NewGroupDraft::default();
                self.show_new_group_panel = false;
            }
        });
    }
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

/// Draggable chip per session plus a drop-accepting frame around them —
/// shared by the master (unassigned pool) and every group column (routed
/// apps). Returns the dropped session's pid, if a chip was released here
/// this frame.
fn session_drop_zone(ui: &mut egui::Ui, sessions: &[SessionInfo]) -> Option<u32> {
    let frame = egui::Frame::group(ui.style());
    let (_, dropped) = ui.dnd_drop_zone::<DragSession, ()>(frame, |ui| {
        if sessions.is_empty() {
            ui.weak("(none)");
        }
        for session in sessions {
            let id = egui::Id::new(("session-chip", session.pid));
            ui.dnd_drag_source(id, DragSession(session.pid), |ui| {
                ui.label(chip_label(session));
            });
        }
    });
    dropped.map(|payload| payload.0)
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
}
