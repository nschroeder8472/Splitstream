//! Graph orchestration: opens ports, spawns capture/mixer/render threads,
//! owns the lock-free command queue and topology epoch.
//!
//! Threading model per `.lattice/context/engine-core.md` L3:
//! - capture ×N (polled) → SPSC ring → mixer ×1 (timer-paced) → SPSC ring → render ×M (event-driven)
//! - param changes flow through a bounded MPSC command queue, tagged with the current `Epoch`
//! - structural changes ([`EngineHandle::rebuild`]) stop and respawn the whole thread set

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;
use rtrb::RingBuffer;

use audio_core::{DomainError, GroupId, Mixer, MixerCommand, OutputId, Topology};

use crate::graph::{self, ConfigSnapshot, GraphPlan};
use crate::ports::{AudioSystem, CapturePort, PortError, RenderPort};

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

#[derive(Clone, Copy)]
struct Envelope {
    epoch: Epoch,
    cmd: MixerCommand,
}

pub struct EngineStats {
    pub xruns: u64,
    pub ring_fill: Vec<(OutputId, f32)>,
    pub group_faults: Vec<GroupId>,
}

const COMMAND_QUEUE_CAPACITY: usize = 256;
/// notes §6: ring capacity = 4x the largest period involved.
const RING_PERIOD_MARGIN: usize = 4;
/// Headroom added on top of the tick-period frame count when sizing `Mixer`'s
/// per-group scratch buffers, to absorb scheduling jitter between ticks.
const BLOCK_FRAME_MARGIN: usize = 8;

struct RingGauge {
    fill_permille: AtomicU32,
}

type GroupConsumers = Vec<(GroupId, rtrb::Consumer<f32>, usize)>;
type OutputProducers = Vec<(OutputId, rtrb::Producer<f32>, usize)>;

struct Persistent {
    commands: ArrayQueue<Envelope>,
    epoch: AtomicU64,
}

struct RunningGraph {
    stop: Arc<AtomicBool>,
    capture_threads: Vec<JoinHandle<()>>,
    mixer_thread: Option<JoinHandle<()>>,
    render_threads: Vec<JoinHandle<()>>,
    xruns: Arc<AtomicU64>,
    ring_fill: Arc<Vec<RingGauge>>,
    output_ids: Vec<OutputId>,
    group_faulted: Arc<Vec<AtomicBool>>,
    group_ids: Vec<GroupId>,
}

pub struct EngineHandle {
    sys: Arc<dyn AudioSystem>,
    persistent: Arc<Persistent>,
    running: Mutex<Option<RunningGraph>>,
}

pub fn start(
    snapshot: &ConfigSnapshot,
    sys: Arc<dyn AudioSystem>,
) -> Result<EngineHandle, EngineError> {
    let persistent = Arc::new(Persistent {
        commands: ArrayQueue::new(COMMAND_QUEUE_CAPACITY),
        epoch: AtomicU64::new(0),
    });
    let running = build_running_graph(snapshot, &sys, &persistent)?;
    Ok(EngineHandle {
        sys,
        persistent,
        running: Mutex::new(Some(running)),
    })
}

impl EngineHandle {
    pub fn apply_params(&self, cmds: &[MixerCommand]) -> Result<(), EngineError> {
        let running = self.running.lock().unwrap();
        if running.is_none() {
            return Err(EngineError::AlreadyStopped);
        }
        let epoch = Epoch(self.persistent.epoch.load(Ordering::Relaxed));
        for &cmd in cmds {
            self.persistent
                .commands
                .push(Envelope { epoch, cmd })
                .map_err(|_| EngineError::CommandQueueFull)?;
        }
        Ok(())
    }

    /// Structural change: stops and fully respawns the thread set against the
    /// new snapshot, bumping the epoch so stale in-flight commands are
    /// dropped by the new mixer thread. **Simplification from the L3 design**
    /// (logged in `.lattice/context/engine-core.md`): this rebuilds the
    /// *entire* graph, not just the affected group/output — correct, but a
    /// config change to one group briefly gaps audio on every group, not
    /// just the changed one. If `build_running_graph` fails, the engine is
    /// left stopped (no rollback to the pre-rebuild graph).
    pub fn rebuild(&self, snapshot: &ConfigSnapshot) -> Result<(), EngineError> {
        let mut running = self.running.lock().unwrap();
        if running.is_none() {
            return Err(EngineError::AlreadyStopped);
        }
        if let Some(rg) = running.take() {
            stop_running_graph(rg);
        }
        self.persistent.epoch.fetch_add(1, Ordering::Relaxed);
        let new_running = build_running_graph(snapshot, &self.sys, &self.persistent)?;
        *running = Some(new_running);
        Ok(())
    }

    pub fn stats(&self) -> EngineStats {
        let running = self.running.lock().unwrap();
        match running.as_ref() {
            None => EngineStats {
                xruns: 0,
                ring_fill: Vec::new(),
                group_faults: Vec::new(),
            },
            Some(rg) => EngineStats {
                xruns: rg.xruns.load(Ordering::Relaxed),
                ring_fill: rg
                    .output_ids
                    .iter()
                    .zip(rg.ring_fill.iter())
                    .map(|(id, gauge)| {
                        (
                            *id,
                            gauge.fill_permille.load(Ordering::Relaxed) as f32 / 1000.0,
                        )
                    })
                    .collect(),
                group_faults: rg
                    .group_ids
                    .iter()
                    .zip(rg.group_faulted.iter())
                    .filter(|(_, faulted)| faulted.load(Ordering::Relaxed))
                    .map(|(id, _)| *id)
                    .collect(),
            },
        }
    }

    pub fn epoch(&self) -> Epoch {
        Epoch(self.persistent.epoch.load(Ordering::Relaxed))
    }

    pub fn shutdown(self) -> Result<(), EngineError> {
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
    for t in rg.capture_threads.drain(..) {
        let _ = t.join();
    }
    if let Some(t) = rg.mixer_thread.take() {
        let _ = t.join();
    }
    for t in rg.render_threads.drain(..) {
        let _ = t.join();
    }
}

/// Resolved config + every port opened, but nothing spawned yet.
struct OpenedGraph {
    plan: GraphPlan,
    captures: Vec<(GroupId, Box<dyn CapturePort>)>,
    renders: Vec<(OutputId, Box<dyn RenderPort>)>,
}

/// Opens every port synchronously, before anything is spawned: fail fast,
/// nothing to unwind if a configured device doesn't open.
fn open_graph(
    snapshot: &ConfigSnapshot,
    sys: &Arc<dyn AudioSystem>,
) -> Result<OpenedGraph, EngineError> {
    let endpoints = sys.enumerate()?;
    let plan = graph::resolve(snapshot, &endpoints)?;

    let mut captures = Vec::with_capacity(plan.group_endpoints.len());
    for (group_id, endpoint_id) in &plan.group_endpoints {
        captures.push((*group_id, sys.open_capture(endpoint_id)?));
    }
    let mut renders = Vec::with_capacity(plan.output_endpoints.len());
    for (output_id, endpoint_id) in &plan.output_endpoints {
        renders.push((*output_id, sys.open_render(endpoint_id)?));
    }

    Ok(OpenedGraph {
        plan,
        captures,
        renders,
    })
}

fn spawn_capture_threads(
    captures: Vec<(GroupId, Box<dyn CapturePort>)>,
    stop: &Arc<AtomicBool>,
    group_faulted: &Arc<Vec<AtomicBool>>,
    sys: &Arc<dyn AudioSystem>,
) -> (Vec<JoinHandle<()>>, GroupConsumers) {
    let mut threads = Vec::with_capacity(captures.len());
    let mut consumers = Vec::with_capacity(captures.len());
    for (index, (group_id, port)) in captures.into_iter().enumerate() {
        let format = port.format();
        let device_period_s = port.poll_interval().as_secs_f64() * 2.0; // polled at ~period/2
        let capacity = ring_capacity_samples(device_period_s, format.sample_rate, format.channels);
        let (producer, consumer) = RingBuffer::<f32>::new(capacity);
        consumers.push((group_id, consumer, format.channels as usize));

        let stop = Arc::clone(stop);
        let faulted = Arc::clone(group_faulted);
        let sys = Arc::clone(sys);
        threads.push(thread::spawn(move || {
            capture_loop(port, producer, &stop, &faulted, index, sys.as_ref());
        }));
    }
    (threads, consumers)
}

fn spawn_render_threads(
    renders: Vec<(OutputId, Box<dyn RenderPort>)>,
    stop: &Arc<AtomicBool>,
    xruns: &Arc<AtomicU64>,
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
        let sys = Arc::clone(sys);
        threads.push(thread::spawn(move || {
            render_loop(port, consumer, &stop, &xruns, sys.as_ref());
        }));
    }
    (threads, producers)
}

fn build_running_graph(
    snapshot: &ConfigSnapshot,
    sys: &Arc<dyn AudioSystem>,
    persistent: &Arc<Persistent>,
) -> Result<RunningGraph, EngineError> {
    let opened = open_graph(snapshot, sys)?;

    let tick_period = compute_tick_period(&opened.captures, &opened.renders);
    let max_block_frames = compute_max_block_frames(&opened.plan, tick_period);
    let mixer = Mixer::new(&opened.plan.topology, max_block_frames)?;
    log_channel_conversions(&opened.plan.topology);

    let stop = Arc::new(AtomicBool::new(false));
    let xruns = Arc::new(AtomicU64::new(0));
    let group_ids: Vec<GroupId> = opened.plan.topology.groups.iter().map(|g| g.id).collect();
    let output_ids: Vec<OutputId> = opened.plan.topology.outputs.iter().map(|o| o.id).collect();
    let group_faulted = Arc::new(
        (0..group_ids.len())
            .map(|_| AtomicBool::new(false))
            .collect::<Vec<_>>(),
    );
    let ring_fill = Arc::new(
        output_ids
            .iter()
            .map(|_| RingGauge {
                fill_permille: AtomicU32::new(0),
            })
            .collect::<Vec<_>>(),
    );

    let (capture_threads, group_consumers) =
        spawn_capture_threads(opened.captures, &stop, &group_faulted, sys);
    let (render_threads, output_producers) =
        spawn_render_threads(opened.renders, &stop, &xruns, sys);

    let mixer_args = MixerThreadArgs {
        max_block_frames,
        persistent: Arc::clone(persistent),
        ring_fill: Arc::clone(&ring_fill),
        stop: Arc::clone(&stop),
        tick_period,
        sys: Arc::clone(sys),
    };
    let mixer_thread = thread::spawn(move || {
        mixer_loop(mixer, group_consumers, output_producers, mixer_args);
    });

    Ok(RunningGraph {
        stop,
        capture_threads,
        mixer_thread: Some(mixer_thread),
        render_threads,
        xruns,
        ring_fill,
        output_ids,
        group_faulted,
        group_ids,
    })
}

/// Off-RT, called once at graph build (startup/rebuild) — never on the mixer
/// thread. Surfaces silently-inserted channel conversions (L3 interaction D:
/// `.lattice/context/channel-mixdown.md`) so a downmix that changes what the
/// user hears is visible, not a hidden mixer-internal detail.
fn log_channel_conversions(topology: &Topology) {
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
    }
}

fn capture_loop(
    mut port: Box<dyn CapturePort>,
    mut producer: rtrb::Producer<f32>,
    stop: &AtomicBool,
    faulted: &[AtomicBool],
    group_index: usize,
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
            // P1 minimal fault handling (spec interaction E): mark faulted and exit.
            // The rest of the graph — other groups, all outputs — keeps running,
            // since each has its own thread and ring. Full recovery is P2.
            Err(_) => {
                faulted[group_index].store(true, Ordering::Relaxed);
                return;
            }
        }
        sleeper.sleep(poll_interval);
    }
}

fn render_loop(
    mut port: Box<dyn RenderPort>,
    mut consumer: rtrb::Consumer<f32>,
    stop: &AtomicBool,
    xruns: &AtomicU64,
    sys: &dyn AudioSystem,
) {
    let _rt = sys.promote_rt_thread();
    let channels = port.format().channels.max(1) as usize;
    let mut buf = vec![0.0f32; port.period_frames() * channels];
    let wait_timeout = Duration::from_millis(100);

    while !stop.load(Ordering::Relaxed) {
        if port.wait_event(wait_timeout).is_err() {
            return; // device invalidated — P1 minimal: exit, rest of the graph keeps running
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
            xruns.fetch_add(1, Ordering::Relaxed);
        }
        if port.write(&buf).is_err() {
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
    stop: Arc<AtomicBool>,
    tick_period: Duration,
    sys: Arc<dyn AudioSystem>,
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
        .map(|(_, _, channels)| vec![0.0f32; args.max_block_frames * channels])
        .collect();
    let mut output_scratch: Vec<Vec<f32>> = output_producers
        .iter()
        .map(|(_, _, channels)| vec![0.0f32; args.max_block_frames * channels])
        .collect();

    while !args.stop.load(Ordering::Relaxed) {
        let tick_start = Instant::now();

        drain_commands(&args.persistent, &mut mixer);
        pull_group_inputs(&mut group_consumers, &mut group_scratch, &mut mixer);
        flush_outputs(
            &mut output_producers,
            &mut output_scratch,
            &mut mixer,
            &args.ring_fill,
        );

        let budget = args.tick_period.saturating_sub(tick_start.elapsed());
        sleeper.sleep(budget);
    }
}

fn drain_commands(persistent: &Persistent, mixer: &mut Mixer) {
    while let Some(envelope) = persistent.commands.pop() {
        if envelope.epoch.0 != persistent.epoch.load(Ordering::Relaxed) {
            continue; // stale — dropped, not applied (topology epoch, notes §7)
        }
        mixer.apply(envelope.cmd);
    }
}

fn pull_group_inputs(
    group_consumers: &mut [(GroupId, rtrb::Consumer<f32>, usize)],
    group_scratch: &mut [Vec<f32>],
    mixer: &mut Mixer,
) {
    for (i, (group_id, consumer, _)) in group_consumers.iter_mut().enumerate() {
        let scratch = &mut group_scratch[i];
        let mut filled = 0;
        while filled < scratch.len() {
            match consumer.pop() {
                Ok(sample) => {
                    scratch[filled] = sample;
                    filled += 1;
                }
                Err(_) => break,
            }
        }
        if filled < scratch.len() {
            // Starved group (silent bus produces no loopback packets) — synthesize
            // silence rather than stalling; this is why the mixer tick is timer-paced.
            scratch[filled..].fill(0.0);
        }
        mixer.push_group(*group_id, scratch);
    }
}

fn flush_outputs(
    output_producers: &mut [(OutputId, rtrb::Producer<f32>, usize)],
    output_scratch: &mut [Vec<f32>],
    mixer: &mut Mixer,
    ring_fill: &[RingGauge],
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
    }
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
/// Capture ports expose only their poll interval (~period/2, so device period
/// is ~2x that); render ports expose `period_frames` directly.
fn compute_tick_period(
    captures: &[(GroupId, Box<dyn CapturePort>)],
    renders: &[(OutputId, Box<dyn RenderPort>)],
) -> Duration {
    let capture_periods = captures
        .iter()
        .map(|(_, c)| c.poll_interval().as_secs_f64() * 2.0);
    let render_periods = renders
        .iter()
        .map(|(_, r)| r.period_frames() as f64 / r.format().sample_rate.max(1) as f64);

    let min_period_s = capture_periods
        .chain(render_periods)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::mock::MockSystem;
    use crate::ports::{Endpoint, EndpointId, EndpointKind};
    use audio_core::{ChannelLayout, Format, Gain};
    use std::thread::sleep;

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
            layout: ChannelLayout::STEREO,
        }
    }

    fn mock_endpoints() -> Vec<Endpoint> {
        vec![
            Endpoint {
                id: EndpointId("bus-1".into()),
                name: "Game".into(),
                kind: EndpointKind::Bus,
                format: stereo(48_000),
            },
            Endpoint {
                id: EndpointId("out-1".into()),
                name: "Speakers".into(),
                kind: EndpointKind::Physical,
                format: stereo(48_000),
            },
        ]
    }

    fn snapshot() -> ConfigSnapshot {
        ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            groups: vec![graph::GroupConfig {
                name: "Game".into(),
                bus_endpoint: "Game".into(),
                output_device: "Speakers".into(),
                gain: Gain::UNITY,
                follow_master: true,
                match_rules: vec![],
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
        broken.groups[0].bus_endpoint = "does-not-exist".into();
        assert!(matches!(
            handle.rebuild(&broken),
            Err(EngineError::Resolve(_))
        ));

        assert!(matches!(
            handle.apply_params(&[]),
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
            .apply_params(&[MixerCommand::SetGroupGain(GroupId(0), Gain::SILENT)])
            .unwrap();
        // No panic / no error is the assertion here: apply_params round-trips
        // through the real lock-free queue into a live mixer thread.
        sleep(Duration::from_millis(30));
        handle.shutdown().unwrap();
    }

    #[test]
    fn start_with_no_matching_endpoints_returns_resolve_error() {
        let sys: Arc<dyn AudioSystem> = Arc::new(MockSystem::new(vec![])); // no endpoints at all
        let result = start(&snapshot(), sys);
        assert!(matches!(result, Err(EngineError::Resolve(_))));
    }
}
