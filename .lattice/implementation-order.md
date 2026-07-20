# Implementation Order

Blueprints in `.lattice/context/`, all `status: approved`. Implement in this order — each builds on the previous one's contracts. Inside-out within each: pure crates first, COM last.

**Before implementing any file, read the matching section of `.lattice/implementation-notes.md`** (cross-reference table at the bottom) — it contains required code patterns for the failure-prone spots.

1. **engine-core** (`engine-core.md`, P0–P1)
   1. `audio-core`: `Format`/`Gain`/ids, `Mixer`, `Src` — pure, unit-test on any platform
   2. `engine::ports` traits + mock implementations
   3. `engine`: graph build, runtime threads (timer-paced mixer), MPSC command queue, `Epoch`
   4. `control`: config load/validate (schema v2), `diff`, `ConfigWatcher`
   5. `win-audio`: COM guards, enumerator, loopback capture (polled), render (event-driven)
   6. `app`: minimal binary — P0 exit: one bus → one output, 10+ min stable; P1 exit: 3 groups → 2 outputs
2. **drift-and-recovery** (`drift-and-recovery.md`, P2)
   1. `Src::set_ratio` + `ResampleRatio` slewing
   2. `DriftController` (pure, synthetic-curve tests, idle guard)
   3. Device events port + `WasapiDeviceMonitor`; `take_events` handoff
   4. Recovery supervisor: fallback (reuse existing output), format-change rebuild, auto-restore
   5. Exit: 8-hour soak, survives unplug
3. **channel-mixdown** (`channel-mixdown.md`, gap fix — slots between P2 and P3; found smoke-testing P2 against real hardware, and P3+ builds on the widened `Format`)
   1. `ChannelLayout` + `Format.layout` field (`sample.rs`) — ripples through every `Format` literal (mocks, tests, win-audio); fix all sites in the same commit
   2. `ChannelMatrix` (`channel.rs`, pure, known-buffer tests — see notes §17 test list)
   3. Mixer integration: `push_group` matrix stage, `matrixed` scratch, `Src` at output channel count
   4. `win-audio` `client_mix_format()`: `dwChannelMask` probe with count fallback
   5. Exit: 8-ch bus (SteelSeries Sonar "Media") → stereo headphones plays; 5.1 movie source → dialogue audible, no clipping
4. **session-routing** (`session-routing.md`, P3)
   1. `RuleMatcher` (pure) + rules parsing/validation
   2. `SessionPort`/`PolicyPort` traits + mocks
   3. `RoutingCoordinator` (reconcile, degradation, `update_topology`)
   4. `win-audio`: `WasapiSessions`, `PolicyRouter` (feature-gated `policy-routing`)
   5. Exit: apps auto-assign by rule, buses hidden, degradation verified
5. **app-shell** (`app-shell.md`, P4)
   1. `control/store.rs`: `ConfigStore` (toml_edit, atomic, echo suppression)
   2. Lifecycle: `InstanceGuard`, autostart (wrapper crates)
   3. `EventPump` fan-out
   4. Tray + hotkeys (global mute)
   5. Settings UI (egui): faders (fast path), output pickers, group add/remove, routed apps
   6. Exit: full control surface, logon launch, single instance
6. **dsp-pipeline** (`dsp-pipeline.md`, P5)
   1. `DspStage` trait, `ParametricEq`, `Limiter` (pure, known-buffer tests)
   2. `DspChain` + swap-and-retire path
   3. Ducker (mixer-level sidechain) + duck-cycle validation
   4. Per-output headroom limiter + telemetry
   5. Config/UI integration for DSP edits
   6. Exit: stages audibly correct, no clipping on shared outputs

Deferred: P6 own driver (spec §13, separate signing track).
