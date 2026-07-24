---
feature: session-search-and-guidance
requirement_doc: null
created: 2026-07-22
status: complete
note: >
  Roadmap Priority 12 (.lattice/ux-gap-roadmap.md). Search/filter across the
  session chip zones, meaningful empty states, and a right-click assign menu
  so routing is discoverable without knowing the drag gesture. No requirement
  spec — roadmap is the origin.
---

# Session Search & Routing Guidance

> Three problems with one root: the chip pool is undiscoverable. Chips give no
> hint they are draggable, every empty zone says the same meaningless
> `(none)`, and a crowded pool has no way to find one app. This feature adds a
> filter across all chip zones, empty states that say what is actually
> happening, and a right-click assign menu as a non-drag path to the same
> result.

## Grounding (2026-07-22, pre-Level-1)

- **One `(none)` for three different situations.** `session_drop_zone`
  (ui.rs:1021) renders `ui.weak("(none)")` whenever `sessions.is_empty()`,
  which covers the master pool being empty (every app already routed — a
  *success* state), a group having nothing routed (the teaching moment), and
  nothing playing audio at all (nothing to drag anywhere). All three read
  identically today.
- **Two call sites, one shared zone.** Master's unassigned pool (ui.rs:483,
  `unassigned_sessions`) and every group's routed list (ui.rs:534,
  `routed_sessions`) both call `session_drop_zone`, so filtering and empty
  states land in one place.
- **Assignment already funnels through one method.** `handle_drop(pid, target,
  all_sessions, groups)` (ui.rs:~541) resolves a pid to its process file name
  and emits the `ConfigEdit::SetRules` batch from `resolve_drag_assign`, with
  `target: None` meaning unassign. A menu-driven assign can reuse it verbatim —
  same edits, different trigger.
- **`dnd_drag_source` senses drag, not click.** It returns
  `dnd_response | response` where `dnd_response` is
  `self.interact(rect, id, Sense::drag())` (ui.rs:2677) and the body is a plain
  `ui.label` (hover only). Nothing in that union senses a click.
- **`context_menu` requires click sense.** `Popup::context_menu` opens on
  `response.secondary_clicked()` (popup.rs:248). Combined with the point above,
  a `.context_menu()` hung off a chip's response **would never open** — the
  same dead-gesture trap as the fader's `double_clicked()` in db-faders.md, in
  a different widget. The chip needs its own click-sensing interaction.
- **Free-text fields keep draft state in this codebase.** The 2026-07-21
  learning that removed draft state applied specifically to dropdowns; the
  `GroupDraft.match_rules` field survives precisely because typing has
  in-progress state worth protecting. A search box is the same shape.

## Design: Level 1 -- Capabilities

**Approved 2026-07-22.**

1. **Search filters every chip zone at once** — typing narrows the master pool
   *and* every group's routed list, so searching `discord` shows which group it
   landed in. Answers "where did this app get routed?", which the UI cannot
   answer today at all.
2. **Match on both label and exe name** — case-insensitive substring against
   the rendered chip label *and* the process file name, so `chrome` finds a
   chip labelled `Google Chrome` and one labelled `chrome.exe`.
3. **Search is dismissible** — a clear button and Esc reset it. Transient UI
   state; never persisted to config.
4. **Four distinct empty states**, replacing today's single `(none)`:
   - **Nothing playing anywhere** -> "No apps are playing audio."
   - **Master pool empty, sessions exist** -> "All apps are routed." (a success
     state, not a problem)
   - **Group empty** -> "Drag an app here, or right-click an app to assign it."
     (the teaching moment)
   - **Filtered to nothing by search** -> "No apps match *query*." — distinct
     from all of the above, so a filtered zone never falsely claims to be empty.
5. **Right-click assign menu on any chip** — "Assign to > *[group list]*" plus
   "Unassign", mirroring the drop targets exactly.
6. **Menu and drag produce identical edits** — the menu routes through the
   existing `handle_drop`, so both gestures emit the same
   `ConfigEdit::SetRules` batch and cannot diverge.
7. **The menu marks the current assignment** — the group a chip is already in
   is indicated, and "Unassign" appears only for a chip that is actually
   assigned.
8. **Dragging still works unchanged** — the click sense the menu needs is added
   alongside the existing drag sense, not in place of it.

Out of scope (v1): fuzzy or regex matching; search history; keyboard navigation
of results; multi-select assign; assigning by glob from the UI (drag and menu
both manage `ExactName` rules only, matching `resolve_drag_assign` today);
searching anything other than session chips (devices, groups).

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-22 | **Search filters every chip zone, not just the master pool.** | Turns a decluttering tool into a locating tool: "where did this app get routed?" is a real question the UI cannot currently answer, and it is worth more than filtering the one crowded zone. Rejected: master-pool-only (simplest, zero interaction with group empty states, but no help finding an already-routed app). Consequence: a zone filtered to nothing needs its own empty state or it would falsely read as unrouted — capability 4's fourth case. |
| 2 | 2026-07-22 | **A right-click assign menu ships alongside the taught gesture**, rather than teaching drag-and-drop alone. | Makes assignment discoverable by exploration instead of instruction, and gives a non-drag path for trackpad users and anyone who cannot perform a drag. Reuses `handle_drop` verbatim, so the two gestures cannot diverge. Accepted cost: real scope growth beyond the roadmap item, which asked only for empty-state guidance. |
| 3 | 2026-07-22 | **The chip needs its own click-sensing interaction; `.context_menu()` on the `dnd_drag_source` response would be dead on arrival.** | `dnd_drag_source` returns `dnd_response \| response` where the drag interaction is `Sense::drag()` (ui.rs:2677) and the body is a hover-only `ui.label`; `Popup::context_menu` opens on `secondary_clicked()` (popup.rs:248), which needs click sense. **Second dead-gesture trap of this kind in two design sessions** — the fader's `double_clicked()` was the first. |
| 4 | 2026-07-22 | **The search box appears only when at least one session exists.** | With nothing playing, a search box is furniture, and "No apps are playing audio" is the entire message at that moment. |
| 5 | 2026-07-22 | **One search box, not one per zone.** (Its location is decision 6.) | Per-zone boxes multiply state and make "where is X?" require searching in the very place you already cannot find it. |
| 6 | 2026-07-22 | **The search box sits at page level, above the column row**, not inside the master column. | Its scope should match its effect: it filters every column, so placing it inside one would make the widget's position contradict its behaviour. Rejected: inside the master column, directly above the unassigned pool (no page-level layout change, visually closest to the zone it most obviously declutters — but reads as master-scoped). Consequence: `COLUMN_CHROME_HEIGHT` must account for the box, or the responsive `fader_height` calculation will overshoot the available space. |
| 7 | 2026-07-22 | **`session_drop_zone` returns a `ChipAction` enum and both variants funnel into the existing `handle_drop`.** | Capability 6 is a non-divergence guarantee between the drag gesture and the menu; routing both through one method makes it structural rather than a convention two call sites have to keep. |
| 8 | 2026-07-22 | **`empty_reason` takes `zone_had_chips`, not just the post-filter count.** | Without it, a group that was already empty would display "No apps match" whenever a search was active — blaming the filter for an emptiness it did not cause. The distinction only exists because decision 1 made search global. |
| 9 | 2026-07-22 | **Design approved at Level 4. Status set to `approved` — ready for implementation.** | All four level sections persisted; no open questions. |
| 11 | 2026-07-23 | **Found and fixed by `/review`: capability 3's Escape-to-clear was dead code.** `search_box` checked `response.has_focus() && escape_pressed`, but egui's `Memory::begin_frame` clears focus globally on an unclaimed Escape *before* the widget renders that frame (memory/mod.rs:583) — confirmed with a two-frame `egui::Context` repro, not assumed. Fixed to `response.lost_focus()`; added `escape_clears_a_focused_search_box`, verified discriminating against the bug (fails on `has_focus()`, passes on `lost_focus()`). Also removed a stale doc comment (the old `session_drop_zone(sessions) -> Option<u32>` description) left orphaned above `enum AssignChoice`. | Third occurrence of the same root class in three sessions — see the operational learning. The click-button clear path was never affected, only the keyboard Escape path. |
| 10 | 2026-07-23 | **Implementation complete**, layer-by-layer (pure functions -> view types/widgets -> call-site wiring). All 9 planned test contracts present; no widget-level tests added, matching the established precedent (db-faders/level-meters) of testing the pure split, not the egui wiring. `COLUMN_CHROME_HEIGHT` bumped 160.0 -> 190.0 per decision 6's anticipated consequence. Full workspace suite green (314 tests) and `cargo clippy --workspace --all-targets` clean. Single file touched (`crates/app/src/ui.rs`), matching the L2 table's "nothing outside app::ui changes" exactly — no scope creep. | Verified against the real diff (`git diff --stat`) before closing, not assumed from the blueprint. Not visually verified in a live running window — drag/right-click interaction confirmed by reading the pinned egui 0.35.0 source (`context_menu`, `secondary_clicked`, `dnd_drag_source`'s returned rect) and by the existing `fader()` reset overlay precedent using the identical `ui.interact(rect, ..., Sense::click())` idiom, not by launching the binary. |

## Design: Level 2 -- Components

**Approved 2026-07-22.** Nothing outside `app::ui` changes: no engine, no
control, no domain, no new crates, no `SessionInfo` change, no config schema
change. `handle_drop` and `resolve_drag_assign` are reused untouched.

| # | Component | Layer | Change | Justification |
|---|---|---|---|---|
| 1 | `SettingsApp.search: String` | UI | **new** field | Free-text needs draft state — the rule that stripped drafts applied to dropdowns only. |
| 2 | `session_matches(&SessionInfo, query: &str) -> bool` | UI | **new**, pure | Capability 2. One matcher for every zone; testable with no egui frame. |
| 3 | `EmptyReason` + `empty_reason(zone, any_sessions, zone_had_chips, searching) -> Option<EmptyReason>` | UI | **new**, pure | Capability 4's four states as a decision function rather than nested `if`s at the render site. `zone_had_chips` is what distinguishes "search hid everything" from "this zone was always empty" — without it a filtered group would falsely say "drag an app here". |
| 4 | `ChipZoneCtx` | UI | **new** param struct for `session_drop_zone` | The zone now needs query, zone kind, group list, current group and total session count. Extracting at this threshold is the established response here (`GroupColumnCtx`, `MasterColumnCtx`, `CaptureFaultCtx`). |
| 5 | `ChipAction` | UI | **new** enum: `Dropped(u32)` \| `Assign { pid, target: Option<String> }` | The zone now has two possible outcomes. Both collapse onto the existing `handle_drop(pid, target, ..)` at the call site, so the two gestures physically cannot produce different edits (capability 6). |
| 6 | `chip_context_menu(..) -> Option<AssignChoice>` | UI | **new** | Built on the chip's own click-sensing interaction (decision 3), never on the drag-source response. |
| 7 | `search_box(ui, &mut String) -> bool` | UI | **new** small widget fn | Keeps the already-long page/column code from growing another block. |
| 8 | `session_drop_zone` | UI | takes `ChipZoneCtx`, filters inside, renders the chosen empty state, returns `Option<ChipAction>` | Filtering inside the zone means one filter site rather than one per caller. |

**Components rejected:**

- **A search-index or matcher abstraction.** One matcher, substring, two
  fields — a trait here would have exactly one implementation.
- **Per-zone search state** (decision 5).
- **A routing service wrapping `handle_drop`.** It already is that.

**Coordination note.** This feature and `app-icons` both rewrite the chip loop
inside `session_drop_zone` — icons add an image before the label, this adds a
click-sensing interaction and filtering around it. Whichever is implemented
second must merge its blueprint's render-site snippet rather than apply it
literally, and `ChipZoneCtx` must absorb the `&mut IconCache` parameter icons
threads in.

**DDD note:** nothing domain-side is involved. `EmptyReason`, `ChipAction` and
`AssignChoice` are all UI-local view types; `SessionInfo` stays the engine's
matching type and is only read.

## Design: Level 3 -- Interactions

**Approved 2026-07-22.** No engine, control, or domain interaction — no domain
events, no aggregate involvement. The entire feature is UI state plus the
existing `ConfigEdit` path.

**Flow A — typing**

```
search_box(ui, &mut self.search) -> mutates SettingsApp.search
  -> next frame, every zone filters against it (no event, no channel)
Esc / clear button -> self.search.clear()
```

**Flow B — zone render, filter and empty state**

```
session_drop_zone(ui, ctx):
    had_chips = ctx.sessions.len() > 0
    shown = ctx.sessions.filter(|s| ctx.query.is_empty() || session_matches(s, ctx.query))

    if shown.is_empty():
        match empty_reason(ctx.zone, ctx.any_sessions, had_chips, !ctx.query.is_empty()):
            NothingPlaying -> "No apps are playing audio."
            AllRouted      -> "All apps are routed."
            GroupEmpty     -> "Drag an app here, or right-click an app to assign it."
            NoMatches      -> "No apps match \"{query}\"."
    else:
        for session in shown: render chip
```

The `dnd_drop_zone` frame wraps the empty label too, so a zone showing any
empty state — including "No apps match" — remains a valid drop target.

**Flow C — right-click assign**

```
per chip:
    let rect = ui.dnd_drag_source(id, DragSession(pid), body).response.rect;
    let menu = ui.interact(rect, id.with("menu"), Sense::click());   // decision 3
    menu.context_menu(|ui| {
        for g in ctx.groups {
            if g.name == ctx.current_group { show as current, no-op }
            else if ui.button(&g.name).clicked() { choice = Assign(g.name) }
        }
        if ctx.current_group.is_some() && ui.button("Unassign").clicked() { choice = Unassign }
    });
  -> ChipAction::Assign { pid, target }
```

**Flow D — both gestures converge**

```
ChipAction::Dropped(pid)           -> handle_drop(pid, zone_target, ..)
ChipAction::Assign { pid, target } -> handle_drop(pid, target.as_deref(), ..)
  -> resolve_drag_assign -> ConfigEdit::SetRules batch
  -> ShellAction::EditParams -> existing apply_params path -> routing.update_rules
```

One method, one edit shape, two triggers.

**Flow E — assigning while a filter is active.** The chip moves from one zone
to another on the next frame and stays visible, because the query that made it
visible still matches it. No special handling needed; stated because it is the
case where search and routing could plausibly have interfered.

**Flow F — dragging while filtered.** Unchanged. The dragged payload is a pid,
not an index into a filtered list, so filtering cannot mis-target a drop.

## Design: Level 4 -- Contracts

**Approved 2026-07-22.**

### Pure decision + matching (no egui types, unit-testable)

```rust
/// Which chip zone this is -- decides which empty state applies.
#[derive(Clone, Copy, PartialEq)]
enum ZoneKind { Unassigned, Group }

/// Why a chip zone is showing nothing.
#[derive(Clone, Copy, PartialEq, Debug)]
enum EmptyReason { NothingPlaying, AllRouted, GroupEmpty, NoMatches }

/// Pure. Called only when the zone's *displayed* list is empty, so it always
/// has an answer. `zone_had_chips` is pre-filter occupancy -- it is what
/// separates "the search hid everything" from "this zone was always empty".
///
///   !any_sessions                 -> NothingPlaying
///   searching && zone_had_chips   -> NoMatches
///   zone == Unassigned            -> AllRouted
///   otherwise                     -> GroupEmpty
fn empty_reason(
    zone: ZoneKind,
    any_sessions: bool,
    zone_had_chips: bool,
    searching: bool,
) -> EmptyReason;

/// Text for an empty zone. `query` is only read for `NoMatches`.
fn empty_message(reason: EmptyReason, query: &str) -> String;

/// Pure. Case-insensitive substring over the chip label *and* the process file
/// name. An empty query matches everything.
fn session_matches(session: &SessionInfo, query: &str) -> bool;
```

### Zone widget

```rust
/// What a chip's context menu produced.
enum AssignChoice { To(String), Unassign }

/// What a chip zone produced this frame. Both variants funnel into
/// `handle_drop` at the call site (decision 7).
enum ChipAction {
    /// A chip was released on this zone -- target is the zone's own identity.
    Dropped(u32),
    /// A menu choice on a chip -- target is explicit and may be any group.
    Assign { pid: u32, target: Option<String> },
}

#[derive(Clone, Copy)]
struct ChipZoneCtx<'a> {
    sessions: &'a [SessionInfo],
    query: &'a str,
    zone: ZoneKind,
    /// The group this zone belongs to; `None` for the master pool.
    current_group: Option<&'a str>,
    /// Every configured group, for the assign menu.
    groups: &'a [GroupConfig],
    /// Whether any session exists anywhere -- drives `NothingPlaying`.
    any_sessions: bool,
}

fn session_drop_zone(ui: &mut egui::Ui, ctx: &ChipZoneCtx) -> Option<ChipAction>;

/// Search field plus clear button; `true` when the query changed this frame.
/// Esc clears. Rendered only when at least one session exists (decision 4).
fn search_box(ui: &mut egui::Ui, query: &mut String) -> bool;
```

### Call sites

```rust
// once per frame, before the column row -- avoids borrowing self.search
// across the &mut self column calls
let query = self.search.clone();

// page level, above the columns (decision 6)
if !all_sessions.is_empty() { search_box(ui, &mut self.search); }

// master column
let ctx = ChipZoneCtx {
    sessions: &unassigned, query: &query, zone: ZoneKind::Unassigned,
    current_group: None, groups: &snapshot.groups,
    any_sessions: !all_sessions.is_empty(),
};
match session_drop_zone(ui, &ctx) {
    Some(ChipAction::Dropped(pid))           => self.handle_drop(pid, None, all_sessions, &snapshot.groups),
    Some(ChipAction::Assign { pid, target }) => self.handle_drop(pid, target.as_deref(), all_sessions, &snapshot.groups),
    None => {}
}
// group column: zone: ZoneKind::Group, current_group: Some(&name),
//               Dropped -> handle_drop(pid, Some(&name), ..)
```

`COLUMN_CHROME_HEIGHT` grows by the search box's height, or `fader_height`
overshoots the space actually left (decision 6's consequence).

### Test contracts

| Test |
|---|
| `nothing_playing_beats_every_other_empty_reason` |
| `an_empty_master_pool_with_sessions_reads_as_all_routed` |
| `an_empty_group_teaches_the_gesture` |
| `a_zone_filtered_to_nothing_reports_no_matches` |
| `a_group_that_was_already_empty_does_not_blame_the_search` — the `zone_had_chips` distinction, the one case that is easy to get wrong |
| `session_matches_finds_the_display_label` |
| `session_matches_finds_the_exe_file_name` |
| `session_matches_ignores_case` |
| `an_empty_query_matches_every_session` |

## Open Questions

*(none — every judgment call raised during design was resolved and logged)*

## Design Summary

**Drift check: skipped — `requirement_doc` is null.** Origin is
`.lattice/ux-gap-roadmap.md` Priority 12, not a requirement spec, so there are
no Scenarios/ACs or `## Technical Constraints` to compare Level 4 against and
nothing to write back.

**Components and layers** — everything is in `app::ui`:

| Kind | Components |
|---|---|
| pure | `session_matches`, `empty_reason`, `empty_message`, `ZoneKind`, `EmptyReason` |
| widgets | `search_box`, `chip_context_menu`, rewritten `session_drop_zone` |
| state | `SettingsApp.search: String` |
| view types | `ChipZoneCtx`, `ChipAction`, `AssignChoice` |
| engine / control / audio-core / win-audio | **untouched** |

**Key contracts** — `empty_reason(zone, any_sessions, zone_had_chips, searching)`
carries the whole empty-state design in one pure function, and
`session_drop_zone(ui, ctx) -> Option<ChipAction>` funnels both gestures into
the existing `handle_drop`.

**Architectural constraints honored**

- No new crates, no new dependencies, no engine or control change; `SessionInfo`
  is read-only to this feature and keeps its domain meaning.
- Assignment keeps a single implementation (`handle_drop` ->
  `resolve_drag_assign` -> `ConfigEdit::SetRules`); the menu is a second
  trigger, never a second code path.
- Decision logic is pure and separated from rendering, matching the split that
  gave `meter_fraction`/`advance_hold`/`peak_for` real tests.

**Domain model** — untouched. `EmptyReason`, `ChipAction` and `AssignChoice`
are UI-local view types.

**Open questions resolved during design** — search scope (decision 1: global,
which is what forces the fourth empty state); whether teaching the gesture was
enough (decision 2: a right-click menu ships too); box placement (decision 6).

**Trap caught at design time** — `.context_menu()` on a chip would never have
opened, because `dnd_drag_source`'s response senses drag and `context_menu`
needs click (decision 3). Second instance of this exact shape in two design
sessions.

## Key Files

| Path | Role |
|---|---|
| crates/app/src/ui.rs | `ZoneKind`, `EmptyReason`, `empty_reason`, `empty_message`, `session_matches` (pure); `AssignChoice`, `ChipAction`, `ChipZoneCtx`, `search_box`, `chip_context_menu`, rewritten `session_drop_zone`; `SettingsApp.search`; `MasterColumnCtx.query`/`GroupColumnCtx.query`; `COLUMN_CHROME_HEIGHT` bump |
