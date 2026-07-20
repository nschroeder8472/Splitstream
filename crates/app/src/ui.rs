//! Settings window (egui/eframe): master + per-group faders, follow-master
//! toggle, per-group output device, live routed-apps list, add/remove group,
//! per-group match-rule editing (deferred here from session-routing.md's own
//! scope — see that doc's L1 "Out of scope: rule-editing UI (P4)").
//!
//! Reads `routes`/`routing_degraded` via `RoutingReader`, polled fresh every
//! frame (see event_pump.rs's doc comment for why: no `EngineEvent` variant
//! signals a route change, so polling is strictly more correct than trying
//! to infer one from unrelated event arrival). All edits go out as
//! `ShellAction`s — this module never touches `ConfigStore` or `EngineHandle`
//! directly (app-shell.md constraint: UI mutates config and sends commands,
//! never calls into `win-audio`).
//!
//! Text fields (output device, match rules, new-group name/bus/output) keep
//! their own per-group **draft** strings rather than re-deriving from the
//! live snapshot every frame — re-deriving would fight in-progress typing.
//! Sliders/checkboxes don't need this: they read the live value directly and
//! commit on every change (matches the fast-path param-edit flow).

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use eframe::egui;

use audio_core::{Gain, GroupId};
use control::ConfigEdit;
use engine::{GroupConfig, RoutingReader, SessionInfo};

use crate::event_pump::UiState;
use crate::ShellAction;

#[derive(Default)]
struct GroupDraft {
    output_device: String,
    match_rules: String,
}

#[derive(Default)]
struct NewGroupDraft {
    name: String,
    bus_endpoint: String,
    output_device: String,
}

pub struct SettingsApp {
    ui: Arc<Mutex<UiState>>,
    routing: RoutingReader,
    actions: Sender<ShellAction>,
    drafts: HashMap<String, GroupDraft>,
    new_group: NewGroupDraft,
}

impl SettingsApp {
    pub fn new(ui: Arc<Mutex<UiState>>, routing: RoutingReader, actions: Sender<ShellAction>) -> SettingsApp {
        SettingsApp {
            ui,
            routing,
            actions,
            drafts: HashMap::new(),
            new_group: NewGroupDraft::default(),
        }
    }

    fn send(&self, action: ShellAction) {
        let _ = self.actions.send(action);
    }

    fn draft_for(&mut self, group: &GroupConfig) -> &mut GroupDraft {
        self.drafts.entry(group.name.clone()).or_insert_with(|| GroupDraft {
            output_device: group.output_device.clone(),
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
        }

        let (snapshot, routes, degraded, xruns, faults) = {
            let state = self.ui.lock().unwrap();
            (
                state.snapshot.clone(),
                state.routes.clone(),
                state.routing_degraded,
                state.stats.xruns,
                state.stats.group_faults.len(),
            )
        };

        egui::CentralPanel::default().show(ui, |ui| {
            if degraded {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 80, 40),
                    "Routing degraded — some app auto-routing may not work.",
                );
                ui.separator();
            }

            ui.heading("Splitstream");
            self.master_controls(ui, &snapshot);
            ui.separator();

            for (i, group) in snapshot.groups.iter().enumerate() {
                self.group_controls(ui, group, GroupId(i as u16), &routes);
            }

            ui.separator();
            self.add_group_controls(ui);

            ui.separator();
            ui.label(format!("xruns: {xruns}   group faults: {faults}"));
        });
    }
}

impl SettingsApp {
    fn master_controls(&self, ui: &mut egui::Ui, snapshot: &engine::ConfigSnapshot) {
        ui.horizontal(|ui| {
            let mut muted = snapshot.muted;
            if ui.checkbox(&mut muted, "Mute").changed() {
                self.send(ShellAction::EditParams(vec![ConfigEdit::SetMuted(muted)]));
            }

            let mut master = snapshot.master.value();
            if ui.add(egui::Slider::new(&mut master, 0.0..=1.0).text("Master")).changed() {
                if let Ok(gain) = Gain::new(master) {
                    self.send(ShellAction::EditParams(vec![ConfigEdit::SetMaster(gain)]));
                }
            }
        });
    }

    fn group_controls(&mut self, ui: &mut egui::Ui, group: &GroupConfig, id: GroupId, routes: &[(GroupId, Vec<SessionInfo>)]) {
        let name = group.name.clone();
        let apps = routed_app_names(routes, id);

        // Pull owned copies of the draft strings out before entering the `ui`
        // closures below: those closures also call `self.send(..)`, and
        // holding a `&mut self.drafts` borrow across them would conflict.
        self.draft_for(group);
        let mut output_draft = self.drafts[&name].output_device.clone();
        let mut rules_draft = self.drafts[&name].match_rules.clone();

        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.strong(&group.name);
                if ui.button("Remove").clicked() {
                    self.send(ShellAction::EditStructure(vec![ConfigEdit::RemoveGroup(name.clone())]));
                }
            });

            let mut gain = group.gain.value();
            if ui.add(egui::Slider::new(&mut gain, 0.0..=1.0).text("Gain")).changed() {
                if let Ok(g) = Gain::new(gain) {
                    self.send(ShellAction::EditParams(vec![ConfigEdit::SetGroupGain(name.clone(), g)]));
                }
            }

            let mut follow = group.follow_master;
            if ui.checkbox(&mut follow, "Follow master").changed() {
                self.send(ShellAction::EditParams(vec![ConfigEdit::SetFollowMaster(name.clone(), follow)]));
            }

            ui.horizontal(|ui| {
                ui.label("Output:");
                ui.text_edit_singleline(&mut output_draft);
                if ui.button("Set").clicked() {
                    self.send(ShellAction::EditStructure(vec![ConfigEdit::SetGroupOutput(
                        name.clone(),
                        output_draft.clone(),
                    )]));
                }
            });

            ui.horizontal(|ui| {
                ui.label("Match rules:");
                ui.text_edit_singleline(&mut rules_draft);
                if ui.button("Save rules").clicked() {
                    let rules = split_rules(&rules_draft);
                    self.send(ShellAction::EditParams(vec![ConfigEdit::SetRules(name.clone(), rules)]));
                }
            });

            if apps.is_empty() {
                ui.label("No apps routed here.");
            } else {
                ui.label(format!("Routed apps: {}", apps.join(", ")));
            }
        });

        if let Some(draft) = self.drafts.get_mut(&name) {
            draft.output_device = output_draft;
            draft.match_rules = rules_draft;
        }
    }

    fn add_group_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.text_edit_singleline(&mut self.new_group.name);
            ui.label("Bus:");
            ui.text_edit_singleline(&mut self.new_group.bus_endpoint);
            ui.label("Output:");
            ui.text_edit_singleline(&mut self.new_group.output_device);

            if ui.button("Create New Audio Source").clicked() && !self.new_group.name.trim().is_empty() {
                let group = GroupConfig {
                    name: self.new_group.name.trim().to_string(),
                    bus_endpoint: self.new_group.bus_endpoint.trim().to_string(),
                    output_device: self.new_group.output_device.trim().to_string(),
                    gain: Gain::UNITY,
                    follow_master: true,
                    match_rules: vec![],
                };
                self.send(ShellAction::EditStructure(vec![ConfigEdit::AddGroup(group)]));
                self.new_group = NewGroupDraft::default();
            }
        });
    }
}

/// Pure — display names for every session routed to `group`, sorted for a
/// stable render order (`routes` is grouping-order from a `HashMap` upstream,
/// not display order).
fn routed_app_names(routes: &[(GroupId, Vec<SessionInfo>)], group: GroupId) -> Vec<String> {
    let mut names: Vec<String> = routes
        .iter()
        .find(|(g, _)| *g == group)
        .map(|(_, sessions)| sessions.iter().map(|s| s.display_name.clone()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// Pure — comma-separated draft text -> trimmed, non-empty rule strings.
fn split_rules(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> SessionInfo {
        SessionInfo {
            pid: 1,
            process_path: name.into(),
            display_name: name.into(),
        }
    }

    #[test]
    fn routed_app_names_returns_sorted_names_for_the_matching_group() {
        let routes = vec![(GroupId(0), vec![session("b.exe"), session("a.exe")])];
        assert_eq!(
            routed_app_names(&routes, GroupId(0)),
            vec!["a.exe".to_string(), "b.exe".to_string()]
        );
    }

    #[test]
    fn routed_app_names_is_empty_for_a_group_with_no_entry() {
        let routes = vec![(GroupId(0), vec![session("a.exe")])];
        assert!(routed_app_names(&routes, GroupId(1)).is_empty());
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
}
