//! Pure profile switch/save/dirty-check logic (profiles.md) — testable
//! without an engine, a file, or a frame. [`apply_profile`] diffs the
//! target profile against the *current live state* field-by-field, so a
//! profile that only changes gains emits only param-path edits (decision 10):
//! it never emits `SetGroupOutput`/`SetSpatial`/`SetDspChain` for a field
//! that didn't actually change, which is exactly what keeps `edit_path`'s
//! structural check from firing on a trivial switch.

use engine::{ConfigSnapshot, ProfileConfig, ProfileGroupConfig};

use crate::store::ConfigEdit;

/// The edit batch that switches live state to match the profile named
/// `name`. An unknown profile name yields no edits (L1 capability 11's
/// tolerance extends to the lookup itself). A profile entry naming a group
/// absent from `snapshot` is skipped; a live group the profile doesn't
/// mention is left untouched — both silent, not errors. `match_rules` is
/// never touched (decision 8) because [`ProfileGroupConfig`] has no such
/// field to read from.
pub fn apply_profile(snapshot: &ConfigSnapshot, name: &str) -> Vec<ConfigEdit> {
    let Some(profile) = snapshot.profiles.iter().find(|p| p.name == name) else {
        return Vec::new();
    };

    let mut edits = Vec::new();
    if snapshot.master != profile.master {
        edits.push(ConfigEdit::SetMaster(profile.master));
    }
    if snapshot.muted != profile.muted {
        edits.push(ConfigEdit::SetMuted(profile.muted));
    }

    for pg in &profile.groups {
        let Some(live) = snapshot.groups.iter().find(|g| g.name == pg.name) else {
            continue; // capability 11: entry names a group that no longer exists
        };
        diff_group(&mut edits, live, pg);
    }

    edits
}

fn diff_group(edits: &mut Vec<ConfigEdit>, live: &engine::GroupConfig, target: &ProfileGroupConfig) {
    if live.gain != target.gain {
        edits.push(ConfigEdit::SetGroupGain(target.name.clone(), target.gain));
    }
    if live.follow_master != target.follow_master {
        edits.push(ConfigEdit::SetFollowMaster(target.name.clone(), target.follow_master));
    }
    if live.muted != target.muted {
        edits.push(ConfigEdit::SetGroupMute(target.name.clone(), target.muted));
    }
    if live.output_device != target.output_device {
        edits.push(ConfigEdit::SetGroupOutput(target.name.clone(), target.output_device.clone()));
    }
    if live.spatial != target.spatial {
        edits.push(ConfigEdit::SetSpatial(target.name.clone(), target.spatial));
    }
    if live.dsp != target.dsp {
        edits.push(ConfigEdit::SetDspChain(target.name.clone(), target.dsp.clone()));
    }
    if live.duck != target.duck {
        edits.push(ConfigEdit::SetDuck(target.name.clone(), target.duck.clone()));
    }
}

/// Builds a [`ProfileConfig`] named `name` from `snapshot`'s current live
/// state — every live group, not a subset (a profile only becomes partial
/// later, by drift, when a group is added/removed after the save). Preserves
/// an existing same-named profile's `hotkey` (save must not silently clear a
/// binding); a new name gets `hotkey: None`.
pub fn capture_profile(snapshot: &ConfigSnapshot, name: &str) -> ProfileConfig {
    let hotkey = snapshot.profiles.iter().find(|p| p.name == name).and_then(|p| p.hotkey);
    ProfileConfig {
        name: name.to_string(),
        hotkey,
        master: snapshot.master,
        muted: snapshot.muted,
        groups: snapshot
            .groups
            .iter()
            .map(|g| ProfileGroupConfig {
                name: g.name.clone(),
                gain: g.gain,
                follow_master: g.follow_master,
                output_device: g.output_device.clone(),
                dsp: g.dsp.clone(),
                duck: g.duck.clone(),
                spatial: g.spatial,
                muted: g.muted,
            })
            .collect(),
    }
}

/// True when live state differs from the stored profile named `name` —
/// the modified indicator (L1 capability 6), computed rather than stored
/// (decision 5) so it can never disagree with the file. An unknown name
/// reads as unmodified: nothing to compare against.
pub fn profile_is_modified(snapshot: &ConfigSnapshot, name: &str) -> bool {
    match snapshot.profiles.iter().find(|p| p.name == name) {
        Some(stored) => capture_profile(snapshot, name) != *stored,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core::Gain;
    use engine::{AppConfig, GroupConfig};

    fn group(name: &str, output: &str, gain: f32) -> GroupConfig {
        GroupConfig {
            name: name.into(),
            output_device: output.into(),
            gain: Gain::new(gain).unwrap(),
            follow_master: true,
            match_rules: vec!["some.exe".into()],
            dsp: Vec::new(),
            duck: None,
            spatial: false,
            muted: false,
        }
    }

    fn snapshot(groups: Vec<GroupConfig>, profiles: Vec<ProfileConfig>) -> ConfigSnapshot {
        ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            groups,
            app: AppConfig::default(),
            profiles,
        }
    }

    #[test]
    fn capture_then_apply_is_a_no_op() {
        let snap = snapshot(vec![group("Game", "Speakers", 0.8)], vec![]);
        let captured = capture_profile(&snap, "Gaming");
        let with_profile = snapshot(snap.groups.clone(), vec![captured]);

        assert!(apply_profile(&with_profile, "Gaming").is_empty());
    }

    #[test]
    fn a_gain_only_profile_emits_only_param_path_edits() {
        let live = group("Game", "Speakers", 1.0);
        let mut target = live.clone();
        target.gain = Gain::new(0.5).unwrap();
        let profile = ProfileConfig {
            name: "Quiet".into(),
            hotkey: None,
            master: Gain::UNITY,
            muted: false,
            groups: vec![ProfileGroupConfig {
                name: target.name.clone(),
                gain: target.gain,
                follow_master: target.follow_master,
                output_device: target.output_device.clone(),
                dsp: target.dsp.clone(),
                duck: target.duck.clone(),
                spatial: target.spatial,
                muted: target.muted,
            }],
        };
        let snap = snapshot(vec![live], vec![profile]);

        let edits = apply_profile(&snap, "Quiet");
        assert_eq!(edits.len(), 1);
        assert!(matches!(edits[0], ConfigEdit::SetGroupGain(..)));
        for edit in &edits {
            assert_eq!(crate::store::edit_path(edit), crate::store::EditPath::Param);
        }
    }

    #[test]
    fn apply_skips_entries_for_groups_that_no_longer_exist() {
        let profile = ProfileConfig {
            name: "Gaming".into(),
            hotkey: None,
            master: Gain::UNITY,
            muted: false,
            groups: vec![ProfileGroupConfig {
                name: "Deleted".into(),
                gain: Gain::UNITY,
                follow_master: true,
                output_device: "Speakers".into(),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                muted: false,
            }],
        };
        let snap = snapshot(vec![group("Game", "Speakers", 1.0)], vec![profile]);

        assert!(apply_profile(&snap, "Gaming").is_empty());
    }

    #[test]
    fn apply_emits_nothing_for_groups_the_profile_does_not_mention() {
        let profile = ProfileConfig {
            name: "Gaming".into(),
            hotkey: None,
            master: Gain::UNITY,
            muted: false,
            groups: vec![], // mentions no groups at all
        };
        let snap = snapshot(vec![group("Game", "Speakers", 1.0)], vec![profile]);

        assert!(apply_profile(&snap, "Gaming").is_empty());
    }

    #[test]
    fn an_unknown_profile_name_yields_no_edits() {
        let snap = snapshot(vec![group("Game", "Speakers", 1.0)], vec![]);
        assert!(apply_profile(&snap, "Nonexistent").is_empty());
    }

    #[test]
    fn profile_is_modified_is_false_immediately_after_capture() {
        let snap = snapshot(vec![group("Game", "Speakers", 1.0)], vec![]);
        let captured = capture_profile(&snap, "Gaming");
        let with_profile = snapshot(snap.groups.clone(), vec![captured]);

        assert!(!profile_is_modified(&with_profile, "Gaming"));
    }

    #[test]
    fn profile_is_modified_detects_a_changed_gain() {
        let snap = snapshot(vec![group("Game", "Speakers", 1.0)], vec![]);
        let captured = capture_profile(&snap, "Gaming");
        let mut live = snap.groups.clone();
        live[0].gain = Gain::new(0.5).unwrap();
        let modified = snapshot(live, vec![captured]);

        assert!(profile_is_modified(&modified, "Gaming"));
    }

    #[test]
    fn applying_a_profile_never_changes_match_rules() {
        // ProfileGroupConfig has no match_rules field at all — a profile
        // batch can structurally never contain a SetRules edit.
        let profile = ProfileConfig {
            name: "Gaming".into(),
            hotkey: None,
            master: Gain::UNITY,
            muted: false,
            groups: vec![ProfileGroupConfig {
                name: "Game".into(),
                gain: Gain::new(0.5).unwrap(),
                follow_master: true,
                output_device: "Speakers".into(),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                muted: false,
            }],
        };
        let snap = snapshot(vec![group("Game", "Speakers", 1.0)], vec![profile]);

        let edits = apply_profile(&snap, "Gaming");
        assert!(!edits.is_empty());
        assert!(!edits.iter().any(|e| matches!(e, ConfigEdit::SetRules(..))));
    }
}
