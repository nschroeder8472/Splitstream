---
feature: graphical-eq
requirement_doc: null
created: 2026-07-22
status: approved
note: >
  Roadmap Priority 6 (.lattice/ux-gap-roadmap.md). Multi-band EQ with a
  draggable response curve and built-in preset curves. No requirement spec —
  roadmap is the origin. The DSP is already multi-band capable; the gap is
  the edit set and the UI.
---

# Graphical Multi-Band EQ

> A real EQ editor: the combined response drawn as a curve, one draggable
> handle per band, add/remove bands, and a few built-in preset curves. The
> biquad cascade behind it already supports every one of these bands — nothing
> in the product can currently create a second one.

## Grounding (2026-07-22, pre-Level-1)

- **The domain is already multi-band.** `DspSpec::Eq { bands: Vec<EqBandSpec> }`
  (dsp.rs:56) and `ParametricEq` builds one `EqBand` per spec, all in series
  (dsp.rs:238). A three-band EQ works today if the TOML is hand-written that
  way — there is simply no way to create one from the product.
- **The missing piece is edits, not DSP.** `ConfigEdit::SetEqBand(name, idx,
  spec)` (store.rs:33) retunes the band at `idx`; there is no `AddEqBand` or
  `RemoveEqBand`. `dsp_controls` (ui.rs) renders `bands.first()` only — three
  sliders for band 0 — and its own doc comment states the gap explicitly.
- **Band count is structural; retuning is not.** Changing the number of bands
  resizes `ParametricEq.bands`, so it must go through the off-RT
  build-and-swap path (`apply_dsp_chain_edits` -> `EngineHandle::apply_dsp_chains`),
  the same route `AddDspStage`/`RemoveDspStage` already take. Retuning an
  existing band stays on the `SetDspParam` fast path and is smoothed
  (`PARAM_TIME_CONSTANT_S`, ~10 ms).
- **Only peaking filters exist.** `EqBand::recompute` calls
  `bq.set_coeffs_peaking(..)` (dsp.rs:227) and `Biquad` has no other
  coefficient constructor. Shelves and high/low-pass would be new coefficient
  math plus a `kind` field on `EqBandSpec`, its serde shape, and the store
  writer.
- **Each band smooths freq/gain/Q independently** (`Smoothed` per parameter,
  dsp.rs:190) and recomputes coefficients once per chunk, not per sample —
  with a logged bug history about advancing the smoother the correct number of
  steps per chunk.
- **Persistence already handles a band list.** `store.rs` writes `bands` as an
  array-of-tables (`bt["freq_hz"] = ..`, store.rs:269) and already rejects the
  hand-written inline-array shape that once panicked `SetEqBand`.
- **A cascade's magnitude response is the sum of its bands' responses in dB** —
  so drawing the combined curve is a pure function over the band list, needing
  no filter state and no audio.

## Design: Level 1 -- Capabilities

**Approved 2026-07-22.**

1. **Add and remove bands** — new edits that resize the band list, routed
   through the existing off-RT chain build-and-swap, never the RT param path.
2. **Graphical response curve** — the cascade's combined magnitude plotted on a
   log frequency axis (20 Hz – 20 kHz) against a dB axis, computed as a pure
   function of the band list.
3. **Draggable band handles** — one handle per band on the curve: horizontal
   drag sets frequency, vertical drag sets gain, scroll over a handle sets Q.
4. **Numeric precision alongside the drag** — the selected band's freq/gain/Q
   shown as editable numbers, because dragging a handle cannot land on 1000 Hz
   exactly. Same reasoning that put a readout on the dB faders.
5. **Built-in preset curves** — a small read-only set (Flat, Bass Boost, Vocal,
   Treble), each applied by replacing the whole band list in one edit.
6. **A bounded band count** — maximum 8, so the cascade's RT cost and the
   curve's clutter both stay bounded; the add control disables at the limit.
7. **Live behaviour matches the two paths** — retuning a band is smoothed and
   glitch-free on the fast path; adding or removing one rebuilds that stage
   off-thread and swaps it in, exactly as adding a DSP stage does today.
8. **Persistence unchanged in shape** — bands round-trip through the existing
   `[[group.dsp.bands]]` array-of-tables. More entries, same schema, no
   `schema_version` bump.
9. **Peaking bells only** — no new coefficient math, no `kind` field, no
   migration for existing band tables.

Out of scope (v1): shelf and high/low-pass filters; user-saved custom presets
(overlaps P5's unbuilt profile design); per-band bypass or solo; a spectrum
analyser behind the curve; linear-phase/FIR EQ; mid-side processing; per-band
gain automation.

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-22 | **Peaking bells only — no new filter types.** | The actual gap is the edit set and the UI, not the DSP; a cascade of bells approximates most consumer EQ shapes. Keeps `EqBandSpec`, its serde shape and the store schema untouched, so there is no migration for existing band tables. Rejected: adding low/high shelf (what most consumer EQs use for tilt, awkward to approximate with bells — but a domain change plus a `kind` field plus migration); adding shelves *and* high/low-pass (the full rival feature set, four new coefficient formulas to derive and test). |
| 2 | 2026-07-22 | **Ship a small built-in preset set** (Flat, Bass Boost, Vocal, Treble), applied by replacing the whole band list. | Directly answers the roadmap's "rivals draw the curve *and* ship presets". Costs one edit path (replace-all-bands) that add/remove needs anyway. Rejected: no presets (leaves half the roadmap item undone); built-ins plus user-saved curves (needs storage, naming and management UI that overlaps P5's unbuilt profile feature). |
| 3 | 2026-07-22 | **Bands are edited by dragging handles on the curve**, not by sliders under a read-only plot. | This is the "graphical EQ" the roadmap names, and how every rival works. Accepted cost: the largest custom-paint and hit-testing surface in this feature. Rejected: read-only curve above the existing sliders (far less UI work and more precise, but it is a visualization bolted onto today's controls rather than the editor asked for). |
| 4 | 2026-07-22 | **Numeric fields accompany the drag** (capability 4). | Dragging cannot land on exact values; the same precision argument that gave the dB faders a readout and typed entry. Without it, "set this band to exactly 1 kHz" becomes impossible. |
| 5 | 2026-07-22 | **Maximum 8 bands.** | Enough for any consumer curve, bounds the biquad cascade's per-sample RT cost, and keeps the curve readable. The add control disables at the limit rather than failing after the fact. |
| 6 | 2026-07-22 | **Q is edited by scrolling over a handle**, plus the numeric field. | Vertical and horizontal drag are already assigned to gain and frequency; a modifier-drag is less discoverable than a scroll, and Q is the least-adjusted of the three. |
| 7 | 2026-07-22 | **Presets apply immediately, with no confirmation.** | A preset click does destroy a hand-tuned curve, but a confirm dialog on every preset makes browsing them miserable, and rebuilding a curve is quick. Flagged as worth revisiting if it turns out to bite in practice. |
| 8 | 2026-07-22 | **One `SetEqBands(group, stage, Vec<EqBandSpec>)` edit instead of separate `AddEqBand`/`RemoveEqBand`/preset edits.** | Add is "current list plus one", remove is "current list minus one", preset is "this list" — all three are the same edit, and the UI already holds the current list from the snapshot. The store rewrites the whole `bands` array regardless, so nothing is gained by granularity; carrying the whole list is also atomic and immune to index races between frames. The existing singular `SetEqBand` stays for fast-path retuning. |
| 9 | 2026-07-22 | **No new `ShellAction` variant — `SetEqBands` rides `EditDspChains`.** | That action already means "the shape of a DSP stage changed, rebuild it off-RT and swap"; a band-count change is exactly that. Only `apply_dsp_chain_edits`' affected-group matcher needs to learn the new variant. |
| 10 | 2026-07-22 | **`eq_response_db` lives in `audio-core`, not in the UI, and its test asserts against the real biquad transfer function.** | If the curve's math is a second implementation of the filter's math, the two drift and the UI silently lies about what the user is hearing. Deriving both from the same place, with a test that pins them together, makes divergence a test failure rather than a bug report. |
| 11 | 2026-07-22 | **One curve editor per `Eq` stage**, not one per group. | Matches how the chain actually works and how `dsp_controls` presents it today; stage order stays meaningful. Rejected: collapsing to a single Eq stage per group (simplest mental model, matches consumer rivals — but a real capability reduction and needs defined behaviour for existing two-stage configs); one editor over the combined response of all stages (most truthful visually, but a dragged handle must resolve which stage owns its band, and adding a band needs an arbitrary rule for which stage receives it). |
| 12 | 2026-07-22 | **`SetEqBands` always rebuilds the stage, even when the band count is unchanged.** | A preset with the same count could in principle retune via the fast path, but detecting that adds a branch on the shape of the edit for a rare, user-initiated action whose rebuild is already off-RT and glitch-free. Uniform behaviour is worth more than the saved rebuild. |
| 13 | 2026-07-22 | **`SetEqBand` gains a stage index: `SetEqBand(group, stage, band, spec)`.** | Today it is `SetEqBand(group, band, spec)` and `store.rs:147` resolves the stage via `find_dsp_stage_mut(group, name, "eq")` — the *first* EQ stage; `edits_to_mixer_commands` does the same with `.position(|s| matches!(s.spec, DspSpec::Eq{..}))`. Under decision 11 (one editor per stage), dragging a handle in the second EQ would silently retune the first. Found by tracing flow B, not at implementation time. |
| 14 | 2026-07-22 | **The real per-group sample rate is exposed to the UI** as `EngineStats.group_rates: Vec<(GroupId, u32)>`, falling back to 48 kHz only when the engine is stopped. | Measured: drawing a 15 kHz Q=1 +6 dB bell at an assumed 48 kHz when the group actually runs at 128 kHz is wrong by **1.34 dB at 10 kHz, 2.07 dB at 18 kHz and 2.97 dB at 20 kHz** — the drawn bell is visibly narrower than what is heard (44.1 kHz vs 128 kHz spans 3.6 dB at 20 kHz). The peak at f0 is always exact, so the error is entirely in the skirts. Below ~5 kHz every rate agrees within 0.1 dB, so the lie is confined to the top two octaves — but there it is half the boost the user asked for, which directly contradicts decision 10's purpose. Plumbing turned out to be nearly free: `RunningGraph.group_formats` already exists (runtime.rs:373) and is already cloned under the lock at two call sites, so this is one field plus one `read_stats` line. Rejected: nominal 48 kHz constant (zero engine change, exact for the common case, but ~3 dB wrong for treble bands elsewhere); drawing the rate-free analog prototype (lands near the 128 kHz curve, so equally wrong for a 44.1 kHz group, and cannot be tested against the real transfer function); clamping the plot to 10 kHz (defensible curve, but an EQ that cannot place a treble band is a real capability loss). |
| 15 | 2026-07-22 | **Design approved at Level 4. Status set to `approved` — ready for implementation.** | All four level sections persisted; no open questions. |

## Design: Level 2 -- Components

**Approved 2026-07-22.** `engine` is untouched — `EngineHandle::apply_dsp_chains`
already does everything needed. `audio-core` gains exactly one public function
and no type changes. No `schema_version` bump.

| # | Component | Layer | Change | Justification |
|---|---|---|---|---|
| 1 | `audio_core::eq_response_db(bands, freq_hz, sample_rate) -> f32` | domain | **new**, pure | Capability 2. Lives beside `set_coeffs_peaking` so the curve and the audio derive from the same math; its test asserts against the actual biquad transfer function, so the drawn curve cannot drift from what the filter does. |
| 2 | `ConfigEdit::SetEqBands(String, usize stage, Vec<EqBandSpec>)` | control | **new** — one edit, not three | Add, remove and preset-apply are all "here is the new band list" (decision 8). |
| 3 | `store.rs` writer for `SetEqBands` | control | **new** arm | Rewrites the `bands` array-of-tables, reusing the existing `find_dsp_stage_mut` plus its inline-shape rejection. |
| 4 | `apply_dsp_chain_edits` affected-group matcher | shell | extended to recognise `SetEqBands` | Band-count changes are structural, so they take the off-RT build-and-swap path `AddDspStage`/`RemoveDspStage` already use. No new `ShellAction` variant (decision 9). |
| 5 | `app::ui::eq_editor` | UI | **new** — curve paint, handle hit-testing, drag/scroll; returns `Option<Vec<EqBandSpec>>` | The feature's main surface; replaces `dsp_controls`' current EQ branch. |
| 6 | `freq_to_x` / `x_to_freq` / `db_to_y` / `y_to_db` | UI | **new**, pure | Log-frequency and dB axis mapping. Pure and testable without a frame — the split that gave `meter_fraction` real tests. |
| 7 | `EqPreset` + `preset_bands(EqPreset) -> Vec<EqBandSpec>` | UI | **new**, pure | Capability 5. Presets are UI-authored curves, not domain concepts. |
| 8 | `SettingsApp.selected_band: Option<(String, usize, usize)>` | UI | **new** field | Which band the numeric fields edit (capability 4). |

**The collapse worth recording:** the obvious decomposition was three edits —
`AddEqBand`, `RemoveEqBand`, `SetEqBands`. Challenging each showed the first two
are special cases of the third, computed UI-side from a band list already in the
snapshot. One variant, one store arm, one matcher entry instead of three of each.

**Components rejected:**

- **A per-band response function in the public API.** Only the combined curve is
  drawn; per-band ghost curves are not in scope.
- **`Decibels`/`Frequency` newtypes.** Same reasoning as db-faders — display
  units living at one boundary.
- **A preset registry type.** A `match` over an enum is the whole thing.

**DDD note:** `EqBandSpec` remains the value object and is unchanged.
`eq_response_db` is a pure domain query over a collection of them — no new
aggregate, entity, or state. `EqPreset` is a UI-local view type.

## Design: Level 3 -- Interactions

**Approved 2026-07-22.** Two contract-level findings came out of tracing these
flows: `SetEqBand` cannot address a stage (decision 13), and the curve needs a
sample rate the UI does not have (decision 14).

**Flow A — draw the curve**

```
rate = stats.group_rates[group]            // decision 14; 48_000 if engine stopped
for x in plot_rect.x_range():
    freq = x_to_freq(x)                    // log 20 Hz .. 20 kHz
    db   = audio_core::eq_response_db(bands, freq, rate)
    points.push(pos2(x, db_to_y(db)))
painter.line(points)  +  one handle per band at (freq_to_x, db_to_y)
```

**Flow B — drag a handle (fast path, every frame)**

```
hit-test nearest handle within radius -> selected_band = (group, stage, idx)
drag -> freq = x_to_freq(pointer.x).clamp(20.0, 20_000.0)
        gain = y_to_db(pointer.y).clamp(-24.0, 24.0)
     -> ConfigEdit::SetEqBand(group, stage, idx, spec)
     -> EditParams -> SetDspParam -> smoothed, no rebuild
```

**Flow C — scroll over a handle** -> Q clamped to `0.1..=10.0`, same fast path.

**Flow D — add a band** -> click empty plot area -> new band at that freq/gain,
Q = 1.0, appended -> `SetEqBands(group, stage, new_list)` -> `EditDspChains` ->
off-RT rebuild and swap. Disabled at 8 bands.

**Flow E — remove a band** -> right-click a handle -> list minus that index ->
`SetEqBands`. The plot area is allocated with `Sense::click_and_drag()` by this
widget, so secondary clicks actually register — applying the dead-gesture lesson
from db-faders and session-search rather than rediscovering it a third time.

**Flow F — apply a preset** -> `SetEqBands(group, stage, preset_bands(p))`.
One edit, one rebuild.

**Flow G — numeric fields** for the selected band -> `SetEqBand` fast path,
identical to flow B.

**Flow H — hot reload / hand-edited TOML** -> unchanged. The curve is derived
from the snapshot every frame, so an externally edited band list simply draws.

## Design: Level 4 -- Contracts

**Approved 2026-07-22.**

### `audio-core` (domain)

```rust
/// Combined magnitude response of a peaking-biquad cascade, in dB, at
/// `freq_hz`. A cascade's magnitudes multiply, so their dB values sum.
/// Mirrors `set_coeffs_peaking`'s math exactly -- the test pins the two
/// together so the drawn curve can never diverge from the audio (decision 10).
pub fn eq_response_db(bands: &[EqBandSpec], freq_hz: f32, sample_rate: u32) -> f32;
```

The pinning test lives in `dsp.rs`'s own `mod tests`, which can reach the
private `Biquad` and evaluate `H(e^{jw})` from the real coefficients.

### `engine`

```rust
pub struct EngineStats {
    // ...
    /// Each group's input sample rate (graphical-eq.md). The EQ curve needs it
    /// to draw the response the filter actually applies -- at 128 kHz a treble
    /// bell is ~3 dB wider than the same band drawn at 48 kHz. Static for the
    /// life of the graph.
    pub group_rates: Vec<(GroupId, u32)>,
}

// read_stats, from the already-present RunningGraph.group_formats:
group_rates: rg.group_formats.iter().map(|(id, f)| (*id, f.sample_rate)).collect(),
```

### `control`

```rust
pub enum ConfigEdit {
    // CHANGED -- gained a stage index (decision 13)
    /// Retune one band of one EQ stage. Fast path, smoothed, no rebuild.
    SetEqBand(String, usize /*stage*/, usize /*band*/, EqBandSpec),

    // NEW (decision 8) -- add, remove and preset-apply all funnel here.
    /// Replace an EQ stage's entire band list. Structural: rebuilds the stage
    /// off-RT and swaps it in.
    SetEqBands(String, usize /*stage*/, Vec<EqBandSpec>),
}
```

`store.rs` gains a `SetEqBands` arm rewriting the `bands` array-of-tables, and
`SetEqBand`'s existing arm resolves the stage by index instead of by
first-match. `apply_dsp_chain_edits`' affected-group matcher learns
`SetEqBands`.

### `app::ui`

```rust
const EQ_MAX_BANDS: usize = 8;
const EQ_MIN_FREQ_HZ: f32 = 20.0;
const EQ_MAX_FREQ_HZ: f32 = 20_000.0;
const EQ_GAIN_LIMIT_DB: f32 = 24.0;
/// Only used while the engine is stopped and `group_rates` is empty.
const EQ_FALLBACK_RATE: u32 = 48_000;

/// Log-frequency and linear-dB axis mapping. Pure, exact inverses.
fn freq_to_x(freq_hz: f32, rect: egui::Rect) -> f32;
fn x_to_freq(x: f32, rect: egui::Rect) -> f32;
fn db_to_y(db: f32, rect: egui::Rect) -> f32;
fn y_to_db(y: f32, rect: egui::Rect) -> f32;

enum EqPreset { Flat, BassBoost, Vocal, Treble }
fn preset_bands(preset: EqPreset) -> Vec<EqBandSpec>;

/// What the editor produced this frame -- the two variants map onto the two
/// engine paths, so the fast/structural split is made at the type level rather
/// than remembered at each call site.
enum EqEdit {
    /// One band retuned -> `SetEqBand` -> `EditParams`.
    Retune { band: usize, spec: EqBandSpec },
    /// Band list replaced -> `SetEqBands` -> `EditDspChains`.
    Replace(Vec<EqBandSpec>),
}

/// Curve, handles, drag/scroll, numeric fields for the selected band.
/// `selected` is this stage's selected band index, read and written in place.
fn eq_editor(
    ui: &mut egui::Ui,
    bands: &[EqBandSpec],
    rate: u32,
    selected: &mut Option<usize>,
    height: f32,
) -> Option<EqEdit>;
```

### Call site (replacing `dsp_controls`' EQ branch, per stage)

```rust
let mut sel = match &self.selected_band {
    Some((g, s, b)) if g == &name && *s == stage_idx => Some(*b),
    _ => None,
};
let edit = eq_editor(ui, bands, rate_for(group_id, &stats), &mut sel, EQ_PLOT_HEIGHT);
self.selected_band = sel.map(|b| (name.clone(), stage_idx, b));

match edit {
    Some(EqEdit::Retune { band, spec }) =>
        self.send(ShellAction::EditParams(vec![ConfigEdit::SetEqBand(name.clone(), stage_idx, band, spec)])),
    Some(EqEdit::Replace(bands)) =>
        self.send(ShellAction::EditDspChains(vec![ConfigEdit::SetEqBands(name.clone(), stage_idx, bands)])),
    None => {}
}
```

### Test contracts

| Layer | Test |
|---|---|
| `audio-core` | `eq_response_matches_the_biquad_transfer_function` — evaluates `H(e^{jw})` from the real coefficients and compares; **the test decision 10 exists for** |
| `audio-core` | `response_at_the_center_frequency_equals_the_band_gain` (exact at every sample rate) |
| `audio-core` | `a_cascade_sums_its_bands_in_db` |
| `audio-core` | `an_empty_band_list_is_flat` |
| `audio-core` | `the_same_band_is_wider_at_a_higher_sample_rate` — pins the behaviour decision 14 is about |
| `control` | `set_eq_bands_round_trips_through_toml` |
| `control` | `set_eq_band_targets_the_named_stage_not_the_first` — regression for decision 13 |
| `control` | `set_eq_bands_is_a_chain_edit_not_a_param` |
| `app` | `freq_to_x_round_trips_through_x_to_freq` |
| `app` | `db_to_y_round_trips_through_y_to_db` |
| `app` | `the_axis_midpoint_is_the_geometric_mean_frequency` (log axis) |
| `app` | `no_preset_exceeds_the_band_limit` |

## Open Questions

*(none — every judgment call raised during design was resolved and logged)*

## Design Summary

**Drift check: skipped — `requirement_doc` is null.** Origin is
`.lattice/ux-gap-roadmap.md` Priority 6, not a requirement spec, so there are no
Scenarios/ACs or `## Technical Constraints` to compare Level 4 against and
nothing to write back.

**Components and layers**

| Layer | Components |
|---|---|
| domain (`audio-core`) | `eq_response_db` — one new public pure function; `EqBandSpec`, `ParametricEq` and the biquad math all unchanged |
| orchestration (`engine`) | `EngineStats.group_rates`, populated from the `RunningGraph.group_formats` that already existed |
| control | `ConfigEdit::SetEqBands` (new), `ConfigEdit::SetEqBand` (gained a stage index), one store arm, one changed store arm |
| shell | `apply_dsp_chain_edits` recognises `SetEqBands`; no new `ShellAction` |
| UI (`app::ui`) | `eq_editor`, axis mapping fns, `EqPreset`/`preset_bands`, `EqEdit`, `SettingsApp.selected_band` |

**Key contracts** — `eq_response_db(bands, freq, rate)` is the whole curve;
`EqEdit::{Retune, Replace}` encodes the fast-path/structural split at the type
level so a call site cannot route a band-count change down the RT param path.

**Architectural constraints honored**

- The RT rule that decides the path: param changes go through the lock-free
  command queue, structural changes through supervisor rebuild. `EqEdit`'s two
  variants are exactly that rule made into a type.
- The curve derives from the same math as the audio and is tested against it,
  so the display cannot drift from the filter.
- No new crates, no new traits, no `schema_version` bump; existing configs and
  hand-written band lists keep working.

**Domain model** — `EqBandSpec` remains the value object, unchanged.
`eq_response_db` is a pure query over a collection of them. `EqPreset` and
`EqEdit` are UI-local view types.

**Open questions resolved during design** — how the curve gets a sample rate
(decision 14, after measuring the error); how multiple `Eq` stages are presented
(decision 11); whether add/remove/preset need separate edits (decision 8: they
do not).

**Traps caught at design time** — `SetEqBand` silently targeting the first EQ
stage regardless of which editor sent it (decision 13), and a ~3 dB curve error
on high-rate devices from assuming 48 kHz (decision 14).
