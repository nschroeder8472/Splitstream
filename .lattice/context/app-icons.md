---
feature: app-icons
requirement_doc: null
created: 2026-07-22
status: complete
note: >
  Roadmap Priority 2 (.lattice/ux-gap-roadmap.md). Real exe icons on the
  session chips, which are bare text today (often `game.exe`). No requirement
  spec — roadmap is the origin. First feature in this codebase to touch the
  Windows shell rather than Windows audio.
---

# App Icons on Session Chips

> Every session chip gets the real application icon beside its label, extracted
> from the process's exe. Chips are the one place the user identifies *which
> app* they are routing, and today they read `game.exe`. Extraction happens off
> the render thread; an initial-letter tile stands in until (or instead of) a
> real icon.

## Grounding (2026-07-22, pre-Level-1)

- **The exe path is already there.** `SessionInfo.process_path: PathBuf`
  (rules.rs:85) is populated for every session and is already used by
  `match_session` and `resolve_drag_assign`. Nothing new is needed to identify
  the file to extract from. It *can* be empty — `chip_label`'s pid fallback
  exists precisely because some sessions arrive without a usable path.
- **One shared render site.** `session_drop_zone` (ui.rs:1021) draws every
  chip as `ui.dnd_drag_source(id, DragSession(pid), |ui| ui.label(chip_label(session)))`,
  and both the master column (ui.rs:483, unassigned pool) and every group
  column (ui.rs:534, routed sessions) call it. The icon lands in exactly one
  place and appears everywhere chips do.
- **`app` contains zero `unsafe`.** Every unsafe block in the workspace — 49 of
  them — lives in `win-audio` (`sessions.rs` 21, `enumerator.rs` 5,
  `process_capture.rs` 5, `render.rs` 5, and so on). Icon extraction is real
  unsafe (`HICON`, `GetIconInfo`, `GetDIBits`, `DestroyIcon`), so where it
  lives is an architectural question, not a filing preference.
- **`app` already depends on `win-audio`** (app/Cargo.toml), and `win-audio`
  already carries the `windows` crate at 0.62.2 with a curated feature list —
  shell/GDI features are not among them yet.
- **Chip labels already have a fallback ladder**: `display_name` (usually empty
  in practice), then the process file name, then the bare pid (ui.rs:1071).
  The icon needs its own equivalent.
- **Custom paint is this codebase's answer to icon glyphs.** Both
  `speaker_mute_button` and `paint_meter` are hand-painted rather than glyph-
  based, after the emoji-range tofu problem was hit and logged. A fallback
  visual should follow that precedent.

## Design: Level 1 -- Capabilities

**Approved 2026-07-22.**

1. **Real app icon on every session chip** — a fixed 16 pt icon slot beside the
   label in `session_drop_zone`, so it appears in both the master column's
   unassigned pool and every group's routed list at once.
2. **Extraction never blocks the render thread** — a cache miss enqueues the
   exe path to a worker; the frame draws the fallback and returns immediately.
   The icon swaps in on a later frame once it is ready.
3. **Initial-letter tile fallback** — a custom-painted square carrying the chip
   label's first letter, in a color derived from the label so it is stable per
   app. Shown while an icon is pending, when the exe has no icon, when
   extraction fails, and when `process_path` is empty (the pid-only session
   case).
4. **Path-keyed cache, shared across pids** — two instances of the same app
   extract once and share one texture; a session closing and reopening does not
   re-extract.
5. **Failures are cached too** — a path that fails extraction is remembered as
   failed, so it is never retried on a later frame. One attempt per path per
   run.
6. **32 px extracted, drawn at 16 pt** — stays crisp on 1.5x/2x displays.
7. **No layout shift** — the icon slot is a fixed size occupied by the tile
   from the very first frame, so the tile→icon swap never moves the label or
   resizes the chip.
8. **Drag behaviour unchanged** — the icon renders inside the existing
   `dnd_drag_source` body, so the whole chip (icon + label) remains one drag
   handle.

Out of scope (v1): UWP/packaged-app icons via their AppX manifest (exe icon
only — a packaged app may show a generic or wrong icon); icon theming or
recoloring; icons anywhere other than chips (tray menu, group headers, device
rows); refreshing an icon when the exe changes on disk; a disk-persisted cache
across restarts.

## Design: Level 2 -- Components

**Approved 2026-07-22.** Nothing changes in `engine`, `control`, or
`audio-core` — this feature never crosses into the audio path.

| # | Component | Layer | Change | Justification |
|---|---|---|---|---|
| 1 | `win_shell::extract_icon_rgba(path: &Path) -> Option<IconImage>` | platform (new crate) | **new**, unsafe Win32: `SHGetFileInfo`/`ExtractIconEx` -> `GetIconInfo` -> `GetDIBits` -> BGRA→RGBA -> `DestroyIcon` | Returns owned pixels + dimensions, no egui types — the crate stays UI-framework-agnostic. |
| 2 | `app::icons::IconCache` | shell/UI | **new** — `HashMap<PathBuf, IconSlot>` where `IconSlot = Pending \| Ready(TextureHandle) \| Failed`, plus the request `Sender` and result `Receiver` | Holds decisions 4 and 5: path-keyed sharing across pids, and negative caching so a failing path is never retried. |
| 3 | `app::icons::spawn_icon_worker` | shell | **new** thread: receives `PathBuf`, calls #1, sends back `(PathBuf, Option<IconImage>)` | Decision 1. Matches the `spawn_tray`/`spawn_hotkeys` precedent — `app` spawns small purpose-built threads rather than multiplexing onto the dispatcher, which is the config/engine funnel. |
| 4 | `IconCache::poll(&mut self, ctx: &egui::Context)` | UI | **new** — drains results, uploads via `ctx.load_texture`, marks `Failed` on `None` | The thread boundary (decision 9): textures can only be created on the UI thread, so the upload happens here once per frame from already-decoded pixels. |
| 5 | `app::ui::letter_tile` + `tile_color` | UI | **new** custom-painted fallback; `tile_color(&str) -> Color32` pure | Decision 2. Custom paint per the `speaker_mute_button`/`paint_meter` precedent; `tile_color` split out pure so it is testable. |
| 6 | `app::ui::session_drop_zone` | UI | icon slot added inside the existing `dnd_drag_source` body | Capabilities 1, 7, 8 — one render site, fixed slot size, drag handle unchanged. |
| 7 | `SettingsApp` | UI | `+ icons: IconCache` | Same shape as the existing `holds` field. |

**Components rejected:**

- **An `IconSource` port trait**, on `AudioSystem` or beside it. One
  implementation, one consumer, and the logged rule says facade growth stops at
  concern boundaries — icons are not audio. `engine` never sees this feature.
- **Extending `SessionInfo` with icon data.** It is a domain matching type
  consumed by `match_session`; pushing UI texture concerns into it pollutes the
  wrong layer.
- **Reusing the dispatcher or routing thread as the worker.** Both own specific
  funnels; adding icon extraction would make them general-purpose.

**Accepted growth:** the cache is unbounded in principle but keyed by distinct
exe paths actually observed — bounded by how many apps the user runs, at ~4 KB
per 32 px texture. Called out explicitly because the `holds` map was flagged
for this exact shape in this session's review; here the bound is real.

**DDD note:** no aggregate, entity, or value object is involved. `IconImage` is
a plain data carrier crossing a thread boundary, and `IconSlot` is UI state.
The domain model is untouched.

## Design: Level 3 -- Interactions

**Approved 2026-07-22.** No engine, control, or domain interaction anywhere —
no domain events, no aggregate involvement. The whole feature is
`UI <-> worker <-> Win32`.

**Flow A — chip render, cache hit**

```
session_drop_zone -> icons.slot(&session.process_path)
  -> IconSlot::Ready(handle) -> ui.image((handle.id(), vec2(16.0, 16.0)))
  -> then ui.label(chip_label(session))    // both inside dnd_drag_source
```

**Flow B — chip render, cache miss**

```
icons.slot(path) -> None
  -> cache.insert(path, Pending); tx.send(path)     // non-blocking
  -> letter_tile(ui, chip_label(session))           // this frame, no stall
```

Chips with an empty `process_path` are never enqueued and never leave the tile
— capability 3's pid-only case.

**Flow C — worker thread**

```
loop {
    let path = rx.recv()?;                             // blocks here, not the UI
    let result = win_shell::extract_icon_rgba(&path);  // shell + filesystem
    results_tx.send((path, result));
    ctx.request_repaint();                             // wake the UI
}
```

**Flow D — result drain, once per frame**

```
IconCache::poll(ctx):
  while let Ok((path, result)) = results_rx.try_recv() {
      match result {
          Some(img) => Ready(ctx.load_texture(
              path_str,
              ColorImage::from_rgba_unmultiplied([w, h], &img.rgba),
              TextureOptions::LINEAR,
          )),
          None => Failed,        // never retried (decision 5)
      }
  }
```

**Flow E — many pids, one path.** Two instances of the same app hit the same
cache entry; the second finds it already `Pending` or `Ready`, so exactly one
extraction happens. A session closing leaves the entry intact, so relaunching
the app is a cache hit.

## Design: Level 4 -- Contracts

**Approved 2026-07-22.**

### New crate `win-shell`

```rust
// crates/win-shell/src/lib.rs

/// Owned, decoded icon pixels crossing the worker -> UI thread boundary.
/// RGBA8, **straight (non-premultiplied) alpha**, row-major, top-down --
/// matching `ColorImage::from_rgba_unmultiplied`'s expectation exactly.
pub struct IconImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,      // width * height * 4
}

/// Extracts the large (32 px) shell icon for an executable.
/// `None` when the path is empty or missing, the shell returns no icon, or any
/// Win32 step fails. Callers cache the `None` -- this is never retried.
pub fn extract_icon_rgba(path: &Path) -> Option<IconImage>;
```

Implementation requirements, stated as contract because each is a known trap:

- `DestroyIcon` on every path out, including early returns — an RAII guard, not
  a trailing call.
- Every GDI object (`HBITMAP`, `HDC`) released on all paths.
- Top-down rows via **negative `biHeight`**, 32 bpp, `BI_RGB`.
- BGRA -> RGBA swizzle (Win32 DIB order is not egui's).
- **The all-zero-alpha case:** legacy 24 bpp icons decode with every alpha byte
  `0`, which renders as a fully transparent — i.e. invisible — icon. If the
  alpha channel is entirely zero, treat the image as opaque (or composite the
  mask bitmap) rather than shipping an invisible texture.

### `app::icons`

```rust
pub struct IconCache { /* entries, request Sender, result Receiver, worker handle */ }

impl IconCache {
    pub fn new() -> IconCache;

    /// Drains finished extractions and uploads their textures. Spawns the
    /// worker on first call, the earliest point a `Context` exists
    /// (decision 10). Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context);

    /// This path's icon, enqueueing an extraction the first time it is seen.
    /// `None` means "draw the fallback" -- pending, failed, or empty path.
    /// Returns a cloned handle so the caller never holds a borrow of the cache
    /// across rendering.
    pub fn texture(&mut self, path: &Path) -> Option<egui::TextureHandle>;
}

fn spawn_icon_worker(
    ctx: egui::Context,
    requests: Receiver<PathBuf>,
    results: Sender<(PathBuf, Option<IconImage>)>,
) -> JoinHandle<()>;
```

`IconSlot` stays private — collapsing the public surface to
`texture() -> Option<TextureHandle>` removes the borrow-across-render problem
entirely (decision 11).

### `app::ui`

```rust
/// Chip icon slot, in points. Fixed so the tile->icon swap never shifts layout.
const CHIP_ICON_SIZE: f32 = 16.0;

/// Icon or fallback tile for one session, drawn at [`CHIP_ICON_SIZE`].
fn chip_icon(ui: &mut egui::Ui, icons: &mut IconCache, session: &SessionInfo);

/// Custom-painted fallback: rounded square in `tile_color(label)` with the
/// label's first character centered. No glyph-font risk.
fn letter_tile(ui: &mut egui::Ui, label: &str, size: f32);

/// Stable per-label tile color -- pure, so identical labels always agree.
fn tile_color(label: &str) -> egui::Color32;
```

Render site becomes:

```rust
ui.dnd_drag_source(id, DragSession(session.pid), |ui| {
    ui.horizontal(|ui| {
        chip_icon(ui, icons, session);
        ui.label(chip_label(session));
    });
});
```

### Manifests

```toml
# crates/win-shell/Cargo.toml
windows = { version = "0.62.2", features = [
    "Win32_UI_Shell", "Win32_Graphics_Gdi", "Win32_Foundation",
] }

# crates/app/Cargo.toml
win-shell = { version = "0.1.0", path = "../win-shell" }
```

Plus the new member in the workspace `members` list.

### Test contracts

| Layer | Test |
|---|---|
| `win-shell` | `a_missing_path_extracts_to_none` |
| `win-shell` | `an_empty_path_extracts_to_none` |
| `win-shell` | `#[ignore]` `a_real_system_exe_yields_thirty_two_pixel_rgba_with_visible_alpha` — asserts 32×32 **and that alpha is not entirely zero**, precisely the legacy-icon trap above. Follows this codebase's existing real-environment `#[ignore]` precedent. |
| `app` | `tile_color_is_stable_for_the_same_label` |
| `app` | `tile_color_differs_across_distinct_labels` |
| `app` | `an_empty_process_path_is_never_enqueued` |

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-22 | **Extraction runs on an off-thread worker; a cache miss renders the fallback and returns immediately.** | Icon extraction hits the shell and the filesystem and can block for seconds on a network or disconnected path. This codebase has violated "never block the render thread" four separate times and fixed a fifth this same session — the rule is treated as structural here, not as an optimization. Rejected: blocking on first sight (much simpler — no thread, channel, or pending state — and sub-millisecond in the common warm-cache case, but one bad path freezes the whole window). |
| 2 | 2026-07-22 | **Fallback is a custom-painted initial-letter tile**, colored from the label. | Keeps every chip visually uniform (nothing ever renders an empty slot) and gives a per-app-stable visual. Custom-painted rather than a glyph, following `speaker_mute_button`/`paint_meter` after this codebase's emoji-range tofu problem. Rejected: label-only (simplest, but chips become ragged once some have icons and some don't); a single generic placeholder (uniform but carries no per-app information). |
| 3 | 2026-07-22 | **Extract at 32 px, draw at 16 pt.** | Crisp on 1.5x/2x displays, which is now the common case, and the window is already DPI-responsive. Costs 4x texture memory per icon — negligible at these counts. Rejected: `SHGFI_SMALLICON` at 16 px (pixel-exact at 100% scaling, visibly soft anywhere else). |
| 4 | 2026-07-22 | **The tile is the pending state as well as the failure state.** | One fallback path instead of two, and a chip never renders an empty slot at any point in its lifecycle. |
| 5 | 2026-07-22 | **Cache is in-memory and process-lifetime; failures are cached alongside successes.** | Negative caching is what stops a failing path being retried every frame forever. Rejected: disk persistence across restarts (a real startup optimization, but it needs an invalidation story for exe updates plus a cache location, for a cost measured in milliseconds per app per run). |
| 6 | 2026-07-22 | **The unsafe Win32 extraction goes in a new `win-shell` crate**, not in `win-audio` and not in `app`. | Keeps `win-audio` honestly about audio and preserves `app`'s zero-`unsafe` invariant — the crate holding the egui render loop should not start carrying `HICON`/`GetDIBits`. `win-shell` gets its own curated `windows` feature list (`Win32_UI_Shell`, `Win32_Graphics_Gdi`) independent of the audio one. Cost: one more workspace member for what is currently a single module. Rejected: `win-audio` as "the Windows platform crate" (zero new crates, existing COM discipline — but the name stops describing the contents, and the next Windows-but-not-audio feature makes it worse); `app` behind a `windows` dep (everything in one crate, but breaks the zero-unsafe invariant). |
| 7 | 2026-07-22 | **No port trait; `engine` is untouched.** | Icon extraction implements nothing `engine` defines, and the `app` -> platform-crate edge already exists for composition-root reasons. A trait here would be an abstraction with one implementation and one consumer. |
| 8 | 2026-07-22 | **A dedicated worker thread, not the dispatcher or routing thread.** | Both existing threads own specific funnels (config/engine commands, session routing); adding icon extraction would make them general-purpose. `app` already spawns small purpose-built threads (`spawn_tray`, `spawn_hotkeys`) — this follows that shape. |
| 9 | 2026-07-22 | **The worker returns raw pixels; the `TextureHandle` is created on the UI thread.** | `Context::load_texture` (context.rs:2322) is the only way in, and it needs the egui context — so the crossing point has to be `IconImage` (owned RGBA + size), uploaded during `IconCache::poll`. This also keeps `win-shell` free of any egui dependency. |
| 10 | 2026-07-22 | **The worker holds a cloned `egui::Context` and calls `request_repaint()` when a result lands.** | The alternative — relying on the mixer screen's existing `request_repaint_after(16ms)` from the level meters to pick icons up incidentally — works today but is an invisible dependency: if that repaint were ever moved, removed, or made conditional, icons would silently stop appearing until the user moved the mouse. `egui::Context` is `Clone + Send`, so this costs one field. Consequence: the worker can only be spawned once a `Context` exists, i.e. lazily on the first `IconCache::poll`, not in `SettingsApp::new`. |
| 11 | 2026-07-22 | **`IconCache`'s public surface is `texture(&mut self, path) -> Option<TextureHandle>`, returning a clone; `IconSlot` stays private.** | An accessor handing back `Option<&IconSlot>` would hold a mutable borrow of the cache across the chip's rendering, fighting the `&mut egui::Ui` in the same scope. Cloning a `TextureHandle` is a refcount bump. Collapsing the enum out of the public API also means "pending", "failed" and "no path" are one case at the call site — exactly what decision 4 wanted (one fallback path). |
| 12 | 2026-07-22 | **`extract_icon_rgba` must handle the all-zero-alpha legacy icon case**, and the real-environment test asserts on it. | A 24 bpp icon's DIB decodes with every alpha byte `0`; shipped verbatim that is a fully transparent texture — an invisible icon that looks exactly like "extraction silently didn't work", with no error to trace. Found by reasoning through the DIB contract at design time; the `#[ignore]`d real-exe test is written to fail on it rather than leaving it to a bug report. |
| 13 | 2026-07-22 | **Design approved at Level 4. Status set to `approved` — ready for implementation.** | All four level sections persisted; no open questions. |
| 14 | 2026-07-23 | **Implementation complete**, inside-out (`win-shell` crate -> `app::icons` -> `app::ui`). Two real corrections found before/during implementation, both fixed: (1) the blueprint's stated `win-shell` Cargo.toml feature list omitted `Win32_UI_WindowsAndMessaging` — `ExtractIconExW`, `DestroyIcon`, `GetIconInfo`, `ICONINFO`, and `HICON` all live under it, confirmed by reading the pinned `windows` 0.62.2 source before writing any code, not assumed from the design doc; (2) the coordination note proposing `ChipZoneCtx` absorb a `&mut IconCache` field doesn't compile — `ChipZoneCtx` is `Copy` and passed as `&ChipZoneCtx`, and a unique reference nested in a field can never be reborrowed back out through a shared reference to the struct. `session_drop_zone` instead takes `icons: &mut IconCache` as its own parameter. RAII cleanup uses `windows_core::Owned<HICON>`/`Owned<HBITMAP>` (both already implement the crate's own `Free` trait) rather than hand-written guard structs — "prefer the simpler path" over what the design's Grounding implied would be needed. All 6 planned test contracts present; the real-exe `#[ignore]` test was run manually against `notepad.exe` and the extracted icon was additionally dumped to a BMP and visually inspected (correct colors, correct orientation) — not just dimension/alpha-checked. Full workspace suite green (320 tests) and `cargo clippy --workspace --all-targets` clean. | Verified against the real diff before closing. No live-app screenshot: no project skill exists for launching this native desktop GUI, and the higher-risk code (the Win32/GDI pipeline) was already empirically verified; the remaining UI wiring is a plain, type-checked call site. |

## Open Questions

*(none — every judgment call raised during design was resolved and logged)*

## Design Summary

**Drift check: skipped — `requirement_doc` is null.** Origin is
`.lattice/ux-gap-roadmap.md` Priority 2, not a requirement spec, so there are no
Scenarios/ACs or `## Technical Constraints` to compare Level 4 against and
nothing to write back.

**Components and layers**

| Layer | Components |
|---|---|
| platform (**new** `win-shell` crate) | `IconImage`, `extract_icon_rgba` — the only `unsafe` this feature adds, deliberately outside both `app` and `win-audio` |
| shell (`app::icons`) | `IconCache` (path-keyed, negative-caching), `spawn_icon_worker` |
| UI (`app::ui`) | `chip_icon`, `letter_tile`, `tile_color`, `CHIP_ICON_SIZE`; `session_drop_zone` gains the icon slot; `SettingsApp` gains an `icons` field |
| engine / control / audio-core | **untouched** |

**Key contracts** — `extract_icon_rgba(&Path) -> Option<IconImage>` (owned
straight-alpha RGBA, no egui types) and
`IconCache::texture(&Path) -> Option<TextureHandle>` (`None` = draw the tile).
Those two signatures carry the whole feature.

**Architectural constraints honored**

- `app` keeps its zero-`unsafe` invariant; `win-audio` keeps meaning "Windows
  *audio*".
- No port trait, no `engine` involvement — icons implement nothing the engine
  defines, and the app -> platform-crate edge already exists.
- The render thread never blocks on the shell or the filesystem (decision 1),
  the rule this codebase has broken four times and fixed a fifth this session.
- `win-shell` takes no egui dependency: the thread boundary is plain pixels.

**Domain model** — untouched. No aggregate, entity, or value object;
`IconImage` is a data carrier and `IconSlot` is UI state.

**Open questions resolved during design** — where the unsafe lives (decision 6:
a new `win-shell` crate rather than stretching `win-audio`'s name or breaching
`app`'s zero-unsafe invariant).

**Traps caught at design time rather than implementation time** — the
all-zero-alpha legacy icon decoding to an invisible texture (decision 12), and
the invisible dependency on the level meters' repaint for icons to ever appear
(decision 10).

**Traps caught during implementation, not design** — the Cargo.toml feature
gap and the `ChipZoneCtx`/`&mut IconCache` borrow-check dead end (decision 14).

**Found and fixed by `/review` (2026-07-23)** — `IconCache`'s decisions 4 and 5
(path-keyed dedup, negative caching) had zero test coverage despite being pure
`HashMap`-state assertions with no Win32 dependency. Added
`a_failed_extraction_is_never_retried` and `a_pending_path_is_not_enqueued_twice`,
both verified discriminating. See the operational learning — fourth occurrence
this session-block of a new type's single most load-bearing guarantee shipping
untested.

## Key Files

| Path | Role |
|---|---|
| crates/win-shell/Cargo.toml | New workspace member; `windows` feature list |
| crates/win-shell/src/lib.rs | `IconImage`, `extract_icon_rgba` — the only `unsafe` this feature adds |
| crates/app/src/icons.rs | `IconCache` (private `IconSlot`), `spawn_icon_worker` |
| crates/app/src/ui.rs | `CHIP_ICON_SIZE`, `chip_icon`, `letter_tile`, `tile_color`; `session_drop_zone` gains `icons: &mut IconCache`; `SettingsApp.icons`; `poll()` called once per frame in `ui()` |
| crates/app/src/main.rs | `mod icons;` |
| crates/app/Cargo.toml, Cargo.toml | `win-shell` dependency; new workspace member |
