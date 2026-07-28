//! Ring-fill PI control loop (drift-and-recovery L4, notes §10). Pure — no
//! threads, no OS — so it's unit-testable with synthetic fill curves before
//! ever touching real hardware. Runs on the recovery-supervisor control
//! thread at `DriftConfig::tick` cadence (~100 ms); RT threads only publish
//! fill via atomics upstream, `tick` emits `MixerCommand::SetOutputRatio`
//! for the RT mixer thread to apply.
//!
//! # Which buffer this regulates
//!
//! The **capture** rings, aggregated per output — not the output ring
//! (output-rate-truncation follow-on, 2026-07-27). The output ring is already
//! regulated:
//! the governor (`group_may_push`) holds it at its threshold by withholding
//! ticks. Pointing this loop at the same buffer made `ratio` an unobservable
//! free variable — it integrated on the governor sawtooth's phase noise and
//! wandered onto both clamp rails, measured in a live audit trace.
//!
//! Since `SincFixedIn` consumes a *fixed* input chunk, in steady state
//! `input consumed/sec = R_out / ratio`. So whatever value `ratio` drifted to
//! silently dictated the capture drain rate, and the capture ring — the only
//! elastic buffer between the capture clock and the DAC's crystal, and the one
//! with no controller on it — absorbed the difference until it saturated and
//! began discarding samples permanently.
//!
//! `ratio` is the only lever that can absorb a clock mismatch. This module
//! points it at the buffer that needs it.
//!
//! Keyed per **output**, not per group, for two reasons. Correctness:
//! `mix_tick` sums every group on an output into one shared accumulator over a
//! single span, so they must produce identical frame counts — a per-group ratio
//! (tried and reverted the same day) notches the shorter group's tail at the
//! tick rate. Physics: process-loopback capture for every app on a machine runs
//! off one WASAPI engine clock at one pinned rate, so two groups' capture
//! streams do not drift apart from each other. The clock that differs is the
//! DAC's, which every group on that output shares. A group's own capture ring
//! is still what is *measured* — the output's sample is the fullest of them.

use std::collections::HashMap;
use std::time::Duration;

use audio_core::{MixerCommand, OutputId, ResampleRatio};

#[derive(Debug, Clone, Copy)]
pub struct DriftConfig {
    pub target_fill: f32,
    pub kp: f64,
    pub ki: f64,
    pub max_correction: f64,
    pub tick: Duration,
}

impl Default for DriftConfig {
    /// Starting gains from notes §10: kp ≈ 0.05, ki ≈ 0.01, max_correction = 0.005.
    ///
    /// `target_fill` is mid-capacity and, unlike the old output-side target, is
    /// **not** computed per build. The output ring's target had to be offset to
    /// the governor sawtooth's midpoint (`drift_target_fill`, deleted) or the
    /// controller would read a permanent error against the governor. A capture
    /// ring has no governor on it — its disturbance is the poll burst, in both
    /// directions — so mid-capacity is the value that leaves symmetric room for
    /// either sign of error.
    fn default() -> DriftConfig {
        DriftConfig {
            target_fill: 0.5,
            kp: 0.05,
            ki: 0.01,
            max_correction: 0.005,
            tick: Duration::from_millis(100),
        }
    }
}

/// One output's capture-ring fill for one `tick`: the fullest ring among the
/// groups routed to it. `active` is the idle guard (drift-and-recovery
/// revision) — pids that have stopped delivering leave a ring draining to
/// empty, and without this the integrator reads that as a huge negative error
/// and pegs the ratio, so the next time audio arrives it starts from a rail.
#[derive(Debug, Clone, Copy)]
pub struct FillSample {
    pub fill: f32,
    pub active: bool,
}

#[derive(Default)]
struct OutputState {
    integ: f64,
}

pub struct DriftController {
    cfg: DriftConfig,
    state: HashMap<OutputId, OutputState>,
}

impl DriftController {
    pub fn new(outputs: &[OutputId], cfg: DriftConfig) -> DriftController {
        DriftController {
            state: outputs.iter().map(|id| (*id, OutputState::default())).collect(),
            cfg,
        }
    }

    /// Pure: measurements in, commands out. An inactive output is skipped
    /// entirely — no command is emitted, so the mixer keeps applying
    /// whichever ratio it last received ("hold last ratio"), and the
    /// integrator does not accumulate error while silent.
    pub fn tick(&mut self, fills: &[(OutputId, FillSample)]) -> Vec<MixerCommand> {
        let tick_secs = self.cfg.tick.as_secs_f64();
        let mut cmds = Vec::with_capacity(fills.len());
        for (id, sample) in fills {
            // Output not registered at construction (e.g. stale id from a pre-rebuild
            // topology) — nothing to correct, ignore rather than panic.
            let Some(state) = self.state.get_mut(id) else {
                continue;
            };
            if !sample.active {
                continue;
            }

            let err = (sample.fill - self.cfg.target_fill) as f64;
            state.integ += err * tick_secs;
            let raw = self.cfg.kp * err + self.cfg.ki * state.integ;
            let corr = raw.clamp(-self.cfg.max_correction, self.cfg.max_correction);
            if raw != corr {
                // Anti-windup: undo the integration step taken this tick while clamped,
                // so the integrator doesn't keep growing during sustained saturation.
                state.integ -= err * tick_secs;
            }

            // Ring too full (err > 0) -> corr > 0 -> ratio < 1. The sign is
            // unchanged by the move from the output ring to the capture ring,
            // for a reason worth stating because it looks like it should have
            // flipped: `SincFixedIn` consumes a FIXED input chunk and produces
            // `chunk_in * ratio` output frames (resample.rs's `Src::process`),
            // so in steady state `input consumed/sec = R_out / ratio`. A
            // capture ring above target needs its input drained FASTER, which
            // is a LOWER ratio — the same direction an over-full output ring
            // needed for the opposite reason (produce less).
            let ratio = ResampleRatio::new(1.0 - corr)
                .expect("max_correction configured within ResampleRatio's clamp range");
            cmds.push(MixerCommand::SetOutputRatio(*id, ratio));
        }
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DriftConfig {
        DriftConfig::default()
    }

    fn active(fill: f32) -> FillSample {
        FillSample { fill, active: true }
    }

    #[test]
    fn drift_controller_lowers_the_ratio_when_the_ring_is_above_target() {
        // B7: SincFixedIn produces chunk_in * ratio output frames, so an
        // overfull ring must be corrected by producing LESS — a lower
        // ratio — not more (replaces the old, inverted
        // `overfull_ring_produces_a_ratio_above_one`).
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        let cmds = ctrl.tick(&[(OutputId(0), active(0.62))]);
        let MixerCommand::SetOutputRatio(id, ratio) = cmds[0] else {
            panic!("expected SetOutputRatio");
        };
        assert_eq!(id, OutputId(0));
        assert!(ratio.value() < 1.0, "overfull ring must produce less output to drain");
        assert!(ratio.value() >= 1.0 - cfg().max_correction);
    }

    #[test]
    fn drift_controller_raises_the_ratio_when_the_ring_is_below_target() {
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        let cmds = ctrl.tick(&[(OutputId(0), active(0.38))]);
        let MixerCommand::SetOutputRatio(_, ratio) = cmds[0] else {
            panic!("expected SetOutputRatio");
        };
        assert!(ratio.value() > 1.0, "underfull ring must produce more output to fill");
    }

    #[test]
    fn constant_error_converges_toward_target_without_diverging() {
        // Synthetic curve (notes §10): model each tick's correction as
        // directly reducing next tick's fill error by the same magnitude —
        // an idealized plant, enough to catch a wrong-sign or unstable
        // control law. A PI loop can briefly overshoot near the setpoint
        // (legitimate integral action, not windup) — the invariant under
        // test is boundedness and eventual convergence, not monotonicity.
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        let initial_err = 0.12f32;
        let mut fill = cfg().target_fill + initial_err;
        for _ in 0..200 {
            let cmds = ctrl.tick(&[(OutputId(0), active(fill))]);
            let MixerCommand::SetOutputRatio(_, ratio) = cmds[0] else {
                panic!("expected SetOutputRatio");
            };
            let corr = (1.0 - ratio.value()) as f32; // B7: ratio = 1 - corr now, not 1 + corr
            fill -= corr;
            let err = (fill - cfg().target_fill).abs();
            assert!(err <= initial_err * 1.1, "error must stay bounded, not diverge");
        }
        let final_err = (fill - cfg().target_fill).abs();
        assert!(final_err < 0.01, "should have converged near target after 200 ticks, got {final_err}");
    }

    #[test]
    fn anti_windup_recovers_immediately_once_error_returns_to_zero() {
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        // Sustained large error saturates the correction every tick — without
        // anti-windup the integrator keeps growing underneath the clamp.
        for _ in 0..500 {
            ctrl.tick(&[(OutputId(0), active(1.0))]);
        }
        // Error now sits exactly at target: a wound-up integrator would still
        // push a large ki*integ correction; anti-windup keeps it near zero.
        let cmds = ctrl.tick(&[(OutputId(0), active(cfg().target_fill))]);
        let MixerCommand::SetOutputRatio(_, ratio) = cmds[0] else {
            panic!("expected SetOutputRatio");
        };
        assert!(
            (ratio.value() - 1.0).abs() < 1.0e-3,
            "wound-up integrator would still apply a large correction at zero error, got {}",
            ratio.value()
        );
    }

    #[test]
    fn idle_output_emits_no_command_and_freezes_integrator() {
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        let warm = ctrl.tick(&[(OutputId(0), active(0.62))]);
        let MixerCommand::SetOutputRatio(_, warm_ratio) = warm[0] else {
            panic!("expected SetOutputRatio");
        };

        // Several idle ticks with unrelated fill values must produce nothing.
        for f in [0.1, 0.9, 0.5] {
            let idle = ctrl.tick(&[(OutputId(0), FillSample { fill: f, active: false })]);
            assert!(idle.is_empty(), "idle output must not emit a command");
        }

        // Resuming with the same fill as before the idle gap must reproduce the
        // same ratio — proof the integrator did not move while idle.
        let resumed = ctrl.tick(&[(OutputId(0), active(0.62))]);
        let MixerCommand::SetOutputRatio(_, resumed_ratio) = resumed[0] else {
            panic!("expected SetOutputRatio");
        };
        assert_eq!(resumed_ratio.value(), warm_ratio.value());
    }

    #[test]
    fn outputs_correct_independently() {
        // Two DACs, two crystals, two independent errors. This is the axis
        // that genuinely varies — unlike two groups on ONE output, whose
        // capture streams share a clock and must share a ratio.
        let mut ctrl = DriftController::new(&[OutputId(0), OutputId(1)], cfg());
        let cmds = ctrl.tick(&[(OutputId(0), active(0.9)), (OutputId(1), active(0.1))]);
        assert_eq!(cmds.len(), 2);
        let ratio_of = |id: OutputId| {
            cmds.iter()
                .find_map(|c| match c {
                    MixerCommand::SetOutputRatio(cid, r) if *cid == id => Some(r.value()),
                    _ => None,
                })
                .unwrap()
        };
        assert!(ratio_of(OutputId(0)) < 1.0, "overfull (0.9) must produce less output");
        assert!(ratio_of(OutputId(1)) > 1.0, "underfull (0.1) must produce more output");
    }

    #[test]
    fn unregistered_output_is_ignored_not_panicking() {
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        let cmds = ctrl.tick(&[(OutputId(99), active(0.9))]);
        assert!(cmds.is_empty());
    }

    #[test]
    fn src_produces_fewer_frames_at_a_lower_ratio() {
        // B7's sign pinned against `Src`'s MEASURED behaviour, not just
        // asserted from a reading of resample.rs (operational learnings:
        // a design doc's stated mechanism has been wrong before even when
        // the fix was right).
        use audio_core::{Format, Src};
        let fmt = Format {
            sample_rate: 48_000,
            channels: 2,
            layout: audio_core::ChannelLayout::STEREO,
        };
        let block = 256;
        let input = vec![0.0f32; block * 2];
        let mut output = vec![0.0f32; block * 2 * 4];

        let mut low = Src::new(fmt, fmt, block).unwrap();
        low.set_ratio(ResampleRatio::new(0.99).unwrap());
        let mut produced_low = 0usize;
        for _ in 0..60 {
            produced_low += low.process(&input, &mut output).produced;
        }

        let mut high = Src::new(fmt, fmt, block).unwrap();
        high.set_ratio(ResampleRatio::new(1.01).unwrap());
        let mut produced_high = 0usize;
        for _ in 0..60 {
            produced_high += high.process(&input, &mut output).produced;
        }

        assert!(
            produced_low < produced_high,
            "a lower ratio must yield fewer output frames than a higher one — \
             the physical fact the drift correction's sign depends on \
             (low={produced_low}, high={produced_high})"
        );
    }

    #[test]
    fn a_saturated_capture_ring_drives_the_ratio_down_not_up() {
        // The bug this loop was moved to fix, as a control-law assertion.
        // A capture ring pinned at its brim (measured: 0.83–0.97 while
        // `capture_drops` climbed ~192/s) must be corrected by consuming its
        // input FASTER. Input consumed/sec = R_out / ratio, so faster means a
        // LOWER ratio — the direction that reads backwards if you think of
        // ratio only as "how much output the resampler emits".
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        let cmds = ctrl.tick(&[(OutputId(0), active(0.97))]);
        let MixerCommand::SetOutputRatio(_, ratio) = cmds[0] else {
            panic!("expected SetOutputRatio");
        };
        assert!(
            ratio.value() < 1.0,
            "a brim-pinned capture ring must drain faster, i.e. ratio below 1, got {}",
            ratio.value()
        );
    }
}
