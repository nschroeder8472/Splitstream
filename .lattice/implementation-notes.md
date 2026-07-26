# Implementation Notes — Code Samples for Tricky Spots

Companion to `.lattice/implementation-order.md`. Each section says WHERE the code belongs and WHY it must look this way.
These are the places where a naive implementation compiles, works for 10 minutes, then fails in the field.
Read the section for a file BEFORE implementing that file.

---

## 1. RT thread rules (applies to ALL audio threads)

**Where:** every thread spawned by `engine/runtime.rs` (capture ×N, mixer, render ×M).
**Why:** one hidden allocation or lock = eventual audible dropout (N2).

Forbidden on RT threads — check every line you write there:

```rust
// ❌ NEVER on capture/mixer/render threads:
vec.push(x);            // may reallocate
format!(...); println!; tracing::info!;   // allocates + may take locks/syscalls
mutex.lock();           // priority inversion
Box::new(x);            // allocates (moving an EXISTING Box is fine — pointer move)
channel.send(x)         // std mpsc Sender allocates nodes — use ArrayQueue/rtrb only
std::thread::sleep      // only the mixer tick sleeps, via spin_sleep (see §5)

// ✅ Allowed:
ring.push(x).ok();      // rtrb / crossbeam ArrayQueue: pre-allocated, lock-free; IGNORE full-queue errors (drop, count via atomic)
counter.fetch_add(1, Ordering::Relaxed);   // telemetry via atomics
buf[i] = x;             // writes into buffers pre-allocated at graph build
```

Telemetry pattern: RT thread bumps `AtomicU64`/`AtomicU32`; control thread reads and logs. Never log from RT.

---

## 2. COM apartment guard

**Where:** `crates/win-audio/src/com.rs`. Every win-audio thread that touches COM calls this FIRST and holds the guard for the thread's whole life.

```rust
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

pub struct ComGuard(());   // !Send by design — guard must not leave its thread

impl ComGuard {
    pub fn init_mta() -> windows::core::Result<Self> {
        // S_FALSE (already initialized) is OK — still must balance with CoUninitialize.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(ComGuard(()))
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) { unsafe { CoUninitialize() } }
}
// GOTCHA: all COM interface pointers created on this thread must be dropped
// BEFORE the ComGuard drops. Order struct fields so interfaces drop first,
// or drop() them explicitly before the guard goes out of scope.
```

---

## 3. Loopback capture pump (polled — NOT event-driven)

**Where:** `crates/win-audio/src/capture.rs`.
**Why:** loopback event mode is historically unreliable (spec Appendix A). Poll at ~period/2. Three gotchas: the SILENT flag, packet absence on idle buses, device invalidation.

```rust
// Called in a loop from the capture thread: pump(); spin_sleep(period/2);
pub unsafe fn pump(&mut self, out: &mut rtrb::Producer<f32>, dropped: &AtomicU64) -> Result<(), PumpError> {
    loop {
        let frames_avail = self.capture.GetNextPacketSize().map_err(map_invalidated)?;
        if frames_avail == 0 { break; }   // NORMAL when bus is silent — do NOT treat as error.
                                          // Mixer synthesizes silence (engine-core revision) — capture just returns.
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).map_err(map_invalidated)?;

        let n = frames as usize * self.channels as usize;
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
            // SILENT: data pointer may be garbage — write zeros, do not read it.
            push_or_drop(out, ZeroIter(n), dropped);
        } else {
            // Mix format is float32 interleaved (we verified at open — see §note below).
            let samples = std::slice::from_raw_parts(data as *const f32, n);
            push_or_drop(out, samples.iter().copied(), dropped);
        }
        self.capture.ReleaseBuffer(frames).map_err(map_invalidated)?; // ALWAYS release, even after ring-full drop
    }
    Ok(())
}

fn map_invalidated(e: windows::core::Error) -> PumpError {
    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED { PumpError::DeviceInvalidated } else { PumpError::Other(e) }
}
// PumpError::DeviceInvalidated → thread signals supervisor (fault channel) and exits. Never retry in-place.
// push_or_drop: if ring full, DROP remaining samples and bump `dropped` — never block, never spin.
```

**Format note:** at `open()`, check `GetMixFormat()` — if `WAVEFORMATEXTENSIBLE`, require `SubFormat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`. Shared-mode mix format is float32 in practice; if it ever isn't, fail `open()` loudly rather than reinterpreting bytes.

---

## 4. Render loop (event-driven, shared mode)

**Where:** `crates/win-audio/src/render.rs`.

```rust
// Init: eventflag + shared mode, then SetEventHandle BEFORE Start.
client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, mix, None)?;
client.SetEventHandle(event)?;   // forgetting this = Initialize succeeds, no events ever fire

// Per event:
pub unsafe fn write_period(&mut self, ring: &mut rtrb::Consumer<f32>, xruns: &AtomicU64) -> Result<(), PumpError> {
    // GetCurrentPadding = frames already queued; only write the free space.
    let padding = self.client.GetCurrentPadding().map_err(map_invalidated)?;
    let free = self.buffer_frames - padding;
    if free == 0 { return Ok(()); }

    let buf = self.render.GetBuffer(free).map_err(map_invalidated)?;   // *mut u8
    let dst = std::slice::from_raw_parts_mut(buf as *mut f32, free as usize * self.channels as usize);

    let got = pop_available(ring, dst);      // copy what the ring has
    if got < dst.len() {
        dst[got..].fill(0.0);                // UNDERFLOW: pad with silence — never wait for the mixer
        xruns.fetch_add(1, Ordering::Relaxed);
    }
    self.render.ReleaseBuffer(free, 0)?;     // flags=0; do NOT use AUDCLNT_BUFFERFLAGS_SILENT here (we filled real data)
    Ok(())
}
// Wait: WaitForSingleObject(event, timeout). Timeout ~2× period → treat as fault, signal supervisor.
```

---

## 5. Windows timer granularity — the mixer tick

**Where:** `crates/engine/src/runtime.rs` (mixer thread pacing).
**Why:** `std::thread::sleep(Duration::from_millis(5))` on Windows actually sleeps ~15.6 ms (default timer resolution). The timer-paced mixer (engine-core revision) dies on this.

```rust
// Use the spin_sleep crate — hybrid sleep+spin, no windows-rs needed, keeps the
// windows-rs-only-in-win-audio constraint intact.
let sleeper = spin_sleep::SpinSleeper::default();
loop {
    let t0 = std::time::Instant::now();
    mixer_tick(...);                         // drain rings → silence-fill starved groups → process → push
    let budget = tick_period.saturating_sub(t0.elapsed());
    sleeper.sleep(budget);                   // accurate to ~µs; do NOT use thread::sleep here
}
// tick_period = half the minimum device period across the graph (typically ~5 ms).
```

---

## 6. Rings + fill telemetry

**Where:** `crates/engine/src/runtime.rs`. PCM rings are `rtrb` (SPSC). Fill level for the drift loop comes from atomics the RT threads maintain — `rtrb`'s `slots()` is only callable from the owning side.

```rust
// Shared per-output gauge, written by render thread each event, read by DriftController:
pub struct RingGauge { pub fill_permille: AtomicU32, pub active: AtomicBool }

// render thread, each event (both are cheap):
gauge.fill_permille.store((queued * 1000 / capacity) as u32, Ordering::Relaxed);
// mixer tick sets active=true when it pushed real (non-synthesized) frames for this output
// in the last N ticks; false otherwise. DriftController freezes integrator when !active (P2 revision).
```

Ring sizing: capacity = 4× the largest period involved, target fill 50%. Too small = xruns on scheduling jitter; too large = latency.

---

## 7. Command queue, Epoch, swap-and-retire

**Where:** queue in `crates/engine/src/runtime.rs`; apply in `crates/audio-core/src/mixer.rs`.
**Why:** ≥3 producers (UI, drift tick, supervisor) → MPSC `ArrayQueue`, NOT rtrb. Stale commands across rebuild/chain-swap must be dropped (Epoch). Retired chains must not be dropped on the RT thread.

```rust
// engine side
pub struct Envelope { pub epoch: Epoch, pub cmd: MixerCommand }
static COMMANDS: ArrayQueue<Envelope>;        // bounded, pre-allocated; push from any thread
static RETIRED: ArrayQueue<Box<DspChain>>;    // mixer → supervisor; supervisor drops (dealloc off RT)

// mixer thread, START of every tick:
while let Some(env) = commands.pop() {
    if env.epoch != current_epoch { continue; }          // stale after rebuild/swap — silently drop
    if let Some(old_chain) = mixer.apply(env.cmd) {      // SwapChain returns the retired Box
        if env_matches_swap { current_epoch.bump(); }    // epoch bumps on swap apply too (P5 revision)
        let _ = retired.push(old_chain);                 // pointer move; if full, supervisor drains next cycle
    }
}
// Box<DspChain> MOVE through the queue = pointer copy, no alloc. Building the Box happens
// on the supervisor thread. Dropping it happens on the supervisor thread. Never mem::drop a
// chain inside mixer code.
```

---

## 8. Parameter smoothing — zipper noise

**Where:** `crates/audio-core/src/mixer.rs` (gains) and `dsp.rs` (bypass, thresholds).
**Why:** applying a new gain instantly = audible "zipper" click. EVERY audible parameter ramps.

```rust
pub struct Smoothed {
    current: f32,
    target: f32,
    coeff: f32,   // one-pole: coeff = exp(-1.0 / (time_constant_s * sample_rate)), tc ~5–15 ms
}
impl Smoothed {
    pub fn set_target(&mut self, t: f32) { self.target = t; }        // called on command apply
    #[inline] pub fn next(&mut self) -> f32 {                        // called PER SAMPLE (or per small sub-block)
        self.current = self.target + self.coeff * (self.current - self.target);
        self.current
    }
}
// Usage in mix loop: for each frame: let g = gain.next(); out[i] = in[i] * g;
// Same struct drives: group gain, master, mute (target 0.0/1.0 — global mute IS a Smoothed at output stage),
// bypass wet/dry (0.0..1.0 crossfade), limiter ceiling, duck gain reduction.
// Biquad coefficient changes: recompute coefficients from smoothed *parameters* per sub-block (e.g. every 32
// frames), not per sample; never interpolate raw biquad coefficients independently (can go unstable).
```

---

## 9. Biquad EQ + denormals

**Where:** `crates/audio-core/src/dsp.rs`.

```rust
// Coefficients: RBJ Audio EQ Cookbook peaking filter. Compute in f64, store f32.
// (Formulas: w0 = 2π·freq/fs, alpha = sin(w0)/(2Q), A = 10^(gain_db/40) — follow the cookbook exactly.)

pub struct Biquad { b0: f32, b1: f32, b2: f32, a1: f32, a2: f32, z1: f32, z2: f32 }
impl Biquad {
    #[inline] pub fn process(&mut self, x: f32) -> f32 {
        // Transposed Direct Form II — best numerical behavior in f32
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}
// DENORMALS: when input goes silent, z1/z2 decay into denormal range → CPU spikes 10–100×.
// Fix: flush at block end (cheap, portable):
#[inline] fn flush(v: f32) -> f32 { if v.abs() < 1.0e-15 { 0.0 } else { v } }
// After each block: self.z1 = flush(self.z1); self.z2 = flush(self.z2);
// One filter instance PER CHANNEL — never share z-state across interleaved channels.
// Deinterleave into per-channel scratch (pre-allocated) or index-stride carefully.
```

---

## 10. Drift PI controller — anti-windup + idle freeze

**Where:** `crates/engine/src/clock.rs`. Pure — unit-test with synthetic fill curves before touching hardware.

```rust
pub fn tick(&mut self, fills: &[(OutputId, FillSample)]) -> Vec<MixerCommand> {
    let mut cmds = Vec::new();                       // control thread — allocation fine here
    for (id, s) in fills {
        let st = self.state.get_mut(id);
        if !s.active { continue; }                   // idle: freeze integrator, hold last ratio (P2 revision)
        let err = s.fill - self.cfg.target_fill;     // e.g. fill 0.62, target 0.5 → err 0.12
        st.integ += err * self.cfg.tick_secs;
        let raw = self.cfg.kp as f32 * err + self.cfg.ki as f32 * st.integ;
        let corr = raw.clamp(-self.cfg.max_correction as f32, self.cfg.max_correction as f32);
        if raw != corr { st.integ -= err * self.cfg.tick_secs; }   // ANTI-WINDUP: don't integrate while clamped
        let ratio = 1.0 + corr as f64;               // ring too full → ratio > 1 → resampler consumes faster
        cmds.push(MixerCommand::SetOutputRatio(*id, ResampleRatio::new(ratio).unwrap()));
    }
    cmds
}
// Start kp ≈ 0.05, ki ≈ 0.01, max_correction = 0.005. Verify sign convention with a unit test:
// constant over-full ring MUST converge fill → target without oscillating.
```

---

## 11. Resample ratio slewing

**Where:** `crates/audio-core/src/resample.rs`.

```rust
// set_ratio stores a target; the ACTUAL ratio glides toward it per process() call.
// Never pass a stepped ratio straight into rubato — steps are audible as pitch ticks.
pub fn set_ratio(&mut self, target: ResampleRatio) { self.target = target.value(); }

fn process(&mut self, input: &[f32], output: &mut [f32]) -> SrcProgress {
    self.current += 0.05 * (self.target - self.current);   // one-pole glide per block
    self.inner.set_resample_ratio_relative(self.current, /*ramp=*/true).expect("ratio in clamp range");
    // rubato FftFixedIn/SincFixedIn want deinterleaved &[Vec<f32>] — deinterleave into
    // PRE-ALLOCATED scratch buffers (self.scratch_in/out), never allocate per block.
    ...
}
```

---

## 12. Duck envelope follower

**Where:** `crates/audio-core/src/mixer.rs` (mixer-level, NOT a DspStage — P5 decision).

```rust
pub struct EnvFollower { env: f32, attack: f32, release: f32 }   // coeffs from ms as in §8
impl EnvFollower {
    pub fn process_block(&mut self, buf: &[f32]) -> f32 {        // returns envelope in dBFS
        for &x in buf {
            let a = x.abs();
            let c = if a > self.env { self.attack } else { self.release };
            self.env = a + c * (self.env - a);
        }
        20.0 * (self.env.max(1.0e-6)).log10()                    // floor avoids log(0) = -inf
    }
}
// Order inside one mixer tick (deterministic, no feedback — P5 decision):
// 1. ALL groups: gain → chain
// 2. ALL trigger envelopes: process_block on post-chain buffers
// 3. ALL targets: duck_gain_db = if trigger_env > threshold { -amount_db } else { 0 };
//    apply via a Smoothed (§8) with the duck's attack/release — then SRC → sum.
```

---

## 13. Undocumented COM — setting the Windows default endpoint

**Where:** `crates/win-audio/src/policy.rs` (no feature gate — double-audio-prevention
capability 4 depends on it unconditionally).
**Why:** Windows exposes no supported way to change the default playback device.
**Superseded:** the old per-app-routing/endpoint-hiding sketch that lived here declared
**2** methods and put `SetDefaultEndpoint` first. The real interface has **12** own
vtable slots with `SetDefaultEndpoint` at **11** — calling the old sketch's first slot
invokes `Unused1`, and a skipped slot shifts every later method: memory corruption, not
an error code. The note carried its own "⚠ VERIFY" marker and was trusted anyway once
already. Both GUIDs and the slot order below were re-derived 2026-07-26 from EarTrumpet's
`Interop/MMDeviceAPI/IPolicyConfig.cs` and `PolicyConfigClient.cs`.

```rust
// VERIFIED 2026-07-26 against EarTrumpet source — this is the real layout, not a sketch.
#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]     // IPolicyConfig (Win7+)
unsafe trait IPolicyConfigWin7: IUnknown {
    unsafe fn _unused1(&self) -> HRESULT;                //  1..8: unused, MUST be declared
    unsafe fn _unused2(&self) -> HRESULT;
    unsafe fn _unused3(&self) -> HRESULT;
    unsafe fn _unused4(&self) -> HRESULT;
    unsafe fn _unused5(&self) -> HRESULT;
    unsafe fn _unused6(&self) -> HRESULT;
    unsafe fn _unused7(&self) -> HRESULT;
    unsafe fn _unused8(&self) -> HRESULT;
    unsafe fn _get_property_value(&self) -> HRESULT;     //  9
    unsafe fn _set_property_value(&self) -> HRESULT;     // 10
    unsafe fn set_default_endpoint(&self, id: PCWSTR, role: i32) -> HRESULT;  // 11 — the only one called
    unsafe fn _set_endpoint_visibility(&self) -> HRESULT;// 12
}
// CPolicyConfigClient CLSID: 870af99c-171d-4f9e-af0d-e63df40c2bc9
// Call once per role: eConsole, eMultimedia, eCommunications. Skipping eCommunications
// leaves Discord-class apps rendering to the real device — the exact double this removes.
// EVERY call: HRESULT != S_OK -> PortError::Backend. NEVER panic, never retry.
// Every role is attempted even after one fails; the first error is returned at the end.
```

**Gone with the tap-vs-transport reframing:** `IAudioPolicyConfigFactory` /
`SetPersistedDefaultAudioEndpoint` (per-app redirect) and endpoint hiding. With the
default pointed at one unheard sink, no per-app endpoint assignment is needed at all.

---

## 14. Session notifications — priming + callback discipline

**Where:** `crates/win-audio/src/sessions.rs`.

```rust
// GOTCHA 1 (spec §9.2): new-session notifications DO NOT FIRE unless GetSessionEnumerator
// was called at least once on that IAudioSessionManager2. Call it during init even if you
// discard the result.
let _prime = manager.GetSessionEnumerator()?;
manager.RegisterSessionNotification(&callbacks)?;
// GOTCHA 2: keep the callback object (and the manager) ALIVE for the whole app lifetime —
// store them in the WasapiSessions struct. Dropping them silently stops notifications.
// GOTCHA 3: callbacks arrive on COM worker threads. Do NOTHING in the callback except
// extract (pid, path, name) and push a SessionEvent into the channel. No locks shared
// with your own threads, no COM calls back into the manager from inside the callback (deadlock risk).
// GOTCHA 4: OnSessionCreated gives IAudioSessionControl — QueryInterface to
// IAudioSessionControl2 for GetProcessId. pid 0 = system sounds session; skip it.
```

---

## 15. ConfigStore — surgical TOML + atomic write + echo suppression

**Where:** `crates/control/src/store.rs`.

```rust
pub fn apply(&mut self, edits: &[ConfigEdit]) -> Result<ConfigSnapshot, StoreError> {
    // 1. Edit the toml_edit::DocumentMut IN PLACE — never Deserialize→Serialize (destroys comments).
    for e in edits {
        match e {
            ConfigEdit::SetGroupGain(name, gain) => {
                let g = find_group_mut(&mut self.doc, name)?;      // doc["group"] is an ArrayOfTables
                g["gain"] = toml_edit::value(gain.value() as f64); // toml_edit stores floats as f64
            }
            // ...
        }
    }
    // 2. Validate the result through the SAME loader the watcher uses (one validation path).
    let snapshot = crate::parse_snapshot(&self.doc.to_string())?;
    // 3. Atomic write: temp file in the SAME DIRECTORY (rename across volumes isn't atomic), then rename.
    let tmp = self.path.with_extension("toml.tmp");
    std::fs::write(&tmp, self.doc.to_string())?;
    std::fs::rename(&tmp, &self.path)?;
    // 4. Echo suppression: remember content hash; watcher-delivered snapshots matching it are ours.
    self.last_write_hash = hash(&self.doc.to_string());
    Ok(snapshot)
}
// Watcher side: notify fires MULTIPLE events per save (editors write-then-rename) — debounce
// watcher events ~100 ms and re-read the file once, don't process each event.
```

---

## 16. Testing pattern — mock ports

**Where:** `crates/engine/tests/`. This is WHY the traits live in engine: the whole graph runs on Linux CI with fakes.

```rust
struct MockSystem { endpoints: Vec<Endpoint>, /* Arc<Mutex<...>> fine — tests aren't RT */ }
impl AudioSystem for MockSystem {
    fn open_capture(&self, id: &EndpointId) -> Result<Box<dyn CapturePort>, PortError> {
        Ok(Box::new(SineCapture::new(440.0, self.fmt)))   // deterministic signal source
    }
    fn open_render(&self, id: &EndpointId) -> Result<Box<dyn RenderPort>, PortError> {
        Ok(Box::new(SinkRender::recording()))              // captures frames for assertions
    }
    fn promote_rt_thread(&self) -> RtGuard { RtGuard::noop() }
    ...
}
// Assert on: SinkRender's recorded samples (gain applied? groups summed?), xrun counters,
// and DriftController convergence with scripted FillSample sequences.
// Mixer unit tests (audio-core): feed known buffers, assert exact f32 output (mix math is deterministic).
```

---

## 17. Channel matrix — layout order, normalization, in-place traps

**Where:** `crates/audio-core/src/sample.rs` (`ChannelLayout`), `crates/audio-core/src/channel.rs` (`ChannelMatrix`), `crates/audio-core/src/mixer.rs` (integration), `crates/win-audio/src/format.rs` (mask probe). Blueprint: `.lattice/context/channel-mixdown.md`.

```rust
// GOTCHA 1 — column index ≠ bit position. WASAPI interleaves channels in ASCENDING
// mask-bit order; the matrix column for a speaker is its RANK among set bits, not the
// bit number. SPEAKER_BACK_LEFT (0x10) in 5.1 (mask 0x3F) is column 4; in quad (0x33) it's column 2.
for (col, speaker) in layout.iter_set_bits_ascending().enumerate() { ... }

// GOTCHA 2 — normalize GLOBALLY, not per row. Scale the WHOLE matrix by 1/max_row_sum
// (only if max_row_sum > 1.0). Per-row normalization changes inter-channel balance
// (rows with different sums get different attenuation → stereo image shifts).
let max_sum = rows.iter().map(|r| r.iter().sum::<f32>()).fold(0.0, f32::max);
if max_sum > 1.0 { for c in coef.iter_mut() { *c /= max_sum; } }

// GOTCHA 3 — coefficient table (apply in this precedence; f32 consts, use FRAC_1_SQRT_2):
// 1. same speaker in both layouts            → 1.0 (pass-through)
// 2. FC missing in output                    → 0.7071 into FL and FR (NOT 0.5/0.5 power split)
// 3. Ls/Sl missing → same-side front only    → 0.7071 into FL (never into FR); mirror for right
// 4. BC missing                              → 0.7071 into both back/front-left+right per availability
// 5. LFE missing in output                   → dropped, coefficient 0.0 (approved decision — never mix it in)
// 6. unknown-position input channel          → 0.7071 into EVERY output (never discard, never error)
// Stereo→mono falls out: FL,FR → FC at 1.0 each, row sum 2.0 → global normalize → 0.5·(L+R). Correct.
// Mono→stereo: FC → FL,FR at 0.7071 each (rule 2). Do NOT special-case to 1.0 — that's +3 dB hot on round-trip.

// GOTCHA 4 — process() is NEVER in-place. Input and output frame counts match but widths
// differ; even at equal width, out[m] reads ALL in[n] per frame. Write to the dedicated
// pre-allocated `matrixed` scratch. Plain overwrite (=) per output sample, not += —
// or zero the frame first. += into stale scratch = garbage audio that passes unit tests
// on zeroed test buffers.
for f in 0..frames {
    for m in 0..out_ch {
        let mut acc = 0.0f32;
        for n in 0..in_ch { acc += coef[m * in_ch + n] * input[f * in_ch + n]; }
        output[f * out_ch + m] = acc;   // overwrite, not +=
    }
}

// GOTCHA 5 — identity = layouts EQUAL, not counts equal. 5.1 (0x3F) and 6.0 (0x10F… differs)
// are both 6ch but need a real matrix. Compare ChannelLayout values.
// Identity path: skip the stage entirely (no copy through scratch).
```

**Mixer integration traps:**
- `Src` after the matrix is built at **output** channel count on BOTH sides (`from = {in_rate, out_ch, out_layout}`, `to = {out_rate, out_ch, out_layout}`). Keep `Src::new`'s `ChannelMismatch` check as an internal invariant — do not delete it.
- Per-group scratch sizing changes: `matrixed` = `max_block_frames * out_ch`; the existing `resampled` scratch must switch from group channels to **out_ch** too. Miss this on a downmix and it silently over-allocates (fine); miss it on an upmix (1→2) and it **undersizes** — the `debug_assert!(progress.consumed == n)` in `push_group` fires only in tests.
- DSP chain (P5) and duck stay at **source** layout — the `Format` handed to `DspChain::process` keeps the input layout. Don't globally swap fmt to output format after the matrix lands in the chain.

**win-audio mask probe (`format.rs`):**
```rust
// dwChannelMask exists ONLY on WAVEFORMATEXTENSIBLE. Check BEFORE casting:
// wFormatTag == WAVE_FORMAT_EXTENSIBLE (0xFFFE) && cbSize >= 22, then cast
// *const WAVEFORMATEX → *const WAVEFORMATEXTENSIBLE and read dwChannelMask.
// Plain WAVEFORMATEX (or mask 0, or popcount(mask) != nChannels — real drivers do
// both) → ChannelLayout::default_for_count(nChannels). Never trust popcount == count.
```

**Format field ripple:** adding `layout` to `Format` breaks every struct-literal construction — mocks (`engine/ports/mock.rs`), every unit test, `win-audio` open paths. Mechanical fix, but do it in the same commit as the field; a `ChannelLayout::default_for_count(channels)` one-liner per site is the correct filler everywhere except the win-audio probe (which reads the real mask).

**Tests that catch the classic mistakes:** 5.1→stereo with only-FC input → both outputs equal at 0.7071·(1/max_row_sum); 5.1→stereo Ls-only input → right output exactly 0.0 (side leakage = rule 3 broken); quad vs 5.1 column indexing (GOTCHA 1); upmix mono→stereo scratch sizing; two different 6-ch layouts not treated as identity.

---

## Quick cross-reference

| Implementing… | Read §§ |
|---|---|
| `audio-core` mixer/gain | 1, 8, 12, 17 |
| `audio-core` dsp | 8, 9 |
| `audio-core` resample | 11, 17 |
| `audio-core` channel matrix / layout | 17 |
| `engine` runtime/rings/commands | 1, 5, 6, 7 |
| `engine` clock | 10 |
| `engine` tests | 16 |
| `win-audio` com/capture/render | 2, 3, 4 |
| `win-audio` format/mask probe | 17 |
| `win-audio` sessions/router | 13, 14 |
| `control` store | 15 |
