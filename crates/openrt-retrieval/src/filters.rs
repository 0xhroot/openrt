//! Metadata filters bounding a hybrid retrieval universe (M9.6).
//!
//! A filter is applied to the candidate set BEFORE fusion; the dense list is
//! bounded by the same universe inside [`crate::hybrid::fuse`], so a filter
//! can never be bypassed by the vector side.

use serde::{Deserialize, Serialize};

/// Metadata filter over retrieval candidates. `None` = unconstrained.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalFilter {
    /// Only chunks of this document pass.
    pub document_id: Option<String>,
    /// Only chunks on this page pass.
    pub page: Option<u32>,
}

impl RetrievalFilter {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.document_id.is_none() && self.page.is_none()
    }

    /// Does this (document, page) pair pass the filter?
    pub fn matches(&self, document_id: &str, page: Option<u32>) -> bool {
        if let Some(want) = &self.document_id {
            if want != document_id {
                return false;
            }
        }
        if let Some(want) = self.page {
            if page != Some(want) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_matches_everything() {
        let f = RetrievalFilter::all();
        assert!(f.is_empty());
        assert!(f.matches("anything", None));
        assert!(f.matches("anything", Some(9)));
    }

    #[test]
    fn document_and_page_constraints() {
        let f = RetrievalFilter {
            document_id: Some("d1".into()),
            page: Some(2),
        };
        assert!(!f.is_empty());
        assert!(f.matches("d1", Some(2)));
        assert!(!f.matches("d1", Some(3)), "page mismatch");
        assert!(!f.matches("d1", None), "missing page fails closed");
        assert!(!f.matches("d2", Some(2)), "document mismatch");
    }

    #[test]
    fn partial_filters_only_constrain_their_field() {
        let f = RetrievalFilter {
            document_id: Some("d1".into()),
            page: None,
        };
        assert!(f.matches("d1", Some(7)));
        assert!(!f.matches("d2", Some(7)));
    }
}
