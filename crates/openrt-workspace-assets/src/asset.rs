use serde::{Deserialize, Serialize};

use crate::canonical::canonical_digest;
use crate::AssetError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub origin: String,
    pub source: String,
}

impl Provenance {
    pub fn new(origin: &str, source: &str) -> Self {
        Self {
            origin: origin.to_string(),
            source: source.to_string(),
        }
    }

    pub fn validate(&self) -> Result<(), AssetError> {
        if self.origin.trim().is_empty() || self.source.trim().is_empty() {
            return Err(AssetError::InvalidProvenance);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationState {
    Registered,
    Validated,
    Active,
    Inactive,
    Invalidated,
}

impl ActivationState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActivationState::Registered => "registered",
            ActivationState::Validated => "validated",
            ActivationState::Active => "active",
            ActivationState::Inactive => "inactive",
            ActivationState::Invalidated => "invalidated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorGrant {
    pub operator: String,
}

impl OperatorGrant {
    pub fn new(operator: &str) -> Result<Self, AssetError> {
        if operator.trim().is_empty() {
            return Err(AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            });
        }
        Ok(Self {
            operator: operator.to_string(),
        })
    }
}

pub trait CanonicalForm {
    fn asset_kind(&self) -> &'static str;
    fn asset_id(&self) -> String;
    fn canonical_pairs(&self) -> Vec<(String, String)>;
    fn digest(&self) -> String {
        canonical_digest(&self.canonical_pairs())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetEnvelope<T> {
    pub payload: T,
    pub digest: String,
}

impl<T: CanonicalForm> AssetEnvelope<T> {
    pub fn seal(payload: T) -> Self {
        let digest = payload.digest();
        Self { payload, digest }
    }

    pub fn verify(&self) -> Result<(), AssetError> {
        if self.payload.digest() != self.digest {
            return Err(AssetError::DigestMismatch {
                kind: self.payload.asset_kind().to_string(),
                id: self.payload.asset_id(),
            });
        }
        Ok(())
    }
}

pub fn sorted_join(items: &[String]) -> String {
    let mut v = items.to_vec();
    v.sort();
    v.dedup();
    v.join(",")
}
