---
feature: session-handoff
created: 2026-07-27
status: >
  Three defects found and fixed, then a FOURTH found by running MT17 (2026-07-27,
  22:30): unassigning a group's last app silenced the whole output permanently.
  Fixed. MT17 re-run PASSES — the static is gone and the unassign no longer
  stalls. 496 tests green, clippy clean. One open item, new and separate:
  light popping while two groups are BOTH producing audio, with the drift
  ratio parked on its lower clamp rail. See "Open: the two-group popping".
related:
  - .lattice/context/output-rate-truncation.md
  - .lattice/context/audio-flow-control.md
---

# Session handoff — the routed-audio static

One session, one symptom, three distinct causes. The symptom stayed roughly
constant while its cause changed underneath twice, which is why this took as
long as it did.

---

# Pick up here

MT17 is **done and passing** (2026-07-27 22:30, results at the bottom of the
MT17 section). The static is gone; so is the unassign stall it exposed.

What is left is one new, separate symptom — light popping while two groups are
both producing audio. It is not the static and not the stall: both of those are
now measured clean. Start at "Open: the two-group popping".

---

# The three defects

## 1. Capture rings were an unregulated buffer

**Symptom:** light static plus occasional pops. `capture_drops` climbing
~190–280 samples/s from ~10 s after audio started, never recovering.

**Cause:** the drift controller regulated the *output* ring — which the
governor (`group_may_push`) already holds at its threshold by withholding
ticks. Two controllers on one actuator, so `ratio` became an unobservable free
variable: it integrated on the governor sawtooth's phase noise and wandered
onto both ±0.5% clamp rails. Since `SincFixedIn` consumes a *fixed* input
chunk, `input consumed/sec = R_out / ratio`, so whatever value `ratio` drifted
to silently dictated the capture drain rate. The capture ring — the only
elastic buffer between the capture clock and the DAC crystal, and the only one
with no controller on it — absorbed the difference until it saturated, then
discarded every excess sample forever.

**Fix:** the drift controller now measures the **capture** rings, aggregated
per output (`output_capture_gauges`, fullest ring wins). `drift_target_fill`
deleted — its sawtooth offset was output-side geometry that does not transfer.

**Verified on hardware:** `capture_drops` went to 0 and stayed there;
`capture_fill` settles at 0.49–0.51 against a 0.5 target.

### Two traps inside this one

- **The control law's sign does not flip.** It reads as though a too-full
  capture ring should raise the ratio. It must *lower* it: input consumed =
  `R_out / ratio`, so draining faster means a smaller ratio. Same direction the
  old output-side law used, for the opposite reason.
- **The idle guard must count what the port DELIVERED, not what the ring
  ACCEPTED.** A saturated ring accepts nothing. Counting accepted samples marks
  the group idle exactly when its surplus needs clearing, the controller skips
  it, the ratio never comes down — the bug survives its own fix. Pinned by
  `a_saturated_ring_still_counts_as_delivering`.

## 2. The controller railed on measurement noise

**Symptom:** `applied_ratio` alternating between 0.995 and 1.005 roughly every
tick, never settling.

**Cause:** the published capture fill was the *instantaneous* ring level, which
is packet phase, not level — a whole poll packet lands, then the mixer drains
it. Measured live at 0.27–0.77 tick to tick against a 0.5 target. At `kp=0.05`
that demands 2.5× the correction clamp every single tick, so the controller sat
on alternate rails.

**Fix:** one-pole smoothing at the source (`CAPTURE_FILL_SMOOTHING = 0.02`,
~500 ms constant, several times the controller's 100 ms tick), seeded at 0.5 so
it does not spend the ramp correcting an error that was never there.

**Verified on hardware:** `applied_ratio` now sits at 0.9989–1.0010 — that
offset is the real DAC-vs-capture clock error, and it holds.

## 3. The shared output span advanced on behalf of one group — THE STATIC

**Symptom:** constant static, scaling with source loudness, unaffected by DAC
rate (tested 48 kHz and 96 kHz) and by spatial on/off. The user isolated the
trigger exactly: **assigning any app to a second group starts it, unassigning
stops it, and whether that app plays audio is irrelevant.**

**Cause:** `mix_tick` emitted a block whenever *any* group produced
(`out.filled = out.filled.max(write_len)`). Groups sharing an output cross
their SRC chunk boundaries on **different** ticks — their pids deliver packets
independently, and the mixer also ticks on render wakes carrying little new
capture input. So on the tick where group 2 completed a chunk and group 1 had
not, a block went out with group 1 **silent**, while group 1's audio was still
sitting in its resampler. A silence block spliced into the music every time
their boundaries fell apart.

With one group this was invisible: producing nothing meant `filled = 0` and
nothing was emitted, so the stream stayed continuous. It needs two groups on
one output, which nothing tested and no earlier configuration hit.

**This was pre-existing**, not introduced this session.

**Fix:** each group's SRC output became a FIFO (`GroupState::resampled_samples`),
and the output's span is now the **least** any group with audio in flight can
supply, not the most. A group that runs ahead parks its surplus for a tick. A
group with nothing in flight at all is genuinely silent and does not gate —
otherwise one idle group would stall its output forever.

**Measured, same test, only the span rule varying:**

| Span rule | Output samples near silence |
|---|---|
| `max` (old) | **49.9%** |
| `min` (fixed) | **0%** |

Control, one group with identical feeding: 0% under both.

## 4. In-flight audio gated a span that would never come — THE UNASSIGN STALL

Found by running MT17, 2026-07-27 22:06. Not a regression from defect 3's fix
so much as its other half: once gating existed at all, it had no bound.

**Symptom:** step 2 (assign a second group) was clean — the static really was
gone. Step 3, **unassigning**, killed audio on that output entirely and
permanently. `ring_fill=0:0.00` and `output_peak=0.0000` for 96 s straight
while `group_peak` still showed the live group producing (0.19 at one point),
`applied_ratio` pinned at the 1.00500 rail, `xruns` never moved.

**Cause:** `Src::has_audio_in_flight()` is true whenever `pending_in_frames > 0`
— a partial SRC chunk. A partial chunk only completes when more input arrives.
When a group's last pid goes away nothing ever pushes to it again, so the
predicate stayed true forever, the group stayed in `mix_tick`'s gating set with
`resampled_samples == 0`, and `min()` held the shared span at zero for the rest
of the session.

Wider than the unassign: **any** group fed and then cut off mid-chunk stalls its
whole output. MT17 step 1 only passed because the second group had never been
fed at all (`pending_in_frames == 0`), which is the case
`a_group_with_nothing_in_flight_does_not_stall_its_output` already pinned.

**Fix, two parts:**

- `pull_group_inputs` calls `Mixer::discard_group_partial_input` the tick a
  slot's pids go empty. Exact signal, fires on the same tick as the unassign,
  so no other group on that output hears anything.
- `mix_tick` bounds gating by **parking capacity**, not by time: in-flight audio
  only holds the span back while the groups running ahead can still park their
  surplus. `resampled` holds two output blocks — one in flight, one parked —
  so once any group on that output holds a full block, waiting longer would
  leave the next block's input unconsumed.

**The bound was written as a tick count first, and that was wrong.** 64 ticks
of stall overflows `resampled` and trips the `resampled scratch undersized for
one block` debug assert — which in a release build is not an assert, it is
silently discarded input. That is also why `capture_fill` sat at a flat 0.13
during the stall instead of climbing: input was being consumed and thrown away,
not backing up. The parking capacity is the real limit and needs no tuning.

**Verified on hardware:** MT17 re-run 2026-07-27 22:30 — unassign at 02:31:00,
`ring_fill` kept sawtoothing 0.50–0.81 and `output_peak` stayed live for the
following 96 s. Repeat assign/unassign cycles behave the same.

Pinned by `a_group_that_stops_being_fed_stops_gating_its_output` (the capacity
bound), `unassigning_a_groups_last_pid_frees_its_outputs_span` (the discard),
and `a_group_that_loses_its_last_pid_stops_gating_its_outputs_span` (engine —
asserts the FIRST tick emits; without the discard it reads `[0, 402, 402, …]`,
the backstop recovering a tick late). Each was A/B'd by reverting the one line
under it.

---

# What changed, by file

| File | Change |
|---|---|
| `audio-core/src/mixer.rs` | `resampled` became a FIFO (`resampled_samples`); span rule `max` → `min` over gating groups; `SetOutputRatio`'s per-output fan-out documented as a correctness constraint |
| `audio-core/src/resample.rs` | `Src::has_audio_in_flight()` — lets `mix_tick` tell "silent" from "not delivered yet" |
| `engine/src/clock.rs` | `DriftController` measures capture rings; `drift_target_fill` deleted; module doc explains which buffer it regulates and why |
| `engine/src/runtime.rs` | `capture_fill` gauge + smoothing; `pushed_samples` idle guard; `push_whole_frames`; `output_capture_gauges`; supervisor rewired; `applied_ratio` telemetry |
| `app/src/main.rs` | `capture_fill` in the audit line |
| `audio-core/src/resample.rs` | `Src::discard_partial_input()` — defect 4 |
| `audio-core/src/mixer.rs` | gating bounded by parking capacity; `Mixer::discard_group_partial_input` — defect 4 |
| `engine/src/runtime.rs` | `pull_group_inputs` discards on an empty pid set — defect 4 |

## Also fixed on the way (independent of the static)

**Frame-aligned capture overflow** (`push_whole_frames`). The per-sample push
loop could place a frame's L and drop its R, because the mixer pops
concurrently and frees slots mid-frame — permanently shifting the ring's
interleave so every later frame arrives channel-swapped. Same defect class as
the pop side's (`pull_group_inputs_never_pops_a_partial_frame`), but where that
one needed a race, this was the *steady state* of a brim-pinned ring.

---

# A dead end worth not repeating

Mid-session the drift controller was re-keyed **per group** on the reasoning
that two groups on one DAC have independent capture clocks. **They do not.**
Process-loopback capture for every app on a machine runs off one WASAPI engine
clock at one pinned rate; the clock that genuinely differs is the DAC's, shared
by every group routed to it. Per-output is the correct physical granularity.

That change also *caused* a regression — independent ratios make groups on one
output produce different frame counts, and the old `max` span rule zero-filled
the shorter one's tail (1% of samples at half amplitude). Reverted the same
session. Pinned now by `groups_sharing_an_output_stay_frame_aligned`.

Both facts are recorded in `MixerCommand::SetOutputRatio`'s doc comment so the
next person does not re-derive them.

---

# MT17 — does the static stop (BLOCKING, and the only open question)

Everything else in this session is either measured clean on hardware or pinned
by a test. This is not.

Setup: quit any running instance, `cargo build --release`, then:
```
$env:SPLITSTREAM_AUDIT = "1"
target\release\splitstream.exe
```

Procedure — the user found a reliable switch, so this is a controlled A/B in
one continuous session:

1. Music playing through **one** group only. ~15 s. (Static absent before, too.)
2. **Assign any app to the second group.** ~20 s. This is where the static
   started every time before.
3. **Unassign it.** ~10 s.

| | Pass | Fail |
|---|---|---|
| Audio in step 2 | Clean, indistinguishable from step 1 | Static returns |
| `capture_drops` | 0 throughout | Any climb |
| `xruns` | 0 throughout | Any climb — would mean the `min` span rule is starving the output ring |
| `applied_ratio` | Settled near 1.0 | Rail-to-rail |

`xruns` is the specific risk this fix introduces: the span now advances at the
slowest group's rate, so if a group stalls it holds the output back. The
"nothing in flight does not gate" rule exists to prevent that, and
`a_group_with_nothing_in_flight_does_not_stall_its_output` pins it, but only
hardware shows whether the real timing agrees.

## MT17 results

Run 1, 2026-07-27 22:05 (`routes` 1 → 2 → 1):

| Phase | Result |
|---|---|
| 1, one group | Clean. `capture_fill=0.49–0.52`, `applied_ratio≈1.0000`, all drops 0. |
| 2, second group assigned (17 s) | **Static gone.** `od=0 cd=0 xr=0`, both rings at 0.49–0.51, ratio settled. |
| 3, unassign | **FAIL** — audio stopped entirely and never came back. Defect 4 above. |

Run 2, 22:27, after the defect-4 fix, with phase 2 held for 60 s and an extra
assign/unassign cycle:

| Phase | Result |
|---|---|
| 1, one group | Clean, no popping. |
| 2, second group assigned **and playing** | Slight popping. `xruns=0`, but `output_drops` climbing in bursts. |
| 3, unassign | **PASS.** Audio continues, popping stops, `ring_fill` sawtooths 0.50–0.81 for 96 s. |
| 4, re-assign then unassign again | Same as 2 and 3 — no state left over between cycles. |

MT17's own criteria: audio in step 2 clean of the static ✅, `capture_drops` ✅
(+128 over 90 s, versus 190–280/s before), `xruns` 0 throughout ✅ — the `min`
span rule does **not** starve the output ring, which was the specific risk the
fix introduced. `applied_ratio` settled ✅ except during the two-group phase,
which is the new item below.

---

# Open: the two-group popping

The one thing MT17 did not settle, and a different symptom from either the
static or the stall.

**Symptom:** light popping, only while two groups are **both producing audio**.
Stops the moment the second group is unassigned. Much lighter than the static
was, and it does not build up.

**Objective correlate:** `output_drops` climbs in bursts of 560–900 samples
(≈3–5 ms at 96 kHz, so 6 bursts ≈ 6 audible pops), *only* during two-group
phases. Every burst lands on a tick where `applied_ratio` is pinned at
**0.99500 — the lower clamp rail** — with `capture_fill` at 0.59–0.69 against a
0.5 target.

This is precisely the failure mode `output-rate-truncation.md`'s MT16 section
names and says should not happen: "a ratio parked at a rail with drops
continuing means the true mismatch exceeds `max_correction`". So either the
mismatch really does exceed ±5000 ppm with two producing groups, or something
other than a clock mismatch is filling the capture ring.

**Control, same build's predecessor** — run 1's two-group phase had a second
group that was assigned but **silent**: `od=0`, both rings 0.49–0.51, ratio
settled. So it takes two groups actually *producing*, not merely two groups
routed. That also means this cannot be attributed to the defect-4 fix on the
evidence available: the two runs differ in workload as well as in build.

## What the oracles ruled OUT, 2026-07-27

Two ignored diagnostics in `mixer.rs`, runnable with
`cargo test -p audio-core probe_ -- --ignored --nocapture`. Between them they
kill three hypotheses. **None of them is the cause**, and that is the useful
result — the span rule and the governor are both exonerated in steady state.

### 1. `min` couples the groups, so a surplus backs up (DEAD)

A sustained rate *deficit* does produce stalls and 5–20% notches. But the live
trace has `cf1` holding 0.36–0.52 for 90 s rather than draining, so the two
groups' rates match. Not this.

### 2. A missed tick costs a group a block, permanently (DEAD)

`probe_a_starved_group_loses_a_block_per_missed_tick`. Both producers at the
same average rate, group 2's arrivals bursty, **with a capture ring in front of
each group** and the mixer popping at most one block per tick from it — as
`pull_group_inputs` does.

| gap every | samples at half amplitude | max tick | ring B peak |
|---|---|---|---|
| never | 0% | 608 | 1 block |
| 50 ticks | **0%** | 608 | 2 blocks |
| 10 ticks | **0%** | 608 | 2 blocks |

Jitter alone notches **nothing**. The group carries a bounded one-block backlog
and emission stays flat.

**This one was briefly recorded as the diagnosis and it was wrong.** The first
version of the probe fed the mixer directly, with no ring, which models a group
receiving genuinely *less* audio rather than receiving it *late* — `push_group`
truncates at one block, so a deficit fed that way is permanent. Adding the ring
made the notching vanish entirely. A probe that omits a buffer the real system
has does not model the real system.

### 3. Parked surplus is pushed into a ring the governor is withholding (DEAD)

`probe_parked_surplus_pushes_into_a_withheld_ring`. Misaligned groups so one is
always parking, an output ring drained slightly slower than production so the
governor actually engages (63 withheld ticks at threshold 0.75).
`emitted_while_withheld` = 0, rejects = 0. A withhold pushes every group an
empty block, the starved group gates, and the span collapses to zero — the
`min` rule holds the surplus back precisely when there is no room for it.

## Where that leaves `output_drops`

`od` counts frames an output ring rejected. For that to happen, either:

- the span exceeded one block — but `accum` **is** exactly one output block
  (`Mixer::new`), so `take_output` cannot return more; or
- the governor's headroom snapshot was stale by flush time — but it is sampled
  at the top of the same tick and only the render thread touches the ring in
  between, and it only *drains*; or
- `group_may_push`'s own overflow guard (`filled + block_out_frames <=
  capacity`) was computed against a `block_out_frames` smaller than what was
  actually produced.

The third is the only one left standing, and the drift ratio is the obvious way
production could exceed the nominal block — but ±0.5% of a block is single-digit
frames, and the observed bursts are 560–900 samples.

## Measured on hardware, 2026-07-27 23:0x — the answer

`EngineStats::last_output_reject`, in output frames. Every reject in the run:

| | span | free | cap | budget |
|---|---|---|---|---|
| 03:03:18 | 858 | 705 | 3840 | 1216 |
| 03:03:51 | 1017 | 717 | 3840 | 1216 |
| 03:03:53 | 1212 | 825 | 3840 | 1216 |
| 03:06:09 | 1012 | 783 | 3840 | 1216 |
| 03:06:40 | 1020 | 710 | 3840 | 1216 |

**`span <= budget` always.** The mixer never overproduced; `group_may_push`'s
arithmetic was never wrong. Third hypothesis dead, by measurement.

**The ring was ~80% full before each push.** free ≈ 710–825 of 3840 means fill
≈ 3100, and one block is 1216 = 31.7% of capacity. The governor admits below
50%, pushes a block, lands at 81.7% — exactly the documented sawtooth
(`ring_fill` 0.50–0.81). An 81% ring is the *normal* post-push peak.

**So the fault is emission on the following tick, before the render thread has
drained.** Fill is still ~81%, the governor withholds and pulls no input — and
the mixer emits anyway, out of parked surplus. That span meets a ring with ~710
frames free and the remainder is rejected. Parking only exists when groups gate
each other, which is why one group never shows this.

**The governor regulates input on the assumption that no input means no
output.** Parked surplus broke that assumption.

### Why oracle 3 said this could not happen

It made every group misaligned-but-active, so on a withhold tick some group
always had nothing parked and `min` took the span to zero. The real case has
groups whose parked amounts are non-zero *simultaneously*, which needs
independent per-group arrival phase.

## The fix, and what is known about it

Gate emission on the same per-output headroom the input pull already answers
to: no room, no span, surplus stays parked — which is what the FIFO is for.
It cannot be done by skipping `flush_outputs` alone, because `take_output`
clears the accumulator: skipping the flush while still running `mix_tick` would
superimpose two ticks of audio in `accum`. The skip belongs inside `mix_tick`,
per output.

`probe_span_rule_against_group_count` measures it at the real 48k→96k pair for
2–5 groups sharing one output:

| groups | notched | rejected | emitted | zero-span ticks | gated ticks |
|---|---|---|---|---|---|
| 2–5, gate off | 0% | 0 | 3538424 | 90 | — |
| 2–5, gate on | 0% | 0 | 3538424 | 0 | 90 |

Nothing degrades with group count, and the gate is a pure relabelling of the
ticks that already emitted nothing — no added latency, no lost frames, rate
conserved. That is the *healthy* regime only.

**What is still unproven: the fix's behaviour under the fault**, because no
oracle here reproduces the fault. Every model feeds each group a uniform amount
per tick; the rejected spans on hardware (858, 1017, 1212, 1012, 1020 against a
1216-frame block) are partial and unequal, so the real cadence delivers
fractions of a block with independent per-group phase. A faithful model needs
that. The alternative is to implement the gate — the invariant "never offer the
ring more than it can take" is unconditionally correct and the trace proves it
is violated — and measure on hardware, where the fault demonstrably occurs.

### Scaling risks that are not the gate's fault

Worth separating, since they get likelier as groups are added to one output:

- `min` over gating groups means any one group that cannot supply throttles all
  of them, and the chance that some group is mid-chunk on a given tick rises
  with group count.
- The parking-capacity backstop is a per-output release: one runaway group
  filling its parking stops *every* in-flight group on that output from gating,
  so their audio is skipped. Measured at 0% up to 5 groups in the healthy
  regime, but it is a shared-fate mechanism by construction.

## If MT17 still shows static

Do not theorise from the symptom — this session's three wrong turns all sounded
plausible and all compiled. Two things that actually worked:

- **The audit trace with the switch flipped inside one session.** `routes=1` →
  `routes=2` → `routes=1` in one log is what eliminated the flow-control domain
  entirely (every counter identical across all three phases).
- **An offline oracle in `mixer.rs`'s tests.** A probe that reproduces the
  defect deterministically, then A/B'd by reverting the one line under
  suspicion. That is what finally located defect 3 after three earlier probe
  designs came back clean — and the clean ones were informative too, since they
  ruled the summing path out.

The remaining unexplored area, if it comes to that, is the capture side: what
`PROCESS_LOOPBACK` hands over versus what the app actually rendered. Everything
downstream of that is now either measured clean or pinned.

---

# Status

- **496 workspace tests, clippy clean.**
- Hardware-verified: `capture_drops` flat, `capture_fill` 0.50, `applied_ratio`
  settled, `xruns` = 0 — and, this run, the static gone and the unassign stall
  gone (MT17).
- Open: the two-group popping, above.

## Known flaky tests

`runtime::tests::mixer_loop_ticks_immediately_on_first_run_before_any_wake` and
`mixer_loop_drains_everything_pending_regardless_of_which_source_woke_it` each
failed once under full-workspace parallel load and passed 5/5 in isolation.
Both use a 30 ms sleep against real thread scheduling. Unrelated to this work,
but they will keep doing it — worth a separate look.

## Uncommitted work

Nothing has been committed for two sessions. `git status`:

```
 M crates/app/src/event_pump.rs        crates/audio-core/src/mixer.rs    (+420)
 M crates/app/src/main.rs      (+50)   crates/audio-core/src/resample.rs (+33)
 M crates/audio-core/src/lib.rs        crates/engine/src/clock.rs        (+109)
 M crates/win-audio/src/format.rs      crates/engine/src/runtime.rs      (+742)
 M crates/win-audio/src/render.rs
?? .lattice/context/output-rate-truncation.md
```

~1238 insertions / 169 deletions across 9 files. This spans **two** distinct
pieces of work — the original output-rate truncation fix (previous session,
hardware-validated) and this session's three defects. Worth splitting into at
least two commits when MT17 lands. `.lattice/context/output-rate-truncation.md`
and this file are both untracked and should go in with them.
