//! Config → resolved graph. Owns the pre-resolution config shape
//! (`ConfigSnapshot`/`GroupConfig`) since `engine` is the consumer that
//! resolves group/device names into endpoint ids — see the "Config type
//! home" decision in `.lattice/context/engine-core.md`: `control` (a later
//! layer) depends on `engine` for this type, not the other way around, so
//! `engine` can be built and tested before `control` exists.

use std::collections::HashSet;

use audio_core::{DspSpec, DuckSpec, Format, Gain, GroupId, GroupSpec, OutputId, OutputSpec, Topology};

use crate::ports::{Endpoint, EndpointId};
use crate::runtime::EngineError;

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigSnapshot {
    pub schema_version: u32,
    pub master: Gain,
    /// Effective master = `muted ? 0 : master` — kept as its own flag (not
    /// folded into `master`) so the pre-mute gain value survives a mute
    /// round-trip; see app-shell.md's mute schema decision.
    pub muted: bool,
    pub groups: Vec<GroupConfig>,
    pub app: AppConfig,
    /// Named snapshots of per-group state (profiles.md). Purely additive —
    /// a config with no `[[profile]]` tables behaves byte-for-byte as today.
    pub profiles: Vec<ProfileConfig>,
}

/// A named snapshot of per-group state (profiles.md) — gain, mute,
/// follow_master, output_device, dsp, duck, spatial, keyed by group name.
/// Value object: compared and copied wholesale, never mutated in place.
/// Does **not** capture the group *set* — switching can never create or
/// delete a group (L1 capability 2) — nor `match_rules` (decision 8).
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileConfig {
    pub name: String,
    /// Optional global hotkey applying this profile (L1 capability 10).
    pub hotkey: Option<HotkeyChord>,
    pub master: Gain,
    pub muted: bool,
    /// Per-group values keyed by name. A group absent here is left untouched
    /// on apply; an entry naming a missing group is skipped (L1 capability 11).
    pub groups: Vec<ProfileGroupConfig>,
}

/// One group's captured state within a [`ProfileConfig`]. `match_rules` is
/// deliberately absent (decision 8) — which apps belong to which group stays
/// shared across profiles.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileGroupConfig {
    pub name: String,
    pub gain: Gain,
    pub follow_master: bool,
    pub output_device: String,
    pub dsp: Vec<DspStageConfig>,
    pub duck: Option<DuckSpecConfig>,
    pub spatial: bool,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupConfig {
    pub name: String,
    pub output_device: String,
    pub gain: Gain,
    pub follow_master: bool,
    /// Raw config strings — parsed into `rules::MatchRule` by `control::group_rules`,
    /// not here (this type only mirrors the TOML shape; see session-routing.md).
    pub match_rules: Vec<String>,
    pub dsp: Vec<DspStageConfig>,
    pub duck: Option<DuckSpecConfig>,
    /// Per-group virtual-surround/stereo-widen toggle (spatial-audio.md) —
    /// mirrors `audio_core::GroupSpec.spatial`.
    pub spatial: bool,
    /// Persisted per-group mute (per-group-mute-solo.md) — mirrors
    /// `audio_core::GroupSpec.mute`. No `solo` counterpart: solo is
    /// session-only, never sourced from config.
    pub muted: bool,
    /// Per-group hotkeys (external-controls.md decision 8) — live on the
    /// group's own table, not `[hotkeys]`, so deleting the group deletes its
    /// bindings. Config-file-only (no editing UI), same as every other
    /// hotkey chord.
    pub hotkey_mute: Option<HotkeyChord>,
    pub hotkey_volume_up: Option<HotkeyChord>,
    pub hotkey_volume_down: Option<HotkeyChord>,
}

/// Pairs a stage's construction spec with its persisted bypass state.
/// `bypassed` has no place on `audio_core::DspSpec` itself — that type is
/// pure construction input (notes: `DspChain::new` always starts a stage
/// un-bypassed); `bypassed` is applied afterward via a `SetDspBypass`
/// command, same fast-path a live UI toggle uses (see `queue_initial_dsp_bypass`).
#[derive(Debug, Clone, PartialEq)]
pub struct DspStageConfig {
    pub spec: DspSpec,
    pub bypassed: bool,
}

/// Config-side mirror of `audio_core::DuckSpec` with an unresolved trigger
/// **name** instead of a `GroupId` — same shape as `bus_endpoint`/
/// `output_device`, resolved against `snapshot.groups` in [`resolve`].
#[derive(Debug, Clone, PartialEq)]
pub struct DuckSpecConfig {
    pub trigger: String,
    pub amount_db: f32,
    pub threshold_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

/// `AppConfig`/`HotkeyMap`/`HotkeyChord` live here rather than `control` (where
/// app-shell.md's L4 contract text placed `HotkeyMap`) — same interface-at-consumer
/// idiom already applied to `GroupRules`/`MatchRule` (see session-routing.md's
/// 2026-07-20 decision): `ConfigSnapshot` is `engine`'s type, so a field on it
/// must be resolvable without `engine` depending back on `control`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppConfig {
    pub autostart: bool,
    pub hotkeys: HotkeyMap,
    /// Name of the currently-active profile (profiles.md decision 5).
    /// Restart returns to the same profile; `None` means no profile is
    /// active (also the state after the active one is deleted, L3 flow H).
    pub active_profile: Option<String>,
    /// Which target is bound to the Windows default playback device's
    /// volume (external-controls.md capability 1) — `"master"` or a group
    /// name; `None` (the default) means unbound, behaving exactly as today.
    pub volume_bind: Option<String>,
    /// Theme mode (visual-identity.md capability 2).
    pub theme: ThemeChoice,
    /// Brand accent preset (visual-identity.md capability 5).
    pub accent: AccentChoice,
    /// Process file names no group may claim (routing-truthfulness.md
    /// capability 1) — checked ahead of every match rule, including a `*`
    /// catch-all. Empty or absent = today's behaviour exactly; the TOML key
    /// is written only on first use.
    pub excluded: Vec<String>,
    /// Friendly name of the endpoint routed apps are parked on
    /// (double-audio-prevention capability 1) — VB-CABLE in v1. Every app
    /// renders here, nobody listens here, and only Splitstream's processed
    /// copy reaches a real device. `None` = not configured yet.
    pub sink_device: Option<String>,
    /// The user opted in to Splitstream owning the Windows default output
    /// (L3 flow B). While true, every start re-asserts the sink.
    pub manage_default: bool,
    /// The default endpoint in effect before Splitstream took it. Written
    /// only when empty (flow B rule 2) and cleared on a clean restore (flow
    /// C), so a value still present at startup means the previous exit was
    /// unclean and the true pre-Splitstream device is still recoverable
    /// (flow D).
    pub previous_default: Option<String>,
}

/// Theme mode (visual-identity.md decision 1). `System` follows the OS
/// light/dark preference live, not just at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    Dark,
    Light,
    #[default]
    System,
}

/// Brand accent preset (visual-identity.md decision 2) — a config type, not
/// an `app` type, per decision 10: `ConfigEdit` lives in `control`, which
/// must never depend on `app`, so the persisted choice has to live where
/// `HotkeyChord` does. `app::theme::accent` maps a choice to its actual
/// dark/light `Color32` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccentChoice {
    #[default]
    Brand,
    Teal,
    Amber,
    Violet,
    Slate,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HotkeyMap {
    pub mute_master: Option<HotkeyChord>,
    /// Master push-to-mute (external-controls.md capability 13) — held
    /// while pressed, restores the prior state on release or max-hold expiry.
    pub push_to_mute: Option<HotkeyChord>,
    pub master_volume_up: Option<HotkeyChord>,
    pub master_volume_down: Option<HotkeyChord>,
}

/// A validated global hotkey chord, e.g. spec §11.3's `"Ctrl+Alt+M"`. At least
/// one modifier is required — a bare-key global hotkey would capture every
/// keypress system-wide, which no OS hotkey API allows unconditionally anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: HotkeyKey,
}

/// The non-modifier key half of a [`HotkeyChord`]. A plain `char` (the
/// original shape) can't represent the named keys external-controls.md's own
/// example config needs (`push_to_mute = "...+Space"`, `master_volume_up =
/// "...+Up"`) — widened to this small closed set rather than accepting
/// arbitrary key-name strings, which would push validation (and platform
/// key-code mapping) failures out to `hotkeys.rs` instead of catching an
/// unsupported name at config-parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyKey {
    Char(char),
    Space,
    Up,
    Down,
}

impl HotkeyChord {
    pub fn parse(s: &str) -> Result<HotkeyChord, String> {
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut key = None;

        for token in s.split('+').map(str::trim) {
            match token.to_ascii_lowercase().as_str() {
                "" => return Err(format!("empty token in hotkey chord {s:?}")),
                "ctrl" | "control" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                "space" => set_key(&mut key, HotkeyKey::Space, token, s)?,
                "up" => set_key(&mut key, HotkeyKey::Up, token, s)?,
                "down" => set_key(&mut key, HotkeyKey::Down, token, s)?,
                _ => {
                    let mut chars = token.chars();
                    let first = chars.next().filter(|c| c.is_ascii_alphanumeric());
                    if first.is_none() || chars.next().is_some() {
                        return Err(format!("invalid key token {token:?} in hotkey chord {s:?}"));
                    }
                    let c = first.map(|c| c.to_ascii_uppercase()).expect("checked Some above");
                    set_key(&mut key, HotkeyKey::Char(c), token, s)?;
                }
            }
        }

        let key = key.ok_or_else(|| format!("hotkey chord {s:?} has no key"))?;
        if !(ctrl || alt || shift) {
            return Err(format!("hotkey chord {s:?} needs at least one modifier"));
        }
        Ok(HotkeyChord { ctrl, alt, shift, key })
    }
}

fn set_key(key: &mut Option<HotkeyKey>, value: HotkeyKey, token: &str, whole: &str) -> Result<(), String> {
    if key.is_some() {
        return Err(format!("invalid key token {token:?} in hotkey chord {whole:?}"));
    }
    *key = Some(value);
    Ok(())
}

impl std::fmt::Display for HotkeyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HotkeyKey::Char(c) => write!(f, "{c}"),
            HotkeyKey::Space => write!(f, "Space"),
            HotkeyKey::Up => write!(f, "Up"),
            HotkeyKey::Down => write!(f, "Down"),
        }
    }
}

impl std::fmt::Display for HotkeyChord {
    /// Inverse of [`HotkeyChord::parse`] — `"Ctrl+Alt+M"` — so a chord can
    /// round-trip through config writes (profiles.md) and UI display alike.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ctrl {
            write!(f, "Ctrl+")?;
        }
        if self.alt {
            write!(f, "Alt+")?;
        }
        if self.shift {
            write!(f, "Shift+")?;
        }
        write!(f, "{}", self.key)
    }
}

/// Config with names resolved to endpoint ids, ready for `runtime` to open
/// ports and build the `Mixer` from. No more `group_endpoints`
/// (process-loopback-capture pivot): a group no longer has any capture
/// endpoint to resolve — its capture *sources* are pids, matched live by
/// `engine::routing` and wired in dynamically via
/// `EngineHandle::capture_control`, never resolved here.
pub struct GraphPlan {
    pub topology: Topology,
    pub output_endpoints: Vec<(OutputId, EndpointId)>,
    /// Friendly device name per `OutputId`, captured as each output is first
    /// assigned (level-meters.md) — the exact name the settings UI shows in its
    /// device pickers. Built here, by the same code that assigns `OutputId`s
    /// across *non-parked* groups, so a parked group never shifts the mapping
    /// the way reproducing it UI-side from `snapshot.groups` would.
    pub output_devices: Vec<(OutputId, String)>,
}

/// Resolves group/device names against the live endpoint list. Multiple
/// groups naming the same `output_device` share one `OutputId` (spec:
/// "shared outputs" — their audio sums cleanly at that one physical device).
///
/// `capture_format` is every group's `input_format` (process-loopback-capture
/// L4) — there is no per-group bus to derive a per-group format from
/// anymore, so the caller passes one fixed format for every group. Confirmed
/// on real hardware: a process-loopback-activated `IAudioClient` doesn't
/// implement `GetMixFormat` at all, so the real `win-audio` implementation
/// *dictates* a fixed format to every process capture stream at `Initialize`
/// time rather than negotiating one — every stream reports the same value
/// regardless of the system's actual default device (see
/// `engine::runtime::PROCESS_CAPTURE_FORMAT`'s doc for the full story).
///
/// `parked` names groups (drift-and-recovery: no fallback device available)
/// to exclude from the resulting graph entirely rather than erroring — the
/// group's `GroupId` stays reserved at its original index (`GroupId(i)`
/// comes from `snapshot.groups`' position, not a running counter), so
/// later groups keep stable ids across a rebuild that parks an earlier one.
pub fn resolve(
    snapshot: &ConfigSnapshot,
    endpoints: &[Endpoint],
    parked: &HashSet<String>,
    capture_format: Format,
) -> Result<GraphPlan, EngineError> {
    let mut groups = Vec::with_capacity(snapshot.groups.len());
    let mut outputs: Vec<OutputSpec> = Vec::new();
    let mut output_endpoints: Vec<(OutputId, EndpointId)> = Vec::new();
    let mut output_devices: Vec<(OutputId, String)> = Vec::new();
    let mut output_by_device: Vec<(&str, OutputId)> = Vec::new();

    for (i, g) in snapshot.groups.iter().enumerate() {
        let group_id = GroupId(i as u16);
        if parked.contains(&g.name) {
            continue;
        }

        let output_id = match output_by_device
            .iter()
            .find(|(name, _)| *name == g.output_device)
        {
            Some((_, id)) => *id,
            None => {
                let physical = endpoints
                    .iter()
                    .find(|e| e.name == g.output_device)
                    .ok_or_else(|| {
                        EngineError::Resolve(format!(
                            "group '{}': output device '{}' not found",
                            g.name, g.output_device
                        ))
                    })?;
                let id = OutputId(outputs.len() as u16);
                outputs.push(OutputSpec {
                    id,
                    format: physical.format,
                });
                output_endpoints.push((id, physical.id.clone()));
                output_devices.push((id, g.output_device.clone()));
                output_by_device.push((g.output_device.as_str(), id));
                id
            }
        };

        let duck = match &g.duck {
            Some(d) => {
                let trigger = snapshot
                    .groups
                    .iter()
                    .position(|gg| gg.name == d.trigger)
                    .map(|i| GroupId(i as u16))
                    .ok_or_else(|| {
                        EngineError::Resolve(format!(
                            "group '{}': duck trigger '{}' not found",
                            g.name, d.trigger
                        ))
                    })?;
                Some(DuckSpec {
                    trigger,
                    amount_db: d.amount_db,
                    threshold_db: d.threshold_db,
                    attack_ms: d.attack_ms,
                    release_ms: d.release_ms,
                })
            }
            None => None,
        };

        groups.push(GroupSpec {
            id: group_id,
            gain: g.gain,
            follow_master: g.follow_master,
            output: output_id,
            input_format: capture_format,
            dsp: g.dsp.iter().map(|s| s.spec.clone()).collect(),
            duck,
            spatial: g.spatial,
            mute: g.muted,
        });
    }

    Ok(GraphPlan {
        topology: Topology {
            master: snapshot.master,
            groups,
            outputs,
        },
        output_endpoints,
        output_devices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core::ChannelLayout;

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
            layout: ChannelLayout::STEREO,
        }
    }

    fn endpoints() -> Vec<Endpoint> {
        vec![
            Endpoint {
                id: EndpointId("out-1".into()),
                name: "Speakers".into(),
                format: stereo(48_000),
            },
            Endpoint {
                id: EndpointId("out-2".into()),
                name: "Headphones".into(),
                format: stereo(48_000),
            },
        ]
    }

    fn no_parked() -> HashSet<String> {
        HashSet::new()
    }

    fn resolve_test(snapshot: &ConfigSnapshot, endpoints: &[Endpoint], parked: &HashSet<String>) -> Result<GraphPlan, EngineError> {
        resolve(snapshot, endpoints, parked, stereo(48_000))
    }

    fn group(name: &str, output: &str) -> GroupConfig {
        GroupConfig {
            name: name.into(),
            output_device: output.into(),
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
        }
    }

    #[test]
    fn hotkey_chord_parses_modifiers_and_key_case_insensitively() {
        let chord = HotkeyChord::parse("ctrl+alt+m").unwrap();
        assert_eq!(
            chord,
            HotkeyChord {
                ctrl: true,
                alt: true,
                shift: false,
                key: HotkeyKey::Char('M'),
            }
        );
    }

    #[test]
    fn hotkey_chord_rejects_a_bare_key_with_no_modifier() {
        assert!(HotkeyChord::parse("M").is_err());
    }

    #[test]
    fn hotkey_chord_rejects_a_multi_character_key_token() {
        assert!(HotkeyChord::parse("Ctrl+Home").is_err());
    }

    #[test]
    fn hotkey_chord_rejects_two_key_tokens() {
        assert!(HotkeyChord::parse("Ctrl+M+N").is_err());
    }

    #[test]
    fn hotkey_chord_display_round_trips_through_parse() {
        let chord = HotkeyChord::parse("ctrl+alt+1").unwrap();
        assert_eq!(chord.to_string(), "Ctrl+Alt+1");
        assert_eq!(HotkeyChord::parse(&chord.to_string()).unwrap(), chord);
    }

    #[test]
    fn hotkey_chord_parses_named_keys_case_insensitively() {
        assert_eq!(HotkeyChord::parse("Ctrl+Alt+Space").unwrap().key, HotkeyKey::Space);
        assert_eq!(HotkeyChord::parse("ctrl+up").unwrap().key, HotkeyKey::Up);
        assert_eq!(HotkeyChord::parse("Ctrl+DOWN").unwrap().key, HotkeyKey::Down);
    }

    #[test]
    fn hotkey_chord_with_a_named_key_round_trips_through_display() {
        for chord_str in ["Ctrl+Alt+Space", "Ctrl+Up", "Ctrl+Down"] {
            let chord = HotkeyChord::parse(chord_str).unwrap();
            assert_eq!(HotkeyChord::parse(&chord.to_string()).unwrap(), chord);
        }
    }

    #[test]
    fn resolves_group_to_its_output_id() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            muted: false,
            app: AppConfig::default(),
            profiles: Vec::new(),
            master: Gain::UNITY,
            groups: vec![group("Game", "Speakers")],
        };
        let plan = resolve_test(&snapshot, &endpoints(), &no_parked()).unwrap();
        assert_eq!(plan.topology.groups.len(), 1);
        assert_eq!(plan.topology.outputs.len(), 1);
        assert_eq!(plan.output_endpoints[0].1, EndpointId("out-1".into()));
        // Friendly device name captured per OutputId (level-meters.md).
        assert_eq!(plan.output_devices, vec![(OutputId(0), "Speakers".to_string())]);
    }

    #[test]
    fn a_parked_earlier_group_does_not_shift_later_output_device_names() {
        // Two groups on distinct devices; the first is parked. The engine must
        // assign OutputId(0) to the *second* group's device — the exact case
        // where reproducing the mapping from all groups UI-side would mislabel.
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            muted: false,
            app: AppConfig::default(),
            profiles: Vec::new(),
            master: Gain::UNITY,
            groups: vec![group("Music", "Speakers"), group("Game", "Headphones")],
        };
        let mut parked = HashSet::new();
        parked.insert("Music".to_string());
        let plan = resolve_test(&snapshot, &endpoints(), &parked).unwrap();
        assert_eq!(plan.output_devices, vec![(OutputId(0), "Headphones".to_string())]);
    }

    #[test]
    fn resolved_groups_share_the_passed_in_capture_format() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            muted: false,
            app: AppConfig::default(),
            profiles: Vec::new(),
            master: Gain::UNITY,
            groups: vec![group("Game", "Speakers")],
        };
        let plan = resolve(&snapshot, &endpoints(), &no_parked(), stereo(44_100)).unwrap();
        assert_eq!(plan.topology.groups[0].input_format, stereo(44_100));
    }

    #[test]
    fn two_groups_sharing_a_device_share_one_output_id() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            muted: false,
            app: AppConfig::default(),
            profiles: Vec::new(),
            master: Gain::UNITY,
            groups: vec![
                group("Game", "Speakers"),
                group("Music", "Speakers"),
            ],
        };
        let plan = resolve_test(&snapshot, &endpoints(), &no_parked()).unwrap();
        assert_eq!(
            plan.topology.outputs.len(),
            1,
            "one physical device, one OutputSpec"
        );
        assert_eq!(
            plan.topology.groups[0].output,
            plan.topology.groups[1].output
        );
    }

    #[test]
    fn missing_output_device_is_a_resolve_error() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            muted: false,
            app: AppConfig::default(),
            profiles: Vec::new(),
            master: Gain::UNITY,
            groups: vec![group("Game", "Nonexistent")],
        };
        assert!(matches!(
            resolve_test(&snapshot, &endpoints(), &no_parked()),
            Err(EngineError::Resolve(_))
        ));
    }

    #[test]
    fn parked_group_is_excluded_without_erroring() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            muted: false,
            app: AppConfig::default(),
            profiles: Vec::new(),
            master: Gain::UNITY,
            groups: vec![group("Game", "Nonexistent")],
        };
        let mut parked = HashSet::new();
        parked.insert("Game".to_string());
        let plan = resolve_test(&snapshot, &endpoints(), &parked).unwrap();
        assert!(plan.topology.groups.is_empty());
        assert!(plan.topology.outputs.is_empty());
    }

    #[test]
    fn parking_an_earlier_group_keeps_later_groups_ids_stable() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            muted: false,
            app: AppConfig::default(),
            profiles: Vec::new(),
            master: Gain::UNITY,
            groups: vec![
                group("Parked", "Speakers"),
                group("Music", "Headphones"),
            ],
        };
        let mut parked = HashSet::new();
        parked.insert("Parked".to_string());
        let plan = resolve_test(&snapshot, &endpoints(), &parked).unwrap();
        assert_eq!(plan.topology.groups.len(), 1);
        // "Music" is snapshot.groups[1] — its GroupId must stay GroupId(1)
        // even though "Parked" (index 0) was skipped, not renumbered to 0.
        assert_eq!(plan.topology.groups[0].id, GroupId(1));
    }

    #[test]
    fn resolves_duck_trigger_name_to_the_triggers_positional_group_id() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            muted: false,
            app: AppConfig::default(),
            profiles: Vec::new(),
            master: Gain::UNITY,
            groups: vec![
                group("Voice", "Speakers"),
                GroupConfig {
                    duck: Some(DuckSpecConfig {
                        trigger: "Voice".into(),
                        amount_db: 12.0,
                        threshold_db: -40.0,
                        attack_ms: 5.0,
                        release_ms: 200.0,
                    }),
                    ..group("Music", "Speakers")
                },
            ],
        };
        let plan = resolve_test(&snapshot, &endpoints(), &no_parked()).unwrap();
        let music = &plan.topology.groups[1];
        assert_eq!(music.duck.unwrap().trigger, GroupId(0), "Voice is snapshot.groups[0]");
    }

    #[test]
    fn unknown_duck_trigger_name_is_a_resolve_error() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            muted: false,
            app: AppConfig::default(),
            profiles: Vec::new(),
            master: Gain::UNITY,
            groups: vec![GroupConfig {
                duck: Some(DuckSpecConfig {
                    trigger: "Nonexistent".into(),
                    amount_db: 12.0,
                    threshold_db: -40.0,
                    attack_ms: 5.0,
                    release_ms: 200.0,
                }),
                ..group("Music", "Speakers")
            }],
        };
        assert!(matches!(
            resolve_test(&snapshot, &endpoints(), &no_parked()),
            Err(EngineError::Resolve(_))
        ));
    }
}
