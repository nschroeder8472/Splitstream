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
use std::time::{Duration, Instant};

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
    DeviceLost {
        groups: Vec<GroupId>,
    },
    /// Session-routing (P3) degradation notice — sent once per degradation
    /// episode by `routing::RoutingCoordinator`; audio path is unaffected.
    RoutingDegraded {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct EngineStats {
    pub xruns: u64,
    pub ring_fill: Vec<(OutputId, f32)>,
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
                .zip(rg.ring_fill.iter())
                .map(|(id, gauge)| (*id, f64::from_bits(gauge.applied_ratio_bits.load(Ordering::Relaxed))))
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

struct RingGauge {
    fill_permille: AtomicU32,
    /// Set by the mixer tick (notes §6); read cross-thread by the recovery
    /// supervisor to build `DriftController` `FillSample`s.
    active: AtomicBool,
    /// Last `ResampleRatio` applied to this output's `Src`s, as `f64::to_bits`
    /// (no `AtomicF64` in std) — surfaced via `EngineStats::applied_ratio`.
    applied_ratio_bits: AtomicU64,
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
        if running.is_none() {
            return Err(EngineError::AlreadyStopped);
        }
        let epoch = Epoch(self.persistent.epoch.load(Ordering::Relaxed));
        for cmd in cmds {
            self.persistent
                .commands
                .push(Envelope { epoch, cmd })
                .map_err(|_| EngineError::CommandQueueFull)?;
        }
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
            let Ok(port) = self.sys.open_process_capture(pid, false) else {
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
            for (pid, port, producer, consumer) in opened {
                let stop = Arc::new(AtomicBool::new(false));
                let thread = {
                    let stop = Arc::clone(&stop);
                    let sys = Arc::clone(&self.sys);
                    thread::spawn(move || pid_capture_loop(port, producer, &stop, sys.as_ref()))
                };
                rg.capture_pids.entry(group).or_default().insert(pid, PidCapture { stop, thread });
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
fn pid_capture_loop(
    mut port: Box<dyn CapturePort>,
    mut producer: rtrb::Producer<f32>,
    stop: &AtomicBool,
    sys: &dyn AudioSystem,
) {
    let _rt = sys.promote_rt_thread();
    let poll_interval = port.poll_interval();
    let channels = port.format().channels.max(1) as usize;
    let mut buf = vec![0.0f32; channels * 256];
    let sleeper = spin_sleep::SpinSleeper::default();

    while !stop.load(Ordering::Relaxed) {
        match port.read(&mut buf) {
            Ok(n) => {
                for &sample in &buf[..n] {
                    let _ = producer.push(sample); // ring full: drop, best-effort (notes §1)
                }
            }
            Err(_) => return, // this pid's stream is done — other pids/groups keep running
        }
        sleeper.sleep(poll_interval);
    }
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

fn spawn_render_threads(
    renders: Vec<(OutputId, Box<dyn RenderPort>)>,
    stop: &Arc<AtomicBool>,
    xruns: &Arc<AtomicU64>,
    faults: &Sender<Fault>,
    sys: &Arc<dyn AudioSystem>,
) -> (Vec<JoinHandle<()>>, OutputProducers) {
    let mut threads = Vec::with_capacity(renders.len());
    let mut producers = Vec::with_capacity(renders.len());
    for (output_id, port) in renders.into_iter() {
        let format = port.format();
        let device_period_s = port.period_frames() as f64 / format.sample_rate.max(1) as f64;
        let capacity = ring_capacity_samples(device_period_s, format.sample_rate, format.channels);
        let (producer, consumer) = RingBuffer::<f32>::new(capacity);
        producers.push((output_id, producer, format.channels as usize));

        let stop = Arc::clone(stop);
        let xruns = Arc::clone(xruns);
        let faults = faults.clone();
        let sys = Arc::clone(sys);
        threads.push(thread::spawn(move || {
            let ctx = RenderFaultCtx {
                xruns: &xruns,
                output_id,
                faults: &faults,
            };
            render_loop(port, consumer, &stop, &ctx, sys.as_ref());
        }));
    }
    (threads, producers)
}

fn build_running_graph(
    snapshot: &ConfigSnapshot,
    sys: &Arc<dyn AudioSystem>,
    persistent: &Arc<Persistent>,
    parked: &HashSet<String>,
) -> Result<RunningGraph, EngineError> {
    let opened = open_graph(snapshot, sys, parked)?;

    let tick_period = compute_tick_period(&opened.renders);
    let max_block_frames = compute_max_block_frames(&opened.plan, tick_period);
    let mixer = Mixer::new(&opened.plan.topology, max_block_frames)?;
    log_channel_conversions(&opened.plan.topology, max_block_frames);

    let stop = Arc::new(AtomicBool::new(false));
    let xruns = Arc::new(AtomicU64::new(0));
    let group_ids: Vec<GroupId> = opened.plan.topology.groups.iter().map(|g| g.id).collect();
    let output_ids: Vec<OutputId> = opened.plan.topology.outputs.iter().map(|o| o.id).collect();
    let ring_fill = Arc::new(
        output_ids
            .iter()
            .map(|_| RingGauge {
                fill_permille: AtomicU32::new(0),
                active: AtomicBool::new(false),
                applied_ratio_bits: AtomicU64::new(1.0f64.to_bits()),
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
    let (render_threads, output_producers) =
        spawn_render_threads(opened.renders, &stop, &xruns, &fault_tx, sys);

    let (capture_tx, capture_rx) = mpsc::channel();
    let mixer_args = MixerThreadArgs {
        max_block_frames,
        persistent: Arc::clone(persistent),
        ring_fill: Arc::clone(&ring_fill),
        output_index_of: output_index_of.clone(),
        stop: Arc::clone(&stop),
        tick_period,
        sys: Arc::clone(sys),
        duck_depth_db: Arc::clone(&duck_depth_db),
        limiter_engaged: Arc::clone(&limiter_engaged),
        group_peak: Arc::clone(&group_peak),
        output_peak: Arc::clone(&output_peak),
        capture_rx,
    };
    let mixer_thread = thread::spawn(move || {
        mixer_loop(mixer, group_consumers, output_producers, mixer_args);
    });

    Ok(RunningGraph {
        stop,
        capture_pids: HashMap::new(),
        capture_tx,
        mixer_thread: Some(mixer_thread),
        render_threads,
        xruns,
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
            let taps = HrirSet::taps_for(out.format.sample_rate);
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
    output_id: OutputId,
    faults: &'a Sender<Fault>,
}

fn render_loop(
    mut port: Box<dyn RenderPort>,
    mut consumer: rtrb::Consumer<f32>,
    stop: &AtomicBool,
    ctx: &RenderFaultCtx,
    sys: &dyn AudioSystem,
) {
    let _rt = sys.promote_rt_thread();
    let channels = port.format().channels.max(1) as usize;
    let mut buf = vec![0.0f32; port.period_frames() * channels];
    let wait_timeout = Duration::from_millis(100);

    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = port.wait_event(wait_timeout) {
            let _ = ctx.faults.send(Fault {
                source: FaultSource::Output(ctx.output_id),
                kind: FaultKind::from(&e),
            });
            return; // device invalidated — exit, rest of the graph keeps running
        }

        let mut got = 0;
        while got < buf.len() {
            match consumer.pop() {
                Ok(sample) => {
                    buf[got] = sample;
                    got += 1;
                }
                Err(_) => break, // ring empty
            }
        }
        if got < buf.len() {
            buf[got..].fill(0.0); // underrun: pad with silence, never wait for the mixer
            ctx.xruns.fetch_add(1, Ordering::Relaxed);
        }
        if let Err(e) = port.write(&buf) {
            let _ = ctx.faults.send(Fault {
                source: FaultSource::Output(ctx.output_id),
                kind: FaultKind::from(&e),
            });
            return;
        }
    }
}

/// Everything the mixer thread needs besides the `Mixer` and its rings —
/// grouped so `mixer_loop` takes 4 parameters instead of 9.
struct MixerThreadArgs {
    max_block_frames: usize,
    persistent: Arc<Persistent>,
    ring_fill: Arc<Vec<RingGauge>>,
    output_index_of: HashMap<OutputId, usize>,
    stop: Arc<AtomicBool>,
    tick_period: Duration,
    sys: Arc<dyn AudioSystem>,
    duck_depth_db: Arc<Vec<AtomicU32>>,
    limiter_engaged: Arc<Vec<AtomicU64>>,
    group_peak: Arc<Vec<AtomicU64>>,
    output_peak: Arc<Vec<AtomicU64>>,
    /// `CaptureControl::apply_capture_sources` sends per-pid add/remove here —
    /// drained once per tick, same as `persistent.commands`.
    capture_rx: Receiver<CaptureMsg>,
}

fn mixer_loop(
    mut mixer: Mixer,
    mut group_consumers: GroupConsumers,
    mut output_producers: OutputProducers,
    args: MixerThreadArgs,
) {
    let _rt = args.sys.promote_rt_thread();
    let sleeper = spin_sleep::SpinSleeper::default();

    let mut group_scratch: Vec<Vec<f32>> = group_consumers
        .iter()
        .map(|slot| vec![0.0f32; args.max_block_frames * slot.channels])
        .collect();
    let mut output_scratch: Vec<Vec<f32>> = output_producers
        .iter()
        .map(|(_, _, channels)| vec![0.0f32; args.max_block_frames * channels])
        .collect();
    // Hysteresis state for RingGauge.active — mixer-thread-owned, no per-tick alloc.
    let mut real_this_tick = vec![false; output_producers.len()];
    let mut ticks_since_real = vec![ACTIVE_HOLD_TICKS; output_producers.len()];
    // Built once at thread start (not per-tick — plain lookups only below):
    // parallel id lists matching `args.duck_depth_db`/`args.limiter_engaged`'s
    // index order, which was built from the same topology.
    let group_ids: Vec<GroupId> = group_consumers.iter().map(|slot| slot.group_id).collect();
    let output_ids: Vec<OutputId> = output_producers.iter().map(|(id, ..)| *id).collect();

    while !args.stop.load(Ordering::Relaxed) {
        let tick_start = Instant::now();

        drain_capture_commands(&args.capture_rx, &mut group_consumers);
        drain_commands(&args.persistent, &mut mixer, &args.ring_fill, &args.output_index_of);
        pull_group_inputs(
            &mut group_consumers,
            &mut group_scratch,
            &mut mixer,
            &mut real_this_tick,
        );
        mixer.mix_tick();
        update_telemetry(&mixer, &group_ids, &output_ids, &args);
        flush_outputs(
            &mut output_producers,
            &mut output_scratch,
            &mut mixer,
            &args.ring_fill,
            &real_this_tick,
            &mut ticks_since_real,
        );

        let budget = args.tick_period.saturating_sub(tick_start.elapsed());
        sleeper.sleep(budget);
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
    ring_fill: &[RingGauge],
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
                ring_fill[index]
                    .applied_ratio_bits
                    .store(ratio.value().to_bits(), Ordering::Relaxed);
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

/// Sums every pid currently captured into a group's scratch buffer, one
/// frame at a time — a group with zero pids (nothing matched yet, or every
/// pid starved this tick) behaves exactly like the old single-consumer
/// "starved" case: silence, never a stall (the mixer tick is timer-paced).
/// `real_this_tick` for a group's output is set if *any* pid fully filled
/// the block this tick (even WASAPI-silent audio still delivers
/// SILENT-flagged packets — notes §6).
fn pull_group_inputs(
    group_consumers: &mut [GroupSlot],
    group_scratch: &mut [Vec<f32>],
    mixer: &mut Mixer,
    real_this_tick: &mut [bool],
) {
    real_this_tick.fill(false);
    for (i, slot) in group_consumers.iter_mut().enumerate() {
        let scratch = &mut group_scratch[i];
        scratch.fill(0.0);
        let mut any_full = false;
        for (_, consumer) in slot.pids.iter_mut() {
            let mut filled = 0;
            while filled < scratch.len() {
                match consumer.pop() {
                    Ok(sample) => {
                        scratch[filled] += sample;
                        filled += 1;
                    }
                    Err(_) => break,
                }
            }
            if filled == scratch.len() {
                any_full = true;
            }
        }
        if any_full {
            real_this_tick[slot.output_index] = true;
        }
        mixer.push_group(slot.group_id, scratch);
    }
}

fn flush_outputs(
    output_producers: &mut [(OutputId, rtrb::Producer<f32>, usize)],
    output_scratch: &mut [Vec<f32>],
    mixer: &mut Mixer,
    ring_fill: &[RingGauge],
    real_this_tick: &[bool],
    ticks_since_real: &mut [u32],
) {
    for (i, (output_id, producer, _)) in output_producers.iter_mut().enumerate() {
        let scratch = &mut output_scratch[i];
        let n = mixer.take_output(*output_id, scratch);
        for &sample in &scratch[..n] {
            let _ = producer.push(sample); // ring full: drop, best-effort (notes §1)
        }
        let capacity = producer.buffer().capacity();
        let filled = capacity - producer.slots();
        let permille = if capacity > 0 {
            (filled * 1000 / capacity) as u32
        } else {
            0
        };
        ring_fill[i]
            .fill_permille
            .store(permille, Ordering::Relaxed);

        let active = update_activity(real_this_tick[i], &mut ticks_since_real[i]);
        ring_fill[i].active.store(active, Ordering::Relaxed);
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

/// notes §5: tick period is half the minimum device period across the graph.
/// No more capture ports here (process-loopback-capture pivot — capture
/// sources are opened dynamically per pid, long after the tick period is
/// fixed at build time): render ports alone set the floor, same fallback for
/// an empty topology as before.
fn compute_tick_period(renders: &[(OutputId, Box<dyn RenderPort>)]) -> Duration {
    let min_period_s = renders
        .iter()
        .map(|(_, r)| r.period_frames() as f64 / r.format().sample_rate.max(1) as f64)
        .fold(f64::INFINITY, f64::min);

    let period_s = if min_period_s.is_finite() {
        (min_period_s / 2.0).max(0.001)
    } else {
        0.005 // no ports at all (empty topology) — arbitrary safe default
    };
    Duration::from_secs_f64(period_s)
}

fn compute_max_block_frames(plan: &GraphPlan, tick_period: Duration) -> usize {
    plan.topology
        .groups
        .iter()
        .map(|g| frames_for(tick_period, g.input_format.sample_rate) + BLOCK_FRAME_MARGIN)
        .max()
        .unwrap_or(BLOCK_FRAME_MARGIN)
}

/// Everything a supervisor tick needs read out of the live `RunningGraph`,
/// captured under one short lock so the rest of the tick runs lock-free.
struct SupervisorSnapshot {
    output_ids: Vec<OutputId>,
    ring_fill: Arc<Vec<RingGauge>>,
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
    let cfg = DriftConfig::default();
    let device_events = sys.subscribe_device_events().ok();
    let mut drift = DriftController::new(&[], cfg);
    let mut known_outputs: Vec<OutputId> = Vec::new();
    let sleeper = spin_sleep::SpinSleeper::default();

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
                ring_fill: Arc::clone(&rg.ring_fill),
                group_outputs: rg.group_outputs.clone(),
                output_endpoints: rg.output_endpoints.clone(),
            })
        };
        let Some(snap) = snapshot else {
            sleeper.sleep(cfg.tick);
            continue;
        };

        // Topology changed since last tick (rebuild happened) — old
        // integrator state no longer applies to a fresh set of rings.
        if snap.output_ids != known_outputs {
            drift = DriftController::new(&snap.output_ids, cfg);
            known_outputs = snap.output_ids.clone();
        }

        let fills: Vec<(OutputId, FillSample)> = snap
            .output_ids
            .iter()
            .zip(snap.ring_fill.iter())
            .map(|(id, gauge)| {
                (
                    *id,
                    FillSample {
                        fill: gauge.fill_permille.load(Ordering::Relaxed) as f32 / 1000.0,
                        active: gauge.active.load(Ordering::Relaxed),
                    },
                )
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
        if let Some(rx) = &device_events {
            while let Ok(evt) = rx.try_recv() {
                match evt {
                    DeviceEvent::Removed(id) => dead_endpoints.push(id),
                    DeviceEvent::Added(endpoint) => added_endpoints.push(endpoint),
                    DeviceEvent::DefaultChanged(_) | DeviceEvent::StateChanged(_) => {}
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

        sleeper.sleep(cfg.tick);
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
        fn write(&mut self, _frames: &[f32]) -> Result<(), PortError> {
            Ok(())
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

        pid_capture_loop(Box::new(FailingCapture), producer, &stop, &sys);
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
        assert_eq!(rg.ring_fill.len(), 1);
        let ratio = f64::from_bits(rg.ring_fill[0].applied_ratio_bits.load(Ordering::Relaxed));
        assert_eq!(ratio, 1.0);
        stop_running_graph(rg);
    }

    #[test]
    fn set_output_ratio_command_updates_applied_ratio_stat() {
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
        sleep(Duration::from_millis(30));
        let applied = f64::from_bits(rg.ring_fill[0].applied_ratio_bits.load(Ordering::Relaxed));
        assert_eq!(applied, 1.003);
        stop_running_graph(rg);
    }

    #[test]
    fn render_loop_reports_non_invalidated_faults_as_other() {
        let stop = AtomicBool::new(false);
        let xruns = AtomicU64::new(0);
        let (fault_tx, fault_rx) = mpsc::channel();
        let sys = MockSystem::new(vec![]);
        let (_producer, consumer) = RingBuffer::<f32>::new(4);
        let ctx = RenderFaultCtx {
            xruns: &xruns,
            output_id: OutputId(0),
            faults: &fault_tx,
        };

        render_loop(Box::new(FailingRender), consumer, &stop, &ctx, &sys);

        let fault = fault_rx.recv().unwrap();
        assert!(matches!(fault.source, FaultSource::Output(OutputId(0))));
        assert!(matches!(fault.kind, FaultKind::Other));
    }

    // --- Recovery supervisor integration tests. These run the real
    // background supervisor thread (started by `start()`) and drive it via
    // `MockSystem`'s device-event/enumerate hooks, so they wait on the
    // supervisor's actual ~100ms tick cadence via `recv_timeout` rather than
    // calling its internal functions directly.

    #[test]
    fn device_removal_falls_back_to_default_output_and_keeps_engine_running() {
        let sys = Arc::new(MockSystem::new(two_output_endpoints()));
        sys.set_default_output(EndpointId("out-2".into()));
        let mut handle = start(&snapshot(), Arc::clone(&sys) as Arc<dyn AudioSystem>).unwrap();
        let events = handle.take_events();
        // Supervisor subscribes to device events at the top of its loop, just
        // after spawn — give it a moment before emitting, or the event has
        // nowhere to land yet (MockSystem drops emits with no subscriber).
        sleep(Duration::from_millis(50));

        sys.remove_endpoint(&EndpointId("out-1".into()));
        sys.emit_device_event(DeviceEvent::Removed(EndpointId("out-1".into())));

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

        pull_group_inputs(&mut consumers, &mut scratch, &mut mixer, &mut real_this_tick);

        assert!(real_this_tick[0], "both pids fully filled this tick");
        // The two pids' contributions are summed sample-by-sample before
        // reaching the mixer: 0.2+0.5, 0.3+(-0.1).
        assert!((scratch[0][0] - 0.7).abs() < 1e-6, "got {}", scratch[0][0]);
        assert!((scratch[0][1] - 0.2).abs() < 1e-6, "got {}", scratch[0][1]);
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

        pull_group_inputs(&mut consumers, &mut scratch, &mut mixer, &mut real_this_tick);

        assert!(!real_this_tick[0], "no pids -> not active");
        assert_eq!(scratch[0], vec![0.0; 4]);
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
