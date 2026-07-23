---
feature: profiles
requirement_doc: null
created: 2026-07-22
status: approved
note: >
  Roadmap Priority 5 (.lattice/ux-gap-roadmap.md). Named profiles capturing
  per-group state, switchable from the tray, the settings window, and an
  optional per-profile global hotkey. No requirement spec — roadmap is the
  origin. Depends on nothing unbuilt, but interacts with the pending
  per-group-mute-solo blueprint (see Grounding).
---

# Profiles

> Named snapshots of per-group state — gain, routing, DSP, mute — that a user
> switches between in one click. `[[group]]` stays exactly what it is today:
> the live state. `[[profile]]` tables hold saved copies, and switching writes
> a profile's values back into the live tables through the config edits that
> already exist.

## Grounding (2026-07-22, pre-Level-1)

- **One config file, one store, one watcher.** `RawConfig` (config.rs:33) is
  `schema_version`, `master`, `muted`, `[[group]]`, `[app]`, `[hotkeys]`, every
  field `#[serde(default)]`. `ConfigStore` does format-preserving `toml_edit`
  writes with echo suppression on the watcher. **Adding an optional
  `[[profile]]` array is purely additive** — every existing config still
  parses, so `SUPPORTED_SCHEMA_VERSION` stays at 2.
- **Per-group state is already a well-defined set.** `GroupConfig` (graph.rs:28)
  = name, output_device, gain, follow_master, match_rules, dsp, duck, spatial.
  A profile entry is a subset of exactly these fields, keyed by `name`.
- **The structural/param split already decides switch cost.** Changing
  `gain`/`follow_master` is a param fast-path edit; changing `output_device` or
  the DSP chain is structural and rebuilds the engine graph. A profile that
  carries routing therefore rebuilds on switch — that is inherent, not a design
  choice made here.
- **Hotkeys are currently singular.** `HotkeyMap { mute_master: Option<HotkeyChord> }`
  (graph.rs:78) and `spawn_hotkeys` registers exactly one `HotKey`
  (hotkeys.rs:47). Per-profile hotkeys require generalizing that to a list —
  the same generalization roadmap P10 needs.
- **The tray already carries actions.** `spawn_tray` builds a menu sending
  `ShellAction::{ToggleMute, ShowSettings, Quit}` (tray.rs:130-134); a profile
  submenu is another set of items on the same channel.
- **Interaction with the pending per-group-mute-solo blueprint.** That design
  adds a persisted `muted` field to `GroupConfig` and a session-only solo. A
  profile should capture the persisted mute and must *not* capture solo — solo
  is explicitly transient there. Recorded so the two designs stay consistent
  if that one lands first.

## Design: Level 1 -- Capabilities

**Approved 2026-07-23.**

1. **Named profiles** stored as `[[profile]]` tables in the existing config
   file. A config with no profiles behaves exactly as today.
2. **A profile captures per-group state by name** — gain, mute, follow_master,
   output_device, dsp, duck, spatial — plus top-level master and muted. It does
   **not** capture the set of groups, so switching can never create or delete
   one.
3. **`[[group]]` remains the live state.** Profiles are saved copies; switching
   writes a profile's values back into the live tables through the config edits
   that already exist. Nothing about how the engine reads config changes.
4. **Switching applies in one action** — a single batch of edits, one rebuild,
   from the settings window or the tray.
5. **Explicit save.** Live edits after a switch are transient; the profile
   changes only when the user saves to it.
6. **A modified indicator** — the active profile shows as modified whenever
   live state differs from its stored values, so a transient tweak is never
   silently lost without warning.
7. **Revert to the active profile** — one action discarding live edits back to
   the stored values.
8. **Create, rename, delete profiles**, including "save current state as a new
   profile".
9. **Tray switching** — a profile submenu alongside the existing
   mute/settings/quit items.
10. **Optional per-profile global hotkey** — `hotkey = "Ctrl+Alt+1"` on a
    profile table. Requires generalizing `spawn_hotkeys` from one registered
    chord to a list, the same generalization roadmap P10 needs; the one place
    this feature reaches beyond its own surface.
11. **Defined tolerance for mismatches** — a profile entry naming a group that
    no longer exists is ignored on apply; a group with no entry in the profile
    is left untouched rather than reset.
12. **Backward and forward compatible** — no `schema_version` bump; the field
    is additive and defaults to empty.

Out of scope (v1): profile import/export as separate files; auto-switching on
app launch or foreground-window detection; per-profile device *sets*; profile
ordering, folders or nesting; capturing session→group assignments beyond the
`match_rules` already in `GroupConfig`; capturing the session-only solo state
the per-group-mute-solo blueprint defines as transient.

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-23 | **A profile captures full per-group state including routing** (gain, mute, follow_master, output_device, dsp, duck, spatial), keyed by group name, plus master/muted. The group *set* is shared and never touched. | Supports the actual headline case — a Streaming profile sending Game to a virtual cable while Gaming sends it to the headset — which mix-only profiles cannot express. Keying by name means switching can never create or delete a group, removing the worst failure mode. Accepted cost: `output_device` and DSP changes are structural, so a switch rebuilds the engine graph rather than riding the param fast path. Rejected: mix state only (instant glitch-free switching with no rebuild at all, but a "Streaming" profile could not reroute or change EQ — most of why people switch); whole-config profiles including the group set (most powerful, closest to Voicemeeter macros, but a switch can delete groups under the user and a half-written profile leaves routing hard to undo). |
| 2 | 2026-07-23 | **Explicit save: live edits are transient until saved to the profile.** | Gives a real "revert to profile" and makes profiles trustworthy reference points to audition against. Requires the modified indicator (capability 6) so a tweak is never silently discarded. Rejected: auto-save on every edit (nothing is ever lost, no dirty state to explain — but no way to audition and back out, and a profile silently becomes whatever you last did to it). |
| 3 | 2026-07-23 | **Profiles live as `[[profile]]` tables inside config.toml.** | One file, one watcher, one `ConfigStore`, one format-preserving writer — all of which already exist. Purely additive, so old configs parse unchanged and `SUPPORTED_SCHEMA_VERSION` stays at 2. Rejected: separate files under `profiles/` (easy to share and back up, keeps config.toml small — but a second watcher and store path, plus new failure modes such as the active profile's file being deleted or corrupted). |
| 4 | 2026-07-23 | **`[[group]]` stays the live state; profiles are saved copies written back into it.** | Existing config keeps working untouched, the engine's read path is unchanged, and "no profiles configured" is byte-for-byte today's behaviour. Rejected: making `[[group]]` structural-only with all values living in profiles (conceptually tidier, but rewrites the config schema, the engine's read path, and every existing user's file). |
| 5 | 2026-07-23 | **The active profile name persists in `[app] active_profile`; the modified flag is computed, never stored.** | A restart returns to the same profile, and a computed flag cannot go stale or disagree with the file. |
| 6 | 2026-07-23 | **Applying a profile is an ordinary `ConfigEdit` batch through the existing structural path**, not a bulk mechanism of its own. | The watcher, echo suppression and rebuild logic then behave exactly as they already do, with no second write path to keep consistent. |
| 7 | 2026-07-23 | **Per-profile hotkeys are in scope, accepting that `spawn_hotkeys` must generalize from one chord to N.** | The roadmap item names "tray + hotkey" as the value; shipping only the tray half would quietly narrow the ask. The generalization is the same one P10 needs, so it is work brought forward rather than duplicated. |
| 8 | 2026-07-23 | **`match_rules` are excluded from profiles.** | Which apps belong to which group stays shared; profiles change what happens *to* those groups. Match rules are also drag-managed live state — every chip drag rewrites them — so a profile switch silently reverting a drag the user just made would be surprising and hard to attribute, and the modified indicator would light up from an action that does not look like editing a profile. Accepted cost: a profile cannot move Spotify from Music to Game, only change where Music itself goes. Rejected: including them (a profile would fully own routing, expressing "while streaming, put the browser in its own group"). Level 1's capture list already implied this; recorded explicitly rather than left as an omission. |
| 9 | 2026-07-23 | **New `ConfigEdit::SetDspChain(group, Vec<DspStageConfig>)`, and no `RenameProfile` edit.** | Applying a profile's DSP has to replace a whole chain, which no existing edit can do; emitting add/remove sequences to reshape it is fragile and index-sensitive. Rename likewise decomposes into `SetProfile` + `RemoveProfile` + `SetActiveProfile`. Both are the same collapse principle: prefer one whole-collection replace over a matched add/remove pair when the writer rewrites the collection anyway. |
| 10 | 2026-07-23 | **A profile batch is routed structural-or-param by its *content*, via a new `control::is_structural(&ConfigEdit)` predicate — not always structural.** | A Gaming→Music switch that only moves faders would otherwise rebuild the engine and reopen devices, costing an audible gap for nothing. Today each call site hardcodes the choice (`ui.rs` sends `EditStructure` for `SetGroupOutput`); a profile batch is mixed, so the knowledge of which edits are structural has to become a predicate the dispatcher can apply to a whole batch. If any edit in the batch is structural the whole batch goes structural, since the rebuild reads everything from the resulting snapshot anyway. |
| 11 | 2026-07-23 | **Revert is flow A against the active profile — not a separate operation.** | Switching and reverting are the same computation (`apply_profile` for a named profile), so giving revert its own path would create a second implementation to keep consistent with the first. |
| 12 | 2026-07-23 | **Decision 10 revised: `is_structural(&ConfigEdit) -> bool` becomes `edit_path(&ConfigEdit) -> EditPath { Param, Structural, Spatial, DspChain }`.** | Writing the Level 4 contract exposed that a bool is too coarse -- this codebase has four apply paths, not two (`EditParams`, `EditStructure`, `EditSpatial` for spatial-audio's build-and-swap, `EditDspChains`), and a profile batch can contain edits needing three of them. A bool would have silently sent a `SetSpatial` down the wrong path. Revised explicitly rather than quietly, per the standing rule about later levels invalidating earlier decisions. |
| 13 | 2026-07-23 | **Design approved at Level 4. Status set to `approved` -- ready for implementation.** | All four level sections persisted; no open questions. |

## Design: Level 2 -- Components

**Approved 2026-07-23.** No new crates. `audio-core` is untouched; `engine`
gains two config types and two snapshot fields; `control` gains a module and
five edit variants; `app` gains one action plus tray/hotkey/UI surfaces.

**A missing edit, found while building this level.** Applying a profile's DSP
means replacing a group's whole `dsp` list, and no edit does that — only
`AddDspStage`/`RemoveDspStage`/`SetDspBypass`/`SetEqBand`. Emitting an
add/remove sequence to reshape a chain is fragile and index-sensitive, so this
adds `ConfigEdit::SetDspChain(group, Vec<DspStageConfig>)` — the same
whole-collection-replace collapse as graphical-eq's `SetEqBands`.

| # | Component | Layer | Change | Justification |
|---|---|---|---|---|
| 1 | `engine::ProfileConfig`, `engine::ProfileGroupConfig` | orchestration | **new** types in `graph.rs` | Config types live in `engine` per the standing "Config type home" decision; `control` only knows how to produce them from TOML. |
| 2 | `ConfigSnapshot.profiles: Vec<ProfileConfig>`, `AppConfig.active_profile: Option<String>` | orchestration | **new** fields | Decisions 3 and 5. Both additive, both `#[serde(default)]`. |
| 3 | `control::config` `RawProfile` / `RawProfileGroup` | control | **new** serde mirrors | Same shape discipline as `RawGroup`. |
| 4 | `ConfigEdit::SetDspChain(String, Vec<DspStageConfig>)` | control | **new** | The gap above. Replaces a group's whole chain in one structural edit. |
| 5 | `ConfigEdit::{SetProfile(ProfileConfig), RemoveProfile(String), SetActiveProfile(Option<String>)}` | control | **new**, three variants | No `RenameProfile` — rename is `SetProfile(new)` + `RemoveProfile(old)` + `SetActiveProfile(new)` (decision 9). |
| 6 | `control::store` writers for the above | control | **new** arms | `toml_edit` array-of-tables, reusing the existing inline-shape rejection. |
| 7 | `control::profiles::apply_profile(&ConfigSnapshot, name) -> Vec<ConfigEdit>` | control | **new**, pure | The switch as a computed edit batch — the whole switch becomes testable with no engine, file, or frame. |
| 8 | `control::profiles::capture_profile(&ConfigSnapshot, name) -> ProfileConfig` | control | **new**, pure | Save and save-as. |
| 9 | `control::profiles::profile_is_modified(&ConfigSnapshot, name) -> bool` | control | **new**, pure | Capability 6's indicator, computed rather than stored (decision 5). |
| 10 | `ShellAction::ApplyProfile(String)` | shell | **new** variant | One action, reachable from tray, hotkey and window alike. |
| 11 | `hotkeys.rs` | shell | `spawn_hotkeys` takes the profile list and registers N chords | Capability 10 / decision 7. |
| 12 | `tray.rs` | shell | profile submenu on the existing action channel | Capability 9. |
| 13 | `app::ui` profile bar | UI | switcher, save, save-as, revert, delete, modified indicator | Capabilities 4-8. |

**Components rejected:**

- **A `ProfileStore`/`ProfileManager` type.** The three pure functions plus the
  existing `ConfigStore` are the whole feature; a manager would own nothing.
- **A `RenameProfile` edit** (decision 9).
- **A separate profile file watcher** (decision 3).

**Dependency recorded:** a profile's `mute` field only exists once the pending
`per-group-mute-solo` blueprint adds `GroupConfig.muted`. If profiles are
implemented first, that field is omitted until then; everything else here is
independent of it.

**DDD note:** `ProfileConfig` is a value object — an immutable named snapshot,
compared and copied wholesale, never mutated in place. The three
`control::profiles` functions are pure transformations over it; there is no
profile aggregate with invariants to protect.

## Design: Level 3 -- Interactions

**Approved 2026-07-23.** No domain events, no aggregate interaction — every
flow is config edits through paths that already exist, and `audio-core` never
learns profiles exist.

**Flow A — apply a profile** (identical from tray, hotkey or window)

```
ShellAction::ApplyProfile(name)
  -> edits = control::profiles::apply_profile(&self.current, &name)
             + ConfigEdit::SetActiveProfile(Some(name))
  -> if edits.iter().any(control::is_structural)   // SetGroupOutput, SetDspChain
         self.apply_structural(&edits)   // store write -> rebuild -> routing update
     else
         self.apply_params(&edits)       // lock-free param path, no rebuild
```

A profile that changes only gains must **not** rebuild the engine: a rebuild
reopens devices and costs an audible gap, absurd for a Gaming→Music switch that
only moves faders. Today the structural/param choice is made by each *call
site* (`ui.rs` picks `EditStructure` for `SetGroupOutput`); a profile batch is
mixed, so that choice moves into a `control::is_structural(&ConfigEdit)`
predicate the dispatcher applies to the whole batch (decision 10).

**Flow B — save to the active profile**

```
capture_profile(&current, active_name) -> ProfileConfig
  -> ConfigEdit::SetProfile(p) -> store write only
  -> no engine involvement at all (live state already *is* what was captured)
```

**Flow C — save as new** -> same, plus `SetActiveProfile(Some(new_name))`.

**Flow D — revert** -> exactly flow A against the active profile. Revert and
switch are the same operation; there is no separate revert path to keep
consistent.

**Flow E — delete** -> `RemoveProfile(name)`, plus `SetActiveProfile(None)`
when the deleted one was active. Live state is left alone — deleting a profile
never changes what the user is hearing.

**Flow F — modified indicator** -> `profile_is_modified(&snapshot, active)`
computed in the UI each frame. Pure, and cheap: a field-wise comparison over a
handful of groups.

**Flow G — hand-edited profile table** -> watcher -> snapshot -> the profile
list updates and the indicator recomputes. Editing a profile table **never**
auto-applies it; only an explicit switch does.

**Flow H — the active profile is deleted externally** -> `active_profile` names
something absent -> the UI shows no active profile, the indicator reads false,
live state is untouched.

**Flow I — mismatch tolerance** (capability 11) -> `apply_profile` skips
entries whose group name is absent from the snapshot, and emits nothing for
groups the profile does not mention. Both silent, not errors: a profile is a
partial description by design.

## Design: Level 4 -- Contracts

**Approved 2026-07-23.**

### `engine` (config types)

```rust
/// A named snapshot of per-group state. Value object: compared and copied
/// wholesale, never mutated in place.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileConfig {
    pub name: String,
    /// Optional global hotkey applying this profile (capability 10).
    pub hotkey: Option<HotkeyChord>,
    pub master: Gain,
    pub muted: bool,
    /// Per-group values keyed by name. A group absent here is left untouched on
    /// apply; an entry naming a missing group is skipped (capability 11).
    pub groups: Vec<ProfileGroupConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProfileGroupConfig {
    pub name: String,
    pub gain: Gain,
    pub follow_master: bool,
    pub output_device: String,
    pub dsp: Vec<DspStageConfig>,
    pub duck: Option<DuckSpecConfig>,
    pub spatial: bool,
    // `muted` joins once per-group-mute-solo lands (L2 dependency note).
    // `match_rules` deliberately absent (decision 8).
}

pub struct ConfigSnapshot { /* ... */ pub profiles: Vec<ProfileConfig> }
pub struct AppConfig      { /* ... */ pub active_profile: Option<String> }
```

### `control`

**Decision 10 revised here.** It called for `is_structural(&ConfigEdit) -> bool`.
Writing the contract out shows a bool is too coarse: this codebase has **four**
apply paths, not two -- `EditParams`, `EditStructure`, `EditSpatial`
(spatial-audio's build-and-swap) and `EditDspChains` -- and a profile batch can
contain edits needing three of them. Replaced by a classifier (decision 12).

```rust
/// Which apply path an edit requires. Today every call site hardcodes this
/// (`ui.rs` sends `EditStructure` for `SetGroupOutput`, `EditSpatial` for
/// `SetSpatial`); a profile batch is mixed, so the knowledge has to become a
/// value the dispatcher can compute per edit.
pub enum EditPath { Param, Structural, Spatial, DspChain }
pub fn edit_path(edit: &ConfigEdit) -> EditPath;

pub enum ConfigEdit {
    // ...
    SetDspChain(String, Vec<DspStageConfig>),
    SetProfile(ProfileConfig),
    RemoveProfile(String),
    SetActiveProfile(Option<String>),
}

// control::profiles -- all pure, all testable without engine, file, or frame
pub fn apply_profile(snapshot: &ConfigSnapshot, name: &str) -> Vec<ConfigEdit>;
pub fn capture_profile(snapshot: &ConfigSnapshot, name: &str) -> ProfileConfig;
pub fn profile_is_modified(snapshot: &ConfigSnapshot, name: &str) -> bool;
```

### `app`

```rust
pub enum ShellAction { /* ... */ ApplyProfile(String) }

impl Dispatcher {
    /// Applies a profile: partitions the batch by `edit_path`, writes the store
    /// once, and rebuilds at most once -- never for a gain-only profile.
    fn apply_profile_action(&mut self, name: &str);
}

// hotkeys.rs -- generalized from one chord to N (decision 7)
pub fn spawn_hotkeys(
    map: &HotkeyMap,
    profiles: &[ProfileConfig],
    actions: Sender<ShellAction>,
) -> Result<HotkeyHandle, ShellError>;

// ui.rs
enum ProfileCommand { Apply(String), Save, SaveAs(String), Revert, Delete(String) }
fn profile_bar(
    ui: &mut egui::Ui,
    profiles: &[ProfileConfig],
    active: Option<&str>,
    modified: bool,
) -> Option<ProfileCommand>;
```

### TOML shape

Purely additive; `SUPPORTED_SCHEMA_VERSION` stays 2.

```toml
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
```

### Test contracts

| Layer | Test |
|---|---|
| `control::profiles` | `capture_then_apply_is_a_no_op` -- the round-trip pinning the two functions together |
| `control::profiles` | `a_gain_only_profile_emits_only_param_path_edits` -- decision 10's regression |
| `control::profiles` | `apply_skips_entries_for_groups_that_no_longer_exist` |
| `control::profiles` | `apply_emits_nothing_for_groups_the_profile_does_not_mention` |
| `control::profiles` | `an_unknown_profile_name_yields_no_edits` |
| `control::profiles` | `profile_is_modified_is_false_immediately_after_capture` |
| `control::profiles` | `profile_is_modified_detects_a_changed_gain` |
| `control::profiles` | `applying_a_profile_never_changes_match_rules` -- decision 8 |
| `control` | `every_config_edit_variant_has_an_edit_path` (exhaustive match) |
| `control::store` | `set_profile_round_trips_through_toml` |
| `control::store` | `remove_profile_leaves_other_profiles_intact` |
| `control::store` | `set_dsp_chain_replaces_the_whole_chain` |

## Open Questions

*(none -- every judgment call raised during design was resolved and logged)*

## Design Summary

**Drift check: skipped -- `requirement_doc` is null.** Origin is
`.lattice/ux-gap-roadmap.md` Priority 5, not a requirement spec, so there are no
Scenarios/ACs or `## Technical Constraints` to compare Level 4 against and
nothing to write back.

**Components and layers**

| Layer | Components |
|---|---|
| domain (`audio-core`) | **untouched** -- never learns profiles exist |
| orchestration (`engine`) | `ProfileConfig`, `ProfileGroupConfig`, `ConfigSnapshot.profiles`, `AppConfig.active_profile` |
| control | `ConfigEdit::{SetDspChain, SetProfile, RemoveProfile, SetActiveProfile}`; `EditPath` + `edit_path`; new `control::profiles` module (`apply_profile`, `capture_profile`, `profile_is_modified`); store writers |
| shell (`app`) | `ShellAction::ApplyProfile`, `Dispatcher::apply_profile_action`, `spawn_hotkeys` generalized to N chords, tray profile submenu |
| UI (`app::ui`) | `profile_bar`, `ProfileCommand` |

**Key contracts** -- the three pure `control::profiles` functions carry the
whole feature (`capture_then_apply_is_a_no_op` pins two of them together), and
`edit_path` turns per-call-site knowledge about apply paths into a value.

**Architectural constraints honored**

- One config file, one store, one watcher, one format-preserving writer -- no
  second persistence path (decision 3).
- Additive schema: existing configs parse unchanged, no `schema_version` bump,
  and a config with no profiles behaves byte-for-byte as today.
- The RT rule still decides the path: a gain-only profile rides the lock-free
  param queue and never rebuilds (decisions 10 and 12).
- All switch logic is pure and lives in `control`, so it is testable without an
  engine, a file, or a frame.

**Domain model** -- `ProfileConfig` is a value object: an immutable named
snapshot, compared and copied wholesale. No profile aggregate, no invariants to
protect, no domain events.

**Open questions resolved during design** -- profile scope (decision 1),
dirty-state semantics (decision 2), storage location (decision 3), whether
`match_rules` are captured (decision 8, which Level 1 had left implicit).

**Gaps found while designing** -- no edit could replace a group's DSP chain
(decision 9), and the structural/param choice lived only at call sites, which a
mixed profile batch cannot use (decisions 10 and 12).
