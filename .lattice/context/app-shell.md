---
feature: app-shell
requirement_doc: Splitstream-Engineering-Spec.md
created: 2026-07-18
status: complete
---

# App Shell (P4)

> P4 — tray icon, global hotkeys, settings UI, config hot-reload surface, autostart, single-instance. Exit criteria: full user control surface; launches at logon; one instance enforced (spec §13).

## Decisions Log

<!-- Add new at bottom. Never remove. -->

| Date | Decision | Reasoning | Alternatives Considered |
|------|----------|-----------|------------------------|
| 2026-07-18 | Blueprint scope = P4: tray + hotkeys + settings UI + autostart + single-instance + hot-reload surface (F9–F11). Builds on approved P0–P3 designs. | Spec §13 phase order; audio + routing designs approved. | P5 DSP. |
| 2026-07-18 | UI toolkit = egui/eframe. **Resolves spec §15.1.** | Spec's own recommendation: fastest iteration, pure Rust, immediate mode fits live faders/meters; non-native look acceptable for tray utility. | slint (native look, licensing consideration); Tauri (heavier, conflicts N1 idle footprint). |
| 2026-07-18 | Shell Win32 needs (named mutex, autostart registry, instance signal) via wrapper crates (single-instance/auto-launch style) — app never imports windows-rs. Constraint kept as written. | Bright-line rule stays greppable; audio-path testability rationale untouched; app is Windows-only binary anyway. Revisit with recorded decision if a crate lacks instance-signaling. | Relaxing constraint wording to COM/WASAPI-only (reviewer judgment creep); wrappers in win-audio (wrong cohesion). |
| 2026-07-18 | UI config writes via `toml_edit`, temp-file + rename atomic; ConfigStore suppresses watcher echo of own writes. | Users hand-edit config (hot-reload is a feature) — comments/ordering must survive UI edits; atomicity prevents watcher reading half-written file. | serde re-serialize (destroys comments/ordering). |
| 2026-07-18 | All shell mutations funnel through ConfigStore → file → watcher → diff → engine. UI/tray/hotkeys never mutate engine directly; reads only. | Config = single source of truth (spec §6.5/§11.1); one mutation path = one validation point; external and UI edits identical downstream. | Direct EngineHandle commands from UI with async config write-back (two sources of truth, drift risk). |
| 2026-07-18 | **Revision of previous decision** (L3 backtrack): param edits get a fast path — immediate `MixerCommand` via EngineHandle + debounced config write of same value. Structural/rules edits remain funnel-only. | `notify` round-trip lags fader drags audibly (100s of ms) — core UX of the app. Idempotent double-apply; config still source of truth. Spec §6.5 explicitly allows UI sending commands. | Funnel-only (strict invariant, laggy faders); command-only with write-back (two sources of truth). |
| 2026-07-18 | Config schema additions: top-level `muted: bool` (effective master = muted ? 0 : master) and `[app] autostart: bool`. | Mute must preserve master gain value; autostart user-controllable per F11. | Mute as master=0 overwrite (loses user's gain setting). |
| 2026-07-18 | Config edits are semantic (`ConfigEdit` enum), never raw text; GroupId = snapshot order with `group_id_for` resolution. | Semantic edits keep toml_edit surgical (comment preservation) and give one validation point; positional GroupId avoids a parallel id registry. | Raw-text patching (fragile); persistent group UUIDs (schema churn, no need at this scale). |
| 2026-07-18 | Design approved at Level 4. Status set to approved — ready for implementation. | All four levels approved and persisted; drift check vs spec complete. | — |
| 2026-07-18 | **Revision (cross-blueprint review):** app-side `EventPump` owns the single `take_events` receiver and fans out — notification channel to tray, state updates to `UiState`, `update_topology` call to routing after structural-rebuild events. `spawn_tray` now takes a pump-fed `Receiver<EngineEvent>`, not the engine's. | `Receiver` is single-consumer; tray and UI both need events. Fan-out is an app concern — engine stays consumer-agnostic. | Broadcast channel in engine; tray-only events (UI blind to degraded/fallback state). |
| 2026-07-18 | **Revision (cross-blueprint review):** mute is **global** — silences all groups including `follow_master = false` ones. Implemented as output-stage kill flag; master gain and group gains untouched. `MixerCommand::SetMuted` semantics updated; recorded as spec §11.2 elaboration. | A mute hotkey that leaves audio playing reads as a bug; global expectation wins. Kill-flag keeps gains restorable. | Master-scope mute (spec-pure, surprising); per-group mute flags (config churn for one hotkey). |
| 2026-07-18 | **Revision (cross-blueprint review):** `InstanceGuard::acquire` returns `InstanceOutcome::Primary(guard, Receiver<SurfaceSignal>)` or `Secondary` (signal already sent, caller exits). Fills the missing listener half of flow G. | Flow G had no L4 counterpart — first instance had no contract for receiving the surface signal. | Polling a lock file (latency, jank); OS window message (needs direct Win32 in app — violates wrapper-crate decision). |
| 2026-07-18 | **Revision (cross-blueprint review):** debounce/hand-edit race is last-writer-wins, documented as a known v1 limitation. | Sub-second window, single-user desktop file — merge machinery disproportionate. Revisit only on real reports. | Hash-check + rebase of pending semantic edits (safer, more write-path machinery). |
| 2026-07-20 | **Implementation-time decision (code-forge):** `AppConfig`/`HotkeyMap`/`HotkeyChord` live in `engine::graph`, not `control` as L4's contract text literally placed `HotkeyMap`. | `ConfigSnapshot` (which embeds `AppConfig`) is `engine`'s type; a field on it must resolve without `engine` depending back on `control`. Same interface-at-consumer idiom already logged twice for `GroupRules`/`MatchRule` (session-routing.md). | Mirrored types converted at the app boundary (more boilerplate, no other consumer needs the split). |
| 2026-07-20 | **Implementation-time decision (code-forge):** `ShellAction` gains a `ToggleMute` variant, not in L4's literal enum. | `hotkeys.rs`/`tray.rs` fire the mute action without owning current `muted` state (no `UiState` access from those modules); only the dispatcher (owns the live snapshot) can resolve `!current` into a concrete `ConfigEdit::SetMuted`. Asked the user directly (real fork vs. giving hotkeys.rs a shared `Arc<AtomicBool>` read handle); confirmed. | Shared read handle into hotkeys.rs (second source of truth for "current muted" alongside `UiState`). |
| 2026-07-20 | **Implementation-time decision (code-forge):** tray and hotkeys each run their own dedicated background thread with its own `tao::EventLoop`, not integrated into eframe's winit loop. | Verified against docs.rs (not memory): both `tray-icon`/`muda` and `global-hotkey` require an active native event loop on their *creation* thread, but it doesn't need to be the main thread. A dedicated thread keeps tray/hotkeys resident and event-driven (near-zero idle CPU, N1) independent of whether the settings window is open — piggybacking on eframe's loop would tie tray presence to window visibility and fight the idle-footprint goal. Asked the user directly; confirmed. | Poll `try_recv()` on tray/hotkey event channels once per egui frame inside eframe's own loop. |
| 2026-07-20 | **Implementation-time decision (code-forge):** `EventPump` does **not** own a `RoutingHandle`, despite the earlier cross-blueprint revision sketching "update_topology call to routing after structural-rebuild events" as one of its jobs. Added `engine::RoutingReader` (`Clone`-able, read-only `is_degraded`/`current_routes`) instead — the settings window polls it every frame; `update_rules`/`update_topology` stay with the dispatcher, called at the exact synchronous points flows C/D/H already specify. | `RoutingHandle` can't be `Clone` (owns the coordinator thread's `JoinHandle`), so an owner-by-value `EventPump` would starve the dispatcher of the same handle. More fundamentally, no `EngineEvent` variant signals "routes changed" or "structural rebuild done" — most route changes (session add/remove, rule edits) never touch that channel, so event-driven refresh would leave the routed-apps list stale most of the time regardless of ownership. | Making `RoutingHandle` itself `Clone` via `Arc<Mutex<Option<JoinHandle>>>` (messier shutdown semantics, multiple owners could race `shutdown()`). |
| 2026-07-20 | **Implementation-time decision (code-forge):** second-launch signaling (`InstanceOutcome`/`SurfaceSignal`) implemented via the `interprocess` crate's local socket (named pipe on Windows) — not named by any prior decision (the `InstanceOutcome` design postdates the "single-instance/auto-launch style" wrapper-crate decision). | Idiomatic Windows single-instance IPC; avoids a loopback-TCP-port hack (port-squatting ambiguity) for a single boolean wake-up signal. | Loopback TCP on a fixed port (simpler, zero extra dep, but a real if small collision risk). |
| 2026-07-20 | **Implementation-time discovery (code-forge):** a real smoke run (`cargo run`, not any test) panicked: `OleInitialize failed! RPC_E_CHANGED_MODE`. Root cause — `engine::start`/`start_routing`/win-audio's WASAPI COM calls were running inline on the main thread *before* `eframe::run_native`, initializing that thread MTA; `eframe`/`winit` then tried `OleInitialize` (STA) on the same already-MTA thread. Fixed by moving all startup (config load through tray/hotkey spawn) and the dispatcher loop onto a dedicated background thread, handing off `UiState`/`RoutingReader`/`Sender<ShellAction>` to the main thread over a channel; main thread now stays COM-untouched and is reserved exclusively for `eframe::run_native`. | No mock or unit test could have caught this — it's a real OS/thread-apartment interaction between two different libraries' COM usage. Re-ran against real hardware (SteelSeries Sonar virtual bus + real headphones) after the fix, twice (with and without a registered global hotkey); both ran stably. | — |
| 2026-07-20 | **Implementation-time decision (code-forge):** settings window has no hide-to-tray-on-close for v1 — closing it quits the whole app, same as tray "Quit." Documented gap, not silently dropped. | Implementing intercept-close-to-hide correctly (viewport commands, focus-on-reopen) is nontrivial additional scope beyond what L3's flows specify; tray itself stays fully independent/resident in every other respect (own thread, own event loop, survives the settings window's lifecycle in code even though this session didn't wire the hide behavior). | Full viewport show/hide plumbing now (real cost, no flow explicitly requires it yet). |
| 2026-07-20 | **Implementation-time decision (code-forge):** per-group output-device and match-rules editors are plain text fields with an explicit commit button (draft-then-commit pattern), not live dropdowns/pickers. | `UiState`'s L4 contract shape has no device-enumeration read model to back a picker; text fields still let a user set/change values, matching the TOML's existing friendly-name convention. | Adding a live device list to `UiState` now (real new read-model surface, not in any approved contract). |

## Design: Level 1 -- Capabilities

Approved 2026-07-18.

1. **Always-on tray presence** — icon from logon; quick menu (master mute, settings, quit); engine notices as tray notifications.
2. **Settings window** — per-group faders + master, follow-master toggle, per-group output picker, live routed-apps list, add/remove group (mockup: "Create New Audio Source").
3. **Global hotkeys** — config-defined, system-wide.
4. **Config round-trip** — UI edits persist to TOML; external edits appear live in UI; invalid edits never break running audio.
5. **Lifecycle** — autostart at logon (per-user), single instance (second launch surfaces first), clean shutdown.

Out of scope: DSP UI (P5), installer/signing pipeline, driver management beyond degraded prompt.

## Design: Level 2 -- Components

Approved 2026-07-18.

| Component | Home / layer | Single responsibility |
|---|---|---|
| Lifecycle host | `app/main.rs` (shell) | Single-instance guard, surface-first-instance signal, autostart registration, startup wiring (engine → routing → watcher → tray → UI), ordered shutdown |
| Tray surface | `app/tray.rs` (shell) | Icon + menu (mute master, settings, quit); `EngineEvent` → tray notifications |
| Hotkey service | `app/hotkeys.rs` (shell) | Config-defined global hotkeys → `ShellAction` |
| Settings window | `app/ui.rs` (shell, egui/eframe) | Render config draft + live routed-apps; faders, output pickers, group add/remove; edits → ConfigStore |
| ConfigStore | `control/store.rs` (application) | Single write path: validate → comment-preserving `toml_edit` → atomic write (temp+rename); suppress watcher echo of own writes |

`ShellAction` enum (app-internal) unifies tray/hotkey/UI intents; all mutations funnel ConfigStore → file → ConfigWatcher → diff → engine/routing. UI reads engine state only (stats, routes, events) — never mutates engine directly.

```mermaid
graph LR
    TRAY[tray] --> ACT[ShellAction dispatch]
    HK[hotkeys] --> ACT
    UI[settings window] --> ACT
    ACT --> CS[control: ConfigStore]
    CS -->|atomic write| FILE[(config.toml)]
    FILE --> W[ConfigWatcher] --> D[diff] --> ENG[EngineHandle / RoutingHandle]
    ENG -->|EngineEvent, stats, routes| TRAY
    ENG --> UI
```

## Design: Level 3 -- Interactions

Approved 2026-07-18.

**A — Startup:** single-instance guard (fail → signal first instance, exit) → autostart registration per config → load config → engine → routing → watcher → tray + hotkeys → tray-resident. Shutdown reverse.

**B — Param edit (fader/master/mute/follow):** fast path — `ShellAction` → `MixerCommand` via EngineHandle immediately + debounced comment-preserving config write. Same value both paths, idempotent; config remains source of truth. (Revises L2 funnel-only decision — see Decisions Log.)

**C — Structural edit (output change, add/remove group):** funnel-only — ConfigStore write → watcher → `Structural` → rebuild + routing update.

**D — External file edit:** watcher validates → apply + UI refresh from snapshot channel; invalid → keep prior snapshot + tray notification with error.

**E — Hotkey:** `mute_master` toggles `muted` flag (schema addition; effective master = muted ? 0 : master, gain value preserved). Same path as B.

**F — Engine notices:** tray drains `EngineEvent`s → notifications (fallback, degraded-once, driver missing prompt).

**G — Second launch:** instance signal → first instance surfaces settings window.

## Design: Level 4 -- Contracts

Approved 2026-07-18. Deltas on approved P0–P3 contracts; signatures only.

### `audio-core`

```rust
pub enum MixerCommand { /* P0–P2 variants… */ SetMuted(bool) }   // effective master = muted ? 0 : master
```

### `control`

```rust
pub struct ConfigSnapshot { /* … */ pub muted: bool, pub app: AppConfig }
pub struct AppConfig { pub autostart: bool, pub hotkeys: HotkeyMap }
pub struct HotkeyMap { pub mute_master: Option<HotkeyChord> }

pub fn group_id_for(snapshot: &ConfigSnapshot, name: &str) -> Option<GroupId>;  // GroupId = snapshot order

// control/store.rs
pub enum ConfigEdit {
    SetGroupGain(String, Gain), SetMaster(Gain), SetMuted(bool), SetFollowMaster(String, bool),
    SetGroupOutput(String, String), AddGroup(GroupConfig), RemoveGroup(String), SetRules(String, Vec<String>),
}
pub enum StoreError { Io(String), Validation(ConfigError) }

pub struct ConfigStore;
impl ConfigStore {
    pub fn open(path: &Path) -> Result<ConfigStore, StoreError>;
    pub fn apply(&mut self, edits: &[ConfigEdit]) -> Result<ConfigSnapshot, StoreError>;  // toml_edit, temp+rename
    pub fn is_echo(&self, snapshot: &ConfigSnapshot) -> bool;                             // own-write suppression
}
```

### `engine`

```rust
impl RoutingHandle { pub fn current_routes(&self) -> Vec<(GroupId, Vec<SessionInfo>)>; }
```

### `app` (internal)

```rust
pub enum ShellAction { EditParams(Vec<ConfigEdit>), EditStructure(Vec<ConfigEdit>), ShowSettings, Quit }
pub enum ShellError { Instance(String), Autostart(String), Hotkey(String) }

pub struct InstanceGuard;
impl InstanceGuard {
    pub fn acquire(app_id: &str) -> Result<Option<InstanceGuard>, ShellError>;  // None → other instance signaled, exit
}
pub fn set_autostart(enabled: bool) -> Result<(), ShellError>;
pub fn spawn_tray(actions: Sender<ShellAction>, notices: Receiver<EngineEvent>) -> TrayHandle;
pub fn spawn_hotkeys(map: &HotkeyMap, actions: Sender<ShellAction>) -> Result<HotkeyHandle, ShellError>;

pub struct UiState { pub snapshot: ConfigSnapshot, pub routes: Vec<(GroupId, Vec<SessionInfo>)>,
                     pub stats: EngineStats, pub routing_degraded: bool }
```

Dispatcher: `EditParams` → `MixerCommand`s via `group_id_for` → `apply_params` immediately + debounced `ConfigStore::apply`; `EditStructure` → `ConfigStore::apply` only (engine follows via watcher). Lifecycle via wrapper crates — no direct windows-rs in app.

## Open Questions

None — §15.1 toolkit resolved to egui (see Decisions Log).

## Constraints

Inherited (binding): UI never calls `win-audio` directly (spec §6.5); UI mutates config + sends commands only; COM in win-audio only; RT constraints unchanged (shell is control-plane).

P4-specific (spec §6.5, §11.1, F9–F11):
- Config is the single source of truth — UI edits write config; engine consumes validated snapshots; invalid edits rejected, previous snapshot retained.
- Single instance via named mutex; second launch surfaces existing instance.
- Autostart = per-user registration (no elevation).
- Signed binary (Authenticode) — N6, packaging concern noted, not designed here.

## Design Revisions (2026-07-18 cross-blueprint review)

```rust
// app — event fan-out (owns EngineHandle::take_events receiver)
pub struct EventPump;
impl EventPump {
    /// Fans out: tray notifications, UiState updates, routing.update_topology on structural events.
    pub fn spawn(events: Receiver<EngineEvent>, tray: Sender<EngineEvent>,
                 ui: Arc<Mutex<UiState>>, routing: RoutingHandle) -> PumpHandle;
}
pub fn spawn_tray(actions: Sender<ShellAction>, notices: Receiver<EngineEvent>) -> TrayHandle; // pump-fed

// app — instance signal listener (completes flow G)
pub enum InstanceOutcome { Primary(InstanceGuard, Receiver<SurfaceSignal>), Secondary }
impl InstanceGuard { pub fn acquire(app_id: &str) -> Result<InstanceOutcome, ShellError>; }
pub struct SurfaceSignal;

// audio-core — mute semantics: SetMuted(true) = output-stage kill for ALL groups
// (follow_master irrelevant to mute); gains untouched, restore on SetMuted(false).
```

Known limitation (documented): UI debounce window vs concurrent hand-edit of config.toml is last-writer-wins.

## Design Summary

- **Components/layers:** lifecycle host, tray surface, hotkey service, settings window (egui/eframe) — all `app` (shell); `ConfigStore` in `control/store.rs` (application).
- **Key contracts:** `ConfigEdit` semantic edit enum + `ConfigStore::{apply, is_echo}`; `ShellAction` dispatch with dual-path rule (params fast path via `apply_params` + debounced write; structure funnel-only); `MixerCommand::SetMuted`; `RoutingHandle::current_routes`; `InstanceGuard`/`set_autostart` via wrapper crates; `UiState` read model.
- **Architectural constraints:** UI never calls win-audio; no direct windows-rs in app (wrapper crates); config = source of truth (fast path is preview of same value); comment-preserving atomic writes; RT path untouched.
- **Domain decisions:** `muted` flag preserves master gain; `HotkeyChord` validated value object; GroupId positional from snapshot order.
- **Resolved during design:** §15.1 toolkit = egui; Win32-in-app via wrapper crates; toml_edit write strategy; L2 funnel-only revised to param fast path (L3 backtrack, logged); mute/autostart schema additions.
- Drift check vs `Splitstream-Engineering-Spec.md` complete — see spec `## Links`.

## Key Files

| Path | Role |
|---|---|
| Splitstream-Engineering-Spec.md | Requirement spec (§6.5, §11, §15.1, F9–F11) |
| .lattice/context/engine-core.md | Approved P0–P1 (EngineHandle, ConfigSnapshot, ConfigWatcher, diff) |
| .lattice/context/drift-and-recovery.md | Approved P2 (EngineEvent channel, EngineStats) |
| .lattice/context/session-routing.md | Approved P3 (RoutingHandle, ConfigDelta::Rules, RoutingDegraded) |
| crates/audio-core/src/mixer.rs | `MixerCommand::SetMuted` + `Mixer.muted` output-stage kill flag |
| crates/engine/src/graph.rs | `ConfigSnapshot.muted`/`.app`; `AppConfig`/`HotkeyMap`/`HotkeyChord` (relocated from `control`, see decision) |
| crates/engine/src/routing.rs | `RoutingHandle::current_routes`/`reader`; `RoutingReader` (new, read-only `Clone`-able view — see decision) |
| crates/control/src/config.rs | `parse` reads `muted`/`[app]`/`[hotkeys]`; `parse` made `pub(crate)` for `store.rs` reuse |
| crates/control/src/store.rs | `ConfigStore`/`ConfigEdit`/`StoreError`/`group_id_for` — toml_edit comment-preserving atomic writes, echo suppression |
| crates/app/src/lifecycle.rs | `InstanceGuard`/`InstanceOutcome`/`SurfaceSignal` (single-instance + `interprocess` local-socket signaling), `set_autostart` |
| crates/app/src/event_pump.rs | `EventPump`/`UiState`/`PumpHandle` — narrowed scope, see decision |
| crates/app/src/tray.rs | `spawn_tray`/`TrayHandle` — dedicated `tao` event-loop thread |
| crates/app/src/hotkeys.rs | `spawn_hotkeys`/`HotkeyHandle` — dedicated `tao` event-loop thread |
| crates/app/src/ui.rs | `SettingsApp` (egui/eframe) |
| crates/app/src/main.rs | `ShellAction`, `Dispatcher`, full startup/shutdown wiring; startup + dispatch run off the main thread (see OleInitialize decision) |
