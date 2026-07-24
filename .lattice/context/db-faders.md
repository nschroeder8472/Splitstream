---
feature: db-faders
requirement_doc: null
created: 2026-07-22
status: complete
note: >
  Roadmap Priority 4 (.lattice/ux-gap-roadmap.md). dB-scaled fader travel,
  dB readout + numeric entry, unity reset. No requirement spec — roadmap is
  the origin. Presentation-layer feature: the domain `Gain` and the TOML
  schema are deliberately untouched.
---

# dB Faders

> Both faders (master and per-group) become real mixer faders: travel linear in
> **dB** rather than in amplitude, a readout that says `-6.0 dB` instead of
> `0.50`, typed dB entry, boost to +6 dB, a true-silence floor, and
> double-click-to-unity. The stored value stays linear amplitude — dB is a
> presentation mapping, so there is no schema change and no migration.

## Grounding (2026-07-22, pre-Level-1)

Real code facts, verified this session — several correct the roadmap's own
description of the gap.

- **The faders already have a number, and it is already editable.**
  `egui::Slider::show_value` defaults to `true` (slider.rs:158) and neither
  call site disables it, so both faders already render a `DragValue` box, and
  `add_contents` runs `value_ui` regardless of orientation (slider.rs:993). The
  roadmap's "no number, no numeric entry" is wrong: the box exists and accepts
  typed input. What's wrong is *what it says* — raw linear `0.50`.
- **egui 0.35 can retarget that box without a new widget.**
  `Slider::custom_formatter` (slider.rs:429) and `custom_parser` (slider.rs:473)
  cover the display and the typed-entry directions respectively. Verified
  against the pinned registry source, per the local-registry rule in
  operational learnings, not from memory.
- **A slider senses drag only.** `allocate_slider_space` calls
  `ui.allocate_response(desired_size, Sense::drag())` (slider.rs:655), so
  `Response::double_clicked()` can never fire on it. The reset gesture needs a
  separate click-sensing interaction over the same rect — a Level 2 concern,
  found before design rather than during implementation.
- **`Gain` already permits boost.** `Gain(f32)` is linear, finite and `>= 0`
  with **no upper bound** (sample.rs:125-144). Only the UI's hardcoded
  `0.0..=1.0` slider range forbids amplification today; the domain needs no
  change to allow +6 dB.
- **Two fader call sites, structurally identical.** Master at ui.rs:452
  (`snapshot.master`, `ConfigEdit::SetMaster`) and group at ui.rs:520
  (`group.gain`, `ConfigEdit::SetGroupGain`). Both do
  `Slider::new(&mut v, 0.0..=1.0).vertical()` then `Gain::new(v)` behind an
  `if let Ok`, silently dropping the error case.
- **Persistence is a plain linear float.** `gain = 1.0` in TOML, written by
  `store.rs:119`/`:318` as `gain.value() as f64`, read back through
  `Gain::new(g.gain)` at config.rs:252. `master` is the same shape at the top
  level.
- **The meter floor is already −60 dBFS** (`METER_FLOOR_DB`, ui.rs), so a
  −60 dB fader floor makes fader and meter share one scale.
- **dB↔linear math is already in the codebase, unevenly.**
  `audio_core::dsp::db_to_linear` exists but is `pub(crate)` (dsp.rs:61), and
  the reverse direction is hand-rolled at four separate sites with three
  different zero-guards (`mixer.rs:180`, `mixer.rs:227` — both
  `max(1e-6).log10()`; `ui.rs:802`, `ui.rs:826` — both `<= 1e-6` branches).
  The fader would be the fifth.

## Design: Level 1 -- Capabilities

**Approved 2026-07-22.**

1. **dB-scaled fader travel** — both faders' position maps linearly to dB
   across **−60 … +6 dB**, replacing today's linear-amplitude `0.0..=1.0`.
   Even dB-per-millimetre, the standard console feel; the squashed bottom of
   the current fader disappears.
2. **True-silence floor** — the very bottom of travel is `Gain::SILENT`
   (linear `0.0`), reading `-inf dB`. Everything above maps across −60…+6.
   Pulling a fader down means *off*, not "quiet".
3. **Boost above unity** — the top 6 dB amplifies. No domain change (`Gain` is
   already unbounded above); made safe-to-*see* rather than safe-by-prohibition,
   since the always-on output headroom limiter catches overs and the
   level-meters clip indicators show them.
4. **dB readout** — the slider's existing value box reads `-6.0 dB`, `0.0 dB`,
   `+3.5 dB`, `-inf dB` via `custom_formatter`. One decimal place.
5. **Typed dB entry** — the same box parses typed dB back to linear via
   `custom_parser`, accepting `-6`, `-6.0`, `+3`, `0`, and `-inf`/`-∞`.
   Out-of-range input clamps to the fader range rather than erroring.
6. **Unity reset** — double-clicking a fader returns it to exactly `0.0 dB`
   (`Gain::UNITY`). Needs its own click-sensing interaction, because the
   slider senses drag only.
7. **Controlled drag precision** — drag and arrow-key steps tuned in dB, so a
   slow drag can land on a specific value. Today's amplitude-space step makes
   precision vary wildly along the travel.
8. **Persistence unchanged** — TOML keeps storing linear amplitude, `Gain`
   keeps its meaning, `schema_version` is untouched, and every existing config
   file and hand-edit keeps working. dB exists only between the pixel and the
   `Gain`.
9. **One shared conversion** — a single dB↔linear mapping used by both faders
   in both directions, so master and group cannot drift apart in scale, floor,
   or rounding.

Out of scope (v1): per-fader custom ranges; fader curves/automation;
gain-staging presets; changing the meter widget (already dBFS); a dB readout
anywhere outside the two faders; localised decimal separators; consolidating
the four pre-existing hand-rolled linear→dB sites listed in Grounding.

## Design: Level 2 -- Components

**Approved 2026-07-22.** Six components, four of them pure functions. No
engine, no control, no `Gain`, no TOML, no `schema_version`, no new crate edges
(`app` already depends on `audio-core`).

| # | Component | Layer | Change | Justification |
|---|---|---|---|---|
| 1 | `audio_core::dsp::db_to_linear` | domain | `pub(crate)` -> `pub`, re-exported from `lib.rs` | Already exists (dsp.rs:61) and is exactly the math the fader needs. |
| 2 | `audio_core::dsp::linear_to_db` | domain | **new** — exact inverse, `NEG_INFINITY` at zero | The reverse direction is hand-rolled at four sites with three different zero-guards today; the fader would have been a fifth. |
| 3 | `app::ui` fader mapping | UI | `FADER_MIN_DB = -60.0`, `FADER_MAX_DB = 6.0`; `fader_db_to_gain(f32) -> Gain`, `gain_to_fader_db(Gain) -> f32` | The taper, the range and the −inf floor are **UI policy**, not domain rules. Pure, so testable without an egui frame. |
| 4 | `app::ui` value-box text | UI | `format_fader_db(f64) -> String`, `parse_fader_db(&str) -> Option<f64>` | Fed straight to `custom_formatter`/`custom_parser`. Split out from #5 precisely so they are testable — the same split that gave `meter_fraction`/`advance_hold` real tests. |
| 5 | `app::ui::fader` | UI | **new shared widget fn**: slider + value box + reset overlay, returns `Option<Gain>` (`Some` only on change or reset) | Capability 9 — this is what makes "master and group can't drift" structural instead of a promise. |
| 6 | `master_column` / `group_column` | UI | call `fader(...)` instead of building a `Slider` inline | Removes the existing duplication rather than doubling it. |

**Components rejected:**

- **A `Decibels` newtype.** One consumer, one boundary, and it would be
  unwrapped immediately for `custom_formatter`'s `f64`. `Gain` is the value
  object; dB is a display unit living for the width of one function.
- **A `FaderRange`/`Taper` abstraction.** One taper, one range, no second
  implementation in sight.
- **Touching the four pre-existing hand-rolled linear→dB sites.** This feature
  adds the shared function; migrating the others is separate cleanup.

**DDD note:** `Gain` stays the value object and the `Mixer` aggregate never
learns about dB. `fader_db_to_gain` is a boundary constructor whose input is
clamped to −60…+6, so the resulting linear value is provably finite and
non-negative and `Gain::new` cannot fail — worth documenting at the call site
rather than silently swallowing an `Err`, which is what both current fader
sites do today.

## Design: Level 3 -- Interactions

**Approved 2026-07-22.** Everything below lives between the pixel and the
existing `ConfigEdit` — no domain events, no aggregate changes, no new path
through engine or control.

**Flow A — display, every frame**

```
snapshot.master / group.gain  (linear Gain)
  -> gain_to_fader_db(gain)    -> f32 dB   (0.0 linear -> NEG_INFINITY)
  -> Slider value (dB units, range -60.0..=6.0)
  -> custom_formatter -> format_fader_db -> "-6.0 dB" / "0.0 dB" / "-inf dB"
```

**Flow B — drag**

```
drag -> slider mutates local db: f32 -> response.changed()
  -> fader_db_to_gain(db) -> Gain          (db <= -60 -> Gain::SILENT)
  -> fader() returns Some(gain)
  -> caller sends ConfigEdit::SetGroupGain(name, g) | SetMaster(g)
  -> existing apply_params fast path: MixerCommand -> mixer, store.apply -> TOML
```

Unchanged from today, including that a drag emits one edit per frame — the
debounced comment-preserving write already absorbs that.

**Flow C — typed entry**

```
click value box -> type "-6" -> Enter
  -> custom_parser -> parse_fader_db("-6") -> Some(-6.0)
  -> slider value = -6.0 -> changed() -> same as flow B from here
```

`parse_fader_db` accepts an optional sign, an optional `dB` suffix, and
`-inf`/`-∞`; returns `None` on anything else, which egui treats as "keep the
old value".

**Flow D — double-click reset**

```
fader() allocates the slider, then:
  ui.interact(slider_rect, id.with("reset"), Sense::click())
  -> response.double_clicked() -> return Some(Gain::UNITY)
```

**Flow E — the silence boundary**

```
db <= FADER_MIN_DB  ->  Gain::SILENT (linear 0.0)   [down-conversion]
gain.value() == 0.0 ->  NEG_INFINITY -> clamped to FADER_MIN_DB for the
                        slider position, but formatted as "-inf dB"
```

The last increment of travel snaps to off and the readout jumps −60.0 → −inf.
Standard DAW behaviour.

**Flow F — a hand-written out-of-range gain** (decision 10). `gain = 4.0` in
TOML is +12 dB, above the fader's +6 max. `SliderClamping` defaults to
`Always`, and `add_contents` (slider.rs:953) calls `set_value(old_value)` on
entry — which clamps — then `if value != old_value { response.mark_changed(); }`
fires. With the default, **merely rendering the window would report a change**
and the `changed()` branch would write +6 dB over the user's +12 dB without any
interaction. `.clamping(SliderClamping::Edits)` is required, not optional.

**Flow G — hot reload / external edit** — unchanged. Watcher → snapshot →
`set_current` → next frame reads it through flow A. dB never touches the config
path.

## Design: Level 4 -- Contracts

**Approved 2026-07-22.** Signatures written against types and egui APIs read
from the pinned sources this session.

### `audio-core` (domain)

```rust
// dsp.rs -- visibility change
/// Linear amplitude factor for a level in decibels: `10^(db/20)`.
pub fn db_to_linear(db: f32) -> f32;

/// Level in decibels for a linear amplitude factor -- the exact inverse of
/// [`db_to_linear`]. Zero (and any non-positive input) is `NEG_INFINITY`:
/// true silence has no finite dB value. Callers needing a finite floor clamp
/// on their own side.
pub fn linear_to_db(linear: f32) -> f32;

// lib.rs
pub use dsp::{db_to_linear, linear_to_db, DspChain, /* ... */};
```

### `app::ui` -- pure mapping (no egui types, fully unit-testable)

```rust
/// Bottom of fader travel. Matches `METER_FLOOR_DB`, so the fader and the
/// level meter beside it share one scale.
const FADER_MIN_DB: f32 = -60.0;
/// Top of fader travel -- the last 6 dB is boost above unity (decision 2).
const FADER_MAX_DB: f32 = 6.0;

/// Fader position (dB) -> `Gain`. At or below [`FADER_MIN_DB`] the result is
/// `Gain::SILENT`, not −60 dB (decision 3). The input range guarantees a
/// finite non-negative linear value, so `Gain::new` cannot fail here.
fn fader_db_to_gain(db: f32) -> Gain;

/// `Gain` -> fader position (dB). `Gain::SILENT` -> `NEG_INFINITY`, which the
/// slider clamps to the bottom of travel while the readout shows `-inf dB`.
fn gain_to_fader_db(gain: Gain) -> f32;

/// Value-box text for a dB reading -- one decimal, explicit sign for boost,
/// `-inf dB` at silence. Wrapped to match `Slider::custom_formatter`'s
/// `Fn(f64, RangeInclusive<usize>) -> String` (slider.rs:429).
fn format_fader_db(db: f64) -> String;

/// Parses typed dB back to a number. Accepts `-6`, `-6.0`, `+3`, `0`, an
/// optional `dB`/`db` suffix, and `-inf`/`-∞`. `None` on anything else, which
/// egui treats as "keep the previous value" (slider.rs:473).
fn parse_fader_db(text: &str) -> Option<f64>;
```

### `app::ui` -- the shared widget

```rust
/// One fader: dB-scaled vertical slider, editable dB value box, and
/// double-click-to-unity. `Some` only when this frame changed the value.
/// `id_salt` distinguishes the reset overlay per fader (group name, or
/// "master"); `length` is the slider's long axis.
fn fader(ui: &mut egui::Ui, gain: Gain, id_salt: &str, length: f32) -> Option<Gain>;
```

Internally: `Slider::new(&mut db, FADER_MIN_DB..=FADER_MAX_DB).vertical()`,
`.clamping(SliderClamping::Edits)` (decision 10, mandatory),
`.custom_formatter(..)`, `.custom_parser(..)`, `.drag_value_speed(0.1)`, then
`ui.interact(rect, ui.id().with(("fader-reset", id_salt)), Sense::click())` for
the reset gesture.

`step_by` is deliberately left unset (decision 12).

### Call sites

```rust
// master_column
if let Some(g) = fader(ui, snapshot.master, "master", height) {
    self.send(ShellAction::EditParams(vec![ConfigEdit::SetMaster(g)]));
}

// group_column -- inside the existing horizontal layout with the meter
if let Some(g) = fader(ui, group.gain, &name, fader_height) {
    self.send(ShellAction::EditParams(vec![ConfigEdit::SetGroupGain(name.clone(), g)]));
}
```

Both lose their `if let Ok(g) = Gain::new(v)`: the conversion now returns a
`Gain` that cannot be invalid, so there is no error left to silently swallow.

### Test contracts

| Layer | Test |
|---|---|
| `audio-core` | `linear_to_db_inverts_db_to_linear_across_the_fader_range` |
| `audio-core` | `linear_to_db_of_silence_is_negative_infinity` |
| `app` | `unity_gain_is_zero_db` |
| `app` | `the_bottom_of_travel_maps_to_true_silence` |
| `app` | `boost_maps_above_zero_db` (+6 dB ≈ 1.995 linear) |
| `app` | `gain_round_trips_through_the_fader_mapping` |
| `app` | `silence_formats_as_minus_inf` |
| `app` | `format_fader_db_shows_one_decimal_and_signs_boost` |
| `app` | `parse_fader_db_accepts_signed_suffixed_and_infinite_forms` |
| `app` | `parse_fader_db_rejects_garbage` |

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-22 | **Fader travel is linear in dB over −60…+6 dB**, replacing linear-amplitude `0.0..=1.0`. | Even dB-per-mm is the standard console feel; linear-amplitude travel crams −20 dB through silence into the bottom 10% of the fader, which is the actual complaint behind the roadmap item. −60 matches `METER_FLOOR_DB` exactly, so fader and meter share one scale. Rejected: readout-only change keeping linear travel (smallest diff, but leaves the squashed range and makes the number appear to move non-uniformly under a steady drag). |
| 2 | 2026-07-22 | **The top 6 dB is boost (above unity).** | `Gain` is already unbounded above (sample.rs:133), so this needs no domain change; the always-on output headroom limiter plus the level-meters clip indicators make overs visible rather than prohibited. Accepted consequence: unity is no longer "all the way up", and master +6 stacks with group +6 to +12 dB. Rejected: −60…0 range (unity at the top, no way to lift a quiet source). |
| 3 | 2026-07-22 | **The bottom of travel is true silence (`Gain::SILENT`), displayed `-inf dB`.** | Every mixer treats a fader pulled fully down as off. Costs one special case at the bottom of the map/unmap pair. Rejected: a hard −60 dB floor (perfectly invertible math, but a fully-down fader would still pass signal, making per-group mute the only real "off"). |
| 4 | 2026-07-22 | **Master uses the same −60…+6 range as groups.** | Standard on real desks; a capped master would be the only fader in the window with different physics, and the shared-conversion capability (9) exists precisely so the two can't diverge. |
| 5 | 2026-07-22 | **TOML keeps storing linear amplitude — dB is a presentation mapping only.** | Storing dB means a `schema_version` bump, a migration, and two representations of one quantity. `Gain` is linear in the domain and the mixer multiplies linearly, so dB belongs at the human boundary. Every existing config and hand-edit keeps working untouched. |
| 6 | 2026-07-22 | **The reset gesture cannot use the slider's own response.** | `allocate_slider_space` senses `Sense::drag()` only (slider.rs:655), so `Response::double_clicked()` never fires. Reset needs a separate click-sensing interaction over the same rect (`Ui::interact`, ui.rs:906) — found during grounding rather than at implementation time. |
| 7 | 2026-07-22 | **dB↔linear math is promoted into `audio-core`'s public API** (`db_to_linear` made `pub`, `linear_to_db` added beside it) rather than kept private to `ui.rs`. | It is the same math the DSP already applies internally, so the fader's parse direction becomes the exact inverse of what the limiter uses *by construction*, not by two copies happening to agree. Cost: two free functions on audio-core's public surface. Rejected: UI-local privates (zero domain change, but a fifth hand-rolled `20*log10` and only coincidental agreement with the DSP). |
| 8 | 2026-07-22 | **One shared `fader()` widget function, used by both columns**, returning `Option<Gain>`. | Capability 9 is a non-divergence guarantee; two inline sliders can only satisfy it by convention. Also the natural home for the reset overlay so both faders get the gesture automatically. |
| 9 | 2026-07-22 | **No `Decibels` newtype.** | Single consumer, and `custom_formatter`/`custom_parser` deal in `f64` anyway — the type would exist only to be unwrapped. `Gain` remains the only value object on this path. |
| 10 | 2026-07-22 | **`Slider::clamping(SliderClamping::Edits)` is mandatory, not a nicety** (flow F). | egui's default is `Always`, which clamps the *existing* value on entry (slider.rs:953) and then marks the response changed — so a hand-written `gain = 4.0` (+12 dB) would be silently rewritten to +6 dB by the act of opening the window, with no user interaction. `Edits` keeps out-of-range existing values intact while still preventing out-of-range *input*. Caught by reading the pinned egui source during design, not by testing. |
| 11 | 2026-07-22 | **Double-click's two stray rail clicks are accepted, not suppressed.** | egui's slider jumps its handle to a clicked rail position, so a reset emits two spurious position edits before the unity edit lands last and wins. Same class and volume as the per-frame edits an ordinary drag already produces, and the final state is correct. Rejected: reordering the overlay ahead of the slider or gating the slider on double-click state (real complexity for a transient no-op). |
| 12 | 2026-07-22 | **`step_by` is left unset; `drag_value_speed(0.1)` provides the precision capability 7 asked for.** | `step_by`'s own doc (slider.rs:305) warns that a stepped value which is out of range under clamping becomes unchangeable — precisely flow F's hand-written `gain = 4.0` case, which decision 10 deliberately preserves. Dropping the step avoids the interaction; with dB units the default drag feel is already even, and 0.1 dB per point on the value box covers fine adjustment. |
| 13 | 2026-07-22 | **Design approved at Level 4. Status set to `approved` — ready for implementation.** | All four level sections persisted; no open questions. |
| 14 | 2026-07-23 | **Implementation complete, inside-out (audio-core -> app::ui).** `db_to_linear` promoted `pub`, `linear_to_db` added; `FADER_MIN_DB`/`FADER_MAX_DB`, `fader_db_to_gain`, `gain_to_fader_db`, `format_fader_db`, `parse_fader_db`, and the shared `fader()` widget all match the L4 contract exactly. Both call sites converted, `if let Ok(Gain::new(v))` swallow removed. All 10 planned test contracts present. Full workspace suite green (audio-core 97, control 50, engine 96, app 61) and `cargo clippy --workspace --all-targets` clean. Only `crates/audio-core/src/dsp.rs`, `lib.rs`, `crates/app/src/ui.rs` touched — no engine/control edge, matching the L2 table exactly. | Verified against the real diff before closing, not assumed from the blueprint. |
| 15 | 2026-07-23 | **Self-caught deviation, fixed same session: `gain_to_fader_db` must NOT clamp to the fader range.** First draft clamped the dB value before handing it to the slider, which silently defeats decision 10 — `SliderClamping::Edits` only skips re-clamping the *existing* value at entry (verified by reading `add_contents` at slider.rs:950-966: `self.set_value(old_value)` only runs when `clamping == Always`), so a pre-clamped input makes the clamping mode moot and a hand-written `gain = 4.0` (+12 dB) would silently display `+6.0 dB` even without clamping-on-render firing a write. Fixed: `gain_to_fader_db` now returns `linear_to_db(gain.value())` raw (including `NEG_INFINITY` and any out-of-range value); confirmed safe via `normalized_from_value`'s explicit `value <= min`/`value >= max` guards (slider.rs:1116-1119), which handle non-finite input without NaN. | Caught during Step 3 post-gen verification against the L4 contract text itself, not by a failing test — none of the 10 planned test contracts exercise this path since it needs a live egui frame to observe. |
| 16 | 2026-07-23 | **Correction to decision 10's stated mechanism, found during `/review` by empirically reproducing it against the pinned egui 0.35.0, not by re-reading the source.** `response.changed()` does **not** fire from mere render under `SliderClamping::Always`, contrary to decision 10's claim — `Slider::get_value()` (slider.rs:598-605) itself applies the `Always` clamp on *every* read, so the "old_value" baseline read at entry is already clamped and agrees with the post-render read; no divergence, no `mark_changed()`. Confirmed via a direct single-frame repro using `egui::__run_test_ui` with `.clamping(Always)` and an out-of-range starting value: the bound value was silently mutated (12.0 -> 6.0, proving `Always` does corrupt the value in place) but `response.changed()` stayed `false` throughout. The actual risk `SliderClamping::Edits` guards against is therefore **silent value/display corruption on render**, not a "changed()-branch overwrites config" write path — `fader()` never writes anything unless `response.changed()` is true, so that specific write-path danger was never live in this implementation regardless of clamping mode. `.clamping(Edits)` is still correct and required (for display truth), just not for the reason originally logged. Test contract updated accordingly: dropped the (non-discriminating) live-frame `__run_test_ui` test in favor of a pure `gain_to_fader_db_does_not_clamp_an_out_of_range_existing_value` test, which is both simpler and actually fails if the decision-15 bug recurs. | A blueprint's stated mechanism for *why* a fix is needed can be wrong even when the fix itself (the required call) is right — verify library behavior empirically before trusting a design doc's description of it, especially across a library-source reading done at design time vs. implementation time. |

## Open Questions

*(none — every judgment call raised during design was resolved and logged)*

## Design Summary

**Drift check: skipped — `requirement_doc` is null.** Origin is
`.lattice/ux-gap-roadmap.md` Priority 4, not a requirement spec, so there are
no Scenarios/ACs or `## Technical Constraints` to compare Level 4 against and
nothing to write back.

**Components and layers**

| Layer | Components touched |
|---|---|
| domain (`audio-core`) | `dsp::db_to_linear` promoted to `pub`; new `dsp::linear_to_db`; both re-exported from `lib.rs`. No type changes, no `Gain` change. |
| UI (`app::ui`) | `FADER_MIN_DB`/`FADER_MAX_DB`; pure `fader_db_to_gain`/`gain_to_fader_db`/`format_fader_db`/`parse_fader_db`; new shared `fader()` widget; `master_column`/`group_column` converted to call it |
| engine / control | **untouched** |

**Key contracts** — `fader(ui, gain, id_salt, length) -> Option<Gain>` is the
whole user-facing surface; `fader_db_to_gain`/`gain_to_fader_db` are the
inverse pair carrying the taper, the range and the −inf floor; `linear_to_db`
is the domain-side inverse of the existing `db_to_linear`.

**Architectural constraints honored**

- No new crate edges (`app` already depends on `audio-core`), no new traits, no
  new types.
- Presentation stays at the boundary: the domain never learns about dB, and
  `Gain` keeps its single linear meaning.
- Persistence untouched — no `schema_version` bump, no migration, every
  existing config file and hand-edit keeps working (decision 5).
- Pure mapping split out from widget code so the taper is testable without an
  egui frame — same split that gave the level-meters helpers real tests.

**Domain model** — no new aggregate, entity, or value object. `Gain` remains
the only value object on this path; a `Decibels` newtype was explicitly
rejected (decision 9).

**Open questions resolved during design** — where the dB↔linear math lives
(decision 7: promoted into audio-core, so the fader's parse is the DSP's exact
inverse by construction rather than by two copies agreeing).

**Grounding corrections worth carrying forward** — the roadmap's "faders have
no number, no numeric entry" was wrong: `show_value` defaults `true` and the box
is already click-to-edit. The real gaps are the unit, the taper and the reset.

**Two egui behaviours that would have been bugs** if found at implementation
time rather than design time: `Sense::drag()` on the slider makes
`double_clicked()` permanently dead (decision 6), and `SliderClamping::Always`
silently rewrites an out-of-range stored gain on mere render (decision 10).

**A third egui behaviour caught during implementation, not design** (decision
15): `SliderClamping::Edits` only helps if the *unclamped* value reaches the
widget — a defensive clamp one layer up in application code silently
reproduces the exact bug decision 10 exists to prevent, just outside the
widget instead of inside it.

## Key Files

| Path | Role |
|---|---|
| crates/audio-core/src/dsp.rs | `db_to_linear` (now `pub`); new `linear_to_db` |
| crates/audio-core/src/lib.rs | re-exports both |
| crates/app/src/ui.rs | `FADER_MIN_DB`/`FADER_MAX_DB`; `fader_db_to_gain`/`gain_to_fader_db`/`format_fader_db`/`parse_fader_db`; shared `fader()` widget; `master_column`/`group_column` call sites |
