---
feature: visual-identity
requirement_doc: null
created: 2026-07-23
status: approved
note: >
  Roadmap Priority 11 (.lattice/ux-gap-roadmap.md). Dark/light/system themes,
  a brand accent with a small preset set, a deliberate style pass, and a real
  brand mark replacing the placeholder tray icon. No requirement spec —
  roadmap is the origin. Touches every widget, including seven blueprints'
  worth of unbuilt ones.
---

# Visual Identity

> Make Splitstream look like a product rather than a dev tool: two designed
> palettes with a system-following preference, one brand accent applied
> consistently, a single style pass for corner radius and spacing, and a brand
> mark to replace the solid blue square currently sitting in the tray.

## Grounding (2026-07-23, pre-Level-1)

- **The UI is already mostly theme-driven.** `paint_meter`,
  `speaker_mute_button` and the rest read `ui.visuals()`
  (`extreme_bg_color`, `strong_text_color()`, `text_color()`), so they follow a
  palette change automatically. This is why the feature is a style pass rather
  than a widget rewrite.
- **Exactly five hardcoded colours exist.** `METER_GREEN`, `METER_AMBER`,
  `METER_RED` (ui.rs:784-786), and the routing-degraded warning colour
  `Color32::from_rgb(220, 80, 40)` **duplicated verbatim** at ui.rs:309 and
  ui.rs:408.
- **egui 0.35 has the whole mechanism.** `ThemePreference::{Dark, Light,
  System}` with `System` as `#[default]` (memory/theme.rs:67);
  `Context::set_theme` (context.rs:2102); `Context::set_visuals_of(theme,
  visuals)` (context.rs:2199) for defining both palettes independently;
  `Context::system_theme()` (context.rs:2084). Style carries
  `corner_radius: CornerRadius` (style.rs:1302) — note the type is
  `CornerRadius`, not the older `Rounding`.
- **There is already an install point.** `eframe::run_native`'s closure
  receives a `CreationContext` and the code already uses it
  (`cc.egui_ctx.clone()`, main.rs:431), so applying a theme before the first
  frame needs no new plumbing.
- **The only brand mark is a placeholder, and it says so.**
  `placeholder_icon()` (tray.rs:158) fills a 16x16 RGBA buffer with a single
  colour, `0x30 0x9c 0xff`. It is the app's most-seen surface — it sits in the
  tray all day.
- **Custom paint is the established idiom for icons here** (`speaker_mute_button`,
  `paint_meter`), adopted after an emoji-range glyph rendered as tofu. A
  code-drawn brand mark follows the same path and needs no asset pipeline.

## Design: Level 1 -- Capabilities

**Approved 2026-07-23.**

1. **Two designed palettes** — a dark and a light `Visuals`, each defined
   deliberately rather than tweaked from egui's defaults, installed via
   `set_visuals_of` so both exist simultaneously and switching is instant.
2. **Three theme modes** — Dark, Light, Follow-system; persisted in config,
   defaulting to Follow-system.
3. **Live OS-theme following** — with Follow-system selected, a Windows
   light/dark change is picked up while running, not only at startup.
4. **A brand accent applied consistently** — selection, focus rings, active
   toggles and the brand mark all draw from one accent value rather than each
   widget choosing.
5. **A small accent preset set** — the brand default plus a handful of
   alternatives, each defining its own dark-mode and light-mode variant,
   because a colour that reads well on dark often does not on light.
6. **Every preset is contrast-verified against both palettes** — a checked
   property with a minimum ratio, not a hope. This is what makes a preset set
   safe rather than a source of unreadable UI.
7. **A single style pass** — corner radius, spacing and stroke widths tuned
   together as one coherent set rather than scattered per-widget constants.
8. **The five hardcoded colours move into the theme** — the three meter colours
   become semantic theme entries, and the duplicated routing-degraded colour
   becomes one definition with one name.
9. **Semantic colours stay semantic** — clip red stays red whatever the accent
   is. An accent must never make a clip indicator look like a normal level.
10. **A programmatic brand mark** replacing `placeholder_icon`, legible at
    16 px, used for both the tray icon and the window icon.
11. **The theme is applied before the first frame** — no flash of default egui
    styling on launch.
12. **Widgets keep reading `ui.visuals()`** — no widget gains a direct
    dependency on the accent except where semantically meaningful, so the seven
    unbuilt blueprints' widgets inherit the theme by construction rather than
    needing retrofitting.

Out of scope (v1): a typography scale and any bundled font asset (deliberately
excluded — the next lever, not this one); user-defined palettes or a free colour
picker; per-widget theme overrides; a dedicated high-contrast/accessibility mode
beyond the contrast-checked presets; animations or transitions; window chrome or
titlebar customisation.

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-23 | **Three theme modes — Dark, Light and Follow-system — with Follow-system as the default.** | Matches egui's own `#[default]` and what Windows apps are expected to do. `set_visuals_of` lets both palettes be defined once and switched by preference, so all three modes cost little more than two. Rejected: Dark+Light only (no runtime OS reaction, but a fresh install ignores a preference the user already expressed); a single designed dark theme (strongest identity and one surface to design, but ignores light-mode users and the roadmap explicitly asked for a toggle). |
| 2 | 2026-07-23 | **A fixed brand accent plus a small preset set**, each preset carrying its own dark and light variant. | Identity by default, personalisation available. Per-theme variants rather than algorithmic lightness-shifting because derived accents come out muddy; two hand-picked constants per preset look right and cost nothing. Rejected: one fixed accent only (simplest, but no personalisation at all); a free colour picker (most flexible and egui ships the widget, but an arbitrary accent can be unreadable on either background and it abandons identity, which is the point of the item). |
| 3 | 2026-07-23 | **Every preset is contrast-checked against both palettes with a minimum WCAG-style ratio, enforced by a test.** | A preset set multiplies the ways an accent can be unreadable; the check is a pure function over colour pairs, so guaranteeing it is cheap while discovering the failure by eye is expensive and unreliable. |
| 4 | 2026-07-23 | **Scope is palette, accent, corner radius and spacing — no typography scale and no bundled font.** | Every existing custom-painted widget already reads `ui.visuals()`, so a style-level pass reaches the whole UI with no new assets or dependencies. Rejected: adding a typography scale (does more for "product vs dev tool" than colour, but touches every label's implied size and would need the responsive fader-height maths re-checked); bundling a custom font (the biggest identity lever, but a licensed asset, added binary size, and a real missing-glyph risk this codebase has already been bitten by). |
| 5 | 2026-07-23 | **Semantic colours stay semantic and are not accent-derived.** | Clip red must read as danger regardless of the chosen accent; letting the accent reach the meter's zone colours would let a preset turn a clip indicator into something that looks like a normal level. |
| 6 | 2026-07-23 | **A programmatic brand mark, drawn in code, replaces `placeholder_icon`.** | The codebase already custom-paints its icons after the emoji-tofu incident, so this is a known technique with no asset pipeline, and it keeps the feature completable without external art. Accepted cost: a code-drawn mark has a quality ceiling and must stay legible at 16 px. Rejected: user-supplied .ico art (best possible result, but the feature could not be finished without it); leaving the placeholder (cleanest boundary, but the app's most-seen surface stays a blue square). |
| 7 | 2026-07-23 | **The tray mark follows the *system* theme, not the app's theme preference.** | The taskbar has its own light/dark theme, independent of whatever the user forced the app to. Rendering the mark against the app's theme would make it vanish on an opposite-themed taskbar. `system_theme()` gives the right input, and the mark is re-rendered when it changes. Accepted cost: the tray mark and the in-window mark can differ at the same moment — correct, but it looks inconsistent to anyone seeing both. Rejected: one accent-coloured mark with a contrast outline (same everywhere and keeps the accent visible, but an outline at 16 px eats a large fraction of the glyph); a monochrome inverting mark (what most Windows tray icons do and the most reliably legible, but it drops the accent from the app's most-seen surface). |
| 8 | 2026-07-23 | **`contrast_ratio` lives in the test module, not the public API.** | Capability 6's check is its only consumer, and user-defined accents are out of scope, so a runtime guard would solve a problem this version does not have. Recorded because the obvious decomposition puts it beside the other pure colour helpers. |
| 9 | 2026-07-23 | **The tray mark is refreshed at startup, on every tray-menu rebuild, and opportunistically while the settings window is open — stale in between, by choice.** | Decision 7 requires the mark to follow the *system* theme, but nothing watches for that: the tray runs on its own thread with no window, and a hidden eframe window does not run `update`, so the UI cannot be the watcher either. This cost only surfaced at Level 3. Rejected: `RegNotifyChangeKeyValue` on the Personalize key from `win-shell` (always correct and immediate, but new unsafe Win32, a watcher thread, and a hard dependency on the app-icons blueprint landing first); polling on a timer (no unsafe and bounded staleness, but a permanent timer doing nothing useful in a process whose entire idle-footprint story is that it does not do that). |
| 10 | 2026-07-23 | **Theme and accent choices are config types in `engine::graph` (`ThemeChoice`, `AccentChoice`), not `app` types.** | `ConfigEdit` lives in `control`, which must never depend on `app`, so an `app`-owned `AccentPreset` could not appear in an edit variant. The standing "Config type home" decision already puts config types in `engine` (`HotkeyChord` is the precedent). `app::theme` maps them to egui types at the boundary, which also removes the redundant `AccentPreset` Level 2 had. Caught at Level 4 and corrected rather than worked around. |
| 11 | 2026-07-23 | **Design approved at Level 4. Status set to `approved` -- ready for implementation.** | All four level sections persisted; no open questions. |

## Design: Level 2 -- Components

**Approved 2026-07-23.** All in `app`; `control` gains two config fields. No
engine, `audio-core` or `win-audio` change, and no new crates.

Confirmed while building this level: `Options::theme()` resolves
`ThemePreference::System` against the OS theme on read, so registering both
palettes via `set_visuals_of` makes capability 3's live following automatic —
no watcher, no re-install on OS theme change.

| # | Component | Layer | Change | Justification |
|---|---|---|---|---|
| 1 | `app::theme::{Accent, AccentPreset, accent()}` | UI | **new**, pure | Decision 2 — each preset carries a dark and a light variant. |
| 2 | `app::theme::{Semantic, semantic(Theme)}` | UI | **new**, pure | Capabilities 8 and 9. Replaces `METER_GREEN/AMBER/RED` and the duplicated warning colour with named, theme-aware, **non-accent-derived** entries. |
| 3 | `app::theme::visuals(Theme, Accent) -> Visuals` | UI | **new**, pure | The palette builder, one per theme. |
| 4 | `app::theme::style() -> Style` | UI | **new**, pure | Corner radius, spacing, strokes. Separate from `visuals` because these are theme-independent — one set of values serves both palettes. |
| 5 | `app::theme::install(ctx, pref, preset)` | UI | **new** — the only impure function here | `set_visuals_of` x2, `set_style`, `set_theme`. Called from the `CreationContext` closure (capability 11) and again on preference change. |
| 6 | `app::theme::brand_icon_rgba(size, accent, theme) -> Vec<u8>` | UI | **new**, pure rasterizer | One implementation feeds the tray icon, the window icon and any in-window brand display (uploaded via `load_texture`, as app-icons does). A second egui-shape renderer for the same mark would drift — the reasoning that made `paint_meter` one painter with an axis flag. |
| 7 | `ConfigEdit::{SetTheme, SetAccent}` + `[app] theme` / `[app] accent` | control | **new** | Config stays the source of truth, rather than eframe's own persisted state. |
| 8 | `app::ui` theme + accent pickers | UI | **new** controls | Capabilities 2 and 5. |
| 9 | `main.rs` install point | shell | one call in the existing `CreationContext` closure | Capability 11 — no new plumbing; the closure is already used. |
| 10 | `tray.rs` | shell | `placeholder_icon` -> `brand_icon_rgba`, re-rendered on system-theme change | Capabilities 10 and decision 7. |

**Challenged and moved:** `contrast_ratio(a, b)` is used **only** by capability
6's check and nothing consumes it at runtime — user-defined accents are out of
scope. It lives in the test module, not the public API. A runtime contrast guard
would be a component solving a problem this version does not have.

**Components rejected:** our own `Theme` enum (egui's is fine); a `ThemeManager`
type (four pure functions plus one installer); per-widget style overrides
(capability 12 exists precisely so widgets do not acquire theme knowledge).

**DDD note:** nothing domain-side. `Accent`, `Semantic` and `AccentPreset` are
UI value types.

## Design: Level 3 -- Interactions

**Approved 2026-07-23.** No domain involvement whatsoever — this feature never
crosses out of `app` except for two config fields.

**Flow A — startup install**

```
run_native's CreationContext closure:
    let app_cfg = handoff.ui_state.lock().snapshot.app;   // already available here
    theme::install(&cc.egui_ctx, app_cfg.theme, app_cfg.accent);
```

Before the first frame, so capability 11 holds with no new plumbing.

**Flow B — the user changes theme or accent**

```
picker -> ConfigEdit::SetTheme / SetAccent -> EditParams -> store write -> new snapshot
UI compares snapshot.app.{theme,accent} against last-installed -> differs -> theme::install(..)
```

No event and no channel — the same compare-and-react shape the rest of the UI
already uses.

**Flow C — the OS theme flips while Follow-system is selected** -> the window
needs nothing: `Options::theme()` re-resolves on read, so the two registered
palettes simply swap. The tray is the exception (decision 9).

**Flow D — semantic colours at their call sites** -> `paint_meter` and the
routing-degraded label call `semantic(ui.visuals().dark_mode)` instead of
reading module constants. The five hardcoded values disappear and the
duplicated warning colour becomes one entry.

**Flow E — the brand mark in-window** -> `brand_icon_rgba(size, accent, theme)`
-> `ctx.load_texture` once, cached like an app icon.

**Flow F — accent change** -> `visuals()` rebuilt for both themes and
re-installed; every widget follows on the next frame because they all read
`ui.visuals()`.

**Flow G — tray mark refresh** -> rendered at startup, again whenever the tray
menu is rebuilt, and immediately if the settings window happens to be open when
the system theme flips. Between those points the mark can be stale
(decision 9).

## Design: Level 4 -- Contracts

**Approved 2026-07-23.**

**One layering correction from writing these.** `ConfigEdit` lives in `control`
and cannot reference an `app` type, so the theme and accent choices must be
config types in `engine::graph` -- the standing "Config type home" decision,
same as `HotkeyChord`. `app::theme` maps them to egui types at the boundary,
which also removes the redundant `AccentPreset` Level 2 had (decision 10).

### `engine::graph` (config types)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ThemeChoice { Dark, Light, #[default] System }

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AccentChoice { #[default] Brand, Teal, Amber, Violet, Slate }

pub struct AppConfig {
    // ...
    pub theme: ThemeChoice,
    pub accent: AccentChoice,
}
```

### `control`

```rust
pub enum ConfigEdit {
    // ...
    SetTheme(ThemeChoice),
    SetAccent(AccentChoice),
}
// Both are `EditPath::Param` -- no engine effect at all.
```

### `app::theme`

```rust
/// One accent, in both palettes -- two hand-picked values rather than one
/// lightness-shifted (decision 2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accent { pub dark: egui::Color32, pub light: egui::Color32 }

pub fn accent(choice: AccentChoice) -> Accent;

/// Colours that carry meaning. **Never derived from the accent** (decision 5):
/// a clip indicator must read as danger under every preset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Semantic {
    pub meter_ok: egui::Color32,
    pub meter_hot: egui::Color32,
    pub meter_clip: egui::Color32,
    pub warning: egui::Color32,
}
pub fn semantic(theme: egui::Theme) -> Semantic;

/// Per-theme palette.
pub fn visuals(theme: egui::Theme, accent: Accent) -> egui::Visuals;

/// Theme-independent: corner radius, spacing, stroke widths.
pub fn style() -> egui::Style;

/// The only impure function here -- registers both palettes, the style and the
/// preference. Called from the `CreationContext` closure and on any change.
pub fn install(ctx: &egui::Context, theme: ThemeChoice, accent: AccentChoice);

/// Rasterized brand mark, `size` x `size` RGBA8, straight alpha. One
/// implementation for the tray icon, the window icon and the in-window mark
/// (decision 6). `theme` is the *surface it will sit on* -- the system theme
/// for the tray (decision 7), the app's theme in-window.
pub fn brand_icon_rgba(size: u32, accent: Accent, theme: egui::Theme) -> Vec<u8>;
```

### Call-site changes

```rust
// ui.rs -- the five hardcoded colours disappear
let sem = theme::semantic(if ui.visuals().dark_mode { Theme::Dark } else { Theme::Light });
painter.rect_filled(fill, 2.0, meter_color(fraction, sem));     // was METER_GREEN/AMBER/RED
ui.colored_label(sem.warning, "Routing degraded -- ...");        // was the duplicated literal

// main.rs -- inside the existing CreationContext closure
theme::install(&cc.egui_ctx, app_cfg.theme, app_cfg.accent);

// tray.rs
Icon::from_rgba(theme::brand_icon_rgba(16, accent, system_theme), 16, 16)
```

### Config

```toml
[app]
theme  = "system"   # dark | light | system
accent = "brand"    # brand | teal | amber | violet | slate
```

### Test contracts

| Layer | Test |
|---|---|
| `app::theme` | `every_accent_meets_minimum_contrast_on_both_palettes` -- presets x themes; **the capability-6 guarantee** |
| `app::theme` | `contrast_ratio_of_black_on_white_is_twenty_one` -- pins the WCAG formula so the check above means something |
| `app::theme` | `semantic_colours_do_not_vary_with_the_accent` -- decision 5 regression |
| `app::theme` | `brand_icon_rgba_returns_size_squared_times_four_bytes` |
| `app::theme` | `the_brand_mark_is_not_fully_transparent` -- the app-icons legacy-alpha lesson, applied before it can bite |
| `app::theme` | `the_brand_mark_differs_between_light_and_dark_surfaces` -- decision 7 |
| `control` | `theme_and_accent_round_trip_through_toml` |
| `control` | `an_unrecognised_theme_value_is_a_validation_error` -- matching how every other enum-valued config field behaves |

## Open Questions

*(none -- every judgment call raised during design was resolved and logged)*

## Design Summary

**Drift check: skipped -- `requirement_doc` is null.** Origin is
`.lattice/ux-gap-roadmap.md` Priority 11, not a requirement spec, so there are
no Scenarios/ACs or `## Technical Constraints` to compare Level 4 against and
nothing to write back.

**Components and layers**

| Layer | Components |
|---|---|
| domain (`audio-core`) | **untouched** |
| orchestration (`engine`) | `ThemeChoice`, `AccentChoice`, two `AppConfig` fields |
| control | `ConfigEdit::{SetTheme, SetAccent}` + store arms; both `EditPath::Param` |
| UI (`app::theme`) | `Accent`, `Semantic`, `accent`, `semantic`, `visuals`, `style`, `install`, `brand_icon_rgba`; `contrast_ratio` test-only |
| shell (`app`) | one `install` call in the existing `CreationContext` closure; `tray.rs` uses the brand mark |

**Key contracts** -- `visuals(theme, accent)` and `style()` are the entire look;
`semantic(theme)` is the deliberate escape hatch for colours that must not
follow the accent; `brand_icon_rgba` is one rasterizer serving three surfaces.

**Architectural constraints honored**

- Config types live in `engine` so `control` never depends on `app` -- caught at
  Level 4 and corrected rather than worked around.
- Widgets keep reading `ui.visuals()`; none acquires theme knowledge, so the
  seven unbuilt blueprints' widgets inherit the theme by construction.
- One rasterizer for the brand mark rather than a second egui-shape renderer,
  the same anti-drift reasoning as `paint_meter`'s single painter.
- Every colour promise that could silently break -- accent contrast, semantic
  independence, mark opacity -- is a test, not an intention.

**Domain model** -- nothing domain-side. `Accent`, `Semantic`, `ThemeChoice` and
`AccentChoice` are value types.

**Open questions resolved during design** -- theme modes (decision 1), accent
model (2), restyling depth (4), icon scope (6), which theme the tray mark
follows (7), and how the tray learns about a system theme change (9).

**Cost accepted at Level 3** -- the tray mark can be stale between rebuilds,
because nothing in a tray-only process is positioned to watch the system theme
without new unsafe or a permanent timer (decision 9).
