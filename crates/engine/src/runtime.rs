//! Graph orchestration: opens ports, spawns capture/mixer/render threads,
//! owns the lock-free command queue and topology epoch.
//!
//! Threading model per `.lattice/context/engine-core.md` L3:
//! - capture ×N (polled) → SPSC ring → mixer ×1 (timer-paced) → SPSC ring → render ×M (event-driven)
//! - param changes flow through a bounded MPSC command queue, tagged with the current `Epoch`
//! - structural changes ([`EngineHandle::rebuild`]) stop and respawn the whole thread set

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_queue::ArrayQueue;
use rtrb::RingBuffer;

use audio_core::{
    ChannelLayout, DomainError, DspChain, DspSpec, Format, GroupId, HrirSet, MeterLevel, Mixer,
    MixerCommand, OutputId, Render, Retired, Topology,
};

use crate::clock::{DriftConfig, DriftController, FillSample};
use crate::graph::{self, ConfigSnapshot, GraphPlan};
use crate::ports::{
    AudioSystem, CapturePort, DeviceEvent, Endpoint, EndpointId, PortError, RenderPort,
};

/// Output is considered active if any group pushed real (non-synthesized)
/// audio to it within the last N mixer ticks — debounces brief per-tick
/// starvation jitter without freezing the drift loop over it (notes §6).
const ACTIVE_HOLD_TICKS: u32 = 10;

/// Distinguishes a recoverable device fault (removal/format-change — the
/// recovery supervisor rebuilds or re-routes) from any other port failure
/// (thread exits, no automatic recovery attempted). Not part of the public
/// contract (drift-and-recovery L4): internal handoff from RT threads to
/// the supervisor.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FaultKind {
    DeviceInvalidated,
    Other,
}

impl From<&PortError> for FaultKind {
    fn from(e: &PortError) -> FaultKind {
        match e {
            PortError::DeviceInvalidated => FaultKind::DeviceInvalidated,
            PortError::NotFound(_) | PortError::Backend(_) => FaultKind::Other,
        }
    }
}

/// Capture-side faults no longer feed this channel (process-loopback-capture
/// pivot): a per-pid capture failure is isolated by construction — other
/// pids in the same group, and every other group, are unaffected — so there
/// is no "the group faulted" event for the recovery supervisor to react to
/// (L3 flow E: per-attempt, never a sticky/global degradation). Only
/// physical output devices (drift-and-recovery's actual scope) still report
/// through here.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FaultSource {
    Output(OutputId),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Fault {
    pub source: FaultSource,
    pub kind: FaultKind,
}

#[derive(Debug)]
pub enum EngineError {
    Resolve(String),
    Port(PortError),
    AlreadyStopped,
    /// Not in the original L4 contract text — added because `Mixer::new` can
    /// fail (e.g. a bus/output channel-count mismatch) and `start`/`rebuild`
    /// need somewhere to put that.
    Domain(DomainError),
    /// Not in the original L4 contract text — added because the command
    /// queue is bounded (notes §7) and `apply_params` needs somewhere to
    /// put a full-queue failure.
    CommandQueueFull,
}

impl From<PortError> for EngineError {
    fn from(e: PortError) -> Self {
        EngineError::Port(e)
    }
}

impl From<DomainError> for EngineError {
    fn from(e: DomainError) -> Self {
        EngineError::Domain(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Epoch(pub u64);

/// Not `Clone`/`Copy` — P5's `SwapChain` variant carries a `Box<DspChain>`,
/// moved through the queue as a pointer, never copied (notes §7).
struct Envelope {
    epoch: Epoch,
    cmd: MixerCommand,
}

/// Notices from the recovery supervisor (drift-and-recovery L4). Delivered
/// through the single-consume handoff on [`EngineHandle::take_events`].
#[derive(Debug, Clone)]
pub enum EngineEvent {
    FallbackApplied {
        groups: Vec<GroupId>,
        from: EndpointId,
        to: EndpointId,
    },
    Recovered {
        groups: Vec<GroupId>,
        on: EndpointId,
    },
    DeviceAvailable(Endpoint),
    /// An endpoint disappeared from the system — the counterpart to
    /// `DeviceAvailable`, and like it, sent for *every* removal regardless of
    /// whether any group was using the device. `DeviceLost` below is the
    /// narrower, audio-path signal ("these groups have nowhere to render");
    /// this one just says the device list changed.
    ///
    /// Added for double-audio-prevention flow F: the sink endpoint is
    /// deliberately never a group output, so its removal produces no
    /// `DeviceLost`/`FallbackApplied` at all and was previously invisible to
    /// the app layer.
    DeviceRemoved(EndpointId),
    DeviceLost {
        groups: Vec<GroupId>,
    },
    /// Session-routing (P3) degradation notice — sent once per degradation
    /// episode by `routing::RoutingCoordinator`; audio path is unaffected.
    RoutingDegraded {
        reason: String,
    },
    /// The OS default playback device changed (external-controls.md flow E).
    /// Forwarded from `DeviceEvent::DefaultChanged` rather than discarded —
    /// this is the one existing `subscribe_device_events` subscription
    /// (`WasapiSystem` allows only one live registration at a time), so a
    /// consumer needing default-device changes (the volume-bind coordinator)
    /// reacts to this event instead of subscribing a second time, which
    /// would silently replace the recovery supervisor's own subscription.
    DefaultDeviceChanged(EndpointId),
}

#[derive(Debug, Clone)]
pub struct EngineStats {
    pub xruns: u64,
    pub ring_fill: Vec<(OutputId, f32)>,
    /// Last resample ratio applied per output — the drift loop's actuator. Per
    /// output, not per group: groups sharing an output must run the same ratio
    /// or they fall out of frame alignment (see `MixerCommand::SetOutputRatio`).
    pub applied_ratio: Vec<(OutputId, f64)>,
    pub group_faults: Vec<GroupId>,
    /// Always-on per-output headroom limiter engagement count (P5) — a
    /// running total, same style as `xruns`, not reset on read.
    pub limiter_engaged: Vec<(OutputId, u64)>,
    /// Current duck gain reduction per group, in dB (0 = not reduced / not a
    /// duck target).
    pub duck_depth_db: Vec<(GroupId, f32)>,
    /// Current post-fader level-meter reading per group (level-meters.md).
    pub group_peak: Vec<(GroupId, MeterLevel)>,
    /// Current post-limiter level-meter reading per output device.
    pub output_peak: Vec<(OutputId, MeterLevel)>,
    /// Friendly device name per `OutputId` (level-meters.md) — static for the
    /// life of the graph, lets the UI label each `output_peak` entry by its
    /// real device without reproducing the engine's `OutputId` assignment.
    pub output_names: Vec<(OutputId, String)>,
    /// Each group's input sample rate (graphical-eq.md). The EQ curve needs
    /// it to draw the response the filter actually applies -- at 128 kHz a
    /// treble bell is ~3 dB wider than the same band drawn at 48 kHz. Static
    /// for the life of the graph.
    pub group_rates: Vec<(GroupId, u32)>,
    /// Frames the mixer produced that an output ring could not accept
    /// (audio-flow-control cap 3). Should stay 0 under the governor —
    /// non-zero means the budget and the ring's real capacity disagree.
    pub output_drops: u64,
    /// Frames the capture side read that a group ring could not accept.
    pub capture_drops: u64,
    /// Per group, the fullest of its pids' capture rings (0.0–1.0), sampled at
    /// each ring's high-water point. The counterpart to `ring_fill` on the
    /// input side: `capture_drops` alone says samples were lost but not why,
    /// and a ring pinned near 1.0 means a standing surplus the mixer's drain
    /// rate structurally cannot absorb, not a transient scheduling burst.
    /// Empty for a group with no pids currently captured.
    pub capture_fill: Vec<(GroupId, f32)>,
    /// Frames offered to a render device that it did not accept. Structurally
    /// impossible post-B1 — counted because "impossible" is what B1 was.
    pub render_shortfall: u64,
    /// The last push an output ring rejected, in that output's own frames:
    /// `(span, free, capacity, budget)`. `None` until one is rejected.
    ///
    /// Diagnostic for the two-group popping (session-2026-07-27-static.md).
    /// `output_drops` says a push was rejected but not which of the three
    /// possible disagreements caused it, and offline oracles ruled out the
    /// other two: `span > budget` means the mixer produced more than
    /// `group_may_push` budgeted for, while `span <= free` means the ring
    /// freed up between the headroom snapshot and the flush.
    pub last_output_reject: Option<(u64, u64, u64, u64)>,
}

/// The last rejected push, in output frames — see
/// [`EngineStats::last_output_reject`]. Written from the mixer thread on the
/// reject path only (which should never run), so nothing is allocated or
/// logged on the RT path; the audit line reads it out.
#[derive(Debug, Default)]
struct RejectDiag {
    seen: AtomicBool,
    span_frames: AtomicU64,
    free_frames: AtomicU64,
    capacity_frames: AtomicU64,
    budget_frames: AtomicU64,
}

/// Packs a [`MeterLevel`] into one `AtomicU64` cell: the f32 peak in the low
/// 32 bits, the clip flag in bit 32. One atomic per meter keeps the peak and
/// its clip flag consistent with each other across the cross-thread read (no
/// AtomicF32 in std, and two separate atomics could tear the pair). `0` decodes
/// to [`MeterLevel::SILENT`], so a freshly-zeroed cell is already correct.
fn encode_meter(m: MeterLevel) -> u64 {
    ((m.clipped as u64) << 32) | m.peak.to_bits() as u64
}

fn decode_meter(bits: u64) -> MeterLevel {
    MeterLevel { peak: f32::from_bits(bits as u32), clipped: (bits >> 32) & 1 != 0 }
}

/// Clone-able read-only handle over the running graph's telemetry, for a
/// thread that isn't the `EngineHandle` owner (the settings UI's per-frame
/// meter poll — level-meters.md). Mirrors the `RoutingReader` idiom: it holds
/// the same `Arc<Mutex<Option<RunningGraph>>>` cell, so it tracks rebuilds and
/// reports empty stats while the engine is stopped, with no re-handoff.
#[derive(Clone)]
pub struct StatsReader {
    running: Arc<Mutex<Option<RunningGraph>>>,
}

impl StatsReader {
    pub fn stats(&self) -> EngineStats {
        read_stats(&self.running)
    }
}

/// Snapshots every cross-thread telemetry gauge under one short lock. Shared by
/// `EngineHandle::stats` and `StatsReader::stats`. Empty vectors when the
/// engine is stopped (no running graph), which the UI renders as meters at the
/// floor.
fn read_stats(running: &Mutex<Option<RunningGraph>>) -> EngineStats {
    let running = running.lock().unwrap();
    match running.as_ref() {
        None => EngineStats {
            xruns: 0,
            ring_fill: Vec::new(),
            applied_ratio: Vec::new(),
            group_faults: Vec::new(),
            limiter_engaged: Vec::new(),
            duck_depth_db: Vec::new(),
            group_peak: Vec::new(),
            output_peak: Vec::new(),
            output_names: Vec::new(),
            group_rates: Vec::new(),
            output_drops: 0,
            capture_drops: 0,
            capture_fill: Vec::new(),
            render_shortfall: 0,
            last_output_reject: None,
        },
        Some(rg) => EngineStats {
            xruns: rg.xruns.load(Ordering::Relaxed),
            ring_fill: rg
                .output_ids
                .iter()
                .zip(rg.ring_fill.iter())
                .map(|(id, gauge)| (*id, gauge.fill_permille.load(Ordering::Relaxed) as f32 / 1000.0))
                .collect(),
            applied_ratio: rg
                .output_ids
                .iter()
                .zip(rg.applied_ratio.iter())
                .map(|(id, bits)| (*id, f64::from_bits(bits.load(Ordering::Relaxed))))
                .collect(),
            // Always empty (process-loopback-capture pivot): a per-pid capture
            // failure is isolated to that one pid, never "the whole group" —
            // no group-level fault concept remains. Field kept for API
            // stability (app-shell reads only its `.len()`).
            group_faults: Vec::new(),
            limiter_engaged: rg
                .output_ids
                .iter()
                .zip(rg.limiter_engaged.iter())
                .map(|(id, count)| (*id, count.load(Ordering::Relaxed)))
                .collect(),
            duck_depth_db: rg
                .group_ids
                .iter()
                .zip(rg.duck_depth_db.iter())
                .map(|(id, bits)| (*id, f32::from_bits(bits.load(Ordering::Relaxed))))
                .collect(),
            group_peak: rg
                .group_ids
                .iter()
                .zip(rg.group_peak.iter())
                .map(|(id, cell)| (*id, decode_meter(cell.load(Ordering::Relaxed))))
                .collect(),
            output_peak: rg
                .output_ids
                .iter()
                .zip(rg.output_peak.iter())
                .map(|(id, cell)| (*id, decode_meter(cell.load(Ordering::Relaxed))))
                .collect(),
            output_names: rg.output_devices.clone(),
            group_rates: rg.group_formats.iter().map(|(id, fmt)| (*id, fmt.sample_rate)).collect(),
            output_drops: rg.output_drops.load(Ordering::Relaxed),
            capture_drops: rg.capture_drops.load(Ordering::Relaxed),
            // `group_ids` order (not the map's) so the vec lines up with
            // `group_peak` and every other per-group gauge. Same reading the
            // drift loop regulates against.
            capture_fill: rg
                .group_ids
                .iter()
                .map(|id| (*id, capture_gauges(rg, *id).0))
                .collect(),
            render_shortfall: rg.render_shortfall.load(Ordering::Relaxed),
            last_output_reject: rg.reject_diag.seen.load(Ordering::Relaxed).then(|| {
                (
                    rg.reject_diag.span_frames.load(Ordering::Relaxed),
                    rg.reject_diag.free_frames.load(Ordering::Relaxed),
                    rg.reject_diag.capacity_frames.load(Ordering::Relaxed),
                    rg.reject_diag.budget_frames.load(Ordering::Relaxed),
                )
            }),
        },
    }
}

const COMMAND_QUEUE_CAPACITY: usize = 256;
/// notes §6: ring capacity = 4x the largest period involved.
const RING_PERIOD_MARGIN: usize = 4;
/// Headroom added on top of the tick-period frame count when sizing `Mixer`'s
/// per-group scratch buffers, to absorb scheduling jitter between ticks.
const BLOCK_FRAME_MARGIN: usize = 8;
/// Retired `DspChain`s awaiting an off-RT drop (notes §7) — sized generously
/// against simultaneous swaps between supervisor drain ticks; the mixer
/// thread's push is best-effort (drop-on-full, same tolerance as every other
/// ring/queue here), so this only needs to absorb a burst, not hold forever.
const RETIRED_CHAIN_QUEUE_CAPACITY: usize = 32;
/// Upper bound on how long `mixer_loop` can stay parked before a tick runs
/// anyway (mixer-demand-driven-wakeup L1 capability 5/Flow B/H) — keeps
/// `EngineStats`/`ring_fill` telemetry from going stale when nothing real
/// (render/capture/command) wakes it.
const MIXER_FALLBACK_INTERVAL: Duration = Duration::from_millis(100);
/// Headroom over one exact render period applied by `compute_wake_unit_period`,
/// replacing the old implicit half-period doubling (notes §5) now that the
/// mixer wakes from a real park/unpark event rather than a polled timer
/// (decision 2).
const WAKE_MARGIN: f64 = 1.25;
/// `render_loop`'s buffer holds this many device periods (audio-flow-control
/// B1), so a device that drained more than one period in one event (post-
/// stall catch-up) still refills in a single `wait_event` cycle. The actual
/// amount popped each event is bounded by `free_frames()`, never this constant
/// directly — this only sizes the preallocated scratch buffer.
const RENDER_BUF_PERIODS: usize = 4;
/// `pid_capture_loop`'s read buffer holds this many poll intervals (B3), so a
/// late wakeup still drains the source in one `read` call instead of leaving
/// packets queued. The old fixed 256-frame buffer was ~53% of a single
/// interval at a typical 10ms poll/48kHz.
const CAPTURE_BUF_INTERVALS: usize = 2;
/// One-pole coefficient for the published capture-ring fill, applied once per
/// poll (~10 ms). 0.02 is a ~500 ms time constant — several times the drift
/// controller's own 100 ms tick, so the controller sees the ring's standing
/// level rather than which part of a poll packet it happened to land in.
const CAPTURE_FILL_SMOOTHING: f32 = 0.02;
/// Seeded at the controller's own target, not 0: a ring starting from a
/// reported-empty state would take the whole time constant to climb, and the
/// controller would spend that ramp correcting an error that was never there.
const CAPTURE_FILL_SMOOTHING_SEED: f32 = 0.5;
/// Fill fraction at or above which the mixer tick governor stops producing
/// for an output (audio-flow-control B2/B4, decision 6: full-block-or-skip).
/// The ring then sawtooths between this floor and one block above it. This
/// *is* the output ring's regulation — which is why the drift loop no longer
/// also aims at it (clock.rs's module doc): two controllers on one actuator
/// left the ratio free to wander.
const GOVERNOR_THRESHOLD_FILL: f32 = 0.5;

/// Wakes the mixer thread out of its park (mixer-demand-driven-wakeup L2/L4).
/// Clone, no lock: `std::thread::Thread` is already `Send + Sync + Clone`, so
/// every render/capture/command-enqueue site can hold its own clone and call
/// `wake()` without a `Condvar`/`Mutex` on this RT-adjacent path.
#[derive(Clone)]
struct MixerWaker(thread::Thread);

impl MixerWaker {
    fn wake(&self) {
        self.0.unpark();
    }
}

struct RingGauge {
    fill_permille: AtomicU32,
    /// Set by the mixer tick (notes §6); read cross-thread by the recovery
    /// supervisor. Since the drift loop moved to the capture ring this is no
    /// longer what it regulates against — it is the second half of the idle
    /// guard: a group is corrected only if its pids are delivering *and* its
    /// output is actually running, so a parked or faulted output does not have
    /// corrections integrated against it while nothing can drain its ring.
    active: AtomicBool,
}

/// One group's mixer-thread-local input state: zero or more per-pid capture
/// rings summed together each tick (process-loopback-capture pivot —
/// replaces the old one-consumer-per-group shape; a group with zero pids
/// behaves exactly like the old "starved" case, silence). `channels`/
/// `output_index` are precomputed once at build time so the mixer thread
/// never has to look them up per tick.
struct GroupSlot {
    group_id: GroupId,
    pids: Vec<(u32, rtrb::Consumer<f32>)>,
    channels: usize,
    output_index: usize,
}

type GroupConsumers = Vec<GroupSlot>;
type OutputProducers = Vec<(OutputId, rtrb::Producer<f32>, usize)>;

/// A live per-pid capture thread, tracked so `CaptureControl` can stop and
/// join exactly one pid without disturbing any other pid or group (L3 flow
/// B/C — diffed, isolated add/remove).
struct PidCapture {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    /// This pid's capture ring fill in permille, published by its own capture
    /// thread right after each push batch — i.e. the ring's high-water point
    /// in the cycle, before the mixer drains it again. The output rings have
    /// had a gauge since drift-and-recovery; the capture rings had none, which
    /// is why `capture_drops` could climb for a whole session with no way to
    /// tell a brim-pinned ring (a standing rate surplus) from an occasional
    /// scheduling burst. Read out per group as `EngineStats::capture_fill`,
    /// and fed to the drift controller as the buffer it regulates.
    fill_permille: Arc<AtomicU32>,
    /// Monotonic count of samples this pid has pushed. The drift loop's idle
    /// guard: the supervisor compares it against the previous tick's value, and
    /// a group whose pids delivered nothing in that window is skipped. Without
    /// it, a paused app's ring drains to empty and the integrator reads a
    /// permanent -0.5 error, pegging the ratio at a rail that the group then
    /// has to unwind from when it resumes.
    pushed_samples: Arc<AtomicU64>,
}

/// Wire format from `CaptureControl::apply_capture_sources` (any thread —
/// typically `engine::routing`'s coordinator thread) into the mixer thread's
/// own local `GroupSlot` list. Carries the `rtrb::Consumer` itself (not
/// `Copy`/`Clone`) — same "move a pre-built off-RT value through a channel"
/// idiom as `MixerCommand::SwapChain`'s boxed `DspChain`, just via a
/// dedicated channel instead of the `MixerCommand` queue, since this mutates
/// `mixer_loop`'s own local capture bookkeeping, not anything `Mixer` itself
/// (`audio_core`) knows about.
enum CaptureMsg {
    Add {
        group: GroupId,
        pid: u32,
        consumer: rtrb::Consumer<f32>,
    },
    Remove {
        group: GroupId,
        pid: u32,
    },
}

struct Persistent {
    commands: ArrayQueue<Envelope>,
    epoch: AtomicU64,
    /// Cloned by the recovery supervisor thread to emit `EngineEvent`s;
    /// survives rebuilds since `Persistent` does.
    events_tx: Sender<EngineEvent>,
    /// Last config the *app* explicitly set via `EngineHandle::rebuild` (or
    /// `start`). The source of truth the supervisor rebuilds from — its own
    /// fallback/park decisions never touch this, only `overrides` below.
    canonical_snapshot: Mutex<ConfigSnapshot>,
    /// Per-group deviation from `canonical_snapshot`, keyed by `GroupId`
    /// (stable across supervisor-triggered rebuilds since those never
    /// change `canonical_snapshot`'s group list/order — see `graph::resolve`'s
    /// positional `GroupId` doc). `Some(endpoint)` = temporarily fallen back
    /// to `endpoint`; `None` = parked (no fallback device available).
    /// Cleared whenever the app sets a new canonical snapshot.
    overrides: Mutex<HashMap<GroupId, Option<Endpoint>>>,
    /// Retired chains/renders from `SwapChain`/`SwapRender` applies, drained
    /// and dropped by the supervisor thread — dealloc never happens on the
    /// mixer's RT thread (notes §7).
    retired: ArrayQueue<Retired>,
}

struct RunningGraph {
    stop: Arc<AtomicBool>,
    /// Live per-pid capture threads, keyed by group then pid —
    /// `CaptureControl::apply_capture_sources` adds/removes entries directly
    /// (through the same `Arc<Mutex<Option<RunningGraph>>>` every other
    /// structural accessor locks), independent of `apply_rebuild`'s full
    /// stop/respawn (a rebuild only touches render/output-side ports; capture
    /// sources are re-established by the routing coordinator's next
    /// reconcile against the fresh topology).
    capture_pids: HashMap<GroupId, HashMap<u32, PidCapture>>,
    /// Sender half the mixer thread currently reading from — `CaptureControl`
    /// reads this out of the locked `RunningGraph` to hand the mixer thread a
    /// newly-opened pid's consumer (or tell it to drop one), since the
    /// mixer's own `GroupSlot` list is local to its thread, not shared state.
    capture_tx: Sender<CaptureMsg>,
    mixer_thread: Option<JoinHandle<()>>,
    render_threads: Vec<JoinHandle<()>>,
    xruns: Arc<AtomicU64>,
    /// Audio-flow-control cap 3 counters — shared with the threads that
    /// actually drop frames, read cross-thread by `EngineHandle::stats`.
    output_drops: Arc<AtomicU64>,
    capture_drops: Arc<AtomicU64>,
    render_shortfall: Arc<AtomicU64>,
    /// Why the last rejected push was rejected — see
    /// [`EngineStats::last_output_reject`].
    reject_diag: Arc<RejectDiag>,
    /// Last resample ratio applied per output, in `output_ids` order, as
    /// `f64::to_bits` (no `AtomicF64` in std) — written by the mixer thread as
    /// it applies each `SetOutputRatio`, surfaced via `EngineStats`.
    applied_ratio: Arc<Vec<AtomicU64>>,
    ring_fill: Arc<Vec<RingGauge>>,
    output_ids: Vec<OutputId>,
    group_ids: Vec<GroupId>,
    /// Which output each group currently routes through — the supervisor
    /// uses this to map a faulted `OutputId` back to affected groups.
    group_outputs: Vec<(GroupId, OutputId)>,
    /// Endpoint currently bound to each output — lets the supervisor
    /// identify which physical device died and dedup by endpoint id.
    output_endpoints: Vec<(OutputId, EndpointId)>,
    /// Drained by the recovery supervisor thread.
    fault_rx: Receiver<Fault>,
    /// Each group's source `Format` and the graph's `max_block_frames` — the
    /// exact construction parameters `Mixer`'s per-group `DspChain`s/`Render`s
    /// were built with, so `EngineHandle::apply_dsp_chains`/`apply_spatial`
    /// can build a replacement off-RT with matching buffer sizes (notes §7).
    group_formats: Vec<(GroupId, Format)>,
    /// Each output's `Format` — the `to` side `EngineHandle::apply_spatial`
    /// needs to decide (and build) a group's replacement `Render`.
    output_formats: Vec<(OutputId, Format)>,
    max_block_frames: usize,
    /// Telemetry gauges, written by the mixer thread each tick, read
    /// cross-thread by `EngineHandle::stats` — same pattern as `ring_fill`.
    duck_depth_db: Arc<Vec<AtomicU32>>,
    limiter_engaged: Arc<Vec<AtomicU64>>,
    /// Level-meter gauges (level-meters.md), one packed `AtomicU64` per group /
    /// per output — written by the mixer thread each tick, read cross-thread by
    /// `stats`. Same publish pattern as `duck_depth_db`; index order matches
    /// `group_ids` / `output_ids`.
    group_peak: Arc<Vec<AtomicU64>>,
    output_peak: Arc<Vec<AtomicU64>>,
    /// Friendly device name per `OutputId` (level-meters.md) — fixed at build,
    /// surfaced verbatim via `EngineStats::output_names`.
    output_devices: Vec<(OutputId, String)>,
    /// Clone of the mixer thread's `Thread` handle (mixer-demand-driven-wakeup)
    /// — every render/capture thread and `EngineHandle::apply_params` holds
    /// its own clone to wake the mixer out of its park on demand.
    mixer_waker: MixerWaker,
}

pub struct EngineHandle {
    sys: Arc<dyn AudioSystem>,
    persistent: Arc<Persistent>,
    /// `Arc` (not a plain `Mutex`, unlike engine-core): the recovery
    /// supervisor thread needs shared access alongside `EngineHandle` itself
    /// to trigger its own rebuilds concurrently with app-triggered ones.
    running: Arc<Mutex<Option<RunningGraph>>>,
    events_rx: Option<Receiver<EngineEvent>>,
    supervisor_stop: Arc<AtomicBool>,
    supervisor_thread: Option<JoinHandle<()>>,
}

pub fn start(
    snapshot: &ConfigSnapshot,
    sys: Arc<dyn AudioSystem>,
) -> Result<EngineHandle, EngineError> {
    let (events_tx, events_rx) = mpsc::channel();
    let persistent = Arc::new(Persistent {
        commands: ArrayQueue::new(COMMAND_QUEUE_CAPACITY),
        epoch: AtomicU64::new(0),
        events_tx,
        canonical_snapshot: Mutex::new(snapshot.clone()),
        overrides: Mutex::new(HashMap::new()),
        retired: ArrayQueue::new(RETIRED_CHAIN_QUEUE_CAPACITY),
    });
    let running_graph = build_running_graph(snapshot, &sys, &persistent, &HashSet::new())?;
    queue_initial_dsp_bypass(&persistent, snapshot);
    let running = Arc::new(Mutex::new(Some(running_graph)));

    let supervisor_stop = Arc::new(AtomicBool::new(false));
    let supervisor_thread = {
        let running = Arc::clone(&running);
        let persistent = Arc::clone(&persistent);
        let sys = Arc::clone(&sys);
        let stop = Arc::clone(&supervisor_stop);
        thread::spawn(move || supervisor_loop(running, persistent, sys, stop))
    };

    Ok(EngineHandle {
        sys,
        persistent,
        running,
        events_rx: Some(events_rx),
        supervisor_stop,
        supervisor_thread: Some(supervisor_thread),
    })
}

impl EngineHandle {
    /// Takes ownership of `cmds` (not `&[MixerCommand]`): `SwapChain` carries
    /// a `Box<DspChain>`, moved into the queue's `Envelope`, never copied.
    pub fn apply_params(&self, cmds: Vec<MixerCommand>) -> Result<(), EngineError> {
        let running = self.running.lock().unwrap();
        let Some(rg) = running.as_ref() else {
            return Err(EngineError::AlreadyStopped);
        };
        let epoch = Epoch(self.persistent.epoch.load(Ordering::Relaxed));
        for cmd in cmds {
            self.persistent
                .commands
                .push(Envelope { epoch, cmd })
                .map_err(|_| EngineError::CommandQueueFull)?;
        }
        // A live edit must apply promptly while the mixer is parked, not wait
        // for the next incidental render/capture wake (mixer-demand-driven-
        // wakeup L3 Flow E — the one call site apply_dsp_chains/apply_spatial
        // both funnel through).
        rg.mixer_waker.wake();
        Ok(())
    }

    /// Structural change: stops and fully respawns the thread set against the
    /// new snapshot, bumping the epoch so stale in-flight commands are
    /// dropped by the new mixer thread. Sets `snapshot` as the new canonical
    /// config and clears any recovery-supervisor fallback/park state — a
    /// deliberate app-driven config change supersedes it (drift-and-recovery
    /// decision: reuse whole-graph rebuild for device-fault recovery too,
    /// rather than building per-output isolation `Mixer` doesn't support
    /// today). **Simplification from the L3 design** (logged in
    /// `.lattice/context/engine-core.md`, extended by drift-and-recovery):
    /// this rebuilds the *entire* graph, not just the affected group/output —
    /// correct, but a config change to one group, or a single device fault,
    /// briefly gaps audio on every group, not just the affected one. If the
    /// rebuild fails, the engine is left stopped (no rollback).
    pub fn rebuild(&self, snapshot: &ConfigSnapshot) -> Result<(), EngineError> {
        apply_rebuild(&self.sys, &self.persistent, &self.running, Some(snapshot.clone()))
    }

    pub fn stats(&self) -> EngineStats {
        read_stats(&self.running)
    }

    /// A `Clone`-able read-only stats handle (level-meters.md) for a second
    /// thread — the settings UI polls it every frame for live meters, the same
    /// idiom as `RoutingHandle::reader`. Shares the same `running` cell, so it
    /// reflects rebuilds without any re-handoff.
    pub fn stats_reader(&self) -> StatsReader {
        StatsReader { running: Arc::clone(&self.running) }
    }

    /// Add/remove-stage change (P5): builds each group's new `DspChain`
    /// off-RT (this call's thread, never the mixer thread — notes §7), then
    /// funnels the swaps through the same command path as `apply_params`.
    /// The epoch bump that invalidates stale in-flight commands for the old
    /// chain shape happens on the mixer thread as a side effect of applying
    /// the swap (`drain_commands`), not here.
    pub fn apply_dsp_chains(&self, chains: Vec<(GroupId, Vec<DspSpec>)>) -> Result<(), EngineError> {
        // Known race (accepted, same tolerance as `apply_rebuild`'s own
        // documented one): the lock is dropped after this read, so a
        // concurrent rebuild between here and `apply_params` below could
        // build a chain against a `max_block_frames`/format that's already
        // stale. Individually-correct racing calls, eventually consistent —
        // not worth a stricter lock-holding scheme for how rarely a DSP edit
        // and a structural rebuild would actually land in the same instant.
        let (max_block_frames, group_formats) = {
            let guard = self.running.lock().unwrap();
            let Some(rg) = guard.as_ref() else {
                return Err(EngineError::AlreadyStopped);
            };
            (rg.max_block_frames, rg.group_formats.clone())
        };

        let mut commands = Vec::with_capacity(chains.len());
        for (group, specs) in chains {
            let Some(&(_, fmt)) = group_formats.iter().find(|(g, _)| *g == group) else {
                continue; // unknown group id — dropped silently, same convention as everywhere else
            };
            let chain = DspChain::new(&specs, fmt, max_block_frames)?;
            commands.push(MixerCommand::SwapChain {
                group,
                chain: Box::new(chain),
            });
        }
        self.apply_params(commands)
    }

    /// Live spatial-audio toggle (spatial-audio.md): builds each group's new
    /// `Render` off-RT (this call's thread, never the mixer thread — notes
    /// §7) against the group's current input/output `Format`s, then funnels
    /// the swaps through the same command path as `apply_params`/
    /// `apply_dsp_chains`. `Render::build` owns the fallback rule (spatial
    /// requested but the output isn't stereo -> plain matrix).
    pub fn apply_spatial(&self, changes: &[(GroupId, bool)]) -> Result<(), EngineError> {
        // Same accepted eventually-consistent race as `apply_dsp_chains`: the
        // lock is dropped after this read, so a concurrent rebuild could
        // make `max_block_frames`/formats stale by the time the command
        // below applies. Individually-correct racing calls, not worth a
        // stricter lock-holding scheme for how rarely this would matter.
        let (max_block_frames, group_formats, group_outputs, output_formats) = {
            let guard = self.running.lock().unwrap();
            let Some(rg) = guard.as_ref() else {
                return Err(EngineError::AlreadyStopped);
            };
            (
                rg.max_block_frames,
                rg.group_formats.clone(),
                rg.group_outputs.clone(),
                rg.output_formats.clone(),
            )
        };

        let mut commands = Vec::with_capacity(changes.len());
        for &(group, spatial) in changes {
            let Some(&(_, from)) = group_formats.iter().find(|(g, _)| *g == group) else {
                continue; // unknown group id — dropped silently, same convention as apply_dsp_chains
            };
            let Some(&(_, output_id)) = group_outputs.iter().find(|(g, _)| *g == group) else {
                continue;
            };
            let Some(&(_, to)) = output_formats.iter().find(|(o, _)| *o == output_id) else {
                continue;
            };
            let render = Render::build(spatial, from, to, max_block_frames);
            commands.push(MixerCommand::SwapRender { group, render: Box::new(render) });
        }
        self.apply_params(commands)
    }

    pub fn epoch(&self) -> Epoch {
        Epoch(self.persistent.epoch.load(Ordering::Relaxed))
    }

    /// Cloneable handle for driving per-pid capture-source changes from any
    /// thread — `engine::routing`'s coordinator thread needs this
    /// independently from whatever thread owns this `EngineHandle`
    /// (process-loopback-capture L4). Same established idiom as
    /// `RoutingHandle::reader()`.
    pub fn capture_control(&self) -> CaptureControl {
        CaptureControl {
            running: Arc::clone(&self.running),
            sys: Arc::clone(&self.sys),
        }
    }

    /// Single-consume handoff (drift-and-recovery revision): the app-side
    /// event pump takes the receiver once and fans out to its own consumers
    /// (tray, UI). `Receiver` is single-consumer, so a second call can't
    /// return a live duplicate.
    pub fn take_events(&mut self) -> Receiver<EngineEvent> {
        self.events_rx
            .take()
            .expect("take_events called more than once on the same EngineHandle")
    }

    pub fn shutdown(self) -> Result<(), EngineError> {
        // Stop + join the supervisor first: it may be mid-rebuild (holding or
        // about to take `running`), and joining it before touching `running`
        // ourselves avoids racing its in-flight `apply_rebuild` call.
        self.supervisor_stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.supervisor_thread {
            let _ = t.join();
        }
        let mut running = self.running.lock().unwrap();
        match running.take() {
            Some(rg) => {
                stop_running_graph(rg);
                Ok(())
            }
            None => Err(EngineError::AlreadyStopped),
        }
    }
}

fn stop_running_graph(mut rg: RunningGraph) {
    rg.stop.store(true, Ordering::Relaxed);
    // Without this, the mixer thread only notices `stop` at its next real
    // render/capture/command wake, or after MIXER_FALLBACK_INTERVAL (100ms)
    // — a real join-latency regression on every shutdown/rebuild (review
    // finding, 2026-07-24) that none of the design's Flows A-I covered.
    rg.mixer_waker.wake();
    // Per-pid capture threads use their own stop flags (not `rg.stop`) so
    // `CaptureControl` can stop one independently mid-run — a full teardown
    // stops every one of them here.
    for (_, pids) in rg.capture_pids.drain() {
        for (_, pc) in pids {
            pc.stop.store(true, Ordering::Relaxed);
            let _ = pc.thread.join();
        }
    }
    if let Some(t) = rg.mixer_thread.take() {
        let _ = t.join();
    }
    for t in rg.render_threads.drain(..) {
        let _ = t.join();
    }
}

/// Drives per-pid capture-source changes into a live `RunningGraph`
/// (process-loopback-capture L4) — a cloneable handle so `engine::routing`'s
/// coordinator thread can call this concurrently with whatever thread owns
/// the `EngineHandle` itself, same pattern as `RoutingHandle`/`RoutingReader`.
#[derive(Clone)]
pub struct CaptureControl {
    running: Arc<Mutex<Option<RunningGraph>>>,
    sys: Arc<dyn AudioSystem>,
}

impl CaptureControl {
    /// Diffs `pids` against the currently-running set for `group`: newly-
    /// present pids get a capture thread opened and wired into the mixer's
    /// `GroupSlot`; no-longer-present pids have theirs stopped and removed;
    /// pids present in both sets are left completely untouched — no full
    /// teardown/rebuild of the group (process-loopback-capture L3 flow B/C,
    /// binding behavior, not an implementation detail). A pid that fails to
    /// open (permission denied, protected process, ...) is skipped and
    /// returned in the result — this call's *other* pids still apply (L3
    /// flow E: per-attempt, isolated, never a global degraded posture).
    /// Returns the pids that failed to open this call (empty = every pid
    /// applied); `engine::routing` uses this to surface a per-attempt
    /// `EngineEvent::RoutingDegraded` notice, not a sticky flag — deviates
    /// from the blueprint's literal `Result<(), EngineError>` signature
    /// (logged in the context doc) since the degradation signal has to come
    /// from somewhere and `routing.rs`, not `runtime.rs`, owns it.
    ///
    /// Three passes, only the middle one unlocked (review finding,
    /// 2026-07-21): `self.sys.open_process_capture` is a blocking WASAPI/COM
    /// activation call — doing it while holding `self.running`'s lock would
    /// stall every other engine control call (`stats`/`apply_params`/
    /// `rebuild`, all sharing the same lock) for however long activation
    /// takes, on every thread that calls them. Stop/remove is bounded
    /// (~poll_interval, same tolerance already accepted for the mixer/render
    /// thread joins in `stop_running_graph`) so it stays under the lock.
    /// Racing this against a concurrent rebuild/`apply_capture_sources` call
    /// is an accepted eventually-consistent race, same tolerance already
    /// documented for `apply_dsp_chains`/`apply_spatial`.
    pub fn apply_capture_sources(&self, group: GroupId, pids: Vec<u32>) -> Result<Vec<u32>, EngineError> {
        let desired: HashSet<u32> = pids.into_iter().collect();

        let to_open: Vec<u32> = {
            let mut guard = self.running.lock().unwrap();
            let Some(rg) = guard.as_mut() else {
                return Err(EngineError::AlreadyStopped);
            };

            // Reap any pid whose thread already exited on its own — a
            // runtime read failure, not an open failure (review finding,
            // 2026-07-21): without this, a dead thread's pid stays "current"
            // forever and is never retried, even though matched sessions are
            // meant to be retried independently every reconcile (L3 flow E).
            if let Some(group_map) = rg.capture_pids.get_mut(&group) {
                let dead: Vec<u32> = group_map
                    .iter()
                    .filter(|(_, pc)| pc.thread.is_finished())
                    .map(|(pid, _)| *pid)
                    .collect();
                for pid in dead {
                    if let Some(pc) = group_map.remove(&pid) {
                        let _ = pc.thread.join();
                    }
                    let _ = rg.capture_tx.send(CaptureMsg::Remove { group, pid });
                }
            }

            let current: HashSet<u32> = rg
                .capture_pids
                .get(&group)
                .map(|m| m.keys().copied().collect())
                .unwrap_or_default();

            for pid in current.difference(&desired).copied().collect::<Vec<_>>() {
                if let Some(pc) = rg.capture_pids.get_mut(&group).and_then(|m| m.remove(&pid)) {
                    pc.stop.store(true, Ordering::Relaxed);
                    let _ = pc.thread.join();
                }
                let _ = rg.capture_tx.send(CaptureMsg::Remove { group, pid });
            }

            desired.difference(&current).copied().collect()
        };

        // Unlocked: open every new pid. Best-effort per pid (L3 flow E) — a
        // failure here excludes just this pid, the others still apply.
        let mut failed = Vec::new();
        let mut opened = Vec::new();
        for pid in to_open {
            let Ok(port) = self.sys.open_process_capture(pid, true) else {
                failed.push(pid);
                continue;
            };
            let format = port.format();
            let device_period_s = port.poll_interval().as_secs_f64() * 2.0; // polled at ~period/2
            let capacity = ring_capacity_samples(device_period_s, format.sample_rate, format.channels);
            let (producer, consumer) = RingBuffer::<f32>::new(capacity);
            opened.push((pid, port, producer, consumer));
        }

        {
            let mut guard = self.running.lock().unwrap();
            let Some(rg) = guard.as_mut() else {
                return Err(EngineError::AlreadyStopped);
            };
            let waker = rg.mixer_waker.clone();
            let drops = Arc::clone(&rg.capture_drops);
            for (pid, port, producer, consumer) in opened {
                let stop = Arc::new(AtomicBool::new(false));
                let fill_permille = Arc::new(AtomicU32::new(0));
                let pushed_samples = Arc::new(AtomicU64::new(0));
                let thread = {
                    let stop = Arc::clone(&stop);
                    let sys = Arc::clone(&self.sys);
                    let waker = waker.clone();
                    let drops = Arc::clone(&drops);
                    let fill = Arc::clone(&fill_permille);
                    let pushed = Arc::clone(&pushed_samples);
                    thread::spawn(move || {
                        pid_capture_loop(
                            port,
                            producer,
                            &stop,
                            sys.as_ref(),
                            waker,
                            CaptureGauges { drops: &drops, fill: &fill, pushed: &pushed },
                        )
                    })
                };
                rg.capture_pids.entry(group).or_default().insert(
                    pid,
                    PidCapture { stop, thread, fill_permille, pushed_samples },
                );
                let _ = rg.capture_tx.send(CaptureMsg::Add { group, pid, consumer });
            }
        }

        Ok(failed)
    }
}

/// One pid's capture loop — same shape as the old per-bus `capture_loop`,
/// minus fault reporting: a per-pid read failure just ends this one thread
/// (process-loopback-capture L3 flow E treats it as this pid quietly
/// dropping out, not a "fault" the recovery supervisor needs to react to).
/// The sizing B3 got wrong, isolated so a test can pin it: `poll_interval`
/// worth of audio at `sample_rate`, times `CAPTURE_BUF_INTERVALS` margin, in
/// interleaved samples. Pure.
fn capture_buf_samples(poll_interval: Duration, sample_rate: u32, channels: usize) -> usize {
    let frames = frames_for(poll_interval * CAPTURE_BUF_INTERVALS as u32, sample_rate);
    frames * channels.max(1)
}

/// One pid capture thread's telemetry cells, grouped at the same parameter
/// threshold as `FlushCtx`/`RenderFaultCtx` (operational learnings) —
/// `pid_capture_loop` would otherwise take 8.
struct CaptureGauges<'a> {
    /// Samples the ring could not accept (audio-flow-control B2/B3).
    drops: &'a AtomicU64,
    /// Ring fill in permille at its high-water point each cycle.
    fill: &'a AtomicU32,
    /// Samples the port delivered — the drift loop's idle guard.
    pushed: &'a AtomicU64,
}

fn pid_capture_loop(
    mut port: Box<dyn CapturePort>,
    mut producer: rtrb::Producer<f32>,
    stop: &AtomicBool,
    sys: &dyn AudioSystem,
    waker: MixerWaker,
    gauges: CaptureGauges<'_>,
) {
    let _rt = sys.promote_rt_thread();
    let poll_interval = port.poll_interval();
    let channels = port.format().channels.max(1) as usize;
    let sample_rate = port.format().sample_rate;
    // Covers a whole poll interval with margin (B3) — the old fixed 256
    // frames was ~53% of a single ~10ms/48kHz interval, chronically
    // under-reading what the source actually produced.
    let mut buf = vec![0.0f32; capture_buf_samples(poll_interval, sample_rate, channels)];

    let capacity = producer.buffer().capacity();
    // Smoothed fill, not the instantaneous reading. A capture ring's level is
    // inherently bursty — a whole poll packet lands, then the mixer drains it —
    // so a single sample swings by most of a packet either way. Measured live
    // at 0.27–0.77 tick to tick around a target of 0.5: with kp=0.05 that is a
    // correction 2.5x the clamp every tick, so the controller sat on alternate
    // rails, driving nothing but noise into the resample ratio. The controller
    // needs the ring's *level*, not its phase within a packet.
    let mut smoothed = CAPTURE_FILL_SMOOTHING_SEED;

    while !stop.load(Ordering::Relaxed) {
        match port.read(&mut buf) {
            Ok(n) => {
                let dropped = push_whole_frames(&mut producer, &buf[..n], channels);
                if dropped > 0 {
                    gauges.drops.fetch_add(dropped, Ordering::Relaxed); // never silent (B2/B3)
                }
                // Counts what the port DELIVERED, not what the ring accepted.
                // A saturated ring rejects everything, and that is precisely
                // the state the drift loop exists to correct — subtracting
                // `dropped` here would mark the group idle exactly when its
                // ring is at the brim, so the controller would stop pulling the
                // ratio down and the surplus would never clear.
                gauges.pushed.fetch_add(n as u64, Ordering::Relaxed);
                let filled = capacity - producer.slots();
                let instant =
                    if capacity > 0 { filled as f32 / capacity as f32 } else { 0.0 };
                smoothed += CAPTURE_FILL_SMOOTHING * (instant - smoothed);
                gauges.fill.store((smoothed * 1000.0) as u32, Ordering::Relaxed);
                // New audio arrived — mix it (mixer-demand-driven-wakeup L3
                // Flow D), bounded by MIXER_FALLBACK_INTERVAL if ever missed.
                waker.wake();
            }
            Err(_) => return, // this pid's stream is done — other pids/groups keep running
        }
        std::thread::sleep(poll_interval);
    }
}

/// Pushes `samples` into a capture ring **whole frames at a time**, returning
/// the sample count that did not fit.
///
/// The per-sample `producer.push(..).is_err()` loop this replaces could fail
/// on one channel of a frame and then succeed on the next, because the mixer
/// pops concurrently and frees slots mid-frame. That splits a frame across the
/// gap and shifts the ring's interleave by one sample **permanently** — every
/// later frame arrives channel-swapped. Same defect class as the pop side's
/// (`pull_group_inputs_never_pops_a_partial_frame`), on the producing end,
/// and far more likely to fire here: a ring carrying a standing rate surplus
/// sits at its brim continuously, so this is the steady state, not a rare race.
///
/// `slots()` only grows behind us (SPSC — only the mixer pops), so a frame
/// that fits at the check still fits at the push. Any tail shorter than a
/// whole frame is counted as dropped rather than pushed: keeping the ring
/// frame-aligned matters more than those samples, and counting makes an
/// off-frame port read visible instead of silently shifting the stream.
fn push_whole_frames(
    producer: &mut rtrb::Producer<f32>,
    samples: &[f32],
    channels: usize,
) -> u64 {
    let mut dropped = 0u64;
    let mut frames = samples.chunks_exact(channels);
    for frame in frames.by_ref() {
        if producer.slots() < channels {
            dropped += channels as u64;
            continue;
        }
        for &sample in frame {
            let _ = producer.push(sample); // guaranteed to fit by the check above
        }
    }
    dropped + frames.remainder().len() as u64
}

/// One group's capture-side gauges, read out of a locked `RunningGraph`: the
/// fullest of its pids' rings (0.0-1.0) and the total samples its pids have
/// delivered.
///
/// The *fullest*, not the mean: one pid backing up is the signal, and averaging
/// it against healthy pids in the same group hides it. A group with no pids
/// currently captured reads (0.0, 0).
fn capture_gauges(rg: &RunningGraph, group: GroupId) -> (f32, u64) {
    let Some(pids) = rg.capture_pids.get(&group) else {
        return (0.0, 0);
    };
    let fill = pids
        .values()
        .map(|pc| pc.fill_permille.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    let pushed = pids.values().map(|pc| pc.pushed_samples.load(Ordering::Relaxed)).sum();
    (fill as f32 / 1000.0, pushed)
}

/// What the drift loop actually controls against: one output's worth of
/// capture state, aggregated over every group routed to it, plus whether the
/// output itself is live.
///
/// The aggregate is the *fullest* ring and the *summed* delivery. One ratio
/// reaches all these groups (`SetOutputRatio`), so it has to answer to whichever
/// of them is closest to overflowing; and delivery by any of them means the
/// output is doing work. `output_active` gates on the render side, so a parked
/// or faulted output never has corrections integrated against it while nothing
/// can drain its ring.
fn output_capture_gauges(rg: &RunningGraph, output: OutputId) -> (f32, u64, bool) {
    let output_active = rg
        .output_ids
        .iter()
        .position(|id| *id == output)
        .and_then(|i| rg.ring_fill.get(i))
        .is_some_and(|gauge| gauge.active.load(Ordering::Relaxed));

    let mut fill = 0.0f32;
    let mut pushed = 0u64;
    for (group, _) in rg.group_outputs.iter().filter(|(_, out)| *out == output) {
        let (g_fill, g_pushed) = capture_gauges(rg, *group);
        fill = fill.max(g_fill);
        pushed += g_pushed;
    }
    (fill, pushed, output_active)
}

/// Applies `overrides` on top of `canonical`: a fallen-back group's
/// `output_device` is rewritten to the substitute endpoint's name; a parked
/// group's name is collected separately for `graph::resolve` to skip.
fn effective_snapshot(
    canonical: &ConfigSnapshot,
    overrides: &HashMap<GroupId, Option<Endpoint>>,
) -> (ConfigSnapshot, HashSet<String>) {
    let mut effective = canonical.clone();
    let mut parked = HashSet::new();
    for (i, g) in effective.groups.iter_mut().enumerate() {
        match overrides.get(&GroupId(i as u16)) {
            Some(Some(fallback)) => g.output_device = fallback.name.clone(),
            Some(None) => {
                parked.insert(g.name.clone());
            }
            None => {}
        }
    }
    (effective, parked)
}

/// Shared rebuild path for both app-triggered (`EngineHandle::rebuild`) and
/// supervisor-triggered (fault/device-event driven) rebuilds — both go
/// through the same canonical-snapshot + overrides state so a device fault
/// and a config change can never race each other into inconsistent state.
///
/// `new_canonical: Some(_)` means an app-driven config change: it replaces
/// the canonical snapshot and clears any in-flight fallback/park state (a
/// deliberate config change supersedes recovery bookkeeping). `None` means
/// a supervisor-triggered rebuild: reuse the current canonical snapshot and
/// whatever `overrides` the caller already updated.
///
/// Known race (accepted, not fixed here): if the app and the supervisor call
/// this concurrently, both may pass the initial "is anything running" check
/// before either takes the lock, causing one rebuild pass to be redundant
/// rather than skipped. Both passes are individually correct and the result
/// is eventually consistent; a stricter compare-and-swap isn't worth the
/// complexity for how infrequently rebuilds actually race.
fn apply_rebuild(
    sys: &Arc<dyn AudioSystem>,
    persistent: &Arc<Persistent>,
    running: &Mutex<Option<RunningGraph>>,
    new_canonical: Option<ConfigSnapshot>,
) -> Result<(), EngineError> {
    {
        let guard = running.lock().unwrap();
        if guard.is_none() {
            return Err(EngineError::AlreadyStopped);
        }
    }

    if let Some(snapshot) = new_canonical {
        *persistent.canonical_snapshot.lock().unwrap() = snapshot;
        persistent.overrides.lock().unwrap().clear();
    }
    let (effective, parked) = {
        let canonical = persistent.canonical_snapshot.lock().unwrap();
        let overrides = persistent.overrides.lock().unwrap();
        effective_snapshot(&canonical, &overrides)
    };

    let mut guard = running.lock().unwrap();
    let Some(rg) = guard.take() else {
        return Err(EngineError::AlreadyStopped);
    };
    stop_running_graph(rg);
    persistent.epoch.fetch_add(1, Ordering::Relaxed);
    let new_running = build_running_graph(&effective, sys, persistent, &parked)?;
    *guard = Some(new_running);
    queue_initial_dsp_bypass(persistent, &effective);
    Ok(())
}

/// Resolved config + every port opened, but nothing spawned yet. No more
/// `captures` (process-loopback-capture pivot): capture sources are pids,
/// matched live by `engine::routing` and wired in dynamically via
/// `CaptureControl` — a graph build never opens any capture port itself.
struct OpenedGraph {
    plan: GraphPlan,
    renders: Vec<(OutputId, Box<dyn RenderPort>)>,
}

/// Every process capture stream's fixed format (`graph::resolve`'s
/// `capture_format` param — every group's `input_format`, see that fn's
/// doc). **Not** queried from the system: confirmed on real hardware
/// (2026-07-21) that a process-loopback-activated `IAudioClient` doesn't
/// implement `GetMixFormat` at all (`E_NOTIMPL`) — the real `win-audio`
/// implementation *dictates* this exact format to every process capture
/// stream at `Initialize` time (`process_capture.rs`'s `fixed_capture_wfx`),
/// rather than negotiating one, so every stream reports the same value
/// regardless of the system's actual default device. `MockSystem` mirrors
/// the same constant independently for the same reason.
const PROCESS_CAPTURE_FORMAT: Format = Format {
    sample_rate: 48_000,
    channels: 2,
    layout: ChannelLayout::STEREO,
};

/// Opens every render port synchronously, before anything is spawned: fail
/// fast, nothing to unwind if a configured device doesn't open.
fn open_graph(
    snapshot: &ConfigSnapshot,
    sys: &Arc<dyn AudioSystem>,
    parked: &HashSet<String>,
) -> Result<OpenedGraph, EngineError> {
    let endpoints = sys.enumerate()?;
    let plan = graph::resolve(snapshot, &endpoints, parked, PROCESS_CAPTURE_FORMAT)?;

    let mut renders = Vec::with_capacity(plan.output_endpoints.len());
    for (output_id, endpoint_id) in &plan.output_endpoints {
        renders.push((*output_id, sys.open_render(endpoint_id)?));
    }

    Ok(OpenedGraph { plan, renders })
}

/// One empty `GroupSlot` per group in `plan` — no pids captured yet at
/// build/rebuild time; `CaptureControl::apply_capture_sources` populates
/// them once `engine::routing` matches live sessions against the fresh
/// topology.
fn build_group_slots(plan: &GraphPlan, group_output_index: &HashMap<GroupId, usize>) -> GroupConsumers {
    plan.topology
        .groups
        .iter()
        .map(|g| GroupSlot {
            group_id: g.id,
            pids: Vec::new(),
            channels: g.input_format.channels as usize,
            output_index: group_output_index[&g.id],
        })
        .collect()
}

type PreparedRenders = Vec<(OutputId, Box<dyn RenderPort>, rtrb::Consumer<f32>)>;

/// Builds every output's ring buffer without spawning any thread yet
/// (mixer-demand-driven-wakeup L3 Flow I): `render_loop` needs a `MixerWaker`,
/// which only exists once the mixer thread is spawned, and the mixer thread
/// needs these same producers — splitting "build the rings" from "spawn the
/// threads" resolves that construction-order circularity.
fn prepare_output_rings(renders: Vec<(OutputId, Box<dyn RenderPort>)>) -> (PreparedRenders, OutputProducers) {
    let mut prepared = Vec::with_capacity(renders.len());
    let mut producers = Vec::with_capacity(renders.len());
    for (output_id, port) in renders.into_iter() {
        let format = port.format();
        let device_period_s = port.period_frames() as f64 / format.sample_rate.max(1) as f64;
        let capacity = ring_capacity_samples(device_period_s, format.sample_rate, format.channels);
        let (producer, consumer) = RingBuffer::<f32>::new(capacity);
        producers.push((output_id, producer, format.channels as usize));
        prepared.push((output_id, port, consumer));
    }
    (prepared, producers)
}

fn spawn_render_threads(
    prepared: PreparedRenders,
    waker: MixerWaker,
    stop: &Arc<AtomicBool>,
    xruns: &Arc<AtomicU64>,
    shortfall: &Arc<AtomicU64>,
    faults: &Sender<Fault>,
    sys: &Arc<dyn AudioSystem>,
) -> Vec<JoinHandle<()>> {
    let mut threads = Vec::with_capacity(prepared.len());
    for (output_id, port, consumer) in prepared.into_iter() {
        let stop = Arc::clone(stop);
        let xruns = Arc::clone(xruns);
        let shortfall = Arc::clone(shortfall);
        let faults = faults.clone();
        let sys = Arc::clone(sys);
        let waker = waker.clone();
        threads.push(thread::spawn(move || {
            let ctx = RenderFaultCtx {
                xruns: &xruns,
                shortfall: &shortfall,
                output_id,
                faults: &faults,
            };
            render_loop(port, consumer, &stop, &ctx, sys.as_ref(), waker);
        }));
    }
    threads
}

fn build_running_graph(
    snapshot: &ConfigSnapshot,
    sys: &Arc<dyn AudioSystem>,
    persistent: &Arc<Persistent>,
    parked: &HashSet<String>,
) -> Result<RunningGraph, EngineError> {
    let opened = open_graph(snapshot, sys, parked)?;

    let wake_unit_period = compute_wake_unit_period(&opened.renders);
    let max_block_frames = compute_max_block_frames(&opened.plan, wake_unit_period);
    let mixer = Mixer::new(&opened.plan.topology, max_block_frames)?;
    log_channel_conversions(&opened.plan.topology, max_block_frames);

    let stop = Arc::new(AtomicBool::new(false));
    let xruns = Arc::new(AtomicU64::new(0));
    let output_drops = Arc::new(AtomicU64::new(0));
    let reject_diag = Arc::new(RejectDiag::default());
    let capture_drops = Arc::new(AtomicU64::new(0));
    let render_shortfall = Arc::new(AtomicU64::new(0));
    let group_ids: Vec<GroupId> = opened.plan.topology.groups.iter().map(|g| g.id).collect();
    let output_ids: Vec<OutputId> = opened.plan.topology.outputs.iter().map(|o| o.id).collect();
    // Index-parallel to group_consumers/output_producers respectively — the
    // governor's block_output_frames needs both sides of each group's rate
    // ratio (audio-flow-control decision 6/7).
    let group_input_rates: Vec<u32> = opened
        .plan
        .topology
        .groups
        .iter()
        .map(|g| g.input_format.sample_rate)
        .collect();
    let output_rates: Vec<u32> = opened
        .plan
        .topology
        .outputs
        .iter()
        .map(|o| o.format.sample_rate)
        .collect();
    let ring_fill = Arc::new(
        output_ids
            .iter()
            .map(|_| RingGauge {
                fill_permille: AtomicU32::new(0),
                active: AtomicBool::new(false),
            })
            .collect::<Vec<_>>(),
    );
    let output_index_of: HashMap<OutputId, usize> =
        output_ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let group_output_index: HashMap<GroupId, usize> = opened
        .plan
        .topology
        .groups
        .iter()
        .map(|g| (g.id, output_index_of[&g.output]))
        .collect();
    // Retained on RunningGraph for the recovery supervisor: maps a faulted
    // OutputId back to the endpoint that was bound to it and the groups
    // routed through it (drift-and-recovery interactions B/D).
    let group_outputs: Vec<(GroupId, OutputId)> = opened
        .plan
        .topology
        .groups
        .iter()
        .map(|g| (g.id, g.output))
        .collect();
    let output_endpoints: Vec<(OutputId, EndpointId)> = opened.plan.output_endpoints.clone();
    let output_devices: Vec<(OutputId, String)> = opened.plan.output_devices.clone();
    let group_formats: Vec<(GroupId, Format)> = opened
        .plan
        .topology
        .groups
        .iter()
        .map(|g| (g.id, g.input_format))
        .collect();
    let output_formats: Vec<(OutputId, Format)> = opened
        .plan
        .topology
        .outputs
        .iter()
        .map(|o| (o.id, o.format))
        .collect();
    let duck_depth_db = Arc::new(
        group_ids
            .iter()
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>(),
    );
    let limiter_engaged = Arc::new(
        output_ids
            .iter()
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>(),
    );
    // Level-meter gauges (level-meters.md): 0 == encode_meter(SILENT), so a
    // freshly-built graph already reports silent bars before the first tick.
    let group_peak = Arc::new(group_ids.iter().map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
    let output_peak = Arc::new(output_ids.iter().map(|_| AtomicU64::new(0)).collect::<Vec<_>>());

    let (fault_tx, fault_rx) = mpsc::channel();
    let group_consumers = build_group_slots(&opened.plan, &group_output_index);
    // Flow I: build every output's ring buffer first (no thread spawned yet),
    // then the mixer thread (its `Thread` handle becomes the one `MixerWaker`
    // every render thread needs), then the render threads themselves — in
    // that order, since `render_loop` requires a `MixerWaker` that only
    // exists once the mixer thread is spawned.
    let (prepared_renders, output_producers) = prepare_output_rings(opened.renders);

    let applied_ratio: Arc<Vec<AtomicU64>> = Arc::new(
        output_ids.iter().map(|_| AtomicU64::new(1.0f64.to_bits())).collect(),
    );

    let (capture_tx, capture_rx) = mpsc::channel();
    let mixer_args = MixerThreadArgs {
        max_block_frames,
        persistent: Arc::clone(persistent),
        ring_fill: Arc::clone(&ring_fill),
        stop: Arc::clone(&stop),
        sys: Arc::clone(sys),
        duck_depth_db: Arc::clone(&duck_depth_db),
        limiter_engaged: Arc::clone(&limiter_engaged),
        group_peak: Arc::clone(&group_peak),
        output_peak: Arc::clone(&output_peak),
        capture_rx,
        group_input_rates,
        output_rates,
        output_drops: Arc::clone(&output_drops),
        reject_diag: Arc::clone(&reject_diag),
        applied_ratio: Arc::clone(&applied_ratio),
        output_index_of: output_index_of.clone(),
    };
    let mixer_thread = thread::spawn(move || {
        mixer_loop(mixer, group_consumers, output_producers, mixer_args);
    });
    let mixer_waker = MixerWaker(mixer_thread.thread().clone());

    let render_threads = spawn_render_threads(
        prepared_renders,
        mixer_waker.clone(),
        &stop,
        &xruns,
        &render_shortfall,
        &fault_tx,
        sys,
    );

    Ok(RunningGraph {
        stop,
        capture_pids: HashMap::new(),
        capture_tx,
        mixer_thread: Some(mixer_thread),
        render_threads,
        xruns,
        output_drops,
        reject_diag,
        capture_drops,
        render_shortfall,
        applied_ratio,
        ring_fill,
        output_ids,
        group_ids,
        group_outputs,
        output_endpoints,
        fault_rx,
        group_formats,
        output_formats,
        max_block_frames,
        duck_depth_db,
        limiter_engaged,
        group_peak,
        output_peak,
        output_devices,
        mixer_waker,
    })
}

/// Off-RT, called once at graph build (startup/rebuild) — never on the mixer
/// thread. Surfaces silently-inserted channel conversions (L3 interaction D:
/// `.lattice/context/channel-mixdown.md`) and spatial-audio render choices
/// (spatial-audio.md's interaction F/E) so a downmix or binaural render that
/// changes what the user hears is visible, not a hidden mixer-internal detail.
fn log_channel_conversions(topology: &Topology, max_block_frames: usize) {
    for g in &topology.groups {
        let Some(out) = topology.outputs.iter().find(|o| o.id == g.output) else {
            continue;
        };
        if g.input_format.layout != out.format.layout {
            println!(
                "group {:?}: {}ch {:?} -> {}ch {:?} channel matrix",
                g.id, g.input_format.channels, g.input_format.layout, out.format.channels, out.format.layout
            );
        }
        if !g.spatial {
            continue;
        }
        if out.format.layout == ChannelLayout::STEREO {
            // Input rate: the spatializer runs pre-SRC (see `Render::build`).
            let taps = HrirSet::taps_for(g.input_format.sample_rate);
            let partition = max_block_frames.max(1).next_power_of_two();
            println!(
                "group {:?}: {}ch {:?} -> binaural (partition {partition}, hrir {taps} taps @{})",
                g.id, g.input_format.channels, g.input_format.layout, out.format.sample_rate
            );
        } else {
            println!("group {:?}: spatial ignored (output not stereo)", g.id);
        }
    }
}

/// Groups the identity + fault-reporting parameters `render_loop` needs
/// besides its port/consumer/stop (operational learnings: extract at that
/// point, not later; a param-count refactor applied to one of two mirrored
/// functions should be applied to both).
struct RenderFaultCtx<'a> {
    xruns: &'a AtomicU64,
    /// Frames offered to the device via `write` that it did not accept
    /// (audio-flow-control B1) — structurally impossible post-B1's
    /// `free_frames`-bounded pop, counted because "impossible" is what B1 was.
    shortfall: &'a AtomicU64,
    output_id: OutputId,
    faults: &'a Sender<Fault>,
}

fn render_loop(
    mut port: Box<dyn RenderPort>,
    mut consumer: rtrb::Consumer<f32>,
    stop: &AtomicBool,
    ctx: &RenderFaultCtx,
    sys: &dyn AudioSystem,
    waker: MixerWaker,
) {
    let _rt = sys.promote_rt_thread();
    let channels = port.format().channels.max(1) as usize;
    // Sized generously (RENDER_BUF_PERIODS periods); the amount actually
    // popped each event is bounded below by `free_frames()`, not this length
    // (audio-flow-control B1 — this used to be conflated with the device's
    // full buffer size).
    let mut buf = vec![0.0f32; RENDER_BUF_PERIODS * port.period_frames() * channels];
    // One `wait_event` means the device freed roughly one period, so one period
    // is what this loop is allowed to take from the ring each event — see the
    // `want_frames` comment below for why taking all of `free_frames` is the
    // bug this bounds.
    let period_frames = port.period_frames().max(1);
    let wait_timeout = Duration::from_millis(100);
    // Fill this ring must reach before the loop starts draining it: the
    // governor's own threshold. The governor stops producing there, so any
    // higher target is only reachable via a block overshoot, which would make
    // this exit condition race the mixer. At the threshold it is exactly
    // decision 7's "two full device periods of cushion".
    let prime_target_frames = (consumer.buffer().capacity() as f32
        / channels.max(1) as f32
        * GOVERNOR_THRESHOLD_FILL) as usize;
    let mut priming = true;

    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = port.wait_event(wait_timeout) {
            let _ = ctx.faults.send(Fault {
                source: FaultSource::Output(ctx.output_id),
                kind: FaultKind::from(&e),
            });
            return; // device invalidated — exit, rest of the graph keeps running
        }

        let free_frames = match port.free_frames() {
            Ok(f) => f,
            Err(e) => {
                let _ = ctx.faults.send(Fault {
                    source: FaultSource::Output(ctx.output_id),
                    kind: FaultKind::from(&e),
                });
                return;
            }
        };
        // Never ask for more than the device just told us it will accept
        // (B1) — the whole point of `free_frames` — and never more than the
        // single period this event actually corresponds to.
        //
        // The period cap is not a refinement, it is the fix for a stable
        // failure state: `free_frames` reports the device's WHOLE free buffer,
        // so taking that much drains the ring below the cushion `priming` just
        // built. The shortfall then gets padded with silence (below), which
        // means the device's queue shrinks, which makes `free_frames` LARGER
        // next event, which drains the ring harder — a self-sustaining loop
        // that punches a silence hole into every buffer written, at the device
        // period rate. Audibly: gated, skipping audio and a permanently
        // climbing `xruns`, not an occasional glitch.
        let want_frames = free_frames
            .min(period_frames)
            .min(buf.len() / channels.max(1));
        let want = want_frames * channels;
        let slice = &mut buf[..want];

        // The governor (Flow C) bounds an output ring from ABOVE only — it
        // withholds production, it cannot create audio — so nothing in the
        // blueprint establishes the ring FLOOR that decision 7's cushion
        // depends on. Production is capture-limited to roughly one device
        // period per period, matching this loop's drain exactly, so a ring
        // that starts empty stays empty: every event pops the little that is
        // there and silence-pads the rest. Priming builds the cushion once,
        // by feeding the device silence while leaving the ring alone.
        //
        // Re-armed only on a COMPLETELY empty ring: an empty ring is already
        // emitting silence, so waiting costs nothing, whereas a partially
        // filled one is mid-stream and must never be interrupted to rebuild a
        // cushion. That also covers engine start, where no capture exists yet.
        let available_frames = consumer.slots() / channels.max(1);
        if available_frames == 0 {
            priming = true;
        } else if available_frames >= prime_target_frames {
            priming = false;
        }

        // WHOLE FRAMES ONLY. The mixer pushes into this ring sample by sample,
        // so a half-written frame is routinely visible to this thread; popping
        // an odd sample count would leave every later pop one sample out of
        // phase with the interleave — a permanent channel swap, not a one-off
        // glitch. `slots()` only ever grows behind us (SPSC), so a frame count
        // derived from it is a safe lower bound and every pop below succeeds.
        let mut got = 0;
        if !priming {
            let take_frames = (slice.len() / channels).min(consumer.slots() / channels);
            for _ in 0..take_frames * channels {
                if let Ok(sample) = consumer.pop() {
                    slice[got] = sample;
                    got += 1;
                }
            }
        }
        // Write only the real audio we actually have. Silence is appended ONLY
        // when there is none at all this event — never behind real samples in
        // the same buffer, which is what gates the stream (see `want_frames`).
        // Writing short simply lets the device's queue shrink for one period;
        // it refills from the ring's cushion on the next event.
        let write_len = if got > 0 { got } else { slice.len() };
        if write_len > got {
            slice[got..write_len].fill(0.0);
            if !priming {
                // Priming is a deliberate cushion build, not an underrun —
                // counting it would make `xruns` unusable as the very signal
                // that tells us whether the floor is holding.
                ctx.xruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        let want_frames = write_len / channels;
        let slice = &buf[..write_len];
        // "I just consumed — refill me for next time" (mixer-demand-driven-
        // wakeup L3 Flow C), synchronized to this real hardware event's own
        // precision, before handing the just-drained buffer to the device.
        waker.wake();
        match port.write(slice) {
            Ok(accepted_frames) => {
                // Structurally impossible now (we only ever offer up to what
                // free_frames() just reported) — counted, never silent, per
                // B1's own "never move more than the receiver can accept AND
                // never let a shortfall go silent" invariant.
                if accepted_frames < want_frames {
                    ctx.shortfall
                        .fetch_add((want_frames - accepted_frames) as u64, Ordering::Relaxed);
                }
            }
            Err(e) => {
                let _ = ctx.faults.send(Fault {
                    source: FaultSource::Output(ctx.output_id),
                    kind: FaultKind::from(&e),
                });
                return;
            }
        }
    }
}

/// Everything the mixer thread needs besides the `Mixer` and its rings —
/// grouped so `mixer_loop` takes 4 parameters instead of 9.
struct MixerThreadArgs {
    max_block_frames: usize,
    persistent: Arc<Persistent>,
    ring_fill: Arc<Vec<RingGauge>>,
    /// Per-output applied-ratio gauge and the index map into it, so the mixer
    /// thread can record each `SetOutputRatio` it applies without a lock.
    applied_ratio: Arc<Vec<AtomicU64>>,
    output_index_of: HashMap<OutputId, usize>,
    stop: Arc<AtomicBool>,
    sys: Arc<dyn AudioSystem>,
    duck_depth_db: Arc<Vec<AtomicU32>>,
    limiter_engaged: Arc<Vec<AtomicU64>>,
    group_peak: Arc<Vec<AtomicU64>>,
    output_peak: Arc<Vec<AtomicU64>>,
    /// `CaptureControl::apply_capture_sources` sends per-pid add/remove here —
    /// drained once per tick, same as `persistent.commands`.
    capture_rx: Receiver<CaptureMsg>,
    /// Each group's input sample rate, index-parallel to `group_consumers` —
    /// the governor's `block_output_frames` needs it alongside the group's
    /// own output's rate (audio-flow-control decision 6/7).
    group_input_rates: Vec<u32>,
    /// Each output's sample rate, index-parallel to `output_producers`.
    output_rates: Vec<u32>,
    /// Frames the governor's budget said fit that an output ring rejected
    /// anyway (cap 3) — should stay 0; a disagreement between the budget and
    /// the ring's real capacity.
    output_drops: Arc<AtomicU64>,
    /// Why the last rejected push was rejected — see
    /// [`EngineStats::last_output_reject`].
    reject_diag: Arc<RejectDiag>,
}

fn mixer_loop(
    mut mixer: Mixer,
    mut group_consumers: GroupConsumers,
    mut output_producers: OutputProducers,
    args: MixerThreadArgs,
) {
    let _rt = args.sys.promote_rt_thread();

    let mut group_scratch: Vec<Vec<f32>> = group_consumers
        .iter()
        .map(|slot| vec![0.0f32; args.max_block_frames * slot.channels])
        .collect();
    // Sized from the mixer's own accumulator, NOT from `max_block_frames`:
    // that counts frames at a group's input rate, while this holds frames at
    // the output device's rate. A short buffer here makes `take_output`
    // truncate the tick and drop the remainder on the floor (the accumulator
    // is cleared regardless), so a 48 kHz capture into a 96 kHz device loses
    // half of every block.
    let mut output_scratch: Vec<Vec<f32>> = output_producers
        .iter()
        .map(|(id, _, _)| vec![0.0f32; mixer.output_capacity(*id)])
        .collect();
    // Hysteresis state for RingGauge.active — mixer-thread-owned, no per-tick alloc.
    let mut real_this_tick = vec![false; output_producers.len()];
    let mut ticks_since_real = vec![ACTIVE_HOLD_TICKS; output_producers.len()];
    // Built once at thread start (not per-tick — plain lookups only below):
    // parallel id lists matching `args.duck_depth_db`/`args.limiter_engaged`'s
    // index order, which was built from the same topology.
    let group_ids: Vec<GroupId> = group_consumers.iter().map(|slot| slot.group_id).collect();
    let output_ids: Vec<OutputId> = output_producers.iter().map(|(id, ..)| *id).collect();
    // Governor budget, computed once (audio-flow-control decision 6): output
    // frames one full input block becomes after each group's own SRC.
    let block_out_frames: Vec<usize> = group_consumers
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            let group_rate = args.group_input_rates[i];
            let output_rate = args.output_rates[slot.output_index];
            block_output_frames(args.max_block_frames, group_rate, output_rate)
        })
        .collect();
    // The same budget, folded per output: the largest block any group feeding
    // it may push. Only the reject diagnostic reads it — `group_may_push`
    // still decides per group, against that group's own block.
    let budget_per_output: Vec<usize> = (0..output_producers.len())
        .map(|out_i| {
            group_consumers
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.output_index == out_i)
                .map(|(g, _)| block_out_frames[g])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut headroom = vec![OutputHeadroom { filled_frames: 0, capacity_frames: 0 }; output_producers.len()];

    while !args.stop.load(Ordering::Relaxed) {
        drain_capture_commands(&args.capture_rx, &mut group_consumers);
        drain_commands(&args.persistent, &mut mixer, &args.applied_ratio, &args.output_index_of);
        // Sampled once per tick, before any group is pulled, so every group
        // this tick decides against the same snapshot (audio-flow-control
        // Flow C) — a stalled output can't be made to look healthier by a
        // push that happens to land between two groups' checks.
        sample_output_headroom(&output_producers, &mut headroom);
        pull_group_inputs(
            &mut group_consumers,
            &mut group_scratch,
            &mut mixer,
            &mut real_this_tick,
            &headroom,
            &block_out_frames,
        );
        mixer.mix_tick();
        update_telemetry(&mixer, &group_ids, &output_ids, &args);
        flush_outputs(
            &mut output_producers,
            &mut output_scratch,
            &mut mixer,
            FlushCtx {
                ring_fill: &args.ring_fill,
                real_this_tick: &real_this_tick,
                ticks_since_real: &mut ticks_since_real,
                drops: &args.output_drops,
                reject_diag: &args.reject_diag,
                budget_frames: &budget_per_output,
            },
        );

        // Block (zero CPU) until real demand wakes this thread again — a
        // render/capture event or a command enqueue (mixer-demand-driven-
        // wakeup L3 Flow B/C/D/E), or the fallback bound if none arrives in
        // time (Flow B/H). `park`/`unpark` coalesce (Flow F): several wakes
        // before this park collapse into one, safe because the tick body
        // above already drained everything pending regardless of source.
        thread::park_timeout(MIXER_FALLBACK_INTERVAL);
    }
}

/// Copies this tick's telemetry (P5 DSP gauges + level-meters.md peak meters)
/// out of the `Mixer` into the cross-thread gauges `stats` reads — same pattern
/// as `RingGauge`. Runs after `mix_tick`, so every reading reflects the buffer
/// `mix_tick` just finished processing; `take_output` (which clears the
/// accumulator) has not run yet, so the output meter still sees this tick's
/// audio.
fn update_telemetry(mixer: &Mixer, group_ids: &[GroupId], output_ids: &[OutputId], args: &MixerThreadArgs) {
    for (i, id) in group_ids.iter().enumerate() {
        args.duck_depth_db[i].store(mixer.group_duck_depth_db(*id).to_bits(), Ordering::Relaxed);
        args.group_peak[i].store(encode_meter(mixer.group_peak(*id)), Ordering::Relaxed);
    }
    for (i, id) in output_ids.iter().enumerate() {
        if mixer.output_limiter_engaged(*id) {
            args.limiter_engaged[i].fetch_add(1, Ordering::Relaxed);
        }
        args.output_peak[i].store(encode_meter(mixer.output_peak(*id)), Ordering::Relaxed);
    }
}

fn drain_commands(
    persistent: &Persistent,
    mixer: &mut Mixer,
    applied_ratio: &[AtomicU64],
    output_index_of: &HashMap<OutputId, usize>,
) {
    while let Some(envelope) = persistent.commands.pop() {
        if envelope.epoch.0 != persistent.epoch.load(Ordering::Relaxed) {
            continue; // stale — dropped, not applied (topology epoch, notes §7)
        }
        // Matches on a reference: `MixerCommand` is no longer `Copy` (P5's
        // `SwapChain` carries a `Box<DspChain>`), and `envelope.cmd` is still
        // needed whole by `mixer.apply` below.
        if let MixerCommand::SetOutputRatio(output_id, ratio) = &envelope.cmd {
            // Surfaced via EngineStats::applied_ratio; output_index_of is a
            // plain lookup (no alloc/lock), safe on the mixer's RT thread.
            if let Some(&index) = output_index_of.get(output_id) {
                applied_ratio[index].store(ratio.value().to_bits(), Ordering::Relaxed);
            }
        }
        if let Some(retired_chain) = mixer.apply(envelope.cmd) {
            // Epoch bump happens HERE, as a side effect of applying the
            // swap — not when the command was sent (dsp-pipeline.md
            // revision). Any command still queued behind this one that
            // targeted the pre-swap chain shape (e.g. a stage index that no
            // longer exists) is dropped by the epoch check above on the
            // next iteration of this loop.
            persistent.epoch.fetch_add(1, Ordering::Relaxed);
            // Retired chain's `Drop` deallocates — never on this RT thread.
            // Best-effort push: a full queue means the supervisor hasn't
            // drained recently; the chain leaks until it does, same
            // tolerance as every other ring/queue in this codebase.
            let _ = persistent.retired.push(retired_chain);
        }
    }
}

/// Applies pending `CaptureControl::apply_capture_sources` add/remove
/// requests to the mixer thread's own local `GroupSlot` list — the only
/// place `GroupSlot.pids` is ever mutated, so no lock is needed even though
/// the requests originate on another thread (process-loopback-capture L4).
fn drain_capture_commands(capture_rx: &Receiver<CaptureMsg>, group_consumers: &mut [GroupSlot]) {
    while let Ok(msg) = capture_rx.try_recv() {
        match msg {
            CaptureMsg::Add { group, pid, consumer } => {
                if let Some(slot) = group_consumers.iter_mut().find(|s| s.group_id == group) {
                    slot.pids.push((pid, consumer));
                }
            }
            CaptureMsg::Remove { group, pid } => {
                if let Some(slot) = group_consumers.iter_mut().find(|s| s.group_id == group) {
                    slot.pids.retain(|(p, _)| *p != pid);
                }
            }
        }
    }
}

/// One output ring's state, sampled once per tick before any group is pulled
/// (audio-flow-control Flow C) — in that output's own frames, not samples.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OutputHeadroom {
    filled_frames: usize,
    capacity_frames: usize,
}

/// Snapshots every output ring's fill/capacity once per tick, in frames.
fn sample_output_headroom(
    output_producers: &[(OutputId, rtrb::Producer<f32>, usize)],
    out: &mut [OutputHeadroom],
) {
    for (i, (_, producer, channels)) in output_producers.iter().enumerate() {
        let capacity_samples = producer.buffer().capacity();
        let filled_samples = capacity_samples - producer.slots();
        let ch = (*channels).max(1);
        out[i] = OutputHeadroom {
            filled_frames: filled_samples / ch,
            capacity_frames: capacity_samples / ch,
        };
    }
}

/// Output frames one full input block becomes after that group's SRC —
/// computed once per group at mixer-thread start (audio-flow-control
/// decision 7), not per tick.
fn block_output_frames(block_frames: usize, group_rate: u32, output_rate: u32) -> usize {
    ((block_frames as u64 * output_rate.max(1) as u64) / group_rate.max(1) as u64) as usize
}

/// Policy (β, decision 6): may this group push a full block this tick? Pure —
/// testable with synthetic headroom, no `Mixer` and no threads. Skips once
/// the output ring is at/above `GOVERNOR_THRESHOLD_FILL`, and as a hard
/// safety net (beyond the default constants' own numbers, which never hit
/// this in practice) never pushes a block that would overflow the ring
/// outright.
fn group_may_push(headroom: OutputHeadroom, block_out_frames: usize) -> bool {
    let threshold_frames = (GOVERNOR_THRESHOLD_FILL * headroom.capacity_frames as f32) as usize;
    headroom.filled_frames < threshold_frames
        && headroom.filled_frames + block_out_frames <= headroom.capacity_frames
}

/// Sums every pid currently captured into a group's scratch buffer, one
/// frame at a time — a group with zero pids (nothing matched yet, or every
/// pid starved this tick) behaves exactly like the old single-consumer
/// "starved" case: silence, never a stall (the mixer tick is timer-paced).
///
/// Governed per group (audio-flow-control B2/B4, decision 6: full-block-or-
/// skip): a group whose own output is at/above threshold (`group_may_push`)
/// is skipped entirely this tick — its pids' rings are left untouched (the
/// audio waits for a tick with headroom) and `push_group` is called with an
/// empty slice, so the mixer sees zero valid frames rather than replaying a
/// stale block. A group allowed to push pulls up to one full block from its
/// pids and pushes only the frames actually popped — never the zero-padded
/// remainder (B4): `filled_max` is the frame count, maxed across pids (pids
/// are summed sample-by-sample; lengths are maxed, matching the existing
/// per-pid summation). `real_this_tick` for a group's output is set whenever
/// *any* pushed group popped at least one real frame (B8: no longer requires
/// a fully-filled block, which the governor's own skip could otherwise make
/// permanently unreachable).
fn pull_group_inputs(
    group_consumers: &mut [GroupSlot],
    group_scratch: &mut [Vec<f32>],
    mixer: &mut Mixer,
    real_this_tick: &mut [bool],
    headroom: &[OutputHeadroom],
    block_out_frames: &[usize],
) {
    real_this_tick.fill(false);
    for (i, slot) in group_consumers.iter_mut().enumerate() {
        let scratch = &mut group_scratch[i];

        // No pids: nothing can ever push to this group again until one is
        // added, so any partial chunk its resampler holds would gate this
        // output's span forever (mixer.rs `GATE_GRACE_TICKS`). Dropping it here
        // is the exact signal — the grace would free the span eventually, but
        // only after an audible gap on every *other* group sharing the output.
        if slot.pids.is_empty() {
            mixer.discard_group_partial_input(slot.group_id);
        }

        if !group_may_push(headroom[slot.output_index], block_out_frames[i]) {
            mixer.push_group(slot.group_id, &[]);
            continue;
        }

        scratch.fill(0.0);
        let channels = slot.channels.max(1);
        let mut filled_max_frames = 0usize;
        for (_, consumer) in slot.pids.iter_mut() {
            // WHOLE FRAMES ONLY, same reasoning as `render_loop`'s pop: the
            // capture thread pushes sample by sample, so a half-written frame
            // is routinely visible here. Popping the odd sample and then
            // dropping it (`filled_samples / channels` truncates, and
            // `scratch` is zeroed next tick) desyncs this ring's interleave
            // permanently — every later frame arrives channel-swapped.
            // `slots()` only grows behind us (SPSC), so every pop below succeeds.
            let take_frames = (scratch.len() / channels).min(consumer.slots() / channels);
            let mut filled_samples = 0;
            for _ in 0..take_frames * channels {
                if let Ok(sample) = consumer.pop() {
                    scratch[filled_samples] += sample;
                    filled_samples += 1;
                }
            }
            filled_max_frames = filled_max_frames.max(filled_samples / channels);
        }
        if filled_max_frames > 0 {
            real_this_tick[slot.output_index] = true;
        }
        mixer.push_group(slot.group_id, &scratch[..filled_max_frames * channels]);
    }
}

/// The parameter threshold, extracted at the same point as `RenderFaultCtx`
/// (operational learnings) — `flush_outputs` would otherwise take 6.
struct FlushCtx<'a> {
    ring_fill: &'a [RingGauge],
    real_this_tick: &'a [bool],
    ticks_since_real: &'a mut [u32],
    /// Frames the governor's budget said fit that the ring rejected anyway —
    /// should stay 0 (cap 3); non-zero means the budget and the ring's real
    /// capacity disagree.
    drops: &'a AtomicU64,
    /// Which of the possible disagreements it was — see
    /// [`EngineStats::last_output_reject`].
    reject_diag: &'a RejectDiag,
    /// Governor budget per output, in that output's frames: the largest block
    /// any group feeding it may push. Index-parallel to `output_producers`.
    budget_frames: &'a [usize],
}

fn flush_outputs(
    output_producers: &mut [(OutputId, rtrb::Producer<f32>, usize)],
    output_scratch: &mut [Vec<f32>],
    mixer: &mut Mixer,
    ctx: FlushCtx<'_>,
) {
    for (i, (output_id, producer, channels)) in output_producers.iter_mut().enumerate() {
        let scratch = &mut output_scratch[i];
        let n = mixer.take_output(*output_id, scratch);
        // Captured before the push loop consumes it: what the ring could have
        // taken at the moment the mixer offered this span.
        let free_before = producer.slots();
        let mut dropped = 0u64;
        for &sample in &scratch[..n] {
            if producer.push(sample).is_err() {
                dropped += 1; // counted, never silent (cap 3) — the governor should prevent this
            }
        }
        if dropped > 0 {
            ctx.drops.fetch_add(dropped, Ordering::Relaxed);
            // In frames, so it compares directly against the governor's budget
            // and the headroom snapshot it decided on. Relaxed and unordered
            // between fields: the four are read together for a human, never
            // acted on, and this path is already a fault.
            let ch = (*channels).max(1);
            let d = ctx.reject_diag;
            d.span_frames.store((n / ch) as u64, Ordering::Relaxed);
            d.free_frames.store((free_before / ch) as u64, Ordering::Relaxed);
            d.capacity_frames
                .store((producer.buffer().capacity() / ch) as u64, Ordering::Relaxed);
            d.budget_frames
                .store(ctx.budget_frames.get(i).copied().unwrap_or(0) as u64, Ordering::Relaxed);
            d.seen.store(true, Ordering::Relaxed);
        }
        let capacity = producer.buffer().capacity();
        let filled = capacity - producer.slots();
        let permille = if capacity > 0 {
            (filled * 1000 / capacity) as u32
        } else {
            0
        };
        ctx.ring_fill[i]
            .fill_permille
            .store(permille, Ordering::Relaxed);

        let active = update_activity(ctx.real_this_tick[i], &mut ctx.ticks_since_real[i]);
        ctx.ring_fill[i].active.store(active, Ordering::Relaxed);
    }
}

/// Hysteresis for `RingGauge.active` (notes §6): resets on any real-audio
/// tick, otherwise counts up; active while under the hold window. Pulled out
/// of `flush_outputs` so the debounce logic is unit-testable without a
/// running `Mixer`.
fn update_activity(real_this_tick: bool, ticks_since_real: &mut u32) -> bool {
    *ticks_since_real = if real_this_tick {
        0
    } else {
        ticks_since_real.saturating_add(1)
    };
    *ticks_since_real < ACTIVE_HOLD_TICKS
}

fn frames_for(duration: Duration, sample_rate: u32) -> usize {
    (duration.as_secs_f64() * sample_rate as f64)
        .ceil()
        .max(1.0) as usize
}

fn ring_capacity_samples(period_s: f64, sample_rate: u32, channels: u16) -> usize {
    let frames = frames_for(Duration::from_secs_f64(period_s.max(0.0)), sample_rate);
    (frames * RING_PERIOD_MARGIN * channels.max(1) as usize).max(64)
}

/// notes §5 (superseded — decision 2): buffer-sizing basis is a full render
/// period plus `WAKE_MARGIN` headroom, not the old implicit half-period
/// doubling. The halving existed to compensate for a polled/`spin_sleep`
/// wake's imprecision; a real park/unpark wake doesn't carry that same
/// imprecision, but some margin is still kept for scheduling jitter between
/// "render drained" and "mixer refilled." No capture ports here
/// (process-loopback-capture pivot — capture sources are opened dynamically
/// per pid, long after this is fixed at build time): render ports alone set
/// the floor, same fallback for an empty topology as before.
fn compute_wake_unit_period(renders: &[(OutputId, Box<dyn RenderPort>)]) -> Duration {
    let min_period_s = renders
        .iter()
        .map(|(_, r)| r.period_frames() as f64 / r.format().sample_rate.max(1) as f64)
        .fold(f64::INFINITY, f64::min);

    let period_s = if min_period_s.is_finite() {
        min_period_s * WAKE_MARGIN
    } else {
        0.005 // no ports at all (empty topology) — arbitrary safe default
    };
    Duration::from_secs_f64(period_s)
}

fn compute_max_block_frames(plan: &GraphPlan, wake_unit_period: Duration) -> usize {
    plan.topology
        .groups
        .iter()
        .map(|g| frames_for(wake_unit_period, g.input_format.sample_rate) + BLOCK_FRAME_MARGIN)
        .max()
        .unwrap_or(BLOCK_FRAME_MARGIN)
}

/// Everything a supervisor tick needs read out of the live `RunningGraph`,
/// captured under one short lock so the rest of the tick runs lock-free.
struct SupervisorSnapshot {
    output_ids: Vec<OutputId>,
    /// Per output, in `output_ids` order: the fullest capture ring among the
    /// groups routed there, the running total of samples those groups' pids
    /// have delivered, and whether the output is live. Read out under the same
    /// lock rather than handed over as `Arc`s, because `capture_pids` is
    /// rebuilt whenever routing changes — a cloned handle would go stale.
    capture: Vec<(f32, u64, bool)>,
    group_outputs: Vec<(GroupId, OutputId)>,
    output_endpoints: Vec<(OutputId, EndpointId)>,
}

fn push_envelope(persistent: &Persistent, cmd: MixerCommand) {
    let epoch = Epoch(persistent.epoch.load(Ordering::Relaxed));
    // Best-effort: if the bounded queue is full, the next drift tick (~100ms
    // later) will just issue a fresh correction — no need to retry here.
    let _ = persistent.commands.push(Envelope { epoch, cmd });
}

/// A fresh `DspChain` always starts un-bypassed (`BypassRamp::new`) — this
/// re-applies any `bypassed: true` persisted in config right after a build,
/// via the same `SetDspBypass` command path a live UI toggle uses. Called
/// after both `start()` and every successful `apply_rebuild()`; a stray
/// command for a group that got parked out of the actual topology is
/// silently dropped by `Mixer::apply`, same as any other unknown-id command.
fn queue_initial_dsp_bypass(persistent: &Persistent, snapshot: &ConfigSnapshot) {
    for (i, group) in snapshot.groups.iter().enumerate() {
        for (stage, cfg) in group.dsp.iter().enumerate() {
            if cfg.bypassed {
                push_envelope(
                    persistent,
                    MixerCommand::SetDspBypass {
                        group: GroupId(i as u16),
                        stage,
                        bypassed: true,
                    },
                );
            }
        }
    }
}

/// Recovery supervisor (drift-and-recovery, notes §10 + L3 interactions
/// A-E): the only thread that ticks `DriftController`, watches for RT-thread
/// faults and OS device-change notifications, and triggers rebuilds to
/// recover from them. Lives for the whole `EngineHandle` lifetime — it is
/// *not* torn down/respawned by `apply_rebuild` the way capture/mixer/render
/// threads are, since it needs to keep running to react to the graph it just
/// rebuilt.
fn supervisor_loop(
    running: Arc<Mutex<Option<RunningGraph>>>,
    persistent: Arc<Persistent>,
    sys: Arc<dyn AudioSystem>,
    stop: Arc<AtomicBool>,
) {
    let default_tick = DriftConfig::default().tick;
    let device_events = sys.subscribe_device_events().ok();
    let mut drift = DriftController::new(&[], DriftConfig::default());
    let mut known_outputs: Vec<OutputId> = Vec::new();
    // Previous tick's pid delivery totals, in `known_outputs` order — the idle
    // guard's baseline. An output whose groups delivered nothing over a tick is
    // skipped by the controller.
    let mut last_pushed: Vec<u64> = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        // Off-RT drop of chains retired by a `SwapChain` apply (notes §7) —
        // runs every tick regardless of graph state, independent of the
        // `running` lock below.
        while let Some(chain) = persistent.retired.pop() {
            drop(chain);
        }

        let snapshot = {
            let guard = running.lock().unwrap();
            guard.as_ref().map(|rg| SupervisorSnapshot {
                output_ids: rg.output_ids.clone(),
                capture: rg.output_ids.iter().map(|id| output_capture_gauges(rg, *id)).collect(),
                group_outputs: rg.group_outputs.clone(),
                output_endpoints: rg.output_endpoints.clone(),
            })
        };
        let Some(snap) = snapshot else {
            std::thread::sleep(default_tick);
            continue;
        };

        // Topology changed since last tick (rebuild happened) — old integrator
        // state no longer applies to a fresh set of rings.
        if snap.output_ids != known_outputs {
            drift = DriftController::new(&snap.output_ids, DriftConfig::default());
            known_outputs = snap.output_ids.clone();
            last_pushed = vec![0; known_outputs.len()];
        }

        // The capture rings are what this loop regulates (clock.rs's module
        // doc), aggregated per output because one ratio drives every group
        // routed there. `active` is delivery since the previous tick, not ring
        // level: a ring sitting at its brim is the state most in need of
        // correction, so a guard keyed on the ring itself would switch the
        // controller off exactly when the surplus needed clearing.
        let fills: Vec<(OutputId, FillSample)> = snap
            .output_ids
            .iter()
            .zip(snap.capture.iter())
            .zip(last_pushed.iter_mut())
            .map(|((id, (fill, pushed, output_active)), last)| {
                let delivering = *pushed > *last;
                *last = *pushed;
                (*id, FillSample { fill: *fill, active: delivering && *output_active })
            })
            .collect();
        for cmd in drift.tick(&fills) {
            push_envelope(&persistent, cmd);
        }

        let mut dead_endpoints: Vec<EndpointId> = Vec::new();
        {
            let guard = running.lock().unwrap();
            if let Some(rg) = guard.as_ref() {
                while let Ok(fault) = rg.fault_rx.try_recv() {
                    if let (FaultKind::DeviceInvalidated, FaultSource::Output(output_id)) =
                        (fault.kind, fault.source)
                    {
                        if let Some((_, endpoint_id)) =
                            rg.output_endpoints.iter().find(|(id, _)| *id == output_id)
                        {
                            dead_endpoints.push(endpoint_id.clone());
                        }
                    }
                }
            }
        }

        let mut added_endpoints: Vec<Endpoint> = Vec::new();
        // Kept separate from `dead_endpoints`, which also collects *faulted*
        // outputs: a fault can be a format change on a device that never left,
        // so announcing `DeviceRemoved` from that set would report removals
        // that didn't happen. Only an actual OS removal notification lands here.
        let mut removed_endpoints: Vec<EndpointId> = Vec::new();
        if let Some(rx) = &device_events {
            while let Ok(evt) = rx.try_recv() {
                match evt {
                    DeviceEvent::Removed(id) => removed_endpoints.push(id),
                    DeviceEvent::Added(endpoint) => added_endpoints.push(endpoint),
                    DeviceEvent::DefaultChanged(id) => {
                        let _ = persistent.events_tx.send(EngineEvent::DefaultDeviceChanged(id));
                    }
                    DeviceEvent::StateChanged(_) => {}
                }
            }
        }

        // Dedup by endpoint id within this tick's drained batch — WASAPI can
        // fire OnDeviceAdded more than once for the same physical device
        // (composite/virtual devices, driver re-enumeration bursts). Same
        // pattern as `dead_endpoints` below; without it a duplicate `Added`
        // in one batch means a duplicate `EngineEvent::DeviceAvailable` to
        // the app layer (harmless but noisy — the restore path itself was
        // already idempotent via `overrides`).
        // Announced before the recovery pass below, and unconditionally —
        // same contract as `DeviceAvailable` in `handle_device_added`: the app
        // layer's device list changed whether or not any group was affected.
        removed_endpoints.sort_by(|a, b| a.0.cmp(&b.0));
        removed_endpoints.dedup();
        for removed in &removed_endpoints {
            let _ = persistent
                .events_tx
                .send(EngineEvent::DeviceRemoved(removed.clone()));
        }
        dead_endpoints.extend(removed_endpoints);

        dead_endpoints.sort_by(|a, b| a.0.cmp(&b.0));
        dead_endpoints.dedup();
        for dead in dead_endpoints {
            handle_endpoint_lost(&persistent, &sys, &running, &snap, &dead);
        }

        added_endpoints.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        added_endpoints.dedup_by(|a, b| a.id == b.id);
        for endpoint in added_endpoints {
            handle_device_added(&persistent, &sys, &running, endpoint);
        }

        std::thread::sleep(default_tick);
    }
}

/// Interaction B/D: a physical device faulted (`PortError::DeviceInvalidated`
/// or an OS removal notification, dedup'd by endpoint id upstream). WASAPI
/// reports both a genuine removal *and* a format change as the same stream
/// invalidation (notes §3/§4) — disambiguated below by re-enumerating: still
/// present means format-change (interaction D, reopen the same endpoint,
/// `Recovered`); gone means a real removal (interaction B, fall the affected
/// groups back to the current default output, or park them silent if none is
/// available). Either way the rest of the graph keeps running (L1 capability 2).
fn handle_endpoint_lost(
    persistent: &Arc<Persistent>,
    sys: &Arc<dyn AudioSystem>,
    running: &Arc<Mutex<Option<RunningGraph>>>,
    snap: &SupervisorSnapshot,
    dead: &EndpointId,
) {
    let Some(&(output_id, _)) = snap
        .output_endpoints
        .iter()
        .find(|(_, endpoint_id)| endpoint_id == dead)
    else {
        return; // not an endpoint currently in use — nothing to do
    };
    let groups: Vec<GroupId> = snap
        .group_outputs
        .iter()
        .filter(|(_, oid)| *oid == output_id)
        .map(|(gid, _)| *gid)
        .collect();
    if groups.is_empty() {
        return;
    }

    let still_present = sys
        .enumerate()
        .map(|eps| eps.iter().any(|e| &e.id == dead))
        .unwrap_or(false);
    if still_present {
        // Format-change: same endpoint, just needs a fresh open to pick up
        // the new mix format — no fallback substitution, overrides untouched.
        if apply_rebuild(sys, persistent, running, None).is_err() {
            return;
        }
        let _ = persistent.events_tx.send(EngineEvent::Recovered {
            groups,
            on: dead.clone(),
        });
        return;
    }

    {
        // Idempotency: both the fault channel and the OS notification can
        // report the same physical removal — only act once per endpoint.
        let overrides = persistent.overrides.lock().unwrap();
        if groups.iter().all(|g| overrides.contains_key(g)) {
            return;
        }
    }

    let fallback = sys.default_output().ok().filter(|e| &e.id != dead);
    {
        let mut overrides = persistent.overrides.lock().unwrap();
        for g in &groups {
            overrides.insert(*g, fallback.clone());
        }
    }

    // Rebuild failure here leaves the engine stopped (apply_rebuild's own
    // documented simplification) — nothing further the supervisor can do;
    // the overrides stay recorded in case a later manual rebuild succeeds.
    if apply_rebuild(sys, persistent, running, None).is_err() {
        return;
    }

    let event = match fallback {
        Some(target) => EngineEvent::FallbackApplied {
            groups,
            from: dead.clone(),
            to: target.id,
        },
        None => EngineEvent::DeviceLost { groups },
    };
    let _ = persistent.events_tx.send(event);
}

/// Interaction C: a device came back (or a new one showed up). Always
/// surfaces `DeviceAvailable` — a new selectable routing target for the app
/// layer (L1 capability 3), independent of recovery. Additionally, if its
/// name matches what any currently-overridden group was originally
/// configured for, restores that group to its canonical device.
fn handle_device_added(
    persistent: &Arc<Persistent>,
    sys: &Arc<dyn AudioSystem>,
    running: &Arc<Mutex<Option<RunningGraph>>>,
    endpoint: Endpoint,
) {
    let _ = persistent
        .events_tx
        .send(EngineEvent::DeviceAvailable(endpoint.clone()));

    let restorable: Vec<GroupId> = {
        let canonical = persistent.canonical_snapshot.lock().unwrap();
        let overrides = persistent.overrides.lock().unwrap();
        overrides
            .keys()
            .filter(|gid| {
                canonical
                    .groups
                    .get(gid.0 as usize)
                    .is_some_and(|g| g.output_device == endpoint.name)
            })
            .copied()
            .collect()
    };
    if restorable.is_empty() {
        return;
    }

    {
        let mut overrides = persistent.overrides.lock().unwrap();
        for g in &restorable {
            overrides.remove(g);
        }
    }

    if apply_rebuild(sys, persistent, running, None).is_err() {
        return;
    }

    let _ = persistent.events_tx.send(EngineEvent::Recovered {
        groups: restorable,
        on: endpoint.id,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::mock::MockSystem;
    use crate::ports::{Endpoint, EndpointId};
    use audio_core::{ChannelLayout, Format, Gain};
    use std::thread::sleep;

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
            layout: ChannelLayout::STEREO,
        }
    }

    fn mono(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 1,
            layout: ChannelLayout::MONO,
        }
    }

    fn mock_endpoints() -> Vec<Endpoint> {
        vec![Endpoint {
            id: EndpointId("out-1".into()),
            name: "Speakers".into(),
            format: stereo(48_000),
        }]
    }

    fn two_output_endpoints() -> Vec<Endpoint> {
        let mut eps = mock_endpoints();
        eps.push(Endpoint {
            id: EndpointId("out-2".into()),
            name: "Headphones".into(),
            format: stereo(48_000),
        });
        eps
    }

    fn snapshot() -> ConfigSnapshot {
        ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            app: graph::AppConfig::default(),
            profiles: Vec::new(),
            groups: vec![graph::GroupConfig {
                name: "Game".into(),
                output_device: "Speakers".into(),
                gain: Gain::UNITY,
                follow_master: true,
                match_rules: vec![],
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                muted: false,
                hotkey_mute: None,
                hotkey_volume_up: None,
                hotkey_volume_down: None,
            }],
        }
    }

    #[test]
    fn start_runs_and_shuts_down_cleanly() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        sleep(Duration::from_millis(30));
        handle.shutdown().unwrap();
    }

    #[test]
    fn healthy_run_reports_no_group_faults() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        sleep(Duration::from_millis(50));
        assert!(handle.stats().group_faults.is_empty());
        handle.shutdown().unwrap();
    }

    #[test]
    fn healthy_run_reports_ring_fill_for_every_output() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        sleep(Duration::from_millis(50));
        assert_eq!(handle.stats().ring_fill.len(), 1); // one output in `snapshot()`
        handle.shutdown().unwrap();
    }

    #[test]
    fn failed_rebuild_leaves_the_engine_stopped_not_rolled_back() {
        // Documents the no-rollback simplification logged in engine-core.md:
        // a rebuild that fails to resolve the new snapshot does not restore
        // the graph that was running before it.
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();

        let mut broken = snapshot();
        broken.groups[0].output_device = "does-not-exist".into();
        assert!(matches!(
            handle.rebuild(&broken),
            Err(EngineError::Resolve(_))
        ));

        assert!(matches!(
            handle.apply_params(vec![]),
            Err(EngineError::AlreadyStopped)
        ));
    }

    #[test]
    fn rebuild_bumps_the_epoch() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        let before = handle.epoch();
        handle.rebuild(&snapshot()).unwrap();
        assert_eq!(handle.epoch(), Epoch(before.0 + 1));
        handle.shutdown().unwrap();
    }

    #[test]
    fn apply_params_are_delivered_to_the_mixer() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        handle
            .apply_params(vec![MixerCommand::SetGroupGain(GroupId(0), Gain::SILENT)])
            .unwrap();
        // No panic / no error is the assertion here: apply_params round-trips
        // through the real lock-free queue into a live mixer thread.
        sleep(Duration::from_millis(30));
        handle.shutdown().unwrap();
    }

    #[test]
    fn healthy_run_reports_dsp_telemetry_for_every_group_and_output() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        sleep(Duration::from_millis(50));
        let stats = handle.stats();
        assert_eq!(stats.duck_depth_db.len(), 1); // one group in `snapshot()`
        assert_eq!(stats.duck_depth_db[0], (GroupId(0), 0.0)); // no duck configured
        assert_eq!(stats.limiter_engaged.len(), 1); // one output in `snapshot()`
        // Level meters (level-meters.md): one gauge per group / per output,
        // keyed by the same ids, silent under the mock's no-signal capture.
        assert_eq!(stats.group_peak.len(), 1);
        assert_eq!(stats.group_peak[0].0, GroupId(0));
        assert_eq!(stats.output_peak.len(), 1);
        assert_eq!(stats.output_peak[0].0, OutputId(0));
        // Device-name mapping for the master-column labels (level-meters.md).
        assert_eq!(stats.output_names.len(), 1);
        assert_eq!(stats.output_names[0].0, OutputId(0));
        handle.shutdown().unwrap();
    }

    #[test]
    fn a_stats_reader_sees_the_same_gauges_as_the_handle() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        let reader = handle.stats_reader();
        sleep(Duration::from_millis(30));

        // The clone-able reader is what the UI thread holds; it must report the
        // same shape as the owning handle while the engine runs.
        let via_reader = reader.clone().stats();
        assert_eq!(via_reader.group_peak.len(), handle.stats().group_peak.len());
        assert_eq!(via_reader.output_peak.len(), handle.stats().output_peak.len());

        handle.shutdown().unwrap();
        // After the graph stops, the same reader reports empty (the UI renders
        // this as meters at the floor), never a stale panic.
        assert!(reader.stats().group_peak.is_empty());
        assert!(reader.stats().output_peak.is_empty());
    }

    #[test]
    fn apply_dsp_chains_swaps_the_running_groups_chain() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        handle
            .apply_dsp_chains(vec![(GroupId(0), vec![DspSpec::Limiter { ceiling_db: -6.0 }])])
            .unwrap();
        // No panic / no error is the assertion here — the swap round-trips
        // through the same command queue as apply_params, is applied by the
        // mixer thread, and the retired original chain is dropped off-RT by
        // the supervisor (asserting the drop itself would need instrumenting
        // DspChain's Drop, which is more than this integration boundary needs).
        sleep(Duration::from_millis(30));
        handle.shutdown().unwrap();
    }

    #[test]
    fn apply_spatial_toggles_the_running_groups_render() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        handle.apply_spatial(&[(GroupId(0), true)]).unwrap();
        // No panic / no error is the assertion here — the swap round-trips
        // through the same command queue as apply_params/apply_dsp_chains,
        // is applied by the mixer thread, and the retired original render
        // is dropped off-RT by the supervisor. `snapshot()`'s one group
        // routes to a stereo output, so this exercises the real Spatial
        // path, not just the non-stereo fallback.
        sleep(Duration::from_millis(30));
        handle.shutdown().unwrap();
    }

    #[test]
    fn apply_spatial_on_unknown_group_is_a_silent_no_op() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        assert!(handle.apply_spatial(&[(GroupId(99), true)]).is_ok());
        handle.shutdown().unwrap();
    }

    #[test]
    fn apply_spatial_after_the_engine_has_stopped_is_an_error() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        let mut broken = snapshot();
        broken.groups[0].output_device = "does-not-exist".into();
        assert!(handle.rebuild(&broken).is_err());

        assert!(matches!(
            handle.apply_spatial(&[(GroupId(0), true)]),
            Err(EngineError::AlreadyStopped)
        ));
    }

    #[test]
    fn apply_dsp_chains_on_unknown_group_is_a_silent_no_op() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        assert!(handle
            .apply_dsp_chains(vec![(GroupId(99), vec![DspSpec::Limiter { ceiling_db: -6.0 }])])
            .is_ok());
        handle.shutdown().unwrap();
    }

    #[test]
    fn apply_dsp_chains_after_the_engine_has_stopped_is_an_error() {
        // Same trigger as `failed_rebuild_leaves_the_engine_stopped_not_rolled_back`:
        // a rebuild that fails to resolve leaves the graph stopped with no rollback.
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        let mut broken = snapshot();
        broken.groups[0].output_device = "does-not-exist".into();
        assert!(handle.rebuild(&broken).is_err());

        assert!(matches!(
            handle.apply_dsp_chains(vec![(GroupId(0), vec![])]),
            Err(EngineError::AlreadyStopped)
        ));
    }

    #[test]
    fn start_with_no_matching_endpoints_returns_resolve_error() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(vec![])); // no endpoints at all
        let result = start(&snapshot(), sys);
        assert!(matches!(result, Err(EngineError::Resolve(_))));
    }

    #[test]
    fn update_activity_stays_active_while_under_the_hold_window() {
        let mut ticks_since_real = 0u32;
        assert!(update_activity(true, &mut ticks_since_real));
        for _ in 0..(ACTIVE_HOLD_TICKS - 1) {
            assert!(update_activity(false, &mut ticks_since_real));
        }
    }

    #[test]
    fn update_activity_goes_inactive_once_the_hold_window_elapses() {
        let mut ticks_since_real = 0u32;
        update_activity(true, &mut ticks_since_real);
        for _ in 0..ACTIVE_HOLD_TICKS {
            update_activity(false, &mut ticks_since_real);
        }
        assert!(!update_activity(false, &mut ticks_since_real));
    }

    #[test]
    fn update_activity_reactivates_immediately_on_real_audio() {
        let mut ticks_since_real = ACTIVE_HOLD_TICKS + 5; // long-silent
        assert!(update_activity(true, &mut ticks_since_real));
    }

    struct FailingCapture;
    impl CapturePort for FailingCapture {
        fn read(&mut self, _buf: &mut [f32]) -> Result<usize, PortError> {
            Err(PortError::DeviceInvalidated)
        }
        fn format(&self) -> Format {
            stereo(48_000)
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_millis(1)
        }
    }

    struct FailingRender;
    impl RenderPort for FailingRender {
        fn wait_event(&mut self, _timeout: Duration) -> Result<(), PortError> {
            Err(PortError::Backend("simulated backend failure".into()))
        }
        fn free_frames(&self) -> Result<usize, PortError> {
            Ok(usize::MAX)
        }
        fn write(&mut self, frames: &[f32]) -> Result<usize, PortError> {
            Ok(frames.len() / self.format().channels.max(1) as usize)
        }
        fn format(&self) -> Format {
            stereo(48_000)
        }
        fn period_frames(&self) -> usize {
            480
        }
    }

    #[test]
    fn pid_capture_loop_exits_quietly_on_a_read_failure() {
        // No fault reporting for per-pid captures (process-loopback-capture
        // pivot): a read error just ends this one thread — asserting on the
        // absence of a panic/hang is the point, there's nothing else to
        // observe from outside the loop.
        let stop = AtomicBool::new(false);
        let sys = MockSystem::new(vec![]);
        let (producer, _consumer) = RingBuffer::<f32>::new(4);
        let waker = MixerWaker(thread::current());
        let drops = AtomicU64::new(0);

        pid_capture_loop(
            Box::new(FailingCapture),
            producer,
            &stop,
            &sys,
            waker,
            CaptureGauges {
                drops: &drops,
                fill: &AtomicU32::new(0),
                pushed: &AtomicU64::new(0),
            },
        );
    }

    #[test]
    fn capture_buf_samples_covers_a_whole_poll_interval() {
        // B3: the old fixed 256-frame buffer was ~53% of a single ~10ms/48kHz
        // interval. CAPTURE_BUF_INTERVALS=2 covers a whole interval with
        // margin: 10ms*2 = 20ms @ 48kHz = 960 frames, stereo -> 1920 samples.
        let samples = capture_buf_samples(Duration::from_millis(10), 48_000, 2);
        assert_eq!(samples, 1920);
    }

    #[test]
    fn pid_capture_loop_counts_ring_full_drops() {
        // A source that always has more audio ready than a tiny consumer
        // ring can hold — every sample that doesn't fit must be counted,
        // never silently discarded via `let _ = producer.push(..)` (B2/B3).
        struct BurstCapture {
            calls: u32,
        }
        impl CapturePort for BurstCapture {
            fn read(&mut self, buf: &mut [f32]) -> Result<usize, PortError> {
                self.calls += 1;
                if self.calls > 1 {
                    return Err(PortError::DeviceInvalidated); // end the loop after one read
                }
                buf.fill(0.1);
                Ok(buf.len())
            }
            fn format(&self) -> Format {
                mono(48_000)
            }
            fn poll_interval(&self) -> Duration {
                Duration::from_millis(1)
            }
        }
        let stop = AtomicBool::new(false);
        let sys = MockSystem::new(vec![]);
        let ring_capacity = 10;
        let (producer, _consumer) = RingBuffer::<f32>::new(ring_capacity);
        let waker = MixerWaker(thread::current());
        let drops = AtomicU64::new(0);
        let fill = AtomicU32::new(0);

        pid_capture_loop(
            Box::new(BurstCapture { calls: 0 }),
            producer,
            &stop,
            &sys,
            waker,
            CaptureGauges { drops: &drops, fill: &fill, pushed: &AtomicU64::new(0) },
        );

        // buf is capture_buf_samples(1ms, 48000, 1) = 96 samples; only
        // ring_capacity fit, the rest must show up here, not vanish.
        let expected_buf_len = capture_buf_samples(Duration::from_millis(1), 48_000, 1);
        assert_eq!(
            drops.load(Ordering::Relaxed),
            (expected_buf_len - ring_capacity) as u64
        );
        // The same overflow must also move the gauge, not only the counter:
        // `capture_drops` says samples were lost, `capture_fill` is what
        // distinguishes a standing surplus from a transient burst. The gauge is
        // one-pole smoothed from a mid-scale seed, so one poll against a full
        // ring moves it up by one coefficient's worth rather than snapping to
        // the brim — the smoothing is the point (an unsmoothed reading railed
        // the drift controller on packet phase).
        let seeded = (CAPTURE_FILL_SMOOTHING_SEED * 1000.0) as u32;
        assert!(
            fill.load(Ordering::Relaxed) > seeded,
            "a full ring must push the smoothed gauge above its seed, got {}",
            fill.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn capture_fill_smoothing_rejects_packet_phase_swing() {
        // What the smoothing is for. A capture ring alternates between "packet
        // just landed" and "mixer just drained" every poll; the drift
        // controller must see the standing level, not that alternation. Live
        // hardware showed 0.27–0.77 tick to tick against a 0.5 target, which at
        // kp=0.05 demands 2.5x the correction clamp — so the controller sat on
        // alternate rails and drove nothing but noise into the resample ratio.
        //
        // Modelled directly: alternate the extremes and check the smoothed
        // value stays close to their mean.
        let mut smoothed = CAPTURE_FILL_SMOOTHING_SEED;
        let mut worst_excursion = 0.0f32;
        for i in 0..2_000 {
            let instant = if i % 2 == 0 { 0.27 } else { 0.77 };
            smoothed += CAPTURE_FILL_SMOOTHING * (instant - smoothed);
            if i > 200 {
                worst_excursion = worst_excursion.max((smoothed - 0.52).abs());
            }
        }
        assert!(
            worst_excursion < 0.02,
            "smoothed fill must track the mean of the swing, not the swing — \
             worst excursion {worst_excursion:.4} from the 0.52 mean"
        );
    }

    #[test]
    fn a_saturated_ring_still_counts_as_delivering() {
        // The drift loop's idle guard skips a group whose pids delivered
        // nothing since the last tick. `pushed_samples` therefore has to count
        // what the PORT handed over, not what the ring accepted: a ring at its
        // brim rejects everything, and that is exactly the state the loop
        // exists to correct. Counting accepted samples would mark the group
        // idle precisely when its surplus needed clearing, and the ratio would
        // never be pulled down — the bug would survive its own fix.
        struct OneBurst {
            done: bool,
        }
        impl CapturePort for OneBurst {
            fn read(&mut self, buf: &mut [f32]) -> Result<usize, PortError> {
                if self.done {
                    return Err(PortError::DeviceInvalidated);
                }
                self.done = true;
                buf.fill(0.25);
                Ok(buf.len())
            }
            fn format(&self) -> Format {
                mono(48_000)
            }
            fn poll_interval(&self) -> Duration {
                Duration::from_millis(1)
            }
        }
        let stop = AtomicBool::new(false);
        let sys = MockSystem::new(vec![]);
        // Capacity 0-ish: `ring_capacity_samples` floors at 64, but here the
        // ring is built directly, so 1 slot makes almost every push fail.
        let (producer, _consumer) = RingBuffer::<f32>::new(1);
        let drops = AtomicU64::new(0);
        let pushed = AtomicU64::new(0);

        pid_capture_loop(
            Box::new(OneBurst { done: false }),
            producer,
            &stop,
            &sys,
            MixerWaker(thread::current()),
            CaptureGauges { drops: &drops, fill: &AtomicU32::new(0), pushed: &pushed },
        );

        let delivered = capture_buf_samples(Duration::from_millis(1), 48_000, 1) as u64;
        assert_eq!(drops.load(Ordering::Relaxed), delivered - 1, "all but one sample rejected");
        assert_eq!(
            pushed.load(Ordering::Relaxed),
            delivered,
            "activity must reflect what the port delivered, not what the full ring took"
        );
    }

    #[test]
    fn a_full_capture_ring_drops_whole_frames_never_half_of_one() {
        // The per-sample push loop this replaces could place a frame's L and
        // drop its R (or vice versa), shifting the ring's interleave by one
        // sample for the rest of the stream — every later frame arrives
        // channel-swapped. A ring carrying a standing surplus sits at its brim
        // continuously, so this is the steady state, not a rare race.
        //
        // Capacity 3 with stereo frames is the shape that catches it: one
        // whole frame fits, and the third slot is a trap the old loop fell
        // into by pushing the next frame's L into it.
        let (mut producer, consumer) = RingBuffer::<f32>::new(3);
        let samples = [1.0f32, -1.0, 2.0, -2.0, 3.0, -3.0];

        let dropped = push_whole_frames(&mut producer, &samples, 2);

        assert_eq!(dropped, 4, "two of the three frames must not fit");
        assert_eq!(
            consumer.slots() % 2,
            0,
            "the ring must hold whole frames only — an odd count means a frame was split \
             across the overflow, swapping L/R for every frame after it"
        );
    }

    #[test]
    fn a_capture_read_that_ends_mid_frame_never_shifts_the_ring() {
        // A port returning a sample count that isn't a whole number of frames
        // shouldn't happen (WASAPI hands over whole frames), but pushing the
        // dangling sample would desync the interleave exactly as an overflow
        // split would. It is counted rather than pushed, so an off-frame port
        // shows up in `capture_drops` instead of silently corrupting.
        let (mut producer, consumer) = RingBuffer::<f32>::new(16);

        let dropped = push_whole_frames(&mut producer, &[1.0, -1.0, 2.0], 2);

        assert_eq!(dropped, 1, "the dangling half-frame is counted");
        assert_eq!(consumer.slots(), 2, "and never enters the ring");
    }

    #[test]
    fn capture_fill_tracks_a_partly_filled_ring() {
        // Not just the brim case: the gauge has to read proportionally, or a
        // ring climbing toward overflow looks identical to a healthy one right
        // up until it starts dropping.
        let (mut producer, _consumer) = RingBuffer::<f32>::new(10);
        push_whole_frames(&mut producer, &[0.5; 4], 2);

        let capacity = producer.buffer().capacity();
        let filled = capacity - producer.slots();

        assert_eq!((filled * 1000 / capacity) as u32, 400);
    }

    #[test]
    fn take_events_returns_a_receiver_once() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let mut handle = start(&snapshot(), sys).unwrap();
        let _rx = handle.take_events();
        handle.shutdown().unwrap();
    }

    #[test]
    #[should_panic(expected = "take_events called more than once")]
    fn take_events_panics_on_second_call() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let mut handle = start(&snapshot(), sys).unwrap();
        let _first = handle.take_events();
        let _second = handle.take_events();
    }

    /// Builds a `Persistent` without spawning a supervisor — for tests that
    /// want to exercise `build_running_graph`/the mixer thread in isolation,
    /// deterministically, without racing the real supervisor's own routine
    /// drift-correction commands (which land on the same command queue).
    fn bare_persistent(snapshot: &ConfigSnapshot) -> Arc<Persistent> {
        let (events_tx, _events_rx) = mpsc::channel();
        Arc::new(Persistent {
            commands: ArrayQueue::new(COMMAND_QUEUE_CAPACITY),
            epoch: AtomicU64::new(0),
            events_tx,
            canonical_snapshot: Mutex::new(snapshot.clone()),
            overrides: Mutex::new(HashMap::new()),
            retired: ArrayQueue::new(RETIRED_CHAIN_QUEUE_CAPACITY),
        })
    }

    #[test]
    fn applied_ratio_starts_at_unity_for_every_output() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let persistent = bare_persistent(&snapshot());
        let rg = build_running_graph(&snapshot(), &sys, &persistent, &HashSet::new()).unwrap();
        assert_eq!(rg.applied_ratio.len(), rg.output_ids.len());
        let ratio = f64::from_bits(rg.applied_ratio[0].load(Ordering::Relaxed));
        assert_eq!(ratio, 1.0);
        stop_running_graph(rg);
    }

    #[test]
    fn a_parked_mixer_still_ticks_within_the_fallback_interval() {
        // Pushes straight onto the raw command queue, bypassing
        // `EngineHandle::apply_params` (the only call site that wakes the
        // mixer explicitly) — so this command can only ever be picked up by
        // `mixer_loop`'s MIXER_FALLBACK_INTERVAL bound (Flow B/H), never a
        // real wake. Sleeping comfortably past that bound (not the old
        // 30ms — that assumed the pre-redesign fixed-clock tick) proves the
        // fallback still fires with nothing else driving it.
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let persistent = bare_persistent(&snapshot());
        let rg = build_running_graph(&snapshot(), &sys, &persistent, &HashSet::new()).unwrap();
        let ratio = audio_core::ResampleRatio::new(1.003).unwrap();
        let epoch = Epoch(persistent.epoch.load(Ordering::Relaxed));
        let pushed = persistent.commands.push(Envelope {
            epoch,
            cmd: MixerCommand::SetOutputRatio(OutputId(0), ratio),
        });
        assert!(pushed.is_ok());
        sleep(MIXER_FALLBACK_INTERVAL + Duration::from_millis(50));
        let applied = f64::from_bits(rg.applied_ratio[0].load(Ordering::Relaxed));
        assert_eq!(applied, 1.003);
        stop_running_graph(rg);
    }

    #[test]
    fn render_loop_reports_non_invalidated_faults_as_other() {
        let stop = AtomicBool::new(false);
        let xruns = AtomicU64::new(0);
        let shortfall = AtomicU64::new(0);
        let (fault_tx, fault_rx) = mpsc::channel();
        let sys = MockSystem::new(vec![]);
        let (_producer, consumer) = RingBuffer::<f32>::new(4);
        let ctx = RenderFaultCtx {
            xruns: &xruns,
            shortfall: &shortfall,
            output_id: OutputId(0),
            faults: &fault_tx,
        };
        let waker = MixerWaker(thread::current());

        render_loop(Box::new(FailingRender), consumer, &stop, &ctx, &sys, waker);

        let fault = fault_rx.recv().unwrap();
        assert!(matches!(fault.source, FaultSource::Output(OutputId(0))));
        assert!(matches!(fault.kind, FaultKind::Other));
    }

    #[test]
    fn render_loop_pops_only_what_the_device_will_accept() {
        // B1's core regression: a device offering far less free space than
        // the ring holds must only ever lose exactly that much from the
        // ring — never the whole buf-sized pop the old GetBufferSize-based
        // sizing produced.
        let format = stereo(48_000);
        let (sink, device) = crate::ports::mock::SinkRender::paced(format, 4, 4);
        let ring_frames = 40;
        let (mut producer, consumer) = RingBuffer::<f32>::new(ring_frames * 2);
        for _ in 0..ring_frames * 2 {
            producer.push(0.5).unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let xruns = AtomicU64::new(0);
        let shortfall = AtomicU64::new(0);
        let (fault_tx, _fault_rx) = mpsc::channel();
        let sys = MockSystem::new(vec![]);
        let ctx = RenderFaultCtx {
            xruns: &xruns,
            shortfall: &shortfall,
            output_id: OutputId(0),
            faults: &fault_tx,
        };
        let waker = MixerWaker(thread::current());

        let device2 = device.clone();
        let stop2 = Arc::clone(&stop);
        let helper = thread::spawn(move || {
            // `drain` releases a counter, not a one-shot signal, so this is
            // safe to call before render_loop even reaches its first
            // wait_event -- no wait needed here.
            device2.drain(0);
            // Poll-until-deadline (established idiom in this file, e.g.
            // `a_pid_whose_capture_thread_dies_mid_stream_is_reaped_and_retried`)
            // instead of guessing a fixed sleep duration: wait for the real
            // observable effect of one pop+write landing before signalling
            // stop, so the test isn't tied to how fast that happens to run.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while device2.filled_frames() == 0 && std::time::Instant::now() < deadline {
                thread::yield_now();
            }
            stop2.store(true, Ordering::Relaxed);
            device2.drain(0); // release the second (now-blocking) wait_event so it can notice stop
        });

        render_loop(Box::new(sink), consumer, &stop, &ctx, &sys, waker);
        helper.join().unwrap();

        assert_eq!(
            device.filled_frames(),
            4,
            "the device must only have accepted one period's worth, not the whole ring"
        );
        let remaining = ring_frames * 2 - producer.slots();
        assert_eq!(
            remaining, 72,
            "only the frames the device could accept may leave the ring — the rest must stay, never be discarded (B1)"
        );
    }

    #[test]
    fn render_loop_counts_a_short_write_instead_of_discarding_it() {
        // cap 3: even though free_frames()-bounded popping makes a short
        // write structurally impossible in the happy path, a write that
        // still accepts less than offered (e.g. the device's state changed
        // between free_frames() and write()) must be counted, never
        // swallowed into a bare success.
        struct ShortWriteRender {
            first_call: bool,
        }
        impl RenderPort for ShortWriteRender {
            fn wait_event(&mut self, _timeout: Duration) -> Result<(), PortError> {
                if self.first_call {
                    self.first_call = false;
                    Ok(())
                } else {
                    Err(PortError::DeviceInvalidated)
                }
            }
            fn free_frames(&self) -> Result<usize, PortError> {
                Ok(8)
            }
            fn write(&mut self, frames: &[f32]) -> Result<usize, PortError> {
                let channels = 2;
                let offered = frames.len() / channels;
                Ok(offered / 2) // accepts half of what free_frames() promised
            }
            fn format(&self) -> Format {
                stereo(48_000)
            }
            fn period_frames(&self) -> usize {
                4
            }
        }
        let stop = AtomicBool::new(false);
        let xruns = AtomicU64::new(0);
        let shortfall = AtomicU64::new(0);
        let (fault_tx, _fault_rx) = mpsc::channel();
        let sys = MockSystem::new(vec![]);
        let (mut producer, consumer) = RingBuffer::<f32>::new(32);
        for _ in 0..16 {
            producer.push(0.1).unwrap(); // 8 frames stereo
        }
        let ctx = RenderFaultCtx {
            xruns: &xruns,
            shortfall: &shortfall,
            output_id: OutputId(0),
            faults: &fault_tx,
        };
        let waker = MixerWaker(thread::current());

        render_loop(Box::new(ShortWriteRender { first_call: true }), consumer, &stop, &ctx, &sys, waker);

        // The loop offers one device period (4 frames), not all 8 free frames —
        // the port accepts half of that, so 2 frames go uncounted-if-silent.
        assert_eq!(
            shortfall.load(Ordering::Relaxed),
            2,
            "a write accepting less than offered must be counted, never silent"
        );
    }

    #[test]
    fn render_loop_never_writes_silence_behind_real_audio_in_one_buffer() {
        // The defect this pins produced ~50 xruns/second on real hardware and
        // gated, skipping output: `free_frames` reports the WHOLE free device
        // buffer, so the loop took far more than the ring held, wrote
        // `real audio + silence padding` in one buffer, and thereby made the
        // device's queue shorter — which made `free_frames` bigger next event.
        // A silence hole per period, forever.
        //
        // Here the device offers 8 free frames but its period is 4 and the ring
        // holds 5, so the old code would have written 5 real + 3 silent frames.
        // Succeeds once then errors, so exactly one event runs.
        struct OneEventRender {
            first: bool,
            written: Arc<Mutex<Vec<f32>>>,
        }
        impl RenderPort for OneEventRender {
            fn wait_event(&mut self, _timeout: Duration) -> Result<(), PortError> {
                if std::mem::replace(&mut self.first, false) {
                    Ok(())
                } else {
                    Err(PortError::DeviceInvalidated)
                }
            }
            fn free_frames(&self) -> Result<usize, PortError> {
                Ok(8) // the whole free device buffer — two full periods
            }
            fn write(&mut self, frames: &[f32]) -> Result<usize, PortError> {
                self.written.lock().unwrap().extend_from_slice(frames);
                Ok(frames.len() / 2)
            }
            fn format(&self) -> Format {
                stereo(48_000)
            }
            fn period_frames(&self) -> usize {
                4
            }
        }

        let ring_frames = 8; // prime target 4 frames, so 5 queued clears priming
        let (mut producer, consumer) = RingBuffer::<f32>::new(ring_frames * 2);
        for _ in 0..5 * 2 {
            producer.push(0.5).unwrap();
        }
        let stop = AtomicBool::new(false);
        let xruns = AtomicU64::new(0);
        let shortfall = AtomicU64::new(0);
        let (fault_tx, _fault_rx) = mpsc::channel();
        let sys = MockSystem::new(vec![]);
        let ctx = RenderFaultCtx {
            xruns: &xruns,
            shortfall: &shortfall,
            output_id: OutputId(0),
            faults: &fault_tx,
        };
        let waker = MixerWaker(thread::current());

        let written = Arc::new(Mutex::new(Vec::new()));
        render_loop(
            Box::new(OneEventRender { first: true, written: Arc::clone(&written) }),
            consumer,
            &stop,
            &ctx,
            &sys,
            waker,
        );

        let written = written.lock().unwrap();
        assert_eq!(
            *written,
            vec![0.5f32; 4 * 2],
            "one event may take exactly one device period, all real audio — the old code \
             wrote 5 real frames plus 3 of silence, gating the stream every period"
        );
        assert_eq!(
            ring_frames * 2 - producer.slots(),
            2,
            "only one period may leave the ring; the rest is the cushion and must stay"
        );
        assert_eq!(
            xruns.load(Ordering::Relaxed),
            0,
            "a ring that supplied a full period is not an underrun"
        );
    }

    #[test]
    fn pull_group_inputs_never_pops_a_partial_frame() {
        // The capture thread pushes sample by sample, so a ring holding an ODD
        // number of samples is a normal mid-write observation, not corruption.
        // The old loop popped that odd sample and then dropped it (integer
        // division to frames), permanently shifting the ring's interleave by
        // one sample — every later frame arrives channel-swapped.
        let mut slots = vec![GroupSlot {
            group_id: GroupId(0),
            pids: Vec::new(),
            channels: 2,
            output_index: 0,
        }];
        let (mut producer, consumer) = RingBuffer::<f32>::new(16);
        for s in [1.0f32, 2.0, 3.0, 4.0, 5.0] {
            producer.push(s).unwrap(); // 2 whole frames + one dangling L sample
        }
        slots[0].pids.push((1234, consumer));

        let mut scratch = vec![vec![0.0f32; 8 * 2]];
        let mut mixer = Mixer::new(
            &Topology {
                master: audio_core::Gain::UNITY,
                groups: vec![audio_core::GroupSpec {
                    id: GroupId(0),
                    gain: audio_core::Gain::UNITY,
                    follow_master: false,
                    output: OutputId(0),
                    input_format: stereo(48_000),
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                    mute: false,
                }],
                outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: stereo(48_000) }],
            },
            8,
        )
        .unwrap();
        let mut real = vec![false];
        let headroom = vec![OutputHeadroom { filled_frames: 0, capacity_frames: 1_000 }];

        pull_group_inputs(&mut slots, &mut scratch, &mut mixer, &mut real, &headroom, &[8]);

        let (_, consumer) = &slots[0].pids[0];
        assert_eq!(
            consumer.slots(),
            1,
            "the dangling half-frame must stay in the ring for the next tick, never be \
             popped and discarded — discarding it swaps L/R for the rest of the stream"
        );
    }

    #[test]
    fn rings_prime_before_the_first_render_event() {
        // Flow G. The governor can only withhold production, never create
        // audio, so a ring that starts empty would otherwise be drained to
        // zero by every event and silence-padded — a gap roughly every third
        // event until the drift controller integrates the level up over
        // seconds. Below the prime target the loop must feed the device
        // silence and leave the ring untouched.
        let format = stereo(48_000);
        let (sink, device) = crate::ports::mock::SinkRender::paced(format, 4, 8);
        // 20 frames capacity -> a 10-frame prime target at GOVERNOR_THRESHOLD_FILL.
        let ring_frames = 20;
        let queued_frames = 6; // real audio, but short of the target
        let (mut producer, consumer) = RingBuffer::<f32>::new(ring_frames * 2);
        for _ in 0..queued_frames * 2 {
            producer.push(0.5).unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let xruns = AtomicU64::new(0);
        let shortfall = AtomicU64::new(0);
        let (fault_tx, _fault_rx) = mpsc::channel();
        let sys = MockSystem::new(vec![]);
        let ctx = RenderFaultCtx {
            xruns: &xruns,
            shortfall: &shortfall,
            output_id: OutputId(0),
            faults: &fault_tx,
        };
        let waker = MixerWaker(thread::current());

        let device2 = device.clone();
        let stop2 = Arc::clone(&stop);
        let helper = thread::spawn(move || {
            device2.drain(0); // release the first wait_event
            // Poll-until-deadline for the real observable effect (the same
            // idiom as `render_loop_pops_only_what_the_device_will_accept`)
            // rather than guessing a sleep duration.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while device2.filled_frames() == 0 && std::time::Instant::now() < deadline {
                thread::yield_now();
            }
            stop2.store(true, Ordering::Relaxed);
            device2.drain(0); // release the now-blocking second wait_event
        });

        render_loop(Box::new(sink), consumer, &stop, &ctx, &sys, waker);
        helper.join().unwrap();

        assert_eq!(
            ring_frames * 2 - producer.slots(),
            queued_frames * 2,
            "below the prime target the ring must not be drained at all — the cushion \
             has to build before the first real pop"
        );
        assert!(
            device.recorded().iter().all(|&s| s == 0.0),
            "a priming event feeds the device silence, never a partial pop"
        );
        assert_eq!(
            xruns.load(Ordering::Relaxed),
            0,
            "priming is a deliberate cushion build, not an underrun — counting it would \
             destroy the one signal that reports whether the floor holds"
        );
    }

    // --- mixer-demand-driven-wakeup test contracts (context doc L4) ---

    #[test]
    fn compute_wake_unit_period_applies_the_margin_not_a_half_period_split() {
        struct FixedPeriodRender {
            period_frames: usize,
            format: Format,
        }
        impl RenderPort for FixedPeriodRender {
            fn wait_event(&mut self, _timeout: Duration) -> Result<(), PortError> {
                Ok(())
            }
            fn free_frames(&self) -> Result<usize, PortError> {
                Ok(usize::MAX)
            }
            fn write(&mut self, frames: &[f32]) -> Result<usize, PortError> {
                Ok(frames.len() / self.format.channels.max(1) as usize)
            }
            fn format(&self) -> Format {
                self.format
            }
            fn period_frames(&self) -> usize {
                self.period_frames
            }
        }
        // 480 frames @ 48kHz = 10ms device period.
        let renders: Vec<(OutputId, Box<dyn RenderPort>)> = vec![(
            OutputId(0),
            Box::new(FixedPeriodRender { period_frames: 480, format: stereo(48_000) }),
        )];

        let wake_unit = compute_wake_unit_period(&renders);

        let expected = Duration::from_secs_f64(0.010 * WAKE_MARGIN);
        assert!(
            (wake_unit.as_secs_f64() - expected.as_secs_f64()).abs() < 1e-9,
            "got {wake_unit:?}"
        );
        assert!(
            wake_unit > Duration::from_millis(10),
            "must apply WAKE_MARGIN over the full period, not the old half-period split, got {wake_unit:?}"
        );
    }

    #[test]
    fn render_loop_wakes_the_mixer_after_draining_its_ring() {
        // Succeeds once (drains + wakes + writes), then fails so the loop
        // exits deterministically without needing a second thread to flip `stop`.
        struct OneShotRender {
            called: bool,
        }
        impl RenderPort for OneShotRender {
            fn wait_event(&mut self, _timeout: Duration) -> Result<(), PortError> {
                if self.called {
                    Err(PortError::DeviceInvalidated)
                } else {
                    self.called = true;
                    Ok(())
                }
            }
            fn free_frames(&self) -> Result<usize, PortError> {
                Ok(usize::MAX)
            }
            fn write(&mut self, frames: &[f32]) -> Result<usize, PortError> {
                Ok(frames.len() / self.format().channels.max(1) as usize)
            }
            fn format(&self) -> Format {
                stereo(48_000)
            }
            fn period_frames(&self) -> usize {
                2
            }
        }
        let stop = AtomicBool::new(false);
        let xruns = AtomicU64::new(0);
        let shortfall = AtomicU64::new(0);
        let (fault_tx, _fault_rx) = mpsc::channel();
        let sys = MockSystem::new(vec![]);
        let (mut producer, consumer) = RingBuffer::<f32>::new(4);
        producer.push(0.25).unwrap();
        let ctx = RenderFaultCtx {
            xruns: &xruns,
            shortfall: &shortfall,
            output_id: OutputId(0),
            faults: &fault_tx,
        };

        // Bounded, race-free: `unpark`'s token persists until the next
        // `park`/`park_timeout` consumes it, so it doesn't matter whether
        // `render_loop`'s wake() lands before or after this thread reaches
        // its own park call.
        let parker = thread::spawn(|| {
            let start = std::time::Instant::now();
            thread::park_timeout(Duration::from_secs(2));
            start.elapsed()
        });
        let waker = MixerWaker(parker.thread().clone());

        render_loop(Box::new(OneShotRender { called: false }), consumer, &stop, &ctx, &sys, waker);

        let elapsed = parker.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(500),
            "render_loop must wake the mixer promptly after draining its ring, got {elapsed:?}"
        );
    }

    #[test]
    fn pid_capture_loop_wakes_the_mixer_after_producing_a_block() {
        struct OneShotCapture {
            called: bool,
        }
        impl CapturePort for OneShotCapture {
            fn read(&mut self, buf: &mut [f32]) -> Result<usize, PortError> {
                if self.called {
                    Err(PortError::DeviceInvalidated)
                } else {
                    self.called = true;
                    buf[0] = 0.1;
                    Ok(1)
                }
            }
            fn format(&self) -> Format {
                stereo(48_000)
            }
            fn poll_interval(&self) -> Duration {
                Duration::from_millis(1)
            }
        }
        let stop = AtomicBool::new(false);
        let sys = MockSystem::new(vec![]);
        let (producer, _consumer) = RingBuffer::<f32>::new(8);

        let parker = thread::spawn(|| {
            let start = std::time::Instant::now();
            thread::park_timeout(Duration::from_secs(2));
            start.elapsed()
        });
        let waker = MixerWaker(parker.thread().clone());
        let drops = AtomicU64::new(0);

        pid_capture_loop(
            Box::new(OneShotCapture { called: false }),
            producer,
            &stop,
            &sys,
            waker,
            CaptureGauges {
                drops: &drops,
                fill: &AtomicU32::new(0),
                pushed: &AtomicU64::new(0),
            },
        );

        let elapsed = parker.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(500),
            "pid_capture_loop must wake the mixer after producing a block, got {elapsed:?}"
        );
    }

    #[test]
    fn apply_params_wakes_the_mixer_after_enqueueing_commands() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        let ratio = audio_core::ResampleRatio::new(1.003).unwrap();

        handle
            .apply_params(vec![MixerCommand::SetOutputRatio(OutputId(0), ratio)])
            .unwrap();

        // Well under MIXER_FALLBACK_INTERVAL (100ms) — only passes if
        // apply_params's own wake() call fired the tick, not the fallback.
        sleep(Duration::from_millis(20));
        assert_eq!(handle.stats().applied_ratio, vec![(OutputId(0), 1.003)]);

        handle.shutdown().unwrap();
    }

    #[test]
    fn mixer_loop_ticks_immediately_on_first_run_before_any_wake() {
        // Nothing in this test ever calls `waker.wake()` or waits past a
        // handful of milliseconds — the only way `ring_fill[0].active` can
        // become true is `mixer_loop` running its first tick unconditionally
        // before ever parking (Flow A). Observing via `RingGauge.active`
        // (set by `pull_group_inputs`'/`flush_outputs`' own bookkeeping) not
        // actual output audio: a real `Mixer` always resamples through a real
        // `Src`, which needs several ticks' worth of input before its sinc
        // filter emits anything — a pipeline-latency property unrelated to
        // whether the mixer ticked, so it's the wrong signal to assert on here.
        let in_capacity = 8; // == max_block_frames below: a full block this tick
        let (mut in_p, in_c) = RingBuffer::<f32>::new(in_capacity);
        for _ in 0..in_capacity {
            in_p.push(0.5).unwrap();
        }
        let mut group_consumers = vec![slot(GroupId(0), 1, 0)];
        group_consumers[0].pids.push((1, in_c));
        let (out_p, _out_c) = RingBuffer::<f32>::new(64);
        let output_producers = vec![(OutputId(0), out_p, 1usize)];

        let topology = Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![audio_core::GroupSpec {
                id: GroupId(0),
                gain: audio_core::Gain::UNITY,
                follow_master: false,
                output: OutputId(0),
                input_format: mono(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                mute: false,
            }],
            outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) }],
        };
        let mixer = Mixer::new(&topology, 8).unwrap();
        let (_capture_tx, capture_rx) = mpsc::channel();
        let mut output_index_of = HashMap::new();
        output_index_of.insert(OutputId(0), 0);
        let stop = Arc::new(AtomicBool::new(false));
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(vec![]));
        let ring_fill = Arc::new(vec![RingGauge {
            fill_permille: AtomicU32::new(0),
            active: AtomicBool::new(false),
        }]);

        let args = MixerThreadArgs {
            max_block_frames: 8,
            persistent: bare_persistent(&snapshot()),
            ring_fill: Arc::clone(&ring_fill),
            applied_ratio: Arc::new(vec![AtomicU64::new(1.0f64.to_bits())]),
            output_index_of,
            stop: Arc::clone(&stop),
            sys,
            duck_depth_db: Arc::new(vec![AtomicU32::new(0)]),
            limiter_engaged: Arc::new(vec![AtomicU64::new(0)]),
            group_peak: Arc::new(vec![AtomicU64::new(0)]),
            output_peak: Arc::new(vec![AtomicU64::new(0)]),
            capture_rx,
            group_input_rates: vec![48_000],
            output_rates: vec![48_000],
            output_drops: Arc::new(AtomicU64::new(0)),
            reject_diag: Arc::new(RejectDiag::default()),
        };
        let handle = thread::spawn(move || mixer_loop(mixer, group_consumers, output_producers, args));

        sleep(Duration::from_millis(30)); // comfortably before MIXER_FALLBACK_INTERVAL
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        assert!(
            ring_fill[0].active.load(Ordering::Relaxed),
            "the pre-loaded full block must have registered as real activity on the first tick"
        );
    }

    #[test]
    fn mixer_loop_drains_everything_pending_regardless_of_which_source_woke_it() {
        // A capture add for two pids AND a queued command are both pending
        // before the thread ever starts; only the guaranteed first tick (no
        // explicit wake) runs before assertions — proving one tick drains
        // every pending source together, not just the one that "woke" it.
        // Each pid fills a full max_block_frames block (not just one sample —
        // `real_this_tick`/`RingGauge.active` only trips on a fully-filled
        // block, and that's the observable used below, not raw output audio:
        // a real `Mixer` always resamples through a real `Src`, which needs
        // several ticks' worth of input before its sinc filter emits
        // anything, unrelated to whether both pids were actually drained).
        let block = 8; // == max_block_frames below
        let (mut p1, c1) = RingBuffer::<f32>::new(block);
        let (mut p2, c2) = RingBuffer::<f32>::new(block);
        for _ in 0..block {
            p1.push(0.2).unwrap();
            p2.push(0.5).unwrap();
        }
        let (capture_tx, capture_rx) = mpsc::channel();
        capture_tx.send(CaptureMsg::Add { group: GroupId(0), pid: 1, consumer: c1 }).unwrap();
        capture_tx.send(CaptureMsg::Add { group: GroupId(0), pid: 2, consumer: c2 }).unwrap();

        let persistent = bare_persistent(&snapshot());
        let ratio = audio_core::ResampleRatio::new(1.05).unwrap();
        let epoch = Epoch(persistent.epoch.load(Ordering::Relaxed));
        let pushed = persistent
            .commands
            .push(Envelope { epoch, cmd: MixerCommand::SetOutputRatio(OutputId(0), ratio) });
        assert!(pushed.is_ok());

        let group_consumers = vec![slot(GroupId(0), 1, 0)]; // starts with zero pids -- the Add messages populate it mid-tick
        let (out_p, _out_c) = RingBuffer::<f32>::new(64);
        let output_producers = vec![(OutputId(0), out_p, 1usize)];
        let topology = Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![audio_core::GroupSpec {
                id: GroupId(0),
                gain: audio_core::Gain::UNITY,
                follow_master: false,
                output: OutputId(0),
                input_format: mono(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                mute: false,
            }],
            outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) }],
        };
        let mixer = Mixer::new(&topology, 8).unwrap();
        let ring_fill = Arc::new(vec![RingGauge {
            fill_permille: AtomicU32::new(0),
            active: AtomicBool::new(false),
        }]);
        let mut output_index_of = HashMap::new();
        output_index_of.insert(OutputId(0), 0);
        let stop = Arc::new(AtomicBool::new(false));
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(vec![]));
        let applied_ratio = Arc::new(vec![AtomicU64::new(1.0f64.to_bits())]);

        let args = MixerThreadArgs {
            max_block_frames: 8,
            persistent: Arc::clone(&persistent),
            ring_fill: Arc::clone(&ring_fill),
            applied_ratio: Arc::clone(&applied_ratio),
            output_index_of,
            stop: Arc::clone(&stop),
            sys,
            duck_depth_db: Arc::new(vec![AtomicU32::new(0)]),
            limiter_engaged: Arc::new(vec![AtomicU64::new(0)]),
            group_peak: Arc::new(vec![AtomicU64::new(0)]),
            output_peak: Arc::new(vec![AtomicU64::new(0)]),
            capture_rx,
            group_input_rates: vec![48_000],
            output_rates: vec![48_000],
            output_drops: Arc::new(AtomicU64::new(0)),
            reject_diag: Arc::new(RejectDiag::default()),
        };
        let handle = thread::spawn(move || mixer_loop(mixer, group_consumers, output_producers, args));

        sleep(Duration::from_millis(30)); // comfortably before MIXER_FALLBACK_INTERVAL
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        assert!(
            ring_fill[0].active.load(Ordering::Relaxed),
            "both pids' full blocks must have been drained together on the first tick"
        );
        let applied = f64::from_bits(applied_ratio[0].load(Ordering::Relaxed));
        assert_eq!(applied, 1.05, "the queued command must also have applied in that same tick");
    }

    #[test]
    fn stopping_a_parked_mixer_joins_promptly_not_after_the_fallback_interval() {
        // Regression (review finding, 2026-07-24): `stop_running_graph` must
        // wake the mixer right after setting `stop`, or shutdown/rebuild
        // silently waits up to MIXER_FALLBACK_INTERVAL to notice — nothing
        // else in this harness (no render/capture threads at all, an empty
        // topology) would ever wake it otherwise.
        let empty_topology =
            Topology { master: audio_core::Gain::UNITY, groups: Vec::new(), outputs: Vec::new() };
        let mixer = Mixer::new(&empty_topology, 8).unwrap();
        let group_consumers: GroupConsumers = Vec::new();
        let output_producers: OutputProducers = Vec::new();
        let (_capture_tx, capture_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(vec![]));

        let args = MixerThreadArgs {
            max_block_frames: 8,
            persistent: bare_persistent(&snapshot()),
            ring_fill: Arc::new(Vec::new()),
            applied_ratio: Arc::new(Vec::new()),
            output_index_of: HashMap::new(),
            stop: Arc::clone(&stop),
            sys,
            duck_depth_db: Arc::new(Vec::new()),
            limiter_engaged: Arc::new(Vec::new()),
            group_peak: Arc::new(Vec::new()),
            output_peak: Arc::new(Vec::new()),
            capture_rx,
            group_input_rates: Vec::new(),
            output_rates: Vec::new(),
            output_drops: Arc::new(AtomicU64::new(0)),
            reject_diag: Arc::new(RejectDiag::default()),
        };
        let handle = thread::spawn(move || mixer_loop(mixer, group_consumers, output_producers, args));
        let waker = MixerWaker(handle.thread().clone());
        sleep(Duration::from_millis(10)); // let it complete its first tick and park

        let start = std::time::Instant::now();
        stop.store(true, Ordering::Relaxed);
        waker.wake(); // the fix under test: stop_running_graph now does this too
        handle.join().unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < MIXER_FALLBACK_INTERVAL / 2,
            "stopping a parked mixer must join promptly via wake(), not wait for the fallback interval, got {elapsed:?}"
        );
    }

    // --- Recovery supervisor integration tests. These run the real
    // background supervisor thread (started by `start()`) and drive it via
    // `MockSystem`'s device-event/enumerate hooks, so they wait on the
    // supervisor's actual ~100ms tick cadence via `recv_timeout` rather than
    // calling its internal functions directly.

    #[test]
    fn device_removal_falls_back_to_default_output_and_keeps_engine_running() {
        let sys = Arc::new(MockSystem::new(two_output_endpoints()));
        sys.seed_default_output(EndpointId("out-2".into()));
        let mut handle = start(&snapshot(), Arc::clone(&sys) as Arc<dyn AudioSystem>).unwrap();
        let events = handle.take_events();
        // Supervisor subscribes to device events at the top of its loop, just
        // after spawn — give it a moment before emitting, or the event has
        // nowhere to land yet (MockSystem drops emits with no subscriber).
        sleep(Duration::from_millis(50));

        sys.remove_endpoint(&EndpointId("out-1".into()));
        sys.emit_device_event(DeviceEvent::Removed(EndpointId("out-1".into())));

        // `Removed` always fires DeviceRemoved first (the device list changed,
        // independent of recovery), mirroring `Added`/DeviceAvailable below.
        let removed = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DeviceRemoved event");
        assert!(matches!(removed, EngineEvent::DeviceRemoved(_)));

        let evt = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a FallbackApplied event");
        match evt {
            EngineEvent::FallbackApplied { groups, from, to } => {
                assert_eq!(groups, vec![GroupId(0)]);
                assert_eq!(from, EndpointId("out-1".into()));
                assert_eq!(to, EndpointId("out-2".into()));
            }
            other => panic!("expected FallbackApplied, got {other:?}"),
        }

        // Engine is still alive against the rebuilt graph, not stopped.
        handle.apply_params(vec![]).unwrap();
        assert_eq!(handle.stats().ring_fill.len(), 1);
        handle.shutdown().unwrap();
    }

    #[test]
    fn device_removal_with_no_fallback_parks_the_group_and_keeps_engine_running() {
        let sys = Arc::new(MockSystem::new(mock_endpoints())); // only one physical endpoint
        let mut handle = start(&snapshot(), Arc::clone(&sys) as Arc<dyn AudioSystem>).unwrap();
        let events = handle.take_events();
        // Supervisor subscribes to device events at the top of its loop, just
        // after spawn — give it a moment before emitting, or the event has
        // nowhere to land yet (MockSystem drops emits with no subscriber).
        sleep(Duration::from_millis(50));

        sys.remove_endpoint(&EndpointId("out-1".into()));
        sys.emit_device_event(DeviceEvent::Removed(EndpointId("out-1".into())));

        let removed = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DeviceRemoved event");
        assert!(matches!(removed, EngineEvent::DeviceRemoved(_)));

        let evt = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DeviceLost event");
        assert!(matches!(evt, EngineEvent::DeviceLost { groups } if groups == vec![GroupId(0)]));

        // Engine survives with a degenerate empty graph (L1 capability 2:
        // "removing an in-use output device never kills the engine").
        handle.apply_params(vec![]).unwrap();
        assert!(handle.stats().ring_fill.is_empty());
        handle.shutdown().unwrap();
    }

    /// double-audio-prevention flow F: the sink endpoint is deliberately never
    /// a group output, so its removal reaches neither `handle_endpoint_lost`'s
    /// fallback path nor `DeviceLost`. Without an unconditional announcement
    /// the app layer would never learn the sink vanished.
    #[test]
    fn removing_an_endpoint_no_group_uses_still_announces_it() {
        let sys = Arc::new(MockSystem::new(two_output_endpoints()));
        let mut handle = start(&snapshot(), Arc::clone(&sys) as Arc<dyn AudioSystem>).unwrap();
        let events = handle.take_events();
        sleep(Duration::from_millis(50));

        // out-2 is present but no group renders to it — the sink's situation.
        sys.remove_endpoint(&EndpointId("out-2".into()));
        sys.emit_device_event(DeviceEvent::Removed(EndpointId("out-2".into())));

        let evt = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DeviceRemoved event");
        assert!(matches!(evt, EngineEvent::DeviceRemoved(id) if id == EndpointId("out-2".into())));

        handle.shutdown().unwrap();
    }

    #[test]
    fn device_returning_restores_a_parked_group() {
        let sys = Arc::new(MockSystem::new(mock_endpoints()));
        let mut handle = start(&snapshot(), Arc::clone(&sys) as Arc<dyn AudioSystem>).unwrap();
        let events = handle.take_events();
        // Supervisor subscribes to device events at the top of its loop, just
        // after spawn — give it a moment before emitting, or the event has
        // nowhere to land yet (MockSystem drops emits with no subscriber).
        sleep(Duration::from_millis(50));

        sys.remove_endpoint(&EndpointId("out-1".into()));
        sys.emit_device_event(DeviceEvent::Removed(EndpointId("out-1".into())));
        events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DeviceRemoved event");
        events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DeviceLost event");

        let speakers = Endpoint {
            id: EndpointId("out-1".into()),
            name: "Speakers".into(),
            format: stereo(48_000),
        };
        sys.add_endpoint(speakers.clone());
        sys.emit_device_event(DeviceEvent::Added(speakers));

        // `Added` always fires DeviceAvailable first (L1 capability 3 — new
        // selectable target, independent of recovery), then Recovered since
        // this device also happens to restore a parked group.
        let available = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DeviceAvailable event");
        assert!(matches!(available, EngineEvent::DeviceAvailable(_)));

        let evt = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a Recovered event");
        match evt {
            EngineEvent::Recovered { groups, on } => {
                assert_eq!(groups, vec![GroupId(0)]);
                assert_eq!(on, EndpointId("out-1".into()));
            }
            other => panic!("expected Recovered, got {other:?}"),
        }

        handle.apply_params(vec![]).unwrap();
        assert_eq!(handle.stats().ring_fill.len(), 1);
        handle.shutdown().unwrap();
    }

    #[test]
    fn unrelated_device_added_emits_device_available_without_recovering_anything() {
        let sys = Arc::new(MockSystem::new(mock_endpoints()));
        let mut handle = start(&snapshot(), Arc::clone(&sys) as Arc<dyn AudioSystem>).unwrap();
        let events = handle.take_events();
        sleep(Duration::from_millis(50));

        let mic = Endpoint {
            id: EndpointId("mic-1".into()),
            name: "USB Mic".into(),
            format: stereo(48_000),
        };
        sys.emit_device_event(DeviceEvent::Added(mic.clone()));

        let evt = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DeviceAvailable event");
        match evt {
            EngineEvent::DeviceAvailable(endpoint) => assert_eq!(endpoint.id, mic.id),
            other => panic!("expected DeviceAvailable, got {other:?}"),
        }
        // Nothing was overridden, so no rebuild/Recovered follows.
        assert!(events.recv_timeout(Duration::from_millis(200)).is_err());

        handle.shutdown().unwrap();
    }

    #[test]
    fn duplicate_device_added_in_one_tick_emits_device_available_once() {
        // Two `Added` events for the same endpoint land in the same
        // supervisor tick (no sleep between emits) — the same real-world
        // shape as WASAPI firing OnDeviceAdded more than once for one
        // physical device. Must dedup to a single DeviceAvailable, not two.
        let sys = Arc::new(MockSystem::new(mock_endpoints()));
        let mut handle = start(&snapshot(), Arc::clone(&sys) as Arc<dyn AudioSystem>).unwrap();
        let events = handle.take_events();
        sleep(Duration::from_millis(50));

        let mic = Endpoint {
            id: EndpointId("mic-1".into()),
            name: "USB Mic".into(),
            format: stereo(48_000),
        };
        sys.emit_device_event(DeviceEvent::Added(mic.clone()));
        sys.emit_device_event(DeviceEvent::Added(mic.clone()));

        let evt = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DeviceAvailable event");
        assert!(matches!(evt, EngineEvent::DeviceAvailable(e) if e.id == mic.id));
        // The duplicate must not produce a second DeviceAvailable.
        assert!(events.recv_timeout(Duration::from_millis(300)).is_err());

        handle.shutdown().unwrap();
    }

    #[test]
    fn default_device_changed_forwards_as_an_engine_event() {
        // external-controls.md flow E: the volume-bind coordinator can't
        // subscribe to device events itself (a second `subscribe_device_events`
        // call would replace the recovery supervisor's own registration), so
        // it reacts to this forwarded event instead. Regression for that
        // forwarding actually happening rather than being silently dropped
        // the way `StateChanged` still is.
        let sys = Arc::new(MockSystem::new(mock_endpoints()));
        let mut handle = start(&snapshot(), Arc::clone(&sys) as Arc<dyn AudioSystem>).unwrap();
        let events = handle.take_events();
        sleep(Duration::from_millis(50));

        sys.emit_device_event(DeviceEvent::DefaultChanged(EndpointId("out-2".into())));

        let evt = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a DefaultDeviceChanged event");
        assert!(matches!(evt, EngineEvent::DefaultDeviceChanged(id) if id == EndpointId("out-2".into())));

        handle.shutdown().unwrap();
    }

    #[test]
    fn format_change_reopens_the_same_endpoint_instead_of_falling_back() {
        // The device never leaves `enumerate()` — only the live stream
        // faults — so this exercises the still-present branch of
        // `handle_endpoint_lost` (interaction D), not the removal branch.
        let sys = Arc::new(MockSystem::new(mock_endpoints()));
        let mut handle = start(&snapshot(), Arc::clone(&sys) as Arc<dyn AudioSystem>).unwrap();
        let events = handle.take_events();

        sys.invalidate_render(&EndpointId("out-1".into()));

        let evt = events
            .recv_timeout(Duration::from_millis(1000))
            .expect("expected a Recovered event");
        match evt {
            EngineEvent::Recovered { groups, on } => {
                assert_eq!(groups, vec![GroupId(0)]);
                assert_eq!(on, EndpointId("out-1".into()));
            }
            other => panic!("expected Recovered, got {other:?}"),
        }

        handle.apply_params(vec![]).unwrap();
        assert_eq!(handle.stats().ring_fill.len(), 1);
        handle.shutdown().unwrap();
    }

    fn slot(group_id: GroupId, channels: usize, output_index: usize) -> GroupSlot {
        GroupSlot {
            group_id,
            pids: Vec::new(),
            channels,
            output_index,
        }
    }

    #[test]
    fn pull_group_inputs_sums_two_pids_captured_into_the_same_group() {
        let (mut p1, c1) = RingBuffer::<f32>::new(16);
        let (mut p2, c2) = RingBuffer::<f32>::new(16);
        p1.push(0.2).unwrap();
        p1.push(0.3).unwrap();
        p2.push(0.5).unwrap();
        p2.push(-0.1).unwrap();

        let mut consumers = vec![slot(GroupId(0), 1, 0)];
        consumers[0].pids.push((100, c1));
        consumers[0].pids.push((200, c2));
        let mut scratch = vec![vec![0.0f32; 2]];
        let mut real_this_tick = vec![false];

        // A real `Mixer` isn't needed to observe `push_group`'s input —
        // exercised for real via `apply_capture_sources_wires_pids_into_the_running_mixer`
        // below; this test isolates the pure summing arithmetic in
        // `pull_group_inputs` itself with a minimal real `Mixer`.
        let topology = Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![audio_core::GroupSpec {
                id: GroupId(0),
                gain: audio_core::Gain::UNITY,
                follow_master: false,
                output: OutputId(0),
                input_format: mono(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                mute: false,
            }],
            outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) }],
        };
        let mut mixer = Mixer::new(&topology, 8).unwrap();
        let headroom = vec![OutputHeadroom { filled_frames: 0, capacity_frames: 1000 }];
        let block_out_frames = vec![2];

        pull_group_inputs(
            &mut consumers,
            &mut scratch,
            &mut mixer,
            &mut real_this_tick,
            &headroom,
            &block_out_frames,
        );

        assert!(real_this_tick[0], "both pids fully filled this tick");
        // The two pids' contributions are summed sample-by-sample before
        // reaching the mixer: 0.2+0.5, 0.3+(-0.1).
        assert!((scratch[0][0] - 0.7).abs() < 1e-6, "got {}", scratch[0][0]);
        assert!((scratch[0][1] - 0.2).abs() < 1e-6, "got {}", scratch[0][1]);
    }

    fn group_spec(id: GroupId, output: OutputId) -> audio_core::GroupSpec {
        audio_core::GroupSpec {
            id,
            gain: audio_core::Gain::UNITY,
            follow_master: false,
            output,
            input_format: mono(48_000),
            dsp: Vec::new(),
            duck: None,
            spatial: false,
            mute: false,
        }
    }

    #[test]
    fn a_group_that_loses_its_last_pid_stops_gating_its_outputs_span() {
        // MT17, 2026-07-27: unassigning a group's only app silenced the whole
        // output permanently. Its resampler kept a partial chunk that nothing
        // could ever complete, and `mix_tick` held the shared span at zero
        // waiting for it. `pull_group_inputs` fires the discard the moment a
        // slot has no pids, which is the signal that no input is coming.
        // Block sized like the real graph, not a toy: below the resampler's
        // sinc length the first chunks produce no output at all, which would
        // make this test silent for reasons that have nothing to do with the
        // span rule.
        let block = 304;
        let topology = Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![
                group_spec(GroupId(0), OutputId(0)),
                group_spec(GroupId(1), OutputId(0)),
            ],
            outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) }],
        };
        let mut mixer = Mixer::new(&topology, block).unwrap();

        // Group 1 takes in half a chunk — enough to be "in flight", never
        // enough to complete — then loses its pid.
        mixer.push_group(GroupId(1), &vec![0.5f32; block / 2]);
        mixer.mix_tick();
        let mut drain = vec![0.0f32; mixer.output_capacity(OutputId(0))];
        mixer.take_output(OutputId(0), &mut drain);

        let mut consumers = vec![slot(GroupId(0), 1, 0), slot(GroupId(1), 1, 0)];
        let (mut feed, taps) = rtrb::RingBuffer::<f32>::new(block * 8);
        consumers[0].pids.push((1, taps));
        let mut scratch = vec![vec![0.0f32; block], vec![0.0f32; block]];
        let mut real_this_tick = vec![false];
        let headroom = vec![OutputHeadroom { filled_frames: 0, capacity_frames: 100_000 }];
        let block_out_frames = vec![block, block];

        let mut per_tick = Vec::new();
        for _ in 0..6 {
            for _ in 0..block {
                feed.push(0.5).unwrap();
            }
            pull_group_inputs(
                &mut consumers,
                &mut scratch,
                &mut mixer,
                &mut real_this_tick,
                &headroom,
                &block_out_frames,
            );
            mixer.mix_tick();
            let mut out = vec![0.0f32; mixer.output_capacity(OutputId(0))];
            per_tick.push(mixer.take_output(OutputId(0), &mut out));
        }

        // First tick, not merely "eventually": `mix_tick`'s parking-capacity
        // backstop also frees the span, but only once the live group has
        // filled its parking — an audible gap, and a block of its input lost
        // to a group that will never produce again. The discard is what makes
        // the unassign silent to everyone else.
        assert!(
            per_tick[0] > 0,
            "the output stalled on the tick the pid went away: {per_tick:?} — a group \
             with no pids left is still holding its output's span at zero"
        );
    }

    #[test]
    fn pull_group_inputs_on_a_group_with_zero_pids_produces_silence() {
        let mut consumers = vec![slot(GroupId(0), 1, 0)];
        let mut scratch = vec![vec![1.0f32; 4]]; // pre-filled with garbage to prove it gets zeroed
        let mut real_this_tick = vec![true];
        let topology = Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![audio_core::GroupSpec {
                id: GroupId(0),
                gain: audio_core::Gain::UNITY,
                follow_master: false,
                output: OutputId(0),
                input_format: mono(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                mute: false,
            }],
            outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) }],
        };
        let mut mixer = Mixer::new(&topology, 8).unwrap();
        let headroom = vec![OutputHeadroom { filled_frames: 0, capacity_frames: 1000 }];
        let block_out_frames = vec![4];

        pull_group_inputs(
            &mut consumers,
            &mut scratch,
            &mut mixer,
            &mut real_this_tick,
            &headroom,
            &block_out_frames,
        );

        assert!(!real_this_tick[0], "no pids -> not active");
        // Scratch itself is zeroed either way — what makes this B4's boundary
        // case is that `filled_max` is 0, so `push_group` receives a genuinely
        // empty slice rather than the whole zero-padded buffer. The slice
        // length is asserted directly (via its downstream effect on the SRC)
        // by `pull_group_inputs_pushes_only_the_frames_it_popped`.
        assert_eq!(scratch[0], vec![0.0; 4]);
    }

    /// One mono group on one output, `block`-frame blocks — the shape both
    /// B4 tests below need. Kept local to them so neither has to carry a
    /// 20-line inline `Topology` the assertions don't depend on.
    fn one_mono_group_topology() -> Topology {
        Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![audio_core::GroupSpec {
                id: GroupId(0),
                gain: audio_core::Gain::UNITY,
                follow_master: false,
                output: OutputId(0),
                input_format: mono(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                mute: false,
            }],
            outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) }],
        }
    }

    /// `block` is a realistic `max_block_frames`, not a token 8: `Src` wraps a
    /// 64-tap sinc (`resample.rs`'s `SINC_LEN`), so at a block of 8 the
    /// resampler spends dozens of chunks filling its delay line and emits
    /// nothing — which would make both B4 tests below pass vacuously.
    const B4_BLOCK: usize = 512;

    /// Pulls one tick through `pull_group_inputs` -> `mix_tick` ->
    /// `take_output` with the governor deliberately wide open (this is a test
    /// about slice lengths, not about the skip decision), and returns the
    /// samples that reached the output.
    fn b4_tick(
        mixer: &mut Mixer,
        consumers: &mut [GroupSlot],
        scratch: &mut [Vec<f32>],
        out_buf: &mut [f32],
    ) -> usize {
        let headroom = [OutputHeadroom { filled_frames: 0, capacity_frames: usize::MAX / 2 }];
        let block_out_frames = [B4_BLOCK];
        let mut real_this_tick = [false];
        pull_group_inputs(consumers, scratch, mixer, &mut real_this_tick, &headroom, &block_out_frames);
        mixer.mix_tick();
        mixer.take_output(OutputId(0), out_buf)
    }

    #[test]
    fn pull_group_inputs_pushes_only_the_frames_it_popped() {
        // B4. `push_group`'s slice length isn't observable directly
        // (`valid_len` is private), so this asserts it through the one thing
        // that depends on it: `Src` consumes a FIXED `chunk_in` ==
        // max_block_frames, buffering anything short of that until a later
        // call completes the chunk. Feed half a block and a correct
        // implementation must produce NOTHING that tick; the pre-fix code,
        // which pushed the whole zero-padded scratch, handed over a full chunk
        // and emitted immediately.
        let half = B4_BLOCK / 2;
        let (mut pid_producer, pid_consumer) = RingBuffer::<f32>::new(B4_BLOCK * 4);
        let mut consumers = vec![slot(GroupId(0), 1, 0)];
        consumers[0].pids.push((1, pid_consumer));
        let mut scratch = vec![vec![0.0f32; B4_BLOCK]];
        let mut mixer = Mixer::new(&one_mono_group_topology(), B4_BLOCK).unwrap();
        let mut out_buf = vec![0.0f32; B4_BLOCK * 2];

        // Warm the sinc delay line first, with whole blocks, so the partial
        // assertions below are about the slice length and not about resampler
        // start-up. A whole block exactly fills `chunk_in`, so this leaves no
        // partial input buffered.
        let mut warmed = false;
        for _ in 0..8 {
            for _ in 0..B4_BLOCK {
                pid_producer.push(0.5).unwrap();
            }
            if b4_tick(&mut mixer, &mut consumers, &mut scratch, &mut out_buf) > 0 {
                warmed = true;
                break;
            }
        }
        assert!(warmed, "resampler never produced output on whole blocks — test setup is wrong");

        for _ in 0..half {
            pid_producer.push(0.5).unwrap();
        }
        assert_eq!(
            b4_tick(&mut mixer, &mut consumers, &mut scratch, &mut out_buf),
            0,
            "half a block of real audio must be handed over as half a block — the SRC \
             then buffers it, producing nothing. Non-zero here means the unfilled tail \
             was zero-padded into the stream (B4)."
        );

        for _ in 0..half {
            pid_producer.push(0.5).unwrap();
        }
        assert!(
            b4_tick(&mut mixer, &mut consumers, &mut scratch, &mut out_buf) > 0,
            "the two half-blocks must accumulate into one full chunk and emit, proving \
             the first half was pushed rather than discarded"
        );
    }

    #[test]
    fn a_partially_filled_group_never_zero_pads_the_stream() {
        // B4's "never fabricate frames to fill a gap", as conservation: feed
        // only ever half a block per tick, and no more frames may come out
        // than went in. The pre-fix code pushed a full zero-padded block every
        // tick, which would roughly DOUBLE the output frame count here.
        let ticks = 40;
        let per_tick = B4_BLOCK / 2;
        let (mut pid_producer, pid_consumer) = RingBuffer::<f32>::new(B4_BLOCK * 4);
        let mut consumers = vec![slot(GroupId(0), 1, 0)];
        consumers[0].pids.push((1, pid_consumer));
        let mut scratch = vec![vec![0.0f32; B4_BLOCK]];
        let mut mixer = Mixer::new(&one_mono_group_topology(), B4_BLOCK).unwrap();
        let mut out_buf = vec![0.0f32; B4_BLOCK * 2];

        let mut total_out = 0usize;
        for _ in 0..ticks {
            for _ in 0..per_tick {
                pid_producer.push(0.5).unwrap();
            }
            total_out += b4_tick(&mut mixer, &mut consumers, &mut scratch, &mut out_buf);
        }

        let total_in = ticks * per_tick;
        assert!(
            total_out <= total_in,
            "no frame may leave the mixer that didn't enter it — {total_out} out vs \
             {total_in} in means the unfilled block tail was fabricated (B4)"
        );
        // Lower bound so this can't pass by producing nothing at all. Loose on
        // purpose: the shortfall is the sinc delay line's start-up plus one
        // in-flight chunk, not loss. The decisive assertion is the upper bound.
        assert!(
            total_out > total_in / 2,
            "expected the real audio to actually flow through, got {total_out} of {total_in}"
        );
    }

    #[test]
    fn ring_gauge_active_is_true_when_a_group_received_real_audio() {
        // B8: the old code required a FULLY-filled block to mark activity —
        // under the governor (and ordinary capture jitter) that can become
        // permanently unreachable. A partial pop must still count.
        let (mut p1, c1) = RingBuffer::<f32>::new(16);
        p1.push(0.5).unwrap(); // far short of a full 8-frame block
        let mut consumers = vec![slot(GroupId(0), 1, 0)];
        consumers[0].pids.push((1, c1));
        let mut scratch = vec![vec![0.0f32; 8]];
        let mut real_this_tick = vec![false];
        let topology = Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![audio_core::GroupSpec {
                id: GroupId(0),
                gain: audio_core::Gain::UNITY,
                follow_master: false,
                output: OutputId(0),
                input_format: mono(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                mute: false,
            }],
            outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) }],
        };
        let mut mixer = Mixer::new(&topology, 8).unwrap();
        let headroom = vec![OutputHeadroom { filled_frames: 0, capacity_frames: 1000 }];
        let block_out_frames = vec![8];

        pull_group_inputs(
            &mut consumers,
            &mut scratch,
            &mut mixer,
            &mut real_this_tick,
            &headroom,
            &block_out_frames,
        );

        assert!(real_this_tick[0], "a partial pop is still real audio (B8) — must not require a full block");
    }

    #[test]
    fn group_may_push_skips_when_its_output_ring_is_at_threshold() {
        let below = OutputHeadroom { filled_frames: 100, capacity_frames: 1000 }; // 10%
        let at = OutputHeadroom { filled_frames: 500, capacity_frames: 1000 }; // exactly 50%
        assert!(group_may_push(below, 50));
        assert!(
            !group_may_push(at, 50),
            "a ring already at the governor threshold must not receive another block"
        );
    }

    #[test]
    fn a_stalled_output_does_not_starve_a_healthy_one() {
        // Level 3 finding: `mix_tick` sums each group into its OWN output's
        // accumulator, so budgets are per-group/per-output — one stalled
        // output must never block a healthy one from being pulled.
        let (mut p0, c0) = RingBuffer::<f32>::new(16);
        let (mut p1, c1) = RingBuffer::<f32>::new(16);
        for _ in 0..8 {
            p0.push(0.3).unwrap();
            p1.push(0.4).unwrap();
        }
        let mut consumers = vec![slot(GroupId(0), 1, 0), slot(GroupId(1), 1, 1)];
        consumers[0].pids.push((100, c0));
        consumers[1].pids.push((200, c1));
        let mut scratch = vec![vec![0.0f32; 8], vec![0.0f32; 8]];
        let mut real_this_tick = vec![false, false];

        let topology = Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![
                audio_core::GroupSpec {
                    id: GroupId(0),
                    gain: audio_core::Gain::UNITY,
                    follow_master: false,
                    output: OutputId(0),
                    input_format: mono(48_000),
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                    mute: false,
                },
                audio_core::GroupSpec {
                    id: GroupId(1),
                    gain: audio_core::Gain::UNITY,
                    follow_master: false,
                    output: OutputId(1),
                    input_format: mono(48_000),
                    dsp: Vec::new(),
                    duck: None,
                    spatial: false,
                    mute: false,
                },
            ],
            outputs: vec![
                audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) },
                audio_core::OutputSpec { id: OutputId(1), format: mono(48_000) },
            ],
        };
        let mut mixer = Mixer::new(&topology, 8).unwrap();

        // Output 0 is stalled (at the governor threshold); output 1 is empty.
        let headroom = vec![
            OutputHeadroom { filled_frames: 500, capacity_frames: 1000 },
            OutputHeadroom { filled_frames: 0, capacity_frames: 1000 },
        ];
        let block_out_frames = vec![8, 8];

        pull_group_inputs(
            &mut consumers,
            &mut scratch,
            &mut mixer,
            &mut real_this_tick,
            &headroom,
            &block_out_frames,
        );

        assert!(!real_this_tick[0], "stalled output's group must be skipped");
        assert!(real_this_tick[1], "a stalled output must not prevent a healthy one from being pulled");
    }

    #[test]
    fn extra_wakes_do_not_over_produce() {
        // B2's core regression: many ticks in a row (simulating repeated
        // spurious wakes with nothing draining the output) must never push
        // more into an output ring than it can hold, and must never do so
        // by silently dropping the excess — the governor stops pulling from
        // the group once the output is saturated, leaving the remainder
        // queued for a later tick instead.
        let block = 8usize; // max_block_frames
        // Far more queued than 20 ticks could drain once the governor locks
        // the output out after its very first real push (capacity below).
        let (mut in_p, in_c) = RingBuffer::<f32>::new(block * 30);
        for _ in 0..block * 30 {
            in_p.push(0.25).unwrap();
        }
        let mut consumers = vec![slot(GroupId(0), 1, 0)];
        consumers[0].pids.push((1, in_c));
        let mut group_scratch = vec![vec![0.0f32; block]];
        let mut real_this_tick = vec![false];

        let topology = Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![audio_core::GroupSpec {
                id: GroupId(0),
                gain: audio_core::Gain::UNITY,
                follow_master: false,
                output: OutputId(0),
                input_format: mono(48_000),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                mute: false,
            }],
            outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) }],
        };
        let mut mixer = Mixer::new(&topology, block).unwrap();

        // Small on purpose: the safety net (filled + block_out_frames <=
        // capacity) locks the output out after its very first non-empty
        // push, regardless of exactly how many frames the SRC's warm-up
        // produces — this is what makes the "leftover stays queued"
        // assertion deterministic rather than timing-dependent.
        let (out_p, _out_c) = RingBuffer::<f32>::new(8); // never drained by this test
        let mut output_producers = vec![(OutputId(0), out_p, 1usize)];
        let mut output_scratch = vec![vec![0.0f32; block]];
        let ring_fill = vec![RingGauge {
            fill_permille: AtomicU32::new(0),
            active: AtomicBool::new(false),
        }];
        let mut ticks_since_real = vec![ACTIVE_HOLD_TICKS];
        let drops = AtomicU64::new(0);
        let mut headroom = vec![OutputHeadroom { filled_frames: 0, capacity_frames: 0 }];
        let block_out_frames = vec![block];

        for _ in 0..20 {
            sample_output_headroom(&output_producers, &mut headroom);
            pull_group_inputs(
                &mut consumers,
                &mut group_scratch,
                &mut mixer,
                &mut real_this_tick,
                &headroom,
                &block_out_frames,
            );
            mixer.mix_tick();
            flush_outputs(
                &mut output_producers,
                &mut output_scratch,
                &mut mixer,
                FlushCtx {
                    ring_fill: &ring_fill,
                    real_this_tick: &real_this_tick,
                    ticks_since_real: &mut ticks_since_real,
                    drops: &drops,
                    reject_diag: &RejectDiag::default(),
                    budget_frames: &[],
                },
            );
        }

        assert_eq!(
            drops.load(Ordering::Relaxed),
            0,
            "the governor must stop pulling before the ring overflows, never drop silently"
        );
        assert!(
            consumers[0].pids[0].1.slots() > 0,
            "once the output ring is saturated, leftover audio must stay queued in the pid ring, not be pulled and dropped"
        );
    }

    #[test]
    fn a_rejected_push_records_why_it_was_rejected() {
        // `output_drops` says a push was rejected but not which disagreement
        // caused it, which is what left the two-group popping unexplained
        // (session-2026-07-27-static.md). A ring far too small for the span
        // stands in for whatever the real cause turns out to be: what matters
        // is that the four numbers a reader needs are captured at the moment
        // it happens, in frames, and comparable against the governor's budget.
        let topology = Topology {
            master: audio_core::Gain::UNITY,
            groups: vec![group_spec(GroupId(0), OutputId(0))],
            outputs: vec![audio_core::OutputSpec { id: OutputId(0), format: mono(48_000) }],
        };
        let mut mixer = Mixer::new(&topology, 304).unwrap();
        // Enough input to drive the SRC past its warm-up and produce a span.
        for _ in 0..4 {
            mixer.push_group(GroupId(0), &vec![0.5f32; 304]);
            mixer.mix_tick();
        }

        let (out_p, _out_c) = RingBuffer::<f32>::new(16); // never drained
        let mut output_producers = vec![(OutputId(0), out_p, 1usize)];
        let mut output_scratch = vec![vec![0.0f32; mixer.output_capacity(OutputId(0))]];
        let ring_fill = vec![RingGauge {
            fill_permille: AtomicU32::new(0),
            active: AtomicBool::new(false),
        }];
        let mut ticks_since_real = vec![ACTIVE_HOLD_TICKS];
        let drops = AtomicU64::new(0);
        let diag = RejectDiag::default();

        flush_outputs(
            &mut output_producers,
            &mut output_scratch,
            &mut mixer,
            FlushCtx {
                ring_fill: &ring_fill,
                real_this_tick: &[true],
                ticks_since_real: &mut ticks_since_real,
                drops: &drops,
                reject_diag: &diag,
                budget_frames: &[304],
            },
        );

        assert!(drops.load(Ordering::Relaxed) > 0, "the 16-slot ring cannot hold a full span");
        assert!(diag.seen.load(Ordering::Relaxed), "a reject must record why");
        assert_eq!(diag.capacity_frames.load(Ordering::Relaxed), 16);
        assert_eq!(diag.budget_frames.load(Ordering::Relaxed), 304);
        assert_eq!(
            diag.free_frames.load(Ordering::Relaxed),
            16,
            "free space is captured BEFORE the push loop consumes it, or it always reads 0"
        );
        assert!(
            diag.span_frames.load(Ordering::Relaxed) > 16,
            "the span is what was offered, not what fit"
        );
    }

    #[test]
    fn drain_capture_commands_adds_and_removes_pid_consumers() {
        let (tx, rx) = mpsc::channel();
        let (_p1, c1) = RingBuffer::<f32>::new(4);
        let (_p2, c2) = RingBuffer::<f32>::new(4);
        let mut consumers = vec![slot(GroupId(0), 1, 0)];

        tx.send(CaptureMsg::Add { group: GroupId(0), pid: 100, consumer: c1 }).unwrap();
        tx.send(CaptureMsg::Add { group: GroupId(0), pid: 200, consumer: c2 }).unwrap();
        drain_capture_commands(&rx, &mut consumers);
        assert_eq!(consumers[0].pids.len(), 2);

        tx.send(CaptureMsg::Remove { group: GroupId(0), pid: 100 }).unwrap();
        drain_capture_commands(&rx, &mut consumers);
        assert_eq!(consumers[0].pids.len(), 1);
        assert_eq!(consumers[0].pids[0].0, 200);
    }

    #[test]
    fn apply_capture_sources_wires_a_pid_into_the_running_mixer_and_removes_it_cleanly() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        let capture = handle.capture_control();

        let failed = capture.apply_capture_sources(GroupId(0), vec![100, 200]).unwrap();
        assert!(failed.is_empty());
        sleep(Duration::from_millis(30));
        // No panic / no stall is the assertion here — two pid capture threads
        // are now feeding the running mixer's GroupSlot(0).

        let failed = capture.apply_capture_sources(GroupId(0), vec![200]).unwrap();
        assert!(failed.is_empty());
        sleep(Duration::from_millis(30));
        // pid 100's capture thread is stopped+joined synchronously inside
        // `apply_capture_sources` — reaching here without hanging is the proof.

        handle.shutdown().unwrap();
    }

    #[test]
    fn apply_capture_sources_on_a_stopped_engine_is_an_error() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(mock_endpoints()));
        let handle = start(&snapshot(), sys).unwrap();
        let capture = handle.capture_control();
        handle.shutdown().unwrap();

        assert!(matches!(
            capture.apply_capture_sources(GroupId(0), vec![100]),
            Err(EngineError::AlreadyStopped)
        ));
    }

    #[test]
    fn a_pid_whose_capture_thread_dies_mid_stream_is_reaped_and_retried() {
        // Regression test (review finding, 2026-07-21): a pid's capture
        // thread that exits from a runtime read error (not an open failure)
        // used to stay "current" forever — never reaped, never retried, even
        // though the same still-matching session keeps getting handed to
        // every subsequent reconcile.
        let sys = Arc::new(MockSystem::new(mock_endpoints()));
        sys.die_on_read(100);
        let sys_dyn: Arc<dyn AudioSystem> = sys.clone();
        let handle = start(&snapshot(), sys_dyn).unwrap();
        let capture = handle.capture_control();

        let failed = capture.apply_capture_sources(GroupId(0), vec![100]).unwrap();
        assert!(failed.is_empty(), "open itself succeeds — only read() fails");
        assert_eq!(sys.open_count(100), 1);

        // The capture thread dies on its very first read (poll_interval is
        // 1ms) — wait for that to actually happen before reconciling again.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while sys.open_count(100) < 2 && std::time::Instant::now() < deadline {
            // Reconcile again: if the dead pid were still wrongly considered
            // "current", this would be a no-op and open_count would never
            // advance past 1.
            capture.apply_capture_sources(GroupId(0), vec![100]).unwrap();
            sleep(Duration::from_millis(20));
        }

        assert!(
            sys.open_count(100) >= 2,
            "expected the dead pid to be reaped and re-opened, got open_count={}",
            sys.open_count(100)
        );
        handle.shutdown().unwrap();
    }
}
