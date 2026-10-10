//! Deterministic BM25 (Okapi, k1=1.2 b=0.75) over chunk text — the lexical
//! half of hybrid retrieval (M9.6).
//!
//! Everything here is a pure function of (corpus, query): same inputs, same
//! ranked list, always. No network, no model, no randomness.

use std::collections::HashMap;

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// One candidate passage for hybrid retrieval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub document_id: String,
    pub chunk_id: String,
    pub page: Option<u32>,
    pub text: String,
}

impl Candidate {
    pub fn new(
        document_id: impl Into<String>,
        chunk_id: impl Into<String>,
        page: Option<u32>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            document_id: document_id.into(),
            chunk_id: chunk_id.into(),
            page,
            text: text.into(),
        }
    }

    pub fn key(&self) -> (String, String) {
        (self.document_id.clone(), self.chunk_id.clone())
    }
}

/// A BM25-scored candidate (rank order is the returned Vec order).
#[derive(Debug, Clone, PartialEq)]
pub struct Bm25Hit {
    pub document_id: String,
    pub chunk_id: String,
    pub page: Option<u32>,
    pub score: f32,
}

/// Tokenizer shared by BM25 and the deterministic reranker: lowercase,
/// split on non-alphanumerics. Deterministic and locale-independent.
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

struct IndexedDoc {
    document_id: String,
    chunk_id: String,
    page: Option<u32>,
    len: f64,
    tf: HashMap<String, u32>,
}

/// BM25 index over a fixed candidate set.
pub struct Bm25Index {
    docs: Vec<IndexedDoc>,
    df: HashMap<String, u32>,
    avgdl: f64,
}

impl Bm25Index {
    pub fn new(candidates: &[Candidate]) -> Self {
        let mut docs = Vec::with_capacity(candidates.len());
        let mut df: HashMap<String, u32> = HashMap::new();
        for c in candidates {
            let tokens = tokenize(&c.text);
            let mut tf: HashMap<String, u32> = HashMap::new();
            for t in &tokens {
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
            for term in tf.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            let len = tokens.len() as f64;
            docs.push(IndexedDoc {
                document_id: c.document_id.clone(),
                chunk_id: c.chunk_id.clone(),
                page: c.page,
                len,
                tf,
            });
        }
        let total_len: f64 = docs.iter().map(|d| d.len).sum();
        let avgdl = if docs.is_empty() {
            0.0
        } else {
            total_len / docs.len() as f64
        };
        Self { docs, df, avgdl }
    }

    /// Ranked (score desc; ties keep corpus insertion order) hits with a
    /// strictly positive BM25 score. `k == 0` returns an empty list.
    pub fn search(&self, query: &str, k: usize) -> Vec<Bm25Hit> {
        if k == 0 || self.docs.is_empty() || self.avgdl <= 0.0 {
            return Vec::new();
        }
        let n = self.docs.len() as f64;
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(usize, f32)> = Vec::new();
        for (i, doc) in self.docs.iter().enumerate() {
            let mut score: f64 = 0.0;
            for term in &query_tokens {
                let Some(&tf) = doc.tf.get(term) else {
                    continue;
                };
                let df = *self.df.get(term).unwrap_or(&0) as f64;
                // Okapi idf, always positive (no negative-idf pathology).
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                let tf = tf as f64;
                let norm = K1 * (1.0 - B + B * doc.len / self.avgdl);
                score += idf * (tf * (K1 + 1.0)) / (tf + norm);
            }
            if score > 0.0 {
                scored.push((i, score as f32));
            }
        }
        // Deterministic: score desc, then corpus insertion order.
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        scored
            .into_iter()
            .take(k)
            .map(|(i, score)| {
                let d = &self.docs[i];
                Bm25Hit {
                    document_id: d.document_id.clone(),
                    chunk_id: d.chunk_id.clone(),
                    page: d.page,
                    score,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(items: &[(&str, &str, &str)]) -> Vec<Candidate> {
        items
            .iter()
            .map(|(id, chunk, text)| Candidate::new(*id, *chunk, None, *text))
            .collect()
    }

    #[test]
    fn tokenizer_is_deterministic_and_lowercase() {
        assert_eq!(tokenize("Hello, World! 42"), vec!["hello", "world", "42"]);
        assert_eq!(tokenize("  --  "), Vec::<String>::new());
        assert_eq!(tokenize("Hello, World! 42"), tokenize("HELLO, world! 42"));
    }

    #[test]
    fn matching_document_ranks_first_with_positive_score() {
        let idx = Bm25Index::new(&cands(&[
            ("d1", "c1", "quantum physics paper"),
            ("d2", "c2", "kitten plays with yarn"),
            ("d3", "c3", "the and of"),
        ]));
        let hits = idx.search("kitten", 10);
        assert_eq!(
            hits.len(),
            1,
            "non-matching docs must be excluded: {hits:?}"
        );
        assert_eq!(hits[0].document_id, "d2");
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn rare_term_beats_common_term() {
        let mut items = vec![("d0", "c0", "common words common here common")];
        for i in 1..10 {
            items.push((
                Box::leak(format!("d{i}").into_boxed_str()),
                Box::leak(format!("c{i}").into_boxed_str()),
                "common words appear common everywhere common",
            ));
        }
        items.push(("dzebra", "cz", "common but also zebra"));
        let idx = Bm25Index::new(&cands(&items));
        let hits = idx.search("common zebra", 5);
        assert_eq!(hits.first().map(|h| h.document_id.as_str()), Some("dzebra"));
    }

    #[test]
    fn ties_keep_insertion_order_and_empty_inputs_are_empty() {
        let idx = Bm25Index::new(&cands(&[
            ("d1", "c1", "identical text"),
            ("d2", "c2", "identical text"),
        ]));
        let hits = idx.search("identical", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].document_id, "d1");
        assert_eq!(hits[1].document_id, "d2");
        assert_eq!(Bm25Index::new(&[]).search("anything", 5), Vec::new());
        assert_eq!(idx.search("   ", 5), Vec::new());
        assert_eq!(idx.search("identical", 0), Vec::new());
    }

    /// Recall fixture (M9.6): of five indexed chunks, exactly three are
    /// relevant to the query — a top_k large enough must retrieve ALL
    /// three (recall 1.0), and a tight top_k must still retrieve the
    /// highest-scoring relevant ones, never a non-relevant doc first.
    #[test]
    fn recall_fixture_retrieves_all_relevant_chunks() {
        let idx = Bm25Index::new(&cands(&[
            ("d1", "c1", "launch window opens at dawn"),
            ("d2", "c2", "launch window closes at dusk"),
            ("d3", "c3", "launch window delays are common"),
            ("d4", "c4", "unrelated gardening notes"),
            ("d5", "c5", "cook pasta in boiling water"),
        ]));
        let hits = idx.search("launch window", 10);
        let relevant: std::collections::HashSet<&str> = ["d1", "d2", "d3"].into();
        let got: std::collections::HashSet<&str> =
            hits.iter().map(|h| h.document_id.as_str()).collect();
        assert_eq!(
            got.intersection(&relevant).count(),
            3,
            "recall must be 1.0 at top_k=10: {hits:?}"
        );
        assert!(
            got.is_subset(&relevant),
            "non-relevant chunk retrieved: {hits:?}"
        );
        // Tight budget: only relevant docs, ordered by score.
        let top = idx.search("launch window", 2);
        assert_eq!(top.len(), 2);
        for h in &top {
            assert!(relevant.contains(h.document_id.as_str()), "{top:?}");
            assert!(h.score > 0.0);
        }
    }

    #[test]
    fn same_inputs_same_scores() {
        let a = Bm25Index::new(&cands(&[
            ("d1", "c1", "alpha beta"),
            ("d2", "c2", "beta gamma"),
        ]));
        let b = Bm25Index::new(&cands(&[
            ("d1", "c1", "alpha beta"),
            ("d2", "c2", "beta gamma"),
        ]));
        assert_eq!(a.search("beta", 5), b.search("beta", 5));
    }
}
