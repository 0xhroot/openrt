//! Temporal correlation (explicitly not causation) and what-if scenarios
//! whose statements are split into KNOWN / SIMULATED / ASSUMED. Scenarios
//! are advisory — no command, control path or safety bypass exists here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::Asset;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Alarm,
    Incident,
    WorkOrder,
    Trend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedEvent {
    pub kind: EventKind,
    pub id: String,
    pub at_ms: i64,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrelateParams {
    /// Events closer than this are grouped together.
    pub gap_ms: i64,
    pub min_members: usize,
    pub max_groups: usize,
}

impl Default for CorrelateParams {
    fn default() -> Self {
        Self {
            gap_ms: 3_600_000,
            min_members: 2,
            max_groups: 50,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    pub kind: EventKind,
    pub id: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalGroup {
    pub group_id: String,
    pub window_from_ms: i64,
    pub window_to_ms: i64,
    pub members: Vec<GroupMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrelationResult {
    pub groups: Vec<TemporalGroup>,
    pub params: CorrelateParams,
    pub events_considered: usize,
    pub caveat: String,
    pub generated_at: String,
}

/// Sweep-line grouping of events whose gaps stay within `gap_ms`.
pub fn correlate(events: &[TimedEvent], params: CorrelateParams) -> CorrelationResult {
    let mut sorted: Vec<&TimedEvent> = events.iter().collect();
    sorted.sort_by_key(|e| e.at_ms);
    let mut groups: Vec<TemporalGroup> = Vec::new();
    let mut current: Vec<&TimedEvent> = Vec::new();
    let mut window_start: i64 = 0;
    let mut last_t: i64 = 0;
    for e in sorted {
        if current.is_empty() {
            current.push(e);
            window_start = e.at_ms;
            last_t = e.at_ms;
            continue;
        }
        if e.at_ms - last_t <= params.gap_ms {
            current.push(e);
            last_t = e.at_ms;
        } else {
            if current.len() >= params.min_members {
                groups.push(TemporalGroup {
                    group_id: format!("grp.{}", groups.len()),
                    window_from_ms: window_start,
                    window_to_ms: last_t,
                    members: current
                        .iter()
                        .map(|e| GroupMember {
                            kind: e.kind,
                            id: e.id.clone(),
                            at_ms: e.at_ms,
                        })
                        .collect(),
                });
            }
            if groups.len() >= params.max_groups {
                break;
            }
            current.clear();
            current.push(e);
            window_start = e.at_ms;
            last_t = e.at_ms;
        }
    }
    if current.len() >= params.min_members && groups.len() < params.max_groups {
        groups.push(TemporalGroup {
            group_id: format!("grp.{}", groups.len()),
            window_from_ms: window_start,
            window_to_ms: last_t,
            members: current
                .iter()
                .map(|e| GroupMember {
                    kind: e.kind,
                    id: e.id.clone(),
                    at_ms: e.at_ms,
                })
                .collect(),
        });
    }
    CorrelationResult {
        groups,
        params,
        events_considered: events.len(),
        caveat: "temporal correlation only — co-occurrence does not establish causation".into(),
        generated_at: openrt_core::now().to_rfc3339(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    Known,
    Simulated,
    Assumed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    pub kind: StatementKind,
    pub text: String,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioInput {
    pub asset: Asset,
    /// Event under consideration, e.g. `failure`, `sensor_loss`, `overhaul`.
    pub event: String,
    pub assumptions: Vec<String>,
    pub edges: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub asset_id: String,
    pub event: String,
    pub statements: Vec<Statement>,
    pub rules_applied: Vec<String>,
    pub simulated: bool,
    pub advisory_only: bool,
    pub generated_at: String,
}

/// Evaluate a scenario. `known` comes from the graph, `simulated` from
/// documented propagation rules, `assumed` from the caller.
pub fn scenario(input: &ScenarioInput) -> ScenarioResult {
    let mut statements: Vec<Statement> = Vec::new();
    let mut rules: Vec<String> = Vec::new();

    if let Some(unit) = &input.asset.unit {
        statements.push(Statement {
            kind: StatementKind::Known,
            text: format!("asset `{}` belongs to unit `{}`", input.asset.id, unit),
            basis: "asset record".into(),
        });
    }
    if let Some(parent) = &input.asset.parent {
        statements.push(Statement {
            kind: StatementKind::Known,
            text: format!("asset `{}` is contained by `{}`", input.asset.id, parent),
            basis: "asset record".into(),
        });
    }
    for (s, p, o) in &input.edges {
        statements.push(Statement {
            kind: StatementKind::Known,
            text: format!("`{s}` --{p}--> `{o}`"),
            basis: "knowledge graph".into(),
        });
    }

    match input.event.as_str() {
        "failure" => {
            rules.push("asset_failure.degrades_parent".into());
            if let Some(unit) = &input.asset.unit {
                statements.push(Statement {
                    kind: StatementKind::Simulated,
                    text: format!(
                        "unit `{unit}` availability would degrade while `{}` is failed",
                        input.asset.id
                    ),
                    basis: "rule asset_failure.degrades_parent".into(),
                });
            }
            rules.push("asset_failure.upstream_alarm_possible".into());
            statements.push(Statement {
                kind: StatementKind::Simulated,
                text: format!(
                    "downstream sensors of `{}` may alarm on out-of-band readings",
                    input.asset.id
                ),
                basis: "rule asset_failure.upstream_alarm_possible".into(),
            });
        }
        "sensor_loss" => {
            rules.push("sensor_loss.blind_control_loop".into());
            statements.push(Statement {
                kind: StatementKind::Simulated,
                text: format!(
                    "control loops consuming `{}` lose visibility; operators are notified",
                    input.asset.id
                ),
                basis: "rule sensor_loss.blind_control_loop".into(),
            });
        }
        "overhaul" => {
            rules.push("overhaul.takes_unit_offline".into());
            if let Some(unit) = &input.asset.unit {
                statements.push(Statement {
                    kind: StatementKind::Simulated,
                    text: format!("unit `{unit}` would be unavailable for the overhaul window"),
                    basis: "rule overhaul.takes_unit_offline".into(),
                });
            }
        }
        other => {
            rules.push("unmatched_event.no_propagation".into());
            statements.push(Statement {
                kind: StatementKind::Simulated,
                text: format!("no propagation rules matched event `{other}` — nothing simulated"),
                basis: "rule unmatched_event.no_propagation".into(),
            });
        }
    }

    for a in &input.assumptions {
        statements.push(Statement {
            kind: StatementKind::Assumed,
            text: a.clone(),
            basis: "caller-provided assumption".into(),
        });
    }

    ScenarioResult {
        asset_id: input.asset.id.clone(),
        event: input.event.clone(),
        statements,
        rules_applied: rules,
        simulated: true,
        advisory_only: true,
        generated_at: openrt_core::now().to_rfc3339(),
    }
}

/// Group related assets by a labelled edge into a map for copilot context.
pub fn group_edges(edges: &[(String, String, String)]) -> BTreeMap<String, Vec<(String, String)>> {
    let mut m: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (s, p, o) in edges {
        m.entry(s.clone()).or_default().push((p.clone(), o.clone()));
        m.entry(o.clone()).or_default().push((p.clone(), s.clone()));
    }
    m
}
