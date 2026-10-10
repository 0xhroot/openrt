//! Industrial domain model: extensible asset records for plants, units,
//! equipment, sensors, alarms, processes, incidents, work orders,
//! inspections and procedures — plus provenance-carrying relations.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The entity kinds the industrial pack knows. Deployments differ: not every
/// plant has incidents or procedures, and `attrs` carries the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Plant,
    Unit,
    Equipment,
    Sensor,
    Alarm,
    Process,
    Incident,
    WorkOrder,
    Inspection,
    Procedure,
}

impl AssetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AssetKind::Plant => "plant",
            AssetKind::Unit => "unit",
            AssetKind::Equipment => "equipment",
            AssetKind::Sensor => "sensor",
            AssetKind::Alarm => "alarm",
            AssetKind::Process => "process",
            AssetKind::Incident => "incident",
            AssetKind::WorkOrder => "work_order",
            AssetKind::Inspection => "inspection",
            AssetKind::Procedure => "procedure",
        }
    }

    pub fn parse(s: &str) -> Option<AssetKind> {
        Some(match s {
            "plant" => AssetKind::Plant,
            "unit" => AssetKind::Unit,
            "equipment" => AssetKind::Equipment,
            "sensor" => AssetKind::Sensor,
            "alarm" => AssetKind::Alarm,
            "process" => AssetKind::Process,
            "incident" => AssetKind::Incident,
            "work_order" => AssetKind::WorkOrder,
            "inspection" => AssetKind::Inspection,
            "procedure" => AssetKind::Procedure,
            _ => return None,
        })
    }
}

/// Where a record came from. Every stored industrial record carries this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Origin, e.g. `ingest:<doc>`, `operator`, `device:<gateway>`, `derived:<method>`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// RFC 3339 instant the source recorded.
    pub at: String,
}

impl Provenance {
    pub fn derived(source: impl Into<String>, at: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            detail: None,
            at: at.into(),
        }
    }
}

/// Canonical relationship predicates of the industrial graph.
pub const REL_PREDICATES: &[&str] = &[
    "belongs_to",
    "measures",
    "associated_with",
    "involves",
    "performed_on",
    "applies_to",
];

/// One asset record. `parent`/`plant`/`unit` give structure; `attrs` is the
/// extension point for deployment-specific fields (severity, first_seen,
/// equipment_type, ratings, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: String,
    pub kind: AssetKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub attrs: BTreeMap<String, Value>,
    pub provenance: Provenance,
    pub created_at: String,
    pub updated_at: String,
}

impl Asset {
    pub fn new(
        id: impl Into<String>,
        kind: AssetKind,
        label: impl Into<String>,
        provenance: Provenance,
    ) -> Self {
        let now = provenance.at.clone();
        Self {
            id: id.into(),
            kind,
            label: label.into(),
            plant: None,
            unit: None,
            parent: None,
            status: None,
            attrs: BTreeMap::new(),
            created_at: now.clone(),
            updated_at: now,
            provenance,
        }
    }

    pub fn with_attr(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.attrs.insert(key.to_string(), value.into());
        self
    }

    pub fn attr(&self, key: &str) -> Option<&Value> {
        self.attrs.get(key)
    }

    pub fn attr_str(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).and_then(Value::as_str)
    }

    /// `equipment_type` drives FMEA tables (`pump`, `compressor`, `valve`,
    /// `heat_exchanger`, `instrument`, anything else falls back to generic).
    pub fn equipment_type(&self) -> &str {
        self.attr_str("equipment_type").unwrap_or("generic")
    }

    pub fn alarm_severity(&self) -> &str {
        self.attr_str("severity").unwrap_or("unknown")
    }
}

/// A directed relation with provenance; mirrored into `knowledge.graph`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub provenance: Provenance,
    pub created_at: String,
}

/// Filter for `list_assets`.
#[derive(Debug, Clone, Default)]
pub struct AssetFilter {
    pub kind: Option<AssetKind>,
    pub plant: Option<String>,
    pub unit: Option<String>,
    pub text: Option<String>,
}

impl AssetFilter {
    pub fn matches(&self, asset: &Asset) -> bool {
        if let Some(k) = self.kind {
            if asset.kind != k {
                return false;
            }
        }
        if let Some(p) = &self.plant {
            if asset.plant.as_deref() != Some(p.as_str()) {
                return false;
            }
        }
        if let Some(u) = &self.unit {
            if asset.unit.as_deref() != Some(u.as_str()) {
                return false;
            }
        }
        if let Some(t) = &self.text {
            let needle = t.to_lowercase();
            let hit = asset.id.to_lowercase().contains(&needle)
                || asset.label.to_lowercase().contains(&needle);
            if !hit {
                return false;
            }
        }
        true
    }
}
