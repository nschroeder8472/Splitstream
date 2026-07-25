---
doc: ux-implementation-order
created: 2026-07-23
author: Nik
status: living
kind: implementation-plan
note: >
  Recommended build order for the eight approved UX-roadmap blueprints in
  `.lattice/context/`. Companion to `.lattice/ux-gap-roadmap.md` (what and why)
  and `.lattice/implementation-order.md` (the original P0-P5 engine phases,
  complete).
---

# UX Roadmap — Implementation Order

Eight blueprints in `.lattice/context/`. This is the order to build them in
and the reasoning behind it. Status as of 2026-07-24: items 1-6
(per-group-mute-solo, db-faders, session-search-and-guidance, app-icons,
graphical-eq, profiles) are `status: complete`; items 7-8
(external-controls, visual-identity) remain `status: approved`, not yet
implemented.

Three forces set the order:

1. **Dependencies** — a blueprint whose contracts another one consumes goes
   first.
2. **Collisions** — two blueprints that rewrite the same function should land
   **adjacently**, so the second is merged while the first is still fresh.
   Spreading a collision pair across weeks is how a blueprint gets applied
   literally over code that has moved.
3. **Blast radius** — the widest-reaching change goes last, so it is tuned
   against real UI rather than imagined UI.

Inside each blueprint, follow the same inside-out discipline the original
implementation order used: pure domain code first, then orchestration, then
platform/COM, then UI.

## Pre-flight

**Commit the level-meters work before starting.** The working tree currently
holds an unreviewed-then-reviewed, tests-green implementation (10 files, 3
warnings fixed) plus nine untracked `.lattice` docs, none of it committed.
Every item below edits `ui.rs`, which that work also touches — starting on top
of an uncommitted change means the first conflict is with yourself.

## The order

### 1. per-group-mute-solo

*Why first:* smallest of the eight, and the most depended-on. `GroupConfig.muted`
is consumed by profiles (a profile captures mute), by external-controls (the
per-group mute hotkey and the tray mute items), and by nothing that has to exist
first. It is also entirely additive — one scalar threaded down the proven
`follow_master` path.

1. `audio-core`: `GroupSpec.mute`, `GroupState.{mute, solo}`,
   `MixerCommand::{SetGroupMute, SetGroupSolo}`, free fn `silenced`,
   `Mixer::solo_active`, the phase-1 `NEG_INFINITY` env publish and the phase-3
   skip
2. `engine`: `graph::GroupConfig.muted` -> `GroupSpec.mute` in `resolve`
3. `control`: `RawGroup.muted`, the diff param arm, `ConfigEdit::SetGroupMute`,
   store writer + `group_table`
4. `app`: `ShellAction::SetSolo`, `Dispatcher::apply_solo`,
   `rebuild_generation`, `UiState.rebuild_generation`,
   `edits_to_mixer_commands` arm
5. `app::ui`: M/S toggles in `group_column`, `soloed` set, `clear_solo_on_rebuild`

*Exit:* muting a group silences only that group and stops it triggering its duck
target; soloing silences every non-soloed group across all outputs; an
explicitly muted group stays silent while soloed; a structural rebuild clears
solo on both sides.

### 2. db-faders

*Why here:* it rewrites the same `group_column` fader region item 1 just touched,
so it lands while that code is still in your head. It also promotes
`db_to_linear` to `pub` and adds `linear_to_db`, which external-controls wants
for its 3 dB stepping.

1. `audio-core`: `db_to_linear` -> `pub`, new `linear_to_db`, both re-exported
2. `app::ui`: `FADER_MIN_DB`/`FADER_MAX_DB`, pure `fader_db_to_gain` /
   `gain_to_fader_db` / `format_fader_db` / `parse_fader_db`
3. `app::ui`: the shared `fader()` widget — slider in dB units,
   `SliderClamping::Edits` (**mandatory**, see that blueprint's decision 10),
   custom formatter/parser, `ui.interact` reset overlay
4. Convert both call sites; delete their `if let Ok(Gain::new(..))` swallowing

*Exit:* both faders read `-6.0 dB` / `0.0 dB` / `-inf dB`, typed entry works,
double-click returns to unity, the bottom of travel is true silence, and a
hand-written `gain = 4.0` is **not** rewritten by opening the window.

### 3. session-search-and-guidance

*Why before app-icons:* this blueprint performs the structural rewrite of
`session_drop_zone` — `ChipZoneCtx`, `ChipAction`, filtering, empty states.
app-icons only adds one element *inside* the resulting chip loop. Reversed, the
icon work gets rewritten by this one.

1. `app::ui` pure: `session_matches`, `ZoneKind`, `EmptyReason`, `empty_reason`,
   `empty_message`
2. `app::ui`: `ChipZoneCtx`, `ChipAction`, `AssignChoice`, rewritten
   `session_drop_zone`, `search_box`, `SettingsApp.search`
3. Chip context menu on its **own** click-sensing `ui.interact` — the
   `dnd_drag_source` response senses drag only and `.context_menu()` on it will
   never open
4. Both call sites map `ChipAction` onto the existing `handle_drop`

*Exit:* searching narrows every zone at once; the four empty states are
distinguishable; an already-empty group does not blame the search; right-click
assign produces byte-identical edits to a drag.

### 4. app-icons

*Why here:* completes the chip pool while item 3 is fresh. Also introduces the
`win-shell` crate, which is a one-off piece of scaffolding better done in
isolation than bundled into a bigger change.

1. New `win-shell` crate: `IconImage`, `extract_icon_rgba` — `DestroyIcon` via
   RAII, negative `biHeight`, BGRA→RGBA, **and the all-zero-alpha legacy-icon
   case** (see that blueprint's decision 12; the `#[ignore]`d real-exe test
   exists to catch it)
2. `app::icons`: `IconCache`, `spawn_icon_worker`, `poll`, `texture`
3. `app::ui`: `chip_icon`, `letter_tile`, `tile_color`, slot inside the chip loop
   from item 3

*Exit:* chips show real exe icons; a cold cache never blocks a frame; a failed
extraction is retried zero times; the tile fallback covers pending, failed and
path-less sessions.

### 5. graphical-eq

*Why here:* it collides with nothing and could be built at any point — but
placing it before profiles means profiles' `edit_path` classifier is written
once against the **complete** edit set rather than gaining an arm afterwards.
This is the item to move if you want to parallelize.

1. `audio-core`: `eq_response_db`, with the test that pins it to the real biquad
   transfer function
2. `engine`: `EngineStats.group_rates` from the existing
   `RunningGraph.group_formats`
3. `control`: `SetEqBand` **gains a stage index** (it silently targets the first
   EQ stage today), new `SetEqBands`, store arms, `apply_dsp_chain_edits` matcher
4. `app::ui`: axis mapping fns, `EqPreset`/`preset_bands`, `EqEdit`, `eq_editor`

*Exit:* bands can be added, dragged, removed and preset-applied; the drawn curve
matches the filter at 44.1, 48 and 96 kHz; retuning uses the param path and
band-count changes rebuild off-RT.

### 6. profiles

*Why here:* it consumes `GroupConfig.muted` from item 1, and it establishes the
shared control-surface infrastructure external-controls plugs into. Lower
technical risk than external-controls (config plus pure functions, no COM), so
it is the better place to introduce the shared shapes.

1. `engine`: `ProfileConfig`, `ProfileGroupConfig`, `ConfigSnapshot.profiles`,
   `AppConfig.active_profile`
2. `control`: `EditPath` + `edit_path`, `SetDspChain`, `SetProfile`,
   `RemoveProfile`, `SetActiveProfile`, store writers
3. `control::profiles`: pure `apply_profile`, `capture_profile`,
   `profile_is_modified` — including `capture_then_apply_is_a_no_op`
4. `app`: `ShellAction::ApplyProfile`, `Dispatcher::apply_profile_action`
   (batch partitioned by `edit_path`, **never a blanket rebuild**)
5. `app`: tray submenu, `spawn_hotkeys` — **adopt external-controls' shapes
   here**, see "Merge points" below
6. `app::ui`: profile bar

*Exit:* a profile round-trips capture → apply as a no-op; a gain-only profile
switch causes no rebuild; switching, saving, reverting and deleting all behave;
`match_rules` are never touched by a switch.

### 7. external-controls

*Why here:* it depends on item 1 (per-group mute), item 2 (dB helpers) and item 6
(hotkey/tray infrastructure), and it carries the most technical risk of the eight
— a new COM interface with a callback, two-way sync, and a hold state machine.
Everything it needs exists by now.

1. `engine::ports`: `EndpointVolumePort`, `VolumeEvent`,
   `AudioSystem::open_default_endpoint_volume` with a default body
2. `win-audio::endpoint_volume`: `IAudioEndpointVolume`,
   `#[implement] IAudioEndpointVolumeCallback`, GUID echo filtering,
   **`Drop`-time `UnregisterControlChangeNotify`** — this codebase has got that
   pair wrong once already
3. `engine::volume_bind`: handle + pure `reconcile` (with `MIRROR_EPSILON`)
4. `app`: pure `step_gain` and `push_to_mute`, the four `ShellAction` variants,
   dispatcher binding arm
5. `app`: hotkey bindings incl. `Released` handling and the max-hold timer; tray
   per-group mute items
6. `control`: `[app] volume_bind`, per-group hotkey fields

*Exit:* the Windows volume keys drive the bound group and the OSD agrees both
ways; mirroring suspends when the bound target's output is the default device;
push-to-mute restores prior state and cannot strand audio muted; every binding
fails inertly.

### 8. visual-identity

*Why last:* it themes every widget, including all the new ones from items 1-7.
Landing it last means the palette and accent are tuned against real UI. Its
capability 12 (widgets keep reading `ui.visuals()`) is what keeps items 1-7 from
needing retrofitting — honour it as you build them.

1. `engine`: `ThemeChoice`, `AccentChoice`, two `AppConfig` fields
2. `control`: `SetTheme`, `SetAccent` (both `EditPath::Param`) + store arms
3. `app::theme`: `Accent`, `accent()`, `Semantic`, `semantic()`, `visuals()`,
   `style()`, `install()`, `brand_icon_rgba()`; `contrast_ratio` test-only
4. Replace the five hardcoded colours; install from the `CreationContext`
   closure; brand mark into `tray.rs`
5. `app::ui`: theme and accent pickers

*Exit:* dark, light and follow-system all work with no flash of default styling;
every accent preset passes the contrast check on both palettes; semantic colours
are unchanged by accent; the tray shows the brand mark, not a blue square.

## Merge points

Two collision pairs and one superseded contract. These are the places where
applying a blueprint literally will produce the wrong result.

| Pair | What collides | How to merge |
|---|---|---|
| 1 ↔ 2 | `group_column`'s fader region | Item 1 adds M/S toggles beside the fader; item 2 replaces the fader itself with `fader()`. Land 1, then write 2's `fader()` against the *current* column layout. |
| 3 ↔ 4 | `session_drop_zone`'s chip loop | Item 3 restructures the whole zone; item 4 adds `chip_icon` inside the resulting `ui.horizontal`. `ChipZoneCtx` must also absorb the `&mut IconCache` parameter. |
| 6 ↔ 7 | `spawn_hotkeys`, `tray.rs` | **`external-controls.md`'s Level 4 supersedes `profiles.md`'s here.** Build item 6 with `spawn_hotkeys(&[HotkeyBinding])` and the `TrayModel` struct from external-controls, adding `HotkeyAction::ApplyProfile` and `TrayModel.profiles`. Do **not** implement profiles' `spawn_hotkeys(map, profiles, actions)` signature — item 7 would immediately have to undo it. |

## Dependency matrix

| Blueprint | Hard dependencies | Soft / merge |
|---|---|---|
| per-group-mute-solo | — | collides with db-faders |
| db-faders | — | collides with per-group-mute-solo |
| session-search | — | collides with app-icons |
| app-icons | — | collides with session-search |
| graphical-eq | — | before profiles, so `edit_path` covers all variants |
| profiles | per-group-mute-solo (`muted` field) | collides with external-controls |
| external-controls | per-group-mute-solo (mute hotkey, tray items), db-faders (dB helpers), profiles (hotkey/tray infra) | — |
| visual-identity | none technically | best after everything it themes |

Only three hard dependencies exist across eight blueprints. That is a
consequence of designing each one against the current codebase rather than
against the others.

## If you reorder

- **Moving graphical-eq anywhere** is free — it is the only blueprint touching
  neither the mixer column, the chip pool, nor the control surfaces. If it lands
  after profiles, add its `SetEqBands` arm to `edit_path` (the exhaustive match
  makes this a compile error, not a silent gap).
- **profiles before per-group-mute-solo** works if you omit `mute` from
  `ProfileGroupConfig` and add it later — the blueprint says so explicitly.
- **external-controls before db-faders** works if external-controls promotes
  `db_to_linear`/`linear_to_db` itself instead.
- **visual-identity earlier** is the one genuinely bad move. A palette tuned
  against widgets that do not exist yet is tuned against imagination, and it will
  need revisiting once they do.

## Not yet designed

Roadmap P7 (mic / input chain) and P8 (streamer dual-mix) have no blueprint.
Both are high-effort and engine-side. Note that P8 interacts with
per-group-mute-solo's semantics — a group audible in one mix and muted in
another means mute stops being one flag per group — so if P8 is coming, item 1's
contract is worth re-reading before P8 is designed.
