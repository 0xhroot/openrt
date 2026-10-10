//! Evidence-linked investigation answers (copilot): structured sections
//! over already-observed records with explicit evidence references. Every
//! section points at record ids — no uncited claims.

use serde::{Deserialize, Serialize};

use crate::anomaly::DetectorRun;
use crate::fmea::{fmea_table, FailureMode};
use crate::model::Asset;
use crate::scenario::TemporalGroup;
use crate::trend::TrendResult;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerSection {
    pub section: String,
    pub text: String,
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RelatedBundle {
    pub alarms: Vec<Asset>,
    pub incidents: Vec<Asset>,
    pub work_orders: Vec<Asset>,
    pub inspections: Vec<Asset>,
    pub procedures: Vec<Asset>,
    pub trends: Vec<TrendResult>,
    pub detector_runs: Vec<DetectorRun>,
    pub neighbors: Vec<(String, String, String)>,
    pub correlation_groups: Vec<TemporalGroup>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Investigation {
    pub asset_id: String,
    pub generated_at: String,
    pub sections: Vec<AnswerSection>,
    pub advisory: String,
}

fn refs(kind: &str, ids: &[String]) -> Vec<EvidenceRef> {
    ids.iter()
        .map(|id| EvidenceRef {
            kind: kind.into(),
            id: id.clone(),
        })
        .collect()
}

/// Build an investigation answer for one asset from its observed context.
pub fn investigate(asset: &Asset, related: &RelatedBundle) -> Investigation {
    let mut sections = Vec::new();

    let status_text = format!(
        "{} `{}` ({}) status={}, kind={}",
        asset.kind.as_str(),
        asset.id,
        asset.label,
        asset.status.as_deref().unwrap_or("unspecified"),
        asset.kind.as_str()
    );
    sections.push(AnswerSection {
        section: "asset_status".into(),
        text: status_text,
        evidence: vec![EvidenceRef {
            kind: "asset".into(),
            id: asset.id.clone(),
        }],
    });

    let alarm_ids: Vec<String> = related.alarms.iter().map(|a| a.id.clone()).collect();
    let anomalies: usize = related
        .detector_runs
        .iter()
        .map(|r| r.anomalies.len())
        .sum();
    sections.push(AnswerSection {
        section: "active_signals".into(),
        text: format!(
            "{} active alarm(s), {} detector anomaly(ies) across {} detector run(s), {} trend(s) evaluated",
            alarm_ids.len(),
            anomalies,
            related.detector_runs.len(),
            related.trends.len()
        ),
        evidence: {
            let mut r = refs("alarm", &alarm_ids);
            for run in &related.detector_runs {
                r.push(EvidenceRef { kind: "detector_run".into(), id: format!("{}:{}", run.method.as_str(), run.series_id) });
            }
            r
        },
    });

    let wo_ids: Vec<String> = related.work_orders.iter().map(|w| w.id.clone()).collect();
    let insp_ids: Vec<String> = related.inspections.iter().map(|i| i.id.clone()).collect();
    sections.push(AnswerSection {
        section: "maintenance_history".into(),
        text: format!(
            "{} work order(s), {} inspection record(s) linked to this asset",
            wo_ids.len(),
            insp_ids.len()
        ),
        evidence: {
            let mut r = refs("work_order", &wo_ids);
            r.extend(refs("inspection", &insp_ids));
            r
        },
    });

    let proc_ids: Vec<String> = related.procedures.iter().map(|p| p.id.clone()).collect();
    sections.push(AnswerSection {
        section: "applicable_procedures".into(),
        text: if proc_ids.is_empty() {
            "no procedure records linked".into()
        } else {
            format!(
                "{} procedure(s) apply: {}",
                proc_ids.len(),
                proc_ids.join(", ")
            )
        },
        evidence: refs("procedure", &proc_ids),
    });

    let mut fmea: Vec<FailureMode> = Vec::new();
    if asset.kind == crate::model::AssetKind::Equipment {
        fmea = fmea_table(asset.equipment_type());
    }
    sections.push(AnswerSection {
        section: "known_failure_modes".into(),
        text: if fmea.is_empty() {
            "no equipment FMEA entry for this asset kind".into()
        } else {
            fmea.iter()
                .map(|m| format!("{} [{}]", m.mode, m.indicators.join(", ")))
                .collect::<Vec<_>>()
                .join("; ")
        },
        evidence: vec![EvidenceRef {
            kind: "fmea".into(),
            id: asset.equipment_type().into(),
        }],
    });

    let neighbor_lines: Vec<String> = related
        .neighbors
        .iter()
        .filter(|(s, _, o)| s == &asset.id || o == &asset.id)
        .map(|(s, p, o)| format!("{s} --{p}--> {o}"))
        .collect();
    sections.push(AnswerSection {
        section: "graph_context".into(),
        text: if neighbor_lines.is_empty() {
            "no graph relations touch this asset".into()
        } else {
            neighbor_lines.join("; ")
        },
        evidence: refs(
            "relation",
            &neighbor_lines.iter().map(|l| l.clone()).collect::<Vec<_>>(),
        ),
    });

    let checks: Vec<String> = fmea
        .iter()
        .flat_map(|m| {
            m.indicators
                .iter()
                .map(|i| format!("check: {i} (mode: {})", m.mode))
        })
        .take(10)
        .collect();
    sections.push(AnswerSection {
        section: "recommended_next_checks".into(),
        text: if checks.is_empty() {
            "review active alarms and trend windows before acting".into()
        } else {
            checks.join("; ")
        },
        evidence: vec![EvidenceRef {
            kind: "fmea".into(),
            id: asset.equipment_type().into(),
        }],
    });

    Investigation {
        asset_id: asset.id.clone(),
        generated_at: openrt_core::now().to_rfc3339(),
        sections,
        advisory: "advisory only — no control action is taken by OpenRT".into(),
    }
}
