---
feature: routing-truthfulness
requirement_doc: null
created: 2026-07-25
status: approved
note: >
  Origin: `.lattice/reviews/2026-07-25-end-to-end-audit.md` B9/B10/B18, plus
  the user-reported "routing seems to be doing something but I can't tell
  what". No requirement spec. This blueprint revises logged decisions in two
  prior blueprints (mixer-ui-redesign, process-loopback-capture) rather than
  patching defects — see Grounding.
---

# Routing Truthfulness

> The mixer UI promises three things it does not deliver: dragging an app out
> of a group silently does nothing whenever a glob matched it, Master's
> "Routed Apps" footer is permanently empty in the shipped default
> configuration, and one drag of an app whose executable cannot be read
> installs a rule that captures every other unreadable app. This makes the
> assignment UI mean what it shows.

## Grounding (2026-07-25, pre-Level-1)

Real code facts, verified this session. The audit's B18 framing does not
survive grounding; B9 turns out to be a logged decision, not a defect.

### The chain nobody traced end to end

1. First-run onboarding creates the default group with
   `match_rules: vec!["*".into()]` (`app/src/ui.rs:614`) — this is
   `process-loopback-capture.md`'s own decision (2026-07-21): `*` **is** the
   recommended catch-all mechanism, reusing `match_session`'s
   exact-beats-glob precedence rather than adding an `is_catch_all` field.
2. `*` matches every session (`engine/src/rules.rs:113-119`).
3. Master's footer renders `unassigned_sessions(all_sessions, routes)` —
   sessions in no route (`app/src/ui.rs:673`). With a `*` group there are
   none, ever.
4. `empty_reason(Unassigned, any_sessions: true, zone_had_chips: false,
   searching: false)` returns `AllRouted` -> **"All apps are routed."**
   Permanently. Literally true, operationally useless.
5. Dropping a chip on Master calls `resolve_drag_assign(name, None, groups)`
   (`app/src/ui.rs:2030`). A glob-matched session has no `ExactName` anywhere,
   so both match arms miss and the batch is **empty**. The chip snaps back on
   the next reconcile with no feedback.

**On a default install, out of the box: the unassigned pool is permanently
empty and Master's drop zone is inert.** Not an edge case — the shipped
default.

### Correction: B18 describes a configuration the product does not ship

The audit says the unassigned pool "reads as a routing destination" and that
unrouted apps bypassing Splitstream looks like a bug. That is the
*no-catch-all* configuration. With the default `*`, the symptom inverts — the
pool is always empty. Both configurations are broken, differently, and a fix
has to address both.

### Correction: B9 is a logged decision, not an unnoticed bug

`mixer-ui-redesign.md` (2026-07-21), Level 2:

> Known v1 limitation: dragging a session back to Master's footer only
> "sticks" if it was exact-name-assigned — a session still covered by a glob
> rule elsewhere re-matches instantly on the next reconcile tick.

and Level 3 flow C: *"No-op visually next frame if a glob rule elsewhere still
matches (documented limitation)."* There is a passing test,
`resolve_drag_assign_never_touches_glob_rules` (`ui.rs:2210`), whose comment
explains the behaviour as intentional.

**The seam:** `mixer-ui-redesign` accepted "unassign fails when a glob exists"
in the same week `process-loopback-capture` made `*` the recommended default.
Each decision is defensible alone; together they make the primary routing
gesture dead in the default configuration. Neither blueprint's own review
could have caught it — exactly the cross-blueprint seam class the operational
learnings already name (2026-07-18 [review]).

### B10 confirmed, marginally worse than written

`resolve_drag_assign` pushes `session_file_name.to_string()` unconditionally
(`ui.rs:2038`); for an unreadable process path that is `""` (`ui.rs:1948` ->
`PathBuf::default()` from `win-audio/src/sessions.rs`'s
`describe_session`). `MatchRule::parse("")` finds no `*`/`?` so it yields
`ExactName("")` (`rules.rs:68-74`), and `match_session` compares it against
**both** `file_name` and `full_path`, each `""` for such a session
(`rules.rs:94-108`) — so one drag captures every unreadable-path session.
Note `split_rules` (`ui.rs:2061`) already filters empty strings, so the
hand-typed rule path is guarded; only the drag path is not.

### The missing primitive

Drag-**assign** across a glob already works: exact rules are checked as a
class before glob rules (`rules.rs:101-119`), so an added `ExactName` wins
over another group's `*`. Only drag-**unassign** is broken, and the reason is
structural: **the rule vocabulary can express "route X to group G" but has no
way to express "do not route X".** That is the gap, not the glob handling.

### Existing hook to reuse

Self-exclusion already runs centrally: `compute_desired(&state.rules,
&state.live_sessions, state.self_pid)` (`routing.rs:250`) skips
`*pid == self_pid` (`routing.rs:220`), covered by
`self_pid_is_never_matched_even_by_a_catch_all_rule` (`routing.rs:485`). A
user-configured exclusion is a different *kind* of exclusion (config, not
runtime) but the precedent for "enforced once, centrally, ahead of matching"
is established.

## Design: Level 1 -- Capabilities

**Approved 2026-07-25.**

1. **Dragging an app out of a group actually removes it, and it stays out** —
   regardless of whether it landed there via an exact rule, a glob, or the
   `*` catch-all. The gesture either works or says why it cannot; it never
   silently no-ops.
2. **Master's "Routed Apps" footer tells the truth about what it holds and
   what dropping there does** — in both configurations: with a catch-all
   (where "unassigned" currently cannot exist) and without one (where
   unassigned apps play straight to Windows, untouched by Splitstream).
3. **An app whose executable Splitstream cannot read never produces a routing
   rule** — it cannot be silently assigned, and it cannot drag every other
   unreadable app into a group with it.
4. **The user can tell *why* an app sits in a group** — matched by name, by a
   pattern, or by the catch-all. Without this, capability 1's outcome is
   unpredictable from the user's side: two chips that look identical behave
   differently when dragged.
5. **Existing configurations keep working untouched** — whatever mechanism
   capability 1 needs must not rewrite anyone's hand-written `match_rules`,
   and a config with no exclusions must behave byte-identically to today.

Capability 4 is the designated cut if scope needs trimming — 1, 2 and 3 are
each a distinct broken behaviour, 4 is comprehension support for 1.

Out of scope: B11/B12 (session discovery), B13-B16 (platform correctness), and
the matching *precedence* rules themselves (exact-beats-glob, config-order
tiebreak) — those stay exactly as `session-routing.md` approved them.

## Design: Level 2 -- Components

**Approved 2026-07-25.**

Drag-*assign* across a glob already works — exact rules are checked as a class
before globs, so an added `ExactName` beats another group's `*`. Only
drag-*unassign* is broken, and structurally: the vocabulary can say "route X
to group G" but not "do not route X". One new precedence tier closes B9 and
B18 together.

```
session -> is self pid?   --yes--> never routed          (unchanged, routing.rs:220)
              | no
              v
           excluded?      --yes--> NOT ROUTED <--+  NEW TIER
              | no                     |         |
              v                        |         |
        exact rule match? --yes--> group         |  unchanged
              | no                               |  precedence
              v                                  |  (session-routing.md)
        glob rule match?  --yes--> group         |
              | no                               |
              v                                  |
           NOT ROUTED -----------------------+---+
        (plays through Windows directly)
```

Both "not routed" outcomes converge on one state — which is what lets Master's
footer be a single honest zone with a single message rather than two cases the
user must distinguish.

**Why the exclusion list is global, not per-group:** dropping on Master means
"not routed by Splitstream", not "not in group X". A per-group exclusion would
let a session excluded from X fall through to Y's glob and reappear in Y — the
same silent surprise in a new place.

| # | Component | Layer | Change | Owns |
|---|---|---|---|---|
| 1 | `match_session` | `engine::rules` — domain (pure) | modified | The new precedence tier, and the guard that an `ExactName("")` never matches anything. |
| 2 | `AppConfig.excluded` + `ConfigEdit::SetExcluded` + store writer | `engine::graph` (config type) + `control` (edit + writer) | new field, new edit | Persisting the exclusion list and editing it format-preservingly. |
| 3 | Match provenance | `engine::rules` + `engine::routing` read model | modified | *How* a session matched — name, pattern, or catch-all — carried on the existing routes read model. **Capability 4 only.** |
| 4 | `resolve_drag_assign` | `app::ui` | modified | Unassign emits an exclusion edit instead of an empty batch; assign clears an exclusion; an empty file name produces no edits at all. |
| 5 | Master pool semantics | `app::ui` — `empty_reason` / `empty_message` / Master footer | modified | Saying what the zone holds and what dropping there does, in both configurations. |
| 6 | Chip provenance indicator | `app::ui` | new | The visible marker. **Capability 4 only.** |
| 7 | Master settings page | `app::ui` — `Screen` enum | new | The exclusion list as a viewable/removable surface, reached by a gear on Master. **Added at Level 3 (user direction) — see decision 5.** |

**Components 3 + 6 are the clean cut line** — they exist solely for capability
4 and drop together without touching 1/2/4/5.

**Challenged and kept minimal:** no `is_catch_all` field
(`process-loopback-capture` already rejected it, nothing here changes that
reasoning); no per-group exclusion type; no new read model (provenance rides
`RoutingReader`'s existing routes output, the level-meters
"ride-the-existing-telemetry-path" precedent); self-pid exclusion is **not**
merged into the new list — it stays in `compute_desired` as a runtime fact
while the config list belongs in `match_session` as matching vocabulary.

**Architecture:** `AppConfig` lives in `engine::graph`, correct by this
codebase's own rule that *persistence, not subject matter, decides which crate
owns a type* (the `AccentPreset` finding). `ConfigEdit::SetExcluded` lives in
`control` and must be classified in `profiles.md`'s `edit_path`; that match is
exhaustive, so a missing arm is a compile error, not a silent gap.

**DDD:** `engine::rules` is the domain module — pure matching, no OS, no
threads. The exclusion list extends the matching *vocabulary*; it is not a new
entity, aggregate, or value object. `MatchRule` remains the only value object
in play.

## Design: Level 3 -- Interactions

**Approved 2026-07-25.**

**Flow A — drag a chip from a group onto Master (unassign).** The gesture that
is currently dead.

1. The drop resolves the session's file name against the current config.
2. Emits **one** edit: append the file name to `excluded`.
3. It does **not** touch `match_rules` — not the target group's, not any
   other's.
4. Next reconcile: `match_session` hits the exclusion tier, returns not-routed,
   the capture for that pid is closed, and the chip appears in Master's
   footer. It stays there.

Not stripping the group's `ExactName` is the deliberate part. The alternative
(add exclusion *and* remove the rule, mirroring today) makes an
assign->unassign->assign round trip three rule rewrites instead of zero, and
edits `match_rules` the user may have hand-written — against capability 5.
Leaving the rule standing makes the round trip purely exclusion add/remove.
Same "diff against live state, emit only real differences" shape the
operational learnings record three times. Cost: a group's rules text can list
`game.exe` while the chip sits in Master — a contradiction that is visible and
true, and that capability 4's provenance marker explains.

**Flow B — drag a chip from Master onto a group (assign).**

1. Remove the file name from `excluded` if present. **(new)**
2. Add `ExactName(file_name)` to the target if absent. (unchanged)
3. Strip the equivalent `ExactName` from every other group. (unchanged)

An app that was merely never-matched takes the identical path with step 1 a
no-op.

**Flow C — drag a chip from group X to group Y.** Unchanged, plus Flow B step
1's idempotent exclusion clear. Already worked, because exact beats glob.

**Flow D — a session whose executable cannot be read.** Two independent
guards:

- **The gesture is prevented, not failed.** A chip with no readable file name
  is not draggable and carries a tooltip saying why. Capability 1 says the
  gesture never silently no-ops; refusing up front beats accepting a drag and
  discarding it.
- **`match_session` refuses an empty `ExactName` regardless.** This also
  repairs configs already poisoned by a drag made before this change — those
  users have a live `""` rule capturing every unreadable session today.

**Flow E — matching, engine side.** `compute_desired` skips `self_pid`
(unchanged), then calls `match_session` with the rules *and* the exclusion
list. An excluded session matches nothing, so no capture is opened for it and
**no session mute is applied to it** — it genuinely plays through Windows,
which is exactly what the pool claims.

**Flow F — Master's footer says what it is.** The zone holds excluded apps
*and* never-matched apps; both are "not routed by Splitstream", so one label
covers both: **"Not routed — playing through Windows directly."**

| Condition | Today | Becomes |
|---|---|---|
| No sessions at all | "No apps are playing audio." | unchanged |
| Search hid everything | "No apps match \"x\"." | unchanged |
| Sessions exist, none unrouted | "All apps are routed." | teaches the gesture: "All apps are routed. Drag one here to stop routing it." |

That last row is B18's default-config half: the state was previously permanent
and unescapable, and is now reachable in both directions.

**Flow G — provenance (capability 4).** `match_session` reports which tier
matched. `compute_desired` records it per pid and it rides out on
`RoutingReader`'s existing routes output — no new channel. The chip renders a
marker: matched by name (the default, unmarked), by pattern, or by the
catch-all. Behaviour is uniform across all three after Flow A, so provenance is
purely explanatory — which is what keeps components 3 + 6 cuttable.

**Flow H — a config with no exclusions.** `excluded` absent or empty -> the
tier is a no-op -> behaviour byte-identical to today, and the TOML key is
written only the first time it is used. Capability 5.

**Flow I — Master settings page (component 7).** Gear on Master's header ->
`Screen::MasterSettings`, mirroring `Screen::GroupSettings(name)`: full-width
page, `⬅ Back` returns to the mixer. The page lists every entry in `excluded`,
each with a remove control emitting a whole-list `SetExcluded`. Unlike
`GroupSettings`, this screen can never go stale — Master always exists — so it
needs no equivalent of `screen_is_stale`'s fallback. The page holds the
exclusion list and nothing else; no other content is invented for it.

**Flow J — cross-feature check, found by tracing rather than assumed.**
`profiles.md` states a profile switch never touches `match_rules`. Exclusions
are the same class of state — routing identity, not a mixing parameter — so
`ProfileGroupConfig` must not capture or apply `excluded` either. Without
this, switching profiles would silently re-route apps the user had excluded.
`AppConfig` is not part of a profile today, so this holds by construction;
recorded so a future profile-scope change does not quietly break it.

## Design: Level 4 -- Contracts

**Approved 2026-07-25.**

```rust
// -- 1. engine::rules -- the exclusion tier, guard, and provenance --------

impl GlobPattern {
    /// `GlobPattern` is a tuple struct with a private field today -- this is
    /// the one accessor `MatchKind::CatchAll` needs, kept in the domain so
    /// the UI never re-derives catch-all-ness from raw rule strings.
    pub fn is_catch_all(&self) -> bool;
}

/// How a session reached its group (capability 4). Explanatory only -- every
/// tier behaves identically under drag once Flow A lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// An `ExactName` rule named this process.
    Name,
    /// A glob rule matched it, and the pattern is not a bare `*`.
    Pattern,
    /// A bare `*` catch-all claimed it.
    CatchAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub group: GroupId,
    pub kind: MatchKind,
}

/// `excluded` is checked ahead of every rule tier (L3 flow E), compared
/// case-insensitively against the process *file name* only -- the drag
/// gesture never writes anything else (decision 4).
///
/// An `ExactName("")` rule never matches, whatever the session (B10). This
/// also repairs configs already poisoned by a pre-fix drag.
///
/// Return type widens from `Option<GroupId>` to `Option<Match>`; the sole
/// caller is `routing::compute_desired`. If capability 4 is cut, this stays
/// `Option<GroupId>` and `MatchKind`/`Match`/`is_catch_all` all disappear.
pub fn match_session(
    info: &SessionInfo,
    rules: &[GroupRules],
    excluded: &[String],
) -> Option<Match>;

// -- 2. engine::graph + control -- persistence ----------------------------

pub struct AppConfig {
    // ...existing fields unchanged...
    /// Process file names no group may claim (capability 1). Empty or absent
    /// = today's behaviour exactly; the TOML key is written only on first use.
    pub excluded: Vec<String>,
}

pub enum ConfigEdit {
    // ...existing variants unchanged...
    /// Whole-list replace, never add/remove -- this codebase's CRUD-collapse
    /// rule (`SetEqBands`, `SetDspChain`, no `RenameProfile`). Atomic, and
    /// immune to index races between frames.
    SetExcluded(Vec<String>),
}

// control::edit_path gains one arm, classified EditPath::Param to match
// SetRules. The match is exhaustive, so omitting it is a compile error.

// -- 3. engine::routing -- provenance on the existing read model ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedSession {
    pub info: SessionInfo,
    pub kind: MatchKind,
}
// RoutingReader's routes output carries Vec<(GroupId, Vec<RoutedSession>)>
// instead of Vec<(GroupId, Vec<SessionInfo>)>. No new channel.

// -- 4. app::ui -- drag resolution ----------------------------------------

/// Gains `excluded`. Unassign (`target: None`) now emits exactly one
/// `SetExcluded`; assign additionally clears the entry. Never edits
/// `match_rules` on the unassign path (L3 flow A, capability 5). An empty
/// `session_file_name` yields an empty batch (B10).
fn resolve_drag_assign(
    session_file_name: &str,
    target: Option<&str>,
    groups: &[GroupConfig],
    excluded: &[String],
) -> Vec<ConfigEdit>;

/// L3 flow D -- prevent the gesture rather than fail it.
fn is_draggable(session: &SessionInfo) -> bool;

// -- 5. app::ui -- pool semantics -----------------------------------------

// EmptyReason is UNCHANGED -- four variants still cover it. Only the
// AllRouted text changes, plus the zone's own label.
fn empty_message(reason: EmptyReason, query: &str) -> String;

// -- 7. app::ui -- Master settings page -----------------------------------

enum Screen {
    Mixer,
    GroupSettings(String),
    /// Master always exists, so unlike `GroupSettings` this variant can never
    /// go stale and needs no `screen_is_stale` fallback.
    MasterSettings,
}

/// Mirrors `group_settings_page`: full-width, `Back` to the mixer. Holds the
/// exclusion list and nothing else.
fn master_settings_page(&mut self, ui: &mut egui::Ui, excluded: &[String]);
```

### Test contracts

| Test | Flow / bug |
|---|---|
| `an_excluded_session_matches_no_group_even_with_a_catch_all` | E / B9 core |
| `an_empty_exclusion_list_matches_exactly_as_before` | H / cap 5 |
| `an_exact_name_rule_of_empty_string_never_matches` | D / B10 |
| `exclusion_is_case_insensitive_on_the_file_name` | E |
| `match_session_reports_name_pattern_and_catch_all_kinds` | G / cap 4 |
| `glob_pattern_is_catch_all_only_for_a_bare_star` | G |
| `unassign_emits_one_exclusion_edit_and_no_rule_edits` | A / cap 5 |
| **`unassigning_a_glob_matched_session_is_no_longer_a_no_op`** | A / B9 — **replaces** `resolve_drag_assign_never_touches_glob_rules` |
| `assign_from_master_clears_the_exclusion_and_adds_the_exact_name` | B |
| `assign_then_unassign_then_assign_leaves_match_rules_unchanged` | A+B round trip / cap 5 |
| `a_drag_with_an_empty_file_name_produces_no_edits` | D / B10 |
| `a_session_with_no_readable_path_is_not_draggable` | D |
| `an_all_routed_pool_teaches_the_unassign_gesture` | F / B18 |
| `set_excluded_is_classified_as_a_param_edit` | edit_path |
| `master_settings_returns_to_the_mixer` | I |
| `removing_one_exclusion_emits_the_whole_remaining_list` | I / CRUD-collapse |
| `profiles_neither_capture_nor_apply_exclusions` | J |

`resolve_drag_assign_never_touches_glob_rules` is **deleted, not fixed** — its
comment documents the behaviour this blueprint reverses (decision 2).
Replacing it with the inverted assertion is what stops a future reader
mistaking the change for a regression.

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-25 | **Scope is B9 + B10 + B18 as one cluster.** | All three are the assignment UI misrepresenting the engine, and all three touch `resolve_drag_assign` / the chip zones / `match_session`. Splitting them means merging against yourself; B9 and B18 additionally share a single root cause (no way to express "do not route X"). |
| 2 | 2026-07-25 | **This blueprint revises logged decisions in two prior blueprints** — `mixer-ui-redesign`'s "glob rules are never touched / unassign is a documented v1 limitation" and the framing of `process-loopback-capture`'s `*` catch-all as sufficient. | Both were correct in isolation and are jointly broken. Recording this explicitly so the existing passing test `resolve_drag_assign_never_touches_glob_rules` is understood as a decision being *revised*, not a regression being introduced. |
| 3 | 2026-07-25 | **The missing primitive is a global exclusion tier checked ahead of all match rules**, not a change to glob handling. | Drag-*assign* across a glob already works (exact beats glob as a class), so glob handling is not the defect. What is missing is any way to express "do not route X". Global rather than per-group because Master's footer means "not routed by Splitstream", not "not in group X" — a per-group exclusion lets the session fall through to another group's glob and reappear there, reproducing the same silent surprise elsewhere. Rejected: converting a glob into explicit exact rules minus the dragged app (destructive, rewrites hand-written rules, violates capability 5); a dedicated `is_catch_all` field (already rejected by `process-loopback-capture`, reasoning unchanged). |
| 4 | 2026-07-25 | **Exclusion entries are plain exact file names (`Vec<String>`, case-insensitive), not parsed through `MatchRule::parse`.** | No capability needs glob exclusions, and the drag gesture only ever writes a single exact name. Rejected: reusing `MatchRule::parse` — nearly free in code and symmetric with `match_rules` in the config file, but adds untested behaviour nobody asked for and lets a hand-written `excluded = ["*"]` silently disable all routing. Cheap to add later precisely because (a) stores strings. |
| 5 | 2026-07-25 | **The exclusion list gets a dedicated Master settings page, reached by a gear on Master — reversing `responsive-ui-refinement`'s "Master has no gear" decision.** Level 2 gains component 7. | User direction, resolving the L3 judgment call about offline exclusions. The reversed decision's stated reasoning was "nothing left to hide behind one once mute moves here" — that premise is exactly what this feature changes, so this is a reversal on changed grounds, not a contradiction. Mirrors the established `Screen::GroupSettings(name)` pattern (gear -> full-width page + Back button, never an inline expand), so it introduces navigation shape that already exists. Rejected: (i) accept the gap — the pool alone is a complete round-trip surface for *running* apps, but an excluded app that is not currently playing has no chip, making its exclusion invisible and unremovable except by hand-editing `splitstream.toml`. |

| 6 | 2026-07-25 | **Unassign never edits `match_rules` — it only adds an exclusion.** | An assign->unassign->assign round trip becomes zero rule rewrites instead of three, and hand-written rules are never touched (capability 5). Same "diff against live state, emit only real differences" shape the operational learnings record three times. Accepted cost: a group's rules text can list an app whose chip sits in Master — visible and true, and explained by capability 4's provenance marker. Rejected: mirroring today's behaviour (strip the `ExactName` as well), which churns rules and brushes against capability 5. |
| 7 | 2026-07-25 | **`match_session`'s return widens to `Option<Match>` carrying `MatchKind`,** and `RoutingReader`'s routes output carries `RoutedSession` rather than `SessionInfo`. | Provenance is computed where the decision is actually made, rather than the UI re-deriving catch-all-ness from raw rule strings — the "prefer the authoritative source over a lookalike reconstruction" rule (level-meters `output_names`, per-group-mute-solo's `Epoch`). Rides the existing read model; no new channel (level-meters precedent). Cleanly reversible: cutting capability 4 restores `Option<GroupId>` and deletes three types. |
| 8 | 2026-07-25 | **The existing test `resolve_drag_assign_never_touches_glob_rules` is deleted and replaced by its inverse**, not amended. | Its body comment documents the reversed behaviour as intentional. Leaving a weakened version would let a future reader read the change as a regression; an explicitly inverted test named `unassigning_a_glob_matched_session_is_no_longer_a_no_op` records the reversal at the place someone would look. |
| 9 | 2026-07-25 | **Design approved at Level 4. Status set to `approved` — ready for implementation.** | All four level sections persisted, including the Level 2 revision made at Level 3 (component 7). No open questions remain. |

## Open Questions

*(none — the offline-exclusion visibility gap raised at Level 3 was resolved by
decision 5, which added the Master settings page.)*

## Constraints

- **Matching precedence is unchanged.** Exact beats glob as a class;
  config-order breaks ties within a tier (`session-routing.md`, 2026-07-18).
- **No hand-written `match_rules` may be rewritten** by any gesture in this
  design (capability 5).
- **A config with no exclusions behaves byte-identically to today** — the
  feature is inert until used.
- **Self-exclusion stays central and unconditional** — no user-facing
  mechanism may make Splitstream's own pid matchable.

## Design Summary

**Drift check: skipped — `requirement_doc` is null.** Origin is the
2026-07-25 end-to-end audit plus a live user report, not a requirement spec,
so there are no Scenarios/ACs or `## Technical Constraints` to compare Level 4
against. Nothing was written back to any requirement doc.

**Components and layer assignments**

| Component | Layer | Change |
|---|---|---|
| `match_session`, `MatchKind`/`Match`, `GlobPattern::is_catch_all` | `engine::rules` — domain (pure) | exclusion tier ahead of all rule tiers; empty-`ExactName` guard; provenance |
| `AppConfig.excluded`, `ConfigEdit::SetExcluded`, store writer, `edit_path` arm | `engine::graph` (config type) + `control` (edit + writer) | new field, new whole-list edit, classified `EditPath::Param` |
| `RoutedSession` on the routes read model | `engine::routing` — orchestration | provenance rides the existing output; no new channel |
| `resolve_drag_assign`, `is_draggable` | `app::ui` | unassign emits one exclusion edit and no rule edits; assign clears it; empty file name yields nothing and cannot be dragged |
| `empty_message`, Master footer label | `app::ui` | the zone names what it holds and what dropping there does |
| Chip provenance marker | `app::ui` | capability 4's visible half |
| `Screen::MasterSettings`, `master_settings_page` | `app::ui` | exclusion list as a viewable/removable surface |

**Key contracts** — the whole feature is one new precedence tier. `excluded`
is checked ahead of exact and glob rules, which is what finally lets the UI
express "do not route X"; everything else follows from that. `SetExcluded` is
a whole-list replace, `match_session` reports *how* it matched so the UI never
re-derives it, and an `ExactName("")` can no longer match anything.

**Architectural constraints honoured**

- Matching *precedence* between exact and glob is untouched
  (`session-routing.md`, 2026-07-18).
- No hand-written `match_rules` is ever rewritten by a gesture (capability 5,
  decision 6).
- A config without exclusions behaves byte-identically to today; the TOML key
  appears only on first use.
- Self-pid exclusion stays central, unconditional, and separate — a runtime
  fact in `compute_desired`, not config vocabulary in `match_session`.
- `AppConfig` stays in `engine::graph` — persistence, not subject matter,
  decides type ownership.
- UI never calls `win-audio`; all mutations still funnel through
  `ShellAction`, exclusion edits on the existing param fast path.

**Domain model** — `engine::rules` is the domain module. The exclusion list
extends the matching *vocabulary*; it is not a new entity, aggregate, or value
object. `MatchRule` remains the only value object in play; `MatchKind`/`Match`
are return-shape types, not domain concepts.

**Decisions revised in other blueprints** — `mixer-ui-redesign`'s "glob rules
are never touched / unassign is a documented v1 limitation" (decision 2) and
`responsive-ui-refinement`'s "Master has no gear" (decision 5). Both are
reversed on changed premises, not contradicted: the first because
`process-loopback-capture` subsequently made `*` the recommended default, the
second because its stated reasoning was "nothing left to hide behind one" and
this feature creates something.

**Open questions resolved during design** — whether the fix belongs in glob
handling (no: drag-assign across a glob already works, the gap is the missing
"do not route" primitive); per-group vs global exclusion (global — Master
means unrouted, not "not in group X"); how expressive an exclusion entry is
(exact names only, decision 4); whether unassign should also strip rules (no,
decision 6); offline-exclusion visibility (decision 5's Master settings page).

**Known accepted gaps**

- A group's `match_rules` can list an app that is currently excluded — visible
  and true, explained by the provenance marker, but a user reading only the
  rules text will be surprised.
- Components 3 + 6 (provenance) remain the clean cut line if scope needs
  trimming; they drop together without touching anything else.

## Key Files

| Path | Purpose |
|------|---------|
| `crates/engine/src/rules.rs` | exclusion tier, empty-name guard, `MatchKind`/`Match`, `GlobPattern::is_catch_all` (component 1) |
| `crates/engine/src/graph.rs` | `AppConfig.excluded` (component 2) |
| `crates/engine/src/routing.rs` | `compute_desired` passes exclusions; `RoutedSession` on the read model (components 1, 3) |
| `crates/control/src/` | `ConfigEdit::SetExcluded`, store writer, `edit_path` arm (component 2) |
| `crates/app/src/ui.rs` | `resolve_drag_assign`, `is_draggable`, pool label/messages, chip marker, `Screen::MasterSettings` + `master_settings_page` (components 4-7) |
