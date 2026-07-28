---
feature: output-rate-truncation
requirement_doc: null
created: 2026-07-26
status: >
  original truncation fix validated on hardware 2026-07-27 (MT11–MT14 pass,
  MT15 waived). Follow-on defect found by that run — the drift loop regulated
  the output ring, which the governor already holds, leaving the capture ring
  unregulated and bleeding samples. Controller moved to the capture rings,
  aggregated per output, with the fill smoothed before the controller sees it.
  A first attempt keyed it per group and regressed — see "Keying". Capture-side
  counters hardware-validated 2026-07-27 (drops 0, fill 0.50, ratio settled).
  The user's static turned out to be a THIRD, pre-existing defect in the shared
  output span, and running MT17 exposed a FOURTH (a group whose pids go away
  gated its output's span forever) — see
  .lattice/context/session-2026-07-27-static.md, which is the live handoff.
  MT16 and MT17 both pass on hardware as of 2026-07-27. The open item is now
  the two-group popping recorded there, not anything in this document.
note: >
  Bug fix, not a designed feature. Origin is a live user report: routed audio
  audible only as brief instants of full-bandwidth signal separated by skips
  forward in time, a few milliseconds apart. Root cause is a units bug —
  `max_block_frames` counts frames at a group's INPUT rate but was used to size
  buffers holding OUTPUT-device frames — which silently truncated every block by
  the rate ratio. The user's DAC reports 96 kHz against a capture pinned at
  48 kHz, so exactly half of every block was discarded.
---

# Output-Rate Truncation (playback skipping)

> Four defects, all in transport rather than in the signal path. The channel
> matrix, spatializer, DSP chain and SRC were never at fault — the user's own
> first suspicion (mixdown) pointed at the right *stage* but the wrong *layer*.

## Grounding — the measurement that named it

Endpoint mix formats on the reporting machine, dumped from a live
`EndpointEnumerator::enumerate()`:

| Endpoint | Mix format |
|---|---|
| Headphones (2- FiiO K11) — **the routed output** | **96 000 Hz**, 2 ch |
| CABLE Input (VB-Audio Virtual Cable) — the sink | 96 000 Hz, 2 ch |
| XG349C (NVIDIA High Definition Audio) | 48 000 Hz, 2 ch |
| Speakers (Steam Streaming Speakers) | 48 000 Hz, 6 ch |

`PROCESS_CAPTURE_FORMAT` is pinned at 48 000 Hz (`runtime.rs` — a
process-loopback-activated `IAudioClient` does not implement `GetMixFormat`,
so the format is dictated, not negotiated). Rate ratio therefore **2.0**.

Corroborating counter, from the user's session log before any fix:

```
xruns=652 … xruns=708   (~40–60/s, from second one, never recovering)
output_drops=0 capture_drops=0 render_shortfall=0
```

Roughly half of all render events underran — consistent with the output ring
receiving half the frames it needed, and inconsistent with any signal-domain
fault (`limiter_engaged_total=0` throughout).

## The four defects

| # | Where | Defect | Why it was invisible |
|---|---|---|---|
| 1 | `mixer.rs` `Mixer::new` | `accum` (and the output limiter) sized `max_block_frames × channels`. That counts INPUT frames; the buffer holds OUTPUT frames. `mix_tick`'s `produced.min(accum.len())` then discarded the surplus. | No counter. `output_drops` only counts *ring* rejections, which never happened — the frames were gone before the ring. |
| 2 | `runtime.rs` `mixer_loop` | `output_scratch` sized the same wrong way, so `take_output` truncated a second time — and it clears the accumulator regardless, making the remainder unrecoverable. | Same. |
| 3 | `runtime.rs` `render_loop` | `free_frames()` reports the device's WHOLE free buffer. Taking that much drained the ring below the priming cushion, then padded the shortfall with silence **in the same buffer as real audio**. That shrinks the device queue, which enlarges `free_frames` next event — a self-sustaining hole-punch at the device period rate. | Counted as `xruns`, but read as a symptom of starvation rather than its cause. |
| 4 | `runtime.rs` `pull_group_inputs`, `render_loop` | Ring pops were not frame-aligned. The producer pushes sample-by-sample, so a half-written frame is a normal observation; the odd sample was popped and then dropped by integer division to frames, shifting the ring's interleave permanently (L/R swap). | Silent. Needs no drop to trigger — only a race with the producer. |

Defect 1 is the one that produced the reported symptom. Defects 3 and 4 were
found first and fixed first; they are real but were not sufficient — the user
confirmed no audible change after fixing 3 and 4 alone, which is what forced
the measurement above.

### Secondary correctness fix (not symptom-causing)

`Render::build` synthesised the HRIR set at the **output** device rate, but the
spatializer runs **pre-SRC** (`mix_tick` phase 3), so it convolves input-rate
samples. At 48 kHz in / 96 kHz out the interaural delay was 2× too long. Fixed
to `from.sample_rate`.

### Hardening added while here

`render.rs::open` now rejects any device whose mix format is not 32-bit IEEE
float, instead of reinterpreting its buffer as `*mut f32`. Windows' shared-mode
engine format is float32 in practice — the bit-depth dropdown in the Sound
control panel drives the driver side, not the client format — but a non-float
device would have produced full-scale noise with no diagnostic.

## Automated coverage (already green — do not re-run by hand)

| Test | Pins |
|---|---|
| `a_faster_output_device_than_the_capture_loses_no_frames` | Defect 1. Fails at ratio 1.0 with the pre-fix sizing (verified by reverting the one line). |
| `every_device_rate_conserves_frames_against_the_fixed_48k_capture` | Defect 1 across 24k/44.1k/48k/88.2k/96k/128k/176.4k/192k, each within 2% of theoretical, none at accumulator brim. |
| `render_loop_never_writes_silence_behind_real_audio_in_one_buffer` | Defect 3. |
| `pull_group_inputs_never_pops_a_partial_frame` | Defect 4. |

484 workspace tests, clippy clean, as of 2026-07-26.

---

# Manual test plan

Numbering continues from MT10 (highest previously used, `double-audio-prevention.md`).

## Setup — do this once per session

1. Build fresh, so the binary matches the tree:
   ```
   cargo build --release
   ```
2. **Kill every previously running instance** (tray icon → Quit, or Task Manager).
   A stale instance still holds the capture streams and the default-sink
   override, and the single-instance mutex will make a relaunch silently no-op.
3. Confirm the config still routes to a real device — `%APPDATA%\Splitstream\splitstream.toml`:
   ```toml
   [[group]] name = "Media"  output_device = "Headphones (2- FiiO K11)"
   [[group]] name = "Game"   output_device = "Headphones (2- FiiO K11)"
   ```
4. Note the DAC's current rate (Windows Sound → Headphones → Advanced). Record it
   in the results table — the whole point of MT13 is varying it.

Tail the log in a second terminal throughout:
```
Get-Content "$env:APPDATA\Splitstream\logs\splitstream.log.<today>" -Tail 30 -Wait
```

---

## MT11 — the reported symptom is gone (BLOCKING, primary)

**This is the test that decides whether the fix worked.** Everything else is
diagnosis for when it fails.

Procedure:
1. Launch `target\release\splitstream.exe`.
2. Play continuous, tonally recognisable material through a Media-group app
   (music with sustained vocals or guitar — not a game, not speech; sustained
   material makes gaps obvious).
3. Listen for 60 seconds.

| | Pass | Fail |
|---|---|---|
| Audio | Continuous. No gaps, no pulsing, no "skipping forward in time". | Any of the reported symptoms — brief instants of audio separated by skips. |
| `xruns` in the log | Flat, or creeping only during device/route changes. | Climbing steadily (was ~40–60/s). |

`xruns` is the objective half of this test — trust it over impressions. A
climbing counter with audio that *sounds* fine still means the ring floor is
not holding.

Record: xruns at t=10 s and t=60 s, and the delta.

---

## MT12 — both groups, simultaneously (BLOCKING)

The reported config has **two groups sharing one output device**, which is the
case where `out.filled = out.filled.max(write_len)` and per-group SRC bursts
interact. A single-group pass does not cover it.

Procedure:
1. With music still playing (Media group), start `DOOMTheDarkAges.exe` or any
   Game-group app producing audio.
2. Listen to both for 60 seconds. Move the Game group's fader.

Pass: both groups audible and continuous, mixing cleanly, no skipping on either.
Fail: either group skips, or one group's audio degrades when the other starts.

Record: xruns delta over the 60 s, and whether `output_drops` left 0.

---

## MT13 — DAC rate sweep (BLOCKING — this is the actual root cause axis)

The automated suite proves the *arithmetic* for eight rates. This proves the
real device path. Change the rate in Windows Sound → Headphones (2- FiiO K11)
→ Advanced → Default Format.

**Splitstream must be restarted after each change** — the output format is read
at graph build time.

| DAC rate | Ratio vs 48 kHz capture | Audio continuous? | xruns delta / 60 s |
|---|---|---|---|
| 48 000 Hz (1.0 — the case that always worked) | 1.0 | | |
| 96 000 Hz (**the reported failure**) | 2.0 | | |
| 192 000 Hz (if offered) | 4.0 | | |
| 44 100 Hz | 0.919 | | |

Pass: every offered rate plays continuously. The 96 kHz row is the regression
proper — if only that row fails, the sizing fix did not take effect.

Also try **24-bit vs 16-bit** at one rate. Expected: no difference (shared-mode
client format stays float32). If Splitstream instead refuses to open the device
with `unsupported mix format: N-bit, wFormatTag T`, that is the new guard firing
— **capture that exact message**, it means this machine has a device that would
previously have emitted noise.

---

## MT14 — spatial on/off (NON-BLOCKING)

The HRIR was being built at the wrong sample rate, so binaural imaging at
48k→96k had 2× the intended interaural delay. Now fixed, but never measured.

Procedure: with `spatial = true` on the Media group, play material with a wide
stereo image. Toggle spatial off (settings UI), then on. Compare.

Pass: spatial on produces a plausible widened/externalised image, and toggling
does not click, glitch, or change the loudness noticeably.
Fail worth reporting: image collapses, sounds phasey/hollow, or the toggle
audibly glitches.

Note this is a *quality* judgement on procedurally-synthesised HRIRs, not
measured ones — "not great" is expected, "obviously broken" is a finding.

---

## MT15 — recovery paths still work (NON-BLOCKING, regression sweep)

The render loop's pop and padding logic changed; these are the paths that
exercise it under stress.

1. **Device removal**: unplug the K11 (or disable it in Sound settings) while
   audio plays. Expected: group falls back or parks, engine keeps running, no
   crash. Re-plug: group recovers.
2. **Rate change while running**: change the DAC's Default Format while audio
   plays. Expected: format-change reopen, brief gap, then continuous audio.
3. **Start with nothing playing**: launch Splitstream first, then start an app.
   Expected: audio starts cleanly within a second, no burst of xruns.

Pass: no crash, no permanent silence, xruns settle after each event.

---

## If MT11 or MT13 fails — diagnostic procedure

Do **not** guess from symptoms; the audit trace exists precisely because the
first two fixes were made on plausible-but-insufficient reasoning.

```
$env:SPLITSTREAM_AUDIT = "1"
target\release\splitstream.exe
```

Emits one `audit` line per second (opt-in, off by default):

```
audit  xruns=… output_drops=… capture_drops=…
       ring_fill=0:0.48  applied_ratio=0:1.00012
       group_peak=0:0.31,1:0.00  output_peak=0:0.29  routes=2
```

Reading it:

| Observation | Means |
|---|---|
| `ring_fill` hovering near 0 with `xruns` climbing | Output ring starving — production is below realtime. Look upstream of `flush_outputs`. |
| `ring_fill` near 1.0, `output_drops` climbing | Governor budget disagrees with real ring capacity. |
| `group_peak` non-zero, `output_peak` ~0 | Signal reaches the group but dies between matrix/SRC and the accumulator — i.e. the truncation class this doc is about, at some other rate pair. |
| `group_peak` ~0 while audio is playing | Capture problem, not mixer. Check `routes` is non-zero and the pid actually matched. |
| `applied_ratio` drifting away from 1.0 | Drift controller chasing a target it cannot reach; suspect the ring-fill target units. |
| `routes=0` | Nothing matched — a routing/config problem, not audio. |

Capture 15–20 consecutive `audit` lines while the symptom is audible. That
sequence is what distinguishes starvation from truncation from a routing miss,
which impressions alone cannot.

---

## Results log

| Date | MT | Result | Notes |
|---|---|---|---|
| 2026-07-26 | — | Implemented, 484 tests green, clippy clean | Not yet run on hardware |
| 2026-07-27 | MT11 | **Partial** — reported symptom gone, new one exposed | Music clear and vibrant, no skipping, `xruns=0` for the whole run. But light background static + occasional pops, absent when playing to the K11 directly. Cause is `capture_drops`, see below. |
| 2026-07-27 | MT12 | Pass | Both groups simultaneously, clean. |
| 2026-07-27 | MT13 | Pass | Every offered DAC rate plays continuously; restart-after-change confirmed necessary and accepted. No `unsupported mix format` — the new float32 guard did not fire on this machine. |
| 2026-07-27 | MT14 | Pass | Spatial toggle audibly changes the profile, no click or loudness jump. HRIR rate fix stands. |
| 2026-07-27 | MT15 | Waived | Device-removal/rate-change-while-running edge cases not exercised. |
| 2026-07-27 | MT16 | Pass | Capture ring holds its level: `capture_fill` settles 0.49–0.52 against 0.5, `capture_drops` flat, `applied_ratio` settled near 1.0, `xruns` 0. Holds for minutes with one group. |
| 2026-07-27 | MT17 | Pass | Static gone, and the unassign stall it exposed (defect 4, see session-2026-07-27-static.md) fixed and re-verified. |
| 2026-07-27 | MT4 | Effectively closed | CPU for one routed app went from ~4% to ≈0. Release build is most of it, compounded by the B5/B6 chore (`spin_sleep` removed, sinc taps cut) and by the truncation fix removing 40–60 xruns/s of underrun handling. |

---

# Follow-on: the capture ring is an unregulated buffer

MT11's residue. Separate defect, exposed rather than caused by the fix above.

## Grounding — the audit trace

`SPLITSTREAM_AUDIT=1`, one routed app, K11 at 96 kHz, capture pinned at 48 kHz:

```
04:08:56  capture_drops=0     ring_fill=0:0.74  applied_ratio=0:1.00500   (silence)
04:09:04  capture_drops=64    ring_fill=0:0.59  applied_ratio=0:0.99886
04:10:04  capture_drops=6912  ring_fill=0:0.72  applied_ratio=0:0.99533
```

- `xruns=0`, `output_drops=0`, `render_shortfall=0` for the entire run. The output
  side is healthy — this is purely an input-side loss.
- `capture_drops` climbs monotonically from ~6 s after audio starts and never
  recovers: ~64/s at first, ~123/s by the end of the window. Roughly 0.1% of a
  48 kHz stereo stream, discarded as isolated samples. That is precisely what
  low-level broadband static with intermittent pops is.
- `ring_fill` sawtooths 0.52–0.81 around `drift_target_fill` = 0.658 — exactly
  what audio-flow-control decision 7 designed. The output loop is behaving.
- `applied_ratio` alternates between **both** clamp rails (0.995 / 1.005) at
  roughly 1 Hz. A settled controller does not live on its rails.

## Why it drops — the loop is closed on the wrong buffer

Two facts already pinned elsewhere in the codebase combine badly:

1. `SincFixedIn` consumes a **fixed** `chunk_in` and produces `chunk_in × ratio`
   output frames (`resample.rs`, restated in `clock.rs`'s B7 comment).
2. The governor (`group_may_push`) skips a group entirely while its output ring
   sits at/above `GOVERNOR_THRESHOLD_FILL`, leaving that group's pid rings
   untouched.

In steady state the output ring is drained at the device rate `R_out`, so the
governor admits `R_out / (B_in × ratio)` ticks per second, and therefore

```
input consumed per second = B_in × ticks = R_out / ratio
surplus                   = R_cap − R_out / ratio
```

`R_cap` is the capture side's own clock; `R_out` is the DAC's crystal. Nothing
makes them agree. The surplus lands in the pid ring, which saturates and then
discards every excess sample forever.

The deeper problem is that **the governor already regulates output ring fill**,
by withholding ticks. The drift controller is measuring that same fill and
trying to regulate it with `ratio` — but the governor holds it at target no
matter what `ratio` does. So the controller reads only the sawtooth's phase
noise and integrates on it: `ratio` is an unobservable free variable, which is
why it wanders onto both rails. Whatever value it drifts to then *dictates*
input consumption via `R_out / ratio`, and the capture ring silently eats the
difference.

`ratio` is the only lever in the system that can absorb a clock mismatch. It is
currently pointed at a buffer that does not need it, while the buffer that does
need it has no controller at all.

### Measurement that settled it (2026-07-27, `capture_fill` gauge)

Two consecutive runs, one routed app, same 96 kHz/48 kHz pair:

| t | `capture_drops` | `capture_fill` |
|---|---|---|
| 04:30:41 | 0 | 0.35 |
| 04:30:51 | 0 | 0.53 |
| 04:31:02 | 1728 | 0.78 |
| 04:31:12 | 4480 | 0.90 |
| 04:31:42 | 12480 | 0.80 |

The ring climbs from mid-scale to its brim within ~20 s of audio starting and
stays there, discarding ~190–280 samples/s for the rest of the session. A
monotonic ramp into saturation, not bursts — which is the signature of a
standing surplus and rules out the scheduling-jitter explanation. Both groups'
rings track each other almost exactly (`0:0.83,1:0.83`), as they must if the
cause is the shared DAC clock rather than anything per-app.

The user's ear agreed with the counter: popping "getting worse toward the end",
against a drop rate that was zero for the first ~10 s.

## Fix (implemented 2026-07-27)

Moved the drift controller onto the **capture** ring — the elastic buffer between
the two clocks, which is what async SRC regulation normally targets. The
governor keeps the output ring; the controller takes the input ring.

The control law's *sign is unchanged*: input consumed = `R_out / ratio`, so a
too-full capture ring must lower the ratio to consume input faster — the same
`ratio = 1 − corr` relation `clock.rs` already implements and tests. What
changes is which fill is fed to `DriftController::tick`, and the key it is
stored under.

Sizing note: `drift_target_fill`'s output-side geometry (governor threshold plus
half a block) does not transfer. The capture ring's disturbance is the poll
burst, not the governor sawtooth, so its target should sit near mid-capacity to
leave symmetric room for both signs of error.

### Keying — got this wrong once, then measured it

First attempt keyed the controller **per group**, on the reasoning that Media
and Game on one DAC have independent capture clocks. **They do not.**
Process-loopback capture for every app on a machine is driven by one WASAPI
engine clock at one pinned rate; two groups' capture streams do not drift apart
from each other. The clock that genuinely differs is the DAC's, which every
group routed there shares. Per **output** is the correct physical granularity.

Per-group keying was also a correctness bug, not just over-engineering.
`mix_tick` sums every group into one shared accumulator over a single span
(`out.filled = out.filled.max(write_len)`), so groups on one output must
produce identical frame counts per tick. Independent ratios make them diverge
and the shorter group's tail is silently zero-filled.

Measured, two groups feeding identical DC 0.25 into one output (a correct sum is
a flat 0.5):

| Ratios | min | max | samples below 0.4 |
|---|---|---|---|
| 1.005 / 0.995 (what the live loop held) | **0.2500** | 0.5000 | **608 / 61041** |
| 1.005 / 1.005 (shared, as shipped) | 0.5000 | 0.5000 | **0** |

1% of output samples dropping to half amplitude, in bursts at the tick rate
(~160/s). Audible as constant static that scales with source level, independent
of device rate (reproduced at 48k→48k) and of spatial. It was mistaken for the
capture-drop clicks it replaced. Pinned by
`groups_sharing_an_output_stay_frame_aligned`.

### Rejected alternatives

| Approach | Why not |
|---|---|
| Grow the capture ring | Delays saturation, does not prevent it. A standing surplus fills any finite buffer. |
| Drop oldest instead of newest | Relocates the artifact, still a splice, still audible. |
| Let the mixer over-drain when the ring is high | The frames have nowhere to go — the output ring would reject them instead, converting `capture_drops` into `output_drops`. |
| Pull the governor out of `pull_group_inputs` | The mixer accumulator is finite too; only `ratio` absorbs a rate mismatch. |

### What landed

| Change | Where |
|---|---|
| `capture_fill` gauge — per group, fullest of its pids' rings, at the high-water point | `EngineStats::capture_fill`, audit line |
| Frame-aligned overflow — a full ring drops whole frames, never half of one | `push_whole_frames` |
| Drift controller moved to the capture rings, aggregated per output | `clock.rs`, `output_capture_gauges` |
| Capture fill one-pole smoothed at the source before the controller sees it | `CAPTURE_FILL_SMOOTHING` |
| `SetOutputRatio` kept (per-output fan-out is a correctness constraint) | `mixer.rs` |
| `drift_target_fill` deleted — the output-side sawtooth offset it existed for is not the capture ring's geometry | `clock.rs` |

The **frame-aligned overflow** fix is independent of the control loop and worth
stating separately: the per-sample push loop could place a frame's L and drop
its R, because the mixer pops concurrently and frees slots mid-frame —
permanently shifting the ring's interleave so every later frame arrives
channel-swapped. Identical defect class to the pop side's (defect 4 above), but
where that one needed a race to fire, this one was the *steady state* of a ring
sitting at its brim. The user reported the static gone after this change alone,
with the popping still present — consistent with the two having separate causes.

#### The trap inside the idle guard

`pushed_samples` counts what the port **delivered**, not what the ring accepted.
Counting accepted samples reads naturally and is wrong in the one case that
matters: a saturated ring accepts nothing, so the group would be marked idle,
the controller would skip it, and the ratio would never be pulled down — the
bug would survive its own fix. Pinned by
`a_saturated_ring_still_counts_as_delivering`.

### MT16 — capture ring holds its level (BLOCKING)

Automated: 489 workspace tests, clippy clean. The control law's direction, its
per-group independence, the idle guard and the frame alignment are all pinned by
unit tests — but a PI loop's *stability* against a real device pair is not
something a synthetic plant proves.

Procedure: same as MT11 (music through a Media-group app, `SPLITSTREAM_AUDIT=1`),
but run it **at least 3 minutes** — the old failure took ~20 s to saturate and
the interesting question is whether the loop still holds after that. Watch for:

- `capture_fill` settling near 0.5 instead of ramping to the brim, and
  `capture_drops` flat after the first few seconds. That is the fix working.
- `applied_ratio` settling somewhere off 1.0 and **staying** there, rather than
  alternating between rails. The offset is the real clock error; a rail-to-rail
  wander means the loop is still not seeing what it controls.
- A ratio parked at a rail (0.995 / 1.005) with drops continuing means the true
  mismatch exceeds `max_correction` — the surplus measured (~2000–2900 ppm) sits
  inside the ±5000 ppm clamp, so this should not happen, but it is the failure
  mode to recognise.
- New `xruns`. The capture ring is now the regulated buffer, so an over-eager
  correction would show up as output starvation instead.
