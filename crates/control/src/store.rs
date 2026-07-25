//! Single write path for shell-driven config edits: semantic edits
//! ([`ConfigEdit`]) applied to a comment-preserving `toml_edit` document,
//! validated, then written atomically (temp file + rename). Per app-shell.md's
//! "machine edits to user-editable files" decision — never raw-text patching,
//! never re-serialize via serde (that would destroy comments/ordering).
//!
//! Keeps its own in-memory `DocumentMut` across `apply()` calls rather than
//! re-reading the file each time — app-shell.md documents the debounce/hand-edit
//! race as last-writer-wins for v1, and this store is one of the writers in
//! that race, so staying with its own last-known document is the accepted
//! behavior, not a bug.

use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

use audio_core::{DspSpec, EqBandSpec, Gain, GroupId};
use engine::{ConfigSnapshot, DspStageConfig, DuckSpecConfig, GroupConfig, ProfileConfig, ProfileGroupConfig};

use crate::config::{parse, ConfigError};

#[derive(Debug, Clone)]
pub enum ConfigEdit {
    SetGroupGain(String, Gain),
    SetMaster(Gain),
    SetMuted(bool),
    SetFollowMaster(String, bool),
    /// Persisted per-group mute (per-group-mute-solo.md). Solo is
    /// deliberately not here -- it is session-only and reaches the mixer via
    /// `ShellAction::SetSolo`, never through `ConfigStore`.
    SetGroupMute(String, bool),
    SetGroupOutput(String, String),
    AddGroup(GroupConfig),
    RemoveGroup(String),
    SetRules(String, Vec<String>),
    /// Retune one band of one EQ stage (graphical-eq.md decision 13 — gained
    /// a stage index; previously resolved the stage by first-type-match,
    /// which silently retuned the wrong stage under a second `Eq` stage).
    /// Fast path, smoothed, no rebuild.
    SetEqBand(String, usize, usize, EqBandSpec),
    /// Replace an EQ stage's entire band list — add, remove, and preset-apply
    /// all funnel through this one edit (decision 8). Structural: rebuilds
    /// the stage off-RT and swaps it in, even when the band count is
    /// unchanged (decision 12).
    SetEqBands(String, usize, Vec<EqBandSpec>),
    SetLimiterCeiling(String, f32),
    SetDuck(String, Option<DuckSpecConfig>),
    SetDspBypass(String, usize, bool),
    AddDspStage(String, DspSpec),
    RemoveDspStage(String, usize),
    /// Live spatial-audio toggle (spatial-audio.md) — funnels through
    /// `ShellAction::EditSpatial`/`EngineHandle::apply_spatial`, not the
    /// plain `EditParams` fast path (see `ConfigDelta.spatial`'s doc).
    SetSpatial(String, bool),
    /// `[app] autostart` (simple-launch.md L4) — rides the plain
    /// `EditParams`/`ConfigStore::apply` path like any other scalar; no
    /// mixer command exists for it (`edits_to_mixer_commands` returns
    /// `None`), the dispatcher reconciles `lifecycle::set_autostart` when
    /// `app.autostart` differs from the prior snapshot instead.
    SetAutostart(bool),
    /// Replace a group's whole DSP chain in one structural edit (profiles.md
    /// decision 9) — applying a profile needs this; no add/remove sequence
    /// against the existing stage list is index-safe for a full replace.
    SetDspChain(String, Vec<DspStageConfig>),
    /// Upsert by name: overwrites an existing `[[profile]]` table with the
    /// same name, or appends a new one (profiles.md — save vs. save-as share
    /// this one edit).
    SetProfile(ProfileConfig),
    RemoveProfile(String),
    /// `[app] active_profile` — `None` clears the key (profiles.md decision 5).
    SetActiveProfile(Option<String>),
}

/// Which apply path an edit requires (profiles.md decision 12 — revises the
/// draft `is_structural(&ConfigEdit) -> bool`: this codebase has four apply
/// paths, not two, and a profile batch can mix them). `Dispatcher` computes
/// this per edit to route a batch instead of each call site hardcoding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditPath {
    /// Lock-free `MixerCommand` fast path, no rebuild.
    Param,
    /// Full engine graph rebuild — output device / group set changes.
    Structural,
    /// `EngineHandle::apply_spatial`'s off-RT build-and-swap.
    Spatial,
    /// `EngineHandle::apply_dsp_chains`'s off-RT build-and-swap.
    DspChain,
}

/// Exhaustive by construction — a new `ConfigEdit` variant is a compile error
/// here until classified, not a silent gap (profiles.md test contract).
pub fn edit_path(edit: &ConfigEdit) -> EditPath {
    match edit {
        ConfigEdit::SetGroupGain(..)
        | ConfigEdit::SetMaster(..)
        | ConfigEdit::SetMuted(..)
        | ConfigEdit::SetFollowMaster(..)
        | ConfigEdit::SetGroupMute(..)
        | ConfigEdit::SetRules(..)
        | ConfigEdit::SetEqBand(..)
        | ConfigEdit::SetLimiterCeiling(..)
        | ConfigEdit::SetDuck(..)
        | ConfigEdit::SetDspBypass(..)
        | ConfigEdit::SetAutostart(..)
        | ConfigEdit::SetProfile(..)
        | ConfigEdit::RemoveProfile(..)
        | ConfigEdit::SetActiveProfile(..) => EditPath::Param,
        ConfigEdit::SetGroupOutput(..) | ConfigEdit::AddGroup(..) | ConfigEdit::RemoveGroup(..) => {
            EditPath::Structural
        }
        ConfigEdit::SetSpatial(..) => EditPath::Spatial,
        ConfigEdit::AddDspStage(..)
        | ConfigEdit::RemoveDspStage(..)
        | ConfigEdit::SetEqBands(..)
        | ConfigEdit::SetDspChain(..) => EditPath::DspChain,
    }
}

#[derive(Debug)]
pub enum StoreError {
    Io(String),
    Validation(ConfigError),
}

/// `GroupId`s are positional (snapshot order), matching `engine::graph::resolve`'s
/// own convention — same caveat as `group_rules`: valid as long as `snapshot`
/// is the one the engine was last built/rebuilt from.
pub fn group_id_for(snapshot: &ConfigSnapshot, name: &str) -> Option<GroupId> {
    snapshot
        .groups
        .iter()
        .position(|g| g.name == name)
        .map(|i| GroupId(i as u16))
}

pub struct ConfigStore {
    path: PathBuf,
    doc: DocumentMut,
    last_written: Option<ConfigSnapshot>,
}

impl ConfigStore {
    /// Reads and validates the file at `path` (both TOML syntax and config
    /// semantics) before accepting it as a base for edits.
    pub fn open(path: &Path) -> Result<ConfigStore, StoreError> {
        let text = fs::read_to_string(path).map_err(|e| StoreError::Io(e.to_string()))?;
        let doc: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| StoreError::Validation(ConfigError::Parse(e.to_string())))?;
        parse(&text).map_err(StoreError::Validation)?;
        Ok(ConfigStore {
            path: path.to_path_buf(),
            doc,
            last_written: None,
        })
    }

    /// Applies every edit to the in-memory document, validates the result
    /// parses to a valid `ConfigSnapshot` (rejecting the whole batch and
    /// leaving the document untouched on failure), then writes atomically.
    pub fn apply(&mut self, edits: &[ConfigEdit]) -> Result<ConfigSnapshot, StoreError> {
        let mut draft = self.doc.clone();
        for edit in edits {
            apply_edit(&mut draft, edit)?;
        }

        let text = draft.to_string();
        let snapshot = parse(&text).map_err(StoreError::Validation)?;
        crate::atomic_write::write_atomic(&self.path, &text).map_err(StoreError::Io)?;

        self.doc = draft;
        self.last_written = Some(snapshot.clone());
        Ok(snapshot)
    }

    /// True when `snapshot` is exactly what this store last wrote — lets a
    /// watcher-delivered snapshot be recognized as this store's own echo
    /// rather than an external edit.
    pub fn is_echo(&self, snapshot: &ConfigSnapshot) -> bool {
        self.last_written.as_ref() == Some(snapshot)
    }
}

fn apply_edit(doc: &mut DocumentMut, edit: &ConfigEdit) -> Result<(), StoreError> {
    match edit {
        ConfigEdit::SetGroupGain(name, gain) => {
            find_group_table(doc, name)?["gain"] = value(gain.value() as f64);
        }
        ConfigEdit::SetMaster(gain) => {
            doc["master"] = value(gain.value() as f64);
        }
        ConfigEdit::SetMuted(muted) => {
            doc["muted"] = value(*muted);
        }
        ConfigEdit::SetFollowMaster(name, follow) => {
            find_group_table(doc, name)?["follow_master"] = value(*follow);
        }
        ConfigEdit::SetGroupMute(name, muted) => {
            find_group_table(doc, name)?["muted"] = value(*muted);
        }
        ConfigEdit::SetGroupOutput(name, device) => {
            find_group_table(doc, name)?["output_device"] = value(device.as_str());
        }
        ConfigEdit::AddGroup(group) => {
            groups_array(doc).push(group_table(group));
        }
        ConfigEdit::RemoveGroup(name) => {
            let groups = groups_array(doc);
            let index = groups
                .iter()
                .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name.as_str()))
                .ok_or_else(|| no_such_group(name))?;
            groups.remove(index);
        }
        ConfigEdit::SetRules(name, rules) => {
            find_group_table(doc, name)?["match_rules"] = value(string_array(rules));
        }
        ConfigEdit::SetEqBand(name, stage_idx, band, spec) => {
            let group = find_group_table(doc, name)?;
            let stage = dsp_array(group, name)?
                .get_mut(*stage_idx)
                .ok_or_else(|| no_such_dsp_stage_index(name, *stage_idx))?;
            let bands = stage["bands"]
                .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
                .as_array_of_tables_mut()
                .ok_or_else(|| malformed_shape(name, "bands", "[[group.dsp.bands]]"))?;
            let band_table = bands.get_mut(*band).ok_or_else(|| no_such_band(name, *band))?;
            band_table["freq_hz"] = value(spec.freq_hz as f64);
            band_table["gain_db"] = value(spec.gain_db as f64);
            band_table["q"] = value(spec.q as f64);
        }
        ConfigEdit::SetEqBands(name, stage_idx, bands) => {
            let group = find_group_table(doc, name)?;
            let stage = dsp_array(group, name)?
                .get_mut(*stage_idx)
                .ok_or_else(|| no_such_dsp_stage_index(name, *stage_idx))?;
            let mut arr = ArrayOfTables::new();
            for b in bands {
                let mut bt = Table::new();
                bt["freq_hz"] = value(b.freq_hz as f64);
                bt["gain_db"] = value(b.gain_db as f64);
                bt["q"] = value(b.q as f64);
                arr.push(bt);
            }
            // Full replace, not a mutate-in-place -- immune to a prior
            // inline-array shape, unlike SetEqBand's field-level writes.
            stage["bands"] = Item::ArrayOfTables(arr);
        }
        ConfigEdit::SetLimiterCeiling(name, ceiling_db) => {
            let group = find_group_table(doc, name)?;
            let stage = find_dsp_stage_mut(group, name, "limiter")?
                .ok_or_else(|| no_such_dsp_stage(name, "limiter"))?;
            stage["ceiling_db"] = value(*ceiling_db as f64);
        }
        ConfigEdit::SetDspBypass(name, stage_idx, bypassed) => {
            let group = find_group_table(doc, name)?;
            let stage = dsp_array(group, name)?
                .get_mut(*stage_idx)
                .ok_or_else(|| no_such_dsp_stage_index(name, *stage_idx))?;
            stage["bypassed"] = value(*bypassed);
        }
        ConfigEdit::AddDspStage(name, spec) => {
            let group = find_group_table(doc, name)?;
            let stage = dsp_stage_table(spec);
            dsp_array(group, name)?.push(stage);
        }
        ConfigEdit::RemoveDspStage(name, stage_idx) => {
            let group = find_group_table(doc, name)?;
            let stages = dsp_array(group, name)?;
            if *stage_idx >= stages.len() {
                return Err(no_such_dsp_stage_index(name, *stage_idx));
            }
            stages.remove(*stage_idx);
        }
        ConfigEdit::SetSpatial(name, on) => {
            find_group_table(doc, name)?["spatial"] = value(*on);
        }
        ConfigEdit::SetAutostart(on) => {
            let app = doc["app"]
                .or_insert(Item::Table(Table::new()))
                .as_table_mut()
                .ok_or_else(malformed_app_shape)?;
            app["autostart"] = value(*on);
        }
        ConfigEdit::SetDuck(name, duck) => {
            let group = find_group_table(doc, name)?;
            match duck {
                Some(d) => {
                    let t = group["duck"]
                        .or_insert(Item::Table(Table::new()))
                        .as_table_mut()
                        .ok_or_else(|| malformed_shape(name, "duck", "[group.duck]"))?;
                    t["trigger"] = value(d.trigger.as_str());
                    t["amount_db"] = value(d.amount_db as f64);
                    t["threshold_db"] = value(d.threshold_db as f64);
                    t["attack_ms"] = value(d.attack_ms as f64);
                    t["release_ms"] = value(d.release_ms as f64);
                }
                None => {
                    group.remove("duck");
                }
            }
        }
        ConfigEdit::SetDspChain(name, dsp) => {
            let group = find_group_table(doc, name)?;
            // Full replace, same collapse as SetEqBands — immune to a prior
            // inline-array shape and to index races between frames.
            let mut arr = ArrayOfTables::new();
            for stage in dsp {
                let mut st = dsp_stage_table(&stage.spec);
                st["bypassed"] = value(stage.bypassed);
                arr.push(st);
            }
            group["dsp"] = Item::ArrayOfTables(arr);
        }
        ConfigEdit::SetProfile(profile) => {
            let profiles = profiles_array(doc);
            let index = profiles
                .iter()
                .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(profile.name.as_str()));
            match index {
                Some(i) => *profiles.get_mut(i).expect("index just found") = profile_table(profile),
                None => profiles.push(profile_table(profile)),
            }
        }
        ConfigEdit::RemoveProfile(name) => {
            let profiles = profiles_array(doc);
            let index = profiles
                .iter()
                .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name.as_str()))
                .ok_or_else(|| no_such_profile(name))?;
            profiles.remove(index);
        }
        ConfigEdit::SetActiveProfile(name) => {
            let app = doc["app"]
                .or_insert(Item::Table(Table::new()))
                .as_table_mut()
                .ok_or_else(malformed_app_shape)?;
            match name {
                Some(n) => app["active_profile"] = value(n.as_str()),
                None => {
                    app.remove("active_profile");
                }
            }
        }
    }
    Ok(())
}

fn groups_array(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    doc["group"]
        .or_insert(toml_edit::Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .expect("`group` key must hold an array of tables")
}

fn profiles_array(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    doc["profile"]
        .or_insert(toml_edit::Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .expect("`profile` key must hold an array of tables")
}

fn no_such_profile(name: &str) -> StoreError {
    StoreError::Validation(ConfigError::Invalid(format!("no profile named {name:?}")))
}

/// Fallible, unlike `groups_array`: `dsp`/`bands`/`duck` are all keys a user
/// can plausibly hand-write in an alternate-but-still-valid TOML shape (e.g.
/// `dsp = [{...}]` as one inline array instead of `[[group.dsp]]` blocks) —
/// `parse()` accepts either shape identically (serde doesn't care), but only
/// the block form is `toml_edit`-editable as an `ArrayOfTables`/`Table` here.
/// A live edit against the inline form must error, not panic (review finding,
/// dsp-pipeline P5 — an inline-array `bands` list crashed `SetEqBand`).
fn dsp_array<'a>(group: &'a mut Table, name: &str) -> Result<&'a mut ArrayOfTables, StoreError> {
    group["dsp"]
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| malformed_shape(name, "dsp", "[[group.dsp]]"))
}

fn find_dsp_stage_mut<'a>(
    group: &'a mut Table,
    name: &str,
    stage_type: &str,
) -> Result<Option<&'a mut Table>, StoreError> {
    Ok(dsp_array(group, name)?
        .iter_mut()
        .find(|t| t.get("type").and_then(|v| v.as_str()) == Some(stage_type)))
}

fn malformed_shape(group: &str, key: &str, expected: &str) -> StoreError {
    StoreError::Validation(ConfigError::Invalid(format!(
        "group {group:?}: `{key}` must be written as `{expected}` blocks, not an inline array/table, to support live edits"
    )))
}

fn malformed_app_shape() -> StoreError {
    StoreError::Validation(ConfigError::Invalid(
        "`app` must be written as a `[app]` table, not an inline table, to support live edits".into(),
    ))
}

fn dsp_stage_table(spec: &DspSpec) -> Table {
    let mut t = Table::new();
    match spec {
        DspSpec::Eq { bands } => {
            t["type"] = value("eq");
            let mut arr = ArrayOfTables::new();
            for b in bands {
                let mut bt = Table::new();
                bt["freq_hz"] = value(b.freq_hz as f64);
                bt["gain_db"] = value(b.gain_db as f64);
                bt["q"] = value(b.q as f64);
                arr.push(bt);
            }
            t["bands"] = Item::ArrayOfTables(arr);
        }
        DspSpec::Limiter { ceiling_db } => {
            t["type"] = value("limiter");
            t["ceiling_db"] = value(*ceiling_db as f64);
        }
    }
    t["bypassed"] = value(false);
    t
}

fn no_such_dsp_stage(group: &str, stage_type: &str) -> StoreError {
    StoreError::Validation(ConfigError::Invalid(format!(
        "group {group:?} has no {stage_type} dsp stage"
    )))
}

fn no_such_dsp_stage_index(group: &str, idx: usize) -> StoreError {
    StoreError::Validation(ConfigError::Invalid(format!(
        "group {group:?} has no dsp stage at index {idx}"
    )))
}

fn no_such_band(group: &str, idx: usize) -> StoreError {
    StoreError::Validation(ConfigError::Invalid(format!(
        "group {group:?}'s eq stage has no band at index {idx}"
    )))
}

fn find_group_table<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table, StoreError> {
    groups_array(doc)
        .iter_mut()
        .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
        .ok_or_else(|| no_such_group(name))
}

fn no_such_group(name: &str) -> StoreError {
    StoreError::Validation(ConfigError::Invalid(format!("no group named {name:?}")))
}

fn group_table(g: &GroupConfig) -> Table {
    let mut t = Table::new();
    t["name"] = value(g.name.as_str());
    t["output_device"] = value(g.output_device.as_str());
    t["gain"] = value(g.gain.value() as f64);
    t["follow_master"] = value(g.follow_master);
    t["spatial"] = value(g.spatial);
    t["muted"] = value(g.muted);
    t["match_rules"] = value(string_array(&g.match_rules));
    write_dsp_stages(&mut t, &g.dsp);
    write_duck(&mut t, g.duck.as_ref());
    // Config-file-only chords (external-controls.md decision 8) — always
    // `None` for `AddGroup`'s actual caller today (no hotkey-editing UI
    // exists), but writing them when present keeps this a faithful whole-
    // `GroupConfig` serializer rather than one that silently drops fields.
    if let Some(h) = &g.hotkey_mute {
        t["hotkey_mute"] = value(h.to_string());
    }
    if let Some(h) = &g.hotkey_volume_up {
        t["hotkey_volume_up"] = value(h.to_string());
    }
    if let Some(h) = &g.hotkey_volume_down {
        t["hotkey_volume_down"] = value(h.to_string());
    }
    t
}

fn profile_table(p: &ProfileConfig) -> Table {
    let mut t = Table::new();
    t["name"] = value(p.name.as_str());
    if let Some(h) = &p.hotkey {
        t["hotkey"] = value(h.to_string());
    }
    t["master"] = value(p.master.value() as f64);
    t["muted"] = value(p.muted);
    if !p.groups.is_empty() {
        let mut arr = ArrayOfTables::new();
        for g in &p.groups {
            arr.push(profile_group_table(g));
        }
        t["group"] = Item::ArrayOfTables(arr);
    }
    t
}

fn profile_group_table(g: &ProfileGroupConfig) -> Table {
    let mut t = Table::new();
    t["name"] = value(g.name.as_str());
    t["output_device"] = value(g.output_device.as_str());
    t["gain"] = value(g.gain.value() as f64);
    t["follow_master"] = value(g.follow_master);
    t["spatial"] = value(g.spatial);
    t["muted"] = value(g.muted);
    write_dsp_stages(&mut t, &g.dsp);
    write_duck(&mut t, g.duck.as_ref());
    t
}

/// Shared by [`group_table`] and [`profile_group_table`] — both carry the
/// same `[[.dsp]]` shape.
fn write_dsp_stages(t: &mut Table, dsp: &[DspStageConfig]) {
    if !dsp.is_empty() {
        let mut arr = ArrayOfTables::new();
        for stage in dsp {
            let mut st = dsp_stage_table(&stage.spec);
            st["bypassed"] = value(stage.bypassed);
            arr.push(st);
        }
        t["dsp"] = Item::ArrayOfTables(arr);
    }
}

/// Shared by [`group_table`] and [`profile_group_table`] — both carry the
/// same optional `.duck` shape.
fn write_duck(t: &mut Table, duck: Option<&DuckSpecConfig>) {
    if let Some(d) = duck {
        let mut dt = Table::new();
        dt["trigger"] = value(d.trigger.as_str());
        dt["amount_db"] = value(d.amount_db as f64);
        dt["threshold_db"] = value(d.threshold_db as f64);
        dt["attack_ms"] = value(d.attack_ms as f64);
        dt["release_ms"] = value(d.release_ms as f64);
        t["duck"] = Item::Table(dt);
    }
}

fn string_array(items: &[String]) -> Array {
    items.iter().map(String::as_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core::Gain;
    use engine::HotkeyChord;

    fn write_file(dir: &std::path::Path, text: &str) -> PathBuf {
        let path = dir.join("splitstream.toml");
        fs::write(&path, text).unwrap();
        path
    }

    const BASE: &str = r#"
schema_version = 2
master = 0.8 # master volume

[[group]]
name = "Game"
output_device = "Speakers"
gain = 1.0
follow_master = true
"#;

    #[test]
    fn open_rejects_malformed_toml_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "this is not [ valid toml");
        assert!(matches!(
            ConfigStore::open(&path),
            Err(StoreError::Validation(ConfigError::Parse(_)))
        ));
    }

    #[test]
    fn open_rejects_semantically_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "schema_version = 2\nmaster = -1.0\n");
        assert!(matches!(
            ConfigStore::open(&path),
            Err(StoreError::Validation(ConfigError::Invalid(_)))
        ));
    }

    #[test]
    fn apply_writes_gain_change_and_preserves_comments_and_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let snapshot = store
            .apply(&[ConfigEdit::SetGroupGain("Game".into(), Gain::new(0.5).unwrap())])
            .unwrap();

        assert_eq!(snapshot.groups[0].gain, Gain::new(0.5).unwrap());
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("# master volume"), "comment must survive an edit");
        assert!(on_disk.contains("gain = 0.5"));
    }

    #[test]
    fn apply_rejects_edit_for_an_unknown_group_and_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        let result = store.apply(&[ConfigEdit::SetGroupGain(
            "Nonexistent".into(),
            Gain::new(0.5).unwrap(),
        )]);

        assert!(matches!(result, Err(StoreError::Validation(_))));
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "rejected batch must not touch the file");
    }

    #[test]
    fn apply_add_then_remove_group_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let added = GroupConfig {
            name: "Chat".into(),
            output_device: "Headset".into(),
            gain: Gain::UNITY,
            follow_master: true,
            match_rules: vec!["chat.exe".into()],
            dsp: Vec::new(),
            duck: None,
            spatial: false,
            muted: false,
            hotkey_mute: None,
            hotkey_volume_up: None,
            hotkey_volume_down: None,
        };
        let snapshot = store.apply(&[ConfigEdit::AddGroup(added)]).unwrap();
        assert_eq!(snapshot.groups.len(), 2);
        assert_eq!(snapshot.groups[1].name, "Chat");
        assert_eq!(snapshot.groups[1].match_rules, vec!["chat.exe".to_string()]);

        let snapshot = store.apply(&[ConfigEdit::RemoveGroup("Game".into())]).unwrap();
        assert_eq!(snapshot.groups.len(), 1);
        assert_eq!(snapshot.groups[0].name, "Chat");
    }

    #[test]
    fn apply_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        store.apply(&[ConfigEdit::SetMaster(Gain::new(0.5).unwrap())]).unwrap();

        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn is_echo_recognizes_only_this_stores_own_last_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();
        let original = parse(BASE).unwrap();
        assert!(!store.is_echo(&original), "nothing written yet");

        let written = store.apply(&[ConfigEdit::SetMuted(true)]).unwrap();
        assert!(store.is_echo(&written));
        assert!(!store.is_echo(&original));
    }

    #[test]
    fn group_id_for_returns_positional_id_matching_snapshot_order() {
        let snapshot = parse(BASE).unwrap();
        assert_eq!(group_id_for(&snapshot, "Game"), Some(GroupId(0)));
        assert_eq!(group_id_for(&snapshot, "Nonexistent"), None);
    }

    #[test]
    fn add_dsp_stage_then_set_eq_band_and_limiter_ceiling_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let eq = DspSpec::Eq {
            bands: vec![EqBandSpec {
                freq_hz: 200.0,
                gain_db: 3.0,
                q: 0.7,
            }],
        };
        let limiter = DspSpec::Limiter { ceiling_db: -1.0 };
        let snapshot = store
            .apply(&[
                ConfigEdit::AddDspStage("Game".into(), eq),
                ConfigEdit::AddDspStage("Game".into(), limiter),
            ])
            .unwrap();
        assert_eq!(snapshot.groups[0].dsp.len(), 2);

        let snapshot = store
            .apply(&[
                ConfigEdit::SetEqBand(
                    "Game".into(),
                    0,
                    0,
                    EqBandSpec {
                        freq_hz: 500.0,
                        gain_db: -2.0,
                        q: 1.2,
                    },
                ),
                ConfigEdit::SetLimiterCeiling("Game".into(), -3.0),
            ])
            .unwrap();

        match &snapshot.groups[0].dsp[0].spec {
            DspSpec::Eq { bands } => {
                assert_eq!(bands[0].freq_hz, 500.0);
                assert_eq!(bands[0].q, 1.2);
            }
            _ => panic!("expected the eq stage at index 0"),
        }
        match &snapshot.groups[0].dsp[1].spec {
            DspSpec::Limiter { ceiling_db } => assert_eq!(*ceiling_db, -3.0),
            _ => panic!("expected the limiter stage at index 1"),
        }
    }

    #[test]
    fn set_dsp_bypass_then_remove_dsp_stage_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        store
            .apply(&[ConfigEdit::AddDspStage(
                "Game".into(),
                DspSpec::Limiter { ceiling_db: -1.0 },
            )])
            .unwrap();

        let snapshot = store
            .apply(&[ConfigEdit::SetDspBypass("Game".into(), 0, true)])
            .unwrap();
        assert!(snapshot.groups[0].dsp[0].bypassed);

        let snapshot = store
            .apply(&[ConfigEdit::RemoveDspStage("Game".into(), 0)])
            .unwrap();
        assert!(snapshot.groups[0].dsp.is_empty());
    }

    #[test]
    fn set_eq_band_on_a_group_with_no_eq_stage_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let result = store.apply(&[ConfigEdit::SetEqBand(
            "Game".into(),
            0,
            0,
            EqBandSpec {
                freq_hz: 200.0,
                gain_db: 0.0,
                q: 0.7,
            },
        )]);
        assert!(matches!(result, Err(StoreError::Validation(_))));
    }

    #[test]
    fn set_eq_band_targets_the_named_stage_not_the_first() {
        // Regression for decision 13: SetEqBand used to resolve its stage by
        // first-type-match ("eq"), so a second EQ stage's edit silently
        // retuned the first one instead.
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let first = DspSpec::Eq { bands: vec![EqBandSpec { freq_hz: 100.0, gain_db: 1.0, q: 1.0 }] };
        let second = DspSpec::Eq { bands: vec![EqBandSpec { freq_hz: 200.0, gain_db: 2.0, q: 1.0 }] };
        store
            .apply(&[
                ConfigEdit::AddDspStage("Game".into(), first),
                ConfigEdit::AddDspStage("Game".into(), second),
            ])
            .unwrap();

        let snapshot = store
            .apply(&[ConfigEdit::SetEqBand(
                "Game".into(),
                1,
                0,
                EqBandSpec { freq_hz: 9000.0, gain_db: 9.0, q: 2.0 },
            )])
            .unwrap();

        match (&snapshot.groups[0].dsp[0].spec, &snapshot.groups[0].dsp[1].spec) {
            (DspSpec::Eq { bands: first_bands }, DspSpec::Eq { bands: second_bands }) => {
                assert_eq!(first_bands[0].freq_hz, 100.0, "the first EQ stage must be untouched");
                assert_eq!(second_bands[0].freq_hz, 9000.0, "the second EQ stage must receive the edit");
            }
            other => panic!("expected two Eq stages, got {other:?}"),
        }
    }

    #[test]
    fn set_eq_bands_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        store
            .apply(&[ConfigEdit::AddDspStage(
                "Game".into(),
                DspSpec::Eq { bands: vec![EqBandSpec { freq_hz: 1000.0, gain_db: 0.0, q: 0.7 }] },
            )])
            .unwrap();

        let new_bands = vec![
            EqBandSpec { freq_hz: 100.0, gain_db: 3.0, q: 1.0 },
            EqBandSpec { freq_hz: 1000.0, gain_db: -3.0, q: 1.0 },
            EqBandSpec { freq_hz: 8000.0, gain_db: 2.0, q: 1.0 },
        ];
        let snapshot = store.apply(&[ConfigEdit::SetEqBands("Game".into(), 0, new_bands.clone())]).unwrap();

        match &snapshot.groups[0].dsp[0].spec {
            DspSpec::Eq { bands } => assert_eq!(bands, &new_bands),
            other => panic!("expected an Eq stage, got {other:?}"),
        }
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("# master volume"), "comment must survive an edit");
    }

    const BASE_TWO_GROUPS: &str = r#"
schema_version = 2
master = 0.8

[[group]]
name = "Game"
output_device = "Speakers"

[[group]]
name = "Voice"
output_device = "Speakers"
"#;

    #[test]
    fn set_spatial_then_unset_round_trips_and_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let snapshot = store
            .apply(&[ConfigEdit::SetSpatial("Game".into(), true)])
            .unwrap();
        assert!(snapshot.groups[0].spatial);
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("# master volume"), "comment must survive an edit");
        assert!(on_disk.contains("spatial = true"));

        let snapshot = store.apply(&[ConfigEdit::SetSpatial("Game".into(), false)]).unwrap();
        assert!(!snapshot.groups[0].spatial);
    }

    #[test]
    fn set_group_mute_then_unset_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let snapshot = store
            .apply(&[ConfigEdit::SetGroupMute("Game".into(), true)])
            .unwrap();
        assert!(snapshot.groups[0].muted);
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("# master volume"), "comment must survive an edit");
        assert!(on_disk.contains("muted = true"));

        let snapshot = store.apply(&[ConfigEdit::SetGroupMute("Game".into(), false)]).unwrap();
        assert!(!snapshot.groups[0].muted);
    }

    #[test]
    fn set_autostart_round_trips_against_an_existing_app_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            "schema_version = 2\nmaster = 1.0\n\n[app]\nautostart = false\n",
        );
        let mut store = ConfigStore::open(&path).unwrap();

        let snapshot = store.apply(&[ConfigEdit::SetAutostart(true)]).unwrap();
        assert!(snapshot.app.autostart);

        let snapshot = store.apply(&[ConfigEdit::SetAutostart(false)]).unwrap();
        assert!(!snapshot.app.autostart);
    }

    #[test]
    fn set_autostart_creates_the_app_table_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let snapshot = store.apply(&[ConfigEdit::SetAutostart(true)]).unwrap();

        assert!(snapshot.app.autostart);
    }

    #[test]
    fn set_duck_then_clear_duck_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE_TWO_GROUPS);
        let mut store = ConfigStore::open(&path).unwrap();

        let snapshot = store
            .apply(&[ConfigEdit::SetDuck(
                "Game".into(),
                Some(DuckSpecConfig {
                    trigger: "Voice".into(),
                    amount_db: 6.0,
                    threshold_db: -40.0,
                    attack_ms: 5.0,
                    release_ms: 200.0,
                }),
            )])
            .unwrap();
        let duck = snapshot.groups[0].duck.as_ref().expect("duck should be set");
        assert_eq!(duck.trigger, "Voice");

        let snapshot = store.apply(&[ConfigEdit::SetDuck("Game".into(), None)]).unwrap();
        assert!(snapshot.groups[0].duck.is_none());
    }

    #[test]
    fn set_eq_band_against_a_hand_written_inline_array_bands_shape_errors_not_panics() {
        // Regression test for a review finding: `bands = [{...}]` (a valid,
        // parseable TOML shape — serde doesn't distinguish it from
        // `[[group.dsp.bands]]`) used to panic `SetEqBand` via an `.expect()`
        // on `as_array_of_tables_mut()`. Must return a StoreError instead.
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            r#"
schema_version = 2
master = 1.0

[[group]]
name = "Game"
output_device = "Out"

[[group.dsp]]
type = "eq"
bands = [{ freq_hz = 200.0, gain_db = 3.0, q = 0.7 }]
"#,
        );
        let mut store = ConfigStore::open(&path).unwrap();
        let result = store.apply(&[ConfigEdit::SetEqBand(
            "Game".into(),
            0,
            0,
            EqBandSpec {
                freq_hz: 500.0,
                gain_db: 1.0,
                q: 1.0,
            },
        )]);
        assert!(matches!(result, Err(StoreError::Validation(_))));
    }

    #[test]
    fn add_dsp_stage_against_a_hand_written_inline_array_dsp_shape_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            r#"
schema_version = 2
master = 1.0

[[group]]
name = "Game"
output_device = "Out"
dsp = [{ type = "limiter", ceiling_db = -1.0 }]
"#,
        );
        let mut store = ConfigStore::open(&path).unwrap();
        let result = store.apply(&[ConfigEdit::AddDspStage(
            "Game".into(),
            DspSpec::Limiter { ceiling_db: -2.0 },
        )]);
        assert!(matches!(result, Err(StoreError::Validation(_))));
    }

    #[test]
    fn set_dsp_chain_replaces_the_whole_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        store
            .apply(&[ConfigEdit::AddDspStage(
                "Game".into(),
                DspSpec::Limiter { ceiling_db: -1.0 },
            )])
            .unwrap();

        let new_chain = vec![engine::DspStageConfig {
            spec: DspSpec::Eq { bands: vec![EqBandSpec { freq_hz: 500.0, gain_db: 2.0, q: 1.0 }] },
            bypassed: true,
        }];
        let snapshot = store.apply(&[ConfigEdit::SetDspChain("Game".into(), new_chain)]).unwrap();

        assert_eq!(snapshot.groups[0].dsp.len(), 1, "old limiter stage replaced, not appended to");
        match &snapshot.groups[0].dsp[0].spec {
            DspSpec::Eq { bands } => assert_eq!(bands[0].freq_hz, 500.0),
            other => panic!("expected the replacement Eq stage, got {other:?}"),
        }
        assert!(snapshot.groups[0].dsp[0].bypassed);
    }

    fn sample_profile(name: &str) -> ProfileConfig {
        ProfileConfig {
            name: name.into(),
            hotkey: Some(HotkeyChord::parse("Ctrl+Alt+1").unwrap()),
            master: Gain::new(0.8).unwrap(),
            muted: false,
            groups: vec![ProfileGroupConfig {
                name: "Game".into(),
                gain: Gain::new(0.5).unwrap(),
                follow_master: true,
                output_device: "Headphones".into(),
                dsp: Vec::new(),
                duck: None,
                spatial: true,
                muted: false,
            }],
        }
    }

    #[test]
    fn set_profile_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let snapshot = store.apply(&[ConfigEdit::SetProfile(sample_profile("Gaming"))]).unwrap();

        assert_eq!(snapshot.profiles.len(), 1);
        let p = &snapshot.profiles[0];
        assert_eq!(p.name, "Gaming");
        assert_eq!(p.hotkey, Some(HotkeyChord::parse("Ctrl+Alt+1").unwrap()));
        assert_eq!(p.master, Gain::new(0.8).unwrap());
        assert_eq!(p.groups[0].output_device, "Headphones");
        assert!(p.groups[0].spatial);

        // Re-applying with the same name upserts in place, not a duplicate.
        let mut updated = sample_profile("Gaming");
        updated.master = Gain::UNITY;
        let snapshot = store.apply(&[ConfigEdit::SetProfile(updated)]).unwrap();
        assert_eq!(snapshot.profiles.len(), 1, "SetProfile upserts by name");
        assert_eq!(snapshot.profiles[0].master, Gain::UNITY);
    }

    #[test]
    fn remove_profile_leaves_other_profiles_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        store
            .apply(&[
                ConfigEdit::SetProfile(sample_profile("Gaming")),
                ConfigEdit::SetProfile(sample_profile("Music")),
            ])
            .unwrap();

        let snapshot = store.apply(&[ConfigEdit::RemoveProfile("Gaming".into())]).unwrap();
        assert_eq!(snapshot.profiles.len(), 1);
        assert_eq!(snapshot.profiles[0].name, "Music");
    }

    #[test]
    fn remove_profile_for_an_unknown_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let result = store.apply(&[ConfigEdit::RemoveProfile("Nonexistent".into())]);
        assert!(matches!(result, Err(StoreError::Validation(_))));
    }

    #[test]
    fn set_active_profile_then_clear_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), BASE);
        let mut store = ConfigStore::open(&path).unwrap();

        let snapshot = store
            .apply(&[ConfigEdit::SetActiveProfile(Some("Gaming".into()))])
            .unwrap();
        assert_eq!(snapshot.app.active_profile, Some("Gaming".to_string()));

        let snapshot = store.apply(&[ConfigEdit::SetActiveProfile(None)]).unwrap();
        assert_eq!(snapshot.app.active_profile, None);
    }

    #[test]
    fn every_config_edit_variant_has_an_edit_path() {
        // Regression for decision 12: the classifier is a plain `match` with
        // no wildcard arm, so a new `ConfigEdit` variant is a compile error
        // here until classified. This test pins the actual classification
        // per group, not just that one exists.
        let structural = [
            ConfigEdit::SetGroupOutput("g".into(), "d".into()),
            ConfigEdit::AddGroup(GroupConfig {
                name: "g".into(),
                output_device: "d".into(),
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
            }),
            ConfigEdit::RemoveGroup("g".into()),
        ];
        for edit in &structural {
            assert_eq!(edit_path(edit), EditPath::Structural);
        }

        assert_eq!(edit_path(&ConfigEdit::SetSpatial("g".into(), true)), EditPath::Spatial);

        let dsp_chain = [
            ConfigEdit::AddDspStage("g".into(), DspSpec::Limiter { ceiling_db: -1.0 }),
            ConfigEdit::RemoveDspStage("g".into(), 0),
            ConfigEdit::SetEqBands("g".into(), 0, vec![]),
            ConfigEdit::SetDspChain("g".into(), vec![]),
        ];
        for edit in &dsp_chain {
            assert_eq!(edit_path(edit), EditPath::DspChain);
        }

        let param = [
            ConfigEdit::SetGroupGain("g".into(), Gain::UNITY),
            ConfigEdit::SetMaster(Gain::UNITY),
            ConfigEdit::SetMuted(true),
            ConfigEdit::SetProfile(sample_profile("Gaming")),
            ConfigEdit::RemoveProfile("g".into()),
            ConfigEdit::SetActiveProfile(None),
        ];
        for edit in &param {
            assert_eq!(edit_path(edit), EditPath::Param);
        }
    }
}
