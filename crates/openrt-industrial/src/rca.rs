//! Bounded root-cause analysis: every finding carries an epistemic label
//! (observed fact / inferred relation / hypothesis / unknown) and concrete
//! evidence references. Reports are capped — no unbounded causal graphs.

use serde::{Deserialize, Serialize};

use crate::anomaly::{AnomalyResult, Severity};
use crate::fmea::FailureMode;
use crate::model::{Asset, AssetKind};
use crate::trend::{TrendDirection, TrendResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EvidenceLabel {
    ObservedFact,
    InferredRelation,
    Hypothesis,
    Unknown,
}

impl EvidenceLabel {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceLabel::ObservedFact => "OBSERVED_FACT",
            EvidenceLabel::InferredRelation => "INFERRED_RELATION",
            EvidenceLabel::Hypothesis => "HYPOTHESIS",
            EvidenceLabel::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub label: EvidenceLabel,
    pub statement: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RcaBounds {
    pub max_findings: usize,
    pub max_depth: usize,
    pub temporal_window_ms: i64,
}

impl Default for RcaBounds {
    fn default() -> Self {
        Self {
            max_findings: 24,
            max_depth: 3,
            temporal_window_ms: 3_600_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RcaInputs {
    pub alarms: usize,
    pub incidents: usize,
    pub work_orders: usize,
    pub trend_points: usize,
    pub anomaly_points: usize,
    pub graph_edges: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RcaReport {
    pub subject: String,
    pub findings: Vec<Finding>,
    pub inputs: RcaInputs,
    pub bounds: RcaBounds,
    /// Describes exactly what was fed in — inputs are hashed summaries, not
    /// copied sensor data.
    pub input_digest: String,
    pub generated_at: String,
    pub caveat: String,
}

/// Everything the report builder consumes. All of it is caller-observed
/// data — nothing here is synthesised.
#[derive(Debug, Clone, Default)]
pub struct RcaContext {
    pub alarms: Vec<Asset>,
    pub incidents: Vec<Asset>,
    pub work_orders: Vec<Asset>,
    pub trends: Vec<TrendResult>,
    pub anomalies: Vec<AnomalyResult>,
    /// `(subject, predicate, object)` triples touching the subject.
    pub edges: Vec<(String, String, String)>,
    pub fmea: Vec<FailureMode>,
    pub bounds: RcaBounds,
}

fn inputs_of(ctx: &RcaContext) -> RcaInputs {
    RcaInputs {
        alarms: ctx.alarms.len(),
        incidents: ctx.incidents.len(),
        work_orders: ctx.work_orders.len(),
        trend_points: ctx.trends.iter().map(|t| t.points_used).sum(),
        anomaly_points: ctx.anomalies.len(),
        graph_edges: ctx.edges.len(),
    }
}

fn parse_ms(ts: &str) -> Option<i64> {
    ts.parse::<openrt_core::Timestamp>()
        .ok()
        .map(|t| t.timestamp_millis())
}

/// Build a bounded RCA report for `subject` from observed context.
pub fn root_cause(subject: &str, ctx: &RcaContext) -> RcaReport {
    let bounds = if ctx.bounds.max_findings == 0 {
        RcaBounds::default()
    } else {
        ctx.bounds
    };
    let inputs = inputs_of(ctx);
    let digest_src = format!(
        "{}|{:?}|{:?}|{:?}|{:?}",
        subject, inputs, ctx.alarms, ctx.work_orders, ctx.edges
    );
    let input_digest = openrt_core::sha256_hex(digest_src.as_bytes());
    let mut findings: Vec<Finding> = Vec::new();
    let push = |f: Finding, findings: &mut Vec<Finding>| {
        if findings.len() < bounds.max_findings
            && !findings.iter().any(|x| x.statement == f.statement)
        {
            findings.push(f);
        }
    };

    for a in &ctx.anomalies {
        push(
            Finding {
                label: EvidenceLabel::ObservedFact,
                statement: format!(
                    "detector `{}` flagged value {:.4} outside band [{:.4}, {:.4}] at t_ms={}",
                    a.detector_id, a.value, a.expected.low, a.expected.high, a.at_ms
                ),
                evidence: vec![
                    format!("series:{}", a.series_id),
                    format!("method:{}", a.method.as_str()),
                ],
            },
            &mut findings,
        );
    }
    for alarm in &ctx.alarms {
        push(
            Finding {
                label: EvidenceLabel::ObservedFact,
                statement: format!(
                    "alarm `{}` active with severity `{}`",
                    alarm.id,
                    alarm.alarm_severity()
                ),
                evidence: vec![format!("asset:{}", alarm.id)],
            },
            &mut findings,
        );
    }
    for t in &ctx.trends {
        if matches!(
            t.direction,
            TrendDirection::Rising | TrendDirection::Falling
        ) {
            push(
                Finding {
                    label: EvidenceLabel::ObservedFact,
                    statement: format!(
                        "series `{}` trend is {} (slope {:?}/point over {} points)",
                        t.series_id,
                        t.direction.as_str(),
                        t.slope_per_point.map(|s| format!("{s:.4}")),
                        t.points_used
                    ),
                    evidence: vec![format!("series:{}", t.series_id)],
                },
                &mut findings,
            );
        }
    }

    let subject_alarm_ms = ctx
        .alarms
        .first()
        .and_then(|a| a.attr_str("first_seen").and_then(parse_ms));
    for inc in &ctx.incidents {
        let inc_ms = inc.attr_str("occurred_at").and_then(parse_ms);
        if let (Some(ia), Some(sa)) = (inc_ms, subject_alarm_ms) {
            if (ia - sa).abs() <= bounds.temporal_window_ms {
                push(
                    Finding {
                        label: EvidenceLabel::InferredRelation,
                        statement: format!(
                            "incident `{}` occurred within {} ms of alarm activity (temporal proximity only)",
                            inc.id,
                            (ia - sa).abs()
                        ),
                        evidence: vec![format!("asset:{}", inc.id)],
                    },
                    &mut findings,
                );
            }
        }
    }
    for (s, p, o) in &ctx.edges {
        if s == subject || o == subject {
            push(
                Finding {
                    label: EvidenceLabel::InferredRelation,
                    statement: format!("graph: `{s}` --{p}--> `{o}`"),
                    evidence: vec![format!("relation:{s}:{p}:{o}")],
                },
                &mut findings,
            );
        }
    }

    for mode in &ctx.fmea {
        push(
            Finding {
                label: EvidenceLabel::Hypothesis,
                statement: format!(
                    "possible failure mode `{}` ({})",
                    mode.mode,
                    mode.possible_causes.join("; ")
                ),
                evidence: vec![format!("fmea:{}", mode.equipment_type)],
            },
            &mut findings,
        );
    }

    if !findings
        .iter()
        .any(|f| f.label == EvidenceLabel::ObservedFact)
    {
        findings.push(Finding {
            label: EvidenceLabel::Unknown,
            statement: "no observed facts available for this subject — root cause unknown".into(),
            evidence: Vec::new(),
        });
    }

    findings.truncate(bounds.max_findings);
    RcaReport {
        subject: subject.to_string(),
        findings,
        inputs,
        bounds,
        input_digest,
        generated_at: openrt_core::now().to_rfc3339(),
        caveat: "labels distinguish observation from inference; hypotheses are unproven".into(),
    }
}

/// Helper: severity of the worst anomaly, if any.
pub fn worst_severity(anomalies: &[AnomalyResult]) -> Option<Severity> {
    anomalies
        .iter()
        .map(|a| a.severity)
        .max_by_key(|s| match s {
            Severity::High => 2,
            Severity::Medium => 1,
            Severity::Low => 0,
        })
}

/// Assets of a kind among related records — small helper for report input.
pub fn of_kind<'a>(assets: &'a [Asset], kind: AssetKind) -> Vec<&'a Asset> {
    assets.iter().filter(|a| a.kind == kind).collect()
}
