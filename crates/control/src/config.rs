//! TOML load/validate, diff against a prior snapshot, and file-watch hot-reload.
//! `ConfigSnapshot`/`GroupConfig` are `engine`'s types (see
//! `.lattice/context/engine-core.md` — "Config type home" decision):
//! `engine` is the consumer that resolves names to endpoint ids, so it owns
//! the shape; `control` only knows how to produce one from TOML.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use serde::Deserialize;

use audio_core::{Gain, GroupId, MixerCommand};
use engine::{AppConfig, ConfigSnapshot, GroupConfig, GroupRules, HotkeyChord, HotkeyMap, MatchRule};

pub const SUPPORTED_SCHEMA_VERSION: u32 = 2;

#[derive(Debug)]
pub enum ConfigError {
    Io(String),
    Parse(String),
    /// Parsed fine but failed validation (bad gain, unsupported schema version, ...).
    /// Callers keep using the prior snapshot on this variant.
    Invalid(String),
}

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default = "current_schema_version")]
    schema_version: u32,
    #[serde(default = "default_gain")]
    master: f32,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    group: Vec<RawGroup>,
    #[serde(default)]
    app: RawAppConfig,
    #[serde(default)]
    hotkeys: RawHotkeys,
}

#[derive(Deserialize, Default)]
struct RawAppConfig {
    #[serde(default)]
    autostart: bool,
}

#[derive(Deserialize, Default)]
struct RawHotkeys {
    #[serde(default)]
    mute_master: Option<String>,
}

#[derive(Deserialize)]
struct RawGroup {
    name: String,
    bus_endpoint: String,
    output_device: String,
    #[serde(default = "default_gain")]
    gain: f32,
    #[serde(default)]
    follow_master: bool,
    #[serde(default)]
    match_rules: Vec<String>,
}

fn current_schema_version() -> u32 {
    SUPPORTED_SCHEMA_VERSION
}

fn default_gain() -> f32 {
    1.0
}

pub fn load(path: &Path) -> Result<ConfigSnapshot, ConfigError> {
    let text = fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
    parse(&text)
}

pub(crate) fn parse(text: &str) -> Result<ConfigSnapshot, ConfigError> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;

    if raw.schema_version > SUPPORTED_SCHEMA_VERSION {
        return Err(ConfigError::Invalid(format!(
            "schema_version {} is newer than supported {SUPPORTED_SCHEMA_VERSION}",
            raw.schema_version
        )));
    }

    let master = Gain::new(raw.master).map_err(|e| ConfigError::Invalid(e.to_string()))?;
    let groups = raw
        .group
        .into_iter()
        .map(|g| {
            let gain = Gain::new(g.gain).map_err(|e| ConfigError::Invalid(e.to_string()))?;
            Ok(GroupConfig {
                name: g.name,
                bus_endpoint: g.bus_endpoint,
                output_device: g.output_device,
                gain,
                follow_master: g.follow_master,
                match_rules: g.match_rules,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;

    let mute_master = raw
        .hotkeys
        .mute_master
        .as_deref()
        .map(HotkeyChord::parse)
        .transpose()
        .map_err(ConfigError::Invalid)?;

    Ok(ConfigSnapshot {
        schema_version: raw.schema_version,
        master,
        muted: raw.muted,
        groups,
        app: AppConfig {
            autostart: raw.app.autostart,
            hotkeys: HotkeyMap { mute_master },
        },
    })
}

/// Struct, not a flat enum (session-routing 2026-07-20 decision): a single
/// config save can change a gain (`params`) and a match rule (`rules`) at
/// once, and both must reach the caller from one `diff()` call — a flat
/// enum can only carry one variant, silently dropping whichever axis it
/// doesn't return. `structural` short-circuits the other two fields (a
/// rebuild makes positional `GroupId`s and any params/rules delta for the
/// pre-rebuild topology moot).
#[derive(Default)]
pub struct ConfigDelta {
    pub structural: bool,
    pub params: Vec<MixerCommand>,
    pub rules: Option<Vec<GroupRules>>,
}

impl ConfigDelta {
    pub fn is_unchanged(&self) -> bool {
        !self.structural && self.params.is_empty() && self.rules.is_none()
    }
}

/// `GroupRules` for every group in `snapshot`, in config order — the same
/// positional `GroupId` convention `engine::graph::resolve` and `diff` use.
/// Used both by `diff` (when `match_rules` changed) and by callers building
/// the initial `Vec<GroupRules>` for `start_routing`/`update_topology`.
pub fn group_rules(snapshot: &ConfigSnapshot) -> Vec<GroupRules> {
    snapshot
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| GroupRules {
            group: GroupId(i as u16),
            rules: g.match_rules.iter().map(|r| MatchRule::parse(r)).collect(),
        })
        .collect()
}

/// `GroupId`s are derived from group position, matching `engine::graph::resolve`'s
/// own convention — valid as long as `old` is the snapshot the engine was last
/// built/rebuilt from (true for the intended `load` → `diff` → `apply_params`/
/// `rebuild` flow; see the "engine wires" line in `.lattice/context/engine-core.md`).
pub fn diff(old: &ConfigSnapshot, new: &ConfigSnapshot) -> ConfigDelta {
    let old_names: Vec<&str> = old.groups.iter().map(|g| g.name.as_str()).collect();
    let new_names: Vec<&str> = new.groups.iter().map(|g| g.name.as_str()).collect();
    if old_names != new_names {
        return ConfigDelta {
            structural: true,
            ..ConfigDelta::default()
        };
    }

    let mut params = Vec::new();
    let mut rules_changed = false;
    if old.master != new.master {
        params.push(MixerCommand::SetMaster(new.master));
    }

    for (i, (o, n)) in old.groups.iter().zip(new.groups.iter()).enumerate() {
        if o.bus_endpoint != n.bus_endpoint || o.output_device != n.output_device {
            return ConfigDelta {
                structural: true,
                ..ConfigDelta::default()
            };
        }
        let id = GroupId(i as u16);
        if o.gain != n.gain {
            params.push(MixerCommand::SetGroupGain(id, n.gain));
        }
        if o.follow_master != n.follow_master {
            params.push(MixerCommand::SetFollowMaster(id, n.follow_master));
        }
        if o.match_rules != n.match_rules {
            rules_changed = true;
        }
    }

    ConfigDelta {
        structural: false,
        params,
        rules: rules_changed.then(|| group_rules(new)),
    }
}

/// `notify`-based; runs its debounce loop on a plain control thread, never RT.
pub struct ConfigWatcher {
    _watcher: notify::RecommendedWatcher,
}

impl ConfigWatcher {
    pub fn spawn(path: &Path) -> Result<(ConfigWatcher, Receiver<ConfigSnapshot>), ConfigError> {
        use notify::Watcher;

        let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = raw_tx.send(res);
        })
        .map_err(|e| ConfigError::Io(e.to_string()))?;

        // Watch the parent directory, not the file directly: editors commonly
        // save via write-then-rename, which some watch backends lose track of
        // on a direct file watch.
        let watch_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        watcher
            .watch(watch_dir, notify::RecursiveMode::NonRecursive)
            .map_err(|e| ConfigError::Io(e.to_string()))?;

        let (out_tx, out_rx) = mpsc::channel::<ConfigSnapshot>();
        let target = path.to_path_buf();
        thread::spawn(move || debounce_loop(raw_rx, out_tx, target));

        Ok((ConfigWatcher { _watcher: watcher }, out_rx))
    }
}

const DEBOUNCE: Duration = Duration::from_millis(100);
const IDLE_POLL: Duration = Duration::from_secs(3600);

/// A single save can fire several raw events (write, then rename, ...) — wait
/// for a quiet window before re-reading, and re-read once (notes §15).
fn debounce_loop(
    raw_rx: Receiver<notify::Result<notify::Event>>,
    out_tx: Sender<ConfigSnapshot>,
    target: PathBuf,
) {
    let file_name = target.file_name().map(|n| n.to_owned());
    let mut pending = false;

    loop {
        let timeout = if pending { DEBOUNCE } else { IDLE_POLL };
        match raw_rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                let relevant = event
                    .paths
                    .iter()
                    .any(|p| p.file_name().map(|n| n.to_owned()) == file_name);
                if relevant {
                    pending = true;
                }
            }
            Ok(Err(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                if pending {
                    pending = false;
                    if let Ok(snapshot) = load(&target) {
                        if out_tx.send(snapshot).is_err() {
                            return; // receiver dropped — stop watching
                        }
                    }
                    // Err: invalid edit — keep prior snapshot by sending nothing.
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: &str, bus: &str, output: &str, gain: f32, follow_master: bool) -> GroupConfig {
        GroupConfig {
            name: name.into(),
            bus_endpoint: bus.into(),
            output_device: output.into(),
            gain: Gain::new(gain).unwrap(),
            follow_master,
            match_rules: vec![],
        }
    }

    #[test]
    fn parses_a_valid_config() {
        let toml = r#"
            schema_version = 2
            master = 0.8

            [[group]]
            name = "Game"
            bus_endpoint = "Splitstream Game"
            output_device = "Speakers"
            gain = 1.0
            follow_master = true
        "#;
        let snapshot = parse(toml).unwrap();
        assert_eq!(snapshot.schema_version, 2);
        assert_eq!(snapshot.master, Gain::new(0.8).unwrap());
        assert_eq!(snapshot.groups.len(), 1);
        assert_eq!(snapshot.groups[0].name, "Game");
    }

    #[test]
    fn parses_muted_app_and_hotkeys() {
        let toml = r#"
            schema_version = 2
            master = 0.8
            muted = true

            [app]
            autostart = true

            [hotkeys]
            mute_master = "Ctrl+Alt+M"
        "#;
        let snapshot = parse(toml).unwrap();
        assert!(snapshot.muted);
        assert!(snapshot.app.autostart);
        assert_eq!(
            snapshot.app.hotkeys.mute_master,
            Some(HotkeyChord {
                ctrl: true,
                alt: true,
                shift: false,
                key: 'M',
            })
        );
    }

    #[test]
    fn missing_muted_app_and_hotkeys_default_to_unset() {
        let toml = "schema_version = 2\nmaster = 1.0\n";
        let snapshot = parse(toml).unwrap();
        assert!(!snapshot.muted);
        assert!(!snapshot.app.autostart);
        assert!(snapshot.app.hotkeys.mute_master.is_none());
    }

    #[test]
    fn invalid_hotkey_chord_is_a_validation_error() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [hotkeys]
            mute_master = "NotAChord"
        "#;
        assert!(matches!(parse(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn missing_schema_version_defaults_to_supported() {
        let toml = r#"
            master = 1.0
        "#;
        let snapshot = parse(toml).unwrap();
        assert_eq!(snapshot.schema_version, SUPPORTED_SCHEMA_VERSION);
    }

    #[test]
    fn schema_version_newer_than_supported_is_invalid() {
        let toml = "schema_version = 99\nmaster = 1.0\n";
        assert!(matches!(parse(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn negative_gain_is_invalid() {
        let toml = "schema_version = 2\nmaster = -1.0\n";
        assert!(matches!(parse(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn malformed_toml_is_a_parse_error() {
        let toml = "this is not [ valid toml";
        assert!(matches!(parse(toml), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let result = load(Path::new("/no/such/path/splitstream.toml"));
        assert!(matches!(result, Err(ConfigError::Io(_))));
    }

    fn group_with_rules(
        name: &str,
        bus: &str,
        output: &str,
        gain: f32,
        follow_master: bool,
        rules: &[&str],
    ) -> GroupConfig {
        GroupConfig {
            match_rules: rules.iter().map(|r| r.to_string()).collect(),
            ..group(name, bus, output, gain, follow_master)
        }
    }

    #[test]
    fn diff_reports_unchanged_for_identical_snapshots() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group("Game", "Bus", "Out", 1.0, true)],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group("Game", "Bus", "Out", 1.0, true)],
        };
        assert!(diff(&a, &b).is_unchanged());
    }

    #[test]
    fn diff_reports_params_for_a_gain_only_change() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group("Game", "Bus", "Out", 1.0, true)],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group("Game", "Bus", "Out", 0.5, true)],
        };
        let delta = diff(&a, &b);
        assert!(!delta.structural);
        assert!(delta.rules.is_none());
        assert_eq!(delta.params.len(), 1);
        assert!(matches!(delta.params[0], MixerCommand::SetGroupGain(GroupId(0), _)));
    }

    #[test]
    fn diff_reports_structural_for_a_renamed_group() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group("Game", "Bus", "Out", 1.0, true)],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group("Music", "Bus", "Out", 1.0, true)],
        };
        let delta = diff(&a, &b);
        assert!(delta.structural);
        assert!(delta.params.is_empty());
        assert!(delta.rules.is_none());
    }

    #[test]
    fn diff_reports_structural_for_a_changed_output_device() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group("Game", "Bus", "Speakers", 1.0, true)],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group("Game", "Bus", "Headphones", 1.0, true)],
        };
        assert!(diff(&a, &b).structural);
    }

    #[test]
    fn diff_reports_rules_for_a_match_rules_only_change() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group_with_rules("Game", "Bus", "Out", 1.0, true, &[])],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group_with_rules("Game", "Bus", "Out", 1.0, true, &["game.exe"])],
        };
        let delta = diff(&a, &b);
        assert!(!delta.structural);
        assert!(delta.params.is_empty());
        let rules = delta.rules.expect("expected a rules delta");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].group, GroupId(0));
        assert_eq!(rules[0].rules, vec![MatchRule::ExactName("game.exe".into())]);
    }

    #[test]
    fn diff_reports_both_params_and_rules_for_a_simultaneous_edit() {
        // Regression case for the ConfigDelta restructure: a single save
        // changing both a gain and a match rule must not silently drop
        // either half (the flat-enum shape this replaced could only return one).
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group_with_rules("Game", "Bus", "Out", 1.0, true, &[])],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![group_with_rules("Game", "Bus", "Out", 0.5, true, &["game.exe"])],
        };
        let delta = diff(&a, &b);
        assert!(!delta.structural);
        assert_eq!(delta.params.len(), 1);
        assert!(matches!(delta.params[0], MixerCommand::SetGroupGain(GroupId(0), _)));
        assert!(delta.rules.is_some(), "rules change must not be dropped");
    }

    #[test]
    fn group_rules_builds_one_entry_per_group_in_config_order() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            groups: vec![
                group_with_rules("Game", "Bus", "Out", 1.0, true, &["game.exe"]),
                group_with_rules("Music", "Bus", "Out", 1.0, true, &["music*.exe"]),
            ],
        };
        let rules = group_rules(&snapshot);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].group, GroupId(0));
        assert_eq!(rules[0].rules, vec![MatchRule::ExactName("game.exe".into())]);
        assert_eq!(rules[1].group, GroupId(1));
        assert_eq!(
            rules[1].rules,
            vec![MatchRule::Glob(engine::GlobPattern::new("music*.exe"))]
        );
    }

    #[test]
    fn watcher_delivers_a_snapshot_after_a_file_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splitstream.toml");
        fs::write(
            &path,
            r#"
            schema_version = 2
            master = 1.0
        "#,
        )
        .unwrap();

        let (_watcher, rx) = ConfigWatcher::spawn(&path).unwrap();

        fs::write(
            &path,
            r#"
            schema_version = 2
            master = 0.42
        "#,
        )
        .unwrap();

        let snapshot = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher should deliver a snapshot after the file changes");
        assert_eq!(snapshot.master, Gain::new(0.42).unwrap());
    }
}
