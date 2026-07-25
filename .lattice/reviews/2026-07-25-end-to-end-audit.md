# End-to-End Audit — 2026-07-25

Scope: every `status: complete` blueprint in `.lattice/context/` checked against
the shipped code. Focus forced by a live bug report: routed audio is pops only,
second output apparently dead, unrouted apps play through normally, ~3.8% CPU
whenever anything is routed.

**Verification state:** `cargo test --workspace` = 118 pass / 0 fail. Every bug
below is invisible to that suite — see B17.

---

## Summary of root causes

| Symptom user reported | Cause |
|---|---|
| Routed app = occasional pops only | **B1** (half of every render buffer discarded) + **B2** (mixer over-produces, rings drop) + **B3** (capture under-reads by ~2x) + **B4** (silence zero-padded into every block) |
| Second output "does nothing" | Same four. Both outputs are equally broken; the second is quieter because it usually has fewer/no matched pids, so its rings are pure silence-padding |
| Unrouted apps play direct to speakers | **By design** — not captured, not muted. But it reads as a bug because the mixer UI shows an "unassigned" pool that looks like a routing zone. See B9/B18 |
| ~3.8% CPU when routed | **B5** (`spin_sleep` busy-tail on MMCSS threads, per routed pid) + **B6** (256-tap sinc SRC) × **B2** (tick inflation) |

---

## P0 — audio corruption

### B1. `render_loop` throws away audio it already popped from the ring
`crates/engine/src/runtime.rs:1205,1217-1241` + `crates/win-audio/src/render.rs:100-125`

`render_loop` sizes `buf` at `port.period_frames() * channels` and pops that
many frames from the ring on every event. `WasapiRender::period_frames()`
returns `GetBufferSize()` (render.rs:131-133), which with the 20 ms
`BUFFER_DURATION_100NS` hint is ~960 frames @48 kHz. But the WASAPI event fires
once per *device period* (~480 frames / 10 ms), so at event time
`GetCurrentPadding()` leaves only ~480 frames free. `write()` computes
`to_write = frame_count.min(free)` and then **silently returns `Ok(())` after
writing only `to_write`** — the other ~480 frames are gone, already consumed
from the ring, never rendered.

Net effect: the output stream advances at ~2x real time with a hole in every
block. That is precisely "occasional pops".

This is a data-loss path by construction, independent of the exact ratio: any
jitter that makes `free < frame_count` drops audio.

Fix direction:
- `RenderPort::period_frames()` should report the *device* period
  (`IAudioClient::GetDevicePeriod`), not `GetBufferSize()`.
- `render_loop` must ask for free space before popping, or `write()` must
  return frames-written and the caller must retain the remainder. Dropping
  must never be silent.

### B2. Mixer has no flow control — one full block produced per wake, whatever the wake was
`crates/engine/src/runtime.rs:1288-1315, 1404-1435, 1437-1465`

`pull_group_inputs` always hands `push_group` the **entire**
`max_block_frames × channels` scratch buffer, so `mix_tick` always produces a
full block, and `flush_outputs` always pushes a full block into every output
ring.

The mixer is woken by *three* independent sources (mixer-demand-driven-wakeup
Flow C/D/E): every render event (runtime.rs:1234), **every capture thread on
every poll** (runtime.rs:829), and every `apply_params` (runtime.rs:502). The
design's sizing assumed "one wake ≈ one real render period"
(`WAKE_MARGIN = 1.25`, runtime.rs:286, 1500-1512) — that assumption does not
hold once N capture threads each unpark at ~10 ms.

Production per tick = `1.25 × render_buffer + 8` ≈ 1208 frames. Drain =
~480 frames per 10 ms. Ticks/s ≥ render events + N×capture polls. The output
ring (capacity 4 × period, runtime.rs:1486-1489) is therefore permanently
saturated and `producer.push` drops on the floor with `let _ =`
(runtime.rs:1449) — arbitrary chunks of real audio discarded, no counter, no
log.

Fix direction: the tick must be governed by ring free space
(`producer.slots()` per output), not by the fact that a wake happened. Mix
exactly the number of frames the hungriest output can accept, or skip the tick.

### B3. Capture thread reads ~256 frames per poll but the process produces ~480
`crates/engine/src/runtime.rs:818, 833` + `crates/win-audio/src/process_capture.rs:273`

```rust
let mut buf = vec![0.0f32; channels * 256];   // 256 frames
...
sleeper.sleep(poll_interval);                  // GetBufferSize/2 ≈ 10 ms
```

At 48 kHz, 10 ms of audio is 480 frames. The loop drains 256. `ProcessCapture::read`
returns as soon as the caller's buffer is full (`while written < buf.len()`,
process_capture.rs:287), leaving whole packets queued in WASAPI — which then
overruns. Group rings are chronically starved at ~53% of the required rate,
which is what makes B4's zero-padding fire on essentially every tick.

Fix: size `buf` from `poll_interval × sample_rate × channels` with ≥2x margin,
and have `read` drain until `GetNextPacketSize() == 0`.

### B4. Partially-filled blocks are zero-padded into the stream
`crates/engine/src/runtime.rs:1411-1434`

`scratch.fill(0.0)`, pop whatever the ring has into `scratch[0..filled]`, then
`mixer.push_group(slot.group_id, scratch)` — the **whole** slice. Everything
from `filled` to the end of the block is silence spliced into the middle of the
audio stream. `push_group` already handles a short slice correctly
(`mixer.rs:549`, `frame_count = frames.len()/channels`), so this is a one-line
class of fix — push `&scratch[..filled_max]`, or better, don't tick until a
full block is available.

---

## P1 — CPU

### B5. `spin_sleep` busy-spins 0.7 ms of every sleep, on MMCSS "Pro Audio" threads
`crates/engine/src/runtime.rs:819, 833` (capture) and `1579, 1599, 1679` (supervisor)

Verified against `spin_sleep-1.3.3` source: on Windows `SpinSleeper::default()`
uses `sleep_accuracy() == 700_000` ns and `SpinStrategy::SpinLoopHint`
(`windows.rs:24-33`, `lib.rs:96-106, 226-236`). So every `sleeper.sleep(d)`
native-sleeps `d - 0.7 ms` and then **tight-spins `std::hint::spin_loop()` for
0.7 ms**.

- `pid_capture_loop`: 0.7 ms spin per ~10 ms poll = **~7% of one core, per
  routed process**, on a thread promoted to MMCSS Pro Audio.
- `supervisor_loop`: 0.7 ms per 100 ms = ~0.7% of a core, always.

On an 8-core box, 3–4 routed pids lands at ~3.5% total. That is the measured
number, and it explains why the cost appears only *when something is routed* —
`mixer_loop` really was fixed by mixer-demand-driven-wakeup; the two threads
that design explicitly deferred (its decision 5) are now the entire cost.

Fix: `SpinSleeper::new(0)` or plain `thread::sleep` for both (neither needs
sub-millisecond wake precision — the capture ring absorbs jitter). Better still
for capture: `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` + `SetEventHandle` +
`WaitForSingleObject`, which is what Microsoft's own `ApplicationLoopback`
sample does, and which would make the capture thread genuinely event-driven
like `render_loop`.

### B6. Always-on 256-tap / 128x-oversampled sinc resampler per group
`crates/audio-core/src/resample.rs:14-16`

```rust
const SINC_LEN: usize = 256;
const OVERSAMPLING_FACTOR: usize = 128;
```

Every group runs a real `SincFixedIn` on every tick even at a nominal 1:1
48k→48k ratio (documented as deliberate, for drift correction). At correct tick
rates this is ~24.5 M MAC/s per stereo group; multiplied by B2's tick inflation
it is the second-largest CPU term after B5.

The drift loop only ever asks for ±0.5% (`DriftConfig::max_correction`,
clock.rs:29). A 256-tap/128x sinc is far more resampler than a 0.5% trim needs.
Consider dropping to `sinc_len` 64 / `oversampling_factor` 32, or a cheap
cubic/linear async resampler for the trim, keeping the high-quality path only
when `from.sample_rate != to.sample_rate`.

---

## P2 — logic bugs

### B7. Drift controller's correction sign is inverted for `SincFixedIn`
`crates/engine/src/clock.rs:89-92`

```rust
// ring too full (err > 0) -> corr > 0 -> ratio > 1 -> resampler consumes faster.
let ratio = ResampleRatio::new(1.0 + corr)
```

`SincFixedIn` consumes a **fixed** input chunk (`chunk_in = max_block_frames`,
resample.rs:59, 78) and produces `chunk_in × ratio` output. Raising the ratio
produces *more* output, filling the ring *further* — positive feedback, not
correction. The integrator winds to `+max_correction` and stays pegged. Should
be `1.0 - corr` (or the error sign flipped).

Currently masked by B8, which stops the loop from running at all during real
playback.

### B8. `RingGauge.active` is effectively never true, so drift correction never runs
`crates/engine/src/runtime.rs:1426-1432`

`any_full` requires a *single* pid to fill the **entire** `max_block_frames`
block in one tick. Under B2/B3 that never happens, so `active` stays false,
`DriftController::tick` skips every output (clock.rs:75-77), and no
`SetOutputRatio` is ever emitted during playback. The whole drift-and-recovery
P2 correction path is dead in practice.

### B9. Drag-to-unassign is a no-op when the match came from a glob rule
`crates/app/src/ui.rs:2030-2057`

`resolve_drag_assign` only adds/removes `MatchRule::ExactName` entries; globs
are deliberately never touched. So dropping a chip onto Master while any group
carries a glob (including the `*` catch-all that
`process-loopback-capture.md` L1 §2 makes the *recommended* master-group
mechanism) produces zero edits — the chip snaps straight back on the next
reconcile with no feedback. Either exclude glob-matched sessions from the
drag-assign UI, or add a per-session exclusion list.

### B10. An unreadable process path becomes an `ExactName("")` rule
`crates/app/src/ui.rs:791, 2038` + `crates/win-audio/src/sessions.rs:270, 282-297`

`process_image_path` returns `None` for a process we can't `OpenProcess`
(elevated/protected), and `describe_session` falls back to
`PathBuf::default()`. `session_file_name` then yields `""`, `resolve_drag_assign`
pushes `""` into `match_rules`, and `match_session` compares
`"".eq_ignore_ascii_case("")` — so one such drag silently routes **every other
unreadable session** into that group. Guard against an empty file name at the
drop site.

### B11. Sessions are enumerated exactly once, at startup — no re-enumeration
`crates/engine/src/routing.rs:167-173, 337-374`

`state.live_sessions` is seeded from one `enumerate()` call and thereafter
maintained purely from `IAudioSessionNotification`/`IAudioSessionEvents`. There
is no periodic re-enumeration. Any notification that silently fails to register
(`register_session_events` and `register_new_session_notifications` are both
best-effort no-ops on failure, sessions.rs:310-330, 359-368) means those apps
are **never discovered for the rest of the process lifetime**, with no error
surfaced anywhere. Given the reported "routing seems to be doing something but
I can't tell what", worth a cheap re-enumerate every N reconcile ticks as a
self-healing fallback.

### B12. New output devices appearing after launch never get session notifications
`crates/win-audio/src/sessions.rs:133-173`

`take_events()` enumerates endpoints once and registers a
`IAudioSessionNotification` per endpoint present at that moment. Plug in a USB
headset later and no session created on it is ever reported. `WasapiSystem`
already receives `DeviceEvent::Added` (monitor.rs) — nothing re-drives session
registration off it.

### B13. Render path assumes every device mix format is 32-bit float
`crates/win-audio/src/format.rs:48-66` + `crates/win-audio/src/render.rs:117`

`Format` carries only `sample_rate`/`channels`/`layout` — no sample type.
`WasapiRender::write` casts the WASAPI buffer to `*mut f32` unconditionally.
Shared-mode `GetMixFormat` is almost always `IEEE_FLOAT` 32-bit, but nothing
validates it; a device reporting integer PCM would render full-scale garbage.
Add a `SubFormat`/`wBitsPerSample` check in `format_from_wfx` and fail
`open_render` loudly rather than producing noise.

### B14. `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` used without `SRC_DEFAULT_QUALITY`
`crates/win-audio/src/process_capture.rs:247`

Microsoft documents these two flags as a pair; `AUTOCONVERTPCM` alone is not
the documented usage. Given the format is *dictated* (48k/stereo/f32) and the
underlying engine format may differ, this is on the path that matters. Verify
against live docs before changing — this crate has been burned twice already on
COM/WASAPI details taken from memory.

### B15. A structural rebuild silences routed apps for ≥100 ms, muted *and* uncaptured
`crates/engine/src/runtime.rs:875-908, 658-680` + `crates/engine/src/routing.rs:243-286`

`apply_rebuild` → `stop_running_graph` kills every per-pid capture thread and
`build_running_graph` starts with `capture_pids: HashMap::new()`. Meanwhile
`routing::State.applied` is untouched, so the session-mute-on-capture mute stays
applied to every routed pid. Until the coordinator's next `RECONCILE_TICK`
(100 ms) those apps are muted at the OS level *and* not being captured — total
silence, not a gap. Every output-device change from the UI hits this
(`ConfigEdit::SetGroupOutput` → `EditStructure`, ui.rs:725).

### B16. `EngineStats.group_faults` is permanently empty — capture failures are invisible
`crates/engine/src/runtime.rs:231-235`

Documented and intentional post-pivot, but the consequence stands: a pid that
repeatedly fails to open surfaces only as a transient `RoutingDegraded` event.
There is no persistent per-app "couldn't capture this" indicator in the mixer
UI, which is exactly what a user staring at "why is nothing happening" needs.

### B17. The mocks model an infinitely fast, infinite-capacity device
`crates/engine/src/ports/mock.rs:250-269, 302-326`

- `SinkRender::wait_event` returns `Ok(())` immediately — no clock.
- `SinkRender::write` `extend_from_slice`s unconditionally — never rejects.
- `SinkRender::period_frames()` is a hardcoded `480`.
- `SineCapture::read` always fills the caller's entire buffer, whatever its size.

So every rate/flow-control bug above (B1, B2, B3, B4, B8) is *structurally
unobservable* by the 118-test suite, which is why it is green. This is the
highest-leverage fix in the list: a paced mock (event fires on a real clock,
`write` accepts at most `free` frames and reports the shortfall, capture yields
at most `elapsed × rate` frames) turns four production-only bugs into failing
unit tests.

### B18. "Unassigned" pool reads as a routing destination
`crates/app/src/ui.rs:634-680, 1990-2010`

Apps in the unassigned pool are not captured, not muted, and play straight to
the Windows default device. That is the design. But the pool is rendered as a
peer drop-zone under Master with the same chip affordance as a real group, so
"my unrouted apps bypass Splitstream entirely" reads as a bug. Either label it
explicitly ("Not routed — playing through Windows directly") or steer users to
create a `*` catch-all group (which `process-loopback-capture.md` L1 §2 already
designates as the intended mechanism, but which nothing in the UI mentions).

---

---

## Addendum — 2026-07-25, measured corrections to B5/B6

Both P1 items were grounded before designing to them. Two of this document's
claims do not survive measurement.

### B6's severity ranking is wrong

The "second-largest CPU term after B5" claim above is derived from a
24.5 M MAC/s arithmetic estimate, not a measurement. Benchmarked against the
real pinned `rubato 0.15.0` at the real block size (608 frames = the
`max_block_frames` audio-flow-control lands on, stereo, 25 s of audio per run,
`--release`):

| Resampler | 48k -> 48k (drift trim only) | 48k -> 44.1k |
|---|---|---|
| **`SincFixedIn` 256/128 (current)** | **0.285% of one core** | 0.268% |
| `SincFixedIn` 64/32 | 0.100% | 0.101% |
| `SincFixedIn` 32/16 | 0.080% | — |
| `FastFixedIn` Cubic | 0.026% | 0.025% |
| `FastFixedIn` Quintic | 0.058% | — |

0.285% of one core **per group**, against B5's 0.7 ms spin per 10 ms poll =
**7% of one core per routed pid**. B5 is ~25x larger. B6 is noise beside it,
not the second-largest term.

Consequence: the rate-equality split this document proposes (cheap resampler at
1:1, high-quality sinc only when rates differ) is not worth its complexity —
`FastFixedIn` buys a further 0.074%/group over a plain `SincFixedIn` 64/32
while adding a second resampler type, a branch, and a polynomial-vs-sinc
quality question on the conversion path. **Reduced to: `SINC_LEN` 256 -> 64,
`OVERSAMPLING_FACTOR` 128 -> 32.**

### B5 does not need an event-driven capture path

`thread::sleep` granularity, measured standalone on the dev machine:

| target | mean | max | overshoot |
|---|---|---|---|
| 10 ms | 10.300 ms | 11.380 ms | +0.30 ms |
| 5 ms | 5.288 ms | 5.910 ms | +0.29 ms |
| 1 ms | 1.467 ms | 2.456 ms | +0.47 ms |

No 15.6 ms legacy granularity — a plain sleep overshoots a 10 ms capture poll
by ~3% (worst case 14%), which audio-flow-control's `CAPTURE_BUF_INTERVALS = 2`
(20 ms of read buffer) absorbs with room to spare, and would still absorb at
15.6 ms on a machine that does have legacy granularity. `spin_sleep`'s 0.7 ms
busy tail buys nothing that matters.

Consequence: the `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` + `SetEventHandle`
suggestion above is not worth pursuing for CPU — once the spin is gone a thread
waking 100x/s costs well under 0.1% of a core. Its premise is also unverified:
this codebase already found `GetMixFormat` returns `E_NOTIMPL` on a
`PROCESS_LOOPBACK`-activated client, so that client type does not necessarily
support the same flags as a normal one. **Reduced to: replace `spin_sleep` with
`thread::sleep` at runtime.rs:819/833 (capture) and 1579/1599/1679
(supervisor), and drop the `spin_sleep` dependency.**

### Net

B5 + B6 together are ~5 lines and 2 constants — a chore, not a design. B5 still
recovers essentially all of the reported ~3.8% CPU.

---

## Manual tests needed

These cannot be settled by reading code — each needs real hardware.

### MT1. Does `ISimpleAudioVolume::SetMute` also silence the process-loopback capture? **(highest priority)**
session-mute-on-capture mutes the app's session so its audio doesn't double up
through the default device (`routing.rs:274-278` → `sessions.rs:180-201`). It is
**unverified** whether `PROCESS_LOOPBACK` capture taps the process's stream
*before* or *after* the session mute is applied. If it taps after, the entire
feature silences the thing it is trying to route — which would independently
explain "routed apps produce almost nothing".

Steps:
1. Start a known audio source (e.g. a looping tone in a media player). Note pid.
2. `SPLITSTREAM_TEST_PID=<pid> cargo test -p win-audio -- --ignored --nocapture open_and_read_a_real_process`
   Record the reported sample count and confirm the samples are non-zero (add a
   peak print if needed).
3. In Volume Mixer, mute that app.
4. Re-run the same command. Compare.
5. **Expected if healthy:** identical non-zero peaks in both runs.
   **If step 4 reads silence/zeros:** session-mute-on-capture is fundamentally
   incompatible with process loopback and must be replaced (e.g. per-session
   volume set to 0 won't help either; the alternative is excluding the target
   process from the default endpoint another way, or accepting the double-audio).

### MT2. Confirm the render period vs. buffer-size mismatch (B1)
Steps:
1. Add a temporary log in `win-audio/src/render.rs::open` printing
   `GetBufferSize()` and `GetDevicePeriod()` (default + minimum) for the device.
2. Add a temporary log in `WasapiRender::write` printing `frame_count`, `free`,
   and `frame_count - to_write` (the dropped count) once per second.
3. Route one app, play audio, watch the log for 10 s.
4. **Expected if B1 is real:** a non-zero dropped count on essentially every
   write, with `free` ≈ half of `buffer_frames`.

### MT3. Confirm capture under-read (B3)
Steps:
1. In `pid_capture_loop`, log `n` (frames read) and `poll_interval` once per
   second, plus the ring's fill after the push loop.
2. Route one app playing continuous audio.
3. **Expected if B3 is real:** `n` pinned at 512 samples (256 frames) with
   `poll_interval` ≈ 10 ms — i.e. ~half the frames the source is producing —
   and the group ring never approaching full.

### MT4. Confirm CPU attribution (B5)
Steps:
1. With nothing routed, record Splitstream's CPU in Task Manager (Details →
   right-click → Set affinity to a single core makes the per-core cost readable).
2. Route one app. Record. Route a second, then a third. Record each.
3. **Expected if B5 is real:** roughly linear ~7%-of-one-core per routed pid,
   independent of whether that app is actually playing audio.
4. Change both `SpinSleeper::default()` call sites to `SpinSleeper::new(0)`,
   rebuild, repeat. Expect the per-pid increment to drop to near zero.

### MT5. Second-output routing, once B1–B4 are fixed
Steps:
1. Two groups, two *different* physical output devices, one app matched to each.
2. Confirm each app is audible only on its own device.
3. Move a group's output device in the UI; confirm the switch takes effect and
   note the silence duration (B15 predicts ≥100 ms of total silence, not a gap).
4. Repeat with both groups on the *same* device to confirm they sum cleanly.

### MT6. Device mix-format sanity across the user's real devices (B13)
Steps:
1. `cargo test -p win-audio -- --ignored --nocapture enumerate_real_render_endpoints`
2. For each device printed, check in Windows Sound → Properties → Advanced what
   the shared-mode format actually is.
3. **Flag any device not reporting 32-bit float** — those will render garbage
   through the current `write()` cast.

### MT7. Session discovery robustness (B11/B12)
Steps:
1. Launch Splitstream, note which apps appear in the mixer.
2. Start a *new* app playing audio. Does it appear within ~1 s?
3. Plug in a USB/Bluetooth audio device. Start an app that renders to *it*.
   Does that session appear?
4. **Expected failures:** step 3 almost certainly does not appear (B12).

---

## Suggested order of work

1. **B17** first — build the paced mocks. Without them, B1–B4 fixes are
   unverifiable and will regress.
2. **B1 + B2 + B3 + B4** together — they are one coherent flow-control problem
   and fixing any one alone will not make audio correct.
3. **MT1** — decide session-mute-on-capture's fate before tuning anything else.
4. **B5** — one-line change, recovers most of the CPU.
5. **B7 + B8** — the drift loop is currently both dead and wrong; fix as a pair.
6. Everything else.
