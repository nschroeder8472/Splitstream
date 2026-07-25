---
feature: audio-flow-control
requirement_doc: null
created: 2026-07-25
status: complete
review: >
  Post-implementation review 2026-07-25 (see that section). Blueprint as
  designed is fully implemented. Two amendments: Level 3 Flow G's priming
  mechanism is wrong (the governor has no floor) and is corrected by a
  `render_loop` priming phase gated on MT3; B4's two test contracts plus
  Flow G's were unimplemented and are now written and mutation-checked.
note: >
  Origin: `.lattice/reviews/2026-07-25-end-to-end-audit.md`, itself forced by a
  live user bug report (routed audio = occasional pops only, second output
  apparently dead, ~3.8% CPU whenever anything is routed). No requirement spec.
  Scope is the audit's B17 + B1/B2/B3/B4 (one coherent flow-control problem)
  plus B7/B8, which this blueprint's own fix activates. B5/B6 and B9-B16/B18
  are explicit fast-follows, not in scope.
---

# Audio Flow Control

> Every stage boundary in the capture -> mix -> render pipeline currently moves
> the wrong number of frames: capture under-reads by ~2x, the mixer produces a
> full block per wake regardless of demand, partially-filled blocks are
> zero-padded into the stream, and the render loop pops more than the device
> can accept and discards the remainder. This restores one invariant at all
> four boundaries — never move more than the receiver can accept, never
> fabricate data to fill a gap — and makes the test doubles capable of
> modelling a bounded receiver so the class stays fixed.

## Grounding (2026-07-25, pre-Level-1)

Real code facts, verified this session before proposing any design. Audit
findings confirmed against source, with three corrections.

**Confirmed:**

- **B1.** `render_loop` sizes `buf` at `port.period_frames() * channels`
  (runtime.rs:1205) and pops that many frames every event.
  `WasapiRender::period_frames()` returns `self.buffer_frames` =
  `GetBufferSize()` (render.rs:131-133), ~960 frames @48 kHz under the 20 ms
  `BUFFER_DURATION_100NS` hint. The event fires once per *device* period
  (~480 frames), so `write` computes `to_write = frame_count.min(free)` and
  returns `Ok(())` having written only part (render.rs:107-124). The rest is
  already popped from the ring and is gone. Silent, unaccounted.
- **B2.** `max_block_frames = frames_for(wake_unit_period) + BLOCK_FRAME_MARGIN`
  (runtime.rs:1514-1520), and `wake_unit_period = period_frames/rate *
  WAKE_MARGIN` = 20 ms x 1.25 = **25 ms of audio produced per tick**, against
  a ~10 ms drain per render event. `flush_outputs` pushes with
  `let _ = producer.push(sample)` (runtime.rs:1449) — no counter, no log.
  Three independent wake sources (render event, every capture poll, every
  `apply_params`) multiply the tick rate on top of that.
- **B3.** `let mut buf = vec![0.0f32; channels * 256]` (runtime.rs:818) against
  `poll_interval` = `buffer_frames/rate/2` = ~10 ms = 480 frames
  (process_capture.rs:273). Chronic ~53% under-read.
- **B4.** `pull_group_inputs` does `scratch.fill(0.0)`, pops into
  `scratch[0..filled]`, then `mixer.push_group(slot.group_id, scratch)` — the
  whole slice (runtime.rs:1413-1433). `filled` is per-pid and never bounds the
  push.
- **B5.** `spin_sleep::SpinSleeper::default()` at runtime.rs:819. Out of scope
  here (fast-follow) but confirmed.
- **B17.** `SinkRender::wait_event` returns `Ok(())` immediately (mock.rs:303);
  `write` does an unconditional `extend_from_slice` (mock.rs:315);
  `period_frames()` is a hardcoded `480` (mock.rs:324); `SineCapture::read`
  returns `Ok(buf.len())` always (mock.rs:259). Every rate/flow bug above is
  structurally unobservable by the current suite.

**Corrections to the audit:**

1. **B3's second half is already implemented.** The audit asks for `read` to
   "drain until `GetNextPacketSize() == 0`" — `ProcessCapture::read` already
   loops on exactly that (process_capture.rs:287-292) and stashes a partial
   packet in `pending`/`pending_len` for the next call. The only real defect is
   the *caller's* `buf` size. Smaller fix than written.
2. **B1 partly feeds B2.** `compute_wake_unit_period` (runtime.rs:1500) reads
   the same `period_frames()` B1 blames, so correcting it to report the device
   period drops per-tick production 25 ms -> 12.5 ms as a side effect. The two
   are one dependency, not two independent fixes — but flow control is still
   needed, since 12.5 ms > 10 ms and there are still three wake sources.
3. **B8 is caused by B2/B4, so this blueprint *arms* B7.** `RingGauge.active`
   requires a single pid to fill an entire `max_block_frames` block in one tick
   (runtime.rs:1426-1432); under B2/B3 that never happens, so
   `DriftController::tick` skips every output (clock.rs:75-77) and the loop is
   dead. Fixing flow control makes it fire — against an inverted correction
   sign (`ResampleRatio::new(1.0 + corr)`, clock.rs:90), where `SincFixedIn`
   consumes a fixed `chunk_in` and produces `chunk_in * ratio` (resample.rs:78,
   106-120), so raising the ratio on a too-full ring fills it *further*.
   Positive feedback, pegged at `+max_correction`. This is why B7/B8 are in
   scope rather than deferred.

**Verification state at design time:** `cargo test --workspace` = 118 pass /
0 fail. Every bug above is invisible to that suite — which is finding B17.

## Design: Level 1 -- Capabilities

**Approved 2026-07-25.**

1. **A routed app's audio arrives at its output device intact** — continuous,
   not intermittent pops. Every frame the capture side reads reaches the render
   side or is accounted for; nothing is popped from a buffer and then
   discarded.
2. **Each output device carries its own groups' audio independently** — a
   second output with fewer matched apps sounds like a quieter mix, not like
   silence-padded noise. No output's behaviour depends on how many pids another
   output happens to have.
3. **When the pipeline can't keep up, that fact is visible** — an underrun or a
   dropped block increments a counter the engine already exposes, instead of
   vanishing into `let _ = producer.push(..)`.
4. **Long playback stays in sync** — the drift correction loop actually runs
   during real playback and pushes the ring toward its target fill rather than
   away from it.
5. **This class of bug fails in `cargo test`** *(developer-facing,
   deliberately)* — the test doubles model a device with a real clock and
   finite capacity, so a rate or flow-control regression is a red test, not a
   production-only symptom. B17 is the highest-leverage item in the audit and
   has no user-facing phrasing; naming it as a capability is what keeps it from
   being cut as "just test scaffolding."

Out of scope (this design): B5 (`spin_sleep` pacing on capture/supervisor
threads), B6 (256-tap sinc resampler cost), B9-B16, B18. All fast-follows.

## Design: Level 2 -- Components

**Approved 2026-07-25.**

The four bugs are one invariant broken at four stage boundaries:

```
  WASAPI process-loopback tap
          |  (1)  B3 -- reads ~256 frames per 480-frame period
          v
     group ring  (rtrb)
          |  (2)  B4 -- pushes a whole block, zero-padding the unfilled tail
          v
   Mixer  (audio_core -- UNCHANGED)
          |  (3)  B2 -- produces a full block per wake, whatever woke it
          v
     output ring (rtrb)
          |  (4)  B1 -- pops period_frames, device accepts ~half, rest discarded
          v
  WASAPI shared-mode render

  observes ring fill: DriftController (B7 sign / B8 never runs)
  stands in for both ends in tests: mock ports (B17)
```

**The invariant: never move more frames than the receiver can accept; never
fabricate frames to fill a gap.**

| # | Component | Layer | Change | Owns |
|---|---|---|---|---|
| 1 | `RenderPort` contract | `engine::ports` — interface | modified | What a render device can tell its caller: its real period, and how much space it has right now. |
| 2 | `WasapiRender` | `win-audio` — platform adapter | modified | Answering that contract truthfully from `GetDevicePeriod` / `GetCurrentPadding` instead of `GetBufferSize`. |
| 3 | `render_loop` drain | `engine::runtime` — orchestration | modified | Boundary (4). Pops at most what the device will accept; a shortfall is counted, never silent. |
| 4 | Mixer tick governor | `engine::runtime` — orchestration | modified | Boundaries (2) and (3). How many frames this tick may produce (from real ring headroom, not from the fact a wake happened), and pushing only the frames actually popped. Also re-derives block sizing and `RingGauge.active`. |
| 5 | `pid_capture_loop` fill | `engine::runtime` — orchestration | modified | Boundary (1). A read buffer sized from `poll_interval * rate * channels`, not a magic 256. |
| 6 | `DriftController` sign | `engine::clock` — pure control | modified | The direction a fill error is corrected in — pinned to `Src`'s actual behaviour by a test, not by a comment. |
| 7 | Paced test doubles | `engine::ports::mock` — test support | modified | A render sink with finite capacity and a real pace, and a capture source that cannot yield more than it has produced. |

Seven, not the methodology's 3-5. Deliberate: this is a repair across an
existing pipeline, so the components already exist and what changes is their
*contract*. Collapsing them would hide which boundary each fix belongs to —
precisely the confusion that let four bugs coexist.

**Integration points (existing infrastructure, not new components):**

- `EngineStats` gains drop/shortfall counters (capability 3). **Engine-only** —
  no `control`/`app`/UI surface in this blueprint. Surfacing them in the mixer
  UI is B16's problem.
- `MixerWaker` / demand-driven wakeup is untouched. This changes *how much* a
  tick produces, never *when* it wakes.
- `audio_core` is untouched.

**DDD:** no domain change. `DriftController` is the only pure-logic piece and
it is a control loop, not an aggregate/entity/value object.

## Design: Level 3 -- Interactions

**Approved 2026-07-25.**

**Finding that shapes this level.** The governor was assumed to need a single
per-tick frame count bounded by the *fullest* output's ring — which would let
one stalled output starve every other. `mixer.rs:542-696` says otherwise:
`push_group` sets that group's `valid_len`, `mix_tick` sums each group into
*its own* output's accumulator, and `out.filled` is a `max` across the groups
feeding it. **Production per output is determined entirely by how many frames
were pushed into the groups feeding it.** So the budget is decided *per group*,
from that group's own output's headroom. Groups on different outputs never
constrain each other; a stalled output cannot starve a healthy one. No
cross-output minimum is needed anywhere.

**Flow A — render drain (boundary 4).**

1. `render_loop` blocks in `wait_event`.
2. Device consumes a period; the event fires.
3. `render_loop` asks the port **how many frames are free right now**.
4. Pops at most that many frames from the output ring. Short pop -> pad the
   tail with silence, `xruns++` (unchanged).
5. Wakes the mixer — **before** the write, unchanged from
   mixer-demand-driven-wakeup Flow C.
6. Writes; the port returns **frames accepted**. Accepted < offered ->
   `render_shortfall += diff`. Structurally this can no longer happen; it is
   counted because "can't happen" is exactly what B1 was.

**Flow B — capture fill (boundary 1).**

1. At thread start, `pid_capture_loop` derives its read-buffer size from
   `poll_interval * sample_rate * channels`, with margin — replacing the magic
   `256`.
2. Each poll, `port.read(&mut buf)` returns `n`. The WASAPI side already drains
   until `GetNextPacketSize() == 0` or the buffer fills, stashing a partial
   packet for the next call — no change there (grounding correction 1).
3. Pushes `n` samples into the group ring. Ring full -> `capture_drops +=
   remainder`, counted rather than swallowed.
4. Wakes the mixer (unchanged).

**Flow C — mixer tick governor (boundaries 2 and 3).** Per tick, before pulling
anything:

1. Sample each output's ring **once**: current fill and capacity, in that
   output's frames — so every group this tick decides against the same
   snapshot.
2. Per group (each `GroupSlot` already carries its `output_index`), decide
   whether it may push this tick, from its own output's headroom converted to
   group-rate frames by that group's rate ratio.
3. Pull at most one block from that group's pids into scratch, tracking
   `filled_max` across pids (pids are summed; lengths are maxed — the existing
   per-pid summation is unchanged).
4. `push_group(group, &scratch[..filled_max * channels])` — **only the frames
   actually popped**. The zero-padded tail is never handed over. This is the
   whole of B4.
5. `mix_tick()` -> `update_telemetry()` (unchanged) -> `flush_outputs`: push
   `take_output`'s `n` samples; anything that doesn't fit -> `output_drops +=
   rest`, counted.

**Flow D — activity signal (B8).** `RingGauge.active` stops meaning "some pid
filled an entire `max_block_frames` block in one tick" — which can no longer be
true once the governor deliberately skips ticks — and starts meaning "this
output received real audio this tick", derived from `filled_max > 0` on any
group feeding it. It then rides the existing `update_activity`
(`ACTIVE_HOLD_TICKS`) hysteresis unchanged.

**Flow E — drift correction (B7).** Transport unchanged: RT threads publish
`fill_permille`/`active`, the supervisor reads them, `DriftController::tick`
emits `SetOutputRatio`. Only the sign changes. Verified against `Src`:
`mix_tick` calls `src.process(matrix_input, &mut g.resampled)` with a *fixed*
input length and `SincFixedIn` produces `consumed * ratio`, so a higher ratio
yields **more** output frames from the same input and fills the ring further. A
ring above target must therefore drive the ratio **below** 1.

**Flow F — paced doubles (B17).** A test constructs `SinkRender::paced(..)`
with a period and a capacity. `wait_event` blocks until the test calls
`drain(frames)`, which both frees space and releases the wait — the test *is*
the device clock. `write` accepts at most the free space and reports what it
took. `SineCapture` yields at most what the test has told it to produce. Under
this mock, B1/B2/B3/B4 each become a failing assertion.

**Flow G — startup priming.** The first tick runs before any render event
(mixer-demand-driven-wakeup Flow A, unchanged). Rings are empty, so headroom is
maximal and the governor allows a full budget — rings prime before the first
device event. Stated explicitly because a naive "only produce what was drained"
governor would produce *nothing* at startup and never start.

## Design: Level 4 -- Contracts

**Approved 2026-07-25.**

```rust
// -- 1. engine::ports -- the RenderPort contract --------------------------

pub trait RenderPort: Send {
    fn wait_event(&mut self, timeout: Duration) -> Result<(), PortError>;

    /// Frames this device will accept *right now* -- its buffer capacity minus
    /// current padding. No default body (decision 3): a default that claims
    /// space is available is B17 in trait form.
    fn free_frames(&self) -> Result<usize, PortError>;

    /// Returns frames actually accepted. A caller offering more than the last
    /// `free_frames()` reported is a caller bug; the shortfall is *reported*,
    /// never swallowed into `Ok(())` (B1).
    fn write(&mut self, frames: &[f32]) -> Result<usize, PortError>;

    fn format(&self) -> Format;

    /// The device *period* -- the audio one `wait_event` wakeup corresponds to.
    /// NOT the total buffer size. Re-specified, not added (B1).
    fn period_frames(&self) -> usize;
}

// -- 2. win-audio::render -- WasapiRender ---------------------------------

pub struct WasapiRender {
    client: IAudioClient,
    render_client: IAudioRenderClient,
    event: HANDLE,
    format: Format,
    /// `GetBufferSize()` -- retained, now only as `free_frames`' basis.
    buffer_frames: u32,
    /// `GetDevicePeriod()`'s default period, in frames at `format.sample_rate`.
    period_frames: u32,
}

/// Pure, so the 100ns->frames conversion is testable without a device.
fn device_period_frames(period_100ns: i64, sample_rate: u32) -> u32;

// -- 3. engine::runtime -- render drain (Flow A) --------------------------

/// `buf` holds this many device periods, so a device that drained more than
/// one period (post-stall catch-up) refills in a single event.
const RENDER_BUF_PERIODS: usize = 4;

struct RenderFaultCtx<'a> {
    xruns: &'a AtomicU64,
    shortfall: &'a AtomicU64,   // new
    output_id: OutputId,
    faults: &'a Sender<Fault>,
}
// render_loop's own signature is unchanged.

// -- 4. engine::runtime -- the governor (Flow C) --------------------------

/// One output ring's state, sampled once per tick before any group is pulled,
/// so every group this tick decides against the same snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OutputHeadroom {
    filled_frames: usize,
    capacity_frames: usize,
}

/// Fill at or above which the governor stops producing for an output.
const GOVERNOR_THRESHOLD_FILL: f32 = 0.5;

/// Output frames one full input block becomes after that group's SRC.
/// Computed once per group at mixer-thread start.
fn block_output_frames(block_frames: usize, group_rate: u32, output_rate: u32) -> usize;

/// Policy (beta): may this group push a full block this tick? Pure -- testable
/// with synthetic headroom, no `Mixer` and no threads.
fn group_may_push(headroom: OutputHeadroom, block_out_frames: usize) -> bool;

/// (beta)'s sawtooth midpoint -- what `DriftController` must aim at (decision 7).
fn drift_target_fill(threshold_fill: f32, block_out_frames: usize, capacity_frames: usize) -> f32;

fn sample_output_headroom(
    output_producers: &[(OutputId, rtrb::Producer<f32>, usize)],
    out: &mut [OutputHeadroom],
);

fn pull_group_inputs(
    group_consumers: &mut [GroupSlot],
    group_scratch: &mut [Vec<f32>],
    mixer: &mut Mixer,
    real_this_tick: &mut [bool],
    headroom: &[OutputHeadroom],
    /// Per group, index-parallel to `group_consumers`.
    block_out_frames: &[usize],
);

/// Extracted at the parameter threshold, not after (operational learning) --
/// `flush_outputs` would otherwise take 7.
struct FlushCtx<'a> {
    ring_fill: &'a [RingGauge],
    real_this_tick: &'a [bool],
    ticks_since_real: &'a mut [u32],
    drops: &'a AtomicU64,
}

fn flush_outputs(
    output_producers: &mut [(OutputId, rtrb::Producer<f32>, usize)],
    output_scratch: &mut [Vec<f32>],
    mixer: &mut Mixer,
    ctx: FlushCtx<'_>,
);

// -- EngineStats -- capability 3, engine-only, no UI surface --------------

pub struct EngineStats {
    // ...existing fields unchanged...
    /// Frames the mixer produced that an output ring could not accept. Should
    /// stay 0 under the governor; non-zero means the budget and the ring's
    /// real capacity disagree.
    pub output_drops: u64,
    /// Frames the capture side read that a group ring could not accept.
    pub capture_drops: u64,
    /// Frames offered to a render device that it did not accept. Structurally
    /// impossible post-B1 -- counted because "impossible" is what B1 was.
    pub render_shortfall: u64,
}
// RunningGraph / MixerThreadArgs each gain the matching `Arc<AtomicU64>`,
// same shape as the existing `xruns`.

// -- 5. engine::runtime -- capture fill (Flow B) --------------------------

/// Read buffer holds this many poll intervals, so a late wakeup still drains
/// WASAPI in one call instead of leaving packets queued. B3's fixed 256 frames
/// was ~53% of a *single* interval.
const CAPTURE_BUF_INTERVALS: usize = 2;

/// Pure -- the sizing B3 got wrong, isolated so a test can pin it.
fn capture_buf_samples(poll_interval: Duration, sample_rate: u32, channels: usize) -> usize;

fn pid_capture_loop(
    port: Box<dyn CapturePort>,
    producer: rtrb::Producer<f32>,
    stop: &AtomicBool,
    sys: &dyn AudioSystem,
    waker: MixerWaker,
    drops: &AtomicU64,          // new
);

// -- 6. engine::clock -- DriftController (Flow E) -------------------------

// No signature changes. Two edits:
//   clock.rs:90   ResampleRatio::new(1.0 + corr)  ->  (1.0 - corr)
//   build_running_graph sets DriftConfig::target_fill from drift_target_fill(..)
//   instead of taking Default's 0.5.

// -- 7. engine::ports::mock -- paced doubles (Flow F) ---------------------

impl SinkRender {
    /// UNCHANGED. Infinite capacity, `wait_event` returns immediately -- for
    /// tests asserting on recorded content (decision 5).
    pub fn new(format: Format) -> SinkRender;

    /// A finite device: `capacity_frames` of buffer, `period_frames` per event.
    /// Returns the port and the test-side clock handle.
    pub fn paced(format: Format, period_frames: usize, capacity_frames: usize)
        -> (SinkRender, SinkDevice);
}

/// Test-side handle to a paced `SinkRender` -- the device clock. `Arc`-backed
/// and `Clone`, same idiom as `MockSessionPort`, because the port itself is
/// moved into the spawned `render_loop` thread.
#[derive(Clone)]
pub struct SinkDevice(Arc<SinkState>);

impl SinkDevice {
    /// Consumes up to `frames` from the simulated device buffer and releases
    /// exactly one `wait_event`. Nothing advances without this call.
    pub fn drain(&self, frames: usize) -> usize;
    pub fn filled_frames(&self) -> usize;
    /// Everything `write` has accepted so far -- `recorded()`'s paced
    /// equivalent, reachable after the port has moved into a thread.
    pub fn recorded(&self) -> Vec<f32>;
}

impl SineCapture {
    /// UNCHANGED. Fills any buffer it is given.
    pub fn new(freq_hz: f32, format: Format) -> SineCapture;
    pub fn paced(freq_hz: f32, format: Format) -> (SineCapture, CaptureSource);
}

#[derive(Clone)]
pub struct CaptureSource(Arc<CaptureState>);

impl CaptureSource {
    /// Makes `frames` more frames available to `read`. A paced `read` returns
    /// at most what has been produced, then 0 -- never `buf.len()`
    /// unconditionally.
    pub fn produce(&self, frames: usize);
}
```

Keeping `new()` unchanged on both mocks (rather than renaming to `unpaced()`)
means zero churn across the existing 118 tests — `paced()` is purely additive.

**Decision 7's number, computed rather than estimated** (48 kHz stereo, 20 ms
buffer hint, 10 ms device period):

| Quantity | Value |
|---|---|
| `period_frames()` (post-B1) | 480 |
| Ring capacity (`RING_PERIOD_MARGIN` = 4) | 1920 frames |
| `compute_wake_unit_period` (x `WAKE_MARGIN` 1.25) | 12.5 ms |
| `max_block_frames` (+ `BLOCK_FRAME_MARGIN` 8) | 608 frames |
| Skip threshold @ 0.5 | 960 frames |
| Sawtooth range | 960 -> 1568 frames |
| **`DriftConfig::target_fill`** | **1264 / 1920 ~= 0.658**, not 0.5 |

Output-ring latency lands at **20-32.7 ms**, with a floor of two full device
periods of cushion before underrun. A lower threshold (0.25) would give
10-22.7 ms but leaves only one period of cushion — taking the safer of the two,
flagged because it is the one user-perceptible consequence of this design.

### Test contracts

| Test | Flow / bug |
|---|---|
| `render_loop_pops_only_what_the_device_will_accept` | A / B1 |
| `render_loop_counts_a_short_write_instead_of_discarding_it` | A / cap 3 |
| `device_period_frames_converts_reference_time_at_the_device_rate` | A / B1 |
| `wasapi_render_period_frames_is_the_device_period_not_the_buffer_size` | `#[ignore]`, real hardware — audit MT2 |
| `capture_buf_samples_covers_a_whole_poll_interval` | B / B3 |
| `pid_capture_loop_counts_ring_full_drops` | B / cap 3 |
| `pull_group_inputs_pushes_only_the_frames_it_popped` | C / B4 |
| `a_partially_filled_group_never_zero_pads_the_stream` | C / B4 |
| `group_may_push_skips_when_its_output_ring_is_at_threshold` | C / B2 |
| `group_may_push_ignores_another_outputs_full_ring` | C / no cross-output starvation |
| `a_stalled_output_does_not_starve_a_healthy_one` | C, integration under paced mocks |
| `extra_wakes_do_not_over_produce` | C / B2 — the core regression |
| `ring_gauge_active_is_true_when_a_group_received_real_audio` | D / B8 |
| `src_produces_fewer_frames_at_a_lower_ratio` | E / B7 — pins the sign to `Src`'s **measured** behaviour |
| `drift_controller_lowers_the_ratio_when_the_ring_is_above_target` | E / B7 |
| `drift_target_fill_lands_on_the_governors_sawtooth_midpoint` | E / decision 7 |
| `paced_sink_rejects_more_than_its_free_space` | F / B17 |
| `paced_sink_wait_event_blocks_until_drained` | F / B17 |
| `sine_capture_yields_no_more_than_produced` | F / B17 |
| `rings_prime_before_the_first_render_event` | G |

`src_produces_fewer_frames_at_a_lower_ratio` runs real samples through a real
`Src` and counts output frames. The operational learnings record three separate
cases where a design doc's stated *mechanism* was wrong even though the fix was
right — B7's sign is exactly that shape, so it gets measured, not asserted from
a reading of `resample.rs`.

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-25 | **Scope is B17 + B1/B2/B3/B4 + B7/B8.** B5, B6 and B9-B18 excluded. | B1-B4 are one coherent flow-control problem — fixing any one alone does not make audio correct (audit's own ordering). B17 first, because without paced mocks the other four are unverifiable and will regress. B7/B8 included because this blueprint's fix is what makes `RingGauge.active` fire, converting B7's inverted sign from dead code into a live positive-feedback loop; shipping without it would be knowingly arming a bug. Rejected: B5 (one-line, real CPU win, but a *pacing* concern not a *flow* concern — bundling it would blur what this blueprint's tests are proving); B6 (a genuine quality/cost tradeoff needing its own design, not a repair). |
| 2 | 2026-07-25 | **`RenderPort` gains free-space reporting AND `write` returns frames accepted** (option (a)), rather than free-space reporting alone. | `free_frames()` makes loss impossible by construction; `write`'s return value is what makes any residual shortfall countable instead of a silent `Ok(())` — capability 3's other half. Rejected: (b) free-space only, `write -> ()`. Simpler, but leaves "never silent" as caller discipline, so a future caller that pops a guess reintroduces B1 with no signal. |
| 3 | 2026-07-25 | **The new `RenderPort` method gets NO default body**, against this codebase's own `set_bus_match` / `open_default_endpoint_volume` precedent. | A default that claims space is available *is* B17 — a mock silently modelling an infinite device. The precedent's rationale (most implementors have nothing meaningful to do with the capability) is inverted here: every implementor must answer honestly or the invariant is unenforceable. Deliberate, logged exception to the operational learning. |
| 4 | 2026-07-25 | **Mocks are paced demand-driven — the test is the clock** (option (i)): `SinkRender` models a fixed-capacity buffer whose space is freed only by a test-called `drain(frames)` hook, which is also what releases `wait_event`. | Fully deterministic, zero wall-time dependence, no sleeps. Rejected: (ii) wall-clock pacing (closest to real hardware, but makes every flow assertion timing-sensitive — the exact flakiness shape the operational learnings flag twice); (iii) a shared virtual clock across all mocks (deterministic, but every existing test spawning real engine threads would have to learn to advance it). |
| 5 | 2026-07-25 | **`SinkRender` keeps two constructors — `unpaced()` (today's infinite sink) and `paced(..)`** — rather than migrating all existing tests to the paced model. | ~All existing `SinkRender` uses assert on recorded *content*, not flow; migrating 118 tests to drive drains would be pure cost. Accepted tradeoff, logged: the *default* stays permissive, so catching a future flow bug still requires someone to reach for `paced()`. |
| 6 | 2026-07-25 | **Governor policy is (β) full-block-or-skip**: a group pushes exactly one full block when its output's ring is below the skip threshold, and pushes nothing otherwise. | `SincFixedIn`'s `chunk_in` *is* `max_block_frames`, so a partial push is buffered and produces no output until a full chunk accumulates — the resampler, not the governor, decides when output appears. (β) keeps every push chunk-aligned, so the SRC emits every tick it runs, and it is what makes the Flow F assertions clean. Rejected: (α) deficit-to-target — better-looking control design (holds fill at target, zero average error against the drift controller) but reintroduces burstiness one layer down as the SRC withholds partial chunks and then emits in lumps. |
| 7 | 2026-07-25 | **`DriftConfig::target_fill` is derived from the governor's threshold plus half a block, not left at the default `0.5`.** | (β) leaves the ring sawtoothing between the skip threshold and one block above it, so its *average* fill sits about half a block above the threshold. A controller aiming at the threshold itself would read a permanent positive error and hold a permanent negative ratio bias against the governor. Deriving it is one pure function; the alternative is two constants that silently disagree. |
| 8 | 2026-07-25 | **`max_block_frames` now serves a third purpose** — SRC `chunk_in`, scratch sizing, and (β)'s production quantum — and this is logged as a constraint rather than decoupled. | The operational learnings flag a constant serving double duty as a silent-breakage risk; making it explicit and pinned by a test is cheaper here than introducing a fourth constant that must be kept in ratio with it. Any future change to `max_block_frames`'s basis must re-derive decision 7's target fill. |
| 9 | 2026-07-25 | **Design approved at Level 4. Status set to `approved` — ready for implementation.** | All four level sections persisted. One open question remains (MT1) and is deliberately non-blocking: it determines whether these fixes are *sufficient*, not whether they are *correct*. |

## Open Questions

- **MT3 — is the ring-floor gap real, and does priming close it?** Blocking for
  landing the `render_loop` priming change (see Post-Implementation Review), not
  for the rest of this blueprint. Measured with the counter the blueprint
  already added: `EngineStats.xruns` increments *only* on the short-pop path
  (`runtime.rs`), so it reads the gap directly. Procedure: route an app, watch
  `xruns`. Climbs for a few seconds at stream start then flatlines → gap
  confirmed, priming is the fix. Climbs indefinitely → worse than analysed,
  reopen the governor design. Flat from the start → the analysis is wrong,
  revert the priming change. Run in the same hardware session as MT1, whose
  result gates interpreting this one.
- **MT1 — does `ISimpleAudioVolume::SetMute` also silence the
  `PROCESS_LOOPBACK` tap?** Unverified. session-mute-on-capture mutes a routed
  app's session so its audio doesn't double up through the default device
  (`routing.rs:274-278` -> `sessions.rs:180-201`); if the process-loopback tap
  reads *after* that mute is applied, the feature silences the very stream it
  is routing. Does not change any capability or contract in this design — the
  flow-control fixes are correct either way — but it determines whether fixing
  them is *sufficient*. Should be run on real hardware before implementation
  starts, so a null result is not misread as "the flow-control fix didn't
  work." Procedure: audit MT1.

## Constraints

- **No change to `audio_core`'s contract.** `Mixer::push_group` already handles
  a short slice correctly (`frame_count = frames.len()/channels`,
  mixer.rs:549), `take_output` and `Src::process` are unchanged. The repair is
  entirely in the orchestration layer, the port contracts, and the platform
  adapter.
- **RT-safety invariants hold.** No allocation, no locking, no blocking added
  to the mixer or render thread bodies.
- **`RenderPort` stays platform-independent.** No raw Win32 handles or
  WASAPI-specific types cross the trait boundary (drift-and-recovery precedent).

## Design Summary

**Drift check: skipped — `requirement_doc` is null.** Origin is
`.lattice/reviews/2026-07-25-end-to-end-audit.md` plus a live user bug report,
not a requirement spec, so there are no Scenarios/ACs or
`## Technical Constraints` to compare Level 4 against. Nothing was written back
to any requirement doc.

**Components and layer assignments**

| Component | Layer | Change |
|---|---|---|
| `RenderPort` | `engine::ports` (interface) | `period_frames()` re-specified as the device period; `free_frames()` added with **no** default body; `write` returns frames accepted |
| `WasapiRender` | `win-audio` (platform adapter) | `GetDevicePeriod` for the period, `GetCurrentPadding` for free space, honest `write` return |
| `render_loop` + `RenderFaultCtx` | `engine::runtime` (orchestration) | pops at most `free_frames()`; counts any shortfall |
| Mixer tick governor (`sample_output_headroom`, `group_may_push`, `block_output_frames`, `pull_group_inputs`, `flush_outputs`/`FlushCtx`) | `engine::runtime` (orchestration) | per-group, per-output production budget; pushes only frames actually popped; counts ring-full drops |
| `pid_capture_loop` + `capture_buf_samples` | `engine::runtime` (orchestration) | read buffer sized from the real poll interval; counts ring-full drops |
| `DriftController` + `drift_target_fill` | `engine::clock` (pure control) | correction sign inverted; target fill derived from the governor's sawtooth |
| `SinkRender`/`SinkDevice`, `SineCapture`/`CaptureSource` | `engine::ports::mock` (test support) | additive paced constructors modelling a finite, test-clocked device |

**Key contracts** — `RenderPort::free_frames()` is the mechanism: once the
render side can state its capacity, "never move more than the receiver can
accept" becomes enforceable at boundary 4, and `group_may_push` applies the
same rule at boundary 3 from the ring's own fill. `push_group(&scratch[..filled])`
applies "never fabricate frames" at boundary 2, and `capture_buf_samples`
removes the under-read at boundary 1. Three `AtomicU64` counters on
`EngineStats` make every remaining loss visible.

**Architectural constraints honoured**

- `audio_core` untouched — `push_group`/`mix_tick`/`take_output`/`Src` unchanged.
- `RenderPort` stays platform-independent; no windows-rs type crosses it.
- Interface-at-consumer preserved: the trait changes in `engine::ports`,
  `win-audio` implements downstream.
- RT-safety unchanged: no allocation, lock, or block added to the mixer or
  render thread bodies. The governor's per-tick work is plain arithmetic over
  `producer.slots()`.
- `MixerWaker` / demand-driven wakeup untouched — this changes *how much* a
  tick produces, never *when* it wakes.

**Domain model** — no change. `DriftController` is a pure control loop, not an
aggregate, entity, or value object.

**Open questions resolved during design** — the governor's unit and whether a
stalled output could starve a healthy one (Level 3 finding: `out.filled` is a
`max` across the groups feeding one output, so budgets are per-group and
outputs never constrain each other); the governor policy and its interaction
with `SincFixedIn`'s fixed `chunk_in` (decision 6); the resulting fill-target
mismatch against `DriftController` (decision 7, computed not estimated); the
default-body question on the new trait method (decision 3, a deliberate
exception to this codebase's own precedent).

**Open question deliberately left open** — MT1. Non-blocking for
implementation, blocking for interpreting the result.

**Known accepted gaps**

- B5 (`spin_sleep` busy-tail on capture/supervisor threads) and B6 (256-tap
  sinc cost) remain — the CPU complaint is *not* fixed by this blueprint.
- The mocks' permissive constructors stay the default (decision 5), so a future
  flow bug still needs someone to reach for `paced()`.
- Output-ring latency rises to a bounded 20-32.7 ms (Level 4 table).

## Key Files

| Path | Purpose |
|------|---------|
| `crates/engine/src/ports/mod.rs` | `RenderPort` contract (component 1) |
| `crates/win-audio/src/render.rs` | `WasapiRender` (component 2) |
| `crates/engine/src/runtime.rs` | render drain, governor, capture fill, `EngineStats` counters (components 3-5) |
| `crates/engine/src/clock.rs` | `DriftController` sign + target fill (component 6) |
| `crates/engine/src/ports/mock.rs` | paced test doubles (component 7) |

## Implementation Notes (2026-07-25, post-Level-4)

Filled in during forging; none change a Level 1-4 decision, each resolves a
gap the contract text left implicit.

1. **`drift_target_fill` is computed in `build_running_graph` but consumed in
   `supervisor_loop`, which needed restructuring to receive it.** The Level 4
   contract said only "`build_running_graph` sets `DriftConfig::target_fill`
   from `drift_target_fill(..)`" — but `DriftConfig` construction lives in
   `supervisor_loop`, a different function with its own lifetime (one
   supervisor thread for the whole `EngineHandle`, surviving every rebuild).
   Wiring: `RunningGraph` gained a `drift_target_fill: f32` field, computed
   once per build; `SupervisorSnapshot` carries it out under the existing
   lock; `supervisor_loop`'s per-topology-change branch (`snap.output_ids !=
   known_outputs`) now builds a fresh `DriftConfig { target_fill: ..,
   ..Default::default() }` there instead of holding one `cfg` for the
   process's whole lifetime. `kp`/`ki`/`max_correction`/`tick` stay the
   shared default throughout — only `target_fill` varies per rebuild.
2. **`drift_target_fill`'s inputs are a representative approximation, not an
   exact per-group value.** `DriftController`/`DriftConfig` are one scalar for
   the *whole* graph, not per-output, so `build_running_graph` reads the
   first output's own ring capacity and assumes `block_out_frames ==
   max_block_frames` (i.e., that output's feeding group shares its rate) —
   correct for the common single-rate case, a reasonable approximation
   otherwise. Logged rather than silently accepted because a future
   multi-rate-per-output topology could make this number visibly off.
3. **`group_may_push` adds a second condition beyond the threshold check**:
   `filled_frames + block_out_frames <= capacity_frames`, not just
   `filled_frames < threshold_frames`. The contract's prose named only the
   threshold; the second condition is a hard safety net against the
   configured constants ever disagreeing with a topology's actual capacity
   (e.g. a future change to `RING_PERIOD_MARGIN`/`WAKE_MARGIN` without a
   matching audit) — belt-and-suspenders for "never move more than the
   receiver can accept," costs one comparison, never triggers under the
   default constants (verified: 960 + 608 = 1568 < 1920).
4. **Mechanical, not a design decision**: implementing Layer 1's `RenderPort`
   trait change required updating three test-local stub implementations in
   `runtime.rs` (`FailingRender`, `FixedPeriodRender`, `OneShotRender`) to add
   `free_frames`/change `write`'s return type, purely to keep the crate
   compiling before Layer 3 touched `render_loop` itself. No behavior change.

## Post-Implementation Review (2026-07-25)

Branch reviewed against this blueprint after forging. Every Level 4 contract is
present and the numbers reconcile exactly with decision 7 (`period_frames` 480,
ring capacity 1920, `max_block_frames` 608, `drift_target_fill` 0.6583). Three
findings, one of which required a code change.

### Correction to Level 3 Flow G — the governor has no floor

**Flow G as written is wrong.** It claims rings prime at startup because "rings
are empty, so headroom is maximal and the governor allows a full budget." The
governor gates on *permission*; production is limited by *available captured
audio*. `group_may_push` can only ever withhold — it cannot create frames — so
it bounds a ring from above and never from below.

Capture supplies ~one device period per device period, and `render_loop` drains
the same, so ring level is `∫production − ∫drain`: it stays wherever it started,
which is empty. `render_loop` then makes zero a *stable* operating point rather
than a stall, because a short pop is silence-padded (never blocks). With the SRC
emitting in 608-frame lumps against 480-frame pops, that is a silence gap
roughly every third render event.

The only thing raising the operating point is `DriftController`: fill ≈ 0 pegs
`corr` at `−max_correction`, giving a 0.5 % surplus ≈ 0.24 frames/ms, so
reaching a safe floor takes **seconds**. Decision 7's "floor of two full device
periods of cushion before underrun" therefore describes a level nothing
establishes.

This matters beyond latency: it reproduces the *original user symptom*
(intermittent pops) while every counter this blueprint added reads clean —
`output_drops`, `capture_drops` and `render_shortfall` are all 0, because no
frame is lost. Only `xruns` moves.

**Fix (implemented): a priming phase in `render_loop`.** Below the target the
loop feeds the device silence and leaves the ring untouched, so the cushion can
build. Two decisions inside it:

- **Target is `GOVERNOR_THRESHOLD_FILL × capacity`, not `drift_target_fill`.**
  The governor stops production *at* the threshold, so anything above it is
  reachable only via a block overshoot — targeting it would race the mixer.
  At the threshold it is exactly decision 7's two-period cushion.
- **Re-armed only on a completely empty ring.** An empty ring is already
  emitting silence, so waiting costs nothing; a partially filled one is
  mid-stream and must never be interrupted to rebuild a cushion. This also
  covers engine start, where no capture source exists yet, so the loop primes
  on the first real audio rather than on engine start.

Priming does **not** increment `xruns` — it is a deliberate cushion build, and
counting it would destroy the one signal that reports whether the floor holds.

**Status: pending real-hardware confirmation (MT3).** The mechanism is analysed,
not measured. Land it only once MT3 confirms the gap is real; the change is
self-contained in `render_loop` and can be reverted wholesale if not.

### Test gap closed — B4 had no test

Four of the twenty test contracts were unimplemented, including **both** that
cover B4 — the most consequential of the four flow fixes. The adapted
`pull_group_inputs_on_a_group_with_zero_pids_produces_silence` asserted on
`scratch` contents, which pass identically against pre-fix code; its comment
claiming B4 coverage has been corrected.

Now implemented:

- `pull_group_inputs_pushes_only_the_frames_it_popped`
- `a_partially_filled_group_never_zero_pads_the_stream`
- `rings_prime_before_the_first_render_event` (Flow G, against the fix above)

**How the B4 tests observe a slice length that isn't public.** `valid_len` is
private, so both go through the one thing that depends on it: `Src` consumes a
fixed `chunk_in == max_block_frames` and buffers anything short of it. Feed half
a block and a correct implementation emits nothing that tick; the zero-padding
version hands over a full chunk and emits immediately. The second test states
the same invariant as conservation — no more frames out than in, which
zero-padding roughly doubles.

Both run at `B4_BLOCK = 512`, not a token 8: `Src` wraps a 256-tap sinc
(`SINC_LEN`), so at a block of 8 the resampler never fills its delay line and
both tests would pass vacuously. This is worth remembering for any future
mixer-level test that asserts on SRC output.

**All three were verified to fail against the pre-fix behaviour** (mutating
`push_group(&scratch[..filled])` back to `push_group(scratch)`, and priming off)
before being accepted. Per capability 5, a flow test that cannot fail is worth
nothing.

`group_may_push_ignores_another_outputs_full_ring` is **resolved by
construction, not by a test**: `group_may_push` takes a single `OutputHeadroom`,
so it cannot observe another output's ring at all. A unit test would assert a
signature. The observable form is the integration test
`a_stalled_output_does_not_starve_a_healthy_one`, which exists.

### Logged, not fixed (fast-follows)

- **Governor skips decay the meters.** A skipped tick pushes an empty slice, so
  `mix_tick` takes `meter.observe_silence(max_block_frames)` instead of
  `observe` (`mixer.rs:627,677`). Before this blueprint `valid_len` was never 0
  for a live group. Real severity is low — `PeakMeter` attack is instant, so an
  allowed tick restores the bar — expect mild jitter, not collapse.
  `EnvFollower` (duck detection) is unaffected: empty input leaves `env`
  untouched rather than decaying it.
- **Per-sample drops can misalign frames in a group ring.** `pid_capture_loop`
  drops sample-by-sample and keeps trying subsequent samples; if the consumer
  frees space mid-loop, pushes resume mid-frame and that group's ring is
  permanently channel-swapped. Pre-existing (identical shape to the old
  `let _ = producer.push(..)`), and this blueprint now *counts* those drops
  without making them frame-aligned. Low likelihood — group rings hold ~8 poll
  intervals — non-recoverable if hit.
- **`out.accum` is only `max_block_frames × channels`** (`mixer.rs:429`), so
  `write_len = produced.min(accum.len())` silently truncates an upsampling
  group (44.1 k → 48 k gives `block_out_frames` 661 against 608 of capacity).
  Correctly outside this blueprint's "no change to `audio_core`" constraint,
  but `block_output_frames` now computes a number the accumulator cannot hold,
  so it should be explicit rather than implicit.
