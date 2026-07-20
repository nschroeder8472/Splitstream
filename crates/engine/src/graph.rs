//! Config → resolved graph. Owns the pre-resolution config shape
//! (`ConfigSnapshot`/`GroupConfig`) since `engine` is the consumer that
//! resolves group/device names into endpoint ids — see the "Config type
//! home" decision in `.lattice/context/engine-core.md`: `control` (a later
//! layer) depends on `engine` for this type, not the other way around, so
//! `engine` can be built and tested before `control` exists.

use std::collections::HashSet;

use audio_core::{Gain, GroupId, GroupSpec, OutputId, OutputSpec, Topology};

use crate::ports::{Endpoint, EndpointId, EndpointKind};
use crate::runtime::EngineError;

#[derive(Clone)]
pub struct ConfigSnapshot {
    pub schema_version: u32,
    pub master: Gain,
    pub groups: Vec<GroupConfig>,
}

#[derive(Clone)]
pub struct GroupConfig {
    pub name: String,
    pub bus_endpoint: String,
    pub output_device: String,
    pub gain: Gain,
    pub follow_master: bool,
    /// Raw config strings — parsed into `rules::MatchRule` by `control::group_rules`,
    /// not here (this type only mirrors the TOML shape; see session-routing.md).
    pub match_rules: Vec<String>,
}

/// Config with names resolved to endpoint ids, ready for `runtime` to open
/// ports and build the `Mixer` from.
pub struct GraphPlan {
    pub topology: Topology,
    pub group_endpoints: Vec<(GroupId, EndpointId)>,
    pub output_endpoints: Vec<(OutputId, EndpointId)>,
}

/// Resolves group/device names against the live endpoint list. Multiple
/// groups naming the same `output_device` share one `OutputId` (spec:
/// "shared outputs" — their audio sums cleanly at that one physical device).
///
/// `parked` names groups (drift-and-recovery: no fallback device available)
/// to exclude from the resulting graph entirely rather than erroring — the
/// group's `GroupId` stays reserved at its original index (`GroupId(i)`
/// comes from `snapshot.groups`' position, not a running counter), so
/// later groups keep stable ids across a rebuild that parks an earlier one.
pub fn resolve(
    snapshot: &ConfigSnapshot,
    endpoints: &[Endpoint],
    parked: &HashSet<String>,
) -> Result<GraphPlan, EngineError> {
    let find = |kind: EndpointKind, name: &str| {
        endpoints.iter().find(|e| e.kind == kind && e.name == name)
    };

    let mut groups = Vec::with_capacity(snapshot.groups.len());
    let mut group_endpoints = Vec::with_capacity(snapshot.groups.len());
    let mut outputs: Vec<OutputSpec> = Vec::new();
    let mut output_endpoints: Vec<(OutputId, EndpointId)> = Vec::new();
    let mut output_by_device: Vec<(&str, OutputId)> = Vec::new();

    for (i, g) in snapshot.groups.iter().enumerate() {
        let group_id = GroupId(i as u16);
        if parked.contains(&g.name) {
            continue;
        }

        let bus = find(EndpointKind::Bus, &g.bus_endpoint).ok_or_else(|| {
            EngineError::Resolve(format!(
                "group '{}': bus endpoint '{}' not found",
                g.name, g.bus_endpoint
            ))
        })?;

        let output_id = match output_by_device
            .iter()
            .find(|(name, _)| *name == g.output_device)
        {
            Some((_, id)) => *id,
            None => {
                let physical = find(EndpointKind::Physical, &g.output_device).ok_or_else(|| {
                    EngineError::Resolve(format!(
                        "group '{}': output device '{}' not found",
                        g.name, g.output_device
                    ))
                })?;
                let id = OutputId(outputs.len() as u16);
                outputs.push(OutputSpec {
                    id,
                    format: physical.format,
                });
                output_endpoints.push((id, physical.id.clone()));
                output_by_device.push((g.output_device.as_str(), id));
                id
            }
        };

        groups.push(GroupSpec {
            id: group_id,
            gain: g.gain,
            follow_master: g.follow_master,
            output: output_id,
            input_format: bus.format,
        });
        group_endpoints.push((group_id, bus.id.clone()));
    }

    Ok(GraphPlan {
        topology: Topology {
            master: snapshot.master,
            groups,
            outputs,
        },
        group_endpoints,
        output_endpoints,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core::{ChannelLayout, Format};

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
            layout: ChannelLayout::STEREO,
        }
    }

    fn endpoints() -> Vec<Endpoint> {
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
            Endpoint {
                id: EndpointId("out-2".into()),
                name: "Headphones".into(),
                kind: EndpointKind::Physical,
                format: stereo(48_000),
            },
        ]
    }

    fn no_parked() -> HashSet<String> {
        HashSet::new()
    }

    fn group(name: &str, bus: &str, output: &str) -> GroupConfig {
        GroupConfig {
            name: name.into(),
            bus_endpoint: bus.into(),
            output_device: output.into(),
            gain: Gain::UNITY,
            follow_master: true,
            match_rules: vec![],
        }
    }

    #[test]
    fn resolves_group_to_bus_and_output_ids() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            groups: vec![group("Game", "Game", "Speakers")],
        };
        let plan = resolve(&snapshot, &endpoints(), &no_parked()).unwrap();
        assert_eq!(plan.topology.groups.len(), 1);
        assert_eq!(plan.topology.outputs.len(), 1);
        assert_eq!(plan.group_endpoints[0].1, EndpointId("bus-1".into()));
        assert_eq!(plan.output_endpoints[0].1, EndpointId("out-1".into()));
    }

    #[test]
    fn two_groups_sharing_a_device_share_one_output_id() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            groups: vec![
                group("Game", "Game", "Speakers"),
                group("Music", "Game", "Speakers"),
            ],
        };
        let plan = resolve(&snapshot, &endpoints(), &no_parked()).unwrap();
        assert_eq!(
            plan.topology.outputs.len(),
            1,
            "one physical device, one OutputSpec"
        );
        assert_eq!(
            plan.topology.groups[0].output,
            plan.topology.groups[1].output
        );
    }

    #[test]
    fn missing_bus_endpoint_is_a_resolve_error() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            groups: vec![group("Game", "Nonexistent", "Speakers")],
        };
        assert!(matches!(
            resolve(&snapshot, &endpoints(), &no_parked()),
            Err(EngineError::Resolve(_))
        ));
    }

    #[test]
    fn missing_output_device_is_a_resolve_error() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            groups: vec![group("Game", "Game", "Nonexistent")],
        };
        assert!(matches!(
            resolve(&snapshot, &endpoints(), &no_parked()),
            Err(EngineError::Resolve(_))
        ));
    }

    #[test]
    fn parked_group_is_excluded_without_erroring() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            groups: vec![group("Game", "Game", "Nonexistent")],
        };
        let mut parked = HashSet::new();
        parked.insert("Game".to_string());
        let plan = resolve(&snapshot, &endpoints(), &parked).unwrap();
        assert!(plan.topology.groups.is_empty());
        assert!(plan.topology.outputs.is_empty());
    }

    #[test]
    fn parking_an_earlier_group_keeps_later_groups_ids_stable() {
        let snapshot = ConfigSnapshot {
            schema_version: 2,
            master: Gain::UNITY,
            groups: vec![
                group("Parked", "Game", "Speakers"),
                group("Music", "Game", "Headphones"),
            ],
        };
        let mut parked = HashSet::new();
        parked.insert("Parked".to_string());
        let plan = resolve(&snapshot, &endpoints(), &parked).unwrap();
        assert_eq!(plan.topology.groups.len(), 1);
        // "Music" is snapshot.groups[1] — its GroupId must stay GroupId(1)
        // even though "Parked" (index 0) was skipped, not renumbered to 0.
        assert_eq!(plan.topology.groups[0].id, GroupId(1));
    }
}
