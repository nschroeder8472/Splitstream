//! Setup state for the sink endpoint routed apps are parked on
//! (double-audio-prevention L4), plus the two config-shape checks that share
//! its subject: whether anything can still be heard once the Windows default
//! points at a sink. All pure — no device access, no config I/O; the caller
//! supplies the facts, this decides what they mean.

use engine::ConfigSnapshot;

/// Whether the configured sink is present and actually in effect. The single
/// derived value capabilities 3 (guided setup) and 6 (honest failure) both
/// render.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkStatus {
    /// No `[app] sink_device` — first run, or the user cleared it.
    NotConfigured,
    /// Configured but absent from the device list (not installed, unplugged,
    /// removed). Apps play through whatever Windows picked instead.
    Missing { configured: String },
    /// Present, but Windows' default is something else — apps still render
    /// somewhere audible, so double-audio is live. Carries the current
    /// default so the UI can name what to change.
    NotDefault {
        sink: String,
        current_default: Option<String>,
    },
    /// Present and is the current default. Normal operation.
    Active { sink: String },
}

/// Device names are compared exactly as Windows reports them — the same basis
/// as the shell's `default_output_name` and `available_devices`, both of which
/// carry `Endpoint::name` verbatim.
pub fn resolve_sink_status(
    configured_sink: Option<&str>,
    available_devices: &[String],
    current_default: Option<&str>,
) -> SinkStatus {
    let Some(sink) = configured_sink else {
        return SinkStatus::NotConfigured;
    };
    if !available_devices.iter().any(|d| d == sink) {
        return SinkStatus::Missing {
            configured: sink.to_string(),
        };
    }
    if current_default == Some(sink) {
        return SinkStatus::Active {
            sink: sink.to_string(),
        };
    }
    SinkStatus::NotDefault {
        sink: sink.to_string(),
        current_default: current_default.map(str::to_string),
    }
}

/// True when quitting right now would leave the machine on the sink with
/// nothing to hand back — the sink is the Windows default, and Splitstream
/// holds no record of what preceded it.
///
/// The two situations that produce this are indistinguishable from here: a
/// user who set the sink as their own default outside Splitstream, and a
/// previous exit that left the default moved without recording it. Guessing a
/// device to restore to would be inventing an answer, so this exists to let
/// the UI *ask* instead — the alternative is a machine that goes silent on
/// quit with no in-app way out.
///
/// Note this is deliberately not a `SinkStatus` variant: the status is a pure
/// function of the three device facts, while this also depends on what has
/// been recorded, which is a different question about the same moment.
pub fn quit_would_strand(previous_default: Option<&str>, status: &SinkStatus) -> bool {
    matches!(status, SinkStatus::Active { .. }) && previous_default.is_none()
}

/// True when no group carries a `*` catch-all rule (capability 2). With the
/// Windows default pointed at the sink, an app that matches nothing is
/// *inaudible*, not merely unprocessed — so the absence of a catch-all stops
/// being a preference and becomes the failure mode. First-run onboarding
/// always creates one; this catches a later edit removing it.
pub fn lacks_catch_all(snapshot: &ConfigSnapshot) -> bool {
    !snapshot
        .groups
        .iter()
        .any(|g| g.match_rules.iter().any(|r| r == "*"))
}

/// Groups whose output device *is* the sink. Nothing stops a user picking the
/// same device in two independent pickers, and this pairing is silently
/// self-defeating: the group renders into the endpoint nobody listens to, so
/// it simply goes quiet. No feedback loop results — routing's self-exclusion
/// keeps Splitstream from capturing its own render — which is exactly why
/// there is no error to notice.
pub fn groups_outputting_to_sink<'a>(snapshot: &'a ConfigSnapshot, sink: Option<&str>) -> Vec<&'a str> {
    let Some(sink) = sink else { return Vec::new() };
    snapshot
        .groups
        .iter()
        .filter(|g| g.output_device == sink)
        .map(|g| g.name.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core::Gain;
    use engine::GroupConfig;

    fn devices(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn snapshot_with_group(name: &str, output_device: &str, match_rules: &[&str]) -> ConfigSnapshot {
        ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            muted: false,
            groups: vec![GroupConfig {
                name: name.into(),
                output_device: output_device.into(),
                gain: Gain::UNITY,
                follow_master: true,
                match_rules: match_rules.iter().map(|r| r.to_string()).collect(),
                dsp: Vec::new(),
                duck: None,
                spatial: false,
                muted: false,
                hotkey_mute: None,
                hotkey_volume_up: None,
                hotkey_volume_down: None,
            }],
            app: engine::AppConfig::default(),
            profiles: Vec::new(),
        }
    }

    #[test]
    fn resolve_sink_status_reports_not_configured_when_no_sink_is_set() {
        let available = devices(&["Headphones", "CABLE Input"]);

        let status = resolve_sink_status(None, &available, Some("Headphones"));

        assert_eq!(status, SinkStatus::NotConfigured);
    }

    #[test]
    fn resolve_sink_status_reports_missing_when_the_sink_is_absent() {
        let available = devices(&["Headphones"]);

        let status = resolve_sink_status(Some("CABLE Input"), &available, Some("Headphones"));

        assert_eq!(
            status,
            SinkStatus::Missing {
                configured: "CABLE Input".to_string()
            }
        );
    }

    #[test]
    fn resolve_sink_status_reports_not_default_and_names_the_current_default() {
        let available = devices(&["Headphones", "CABLE Input"]);

        let status = resolve_sink_status(Some("CABLE Input"), &available, Some("Headphones"));

        assert_eq!(
            status,
            SinkStatus::NotDefault {
                sink: "CABLE Input".to_string(),
                current_default: Some("Headphones".to_string()),
            }
        );
    }

    #[test]
    fn resolve_sink_status_reports_active_when_the_sink_is_the_default() {
        let available = devices(&["Headphones", "CABLE Input"]);

        let status = resolve_sink_status(Some("CABLE Input"), &available, Some("CABLE Input"));

        assert_eq!(
            status,
            SinkStatus::Active {
                sink: "CABLE Input".to_string()
            }
        );
    }

    /// A present sink with no known default is `NotDefault`, not `Active` —
    /// "we couldn't read the default" must never be reported as "the sink is
    /// in effect" (capability 6: every failure in this area to date has been
    /// invisible).
    #[test]
    fn resolve_sink_status_reports_not_default_when_the_current_default_is_unknown() {
        let available = devices(&["CABLE Input"]);

        let status = resolve_sink_status(Some("CABLE Input"), &available, None);

        assert_eq!(
            status,
            SinkStatus::NotDefault {
                sink: "CABLE Input".to_string(),
                current_default: None,
            }
        );
    }

    // --- stranding: would quitting leave the machine silent? --------------

    /// Reproduces the state a user actually reached on hardware 2026-07-26:
    /// the sink was the default, the record had been cleared, and quitting
    /// left the machine silent with nothing in the app able to undo it.
    #[test]
    fn the_sink_being_default_with_nothing_recorded_would_strand_on_quit() {
        let status = SinkStatus::Active {
            sink: "CABLE Input".into(),
        };

        assert!(quit_would_strand(None, &status));
    }

    #[test]
    fn a_recorded_previous_default_means_quitting_restores_it() {
        let status = SinkStatus::Active {
            sink: "CABLE Input".into(),
        };

        assert!(!quit_would_strand(Some("Headphones"), &status));
    }

    /// Nothing to strand on when the sink isn't in effect — apps are still
    /// reaching a real device, which is the double-audio complaint, not the
    /// silence one.
    #[test]
    fn a_sink_that_is_not_the_default_cannot_strand() {
        let status = SinkStatus::NotDefault {
            sink: "CABLE Input".into(),
            current_default: Some("Headphones".into()),
        };

        assert!(!quit_would_strand(None, &status));
    }

    #[test]
    fn an_unconfigured_or_missing_sink_cannot_strand() {
        assert!(!quit_would_strand(None, &SinkStatus::NotConfigured));
        assert!(!quit_would_strand(
            None,
            &SinkStatus::Missing {
                configured: "CABLE Input".into()
            }
        ));
    }

    // --- capability 2: is anything still audible? -------------------------

    #[test]
    fn a_config_whose_groups_have_no_catch_all_rule_is_flagged() {
        let snapshot = snapshot_with_group("Game", "Headphones", &["game.exe"]);

        assert!(lacks_catch_all(&snapshot));
    }

    #[test]
    fn a_config_with_a_catch_all_rule_is_not_flagged() {
        let snapshot = snapshot_with_group("Game", "Headphones", &["*"]);

        assert!(!lacks_catch_all(&snapshot));
    }

    /// The two device pickers are independent, so nothing stops both landing
    /// on the same endpoint — and the result is a group that simply goes
    /// quiet, with no error raised anywhere.
    #[test]
    fn a_group_pointed_at_the_sink_device_is_flagged() {
        let snapshot = snapshot_with_group("Game", "CABLE Input", &["*"]);

        let offenders = groups_outputting_to_sink(&snapshot, Some("CABLE Input"));

        assert_eq!(offenders, vec!["Game"]);
    }

    #[test]
    fn groups_on_real_devices_are_not_flagged() {
        let snapshot = snapshot_with_group("Game", "Headphones", &["*"]);

        assert!(groups_outputting_to_sink(&snapshot, Some("CABLE Input")).is_empty());
    }

    #[test]
    fn no_configured_sink_flags_no_groups() {
        let snapshot = snapshot_with_group("Game", "Headphones", &["*"]);

        assert!(groups_outputting_to_sink(&snapshot, None).is_empty());
    }

    /// `Missing` outranks `NotDefault`: a sink that isn't there can't be made
    /// the default, so the UI must say "install it", not "switch to it".
    #[test]
    fn resolve_sink_status_reports_missing_even_when_it_is_named_as_the_default() {
        let available = devices(&["Headphones"]);

        let status = resolve_sink_status(Some("CABLE Input"), &available, Some("CABLE Input"));

        assert_eq!(
            status,
            SinkStatus::Missing {
                configured: "CABLE Input".to_string()
            }
        );
    }
}
