---
feature: per-group-mute-solo
requirement_doc: null
created: 2026-07-22
status: complete
note: >
  Roadmap Priority 3 (.lattice/ux-gap-roadmap.md). Per-strip mute + solo — the
  standard mixer control Splitstream lacks (only global master mute exists).
  No requirement spec — roadmap is the origin. Rides the proven per-group
  telemetry/command path (config → store → diff → MixerCommand → mixer skip),
  the same one master mute already uses.
---

# Per-Group Mute + Solo

> Each group column gets a **Mute (M)** and **Solo (S)** toggle. Mute silences
> that group's contribution to its output; solo silences every *non*-soloed
> group. Both are the exact per-group analogue of the existing global
> `muted` output-stage kill (`mixer.rs:600`) — one more `bool` per group on the
> already-proven config → command → mixer path, plus a widget per column.

## Grounding (2026-07-22, pre-Level-1)

Real code facts the design attaches to:

- **Global mute is an output-stage skip.** `Mixer.muted` (mixer.rs:301); the
  per-group sum loop does `if self.muted { continue; }` (mixer.rs:600) — group
  gain/DSP/duck/matrix/SRC all still run, only the write into the shared output
  accumulator is skipped, so unmute resumes bit-identical with no re-ramp. A
  per-group mute is the same `continue`, gated on a per-group flag.
- **The per-group config→command path is fully proven.** `GroupConfig`
  (graph.rs:28: name, output_device, gain, follow_master, match_rules, dsp,
  duck, spatial) ↔ `RawGroup` serde (config.rs, each field `#[serde(default)]`)
  ↔ `ConfigEdit::Set*` (store.rs:24, toml_edit persistence) ↔ config diff →
  `MixerCommand::Set*` (config.rs) ↔ `Mixer::apply` (mixer.rs:440). Adding a
  per-group scalar means one field on each of these — `follow_master`/`spatial`
  are exact precedents (each a per-group `bool` threaded the whole way).
- **`GroupState`/`GroupSpec` carry per-group flags already.** `GroupState`
  holds `follow_master`; `build_group` (mixer.rs:391) threads spec→state. A
  `mute`/`solo` flag slots in beside `follow_master` identically.
- **Group meter tap is pre-mute.** The post-fader group meter observes at
  mixer.rs:575, *before* the mute `continue` at 600 — so under global mute a
  group's own bar stays live and only the output/device bar reads silent
  (level-meters.md decision). Per-group mute inherits this: a muted group's own
  meter keeps showing its (unrouted) signal, consistent with master mute.
- **UI has the widget precedent.** The master column's global-mute control is a
  custom-painted `speaker_mute_button` (ui.rs). Per-group M/S are plain-letter
  toggles (no glyph-font risk), one pair per group column beside the fader.

## Design: Level 1 -- Capabilities

**Approved 2026-07-22.**

1. **Per-group mute** — a Mute toggle per group column; when on, that group
   contributes nothing to its output. Independent of every other group and of
   `follow_master` (a global-kill analogue, same as master mute).
2. **Per-group solo (global scope, session-only)** — a Solo toggle per group
   column; when *any* group is soloed, only soloed groups are audible and every
   non-soloed group is silenced, **across all output devices** (one global solo
   bus, not per-device). Multiple groups may be soloed at once (a solo set).
   Solo is transient: never written to TOML, gone on restart.
3. **Defined precedence — mute wins** — one effective-silence rule, no
   ambiguous combination:

   ```
   audible(g) = !master_muted && !g.mute && (!solo_active || g.solo)
   solo_active = any group has solo == true
   ```

   An explicitly muted group stays silent even while soloed.
4. **Split persistence** — per-group **mute** round-trips through TOML (same as
   master `muted`, exact `follow_master` precedent), surviving restart and
   honoring TOML-is-source-of-truth. **Solo** deliberately does not: it is an
   audition tool, and a solo left on at quit would mean a silent startup with
   no visible cause.
5. **Silence suppresses ducking** — a group silenced by mute *or* by another
   group's solo stops acting as a duck trigger. Inaudible audio must not duck
   anything. The env follower keeps advancing (no frozen ballistics); only the
   published trigger env is forced to `NEG_INFINITY`.
6. **Live + click-consistent** — toggling applies instantly via the lock-free
   command ring, inheriting the *same* transition behavior as master mute (hard
   output-stage skip after matrix/SRC, smoothers and resampler stay warm, no
   new ramp added in v1). A silenced group's own meter stays live (the tap at
   mixer.rs:575 is pre-skip) — inherited from master mute, consistent.
7. **Clear visual state** — active M/S visibly distinct (M lit red, S lit
   amber); groups silenced by someone else's solo are dimmed.

Out of scope (v1): fade/ramp on mute transitions (matches master-mute
precedent — possible fast-follow), mute/solo automation, mute groups / VCA
grouping, solo-safe/solo-isolate flags, per-output (device) mute, per-device
solo scoping.

## Design: Level 2 -- Components

**Approved 2026-07-22.** No new crate edges, no new trait, no new abstraction:
mute is a scalar threaded down the proven `follow_master` path, solo is a
scalar plus one new shell action. Dependency direction unchanged
(`app -> engine -> audio-core`, `control -> engine`).

| # | Component | Layer | Change | Justification |
|---|---|---|---|---|
| 1 | `audio_core::GroupState` | domain (RT) | `+ mute: bool`, `+ solo: bool` | Exact `follow_master` slot (mixer.rs:235). |
| 2 | `audio_core::GroupSpec` | domain | `+ mute: bool` (deliberately no `solo`) | `build_group` (mixer.rs:375) threads config mute in; solo always starts `false`. That *is* decision 5's "rebuild clears solo" — enforced structurally by the absence of the field, not by a clearing step someone can forget to call. |
| 3 | `audio_core::MixerCommand` | domain | `+ SetGroupMute(GroupId, bool)`, `+ SetGroupSolo(GroupId, bool)` | Two commands, not one `SetSilence`: mute persists and solo does not, and precedence must be resolved by the mixer rather than by each caller. |
| 4 | `audio_core::Mixer::mix_tick` | domain (RT) | silence predicate at the phase-3 `continue` (mixer.rs:600); `NEG_INFINITY` trigger-env publish in phase 1 (mixer.rs:540) | Decisions 2 and 4 both land here. No new struct. |
| 5 | `engine::graph::GroupConfig` | orchestration | `+ muted: bool`, mapped to `GroupSpec.mute` in `resolve` | `follow_master`/`spatial` precedent (graph.rs:32). |
| 6 | `control::config` | control | `RawGroup.muted` (`#[serde(default)]`); diff emits `MixerCommand::SetGroupMute` | Mirrors the `follow_master` diff arm (config.rs:383) exactly. |
| 7 | `control::store::ConfigEdit` | control | `+ SetGroupMute(String, bool)` + `toml_edit` write | Mirrors store.rs:128. |
| 8 | `app::ShellAction` | shell | `+ SetSolo(String, bool)` | **Required, not optional.** Every existing UI mutation is `EditParams(Vec<ConfigEdit>)`, and that path always writes the store (main.rs:180). Session-only solo must reach the mixer without persisting, so it cannot ride `ConfigEdit` at all. |
| 9 | `app::Dispatcher` | shell | `+ rebuild_generation: u64`, bumped at the two rebuild sites (main.rs:133 watcher reload, main.rs:156 `apply_structural`); `+ apply_solo(name, bool)` | Resolves open question 1: the authoritative side publishes the rebuild fact. |
| 10 | `app::event_pump::UiState` | shell | `+ rebuild_generation: u64` | Written in `set_current` beside `snapshot`, the existing shell->UI publish point. |
| 11 | `app::ui::SettingsApp` | UI | `+ soloed: HashSet<String>`, `+ seen_generation: u64`; M/S toggles in `group_column`; dim groups silenced by another group's solo | `HashSet` is right here even though `Screen` replaced a bool map with an enum — multi-solo is explicitly allowed, so "several true at once" is a valid state, not a representable-invalid one. |

**Components rejected:**

- A `SoloBus` / `SilenceResolver` domain type — one call site, one boolean
  expression. Inlined into `mix_tick`.
- Shell-owned solo state (decision 5).
- A new engine telemetry field for rebuild detection — `Epoch` already exists
  but bumps on DSP chain swaps too (runtime.rs:1277), so it would clear solo on
  an unrelated EQ edit; and the shell already knows when it rebuilds, so no
  engine change is needed at all.

**DDD note:** nothing here is a new aggregate or value object. `mute`/`solo` are
flags on the existing `GroupState` entity, inside the `Mixer` aggregate whose
root already owns every group's lifecycle. The silence rule is an invariant of
that aggregate — which is exactly why it is resolved inside `mix_tick` and not
by callers.

## Design: Level 3 -- Interactions

**Approved 2026-07-22.** Mute rides the existing param fast path unchanged;
solo gets one new shell-level path that deliberately stops short of the store.

Verified before drafting: `control::group_id_for` is positional over
`snapshot.groups`, and `engine::graph::resolve` *reserves* the index of a
parked group rather than compacting (graph.rs:158, test
`parked_group_keeps_its_reserved_group_id` at graph.rs:446) — so the
snapshot→engine `GroupId` mapping holds under drift-and-recovery. This is
**not** the level-meters `output_names` bug family. It does expose one
asymmetry, flow F.

**Flow A — mute toggle (persisted, existing fast path, zero new plumbing)**

```
UI click M
  -> ShellAction::EditParams([ConfigEdit::SetGroupMute(name, !muted)])
  -> Dispatcher::apply_params (main.rs:171)
      |- edits_to_mixer_commands(edits, &self.current)   // pre-edit snapshot ids
      |    -> MixerCommand::SetGroupMute(GroupId, bool)
      |    -> handle.apply_params -> epoch-tagged lock-free queue
      |    -> mixer thread: Mixer::apply -> g.mute = b    // audible next tick
      \- store.apply(edits) -> toml_edit write -> new snapshot
           -> set_current -> UiState.snapshot -> M renders lit from snapshot
  -> watcher sees own write -> store.is_echo -> set_current only, no re-apply
```

The M button renders from `snapshot.groups[i].muted`, not from local UI state —
same as every existing control.

**Flow B — solo toggle (session-only, new path)**

```
UI click S
  -> SettingsApp.soloed.insert/remove(name)     // UI owns it, renders from here
  -> ShellAction::SetSolo(name, on)
  -> Dispatcher::apply_solo
      |- control::group_id_for(&self.current, name)
      |- handle.apply_params([MixerCommand::SetGroupSolo(id, on)])
      \- (no store.apply -- no TOML write, no snapshot change, no echo)
```

**Flow C — silence resolution inside `mix_tick` (RT, per tick)**

```
phase 0  solo_active = groups.iter().any(|g| g.solo)          // derived, decision 6
phase 1  for each g:
           env = g.env_follower.process_block(...)            // ALWAYS runs -- ballistics must advance
           g.last_env_db = if silenced(g) { NEG_INFINITY } else { env }
phase 2  duck targets read last_env_db -> silenced groups trigger nothing (decision 4)
phase 3  for each g: gain -> dsp -> duck -> meter.observe -> matrix -> SRC
           if self.muted || silenced(g) { continue; }         // output-stage skip, mixer.rs:600
           sum into out.accum
phase 4  per-output limiter -> output meter

silenced(g) = g.mute || (solo_active && !g.solo)              // decision 2: mute wins
```

Two invariants inherited from master mute, both load-bearing: smoothers/SRC
stay warm because the skip is *after* SRC (unmute resumes with no re-ramp), and
the group's **own** meter stays live because the tap at mixer.rs:575 is
pre-skip, while its output-device meter falls.

**Flow D — structural rebuild clears solo (decisions 5, 8)**

```
structural edit, or watcher snapshot with delta.structural
  -> handle.rebuild -> new Mixer, every GroupState.solo = false   // GroupSpec has no solo field
  -> Dispatcher.rebuild_generation += 1
  -> set_current -> UiState.rebuild_generation
  -> next UI frame: gen != seen_generation -> soloed.clear(); seen_generation = gen
```

Both sides converge on "no solo" from opposite directions; neither re-derives
the other's logic.

**Flow E — hand-edited `muted = true` in TOML** — `handle_watcher_snapshot`
(main.rs:122) -> `control::diff` -> non-structural -> `delta.params` carries
`SetGroupMute` -> mixer. Requires the diff to classify `muted` as a **param**,
alongside `follow_master` (config.rs:383). No rebuild.

**Flow F — parked-group asymmetry (accepted, decision 9)** — a parked group
(drift-and-recovery: no device available) has no `GroupState`, so
`Mixer::apply` finds no target for its command. Mute stays eventually
consistent (it persists to TOML and returns via `GroupSpec.mute` when the group
un-parks); solo does not.

**Domain events:** none introduced. Cross-aggregate communication is unchanged
— the duck sidechain already reads `last_env_db` within the single `Mixer`
aggregate, and decision 4 changes what that field *publishes*, not who reads it
or when.

## Design: Level 4 -- Contracts

**Approved 2026-07-22.** Every signature below is written against types read
from the real code this session, not from memory.

### `audio-core` (domain)

```rust
// sample.rs -- GroupSpec, beside `spatial`
pub struct GroupSpec {
    // ...
    /// Persisted per-group mute (per-group-mute-solo.md). Deliberately no
    /// `solo` counterpart: solo is session-only, so every rebuild starts each
    /// group unsoloed -- the absence of the field *is* the guarantee.
    pub mute: bool,
}

// mixer.rs -- MixerCommand
pub enum MixerCommand {
    // ...
    /// Per-group output-stage kill, persisted in config. Same skip point as
    /// `SetMuted` (after matrix/SRC), so smoothers and the resampler stay warm.
    SetGroupMute(GroupId, bool),
    /// Session-only solo, never persisted. Any group soloed puts the whole
    /// mixer in solo mode: non-soloed groups are silenced on every output.
    SetGroupSolo(GroupId, bool),
}

// mixer.rs -- GroupState, beside `follow_master`
struct GroupState { /* ... */ mute: bool, solo: bool }

impl Mixer {
    /// Derived once per `mix_tick`, never cached (decision 6).
    fn solo_active(&self) -> bool { self.groups.iter().any(|g| g.solo) }
}

/// The single effective-silence rule (decision 2: mute wins over solo). Free
/// function, not a method -- it needs one group and one flag, and keeping it
/// callable from both phase 1 and phase 3 of `mix_tick` avoids a second borrow
/// of `self`.
fn silenced(g: &GroupState, solo_active: bool) -> bool {
    g.mute || (solo_active && !g.solo)
}
```

### `engine` (orchestration)

```rust
// graph.rs -- GroupConfig, beside `spatial`
pub struct GroupConfig { /* ... */ pub muted: bool }
// resolve(): GroupSpec { mute: g.muted, .. }  -- solo is never sourced from config
```

### `control`

```rust
// config.rs -- RawGroup
#[derive(Deserialize)]
struct RawGroup { /* ... */ #[serde(default)] muted: bool }

// config.rs -- diff(), beside the follow_master arm (config.rs:383)
if o.muted != n.muted { params.push(MixerCommand::SetGroupMute(id, n.muted)); }

// store.rs -- ConfigEdit
pub enum ConfigEdit { /* ... */ SetGroupMute(String, bool) }

// store.rs -- apply arm + group_table writer
ConfigEdit::SetGroupMute(name, muted) => {
    find_group_table(doc, name)?["muted"] = value(*muted);
}
t["muted"] = value(g.muted);   // in group_table, so AddGroup round-trips
```

TOML shape -- one scalar, `#[serde(default)]` so every existing config still
parses unchanged:

```toml
[[group]]
name = "Game"
muted = true
```

### `app` (shell + UI)

```rust
// main.rs
pub enum ShellAction {
    // ...
    /// Session-only per-group solo. Deliberately NOT an `EditParams` variant:
    /// `apply_params` always follows the mixer command with `store.apply`, and
    /// solo must never reach TOML (decision 1).
    SetSolo(String, bool),
}

impl Dispatcher {
    fn apply_solo(&mut self, name: &str, on: bool);   // no store write
    // + field: rebuild_generation: u64
}

// main.rs -- edits_to_mixer_commands, beside the SetFollowMaster arm
ConfigEdit::SetGroupMute(name, muted) =>
    control::group_id_for(current, name).map(|id| MixerCommand::SetGroupMute(id, *muted)),

// event_pump.rs -- UiState
pub struct UiState {
    // ...
    /// Bumped by the dispatcher on every mixer rebuild so the settings window
    /// can drop its session-only solo set (decision 8). The UI must not infer
    /// this from snapshot diffs.
    pub rebuild_generation: u64,
}

// ui.rs -- SettingsApp
struct SettingsApp {
    // ...
    /// Session-only solo set, keyed by group name. UI-owned (decision 5); a
    /// `HashSet` because multiple groups may be soloed at once.
    soloed: HashSet<String>,
    seen_generation: u64,
}

/// Pure, so the clear-on-rebuild rule is unit-testable without an egui frame.
fn clear_solo_on_rebuild(soloed: &mut HashSet<String>, seen: &mut u64, current: u64);

/// Plain-letter toggle (M / S). ASCII only -- no glyph-font risk, unlike an
/// emoji-range icon; `speaker_mute_button`'s custom paint isn't needed for a
/// letter.
fn toggle_button(ui: &mut egui::Ui, label: &str, active: bool, tint: egui::Color32) -> bool;
```

`GroupColumnCtx` gains **nothing**: `group_column` already takes `&mut self`,
so it reads `self.soloed` directly (`solo_active = !self.soloed.is_empty()`).
Keeps the struct off the `too_many_arguments` line it was extracted to dodge.

### Test contracts

| Layer | Test |
|---|---|
| `audio-core` | `a_muted_group_contributes_nothing_while_other_groups_still_sum` |
| `audio-core` | `solo_silences_every_non_soloed_group_across_outputs` |
| `audio-core` | `an_explicitly_muted_group_stays_silent_while_soloed` (decision 2) |
| `audio-core` | `a_silenced_group_stops_triggering_its_duck_target` (decision 4) |
| `audio-core` | `a_muted_groups_own_meter_stays_live` (flow C invariant) |
| `control` | `set_group_mute_round_trips_through_toml` |
| `control` | `a_changed_group_muted_diffs_as_a_param_not_structural` (flow E) |
| `app` | `edits_to_mixer_commands_maps_set_group_mute` |
| `app` | `a_bumped_rebuild_generation_clears_the_solo_set` (pure helper) |

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-22 | **Solo is session-only, never persisted to TOML.** | Matches every DAW/mixer convention (solo = transient audition). A persisted solo left on at quit means a mostly-silent startup with no discoverable cause. Rejected: persist as a `RawGroup.solo` field (would have been a zero-new-plumbing `follow_master` clone, but the silent-startup trap outweighs the consistency win). Consequence: solo needs a mixer path that bypasses `ConfigStore`. |
| 2 | 2026-07-22 | **Mute wins over solo.** `audible = !master_muted && !mute && (!solo_active \|\| solo)`. | An explicit per-strip mute is the user's most direct statement; a mode-like solo must not override it. Matches Reaper/Pro Tools/Ableton. Rejected: solo un-mutes (some hardware desks do this, but a lit mute button over audible signal is a contradictory UI state). |
| 3 | 2026-07-22 | **Solo scope is global across all output devices**, one `solo_active` flag on the mixer. | Single-desk mental model; the mixer window already presents every group as one strip row regardless of destination device. Rejected: per-`OutputId` solo (more useful for genuinely unrelated headset/speaker feeds, but makes `solo_active` a per-output lookup and forces the UI to show *which devices* are in solo mode — complexity not earned yet). Revisit if per-device audition is requested. |
| 4 | 2026-07-22 | **A silenced group (mute or solo-suppressed) is not a duck trigger.** | Ducking for audio the user cannot hear reads as a bug: muting Chat would otherwise leave Game permanently ducked whenever Discord makes noise. Implementation: `mix_tick` phase 1 (mixer.rs:540) still runs `env_follower.process_block` (ballistics must keep advancing — see the frozen-meter learning, 2026-07-22) but publishes `NEG_INFINITY` as `last_env_db` for silenced groups. The duck target's own `DuckTargetGain` smoothing handles the resulting release. Rejected: leave duck untouched (smallest diff, wrong behavior). |
| 5 | 2026-07-22 | **Solo state is owned by the UI; a structural rebuild clears it.** | Smallest change — no shell field, no re-apply hook in `apply_structural`. A rebuild returning every group to audible fails loud, not silent. Rejected: shell-owned solo re-applied after each rebuild (survives unrelated config edits, but adds runtime state to the shell for a transient control). Consequence: the UI must learn that a rebuild happened so it can clear its own set — see open question 1 (resolved by decision 8). |
| 6 | 2026-07-22 | **`solo_active` is derived by scanning groups once per `mix_tick`, not cached as a `solo_count` counter maintained in `apply`.** | Group count is ~4 — the scan is free next to the per-sample work already in the loop. A counter is state that can desync from the flags it summarizes (a missed decrement leaves the mixer permanently in solo mode with no group lit); a derived value has no invalid state. Rejected: cached counter (micro-optimization buying a real invariant to maintain). |
| 7 | 2026-07-22 | **New `ShellAction::SetSolo(String, bool)` rather than reusing `EditParams(Vec<ConfigEdit>)`.** | `apply_params` (main.rs:171) *always* follows the mixer command with `store.apply(edits)`. There is no existing UI mutation path that reaches the mixer without persisting, so decision 1 (session-only solo) forces a new variant. `apply_solo` resolves name → `GroupId` against `self.current` and calls `handle.apply_params` with the mixer command only. |
| 8 | 2026-07-22 | **Rebuild detection is app-layer: `Dispatcher.rebuild_generation` → `UiState.rebuild_generation` → UI compares against `seen_generation` and clears its solo set.** Resolves open question 1. | The shell already knows when it rebuilds (main.rs:133, :156) and already publishes to `UiState` in `set_current` — no engine change. Rejected: reusing `EngineHandle::epoch()` (bumps on DSP chain swaps too, runtime.rs:1277, so an EQ edit would silently clear solo); rejected: UI diffing snapshots to infer a structural change (re-derives `apply_structural`'s trigger condition on the wrong side of the boundary — same shape as the level-meters `output_names` review finding). |

| 9 | 2026-07-22 | **Soloing a parked group is accepted as a no-op in v1** (flow F) — no UI disabling, no parked-state plumbing into `UiState`. | A parked group has no `GroupState`, so `SetGroupSolo` lands nowhere, `solo_active` stays false, and the UI shows S lit while nothing else is silenced. Accepted because the parked group is *already* silent (its device is gone), so the only wrong part of the user's experience is that other groups keep playing. Rejected: exposing parked names in `UiState` to disable the button (new cross-layer plumbing for a transient degraded state). Revisit if drift-and-recovery episodes turn out to be common in practice. |

| 10 | 2026-07-22 | **Design approved at Level 4. Status set to `approved` — ready for implementation.** | All four level sections persisted; no open questions remain. |
| 11 | 2026-07-23 | **Implementation complete, inside-out (audio-core → engine → control → app → UI).** All 9 planned test contracts present, plus one symmetric bonus (`an_unchanged_rebuild_generation_leaves_the_solo_set_alone`, pure-helper coverage of the no-op branch). Full workspace suite green (audio-core 95, control 50, engine 96, app 49) and `cargo clippy --workspace --all-targets` clean. No deviation from the L2 component list or L4 contracts — every touched file matches the Design Summary's layer table exactly. Status → complete. | Verified against the real diff before closing, not assumed from the blueprint. |

## Open Questions

*(none open — question 1 resolved by decision 8)*

## Design Summary

**Drift check: skipped — `requirement_doc` is null.** This feature's origin is
`.lattice/ux-gap-roadmap.md` Priority 3, not a requirement spec, so there are no
Scenarios/ACs or `## Technical Constraints` to compare Level 4 against and
nothing to write back.

**Components and layers**

| Layer | Components touched |
|---|---|
| domain (`audio-core`) | `GroupSpec.mute`; `GroupState.{mute, solo}`; `MixerCommand::{SetGroupMute, SetGroupSolo}`; `Mixer::solo_active`; free fn `silenced`; the phase-1 env publish and phase-3 skip in `mix_tick` |
| orchestration (`engine`) | `graph::GroupConfig.muted`; `resolve` maps it to `GroupSpec.mute` |
| control | `RawGroup.muted`; `diff` param arm; `ConfigEdit::SetGroupMute` + `toml_edit` write + `group_table` writer |
| shell (`app`) | `ShellAction::SetSolo`; `Dispatcher::apply_solo`; `Dispatcher.rebuild_generation`; `UiState.rebuild_generation`; `edits_to_mixer_commands` arm |
| UI (`app::ui`) | `SettingsApp.{soloed, seen_generation}`; `clear_solo_on_rebuild`; `toggle_button`; M/S in `group_column` with dimming |

**Key contracts** — `silenced(g, solo_active) = g.mute || (solo_active && !g.solo)`
is the whole feature's semantic core; `SetGroupMute` persists, `SetGroupSolo`
does not; `GroupSpec` carries `mute` and pointedly not `solo`.

**Architectural constraints honored**

- Dependency direction unchanged; no new crate edges, traits, or abstractions.
- RT rules: no allocation, no locking, no new per-sample work on the mixer
  thread — `solo_active` is one `any()` over ~4 groups per tick.
- TOML-is-source-of-truth holds for mute; solo is the deliberate, documented
  exception (decision 1) and is therefore given a path that structurally cannot
  reach the store (decision 7).
- The UI never re-derives another layer's decision: the rebuild fact is
  published by the side that owns it (decision 8).

**Domain model** — no new aggregate, entity, or value object. `mute`/`solo` are
flags on the existing `GroupState` entity inside the `Mixer` aggregate, and the
silence rule is an aggregate invariant, which is why it resolves inside
`mix_tick` rather than at any caller.

**Open questions resolved during design** — how the UI learns a rebuild
happened (decision 8: shell-published generation counter, not `Epoch`, not
snapshot diffing).

**Known accepted gap** — soloing a parked group is a no-op (flow F, decision 9).

## Key Files

| Path | Role |
|---|---|
| .lattice/ux-gap-roadmap.md | Origin (Priority 3) |
| crates/audio-core/src/sample.rs | `GroupSpec.mute` |
| crates/audio-core/src/mixer.rs | `GroupState.{mute, solo}`; `MixerCommand::{SetGroupMute, SetGroupSolo}`; `silenced` fn; `Mixer::solo_active`; `mix_tick` phase 1 env-publish + phase 3 skip |
| crates/engine/src/graph.rs | `GroupConfig.muted`; `resolve` maps it to `GroupSpec.mute` |
| crates/control/src/config.rs | `RawGroup.muted`; diff param arm |
| crates/control/src/store.rs | `ConfigEdit::SetGroupMute`; `toml_edit` write + `group_table` writer |
| crates/app/src/main.rs | `ShellAction::SetSolo`; `Dispatcher::apply_solo`; `Dispatcher.rebuild_generation`; `edits_to_mixer_commands` arm |
| crates/app/src/event_pump.rs | `UiState.rebuild_generation` |
| crates/app/src/ui.rs | `SettingsApp.{soloed, seen_generation}`; `clear_solo_on_rebuild`; `toggle_button`; M/S in `group_column` with dimming |
