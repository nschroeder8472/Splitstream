---
feature: mixer-demand-driven-wakeup
requirement_doc: null
created: 2026-07-24
status: complete
note: >
  Origin: direct user bug report (sustained idle CPU with the settings
  window backgrounded/idle), not the UX roadmap. No requirement spec.
  User benchmarked VoiceMeeter/SteelSeries Sonar/NVIDIA Broadcast (all
  real WASAPI-based audio processors) at <0.1% CPU idle, against
  Splitstream's ~2-3% sustained regardless of routing or UI state.
---

# Mixer Thread Demand-Driven Wakeup

> `mixer_loop` (one thread, serves every group/output) paces itself with
> `spin_sleep` on a fixed `tick_period` clock, running continuously from
> engine start regardless of whether anything is actually routed or the UI
> is even open. Every other thread that touches real hardware
> (`render_loop`) is genuinely event-driven and costs near-zero CPU at idle.
> This redesigns `mixer_loop`'s wakeup so it is woken by real demand (a ring
> buffer running low) instead of an independent clock.

## Grounding (2026-07-24, pre-Level-1)

Real code facts, verified this session before proposing any design:

- **`render_loop` (win-audio/render.rs, engine/runtime.rs:1120) is already
  fully event-driven.** One thread per output device, blocks on
  `port.wait_event(wait_timeout)` — a real `WaitForSingleObject` on the
  WASAPI event handle (`SetEventHandle` before `Start`, notes §4). No polling,
  no spin. This is *not* the cost the user measured.
- **`mixer_loop` (runtime.rs:1184) is the one thread with an independent
  clock.** `let sleeper = spin_sleep::SpinSleeper::default(); ... let budget =
  args.tick_period.saturating_sub(tick_start.elapsed()); sleeper.sleep(budget);`
  — runs every `tick_period` forever, whether or not any group has audio,
  whether or not the UI is open. `tick_period` = half the smallest render
  device's buffer period (`compute_tick_period`, runtime.rs:1415), typically
  ~5-10ms — so this thread wakes ~100-200 times/second, permanently, and
  `spin_sleep`'s busy-tail runs on every one of those wakeups on an
  RT-promoted thread (`args.sys.promote_rt_thread()`, line 1190).
- **Why it can't just block on a render event today:** `mixer_loop` serves
  *every* output device from one `Mixer` instance via one `rtrb` ring buffer
  per output (`OutputProducers = Vec<(OutputId, rtrb::Producer<f32>, usize)>`,
  runtime.rs:294). A single output's hardware event firing says nothing about
  whether a *different* output's ring is running low. The independent timer
  exists to keep every ring topped up regardless of which output needs it
  next — not because RT correctness specifically requires a fixed clock.
- **`supervisor_loop` (runtime.rs:1484, drift/recovery) is a third,
  lower-frequency `spin_sleep`-paced thread** (`cfg.tick`, drift-and-recovery's
  own cadence). Smaller contributor than `mixer_loop` given its coarser
  period, but the same shape. Explicitly out of scope for this design (see
  below) — worth a fast-follow once `mixer_loop`'s pattern is proven.
- **`pid_capture_loop` (runtime.rs:761) is a fourth, per-matched-pid
  `spin_sleep`-paced thread**, one per currently-routed session. Only costs
  CPU when sessions are actually captured (not "always on" the way
  `mixer_loop` is), and process-loopback capture may not expose an
  event-driven wait the way a full endpoint does. Also out of scope — a
  separate investigation, since "does `CapturePort` support an event handle"
  is an open question this session hasn't answered.
- **Ring buffer sizing already assumes a lead-time margin.**
  `ring_capacity_samples(device_period_s, ...)` (runtime.rs:731) sizes each
  ring off `port.poll_interval() * 2` — i.e. the existing design already
  assumes the producer runs somewhat ahead of the consumer's drain rate, not
  in lockstep. This margin is exactly what a low-watermark wake signal needs
  to have enough lead time to refill before the next real drain.
- **`update_telemetry` (runtime.rs:1243) runs once per mixer tick** and feeds
  `EngineStats.group_peak`/`output_peak`/`duck_depth_db`/`limiter_engaged` —
  consumed by `StatsReader` (UI, polled every frame) and by
  `supervisor_loop`'s drift correction (`ring_fill`/`applied_ratio`, read via
  `RingGauge`). A demand-driven mixer tick still needs to keep these
  reasonably fresh — this is a real constraint on how sparse "no demand" can
  make ticks, not just an audio-glitch concern.
- **No requirement doc** — this design's origin is a live user report,
  benchmarked against three real competing products at idle. The bar is
  "near-zero CPU when nothing is happening," not a specific number.

## Design: Level 1 -- Capabilities

**Approved 2026-07-24.**

1. **`mixer_loop` wakes on demand, not a fixed clock** — when every output's
   ring buffer has comfortable headroom and no group has new capture data
   waiting, the thread blocks (zero CPU) instead of spin-sleeping through an
   empty tick.
2. **A ring running low always wakes the mixer in time to avoid an
   underrun** — the wake signal fires with enough lead time (using the
   existing `poll_interval * 2` margin) that `mixer_loop` can refill before
   the render thread's next real drain.
3. **New capture data arriving also wakes the mixer** — a group with audio
   actively playing should tick promptly, not wait for a ring-low signal
   that may not fire if the group's own consumption pattern doesn't trigger
   it.
4. **No behavioral change to xrun/glitch behavior** — this changes *when*
   `mixer_loop` wakes, never what it does once awake. Existing RT-safety
   invariants (lock-free command queue, no allocation in the tick, warm
   smoothers) are unchanged.
5. **Telemetry consumers stay adequately fresh** — `EngineStats` (UI meters)
   and drift-and-recovery's `ring_fill` sampling must not silently go stale
   just because "nothing is happening" now means fewer ticks.
6. **Idle cost approaches `render_loop`'s** — with nothing routed and no
   audio playing, `mixer_loop` should cost close to zero, matching the
   competitive bar the user measured (VoiceMeeter/Sonar/Broadcast <0.1%).

Out of scope (this design): `supervisor_loop`'s and `pid_capture_loop`'s
independent `spin_sleep` pacing (related, same shape, but separate
investigations — noted as fast-follows, not solved here).

## Design: Level 2 -- Components

**Approved 2026-07-24.**

| # | Component | Layer | Change | Justification |
|---|---|---|---|---|
| 1 | `MixerWaker` | engine (orchestration) | **new** — thin `Clone`able wrapper around `std::thread::Thread`; `.wake(&self)` calls `.unpark()` | Lock-free wake signal callable from any producer thread; no `Condvar`/`Mutex` needed, no priority-inversion risk on an RT-adjacent path. |
| 2 | `mixer_loop` | engine | modified — `thread::park_timeout(FALLBACK_INTERVAL)` replaces `spin_sleep::SpinSleeper` pacing on a fixed `tick_period` clock. Tick body (drain commands, pull inputs, `mix_tick`, telemetry, flush outputs) unchanged in shape. | This is the actual fix: a real blocking primitive with zero busy-spin, instead of an independently-clocked timer with a busy tail. |
| 3 | `render_loop` | engine | modified — calls `waker.wake()` after draining its ring, every time its real hardware event fires | "I just consumed — refill me for next time." Synchronizes the mixer's cadence to the same real event `render_loop` already blocks on, at that event's own precision. |
| 4 | `pid_capture_loop` | engine | modified — calls `waker.wake()` after producing a block | "New audio arrived — mix it," bounded by the fallback interval if this signal is somehow missed. |
| 5 | Command-enqueue paths (`EngineHandle::apply_params`/`apply_dsp_chains`/`apply_spatial`, wherever `persistent`'s queue is pushed) | engine | modified — call `waker.wake()` after enqueueing | A live UI edit (fader drag) must still apply promptly while the mixer is parked, not wait for the next incidental render/capture wake. |
| 6 | `compute_max_block_frames` / buffer sizing | engine | modified — resize for "one wake ≈ one real render period" (removing the old implicit half-period doubling), with an explicit named margin constant | The old `tick_period` (half the render period) was both the sleep interval *and* the buffer-sizing basis — decoupling the wake mechanism from that period without re-deriving the sizing risks silent underruns. Must get this right, not hand-wave it. |
| 7 | `MixerThreadArgs` / `RunningGraph` | engine | modified — thread one shared `MixerWaker` to every wake-triggering site | Plumbing; no new architectural surface. |

**Components rejected:**

- **A `Condvar` + `Mutex` wake signal.** `park`/`unpark` gets the identical
  "wake me when something happens" semantics without any lock — safer for
  threads this close to the RT path, and Rust's own `park`/`unpark` is
  specifically documented to be race-free against an unpark arriving before
  the matching park (the token persists until consumed).
- **`WaitForMultipleObjects` across every render device's raw event handle.**
  Would require exposing native Win32 handles up through the
  platform-independent `RenderPort` trait just to multiplex waits across
  several outputs. Per-caller `unpark()` achieves the same "wake on any of N
  sources" effect without breaking that abstraction.
- **Redesigning `supervisor_loop`/`pid_capture_loop`'s own independent
  pacing.** Deferred — Level 1 scope.

**DDD note:** no domain change. `audio_core::Mixer`'s contract (`mix_tick`,
`push_group`, `take_output`) is untouched — same calls, same order, just from
a differently-paced caller. `MixerWaker` is pure orchestration-layer
infrastructure, not a domain concept.

## Design: Level 3 -- Interactions

**Approved 2026-07-24.**

**Flow A — startup.** `mixer_loop` spawns, its `JoinHandle::thread()` becomes
the one `MixerWaker`, cloned out to render/capture/command sites via
`RunningGraph`. First tick runs immediately (unchanged — the loop body runs
before the first park, same as it ran before the first sleep), so rings are
primed before any render event fires.

**Flow B — idle (nothing playing, no commands).** `mixer_loop` parks with a
bounded fallback timeout. No real wake fires; it wakes only on the fallback,
runs a normal tick (drains an empty command queue, pulls silence, ticks,
flushes silence), re-parks. Cost between wakes is a true OS block — no spin.

**Flow C — audio playing.** `render_loop`'s WASAPI event fires at real
hardware cadence (unchanged); after draining its ring it calls `waker.wake()`.
`mixer_loop` wakes, refills exactly one wake-unit's worth for the next render
period, re-parks. Mixer cadence is now the same event `render_loop` already
blocks on, at that event's own precision — same near-zero cost profile.

**Flow D — new capture data.** `pid_capture_loop` (still `spin_sleep`-paced
internally, out of scope) produces a block, calls `waker.wake()`. Mixer wakes
promptly rather than waiting for the next render-driven wake — adds at most
one render-period's latency in the worst case, within the engine's existing
buffering budget.

**Flow E — a live UI edit.** `apply_dsp_chains`/`apply_spatial` both already
funnel into `apply_params`'s single push loop (`self.apply_params(commands)`)
— so there is exactly **one** call site to wake from, not three. `apply_params`
pushes into the lock-free command queue, then calls `waker.wake()`. Applies
same-tick, same latency class as today.

**Flow F — coalesced wakes.** `unpark()` is a binary token, not a counter —
several wake calls before the next park collapse into one. Safe here because
the tick body already drains *everything* pending (all commands, all groups,
all outputs) on every tick regardless of which source woke it — a coalesced
wake never loses data.

**Flow G — multiple outputs.** All render/capture/command sites share one
`MixerWaker` clone. The tick body already iterates every
`output_producers`/`group_consumers` entry per tick, so no output goes
unserved regardless of which specific source triggered this wake.

**Flow H — telemetry freshness.** The fallback `park_timeout` bound
guarantees a tick at least every `MIXER_FALLBACK_INTERVAL` even with zero real
signals, so `EngineStats`/`ring_fill` can never go stale longer than that
bound (capability 5).

**Flow I — construction-order fix (found tracing the real code, not assumed).**
`spawn_render_threads` today creates each output's ring buffer *and* spawns
its `render_loop` thread in the same pass, called *before* `mixer_thread`
exists — but `render_loop` now needs a `MixerWaker`, which only exists once
`mixer_thread` is spawned, and `mixer_thread` needs the ring producers that
same function currently produces. Resolved by splitting the one function into
two: `prepare_output_rings` (builds ring buffers + returns ports/consumers
paired with producers, no thread spawned yet) runs first, then `mixer_thread`
spawns using the producers (yielding the `MixerWaker` via
`mixer_thread.thread().clone()`), then `spawn_render_threads` (now taking
`waker: MixerWaker`) spawns the actual `render_loop` threads. Construction-time
reordering only — no RT-thread behavior changes.

## Design: Level 4 -- Contracts

**Approved 2026-07-24.**

```rust
/// Wakes the mixer thread out of its park. Clone, no lock:
/// `std::thread::Thread` is already Send + Sync + Clone.
#[derive(Clone)]
struct MixerWaker(std::thread::Thread);
impl MixerWaker {
    fn wake(&self) { self.0.unpark(); }
}

// mixer_loop: sleeper/tick_period removed entirely, tail becomes:
std::thread::park_timeout(MIXER_FALLBACK_INTERVAL);
const MIXER_FALLBACK_INTERVAL: Duration = Duration::from_millis(100);

// render_loop gains a parameter, calls it right after draining (before write):
fn render_loop(.., waker: MixerWaker) { .. waker.wake(); port.write(&buf)?; .. }

// pid_capture_loop gains a parameter, calls it right after producing a block:
fn pid_capture_loop(.., waker: MixerWaker) { .. waker.wake(); }

// EngineHandle::apply_params -- the ONE command call site (apply_dsp_chains/
// apply_spatial both already funnel here) -- calls it once, after the push loop:
pub fn apply_params(&self, cmds: Vec<MixerCommand>) -> Result<(), EngineError> {
    let running = self.running.lock().unwrap();
    let rg = running.as_ref().ok_or(EngineError::AlreadyStopped)?;
    /* push loop unchanged */
    rg.mixer_waker.wake();
    Ok(())
}

// RunningGraph gains one field:
mixer_waker: MixerWaker,

// Buffer-sizing: compute_tick_period -> compute_wake_unit_period, drops the
// old "/2.0" halving, applies an explicit named margin instead:
const WAKE_MARGIN: f64 = 1.25; // 25% headroom over one exact render period
fn compute_wake_unit_period(renders: &[(OutputId, Box<dyn RenderPort>)]) -> Duration {
    let min_period_s = renders.iter()
        .map(|(_, r)| r.period_frames() as f64 / r.format().sample_rate.max(1) as f64)
        .fold(f64::INFINITY, f64::min);
    let period_s = if min_period_s.is_finite() { min_period_s * WAKE_MARGIN } else { 0.005 };
    Duration::from_secs_f64(period_s)
}
// compute_max_block_frames's call site is unchanged otherwise; ring_capacity_samples
// is untouched -- it's already based on the render device's own real period via
// RING_PERIOD_MARGIN, never on tick_period, so it needs no change.

// spawn_render_threads split in two (Flow I's construction-order fix):
fn prepare_output_rings(renders: Vec<(OutputId, Box<dyn RenderPort>)>)
    -> (Vec<(OutputId, Box<dyn RenderPort>, rtrb::Consumer<f32>)>, OutputProducers);
fn spawn_render_threads(prepared: .., waker: MixerWaker, stop: &Arc<AtomicBool>, ..)
    -> Vec<JoinHandle<()>>;
```

### Test contracts

| Test |
|---|
| `mixer_loop_ticks_immediately_on_first_run_before_any_wake` — Flow A invariant |
| `render_loop_wakes_the_mixer_after_draining_its_ring` |
| `pid_capture_loop_wakes_the_mixer_after_producing_a_block` |
| `apply_params_wakes_the_mixer_after_enqueueing_commands` |
| `a_parked_mixer_still_ticks_within_the_fallback_interval` — Flow B/H |
| `compute_wake_unit_period_applies_the_margin_not_a_half_period_split` — regression against notes §5's superseded halving |
| `mixer_loop_drains_everything_pending_regardless_of_which_source_woke_it` — Flow F/G coalescing invariant |

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-24 | **`mixer_loop`'s wake mechanism changes from an independently-clocked `spin_sleep` to `thread::park`/`unpark`, triggered by real render/capture/command events.** | `render_loop` is already event-driven and costs near-zero CPU; `mixer_loop`'s own independent timer was the actual gap against VoiceMeeter/Sonar/Broadcast, not wake *frequency*. Rejected: `Condvar`+`Mutex` (same effect, adds a lock on an RT-adjacent path); `WaitForMultipleObjects` across raw render handles (breaks the `RenderPort` platform abstraction). |
| 2 | 2026-07-24 | **Buffer sizing (`compute_max_block_frames`'s basis) is recomputed from a full render period plus an explicit `WAKE_MARGIN` (1.25x), not the old implicit half-period doubling.** | The old halving (documented as "notes §5") existed specifically to compensate for `spin_sleep`/coarse-timer imprecision under a polled model; a real park/unpark wake doesn't carry that same imprecision, but *some* margin is still kept for scheduling jitter between "render drained" and "mixer refilled." Rejected: keeping the exact old halving unchanged (simplest, but wakes at 2x real render frequency for no reason once the wake source *is* the render event itself — doesn't save the CPU this design exists to save). |
| 3 | 2026-07-24 | **`apply_dsp_chains`/`apply_spatial` need no separate wake call — both already funnel into `apply_params`'s single push loop.** | Found by reading the real code, not assumed from the Level 2 component list's original three-call-site sketch. One call site, not three. |
| 4 | 2026-07-24 | **`spawn_render_threads` is split into `prepare_output_rings` + `spawn_render_threads`, reordering ring creation ahead of thread spawn.** | `render_loop` needs a `MixerWaker`, which only exists once `mixer_thread` is spawned; `mixer_thread` needs the ring producers `spawn_render_threads` currently creates in the same pass as spawning `render_loop` itself — a real circular dependency in the current code, resolved by separating "build the rings" from "spawn the threads." Construction-time only, no RT behavior change. |
| 5 | 2026-07-24 | **`supervisor_loop` and `pid_capture_loop`'s own independent `spin_sleep` pacing are explicitly out of scope**, deferred to a fast-follow. | Same shape, smaller/conditional cost (supervisor is coarser-period; capture only costs CPU when sessions are actually routed, unlike `mixer_loop` which runs regardless). Keeping this design's surface to the one thread that's *always* running regardless of routing or UI state — the actual root cause of "no dropoff, ever." |
| 6 | 2026-07-24 | **Design approved at Level 4. Status set to `approved` — ready for implementation.** | All four level sections persisted; no open questions. |

## Open Questions

*(none — every judgment call raised during design was resolved and logged)*

## Design Summary

**Drift check: skipped — `requirement_doc` is null.** Origin is a direct
user bug report benchmarked against three competing products, not a
requirement spec — no Scenarios/ACs or `## Technical Constraints` to compare
Level 4 against.

**Components and layers** — everything is in `engine::runtime` (orchestration
layer); `audio_core::Mixer`'s contract is untouched.

| Component | Change |
|---|---|
| `MixerWaker` | new — `Clone`able `std::thread::Thread` wrapper, `.wake()` = `.unpark()` |
| `mixer_loop` | `spin_sleep` on a fixed clock -> `park_timeout(MIXER_FALLBACK_INTERVAL)` |
| `render_loop` | gains `waker: MixerWaker`, wakes after draining |
| `pid_capture_loop` | gains `waker: MixerWaker`, wakes after producing |
| `EngineHandle::apply_params` | wakes after enqueueing (covers `apply_dsp_chains`/`apply_spatial` too) |
| `RunningGraph` | gains `mixer_waker: MixerWaker` field |
| `compute_tick_period` -> `compute_wake_unit_period` | drops the old halving, applies `WAKE_MARGIN` explicitly |
| `spawn_render_threads` | split into `prepare_output_rings` + `spawn_render_threads` to resolve the mixer-waker/producer circular dependency |

**Key contracts** — `MixerWaker::wake()` is the whole mechanism; every
producer of work for the mixer thread (a render event, new capture data, an
enqueued command) calls it, and the mixer's own tick body already drains
everything pending regardless of which source woke it, so coalesced wakes
lose nothing.

**Architectural constraints honored**

- No change to `audio_core::Mixer`'s contract or call order — same
  `mix_tick`/`push_group`/`take_output` sequence, differently-paced caller.
- No new lock on an RT-adjacent path — `park`/`unpark` is lock-free by
  construction.
- `RenderPort`'s platform-independence is preserved — no raw Win32 handles
  cross the trait boundary.
- Xrun/glitch behavior is unchanged by design (capability 4) — only *when*
  `mixer_loop` wakes changes, never what it does once awake.

**Domain model** — untouched. No aggregate, entity, or value object;
`MixerWaker` is orchestration-layer infrastructure.

**Open questions resolved during design** — the buffer-sizing/wake-frequency
coupling (decision 2, found by tracing `compute_max_block_frames`'s actual
dependency on the old `tick_period` halving before proposing any change); the
`apply_params`/`apply_dsp_chains`/`apply_spatial` call-site count (decision
3, corrected from three assumed sites to the one real site); the
construction-order circular dependency between `mixer_thread` and
`render_loop` (decision 4).

**Known accepted gap** — `supervisor_loop` and `pid_capture_loop` keep their
own independent `spin_sleep` pacing (decision 5); a routed session's capture
thread and the drift controller's tick still cost what they cost today.

## Implementation

**Complete 2026-07-24.** Single file touched: `crates/engine/src/runtime.rs`
(engine::runtime, orchestration layer — no other crate needed a change).
Built in the planned inside-out order (MixerWaker → compute_wake_unit_period
→ prepare_output_rings/spawn_render_threads split → render_loop → pid_capture_loop/
CaptureControl → mixer_loop → RunningGraph/build_running_graph wiring →
apply_params). Full workspace `cargo build`/`cargo test`/`cargo clippy` all
clean; all 7 Level 4 test contracts implemented and passing, plus 3 existing
tests updated for the new `waker`/`MixerWaker` parameter
(`pid_capture_loop_exits_quietly_on_a_read_failure`,
`render_loop_reports_non_invalidated_faults_as_other`) or renamed
(`set_output_ratio_command_updates_applied_ratio_stat` →
`a_parked_mixer_still_ticks_within_the_fallback_interval`, sleep bumped past
`MIXER_FALLBACK_INTERVAL` since it bypasses `apply_params`'s wake path by
design).

| # | Date | Decision | Reasoning |
|---|------|----------|-----------|
| 7 | 2026-07-24 | **`supervisor_loop`'s own drift-correction command pushes (direct to `persistent.commands`, bypassing `apply_params`) do NOT get a `waker.wake()` call.** Left fallback-bound (up to `MIXER_FALLBACK_INTERVAL` to apply during silence). | User-confirmed judgment call raised during implementation (not anticipated at design time): under the old fixed-clock design this never mattered (mixer ticked every ~5-10ms regardless); under demand-driven wake it's a real behavior change, but inaudible (nothing playing during silence) and moot during real audio (render-driven wakes fire constantly anyway). Matches decision 5's existing scope boundary — no new call site added. |

**Test-design finding (harvested as an operational learning):** the first
draft of `mixer_loop_ticks_immediately_on_first_run_before_any_wake` and
`mixer_loop_drains_everything_pending_regardless_of_which_source_woke_it`
asserted on actual mixed audio output and failed even though the redesign was
correct — `audio_core::Src` always runs a real `rubato::SincFixedIn`
resampler (kept variable-ratio for drift correction even at nominal 1:1),
which has genuine warm-up latency unrelated to whether `mixer_loop` ticked.
Fixed by asserting on `RingGauge.active` instead (`pull_group_inputs`' own
full-block-fill bookkeeping, upstream of the resampler entirely).
