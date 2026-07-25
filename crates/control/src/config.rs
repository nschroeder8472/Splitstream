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

use audio_core::{DspSpec, DuckSpec, EqBandSpec, Gain, GroupId, MixerCommand};
use engine::{
    AccentChoice, AppConfig, ConfigSnapshot, DspStageConfig, DuckSpecConfig, GroupConfig, GroupRules,
    HotkeyChord, HotkeyMap, MatchRule, ProfileConfig, ProfileGroupConfig, ThemeChoice,
};

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
    /// `[[profile]]` tables (profiles.md) — purely additive, no schema bump.
    #[serde(default)]
    profile: Vec<RawProfile>,
}

#[derive(Deserialize, Default)]
struct RawAppConfig {
    #[serde(default)]
    autostart: bool,
    #[serde(default)]
    active_profile: Option<String>,
    #[serde(default)]
    volume_bind: Option<String>,
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    accent: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawHotkeys {
    #[serde(default)]
    mute_master: Option<String>,
    #[serde(default)]
    push_to_mute: Option<String>,
    #[serde(default)]
    master_volume_up: Option<String>,
    #[serde(default)]
    master_volume_down: Option<String>,
}

#[derive(Deserialize)]
struct RawGroup {
    name: String,
    output_device: String,
    #[serde(default = "default_gain")]
    gain: f32,
    #[serde(default)]
    follow_master: bool,
    #[serde(default)]
    match_rules: Vec<String>,
    #[serde(default)]
    dsp: Vec<RawDspStage>,
    #[serde(default)]
    duck: Option<RawDuck>,
    #[serde(default)]
    spatial: bool,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    hotkey_mute: Option<String>,
    #[serde(default)]
    hotkey_volume_up: Option<String>,
    #[serde(default)]
    hotkey_volume_down: Option<String>,
}

/// TOML shape for `[[group.dsp]]` (spec §11.3). `bypassed` has no place on
/// `audio_core::DspSpec` (pure construction input) — see
/// `engine::DspStageConfig`'s doc comment for why it's paired here instead.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum RawDspStage {
    Eq {
        bands: Vec<RawEqBand>,
        #[serde(default)]
        bypassed: bool,
    },
    Limiter {
        ceiling_db: f32,
        #[serde(default)]
        bypassed: bool,
    },
}

#[derive(Deserialize)]
struct RawEqBand {
    freq_hz: f32,
    gain_db: f32,
    q: f32,
}

#[derive(Deserialize)]
struct RawDuck {
    trigger: String,
    amount_db: f32,
    threshold_db: f32,
    attack_ms: f32,
    release_ms: f32,
}

/// A `[[profile]]` table (profiles.md) — a named, partial snapshot of
/// per-group state. `group` entries are a subset of the live groups by
/// design (L1 capability 11): a name absent from the config is skipped on
/// apply, not an error, so no whole-snapshot validation runs over these the
/// way [`validate_duck_config`] does for live groups.
#[derive(Deserialize)]
struct RawProfile {
    name: String,
    #[serde(default)]
    hotkey: Option<String>,
    #[serde(default = "default_gain")]
    master: f32,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    group: Vec<RawProfileGroup>,
}

/// A `[[profile.group]]` table — same shape as [`RawGroup`] minus
/// `match_rules` (decision 8: match rules stay shared, not per-profile).
#[derive(Deserialize)]
struct RawProfileGroup {
    name: String,
    output_device: String,
    #[serde(default = "default_gain")]
    gain: f32,
    #[serde(default)]
    follow_master: bool,
    #[serde(default)]
    dsp: Vec<RawDspStage>,
    #[serde(default)]
    duck: Option<RawDuck>,
    #[serde(default)]
    spatial: bool,
    #[serde(default)]
    muted: bool,
}

fn current_schema_version() -> u32 {
    SUPPORTED_SCHEMA_VERSION
}

fn default_gain() -> f32 {
    1.0
}

fn parse_hotkey(raw: Option<String>) -> Result<Option<HotkeyChord>, ConfigError> {
    raw.as_deref().map(HotkeyChord::parse).transpose().map_err(ConfigError::Invalid)
}

/// `None` (key absent) resolves to [`ThemeChoice::default`], matching every
/// other optional scalar in `[app]`; a *present but unrecognised* string is
/// `ConfigError::Invalid`, same shape as [`parse_hotkey`].
fn parse_theme(raw: Option<String>) -> Result<ThemeChoice, ConfigError> {
    match raw.as_deref() {
        None => Ok(ThemeChoice::default()),
        Some("dark") => Ok(ThemeChoice::Dark),
        Some("light") => Ok(ThemeChoice::Light),
        Some("system") => Ok(ThemeChoice::System),
        Some(other) => Err(ConfigError::Invalid(format!(
            "unrecognised theme {other:?} (expected \"dark\", \"light\", or \"system\")"
        ))),
    }
}

/// See [`parse_theme`] — same default-on-absent, error-on-unrecognised shape.
fn parse_accent(raw: Option<String>) -> Result<AccentChoice, ConfigError> {
    match raw.as_deref() {
        None => Ok(AccentChoice::default()),
        Some("brand") => Ok(AccentChoice::Brand),
        Some("teal") => Ok(AccentChoice::Teal),
        Some("amber") => Ok(AccentChoice::Amber),
        Some("violet") => Ok(AccentChoice::Violet),
        Some("slate") => Ok(AccentChoice::Slate),
        Some(other) => Err(ConfigError::Invalid(format!(
            "unrecognised accent {other:?} (expected \"brand\", \"teal\", \"amber\", \"violet\", or \"slate\")"
        ))),
    }
}

fn convert_duck(d: RawDuck) -> DuckSpecConfig {
    DuckSpecConfig {
        trigger: d.trigger,
        amount_db: d.amount_db,
        threshold_db: d.threshold_db,
        attack_ms: d.attack_ms,
        release_ms: d.release_ms,
    }
}

fn convert_dsp_stage(raw: RawDspStage) -> Result<DspStageConfig, ConfigError> {
    match raw {
        RawDspStage::Eq { bands, bypassed } => {
            let bands = bands
                .into_iter()
                .map(|b| {
                    let band = EqBandSpec {
                        freq_hz: b.freq_hz,
                        gain_db: b.gain_db,
                        q: b.q,
                    };
                    validate_eq_band_shape(&band)?;
                    Ok(band)
                })
                .collect::<Result<Vec<_>, ConfigError>>()?;
            Ok(DspStageConfig {
                spec: DspSpec::Eq { bands },
                bypassed,
            })
        }
        RawDspStage::Limiter { ceiling_db, bypassed } => Ok(DspStageConfig {
            spec: DspSpec::Limiter { ceiling_db },
            bypassed,
        }),
    }
}

/// Partial validation only — freq/Q sanity, not the nyquist upper bound
/// (that needs the group's resolved sample rate, unknown until `engine`
/// resolves the bus endpoint against a live device). `DspChain::new` catches
/// the nyquist bound later as a `DomainError` — defense in depth, not
/// duplicated logic, since this layer genuinely can't check it yet.
fn validate_eq_band_shape(band: &EqBandSpec) -> Result<(), ConfigError> {
    if band.freq_hz > 0.0 && band.q > 0.0 && band.q.is_finite() && band.gain_db.is_finite() {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!(
            "invalid EQ band: freq_hz {} and q {} must be positive and finite",
            band.freq_hz, band.q
        )))
    }
}

/// Whole-snapshot duck validation (spec: "duck cycles, unknown triggers ...
/// → ConfigError::Invalid"): every configured `trigger` name must resolve to
/// another group in the same snapshot, and following the trigger chain from
/// any duck-configured group must never lead back to itself.
fn validate_duck_config(groups: &[GroupConfig]) -> Result<(), ConfigError> {
    let names: std::collections::HashSet<&str> = groups.iter().map(|g| g.name.as_str()).collect();
    let trigger_of: std::collections::HashMap<&str, &str> = groups
        .iter()
        .filter_map(|g| g.duck.as_ref().map(|d| (g.name.as_str(), d.trigger.as_str())))
        .collect();

    for (name, trigger) in &trigger_of {
        if !names.contains(trigger) {
            return Err(ConfigError::Invalid(format!(
                "group '{name}': duck trigger '{trigger}' not found"
            )));
        }
    }

    for &start in trigger_of.keys() {
        let mut current = start;
        for _ in 0..trigger_of.len() {
            match trigger_of.get(current) {
                Some(&next) if next == start => {
                    return Err(ConfigError::Invalid(format!(
                        "duck cycle detected involving group '{start}'"
                    )));
                }
                Some(&next) => current = next,
                None => break, // chain ends at a group with no duck config — no cycle through here
            }
        }
    }
    Ok(())
}

pub fn load(path: &Path) -> Result<ConfigSnapshot, ConfigError> {
    let text = fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
    parse(&text)
}

/// Machine-neutral seed (simple-launch.md L4): no `[[group]]` — baking a
/// device name into the shipped template would be wrong on every machine but
/// the one it was written on. Onboarding (app-shell) adds the first group
/// once the user picks an output device (process-loopback-capture pivot:
/// no virtual bus to pick anymore — just an output). Body built from
/// `SUPPORTED_SCHEMA_VERSION` rather than a second hardcoded literal, so a
/// future schema bump can't leave the seed silently stale.
fn default_config_template() -> String {
    format!(
        "# Splitstream configuration.\n\
         # No audio group is configured yet — first-run onboarding adds one once you\n\
         # pick an output device to route apps through.\n\
         schema_version = {SUPPORTED_SCHEMA_VERSION}\n\
         master = 1.0\n\
         muted = false\n\
         \n\
         [app]\n\
         autostart = true\n"
    )
}

/// Create-if-missing then load (simple-launch.md L4). Never touches an
/// existing file — only writes the seed when `path` doesn't exist yet.
pub fn ensure_config(path: &Path) -> Result<ConfigSnapshot, ConfigError> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ConfigError::Io(e.to_string()))?;
        }
        crate::atomic_write::write_atomic(path, &default_config_template()).map_err(ConfigError::Io)?;
    }
    load(path)
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
            let dsp = g
                .dsp
                .into_iter()
                .map(convert_dsp_stage)
                .collect::<Result<Vec<_>, ConfigError>>()?;
            let duck = g.duck.map(convert_duck);
            Ok(GroupConfig {
                name: g.name,
                output_device: g.output_device,
                gain,
                follow_master: g.follow_master,
                match_rules: g.match_rules,
                dsp,
                duck,
                spatial: g.spatial,
                muted: g.muted,
                hotkey_mute: parse_hotkey(g.hotkey_mute)?,
                hotkey_volume_up: parse_hotkey(g.hotkey_volume_up)?,
                hotkey_volume_down: parse_hotkey(g.hotkey_volume_down)?,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;
    validate_duck_config(&groups)?;

    let mute_master = parse_hotkey(raw.hotkeys.mute_master)?;
    let push_to_mute = parse_hotkey(raw.hotkeys.push_to_mute)?;
    let master_volume_up = parse_hotkey(raw.hotkeys.master_volume_up)?;
    let master_volume_down = parse_hotkey(raw.hotkeys.master_volume_down)?;

    let profiles = raw
        .profile
        .into_iter()
        .map(convert_profile)
        .collect::<Result<Vec<_>, ConfigError>>()?;

    Ok(ConfigSnapshot {
        schema_version: raw.schema_version,
        master,
        muted: raw.muted,
        groups,
        app: AppConfig {
            autostart: raw.app.autostart,
            hotkeys: HotkeyMap { mute_master, push_to_mute, master_volume_up, master_volume_down },
            active_profile: raw.app.active_profile,
            volume_bind: raw.app.volume_bind,
            theme: parse_theme(raw.app.theme)?,
            accent: parse_accent(raw.app.accent)?,
        },
        profiles,
    })
}

fn convert_profile(raw: RawProfile) -> Result<ProfileConfig, ConfigError> {
    let master = Gain::new(raw.master).map_err(|e| ConfigError::Invalid(e.to_string()))?;
    let hotkey = raw
        .hotkey
        .as_deref()
        .map(HotkeyChord::parse)
        .transpose()
        .map_err(ConfigError::Invalid)?;
    let groups = raw
        .group
        .into_iter()
        .map(|g| {
            let gain = Gain::new(g.gain).map_err(|e| ConfigError::Invalid(e.to_string()))?;
            let dsp = g
                .dsp
                .into_iter()
                .map(convert_dsp_stage)
                .collect::<Result<Vec<_>, ConfigError>>()?;
            let duck = g.duck.map(convert_duck);
            Ok(ProfileGroupConfig {
                name: g.name,
                output_device: g.output_device,
                gain,
                follow_master: g.follow_master,
                dsp,
                duck,
                spatial: g.spatial,
                muted: g.muted,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;

    Ok(ProfileConfig {
        name: raw.name,
        hotkey,
        master,
        muted: raw.muted,
        groups,
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
    /// Per-group DSP chain shape changes (a stage added/removed/reordered) —
    /// funnels through `EngineHandle::apply_dsp_chains`'s RT-safe swap path,
    /// not a full rebuild. A pure param tweak (existing stage's band/ceiling
    /// value) or a bypass toggle stays in `params` instead (see `diff`).
    pub dsp_chains: Option<Vec<(GroupId, Vec<DspSpec>)>>,
    /// Spatial-audio toggle changes — funnels through
    /// `EngineHandle::apply_spatial`'s off-thread `Render` rebuild + swap
    /// path (spatial-audio.md), not a full rebuild and not a plain
    /// `MixerCommand` (unlike `params`, building a `Render` needs the
    /// group's current topology, same reason `dsp_chains` isn't in `params`).
    pub spatial: Option<Vec<(GroupId, bool)>>,
}

impl ConfigDelta {
    pub fn is_unchanged(&self) -> bool {
        !self.structural
            && self.params.is_empty()
            && self.rules.is_none()
            && self.dsp_chains.is_none()
            && self.spatial.is_none()
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
    let mut dsp_chains: Vec<(GroupId, Vec<DspSpec>)> = Vec::new();
    let mut spatial: Vec<(GroupId, bool)> = Vec::new();
    if old.master != new.master {
        params.push(MixerCommand::SetMaster(new.master));
    }

    for (i, (o, n)) in old.groups.iter().zip(new.groups.iter()).enumerate() {
        if o.output_device != n.output_device {
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
        if o.muted != n.muted {
            params.push(MixerCommand::SetGroupMute(id, n.muted));
        }
        if o.match_rules != n.match_rules {
            rules_changed = true;
        }
        if o.spatial != n.spatial {
            spatial.push((id, n.spatial));
        }

        // Stage count/type/order/param changed — an RT-safe chain swap, not
        // a full rebuild (dsp-pipeline.md). Compared on `spec` only, so a
        // pure bypass-flag flip (same specs) falls through to the dedicated
        // check below instead of rebuilding the whole chain for it.
        let o_specs: Vec<&DspSpec> = o.dsp.iter().map(|s| &s.spec).collect();
        let n_specs: Vec<&DspSpec> = n.dsp.iter().map(|s| &s.spec).collect();
        if o_specs != n_specs {
            dsp_chains.push((id, n.dsp.iter().map(|s| s.spec.clone()).collect()));
        } else {
            for (stage, (ob, nb)) in o.dsp.iter().zip(n.dsp.iter()).enumerate() {
                if ob.bypassed != nb.bypassed {
                    params.push(MixerCommand::SetDspBypass {
                        group: id,
                        stage,
                        bypassed: nb.bypassed,
                    });
                }
            }
        }

        if o.duck != n.duck {
            // Trigger name resolved against `new` (same positional `GroupId`
            // convention as everywhere else here); an edit that names a
            // trigger not found in `new` shouldn't reach `diff` at all
            // (`validate_duck_config` at parse time is the real gate) — if
            // it somehow does, drop to no-duck rather than guess.
            let duck = n.duck.as_ref().and_then(|d| {
                new.groups
                    .iter()
                    .position(|gg| gg.name == d.trigger)
                    .map(|ti| DuckSpec {
                        trigger: GroupId(ti as u16),
                        amount_db: d.amount_db,
                        threshold_db: d.threshold_db,
                        attack_ms: d.attack_ms,
                        release_ms: d.release_ms,
                    })
            });
            params.push(MixerCommand::SetDuck { group: id, duck });
        }
    }

    ConfigDelta {
        structural: false,
        params,
        rules: rules_changed.then(|| group_rules(new)),
        dsp_chains: (!dsp_chains.is_empty()).then_some(dsp_chains),
        spatial: (!spatial.is_empty()).then_some(spatial),
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
    use engine::HotkeyKey;

    fn group(name: &str, output: &str, gain: f32, follow_master: bool) -> GroupConfig {
        GroupConfig {
            name: name.into(),
            output_device: output.into(),
            gain: Gain::new(gain).unwrap(),
            follow_master,
            match_rules: vec![],
            dsp: Vec::new(),
            duck: None,
            spatial: false,
            muted: false,
            hotkey_mute: None,
            hotkey_volume_up: None,
            hotkey_volume_down: None,
        }
    }

    #[test]
    fn parses_a_valid_config() {
        let toml = r#"
            schema_version = 2
            master = 0.8

            [[group]]
            name = "Game"
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
                key: HotkeyKey::Char('M'),
            })
        );
    }

    #[test]
    fn volume_bind_and_per_group_hotkeys_round_trip_through_toml() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [app]
            volume_bind = "Game"

            [hotkeys]
            push_to_mute = "Ctrl+Alt+Space"
            master_volume_up = "Ctrl+Alt+Up"
            master_volume_down = "Ctrl+Alt+Down"

            [[group]]
            name = "Game"
            output_device = "Speakers"
            hotkey_mute = "Ctrl+Alt+1"
            hotkey_volume_up = "Ctrl+Shift+Up"
            hotkey_volume_down = "Ctrl+Shift+Down"
        "#;
        let snapshot = parse(toml).unwrap();
        assert_eq!(snapshot.app.volume_bind, Some("Game".to_string()));
        assert_eq!(
            snapshot.app.hotkeys.push_to_mute,
            Some(HotkeyChord { ctrl: true, alt: true, shift: false, key: HotkeyKey::Space })
        );
        assert_eq!(
            snapshot.app.hotkeys.master_volume_up,
            Some(HotkeyChord { ctrl: true, alt: true, shift: false, key: HotkeyKey::Up })
        );
        assert_eq!(
            snapshot.app.hotkeys.master_volume_down,
            Some(HotkeyChord { ctrl: true, alt: true, shift: false, key: HotkeyKey::Down })
        );
        let group = &snapshot.groups[0];
        assert_eq!(
            group.hotkey_mute,
            Some(HotkeyChord { ctrl: true, alt: true, shift: false, key: HotkeyKey::Char('1') })
        );
        assert_eq!(
            group.hotkey_volume_up,
            Some(HotkeyChord { ctrl: true, shift: true, alt: false, key: HotkeyKey::Up })
        );
        assert_eq!(
            group.hotkey_volume_down,
            Some(HotkeyChord { ctrl: true, shift: true, alt: false, key: HotkeyKey::Down })
        );
    }

    #[test]
    fn theme_and_accent_round_trip_through_toml() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [app]
            theme = "light"
            accent = "teal"
        "#;
        let snapshot = parse(toml).unwrap();
        assert_eq!(snapshot.app.theme, engine::ThemeChoice::Light);
        assert_eq!(snapshot.app.accent, engine::AccentChoice::Teal);
    }

    #[test]
    fn an_absent_theme_or_accent_defaults_to_system_and_brand() {
        let toml = r#"
            schema_version = 2
            master = 1.0
        "#;
        let snapshot = parse(toml).unwrap();
        assert_eq!(snapshot.app.theme, engine::ThemeChoice::System);
        assert_eq!(snapshot.app.accent, engine::AccentChoice::Brand);
    }

    #[test]
    fn an_unrecognised_theme_value_is_a_validation_error() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [app]
            theme = "purple"
        "#;
        assert!(matches!(parse(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn an_unrecognised_accent_value_is_a_validation_error() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [app]
            accent = "chartreuse"
        "#;
        assert!(matches!(parse(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn an_absent_hotkey_registers_nothing() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [[group]]
            name = "Game"
            output_device = "Speakers"
        "#;
        let snapshot = parse(toml).unwrap();
        assert_eq!(snapshot.app.volume_bind, None);
        assert_eq!(snapshot.app.hotkeys.push_to_mute, None);
        assert_eq!(snapshot.app.hotkeys.master_volume_up, None);
        assert_eq!(snapshot.app.hotkeys.master_volume_down, None);
        let group = &snapshot.groups[0];
        assert_eq!(group.hotkey_mute, None);
        assert_eq!(group.hotkey_volume_up, None);
        assert_eq!(group.hotkey_volume_down, None);
    }

    #[test]
    fn parses_a_profile_with_a_group_and_active_profile() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [app]
            active_profile = "Gaming"

            [[profile]]
            name = "Gaming"
            hotkey = "Ctrl+Alt+1"
            master = 0.8
            muted = false

              [[profile.group]]
              name = "Game"
              gain = 1.0
              follow_master = true
              output_device = "Headphones"
              spatial = true
        "#;
        let snapshot = parse(toml).unwrap();
        assert_eq!(snapshot.app.active_profile, Some("Gaming".to_string()));
        assert_eq!(snapshot.profiles.len(), 1);
        let p = &snapshot.profiles[0];
        assert_eq!(p.name, "Gaming");
        assert_eq!(p.master, Gain::new(0.8).unwrap());
        assert!(!p.muted);
        assert_eq!(
            p.hotkey,
            Some(HotkeyChord { ctrl: true, alt: true, shift: false, key: HotkeyKey::Char('1') })
        );
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].output_device, "Headphones");
        assert!(p.groups[0].spatial);
    }

    #[test]
    fn a_config_with_no_profiles_behaves_byte_for_byte_as_today() {
        let toml = "schema_version = 2\nmaster = 1.0\n";
        let snapshot = parse(toml).unwrap();
        assert!(snapshot.profiles.is_empty());
        assert_eq!(snapshot.app.active_profile, None);
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
        output: &str,
        gain: f32,
        follow_master: bool,
        rules: &[&str],
    ) -> GroupConfig {
        GroupConfig {
            match_rules: rules.iter().map(|r| r.to_string()).collect(),
            ..group(name, output, gain, follow_master)
        }
    }

    #[test]
    fn diff_reports_unchanged_for_identical_snapshots() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group("Game", "Out", 1.0, true)],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group("Game", "Out", 1.0, true)],
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
            profiles: Vec::new(),
            groups: vec![group("Game", "Out", 1.0, true)],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group("Game", "Out", 0.5, true)],
        };
        let delta = diff(&a, &b);
        assert!(!delta.structural);
        assert!(delta.rules.is_none());
        assert_eq!(delta.params.len(), 1);
        assert!(matches!(delta.params[0], MixerCommand::SetGroupGain(GroupId(0), _)));
    }

    #[test]
    fn a_changed_group_muted_diffs_as_a_param_not_structural() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group("Game", "Out", 1.0, true)],
        };
        let mut b = a.clone();
        b.groups[0].muted = true;

        let delta = diff(&a, &b);
        assert!(!delta.structural);
        assert!(delta.rules.is_none());
        assert_eq!(delta.params.len(), 1);
        assert!(matches!(
            delta.params[0],
            MixerCommand::SetGroupMute(GroupId(0), true)
        ));
    }

    #[test]
    fn diff_reports_structural_for_a_renamed_group() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group("Game", "Out", 1.0, true)],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group("Music", "Out", 1.0, true)],
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
            profiles: Vec::new(),
            groups: vec![group("Game", "Speakers", 1.0, true)],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group("Game", "Headphones", 1.0, true)],
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
            profiles: Vec::new(),
            groups: vec![group_with_rules("Game", "Out", 1.0, true, &[])],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group_with_rules("Game", "Out", 1.0, true, &["game.exe"])],
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
            profiles: Vec::new(),
            groups: vec![group_with_rules("Game", "Out", 1.0, true, &[])],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group_with_rules("Game", "Out", 0.5, true, &["game.exe"])],
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
            profiles: Vec::new(),
            groups: vec![
                group_with_rules("Game", "Out", 1.0, true, &["game.exe"]),
                group_with_rules("Music", "Out", 1.0, true, &["music*.exe"]),
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
    fn parses_eq_and_limiter_dsp_stages() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [[group]]
            name = "Music"
            output_device = "Out"

            [[group.dsp]]
            type = "eq"
            bands = [{ freq_hz = 200.0, gain_db = 3.0, q = 0.7 }]

            [[group.dsp]]
            type = "limiter"
            ceiling_db = -1.0
            bypassed = true
        "#;
        let snapshot = parse(toml).unwrap();
        let dsp = &snapshot.groups[0].dsp;
        assert_eq!(dsp.len(), 2);
        assert!(matches!(dsp[0].spec, DspSpec::Eq { .. }));
        assert!(!dsp[0].bypassed);
        assert!(matches!(dsp[1].spec, DspSpec::Limiter { ceiling_db } if ceiling_db == -1.0));
        assert!(dsp[1].bypassed);
    }

    #[test]
    fn eq_band_with_non_positive_q_is_invalid() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [[group]]
            name = "Music"
            output_device = "Out"

            [[group.dsp]]
            type = "eq"
            bands = [{ freq_hz = 200.0, gain_db = 3.0, q = 0.0 }]
        "#;
        assert!(matches!(parse(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn parses_duck_config() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [[group]]
            name = "Voice"
            output_device = "Out"

            [[group]]
            name = "Music"
            output_device = "Out"

            [group.duck]
            trigger = "Voice"
            amount_db = 12.0
            threshold_db = -40.0
            attack_ms = 5.0
            release_ms = 200.0
        "#;
        let snapshot = parse(toml).unwrap();
        let duck = snapshot.groups[1].duck.as_ref().expect("Music should have duck config");
        assert_eq!(duck.trigger, "Voice");
        assert_eq!(duck.amount_db, 12.0);
    }

    #[test]
    fn unknown_duck_trigger_name_is_invalid() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [[group]]
            name = "Music"
            output_device = "Out"

            [group.duck]
            trigger = "Nonexistent"
            amount_db = 12.0
            threshold_db = -40.0
            attack_ms = 5.0
            release_ms = 200.0
        "#;
        assert!(matches!(parse(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn a_duck_cycle_is_invalid() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [[group]]
            name = "A"
            output_device = "Out"
            [group.duck]
            trigger = "B"
            amount_db = 6.0
            threshold_db = -40.0
            attack_ms = 5.0
            release_ms = 200.0

            [[group]]
            name = "B"
            output_device = "Out"
            [group.duck]
            trigger = "A"
            amount_db = 6.0
            threshold_db = -40.0
            attack_ms = 5.0
            release_ms = 200.0
        "#;
        assert!(matches!(parse(toml), Err(ConfigError::Invalid(_))));
    }

    fn group_with_dsp(name: &str, output: &str, dsp: Vec<DspStageConfig>) -> GroupConfig {
        GroupConfig {
            dsp,
            ..group(name, output, 1.0, true)
        }
    }

    #[test]
    fn diff_reports_dsp_chains_for_a_stage_shape_change() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group_with_dsp("Game", "Out", vec![])],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group_with_dsp(
                "Game",
                "Out",
                vec![DspStageConfig {
                    spec: DspSpec::Limiter { ceiling_db: -1.0 },
                    bypassed: false,
                }],
            )],
        };
        let delta = diff(&a, &b);
        assert!(!delta.structural);
        let chains = delta.dsp_chains.expect("expected a dsp_chains delta");
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].0, GroupId(0));
        assert_eq!(chains[0].1, vec![DspSpec::Limiter { ceiling_db: -1.0 }]);
    }

    #[test]
    fn diff_reports_a_fast_path_bypass_command_not_a_chain_rebuild_for_a_bypass_only_change() {
        let stage = |bypassed: bool| DspStageConfig {
            spec: DspSpec::Limiter { ceiling_db: -1.0 },
            bypassed,
        };
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group_with_dsp("Game", "Out", vec![stage(false)])],
        };
        let b = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group_with_dsp("Game", "Out", vec![stage(true)])],
        };
        let delta = diff(&a, &b);
        assert!(delta.dsp_chains.is_none(), "same spec shape must not trigger a chain rebuild");
        assert_eq!(delta.params.len(), 1);
        assert!(matches!(
            delta.params[0],
            MixerCommand::SetDspBypass { group: GroupId(0), stage: 0, bypassed: true }
        ));
    }

    #[test]
    fn diff_reports_a_set_duck_command_with_the_triggers_resolved_group_id() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![
                group("Voice", "Out", 1.0, true),
                group("Music", "Out", 1.0, true),
            ],
        };
        let mut b = a.clone();
        b.groups[1].duck = Some(DuckSpecConfig {
            trigger: "Voice".into(),
            amount_db: 12.0,
            threshold_db: -40.0,
            attack_ms: 5.0,
            release_ms: 200.0,
        });
        let delta = diff(&a, &b);
        assert!(!delta.structural);
        assert!(delta.dsp_chains.is_none());
        assert_eq!(delta.params.len(), 1);
        assert!(matches!(
            &delta.params[0],
            MixerCommand::SetDuck { group: GroupId(1), duck: Some(d) } if d.trigger == GroupId(0)
        ));
    }

    #[test]
    fn diff_reports_a_spatial_entry_for_a_spatial_only_change_not_structural() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group("Game", "Out", 1.0, true)],
        };
        let mut b = a.clone();
        b.groups[0].spatial = true;

        let delta = diff(&a, &b);
        assert!(!delta.structural);
        assert!(delta.dsp_chains.is_none());
        assert!(delta.params.is_empty(), "spatial has no direct MixerCommand equivalent");
        let spatial = delta.spatial.expect("expected a spatial delta");
        assert_eq!(spatial, vec![(GroupId(0), true)]);
    }

    #[test]
    fn diff_reports_unchanged_when_spatial_flag_is_the_same() {
        let a = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![group("Game", "Out", 1.0, true)],
        };
        let b = a.clone();
        assert!(diff(&a, &b).is_unchanged());
    }

    #[test]
    fn parses_the_spatial_flag_defaulting_to_false() {
        let toml = r#"
            schema_version = 2
            master = 1.0

            [[group]]
            name = "Game"
            output_device = "Out"

            [[group]]
            name = "Music"
            output_device = "Out"
            spatial = true
        "#;
        let snapshot = parse(toml).unwrap();
        assert!(!snapshot.groups[0].spatial);
        assert!(snapshot.groups[1].spatial);
    }

    #[test]
    fn ensure_config_writes_the_default_template_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splitstream.toml");

        let snapshot = ensure_config(&path).unwrap();

        assert!(path.exists());
        assert!(snapshot.groups.is_empty());
        assert!(snapshot.app.autostart);
    }

    #[test]
    fn ensure_config_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("splitstream.toml");

        ensure_config(&path).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn ensure_config_leaves_an_existing_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splitstream.toml");
        fs::write(&path, "schema_version = 2\nmaster = 0.42\n").unwrap();

        let snapshot = ensure_config(&path).unwrap();

        assert_eq!(snapshot.master, Gain::new(0.42).unwrap());
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
