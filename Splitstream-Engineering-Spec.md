# Splitstream — Engineering Specification

**Working codename:** Splitstream (final name TBD)
**Document status:** Draft v0.1
**Last updated:** 2026-07-22 (body revised to match the process-loopback-capture pivot — see `## Links`)
**Owner:** NMS App Works
**Audience:** Implementing engineers, reviewers

---

## 1. Purpose

Splitstream is a lightweight Windows application that gives users multiple, independently-controlled audio buses ("groups"), each with its own volume, optional DSP, and its own physical output device — without any app taking exclusive control of a device. It is conceptually similar to SteelSeries Sonar / VoiceMeeter, but with a deliberately simpler mental model: the user thinks in terms of app-named groups, each routable to a chosen output.

This document specifies the architecture, component boundaries, runtime model, Windows integration surface, configuration, error handling, testing approach, and a phased delivery plan.

---

## 2. Goals and non-goals

### 2.1 Goals

- Route running applications into one of N logical **groups**.
- Per-group **volume**, independent of a **master** volume, with each group optionally *bound to* or *independent of* master.
- Per-group **output device selection** (e.g. Game → headset, Media → speakers), with multiple groups allowed to target the same device.
- **Non-exclusive** device access throughout — never open a physical endpoint in exclusive mode.
- Idle footprint low enough to run continuously from logon in the system tray.
- Optional per-group **DSP** (EQ, ducking, limiter) as a later phase.
- Robust operation over long sessions (hours) across device add/remove and format changes.

### 2.2 Non-goals (v1)

- Shipping our own kernel-mode virtual audio driver, or depending on anyone else's. v1 has no virtual-driver dependency of any kind — each app's audio is captured directly by process id via `ActivateAudioInterfaceAsync`/`PROCESS_LOOPBACK` (see §9.3). Building/signing our own driver is a separate, later track (§13, Phase 6), revisited only if a future need (e.g. exposing a mix as a capture device) actually requires one.
- Pro-audio-grade sub-5 ms latency. Target is glitch-free shared-mode operation (~10–30 ms), not live monitoring for musicians.
- Cross-platform. Windows 10 (1803+) and Windows 11 only.
- Network/streaming features (exposing a mix as a capture device for OBS/Discord) — possible later, out of scope for v1.

---

## 3. Background and design constraints

Two Windows-audio facts drive the entire design. Implementers should internalize both before reading the component design.

### 3.1 The mix point

Windows mixes all audio sessions on a given render endpoint into a **single PCM stream** before any external code can read it — plain endpoint loopback yields the already-mixed result, with no way to isolate one session's contribution from it.

Windows 10 2004+ (Build 19041+) exposes a separate, documented activation path that sidesteps this: `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` activates an `IAudioClient` scoped to one process id (optionally including its child process tree), independent of which endpoint that process is currently rendering to. This is genuinely per-session capture, not endpoint-mixed capture — it corrects an earlier, incorrect belief that no such API existed (v0.1 of this document assumed "Model B," one virtual endpoint per group, on that now-superseded premise; see the `## Links` history).

**Therefore: capture is per-process, not per-endpoint.** No virtual endpoints are created, hidden, or depended on (§9.3–9.5) — each matched app's audio is captured directly by pid.

### 3.2 Clock domains and drift

Every WASAPI endpoint runs off an independent hardware clock. Capture endpoints and render endpoints will drift relative to one another over minutes, even when nominally the same sample rate. The engine must **decouple** capture from render via elastic buffers and continuously **resample** to compensate for drift. This is the single hardest subsystem and the difference between a router that runs 10 minutes and one that runs 10 hours (§7.3).

---

## 4. Requirements

### 4.1 Functional

| ID | Requirement |
|----|-------------|
| F1 | Discover all active render endpoints as physical output candidates. |
| F2 | Capture each matched app's PCM directly, by process id. |
| F3 | Apply per-group gain, optional DSP, and sample-rate conversion, then mix into per-output buffers. |
| F4 | Render each output buffer to its physical device in shared, event-driven mode. |
| F5 | Enumerate running audio sessions and map each to a group (by process name/PID rules). |
| F6 | Capture a given app's session directly by process id, into its matched group. |
| F7 | *(eliminated — no virtual endpoints are created, so there is nothing to hide)* |
| F8 | Master volume control; each group either follows master or is independent. |
| F9 | Persist configuration; hot-reload on change. |
| F10 | System-tray control surface + global hotkeys + a settings window. |
| F11 | Launch at user logon; enforce single instance. |

### 4.2 Non-functional

| ID | Requirement |
|----|-------------|
| N1 | Idle CPU negligible; steady-state CPU dominated by SRC, low single-digit %. |
| N2 | No audio dropouts under normal desktop load; RT threads never block, allocate, or lock. |
| N3 | Survive device hotplug and endpoint format changes without a restart. |
| N4 | Graceful degradation when a pid's capture activation fails (permission denied, protected process) — isolated to that pid, log, continue. |
| N5 | All `unsafe`/COM confined to a single crate; core DSP testable without Windows. |
| N6 | ~~Signed binary (Authenticode) for SmartScreen~~ — overridden, v1 ships unsigned (§12); driver signing (moot, §9.5) tracked separately regardless. |

---

## 5. High-level architecture

Per-process capture: each matched app's session is captured directly by pid; no virtual endpoints exist. Capture streams belonging to the same group are summed, processed, and rendered to physical outputs.

```
 App sessions ──match──▶ Per-pid capture ──▶ Input rings ──▶ Mixer+DSP ──▶ Output rings ──▶ Physical outputs
  (Game/Plex/Chat)         (capture ×N)      (SPSC/pid)     (vol·EQ·SRC)   (SPSC/output)    (render ×M)
                                                                  ▲
                                                            Control plane
                                                    (sessions · config · UI commands)
```

- Capture side runs on the **capture clock(s)**; render side on the **render clock(s)**; the rings and mixer are clock-free elastic between them.
- The control plane never touches the audio path; it pushes parameters to the mixer over a lock-free command channel.

---

## 6. Component design (crate layout)

Cargo workspace. The hard architectural rule: **all `windows-rs` / COM lives in `win-audio` only.** `audio-core` and the engine's graph logic compile and unit-test on any platform.

```
splitstream/                      # cargo workspace
├─ crates/
│  ├─ audio-core/                 # NO windows-rs. Pure, testable DSP + buffers.
│  │   ├─ sample.rs               #   frame/format types, f32 interleaved buffers
│  │   ├─ mixer.rs                #   per-group gain, summing into output buses
│  │   ├─ dsp.rs                  #   EQ, ducking, limiter — trait-based stages
│  │   └─ resample.rs             #   rubato wrapper, variable-ratio drift correction
│  │
│  ├─ win-audio/                  # ← EVERY windows-rs seam is here, nowhere else
│  │   ├─ com.rs                  #   CoInitializeEx(MTA), apartment guards
│  │   ├─ enumerator.rs           #   IMMDeviceEnumerator → discover physical render endpoints
│  │   ├─ process_capture.rs      #   ActivateAudioInterfaceAsync(PROCESS_LOOPBACK) (one per matched pid)
│  │   ├─ render.rs               #   IAudioClient + IAudioRenderClient (event-driven, shared)
│  │   ├─ sessions.rs             #   IAudioSessionManager2 + session notifications
│  │   └─ mmcss.rs                #   AvSetMmThreadCharacteristics("Pro Audio")
│  │
│  ├─ engine/                     # orchestration — owns the graph, not the COM
│  │   ├─ graph.rs                #   group → output wiring from config (no bus resolution)
│  │   ├─ routing.rs              #   session match → live per-pid capture-source diffing
│  │   ├─ clock.rs                #   per-endpoint clock tracking, drift → resample ratio
│  │   └─ runtime.rs              #   spawns per-pid capture / render / mixer threads, wires rings
│  │
│  ├─ control/                    # config + session/command plane
│  │   ├─ config.rs               #   serde + toml, `notify` file-watch, hot-reload
│  │   └─ command.rs              #   lock-free command bus: UI/session events → engine
│  │
│  └─ app/                        # the only binary: tray + UI + lifecycle
│      ├─ main.rs                 #   process lifecycle, single-instance guard, autostart
│      ├─ tray.rs                 #   tray-icon + global-hotkey
│      └─ ui.rs                   #   settings window: faders, per-group output picker
└─ Cargo.toml
```

### 6.1 `audio-core` (pure Rust, no OS)

Owns sample/frame/format types and all signal processing. Receives `f32` interleaved frames, emits `f32` frames. Key types:

- `Format { sample_rate, channels, layout }`
- `Mixer` — holds per-group gain (linear) and the group→output routing table; sums groups into output accumulators.
- `DspStage` trait — `process(&mut self, buf: &mut [f32], fmt: Format)`; concrete: `Biquad`/`ParametricEq`, `Ducker`, `Limiter`.
- `DriftResampler` — wraps `rubato` async resampler; exposes `set_ratio(f64)` for drift correction.

**Testability:** feed known buffers, assert output. Runs on the CI Linux runner. No `windows-rs` in `[dependencies]`.

### 6.2 `win-audio` (the COM seam)

Every unsafe COM call is wrapped in a safe API here. Public surface (traits so the engine mocks them in tests):

- `EndpointEnumerator::list() -> Vec<Endpoint>` — `IMMDeviceEnumerator`; each `Endpoint` carries id, friendly name, and mix format — plain physical-output candidates, no classification needed.
- `ProcessCapture::open(pid, include_tree: bool) -> CaptureStream` — `ActivateAudioInterfaceAsync` + `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` (§9.3). See §7 and Appendix A for the polling caveat.
- `Renderer::open(dev: &Endpoint) -> RenderStream` — `IAudioClient` shared + `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, `IAudioRenderClient`.
- `SessionMonitor` — `IAudioSessionManager2`, `IAudioSessionEnumerator`, session + new-session notifications; yields `(pid, process_path, display_name)`.
- `mmcss::promote_current_thread()` — `AvSetMmThreadCharacteristicsW(L"Pro Audio")`.

### 6.3 `engine`

Reads config, builds the graph, spawns and supervises threads, wires rings, and runs the clock/drift loop. Depends on `win-audio` traits and `audio-core`; contains no COM itself. `runtime.rs` is the supervisor: on device error it tears down and rebuilds the affected sub-graph (§10).

### 6.4 `control`

- `config.rs`: load/validate TOML, watch file via `notify`, publish validated snapshots.
- `command.rs`: the lock-free command bus. UI actions, session events, and config reloads become `Command` messages delivered to the mixer without blocking RT threads.

### 6.5 `app`

Only binary. Owns process lifecycle (single-instance named mutex, autostart registration), the tray icon and hotkeys (`tray-icon`, `global-hotkey`), and the settings UI (`egui`/`eframe` or `slint` — decision in §15). UI mutates config and sends commands; it never calls into `win-audio` directly.

---

## 7. Runtime and threading model

### 7.1 Threads

- **Capture thread per matched pid** (N): pulls per-process loopback PCM, writes into that pid's input ring. Spawned/stopped live as sessions start/stop and rules re-match — not a fixed set from startup; multiple pids in the same group each get their own thread/ring and are summed before the mixer.
- **Mixer thread** (1, or a small pool): drains all input rings, applies group gain → DSP → SRC, sums into per-output accumulators, writes to output rings.
- **Render thread per physical output** (M): event-driven; on each device event, pulls from its output ring and writes to `IAudioRenderClient`. **The render devices set the pace** — they are the pull clock for their side.
- **Control thread** (1): session notifications, config file-watch, UI commands → `command.rs`.

All audio threads are promoted via MMCSS ("Pro Audio").

### 7.2 Inter-thread transport

- PCM between threads moves only through **single-producer/single-consumer lock-free ring buffers** (`rtrb`). No `Mutex` on the audio path.
- Parameters (gains, routing, DSP coefficients) reach the mixer via the command ring or atomics. **RT threads must never allocate, lock, block, or log-to-disk.** Pre-allocate all buffers at graph build time.

### 7.3 Clock and drift compensation

Because capture and render clocks are independent:

1. Each output ring has a target fill level (e.g. 50%).
2. The clock loop samples actual device rates (`IAudioClock`/`IAudioClock2` positions) and each ring's fill.
3. It computes a corrective ratio and calls `DriftResampler::set_ratio` so the resampler slowly speeds up or slows down to hold the ring centered.
4. Corrections are small and smoothed — never step the ratio abruptly (audible).

Treat this loop as first-class. Add telemetry (ring fill, applied ratio, xrun counts) behind a debug flag from day one; you will need it to tune.

### 7.4 Latency budget

Shared-mode, event-driven render period is typically ~10 ms. End-to-end (capture buffer + ring + render buffer) target ≤ ~30 ms. Optionally use `IAudioClient3::GetSharedModeEnginePeriod` for a tighter period on capable systems. Exclusive mode is explicitly disallowed (would lock the device and violate the non-exclusive requirement).

---

## 8. Audio signal flow (detail)

1. **Format discovery:** each render endpoint's mix format via `IAudioClient::GetMixFormat` (commonly 32-bit float, 48 kHz, stereo). All internal processing is `f32`. A process-loopback-activated `IAudioClient` does **not** support `GetMixFormat` (`E_NOTIMPL`) — its format (48 kHz/stereo/f32) is instead fixed and dictated by the caller at `Initialize` time (§9.3).
2. **Capture:** each matched process's audio is captured directly by pid, one capture stream per pid; multiple pids in the same group are summed before the next stage. Convert to the internal `f32` layout if needed.
3. **Group processing:** apply group gain (with master applied per the bound/independent rule, §11.2), then DSP stages.
4. **SRC:** resample from the fixed capture format to each target output's mix format, with the drift ratio applied.
5. **Mix:** sum all groups routed to a given output into that output's accumulator (with headroom/limiting to avoid clipping when multiple loud groups share an output).
6. **Render:** write to each output's `IAudioRenderClient` on its device event.

---

## 9. Windows integration surface

### 9.1 COM initialization

Audio worker threads call `CoInitializeEx(COINIT_MULTITHREADED)`; each thread that touches COM owns its apartment for its lifetime. Provide an RAII guard in `com.rs`.

### 9.2 Session enumeration (F5)

- `IAudioSessionManager2::GetSessionEnumerator` → iterate `IAudioSessionControl2`, read `GetProcessId` and process metadata.
- Register `IAudioSessionManager2::RegisterSessionNotification` for new sessions and per-session `RegisterAudioSessionNotification` for state changes.
- **Known gotcha:** new-session notifications often do not fire unless `GetSessionEnumerator` has been called at least once on that manager. Prime it during init.

### 9.3 Per-process capture (F6)

Each matched app's audio is captured directly, by process id, via `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` (Windows 10 Build 20348+, `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId, ProcessLoopbackMode }`). This is a **documented**, `windows-rs`-native API — no hand-declared vtables, no undocumented surface. The returned `IAudioClient` does not implement `GetMixFormat`; the caller dictates a fixed format at `Initialize` time instead (§8 step 1). A pid that fails to activate (permission denied, protected process) is excluded from that group only — isolated per pid, never a global degraded posture (§10).

Process-loopback capture is a **tap, not a redirect** — the source process keeps rendering to Windows normally alongside Splitstream's own copy. Left alone this double-plays whenever the group's output device is also Windows' current default. Splitstream mutes a session's own Windows volume (`ISimpleAudioVolume::SetMute`, another documented Core Audio interface) for exactly as long as that pid is actively captured — muted the moment capture is confirmed open, unmuted the moment it's released (rule change, session end) or on Splitstream's own clean shutdown. Best-effort, same isolation posture as activation failures (§10) — never blocks routing, never treated as a degraded condition. See `.lattice/context/session-mute-on-capture.md`.

### 9.4 Endpoint visibility (F7 — eliminated)

Not applicable. Process-loopback capture creates no virtual endpoints, so there is nothing to hide from the Windows device UI or to set as a branded default.

### 9.5 Virtual driver dependency (eliminated)

v1 has no virtual driver dependency, bundled or user-supplied. Capture is per-process (§9.3), not per-endpoint, so there is no virtual bus for a driver to provide. Phase 6 (§13) — a signed WDM driver — remains deferred/optional, revisited only if a future need (e.g. exposing a mix as a capture device, §15 open question #6) actually requires a new kind of endpoint.

---

## 10. Error handling and resilience (N3, N4)

| Event | Handling |
|-------|----------|
| Physical device removed | Render thread for it exits cleanly; supervisor re-routes affected groups to a fallback (default device) and surfaces a tray notice. |
| Device added | Enumerator notification → offer as a routing target; no restart. |
| Endpoint format change | Render returns `AUDCLNT_E_DEVICE_INVALIDATED`; supervisor rebuilds that stream with the new mix format and rebuilds SRC. |
| Process capture activation fails (pid) | That pid excluded from its group this attempt only; every other pid/group unaffected; retried on the next reconcile pass (no global degraded flag). |
| Matched process exits | Its capture thread stops; the group's remaining pids (if any) keep playing undisturbed; a relaunch is matched fresh. |
| Session mute/unmute call fails | Logged, isolated to that pid — never blocks reconcile, never a `RoutingDegraded` condition (cosmetic possible-double-audio, not a routing failure). Splitstream crashing (not a clean exit) while a still-running app is muted leaves it muted until manually un-muted (Volume Mixer) or until the app/Splitstream restarts and re-captures it — accepted, no persisted recovery state in v1. |
| Xrun / ring underflow | Emit silence for the missing frames, bump a counter, let drift loop recenter; never block. |

Supervisor owns teardown/rebuild of any sub-graph so a single device fault never takes down the whole engine.

---

## 11. Configuration

### 11.1 Model

Config is a validated snapshot published to the engine. File watched via `notify` for hot-reload; invalid edits are rejected with the previous snapshot retained.

### 11.2 Master / group volume semantics

- `master`: linear gain applied to bound groups.
- Each group has `gain` and `follow_master: bool`.
- Effective group gain = `gain * master` if `follow_master`, else `gain`. Because gain is applied per group, on that group's own summed capture, an independent group's output genuinely does not change when master moves.

### 11.3 Schema (TOML example)

```toml
schema_version = 2
master = 0.8
muted = false

[[group]]
name = "Game"
output_device = "Headset (USB)"        # physical target — no virtual bus involved
gain = 1.0
follow_master = true
match_rules = ["game.exe", "*Steam*"]  # session-matching rules → this group

  [[group.dsp]]
  type = "eq"
  bands = [{ freq = 60, gain_db = 2.0, q = 0.7 }]

[[group]]
name = "Media"
output_device = "Speakers (Realtek)"
gain = 0.9
follow_master = false                  # stays put when master moves

[[group]]
name = "Chat"
output_device = "Headset (USB)"        # shares Headset with Game → summed
gain = 1.0
follow_master = true

[[group]]
name = "Everything else"
output_device = "Speakers (Realtek)"
match_rules = ["*"]                    # catch-all: unmatched apps still get a destination

[app]
autostart = true

[hotkeys]
mute_master = "Ctrl+Alt+M"
```

---

## 12. Security and signing

- v1 ships the installer and binary **unsigned** (§13 N6 override) — OV/EV cert cost + recurring renewal disproportionate for a free, no-revenue OSS project; onboarding/README document the SmartScreen "More info → Run anyway" step. Revisit if the project gains funding.
- Memory safety (Rust) meaningfully shrinks the attack-surface class for an always-running background process.
- Process-loopback capture (§9.3) uses a documented Windows API and runs in the user's context; no elevation required.
- **Driver track (Phase 6 only, still deferred):** a kernel-mode virtual audio driver requires an EV code-signing certificate plus Microsoft attestation signing (or full WHQL). Budget cost and lead time; not needed for v1 at all (§9.5).

---

## 13. Delivery phases

| Phase | Deliverable | Exit criteria |
|-------|-------------|---------------|
| **P0 — Loop** | Enumerate physical outputs; one process capture → one render passthrough. | Audio from one process plays through one physical device, stable for 10+ min. |
| **P1 — Groups** | N groups, each capturing its matched processes directly by pid; mixer; per-group volume; per-group output routing. | 3 groups → 2 outputs with independent volume; no exclusive locks. |
| **P2 — Robustness** | Clock/drift loop; format-change + hotplug recovery; multi-output. | 8-hour soak with no drift-induced dropouts; survives unplugging a device. |
| **P3 — Routing** | Session enumeration; per-app match rules; live per-pid capture. | Apps auto-assign to groups by rule; per-pid activation failures degrade gracefully. |
| **P4 — Shell** | Tray, hotkeys, settings UI, config hot-reload, autostart, single-instance. | Full user control surface; launches at logon; one instance enforced. |
| **P5 — DSP** | EQ, ducking, limiter; per-output headroom management. | DSP stages audibly correct; no clipping when groups share an output. |
| **P6 — Own driver (optional, dropped)** | Not needed by v1's architecture at all (§9.5) — would only matter if a future need (e.g. exposing a mix as a capture device) requires a new kind of endpoint. | N/A — not planned. |

P0–P1 are the "prove the model" milestones; do not build UI before P1 audio is solid.

---

## 14. Testing strategy

- **Unit (CI, Linux):** `audio-core` mixer/DSP/resampler against known buffers; graph wiring logic in `engine` with mocked `win-audio` traits.
- **Integration (Windows runner):** endpoint enumeration, capture→render passthrough, format negotiation, device-invalidation recovery.
- **Soak:** multi-hour runs asserting zero xruns and bounded ring-fill drift; log the drift telemetry from §7.3.
- **Manual audio QA matrix:** real apps (a game, Plex, a chat client) × multiple output devices × hotplug events × sample-rate mismatches (44.1 vs 48 kHz sources).
- **Fault injection:** kill/unplug devices mid-stream; simulate a pid's capture activation failing (permission denied, protected process) and confirm it degrades only that pid, never the group or the engine.

---

## 15. Open questions / decisions to make

1. **UI toolkit:** `egui`/`eframe` (fast, immediate-mode, easy) vs `slint` (more native look) vs Tauri (`wry`, web UI, heavier). Recommendation: `egui` for speed of iteration; revisit if the design needs a more native feel.
2. **~~Bundled virtual driver~~** — moot. §9.5: v1 has no virtual driver dependency of any kind; capture is per-process (§9.3).
3. **Ring library:** `rtrb` (SPSC, minimal) vs `ringbuf` (more features). Default `rtrb`.
4. **Mixer threading:** single mixer thread vs a small pool keyed by output. Start single; measure.
5. **Session→group matching rules:** exact process name, path globs, or window title — define the precedence order.
6. **Do we ever need to expose a mix as a capture device** (OBS/Discord)? If yes, it re-enters scope and influences the driver decision in P6.

---

## 16. Dependencies

| Crate | Purpose |
|-------|---------|
| `windows` (windows-rs) | All WASAPI / COM bindings (in `win-audio` only) |
| `wasapi` | Optional higher-level WASAPI wrappers for capture/render plumbing |
| `rubato` | Asynchronous, variable-ratio sample-rate conversion (drift correction) |
| `rtrb` | Lock-free SPSC ring buffers for the RT audio path |
| `tray-icon` | System tray |
| `global-hotkey` | Global hotkeys |
| `serde` + `toml` | Config serialization |
| `notify` | Config file hot-reload |
| `tracing` | Structured logging (off the RT path) |
| `thiserror` / `anyhow` | Error types |
| `egui` / `eframe` (or `slint`) | Settings UI (pending §15.1) |

### Reference implementation to study

**CamillaDSP** (open source, Rust) performs almost exactly this capture → DSP → render loopback loop over WASAPI, and its author maintains `wasapi`, `rubato`, and the surrounding patterns. Read it before implementing P0–P2; it will save weeks of learning which WASAPI flag combinations actually work in practice.

---

## Appendix A — ProcessCapture seam (skeleton)

Illustrative shape of the `win-audio/process_capture.rs` seam (§9.3). Two things make this different from plain endpoint loopback: activation is scoped to a **process id**, not a device, via a genuinely **async** completion callback (bound the wait — don't block forever on a stuck activation); and the resulting `IAudioClient` does not support `GetMixFormat` (`E_NOTIMPL`) — the caller must dictate a fixed format at `Initialize` time instead. Like plain loopback, event-driven mode is historically unreliable, so this polls at roughly half the stream period and pushes frames into an `rtrb` producer. Error handling, the `PROPVARIANT`/`AUDIOCLIENT_ACTIVATION_PARAMS` blob construction, and format conversion elided — see `process_capture.rs` for the verified real shape.

```rust
// win-audio/process_capture.rs  — all unsafe COM confined here.
use windows::Win32::Media::Audio::*;
use rtrb::Producer;

pub struct ProcessCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    period: std::time::Duration,
}

impl ProcessCapture {
    /// Activate process-loopback capture for `pid` (optionally its child
    /// tree). Async: blocks the calling thread on a bounded wait for
    /// Windows's completion callback, then dictates a fixed WAVEFORMATEX
    /// (GetMixFormat is unsupported on this client type).
    pub fn open(pid: u32, include_tree: bool) -> windows::core::Result<Self> {
        // ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        //   &IAudioClient::IID, activation_params_blob(pid, include_tree), handler)
        // → wait_timeout on the completion condvar, then GetActivateResult.
        let client: IAudioClient = /* activate_process_loopback(pid, include_tree)? */ todo!();
        let wfx = fixed_capture_wfx(); // 48kHz/stereo/f32 — not queried, dictated

        // Shared mode + LOOPBACK + AUTOCONVERTPCM. No event flag: we poll.
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            /*hnsBufferDuration*/ 200_000,
            0,
            &wfx,
            None,
        )?;

        let capture: IAudioCaptureClient = client.GetService()?;
        let period = /* derive from GetBufferSize() / sample rate */ std::time::Duration::from_millis(10);
        client.Start()?;
        Ok(Self { client, capture, period })
    }

    /// Pump available frames into the ring. Call from a dedicated,
    /// MMCSS-promoted thread on a ~period/2 cadence. Never blocks on a lock.
    pub unsafe fn pump(&self, out: &mut Producer<f32>) -> windows::core::Result<()> {
        loop {
            let packet = self.capture.GetNextPacketSize()?;
            if packet == 0 { break; }

            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;

            // AUDCLNT_BUFFERFLAGS_SILENT → treat as zeros; else copy f32 samples.
            // Convert/interleave into the internal layout and push into `out`
            // (drop on overflow rather than block — the drift loop recenters).

            self.capture.ReleaseBuffer(frames)?;
        }
        Ok(())
    }
}
```

Real code must: register the thread with MMCSS before the loop; handle `AUDCLNT_E_DEVICE_INVALIDATED` by signalling the supervisor to rebuild; and convert from the endpoint format to the internal `f32` layout. Everything downstream of the `rtrb` producer is pure `audio-core` and needs no Windows to test.

---

## Links

- Design override: `§6.2 trait ownership` — changed from port traits defined in `win-audio` to traits defined in `engine::ports`, implemented by `win-audio`. Reason: `win-audio` carries `windows-rs`; engine importing its traits would break Linux CI builds required by §6/N5 (interface-at-consumer idiom).
- Design override: `Appendix A capture seam shape` — changed from `LoopbackCapture` pumping into an `rtrb::Producer` to `CapturePort::read(&mut [f32])` pull model behind an `AudioSystem` facade. Reason: keeps ring library out of port contracts; ring ownership stays in `engine::runtime`; mocks trivial.
- Design alignment: remaining L4 contracts (Mixer semantics §11.2, polled loopback, event-driven render, command plane §6.4, hot-reload §11.1) consistent with requirement spec — no further overrides. See `.lattice/context/engine-core.md`.
- Design override: `§7.3 drift input` — changed from sampling device rates (`IAudioClock`/`IAudioClock2`) + ring fill to ring-fill-only PI control. Reason: fill error is the held quantity, self-correcting, no port/COM surface growth; revisit only if 8h soak shows slow convergence (P2 design).
- Design override: `§10 device-added handling` — elaborated from "offer as a routing target" to auto-restore: a fallen-back group returns to its configured device automatically when it reappears. Reason: config is source of truth; replug UX. See `.lattice/context/drift-and-recovery.md`.
- Design alignment: remaining P2 contracts (smoothed corrections §7.3, supervisor rebuild §10, telemetry, fallback-to-default) consistent with requirement spec.
- Design override: `§15.5 session→group matching` — resolved from open question to: process image name + full path with glob syntax; precedence exact name > glob, ties by config order; window title excluded. Reason: titles volatile (rematch churn), spec config example already glob-shaped (P3 design).
- Design alignment: remaining P3 contracts (session enumeration §9.2 with priming, best-effort routing §9.3, visibility/default §9.4, degradation N4, feature-gating per §9 risk note) consistent with requirement spec. See `.lattice/context/session-routing.md`.
- Design override: `§15.1 UI toolkit` — resolved to `egui`/`eframe` per spec's own recommendation (P4 design).
- Design override: `§11.3 config schema` — added top-level `muted: bool` (effective master = muted ? 0 : master, preserves gain) and `[app]` table with `autostart: bool`. Reason: mute hotkey must not overwrite master gain; autostart user-controllable (F11).
- Design alignment: remaining P4 contracts (UI mutates config and sends commands §6.5 — param fast path + config funnel; single-instance/autostart F11; hot-reload §11.1 with echo suppression) consistent with requirement spec. See `.lattice/context/app-shell.md`.
- Design override: `§6.1 Ducker` — changed from a `DspStage` in the per-group chain to a mixer-level cross-group processor (trigger envelope followers → target gain reduction). Reason: sidechain spans groups; keeps `DspStage` single-group pure. Duck config lives target-side in `[[group.dsp]]` with `trigger = "<group>"`.
- Design alignment: remaining P5 contracts (`DspStage` trait shape §6.1, DSP placement after gain before SRC §8, per-output headroom limiting §8 step 5, `[[group.dsp]]` schema §11.3) consistent with requirement spec. See `.lattice/context/dsp-pipeline.md`.
- Design override: `§11.2 mute semantics` — mute hotkey silences ALL groups (output-stage kill flag), including `follow_master = false` groups; master and group gains untouched. Reason: a mute that leaves audio playing reads as a bug (cross-blueprint review).
- Design override: `§7.2 command transport` — parameters reach the mixer via a bounded lock-free MPSC queue (not an SPSC ring); SPSC `rtrb` remains for PCM only. Reason: ≥3 command producers by P5 (UI fast path, drift loop, supervisor swaps).
- Design override: `§8 signal flow` — added channel-matrix step between DSP and SRC: `ChannelMatrix` (ITU-R BS.775 N→M downmix/upmix, LFE discarded, unknown positions folded at −3 dB, rows normalized). Reason: spec pipeline assumed channel counts compatible; 8-ch bus → stereo output hard-failed (`DomainError::ChannelMismatch`, found in P2 hardware smoke test). See `.lattice/context/channel-mixdown.md`.
- Design alignment: `§6.1 Format` — `layout` field restored as `ChannelLayout` value object (WASAPI-mask-compatible); implementation had dropped it. Design now matches spec's original domain model (channel-mixdown design).
- Design override: `§6.1 key types` — `ChannelMatrix` added to audio-core's type list. Reason: N→M channel conversion is pure domain DSP (channel-mixdown design).
- Design elaboration (cross-blueprint review): mixer thread is timer-paced with silence synthesis for idle loopback buses (§7.1); topology `Epoch` guards in-flight commands across rebuilds/chain swaps; `schema_version` policy — v2 covers `muted`/`[app]`/duck fields, unknown newer versions rejected (§11.3). Details in the five `.lattice/context/*.md` revision sections.
- Design addition: `§2.1/§6.1 DSP scope` — HRTF virtual-surround rendering added beyond the spec's "EQ, ducking, limiter" set: `Render::Spatial(Spatializer)` as an alternative N→2 stage beside `ChannelMatrix` (fixed-virtual-speaker HRIR convolution, partitioned FFT, embedded public-domain set). Reason: fills the surround-on-headphones gap vs SteelSeries Sonar. See `.lattice/context/spatial-audio.md`.
- Design override: channel-mixdown's recorded "no synthetic surround / no HRTF" boundary — superseded: HRTF virtualization now in scope as a per-group opt-in alternative render path; the plain matrix path and all its decisions (BS.775, LFE drop) stand untouched. Spatial path diverges deliberately on LFE: mixed into both ears at −6 dB (spatial-audio design).
- Design addition: `§11.3 config schema` — per-group `spatial: bool` (default false) toggles virtualization; effective only when the group's output is stereo, silently falls back to matrix otherwise (spatial-audio design).
- Design override: `N6 / §13 Authenticode signing` — v1 ships the installer and binary **unsigned**; onboarding/README document the SmartScreen "More info → Run anyway" step. Reason: OV/EV cert cost + recurring renewal is the same overhead class that permanently dropped the P6 own-driver, disproportionate for a free, no-revenue OSS project. Revisit if the project gains funding. Packaging otherwise realizes P4's "launches at logon" via an Inno Setup machine-wide installer + per-user first-run (config in `%APPDATA%\Splitstream`, autostart owned by the app, not the elevated installer). See `.lattice/context/simple-launch.md`.
- Design override: `§15.2 bundled virtual driver` — resolved from open question to: no bundled/installed driver at all, BYOD. §9.5, §2.2, §10, §13 updated. Reason: rejected both for the licensing question that motivated the open question (VB-Audio's EULA restricts commercial/business use) and because Windows requires *some* signed driver to create a new audio endpoint — bundling one only relocates that cost, doesn't remove it (same reasoning that dropped P6). Free VB-CABLE recommended in onboarding docs as the default suggestion; anything already installed (Sonar, VoiceMeeter, an unused physical output) works identically since bus classification is by configurable name prefix, not a hardcoded vendor.
- Design override: `§6 "no supported per-session PCM capture"` — this stated fact is outdated/incorrect. `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` (Windows 10 2004+, documented, `windows-rs`-native bindings — verified against Microsoft Learn + windows-rs docs, not memory) captures one process's audio directly by PID via a different activation path than plain endpoint loopback. Model B (one virtual endpoint per group) is superseded, not merely alternatived. Reason: production instability in the BYOD/per-app-redirect implementation (undocumented `IPolicyConfig`/`AudioPolicyConfig` surfaces — bus `EndpointId` churn, silent per-app override failures) traced back to exactly the fragility §9's own risk note had already flagged; a documented API removes the whole risk class rather than working around it. See `.lattice/context/process-loopback-capture.md`.
- Design override: `F6` — changed from "route a given app's session to its group's bus endpoint" to "capture a given app's session directly by process id." No bus endpoint exists to route to.
- Design override: `F7` — eliminated. No virtual endpoints are created, so there is nothing to hide from the Windows device UI.
- Design override: `§9.5` — eliminated. No virtual driver dependency of any kind (not even user-supplied/BYOD) — supersedes the `§15.2` override immediately above, which only removed the *bundled* driver, not the driver dependency itself.
- Design override: `§9.3`/`§9.4` — eliminated, including their entire risk note ("undocumented and can change across Windows builds"). Replaced by a single documented, bindgen'd Windows API (`ActivateAudioInterfaceAsync`), removing the undocumented-surface risk class rather than isolating/feature-gating it.
- Design alignment: `F5`/`§9.2` (session enumeration, per-app match rules by process name/path) — unchanged, still accurate. See `.lattice/context/process-loopback-capture.md`.
- Design alignment: `§15.1` UI toolkit unchanged (`egui`/`eframe`); settings-window layout/interaction model (columns, dropdown-backed pickers, drag-assign) has no prior spec text to diverge from — pure elaboration on app-shell.md's approved F9/§6.5 contracts. See `.lattice/context/mixer-ui-redesign.md`.
- Design addition: `§9.3` gains session-mute-on-capture — found live (process-loopback capture is a tap, not a redirect, so a captured session's own Windows output kept playing unmuted, double-audio whenever the group's output device is also Windows' default). Splitstream now mutes (`ISimpleAudioVolume::SetMute`) a session's own volume for exactly as long as it's actively captured, unmuted on release or clean shutdown. `§10` gains a matching error-table row. No existing spec text overridden — nothing previously said the source session would be left alone, this fills a genuine gap. See `.lattice/context/session-mute-on-capture.md`.

---

## Appendix B — Glossary

- **Group:** a logical channel of grouped app audio with its own volume, DSP, and output — apps are captured into it directly by pid, not routed through any virtual device.
- **Process-loopback capture:** capturing one process's audio directly via `ActivateAudioInterfaceAsync` + `PROCESS_LOOPBACK`, independent of which endpoint it renders to (§9.3).
- **Mix point:** the moment Windows blends sessions *on a shared endpoint*; process-loopback capture reads a session before that mix happens, so it isn't subject to this.
- **Drift:** divergence between independent endpoint clocks, corrected by variable-ratio SRC.
- **MMCSS:** Multimedia Class Scheduler Service; grants pro-audio scheduling priority to RT threads.
