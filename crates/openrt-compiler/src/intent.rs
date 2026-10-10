//! Structured goal intent normalization (STEP 1 input to rejection checks).
//!
//! Deterministic, word-boundary normalization of the natural-language request
//! into **action classes** and **artifact intents** — no giant substring
//! matcher, no LLM. Fail-closed behavior is unchanged: a request with no
//! recognized action, file reference, artifact output, or network marker is
//! still rejected as `unsupported_goal`; conflicting create/delete intents
//! without sequencing are still rejected as `ambiguous_goal`.
//!
//! The vocabulary is the union of common synonyms operators actually type
//! (`create|make|generate|produce|build|write|draft|prepare|export|render|
//! compile|analyze|inspect|read|find|search|list|verify|validate|test|run|
//! open|launch|navigate|…`). Matching is token-exact or stem-prefix over the
//! punctuation-split token stream, so "make" matches "making"/"made" but
//! "cram" never matches "create".

/// Recognized action class (a request may carry several).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActionClass {
    Create,
    Read,
    Analyze,
    Verify,
    Search,
    List,
    Run,
    Navigate,
    Transform,
    Delete,
}

impl ActionClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Read => "read",
            Self::Analyze => "analyze",
            Self::Verify => "verify",
            Self::Search => "search",
            Self::List => "list",
            Self::Run => "run",
            Self::Navigate => "navigate",
            Self::Transform => "transform",
            Self::Delete => "delete",
        }
    }
}

/// Recognized output artifact (common artifact/object pairs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArtifactKind {
    Pdf,
    Docx,
    Pptx,
    Xlsx,
    Csv,
    Report,
    Markdown,
    Html,
    Json,
    Image,
    Chart,
    Video,
    Audio,
}

impl ArtifactKind {
    /// Stable short id (matches the diagnostic/report vocabulary).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Xlsx => "xlsx",
            Self::Csv => "csv",
            Self::Report => "report",
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Json => "json",
            Self::Image => "image",
            Self::Chart => "chart",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }

    /// Human label used in expected outputs and backend diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pdf => "PDF",
            Self::Docx => "DOCX",
            Self::Pptx => "PPTX",
            Self::Xlsx => "XLSX",
            Self::Csv => "CSV",
            Self::Report => "report",
            Self::Markdown => "Markdown",
            Self::Html => "HTML",
            Self::Json => "JSON",
            Self::Image => "image",
            Self::Chart => "chart",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }

    /// Value used in `GoalSpec::expected_outputs` (derived from an artifact
    /// the request explicitly asked for — never a fabricated output).
    pub fn expected_output(&self) -> String {
        format!("{} artifact", self.label())
    }
}

/// Normalized intent for one request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Intent {
    /// Distinct recognized action classes, in discovery order.
    pub actions: Vec<ActionClass>,
    /// Distinct recognized output artifacts, in discovery order.
    pub artifacts: Vec<ArtifactKind>,
    /// Dotted file-like references (`report.pdf`), deduplicated.
    pub file_refs: Vec<String>,
    /// The request needs network (http/fetch/download/web markers).
    pub network: bool,
    /// Explicit sequencing words present ("then", "after", "next", …).
    pub sequenced: bool,
    /// The request asks for a summary/digest/overview artifact.
    pub summary: bool,
    /// Original request (provenance for diagnostics).
    pub request: String,
}

impl Intent {
    pub fn has(&self, class: ActionClass) -> bool {
        self.actions.contains(&class)
    }

    pub fn artifact(&self, kind: ArtifactKind) -> bool {
        self.artifacts.contains(&kind)
    }

    /// True when the request carries any signal the compiler can plan:
    /// a recognized action, an artifact output, a file reference, or a
    /// network marker (mirrors the historic fail-closed acceptance rule).
    pub fn actionable(&self) -> bool {
        !self.actions.is_empty()
            || !self.artifacts.is_empty()
            || !self.file_refs.is_empty()
            || self.network
    }

    /// Merge another normalization of the same goal (objective + original
    /// request) without duplicating entries.
    pub fn merge(&mut self, other: Intent) {
        for a in other.actions {
            if !self.actions.contains(&a) {
                self.actions.push(a);
            }
        }
        for a in other.artifacts {
            if !self.artifacts.contains(&a) {
                self.artifacts.push(a);
            }
        }
        for f in other.file_refs {
            if !self.file_refs.contains(&f) {
                self.file_refs.push(f);
            }
        }
        self.network |= other.network;
        self.sequenced |= other.sequenced;
        self.summary |= other.summary;
    }

    /// Extract the artifacts named in an arbitrary text (subgoal objective /
    /// outputs) using the same vocabulary. Token order (not table order), so
    /// classification is deterministic in reading order.
    pub fn artifacts_in(text: &str) -> Vec<ArtifactKind> {
        let tokens = tokenize(text);
        let mut out: Vec<ArtifactKind> = Vec::new();
        for tok in &tokens {
            if let Some((kind, _, _)) = ARTIFACT_VOCAB
                .iter()
                .find(|(_, exact, stems)| word_match(tok, exact, stems))
            {
                if !out.contains(kind) {
                    out.push(*kind);
                }
            }
        }
        out
    }
}

/// The artifact a request is *creating* (drives the artifact-capability
/// backend choice): the first artifact token at or after the first
/// create-class token. Returns `None` when there is no create verb — bare
/// artifact mentions are topics/inputs, not promised outputs — so callers
/// keep their historic "goal output" default instead of inventing one.
pub fn output_artifact_in(text: &str) -> Option<ArtifactKind> {
    let tokens = tokenize(text);
    let first_create = tokens.iter().position(|tok| {
        ACTION_VOCAB
            .iter()
            .any(|(c, exact, stems)| *c == ActionClass::Create && word_match(tok, exact, stems))
    })?;
    tokens[first_create..].iter().find_map(|tok| {
        ARTIFACT_VOCAB
            .iter()
            .find(|(_, exact, stems)| word_match(tok, exact, stems))
            .map(|(kind, _, _)| *kind)
    })
}

/// Sequencing markers that disambiguate mixed create/delete requests.
const SEQUENCE: &[&str] = &[
    " then ",
    " after ",
    " first ",
    " next ",
    " finally ",
    " and then ",
];

/// (class, exact words, stems). A token matches when it equals an exact word
/// or starts with a stem (stems are ≥4 chars so "run" cannot swallow "runner"
/// style accidents for short words — short words are exact-only).
const ACTION_VOCAB: &[(ActionClass, &[&str], &[&str])] = &[
    // ---- create / produce ----
    (
        ActionClass::Create,
        &[
            "make", "makes", "made", "making", "build", "builds", "write", "writes", "save",
            "saves", "draft", "compile",
        ],
        &[
            "creat", "generat", "produc", "prepar", "export", "render", "draft", "author",
            "compos", "assembl", "writ",
        ],
    ),
    // summarize/digest/overview are analyzed AND produced (historic behavior:
    // they both decompose into analysis and trigger the generate subgoal).
    (
        ActionClass::Create,
        &["summarise", "summarises", "summarize", "summarizes"],
        &["summar", "digest", "overview"],
    ),
    (
        ActionClass::Analyze,
        &["summarise", "summarises", "summarize", "summarizes"],
        &["summar", "digest", "overview"],
    ),
    // ---- read / ingest ----
    (
        ActionClass::Read,
        &["read", "reads", "load", "loads", "view", "views"],
        &["ingest", "pars", "extract"],
    ),
    // ---- analyze / inspect ----
    (
        ActionClass::Analyze,
        &[
            "review", "reviews", "compare", "compares", "find", "finds", "identify",
        ],
        &[
            "analys",
            "analyz",
            "insp",
            "examin",
            "assess",
            "evaluat",
            "investigat",
            "explor",
            "diagnos",
            "monitor",
        ],
    ),
    // ---- verify / test ----
    (
        ActionClass::Verify,
        &[
            "check", "checks", "test", "tests", "confirm", "confirms", "audit",
        ],
        &["verif", "validat"],
    ),
    // ---- search / discover ----
    (
        ActionClass::Search,
        &[
            "search", "searches", "find", "finds", "grep", "lookup", "query", "collect", "gather",
        ],
        &["discover", "queri"],
    ),
    // ---- list ----
    (ActionClass::List, &["list", "lists"], &["enumerat"]),
    // ---- run / execute ----
    (
        ActionClass::Run,
        &["run", "runs", "start", "starts", "deploy"],
        &["execut", "launch"],
    ),
    // ---- navigate (besides the `open …` phrase rule) ----
    (
        ActionClass::Navigate,
        &["navigate", "visits", "visit", "browse"],
        &["navigat"],
    ),
    // ---- transform ----
    (
        ActionClass::Transform,
        &["clean", "sort", "split", "filter"],
        &[
            "transform",
            "convert",
            "merg",
            "encrypt",
            "decrypt",
            "normaliz",
            "dedupl",
        ],
    ),
    // ---- delete / remove ----
    (
        ActionClass::Delete,
        &["delete", "deletes", "remove", "removes", "wipe"],
        &["delet", "remov", "destroy", "eras", "purge"],
    ),
];

/// (artifact, exact words, stems) — artifact/object pairs PART C.
const ARTIFACT_VOCAB: &[(ArtifactKind, &[&str], &[&str])] = &[
    (ArtifactKind::Pdf, &["pdf"], &[]),
    (ArtifactKind::Docx, &["docx", "doc", "word"], &[]),
    (
        ArtifactKind::Pptx,
        &["pptx", "ppt", "powerpoint", "slide", "slides"],
        &[],
    ),
    (
        ArtifactKind::Xlsx,
        &["xlsx", "xls", "excel", "spreadsheet", "workbook"],
        &[],
    ),
    (ArtifactKind::Csv, &["csv"], &[]),
    (
        ArtifactKind::Report,
        &[
            "report",
            "reports",
            "writeup",
            "write-up",
            "document",
            "documentation",
        ],
        &[],
    ),
    (ArtifactKind::Markdown, &["markdown"], &[]),
    (ArtifactKind::Html, &["html", "webpage"], &[]),
    (ArtifactKind::Json, &["json"], &[]),
    (
        ArtifactKind::Image,
        &["image", "images", "png", "jpg", "jpeg", "picture", "photo"],
        &[],
    ),
    (
        ArtifactKind::Chart,
        &[
            "chart",
            "charts",
            "graph",
            "graphs",
            "plot",
            "diagram",
            "visualization",
        ],
        &[],
    ),
    (ArtifactKind::Video, &["video", "videos"], &[]),
    (ArtifactKind::Audio, &["audio", "song", "podcast"], &[]),
];

/// Network markers (also actionable on their own — historic rule).
const NETWORK_WORDS: &[&str] = &[
    "http", "https", "www", "url", "web", "fetch", "download", "wget", "curl",
];
const NETWORK_STEMS: &[&str] = &["fetch", "download"];

/// `open` as a navigation verb only in front of web-ish nouns; otherwise it
/// is the historic "open the file" read action.
const OPEN_NAV: &[&str] = &[
    "browser", "url", "website", "webpage", "page", "site", "link", "web", "internet", "homepage",
];

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '_')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn word_match(tok: &str, exact: &[&str], stems: &[&str]) -> bool {
    exact.iter().any(|w| tok == *w) || stems.iter().any(|s| tok.starts_with(s))
}

/// Normalize a natural-language request into an [`Intent`].
pub fn normalize(request: &str) -> Intent {
    let tokens = tokenize(request);
    let mut intent = Intent {
        request: request.trim().to_string(),
        ..Intent::default()
    };

    for (i, tok) in tokens.iter().enumerate() {
        // file references (dotted tokens with a short alphanumeric extension)
        if let Some((_, ext)) = tok.rsplit_once('.') {
            if !ext.is_empty() && ext.len() <= 6 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
                intent.file_refs.push(tok.clone());
            }
        }
        // `open <web thing>` navigates; plain `open` reads.
        if tok == "open" {
            // skip at most one determiner/preposition: "open the browser"
            let next = (0..2)
                .find_map(|k| tokens.get(i + 1 + k).map(String::as_str))
                .unwrap_or("");
            let next = if matches!(next, "the" | "a" | "an" | "to" | "up") {
                tokens.get(i + 2).map(String::as_str).unwrap_or("")
            } else {
                next
            };
            if OPEN_NAV.contains(&next) {
                push_action(&mut intent, ActionClass::Navigate);
            } else {
                push_action(&mut intent, ActionClass::Read);
            }
            continue;
        }
        // `look up` → search (two-token phrase)
        if tok == "look" && tokens.get(i + 1).map(String::as_str) == Some("up") {
            push_action(&mut intent, ActionClass::Search);
            continue;
        }
        for (class, exact, stems) in ACTION_VOCAB {
            if word_match(tok, exact, stems) {
                push_action(&mut intent, *class);
            }
        }
        for (kind, exact, stems) in ARTIFACT_VOCAB {
            if word_match(tok, exact, stems) && !intent.artifacts.contains(kind) {
                intent.artifacts.push(*kind);
            }
        }
        if word_match(tok, NETWORK_WORDS, NETWORK_STEMS) {
            intent.network = true;
        }
        // summary markers (token or stem: "summary"/"summarize"/"digest"/…)
        // are both produced and analyzed, like the historic summarize needles.
        if word_match(
            tok,
            &["summary", "summaries", "digest", "overview"],
            &["summar"],
        ) {
            intent.summary = true;
            if !intent.actions.contains(&ActionClass::Create) {
                intent.actions.push(ActionClass::Create);
            }
            if !intent.actions.contains(&ActionClass::Analyze) {
                intent.actions.push(ActionClass::Analyze);
            }
        }
    }

    // file-ish dotted extensions the historic checker accepted without a verb
    // are already covered by file_refs; network markers count too.
    let lowered = format!(" {}", request.to_lowercase()).replace('\n', " ");
    intent.sequenced = SEQUENCE.iter().any(|s| lowered.contains(s));
    intent
}

fn push_action(intent: &mut Intent, class: ActionClass) {
    if !intent.actions.contains(&class) {
        intent.actions.push(class);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acts(req: &str) -> Vec<ActionClass> {
        normalize(req).actions
    }

    #[test]
    fn create_synonyms_normalize_to_one_class() {
        for req in [
            "create a pdf about 8051",
            "make a pdf about 8051",
            "generate a PDF on microprocessor 8051",
            "produce a pdf for the 8051",
            "build a report for 8051",
            "write a report about 8051",
            "draft a report about 8051",
            "prepare a report about 8051",
            "export this as PDF",
            "render the report as pdf",
            "compile a pdf for 8051",
        ] {
            let i = normalize(req);
            assert!(
                i.has(ActionClass::Create),
                "{req:?} should normalize to Create, got {:?}",
                i.actions
            );
            assert!(i.actionable(), "{req:?} must be actionable");
        }
    }

    #[test]
    fn artifact_pairs_are_detected() {
        assert!(normalize("make a pdf on microprocessor 8051").artifact(ArtifactKind::Pdf));
        assert!(normalize("generate a report about 8051").artifact(ArtifactKind::Report));
        assert!(normalize("make a DOCX for review").artifact(ArtifactKind::Docx));
        assert!(normalize("build a PPTX deck").artifact(ArtifactKind::Pptx));
        assert!(normalize("create an image of the block diagram").artifact(ArtifactKind::Image));
        assert!(normalize("generate a chart of throughput").artifact(ArtifactKind::Chart));
        assert!(normalize("export rows to CSV").artifact(ArtifactKind::Csv));
    }

    #[test]
    fn read_analyze_verify_search_list_run_navigate_vocabulary() {
        assert!(acts("read report.txt").contains(&ActionClass::Read));
        assert!(acts("inspect the repository").contains(&ActionClass::Analyze));
        assert!(acts("analyze the log").contains(&ActionClass::Analyze));
        assert!(acts("validate the schema").contains(&ActionClass::Verify));
        assert!(acts("run the tests").contains(&ActionClass::Run));
        assert!(acts("list files in the current directory").contains(&ActionClass::List));
        assert!(acts("find the config file").contains(&ActionClass::Search));
        assert!(acts("open the browser").contains(&ActionClass::Navigate));
        assert!(acts("open report.txt").contains(&ActionClass::Read));
        assert!(acts("navigate to the dashboard").contains(&ActionClass::Navigate));
        assert!(acts("launch the server").contains(&ActionClass::Run));
    }

    #[test]
    fn nonsense_stays_unactionable_fail_closed() {
        let i = normalize("Transmute lead into unobtanium with psychic waves");
        assert!(!i.actionable(), "{i:?}");
        let j = normalize("microprocessor 8051");
        assert!(!j.actionable(), "{j:?}");
    }

    #[test]
    fn sequencing_words_are_recognized() {
        assert!(normalize("create A then delete B").sequenced);
        assert!(!normalize("create A or delete B").sequenced);
    }

    #[test]
    fn word_boundaries_do_not_match_substrings() {
        // "cram" must not look like "creat…" style prefix matches.
        assert!(!normalize("cram the data").has(ActionClass::Create));
        // stem matches still work for real inflections
        assert!(normalize("creating a report").has(ActionClass::Create));
    }

    #[test]
    fn network_markers_mark_requires_network() {
        assert!(normalize("download the dataset from https://x/y").network);
        assert!(normalize("fetch https://example.com").network);
    }

    #[test]
    fn summary_markers_set_summary_flag_from_stems() {
        assert!(normalize("summarize the report").summary);
        assert!(normalize("give me a summary").summary);
        assert!(normalize("a digest of events").summary);
        assert!(!normalize("read the file").summary);
    }

    #[test]
    fn output_artifact_follows_the_create_verb() {
        // create verb first → the pdf being made is the output
        assert_eq!(
            output_artifact_in("make a pdf on microprocessor 8051"),
            Some(ArtifactKind::Pdf)
        );
        assert_eq!(
            output_artifact_in("generate a report about 8051"),
            Some(ArtifactKind::Report)
        );
        // artifact mentioned before the create verb is an input, not output
        assert_eq!(
            output_artifact_in("read the pdf then create a chart"),
            Some(ArtifactKind::Chart)
        );
        // no create verb → no invented output
        assert_eq!(output_artifact_in("read report.txt"), None);
        assert_eq!(output_artifact_in("Transmute lead into unobtanium"), None);
        // create verb but no artifact → historic "goal output" default
        assert_eq!(output_artifact_in("create a file"), None);
    }

    #[test]
    fn artifacts_in_returns_reading_order() {
        let a = Intent::artifacts_in("create slides then export a pdf");
        assert_eq!(a, vec![ArtifactKind::Pptx, ArtifactKind::Pdf]);
    }
}
