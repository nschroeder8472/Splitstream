//! Ring-fill PI control loop (drift-and-recovery L4, notes §10). Pure — no
//! threads, no OS — so it's unit-testable with synthetic fill curves before
//! ever touching real hardware. Runs on the recovery-supervisor control
//! thread at `DriftConfig::tick` cadence (~100 ms); RT threads only publish
//! fill via atomics upstream, `tick` emits `MixerCommand::SetOutputRatio`
//! for the RT mixer thread to apply.

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

/// Per-output fill measurement for one `tick`. `active` is the idle guard
/// (drift-and-recovery revision): a silent bus produces no loopback packets,
/// so without this the integrator winds up on silence and pegs the ratio.
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

            // ring too full (err > 0) -> corr > 0 -> ratio > 1 -> resampler consumes faster.
            let ratio = ResampleRatio::new(1.0 + corr)
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
    fn overfull_ring_produces_a_ratio_above_one() {
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        let cmds = ctrl.tick(&[(OutputId(0), active(0.62))]);
        let MixerCommand::SetOutputRatio(id, ratio) = cmds[0] else {
            panic!("expected SetOutputRatio");
        };
        assert_eq!(id, OutputId(0));
        assert!(ratio.value() > 1.0, "overfull ring must speed up consumption");
        assert!(ratio.value() <= 1.0 + cfg().max_correction);
    }

    #[test]
    fn underfull_ring_produces_a_ratio_below_one() {
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        let cmds = ctrl.tick(&[(OutputId(0), active(0.38))]);
        let MixerCommand::SetOutputRatio(_, ratio) = cmds[0] else {
            panic!("expected SetOutputRatio");
        };
        assert!(ratio.value() < 1.0, "underfull ring must slow down consumption");
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
            let corr = (ratio.value() - 1.0) as f32;
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
        assert!(ratio_of(OutputId(0)) > 1.0);
        assert!(ratio_of(OutputId(1)) < 1.0);
    }

    #[test]
    fn unregistered_output_is_ignored_not_panicking() {
        let mut ctrl = DriftController::new(&[OutputId(0)], cfg());
        let cmds = ctrl.tick(&[(OutputId(99), active(0.9))]);
        assert!(cmds.is_empty());
    }
}
