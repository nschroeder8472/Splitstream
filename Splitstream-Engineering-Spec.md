# Splitstream — Engineering Specification

**Working codename:** Splitstream (final name TBD)
**Document status:** Draft v0.1
**Last updated:** 2026-07-11
**Owner:** NMS App Works
**Audience:** Implementing engineers, reviewers

---

## 1. Purpose

Splitstream is a lightweight Windows application that gives users multiple, independently-controlled audio buses ("groups"), each with its own volume, optional DSP, and its own physical output device — without any app taking exclusive control of a device. It is conceptually similar to SteelSeries Sonar / VoiceMeeter, but with a deliberately simpler mental model: the user thinks in terms of app-named groups, each routable to a chosen output.

This document specifies the architecture, component boundaries, runtime model, Windows integration surface, configuration, error handling, testing approach, and a phased delivery plan.

---

## 2. Goals and non-goals

### 2.1 Goals

- Route running applications into one of N logical **groups (buses)**.
- Per-group **volume**, independent of a **master** volume, with each group optionally *bound to* or *independent of* master.
- Per-group **output device selection** (e.g. Game → headset, Media → speakers), with multiple groups allowed to target the same device.
- **Non-exclusive** device access throughout — never open a physical endpoint in exclusive mode.
- Idle footprint low enough to run continuously from logon in the system tray.
- Optional per-group **DSP** (EQ, ducking, limiter) as a later phase.
- Robust operation over long sessions (hours) across device add/remove and format changes.

### 2.2 Non-goals (v1)

- Shipping our own kernel-mode virtual audio driver. v1 sits on top of an **existing signed virtual driver** (see §4.1). Building/signing our own driver is a separate, later track (§13, Phase 6).
- Pro-audio-grade sub-5 ms latency. Target is glitch-free shared-mode operation (~10–30 ms), not live monitoring for musicians.
- Cross-platform. Windows 10 (1803+) and Windows 11 only.
- Network/streaming features (exposing a mix as a capture device for OBS/Discord) — possible later, out of scope for v1.

---

## 3. Background and design constraints

Two Windows-audio facts drive the entire design. Implementers should internalize both before reading the component design.

### 3.1 The mix point

Windows mixes all audio sessions on a given render endpoint into a **single PCM stream** before any external code can read it. There is no supported per-session PCM capture — WASAPI loopback on an endpoint yields the already-mixed result. Consequence:

- Per-app **volume** can be controlled on a shared endpoint (session volume is applied *before* the mix).
- Per-app **output routing** and **per-group DSP** require the apps to be **separated onto different endpoints** *before* they mix.

**Therefore: the number of virtual endpoints equals the number of independently-processable groups.** This is why we use one virtual endpoint per group ("Model B"), accepting that Windows enumerates them as separate devices (we hide all but a branded default; see §9.4).

### 3.2 Clock domains and drift

Every WASAPI endpoint runs off an independent hardware clock. Capture endpoints and render endpoints will drift relative to one another over minutes, even when nominally the same sample rate. The engine must **decouple** capture from render via elastic buffers and continuously **resample** to compensate for drift. This is the single hardest subsystem and the difference between a router that runs 10 minutes and one that runs 10 hours (§7.3).

---

## 4. Requirements

### 4.1 Functional

| ID | Requirement |
|----|-------------|
| F1 | Discover all active render endpoints and classify each as a Splitstream **bus** or a **physical output**. |
| F2 | Capture the mixed PCM of each bus endpoint. |
| F3 | Apply per-group gain, optional DSP, and sample-rate conversion, then mix into per-output buffers. |
| F4 | Render each output buffer to its physical device in shared, event-driven mode. |
| F5 | Enumerate running audio sessions and map each to a group (by process name/PID rules). |
| F6 | Route a given app's session to its group's bus endpoint. |
| F7 | Hide non-default bus endpoints from the standard Windows device UI. |
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
| N4 | Graceful degradation when an undocumented API is unavailable (route best-effort, log, continue). |
| N5 | All `unsafe`/COM confined to a single crate; core DSP testable without Windows. |
| N6 | Signed binary (Authenticode) for SmartScreen; driver signing tracked separately. |

---

## 5. High-level architecture

Model B: one virtual endpoint per group. Apps route into their group's bus endpoint; the engine loopback-captures each bus, mixes/processes, and renders to physical outputs.

```
 App sessions ──route──▶ Bus endpoints ──loopback──▶ Input rings ──▶ Mixer+DSP ──▶ Output rings ──▶ Physical outputs
  (Game/Plex/Chat)        (capture ×N)   (SPSC/bus)   (vol·EQ·SRC)   (SPSC/output)   (render ×M)
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
│  │   ├─ enumerator.rs           #   IMMDeviceEnumerator → discover bus + physical endpoints
│  │   ├─ capture.rs              #   IAudioClient(loopback) + IAudioCaptureClient  (one per bus)
│  │   ├─ render.rs               #   IAudioClient + IAudioRenderClient (event-driven, shared)
│  │   ├─ sessions.rs             #   IAudioSessionManager2 + session notifications
│  │   ├─ router.rs               #   app→bus routing + endpoint visibility (undocumented APIs)
│  │   └─ mmcss.rs                #   AvSetMmThreadCharacteristics("Pro Audio")
│  │
│  ├─ engine/                     # orchestration — owns the graph, not the COM
│  │   ├─ graph.rs                #   bus → mixer → output wiring from config
│  │   ├─ clock.rs                #   per-endpoint clock tracking, drift → resample ratio
│  │   └─ runtime.rs              #   spawns capture/render/mixer threads, wires rings
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

- `EndpointEnumerator::list() -> Vec<Endpoint>` — `IMMDeviceEnumerator`; each `Endpoint` carries id, friendly name, data-flow, mix format, and a `kind: Bus | Physical` classification.
- `LoopbackCapture::open(bus: &Endpoint) -> CaptureStream` — `IAudioClient` with `AUDCLNT_STREAMFLAGS_LOOPBACK`. See §7 and Appendix A for the polling caveat.
- `Renderer::open(dev: &Endpoint) -> RenderStream` — `IAudioClient` shared + `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, `IAudioRenderClient`.
- `SessionMonitor` — `IAudioSessionManager2`, `IAudioSessionEnumerator`, session + new-session notifications; yields `(pid, process_path, display_name)`.
- `AppRouter` — `route(pid, bus_id)` and `set_endpoint_visible(id, bool)` / `set_default(id)`. Wraps the two undocumented surfaces in §9.3–9.4.
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

- **Capture thread per bus** (N): pulls loopback PCM, writes into that bus's input ring.
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

1. **Format discovery:** each endpoint's mix format via `IAudioClient::GetMixFormat` (commonly 32-bit float, 48 kHz, stereo). All internal processing is `f32`.
2. **Capture:** loopback yields the bus endpoint's mixed stream in its mix format. Convert to the internal `f32` layout if needed.
3. **Group processing:** apply group gain (with master applied per the bound/independent rule, §11.2), then DSP stages.
4. **SRC:** resample from the bus format to each target output's mix format, with the drift ratio applied.
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

### 9.3 Per-app → bus routing (F6)

Routing a specific app's default output to a specific endpoint uses the Windows 10 1803+ "app volume and device preferences" mechanism — an **undocumented** interface reached via activation of the internal `AudioPolicyConfig` class (`IAudioPolicyConfigFactory` → `SetPersistedDefaultAudioEndpoint(pid, flow, role, endpointId)`). Reference implementation: the open-source EarTrumpet project. There are no SDK headers; declare the vtable by hand in `router.rs`. Treat as best-effort with fallback and logging (N4).

### 9.4 Endpoint visibility / defaults (F7)

Hiding Splitstream's extra bus endpoints from the default device UI and setting the branded default uses the older undocumented `IPolicyConfig` COM object (`SetEndpointVisibility`, `SetDefaultEndpoint`). Also hand-declared. Same best-effort posture.

> **Risk note:** §9.3 and §9.4 are undocumented and can change across Windows builds. Isolate them, feature-gate them, and ensure the app still routes audio (just with less polish) if they fail.

### 9.5 The virtual driver (v1 dependency)

v1 does **not** ship a driver. It requires a bundled/detected existing signed virtual driver providing the per-group endpoints — e.g. VB-Audio's multi-channel matrix product or multiple VB-CABLE instances. The installer detects/installs it; Splitstream enumerates its endpoints as buses. Phase 6 (§13) replaces this with our own signed WDM driver if the product warrants it.

---

## 10. Error handling and resilience (N3, N4)

| Event | Handling |
|-------|----------|
| Physical device removed | Render thread for it exits cleanly; supervisor re-routes affected groups to a fallback (default device) and surfaces a tray notice. |
| Device added | Enumerator notification → offer as a routing target; no restart. |
| Endpoint format change | Render/capture returns `AUDCLNT_E_DEVICE_INVALIDATED`; supervisor rebuilds that stream with the new mix format and rebuilds SRC. |
| Bus (virtual driver) missing | Enter a degraded state; UI prompts to (re)install the virtual driver. |
| Undocumented routing API fails | Log, skip the routing polish, keep audio flowing (N4). |
| Xrun / ring underflow | Emit silence for the missing frames, bump a counter, let drift loop recenter; never block. |

Supervisor owns teardown/rebuild of any sub-graph so a single device fault never takes down the whole engine.

---

## 11. Configuration

### 11.1 Model

Config is a validated snapshot published to the engine. File watched via `notify` for hot-reload; invalid edits are rejected with the previous snapshot retained.

### 11.2 Master / group volume semantics

- `master`: linear gain applied to bound groups.
- Each group has `gain` and `follow_master: bool`.
- Effective group gain = `gain * master` if `follow_master`, else `gain`. Because groups have their own endpoints (Model B), an independent group's output genuinely does not change when master moves.

### 11.3 Schema (TOML example)

```toml
schema_version = 1
master = 0.8

[[group]]
name = "Game"
bus_endpoint = "Splitstream Bus 1"     # virtual endpoint friendly name / id
output_device = "Headset (USB)"        # physical target
gain = 1.0
follow_master = true
match = ["game.exe", "*Steam*"]        # session-matching rules → this group

  [[group.dsp]]
  type = "eq"
  bands = [{ freq = 60, gain_db = 2.0, q = 0.7 }]

[[group]]
name = "Media"
bus_endpoint = "Splitstream Bus 2"
output_device = "Speakers (Realtek)"
gain = 0.9
follow_master = false                  # stays put when master moves

[[group]]
name = "Chat"
bus_endpoint = "Splitstream Bus 3"
output_device = "Headset (USB)"        # shares Headset with Game → summed
gain = 1.0
follow_master = true

[hotkeys]
mute_master = "Ctrl+Alt+M"
```

---

## 12. Security and signing

- Ship the binary **Authenticode-signed** (EV or OV cert) to avoid SmartScreen friction.
- Memory safety (Rust) meaningfully shrinks the attack-surface class for an always-running background process.
- The undocumented COM interfaces run in the user's context; no elevation required for routing.
- **Driver track (Phase 6 only):** a kernel-mode virtual audio driver requires an EV code-signing certificate plus Microsoft attestation signing (or full WHQL). Budget cost and lead time; this is the single largest gate on shipping our own driver and is intentionally deferred.

---

## 13. Delivery phases

| Phase | Deliverable | Exit criteria |
|-------|-------------|---------------|
| **P0 — Loop** | Enumerate endpoints; one loopback capture → one render passthrough. | Audio from one bus plays through one physical device, stable for 10+ min. |
| **P1 — Groups** | N buses on the bundled virtual driver; mixer; per-group volume; per-group output routing. | 3 groups → 2 outputs with independent volume; no exclusive locks. |
| **P2 — Robustness** | Clock/drift loop; format-change + hotplug recovery; multi-output. | 8-hour soak with no drift-induced dropouts; survives unplugging a device. |
| **P3 — Routing** | Session enumeration; per-app→bus routing; endpoint hiding. | Apps auto-assign to groups by rule; extra devices hidden; graceful fallback verified. |
| **P4 — Shell** | Tray, hotkeys, settings UI, config hot-reload, autostart, single-instance. | Full user control surface; launches at logon; one instance enforced. |
| **P5 — DSP** | EQ, ducking, limiter; per-output headroom management. | DSP stages audibly correct; no clipping when groups share an output. |
| **P6 — Own driver (optional)** | Signed WDM virtual audio driver replacing the bundled one. | Our endpoints appear natively; EV + attestation signing complete. |

P0–P1 are the "prove the model" milestones; do not build UI before P1 audio is solid.

---

## 14. Testing strategy

- **Unit (CI, Linux):** `audio-core` mixer/DSP/resampler against known buffers; graph wiring logic in `engine` with mocked `win-audio` traits.
- **Integration (Windows runner):** endpoint enumeration, capture→render passthrough, format negotiation, device-invalidation recovery.
- **Soak:** multi-hour runs asserting zero xruns and bounded ring-fill drift; log the drift telemetry from §7.3.
- **Manual audio QA matrix:** real apps (a game, Plex, a chat client) × multiple output devices × hotplug events × sample-rate mismatches (44.1 vs 48 kHz sources).
- **Fault injection:** kill/unplug devices mid-stream; revoke the undocumented routing API (simulate failure) and confirm graceful degradation.

---

## 15. Open questions / decisions to make

1. **UI toolkit:** `egui`/`eframe` (fast, immediate-mode, easy) vs `slint` (more native look) vs Tauri (`wry`, web UI, heavier). Recommendation: `egui` for speed of iteration; revisit if the design needs a more native feel.
2. **Bundled virtual driver:** which product (VB-Audio matrix vs multiple VB-CABLE) — licensing terms for redistribution must be confirmed before P1.
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

## Appendix A — LoopbackCapture seam (skeleton)

Illustrative shape of the `win-audio/capture.rs` seam. Loopback capture is historically **unreliable in event-driven mode**, so the reference approach polls at roughly half the endpoint period and pushes frames into an `rtrb` producer. Error handling and format conversion elided.

```rust
// win-audio/capture.rs  — all unsafe COM confined here.
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use rtrb::Producer;

pub struct LoopbackCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    period: std::time::Duration,
}

impl LoopbackCapture {
    /// Open loopback capture on a *render* endpoint (a Splitstream bus).
    pub unsafe fn open(device: &IMMDevice) -> windows::core::Result<Self> {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;                 // endpoint mix format (usually f32/48k)

        // Shared mode + LOOPBACK. No event flag: we poll (see note above).
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            /*hnsBufferDuration*/ 0,
            0,
            mix,
            None,
        )?;

        let capture: IAudioCaptureClient = client.GetService()?;
        let period = /* derive from GetDevicePeriod() */ std::time::Duration::from_millis(10);
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

## Appendix B — Glossary

- **Bus / group:** a logical channel of grouped app audio with its own volume, DSP, and output.
- **Bus endpoint:** the virtual render device backing a bus; apps play into it, the engine loopback-captures it.
- **Loopback capture:** reading the mixed output of a render endpoint as a capture stream.
- **Mix point:** the moment Windows blends sessions on an endpoint; separation is impossible after it.
- **Drift:** divergence between independent endpoint clocks, corrected by variable-ratio SRC.
- **MMCSS:** Multimedia Class Scheduler Service; grants pro-audio scheduling priority to RT threads.
