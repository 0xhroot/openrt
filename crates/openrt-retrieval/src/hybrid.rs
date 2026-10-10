//! BM25 + dense fusion via Reciprocal Rank Fusion (RRF) — the hybrid core
//! of M9.6. Pure and deterministic: same candidate set, same dense list,
//! same query ⇒ same ranked output with explicit `retrieval_source`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::bm25::{Bm25Index, Candidate};
use crate::filters::RetrievalFilter;

/// A dense (vector-index) scored candidate. Identity is
/// `(document_id, chunk_id)`; page/text are resolved through the candidate
/// universe so a dense hit can never surface an unknown chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct DenseHit {
    pub document_id: String,
    pub chunk_id: String,
    pub score: f32,
}

impl DenseHit {
    pub fn new(document_id: impl Into<String>, chunk_id: impl Into<String>, score: f32) -> Self {
        Self {
            document_id: document_id.into(),
            chunk_id: chunk_id.into(),
            score,
        }
    }
}

/// Which retrieval list(s) produced a hit — the `retrieval_source`
/// provenance required by M9.6/TEST 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetrievalSource {
    /// Matched only the lexical (BM25) list.
    Lexical,
    /// Matched only the dense (embedding) list.
    Dense,
    /// Matched both lists and was fused by RRF.
    Hybrid,
}

impl RetrievalSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            RetrievalSource::Lexical => "lexical",
            RetrievalSource::Dense => "dense",
            RetrievalSource::Hybrid => "hybrid",
        }
    }
}

/// Which reranker produced `rerank_score` (M9.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RerankKind {
    /// Pure token-coverage reordering — always available, no backend.
    Deterministic,
    /// Real model-backed reranking through the model-provider infrastructure.
    Model,
}

impl RerankKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RerankKind::Deterministic => "deterministic",
            RerankKind::Model => "model",
        }
    }
}

/// One fused hit with its full score provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridHit {
    pub document_id: String,
    pub chunk_id: String,
    pub page: Option<u32>,
    pub source: RetrievalSource,
    pub bm25_score: Option<f32>,
    pub dense_score: Option<f32>,
    /// Reciprocal-rank-fusion score (all contributing lists summed).
    pub fused_score: f32,
    pub rerank_score: Option<f32>,
    pub rerank_kind: Option<RerankKind>,
}

/// RRF / truncation parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct FusionOptions {
    /// RRF smoothing constant (standard default: 60).
    pub rrf_k: f32,
    /// Maximum hits returned after fusion.
    pub top_k: usize,
}

impl Default for FusionOptions {
    fn default() -> Self {
        Self {
            rrf_k: 60.0,
            top_k: 10,
        }
    }
}

/// Fuse the BM25 ranking of `candidates` with `dense` via RRF.
///
/// * dense hits for chunks outside `candidates` are dropped — the candidate
///   set is the metadata universe (this is how a `RetrievalFilter` bounds
///   the dense side too);
/// * ties break deterministically on `(document_id, chunk_id)`;
/// * `top_k == 0`, an empty candidate set, or an empty dense list degrade
///   honestly (lexical-only / dense-only / empty) — never to fake scores.
pub fn fuse(
    candidates: &[Candidate],
    dense: &[DenseHit],
    query: &str,
    opts: &FusionOptions,
) -> Vec<HybridHit> {
    if opts.top_k == 0 || candidates.is_empty() {
        return Vec::new();
    }
    let lookup: HashMap<(String, String), &Candidate> = candidates
        .iter()
        .map(|c| ((c.document_id.clone(), c.chunk_id.clone()), c))
        .collect();

    let lexical = Bm25Index::new(candidates).search(query, candidates.len());

    // Dense ranking: score desc, deterministic tie-break; unknown chunks
    // (not in the candidate universe) are dropped.
    let mut dense_ranked: Vec<&DenseHit> = dense
        .iter()
        .filter(|h| lookup.contains_key(&(h.document_id.clone(), h.chunk_id.clone())))
        .collect();
    dense_ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.document_id.cmp(&b.document_id))
            .then(a.chunk_id.cmp(&b.chunk_id))
    });

    // Accumulate RRF contributions and per-list scores.
    struct Acc {
        page: Option<u32>,
        bm25: Option<f32>,
        dense: Option<f32>,
        fused: f32,
        in_lex: bool,
        in_dense: bool,
    }
    let mut acc: HashMap<(String, String), Acc> = HashMap::new();
    for (rank, h) in lexical.iter().enumerate() {
        let key = (h.document_id.clone(), h.chunk_id.clone());
        let e = acc.entry(key).or_insert(Acc {
            page: h.page,
            bm25: None,
            dense: None,
            fused: 0.0,
            in_lex: false,
            in_dense: false,
        });
        e.fused += 1.0 / (opts.rrf_k + (rank as f32 + 1.0));
        e.bm25 = Some(h.score);
        e.in_lex = true;
    }
    for (rank, h) in dense_ranked.iter().enumerate() {
        let cand = lookup
            .get(&(h.document_id.clone(), h.chunk_id.clone()))
            .expect("filtered above");
        let key = (h.document_id.clone(), h.chunk_id.clone());
        let e = acc.entry(key).or_insert(Acc {
            page: cand.page,
            bm25: None,
            dense: None,
            fused: 0.0,
            in_lex: false,
            in_dense: false,
        });
        e.fused += 1.0 / (opts.rrf_k + (rank as f32 + 1.0));
        e.dense = Some(h.score);
        e.in_dense = true;
    }

    let mut hits: Vec<HybridHit> = acc
        .into_iter()
        .map(|((document_id, chunk_id), e)| HybridHit {
            document_id,
            chunk_id,
            page: e.page,
            source: match (e.in_lex, e.in_dense) {
                (true, true) => RetrievalSource::Hybrid,
                (true, false) => RetrievalSource::Lexical,
                (false, true) => RetrievalSource::Dense,
                (false, false) => unreachable!("entry created only on contribution"),
            },
            bm25_score: e.bm25,
            dense_score: e.dense,
            fused_score: e.fused,
            rerank_score: None,
            rerank_kind: None,
        })
        .collect();
    hits.sort_by(|a, b| {
        b.fused_score
            .partial_cmp(&a.fused_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.document_id.cmp(&b.document_id))
            .then(a.chunk_id.cmp(&b.chunk_id))
    });
    hits.truncate(opts.top_k);
    hits
}

/// Apply a metadata filter to a candidate universe (lexical side; the dense
/// side is bounded by the same universe inside [`fuse`]).
pub fn filter_candidates<'a>(
    candidates: &'a [Candidate],
    filter: &RetrievalFilter,
) -> Vec<&'a Candidate> {
    candidates
        .iter()
        .filter(|c| filter.matches(&c.document_id, c.page))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, text: &str) -> Candidate {
        Candidate::new(id, "c1", None, text)
    }

    fn opts(top_k: usize) -> FusionOptions {
        FusionOptions { rrf_k: 60.0, top_k }
    }

    #[test]
    fn fusion_lifts_a_doc_present_in_both_lists() {
        // Lexical ranks X first, D second; dense ranks Y first, D second.
        // D (two rank-2 contributions) must outrank the single rank-1 lists.
        let candidates = vec![
            cand("X", "xray query terms"),
            cand("D", "the shared xray passage"),
            cand("Y", "yellow submarine"),
        ];
        let dense = vec![DenseHit::new("Y", "c1", 0.9), DenseHit::new("D", "c1", 0.5)];
        let hits = fuse(&candidates, &dense, "xray", &opts(10));
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert_eq!(hits[0].document_id, "D", "hybrid lift: {hits:?}");
        assert_eq!(hits[0].source, RetrievalSource::Hybrid);
        assert!(hits[0].bm25_score.is_some() && hits[0].dense_score.is_some());
        // Single-source rank-1 docs follow, deterministically ordered.
        let singles: Vec<&str> = hits[1..].iter().map(|h| h.document_id.as_str()).collect();
        assert_eq!(
            singles,
            vec!["X", "Y"],
            "X (lexical only) then Y (dense only): {hits:?}"
        );
        assert_eq!(hits[1].source, RetrievalSource::Lexical);
        assert_eq!(hits[2].source, RetrievalSource::Dense);
    }

    #[test]
    fn dense_only_degrades_to_lexical_and_vice_versa() {
        let candidates = vec![cand("A", "alpha"), cand("B", "beta")];
        // No dense list: lexical-only, honestly labeled.
        let hits = fuse(&candidates, &[], "alpha", &opts(10));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, RetrievalSource::Lexical);
        assert_eq!(hits[0].dense_score, None);

        // Dense present but lexical query matches nothing: dense-only hit.
        let dense = vec![DenseHit::new("B", "c1", 0.7)];
        let hits = fuse(&candidates, &dense, "unrelated-zzz", &opts(10));
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].document_id, "B");
        assert_eq!(hits[0].source, RetrievalSource::Dense);
        assert_eq!(hits[0].bm25_score, None);
    }

    #[test]
    fn dense_hits_outside_the_candidate_universe_are_dropped() {
        let candidates = vec![cand("A", "alpha")];
        let dense = vec![
            DenseHit::new("A", "c1", 0.9),
            DenseHit::new("EVIL", "c1", 0.99), // filtered metadata universe
        ];
        let hits = fuse(&candidates, &dense, "alpha", &opts(10));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].document_id, "A");
    }

    #[test]
    fn top_k_and_empty_inputs_fail_honestly() {
        let candidates = vec![cand("A", "alpha"), cand("B", "alpha beta")];
        assert!(fuse(&candidates, &[], "alpha", &opts(0)).is_empty());
        assert!(fuse(&[], &[], "alpha", &opts(5)).is_empty());
        let hits = fuse(&candidates, &[], "alpha", &opts(1));
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn same_inputs_same_order() {
        let candidates = vec![
            cand("A", "shared alpha"),
            cand("B", "shared beta"),
            cand("C", "shared gamma"),
        ];
        let dense = vec![DenseHit::new("C", "c1", 0.6), DenseHit::new("A", "c1", 0.2)];
        let a = fuse(&candidates, &dense, "shared", &opts(3));
        let b = fuse(&candidates, &dense, "shared", &opts(3));
        assert_eq!(a, b);
    }

    #[test]
    fn filter_bounds_the_lexical_universe() {
        let candidates = vec![
            Candidate::new("d1", "c1", Some(1), "alpha"),
            Candidate::new("d1", "c2", Some(2), "alpha"),
            Candidate::new("d2", "c1", Some(1), "alpha"),
        ];
        let f = RetrievalFilter {
            document_id: None,
            page: Some(1),
        };
        let kept = filter_candidates(&candidates, &f);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|c| c.page == Some(1)));
        let f = RetrievalFilter {
            document_id: Some("d2".into()),
            page: None,
        };
        let kept = filter_candidates(&candidates, &f);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].document_id, "d2");
    }
}
