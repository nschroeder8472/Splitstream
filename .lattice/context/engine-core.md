---
feature: engine-core
requirement_doc: Splitstream-Engineering-Spec.md
created: 2026-07-18
status: approved
---

# Engine Core (P0–P1)

> Audio engine core for Splitstream phases P0–P1: endpoint enumeration, loopback capture of bus endpoints, mixing with per-group gain, per-group output routing, and shared-mode render to physical devices.

## Decisions Log

<!-- Add new at bottom. Never remove. -->

| Date | Decision | Reasoning | Alternatives Considered |
|------|----------|-----------|------------------------|
| 2026-07-18 | Blueprint scope = P0–P1 engine core (enumeration, capture, mixer, routing, render). UI, DSP, drift loop, session routing deferred to later phases. | Spec §13: P0–P1 are "prove the model" milestones; no UI before P1 audio is solid. Hardest architectural decisions live here. | Whole-system blueprint (too shallow, duplicates spec); P0 only (defers mixer/graph decisions that shape everything). |
| 2026-07-18 | Port traits (capture/render/enumerate) live in `engine`, implemented by `win-audio`. **Design override of spec §6.2** (spec had traits in win-audio). | win-audio carries windows-rs; engine importing its traits would not compile on Linux, breaking N5/§6 cross-platform testability. Rust idiom: interface at consumer. | cfg-gating windows-rs inside win-audio (fragile); separate ports crate (5th crate for 3 traits, no second consumer). |
| 2026-07-18 | Keep `control` as its own crate per spec layout. | P4 tray UI will need config/command types without depending on engine. | Folding control into engine modules (simpler now, split later). |
| 2026-07-18 | Fixed-ratio SRC in P1 for format mismatches only; variable drift ratio deferred to P2. | F3 requires SRC when bus and output formats differ; drift loop is P2 per §13. | Requiring matching formats in P0–P1 (fails on common 44.1/48 kHz mismatch). |
| 2026-07-18 | Live-control split: param changes (gain/master/follow) via lock-free command ring to mixer; structural changes (group add/remove, output device change) via supervisor teardown+rebuild of affected sub-graph. | Mixer owning stream lifecycle would force alloc/blocking on RT thread (violates N2). Rebuild gap confined to affected group. | All changes via mixer commands (RT violation); full-engine restart on any change (needless global gap). |
| 2026-07-18 | Port surface: `AudioSystem` facade + `CapturePort::read(&mut [f32])` pull model instead of spec's per-struct `open()` + pump-into-Producer. | Keeps rtrb out of port contracts; mocks trivial; ring ownership stays in engine.runtime. | Spec Appendix A shape (capture pushes into rtrb Producer — couples port trait to ring library). |
| 2026-07-18 | Design approved at Level 4. Status set to approved — ready for implementation. | All four levels approved and persisted; drift check vs spec complete. | — |
| 2026-07-18 | **Revision (cross-blueprint review):** mixer thread is timer-paced (~half the minimum device period), not input-paced. Each tick drains whatever input rings hold and synthesizes silence for starved groups. | WASAPI loopback delivers zero packets on silent buses — input-paced mixer stalls when all groups silent → output starvation → drift ratio pegs. Timer pacing keeps output rings fed regardless of source activity. | Input-arrival pacing (stalls on silence); render-event pacing (couples mixer to one output's clock). |
| 2026-07-18 | **Revision (cross-blueprint review):** command channel = bounded lock-free MPSC queue (e.g. crossbeam `ArrayQueue`), pre-allocated. SPSC `rtrb` remains for PCM rings only. | Command producers number ≥3 by P5 (control thread, UI fast-path dispatcher, drift tick, supervisor swap) — SPSC contract was silently violated. MPSC push/pop stays lock-free + alloc-free after init. | All producers funneling through one thread into SPSC (extra hop + queue per producer); SPSC as designed (unsound with multiple producers). |
| 2026-07-18 | **Revision (cross-blueprint review):** topology `Epoch(u64)` introduced — bumped on every structural rebuild and chain swap. `EngineHandle` tags all commands with current epoch; mixer drops stale-epoch commands. | Positional GroupIds shift on group add/remove; in-flight fast-path (P4) and DSP (P5) commands could hit the wrong group mid-rebuild. Epoch check makes stale commands harmless. | Persistent group UUIDs (schema churn, rejected in P4); accepting the race (wrong-group volume/DSP changes). |
| 2026-07-18 | **Revision (cross-blueprint review):** `schema_version` bumped to 2 covering P4/P5 additions (`muted`, `[app]`, duck fields). Policy: missing fields get defaults; `schema_version` greater than supported → `ConfigError::Invalid`, prior snapshot retained. | Schema grew across P4/P5 with no versioning/compat policy anywhere. | Silent acceptance of unknown versions (undefined behavior on downgrade). |

## Design: Level 1 -- Capabilities

Approved 2026-07-18.

1. **Group routing works end-to-end** — app audio played into a group's bus endpoint emerges from that group's chosen physical output device.
2. **Independent volume per group** — each group has its own volume; master volume scales only groups bound to it (`follow_master`), independent groups unaffected.
3. **Shared outputs** — multiple groups may target the same physical device; their audio sums cleanly.
4. **Live control** — volume and routing changes apply while audio runs, without glitches or restart.
5. **Non-exclusive, stable operation** — devices never locked (other apps keep full access); passthrough stable for 10+ minutes (P0 exit criterion).

Out of scope: DSP, drift compensation, hotplug recovery, per-app session auto-routing, UI/tray, endpoint hiding.

## Design: Level 2 -- Components

Approved 2026-07-18.

| Component | Layer | Single responsibility | P0–P1 contents |
|---|---|---|---|
| `audio-core` | Domain | Pure sample processing — types + mixing math, no OS | `Format`, frame buffers, `Mixer` (per-group gain, master bind rule, group→output summing), fixed-ratio SRC (format mismatch only; drift ratio is P2) |
| `win-audio` | Infrastructure | Sole COM/WASAPI seam | COM guards, endpoint enumerator (bus vs physical classify), polled loopback capture, event-driven shared render, MMCSS promote |
| `engine` | Application | Graph orchestration — build wiring from config, spawn/supervise threads, own rings; **defines port traits** (capture/render/enumerate) | `graph`, `runtime`, `ports` |
| `control` | Application | Config + command plane | TOML load/validate → snapshot; file-watch hot-reload; diff → lock-free commands to mixer |
| `app` | Shell | Minimal dev binary | Load config, start engine, wait ctrl-c. Tray/UI = P4 |

```mermaid
graph TD
    APP[app - binary] --> ENG[engine - application]
    APP --> CTL[control - config+commands]
    CTL -->|command channel| ENG
    ENG --> AC[audio-core - domain, pure]
    WA[win-audio - infrastructure, COM] -->|implements engine ports| ENG
    APP --> WA
```

DDD: `Format`, `Gain`, endpoint/group IDs = validated immutable value objects; config snapshot = immutable value object; `Mixer` = domain service with routing table. No aggregates/persistence — DDD applied lightly.

## Design: Level 3 -- Interactions

Approved 2026-07-18.

**A — Startup:** app → control loads/validates TOML → `ConfigSnapshot` → engine.graph resolves names→endpoint ids via enumerator port → `GraphPlan` → runtime pre-allocates rings/mixer/SRC/command ring → spawns capture ×N, mixer ×1, render ×M (MMCSS-promoted) → opens streams via ports → start.

**B — Steady state (lock-free hot path):** capture thread polls loopback ~period/2 → interleaved f32 frames → input ring (SPSC) → mixer thread drains rings, applies gain·(master if bound) → SRC → sums into per-output accumulators → output rings → render thread on device event pulls one period → render buffer. Overflow → drop; underflow → silence + counter. Rings carry interleaved f32 only.

**C — Live control:** notify file event → control re-validates (invalid → keep old snapshot) → diff: param change (gain/master/follow) → `Command` on lock-free command ring → mixer applies between blocks, glitch-free; structural change (group add/remove, output device change) → supervisor teardown+rebuild of affected sub-graph only (brief gap on that group).

**D — Shutdown:** stop flags → threads exit → stop streams → join → drop rings.

**E — Device error (minimal):** port returns device-invalidated → thread signals supervisor + exits → supervisor tears down affected sub-graph, logs, rest keeps running. Full recovery/fallback = P2.

Decision: structural changes go through supervisor rebuild, not mixer commands — mixer owning stream lifecycle would violate RT no-alloc/no-block. Param changes only on the command ring.

## Design: Level 4 -- Contracts

Approved 2026-07-18. Rust signatures only; implementation belongs to code-forge.

### `audio-core` (domain)

```rust
pub struct Format { pub sample_rate: u32, pub channels: u16 }   // samples always f32 interleaved
pub struct Gain(f32);                                            // linear; finite, >= 0
impl Gain { pub fn new(v: f32) -> Result<Gain, DomainError>; }
pub struct GroupId(pub u16);
pub struct OutputId(pub u16);

pub struct GroupSpec { pub id: GroupId, pub gain: Gain, pub follow_master: bool, pub output: OutputId, pub input_format: Format }
pub struct OutputSpec { pub id: OutputId, pub format: Format }
pub struct Topology { pub master: Gain, pub groups: Vec<GroupSpec>, pub outputs: Vec<OutputSpec> }

pub enum MixerCommand { SetGroupGain(GroupId, Gain), SetMaster(Gain), SetFollowMaster(GroupId, bool) }

pub struct Mixer { /* per-group state, SRC per (group,output), output accumulators — pre-allocated */ }
impl Mixer {
    pub fn new(topology: &Topology, max_block_frames: usize) -> Result<Mixer, DomainError>;
    pub fn apply(&mut self, cmd: MixerCommand);
    pub fn push_group(&mut self, group: GroupId, frames: &[f32]);          // gain·(master if bound) → SRC → accumulate
    pub fn take_output(&mut self, output: OutputId, buf: &mut [f32]) -> usize;
}

pub struct Src { /* rubato wrapper, fixed ratio in P1; set_ratio arrives P2 */ }
impl Src {
    pub fn new(from: Format, to: Format, max_block_frames: usize) -> Result<Src, DomainError>;
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> SrcProgress; // {consumed, produced}
}
```

### `engine::ports` (engine-owned; win-audio implements)

```rust
pub struct EndpointId(pub String);
pub enum EndpointKind { Bus, Physical }
pub struct Endpoint { pub id: EndpointId, pub name: String, pub kind: EndpointKind, pub format: Format }
pub enum PortError { DeviceInvalidated, NotFound(EndpointId), Backend(String) }

pub trait AudioSystem: Send + Sync {
    fn enumerate(&self) -> Result<Vec<Endpoint>, PortError>;
    fn open_capture(&self, id: &EndpointId) -> Result<Box<dyn CapturePort>, PortError>;
    fn open_render(&self, id: &EndpointId) -> Result<Box<dyn RenderPort>, PortError>;
    fn promote_rt_thread(&self) -> RtGuard;          // MMCSS "Pro Audio"; no-op in mocks
}
pub trait CapturePort: Send {                        // polled ~period/2 (spec Appendix A)
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, PortError>;
    fn format(&self) -> Format;
    fn poll_interval(&self) -> Duration;
}
pub trait RenderPort: Send {                         // event-driven shared mode
    fn wait_event(&mut self, timeout: Duration) -> Result<(), PortError>;
    fn write(&mut self, frames: &[f32]) -> Result<(), PortError>;
    fn format(&self) -> Format;
    fn period_frames(&self) -> usize;
}
```

### `engine` (application)

```rust
pub enum EngineError { Resolve(String), Port(PortError), AlreadyStopped }

pub fn start(snapshot: &ConfigSnapshot, sys: Arc<dyn AudioSystem>) -> Result<EngineHandle, EngineError>;

impl EngineHandle {
    pub fn apply_params(&self, cmds: &[MixerCommand]) -> Result<(), EngineError>; // → lock-free command ring
    pub fn rebuild(&self, snapshot: &ConfigSnapshot) -> Result<(), EngineError>;  // structural: supervisor teardown+rebuild
    pub fn stats(&self) -> EngineStats;                                           // §7.3 telemetry from day one
    pub fn shutdown(self) -> Result<(), EngineError>;
}
pub struct EngineStats { pub xruns: u64, pub ring_fill: Vec<(OutputId, f32)>, pub group_faults: Vec<GroupId> }
```

### `control`

```rust
pub enum ConfigError { Io(String), Parse(String), Invalid(String) }   // invalid edit → keep prior snapshot

pub struct ConfigSnapshot { pub schema_version: u32, pub master: Gain, pub groups: Vec<GroupConfig> }
pub struct GroupConfig { pub name: String, pub bus_endpoint: String, pub output_device: String,
                         pub gain: Gain, pub follow_master: bool, pub match_rules: Vec<String> } // rules unused until P3

pub fn load(path: &Path) -> Result<ConfigSnapshot, ConfigError>;

pub enum ConfigDelta { Params(Vec<MixerCommand>), Structural, Unchanged }
pub fn diff(old: &ConfigSnapshot, new: &ConfigSnapshot) -> ConfigDelta;

pub struct ConfigWatcher;   // notify-based; control thread, never RT
impl ConfigWatcher { pub fn spawn(path: &Path) -> Result<(Self, Receiver<ConfigSnapshot>), ConfigError>; }
```

`win-audio` implements the port traits as `WasapiSystem` (all COM inside, safe public surface). `app` wires: `load` → `engine::start` → watcher loop → `diff` → `apply_params`/`rebuild` → ctrl-c → `shutdown`.

## Open Questions

- Ring library: `rtrb` vs `ringbuf` (spec §15.3 defaults `rtrb`).
- Mixer threading: single thread vs small pool (spec §15.4 says start single; measure).
- Bundled virtual driver product choice (spec §15.2) — licensing must be confirmed before P1.

## Constraints

- All `windows-rs` / COM confined to `win-audio` crate only; `audio-core` and `engine` graph logic compile and unit-test on any platform (spec §6, N5).
- Never open a physical endpoint in exclusive mode (spec §2.1, §7.4).
- RT threads never allocate, lock, block, or log-to-disk; PCM crosses threads only via SPSC lock-free rings; buffers pre-allocated at graph build (spec §7.2, N2).
- All internal processing is `f32` interleaved (spec §8).
- Loopback capture is polled (~period/2), not event-driven (spec Appendix A).
- Render devices set the pace — event-driven shared mode, render side is the pull clock (spec §7.1).

## Design Revisions (2026-07-18 cross-blueprint review)

Contract deltas from the five-blueprint seam review; re-approved same day.

```rust
// engine — topology epoch (stale-command safety across rebuilds and chain swaps)
pub struct Epoch(pub u64);
pub struct Envelope { pub epoch: Epoch, pub cmd: MixerCommand }   // internal wire format on the command queue
impl EngineHandle { pub fn epoch(&self) -> Epoch; }               // apply_params tags internally; mixer drops stale

// engine — command transport: bounded lock-free MPSC (crossbeam ArrayQueue), pre-allocated.
// rtrb SPSC remains for PCM rings only.

// engine — mixer thread: timer-paced tick at ~half the minimum device period.
// Each tick: drain input rings (non-blocking) → silence for starved groups → process → push output rings.

// control — schema_version = 2; missing fields default; version > supported → ConfigError::Invalid.
```

## Design Summary

- **Components/layers:** `audio-core` (domain, pure — Format/Gain/Mixer/Src), `engine` (application — ports, graph, runtime supervisor), `win-audio` (infrastructure — sole COM seam, `WasapiSystem`), `control` (application — config snapshot, watcher, diff), `app` (shell — minimal binary).
- **Key contracts:** `AudioSystem`/`CapturePort`/`RenderPort` port traits (engine-owned); `Mixer::{apply, push_group, take_output}`; `MixerCommand` enum; `ConfigDelta::{Params, Structural, Unchanged}`; `EngineHandle::{apply_params, rebuild, stats, shutdown}`; `PortError::DeviceInvalidated` as the supervisor trigger.
- **Architectural constraints:** COM confined to win-audio; RT threads never alloc/lock/block/log; SPSC f32 rings only; buffers pre-allocated at graph build; no exclusive device access; render side is pull clock; loopback polled.
- **Domain model:** value objects `Format`, `Gain`, `GroupId`, `OutputId`, `EndpointId`, immutable `ConfigSnapshot`/`Topology`; `Mixer` as domain service. No aggregates — systems domain.
- **Resolved during design:** trait home (engine, spec override); control stays its own crate; fixed-ratio SRC in P1; param-vs-structural live-control split; port surface as pull-model facade.
- Drift check vs `Splitstream-Engineering-Spec.md` complete — overrides recorded in spec `## Links`.

## Key Files

| Path | Role |
|---|---|
| Splitstream-Engineering-Spec.md | Requirement/engineering spec (source of constraints) |
