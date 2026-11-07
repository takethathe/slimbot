//! Derived entry index over memory documents.
//!
//! Memory documents are authoritative; [`MemoryEntry`] values and the index
//! itself are derived views. The index is built on demand from
//! [`DocumentSnapshot`]s read through the [`MemoryBackend`](super::backend::MemoryBackend),
//! lives in memory only, and is never persisted (see ADR 0004).
//!
//! Slice 1 ships a single keyword implementation: latin word tokens plus CJK
//! bigrams, scored with BM25-lite. The [`MemoryIndex`] trait is the seam a
//! future inverted index / FTS5 / vector store can replace without touching
//! callers.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use anyhow::Result;
use async_trait::async_trait;

use super::category::MemoryCategory;
use super::frontmatter::parse_skill_frontmatter;

/// Default number of results when the caller does not ask for a specific count.
pub const DEFAULT_LIMIT: usize = 10;
/// Hard cap on results; larger requests are clamped rather than rejected.
pub const MAX_LIMIT: usize = 50;

/// Fixed documents that are always part of the search corpus, in canonical
/// order. `Daily` has no writer yet, so it is naturally empty; `Skill` /
/// `Custom` are enumerated dynamically by name.
pub const SEARCH_CATEGORIES: [MemoryCategory; 5] = [
    MemoryCategory::Soul,
    MemoryCategory::User,
    MemoryCategory::Agent,
    MemoryCategory::Tools,
    MemoryCategory::Core,
];

/// Workspace-relative path for a document, surfaced as an entry's `source`.
pub(crate) fn source_path(category: &MemoryCategory, name: Option<&str>) -> String {
    match category {
        MemoryCategory::Soul => "SOUL.md".to_string(),
        MemoryCategory::User => "USER.md".to_string(),
        MemoryCategory::Agent => "AGENTS.md".to_string(),
        MemoryCategory::Tools => "TOOLS.md".to_string(),
        MemoryCategory::Core => "memory/MEMORY.md".to_string(),
        // `Daily` is intentionally absent from `SEARCH_CATEGORIES` (no writer
        // yet), so it never reaches this helper. Fail loudly if a future
        // caller adds it without giving it a real date-based source.
        MemoryCategory::Daily => unreachable!("Daily documents are not searchable yet"),
        MemoryCategory::Skill => format!("skills/{}/SKILL.md", name.unwrap_or("unknown")),
        MemoryCategory::Custom => format!("memory/{}.md", name.unwrap_or("unknown")),
    }
}

/// A snapshot of one memory document, as read through the backend.
#[derive(Debug, Clone)]
pub struct DocumentSnapshot {
    pub category: MemoryCategory,
    pub name: Option<String>,
    /// Workspace-relative document path, used as the entry's `source`.
    pub source: String,
    pub content: String,
}

/// Derived minimal retrievable unit (never persisted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    /// Original paragraph text, returned to callers.
    pub content: String,
    pub category: MemoryCategory,
    /// Dynamic name for `Skill` / `Custom` documents.
    pub name: Option<String>,
    /// Workspace-relative source document path.
    pub source: String,
    /// Nearest enclosing ATX heading text, if any.
    pub heading: Option<String>,
    /// Hash of the normalized content: an in-process entry identity and the
    /// raw material for a future dedup gate. `DefaultHasher` is deliberately
    /// not stable across Rust versions, so this must never be persisted.
    pub hash: u64,
}

/// An entry together with its relevance score.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredEntry {
    pub entry: MemoryEntry,
    pub score: f64,
}

/// Replaceable index implementation. Slice 1 ships only [`KeywordIndex`].
// `double_must_use` is an `async_trait` desugaring artifact on methods
// returning `Result`, not a real attribute on this code.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait MemoryIndex: Send + Sync {
    /// Derive entries from a document set, replacing any previous state.
    async fn rebuild(&mut self, documents: &[DocumentSnapshot]) -> Result<()>;

    /// Return the top-`limit` entries for `query`, score-descending with a
    /// deterministic tie-break. An optional category filter narrows the set,
    /// it is a filter and not a permission boundary.
    fn search(
        &self,
        query: &str,
        limit: usize,
        category: Option<&MemoryCategory>,
    ) -> Vec<ScoredEntry>;
}

/// Content fingerprint of a document set: the hash of every enumerable
/// document's category, name, source and content, in enumeration order.
///
/// Content (not mtime/size) is used on purpose: the document-level backend
/// exposes neither paths nor stat, and content hashing needs zero trait
/// changes while also holding for a future sqlite backend.
pub(crate) fn fingerprint(documents: &[DocumentSnapshot]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for doc in documents {
        doc.category.as_str().hash(&mut hasher);
        doc.name.hash(&mut hasher);
        doc.source.hash(&mut hasher);
        doc.content.hash(&mut hasher);
    }
    hasher.finish()
}

/// Hash of the normalized content. Normalization collapses whitespace runs so
/// that reindenting a paragraph does not change its identity.
fn content_hash(content: &str) -> u64 {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

/// Recognize an ATX heading (`#`..`######` followed by whitespace or EOL).
/// `#hashtag` is not a heading. Returns the heading text.
fn parse_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if rest.is_empty() {
        return Some(String::new());
    }
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some(rest.trim().to_string())
}

/// Recognize a code fence opener/closer (` ``` ` or `~~~`).
fn fence_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Split a document into paragraph-block entries.
///
/// Boundaries: blank lines separate blocks (consecutive non-empty lines form
/// one entry). ATX heading lines set the current heading but produce no entry.
/// Fenced code blocks are indexed as a single entry (blank lines inside a
/// fence do not split it); the fence marker lines themselves are excluded, so
/// the info string never becomes a token. `Skill` frontmatter is stripped
/// before parsing.
pub(crate) fn parse_document(doc: &DocumentSnapshot) -> Vec<MemoryEntry> {
    let content = if doc.category == MemoryCategory::Skill {
        match parse_skill_frontmatter(&doc.content) {
            Some(meta) => meta.content,
            None => doc.content.clone(),
        }
    } else {
        doc.content.clone()
    };

    let mut entries = Vec::new();
    let mut heading: Option<String> = None;
    let mut block: Vec<&str> = Vec::new();
    let mut fence: Option<&str> = None;

    for line in content.lines() {
        if let Some(marker) = fence {
            if line.trim_start().starts_with(marker) {
                fence = None;
                flush_block(&mut block, &mut entries, doc, heading.as_deref());
            } else {
                block.push(line);
            }
            continue;
        }
        if let Some(marker) = fence_marker(line) {
            flush_block(&mut block, &mut entries, doc, heading.as_deref());
            fence = Some(marker);
            continue;
        }
        if let Some(text) = parse_heading(line) {
            flush_block(&mut block, &mut entries, doc, heading.as_deref());
            heading = (!text.is_empty()).then_some(text);
            continue;
        }
        if line.trim().is_empty() {
            flush_block(&mut block, &mut entries, doc, heading.as_deref());
            continue;
        }
        block.push(line);
    }
    // An unterminated fence is treated as a normal block rather than dropped.
    flush_block(&mut block, &mut entries, doc, heading.as_deref());
    entries
}

fn flush_block(
    block: &mut Vec<&str>,
    entries: &mut Vec<MemoryEntry>,
    doc: &DocumentSnapshot,
    heading: Option<&str>,
) {
    if block.is_empty() {
        return;
    }
    let joined = block.join("\n");
    let content = joined.trim();
    block.clear();
    if content.is_empty() {
        return;
    }
    entries.push(MemoryEntry {
        content: content.to_string(),
        category: doc.category,
        name: doc.name.clone(),
        source: doc.source.clone(),
        heading: heading.map(str::to_string),
        hash: content_hash(content),
    });
}

/// Whether `c` belongs to a script written without word separators.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF        // Hiragana + Katakana
        | 0x3400..=0x4DBF      // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF      // CJK Unified Ideographs
        | 0xF900..=0xFAFF      // CJK Compatibility Ideographs
        | 0xAC00..=0xD7AF      // Hangul syllables
        | 0x20000..=0x2FA1F    // CJK Extensions B+ (supplementary planes)
    )
}

fn flush_latin(tokens: &mut Vec<String>, latin: &mut String) {
    if latin.is_empty() {
        return;
    }
    if latin.chars().count() >= 2 {
        tokens.push(std::mem::take(latin));
    } else {
        latin.clear();
    }
}

fn flush_cjk(tokens: &mut Vec<String>, run: &mut Vec<char>) {
    if run.is_empty() {
        return;
    }
    if run.len() == 1 {
        tokens.push(run[0].to_string());
    } else {
        for pair in run.windows(2) {
            tokens.push(pair.iter().collect());
        }
    }
    run.clear();
}

/// Tokenize text for indexing or querying.
///
/// Latin runs are lowercased (via `char::is_alphanumeric`) and split on any
/// other character; tokens shorter than two chars are dropped. CJK runs are
/// indexed as adjacent-character bigrams; a run of exactly one character
/// yields a unigram so single-character queries can fall back to it.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut latin = String::new();
    let mut run: Vec<char> = Vec::new();

    for c in text.chars() {
        if is_cjk(c) {
            flush_latin(&mut tokens, &mut latin);
            run.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut tokens, &mut run);
            for lower in c.to_lowercase() {
                latin.push(lower);
            }
        } else {
            flush_latin(&mut tokens, &mut latin);
            flush_cjk(&mut tokens, &mut run);
        }
    }
    flush_latin(&mut tokens, &mut latin);
    flush_cjk(&mut tokens, &mut run);
    tokens
}

/// A query that is exactly one CJK character, which needs unigram fallback
/// because CJK runs are indexed as bigrams.
fn single_cjk_char(query: &str) -> Option<char> {
    let mut chars = query.trim().chars();
    let first = chars.next()?;
    if chars.next().is_none() && is_cjk(first) {
        Some(first)
    } else {
        None
    }
}

fn term_counts(tokens: Vec<String>) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for token in tokens {
        *counts.entry(token).or_insert(0) += 1;
    }
    counts
}

/// Whether an entry passes the optional category filter. The filter narrows
/// the result set; it is not a permission boundary.
fn passes_filter(entry: &MemoryEntry, category: Option<&MemoryCategory>) -> bool {
    match category {
        Some(filter) => entry.category == *filter,
        None => true,
    }
}

/// Canonical rank of a category, used as the primary tie-break.
fn category_rank(category: &MemoryCategory) -> usize {
    MemoryCategory::ALL
        .iter()
        .position(|c| c == category)
        .unwrap_or(usize::MAX)
}

/// Deterministic ordering: score descending, then category canonical order,
/// then source, heading and content hash.
fn sort_scored(entries: &mut [ScoredEntry]) {
    entries.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| category_rank(&a.entry.category).cmp(&category_rank(&b.entry.category)))
            .then_with(|| a.entry.source.cmp(&b.entry.source))
            .then_with(|| a.entry.heading.cmp(&b.entry.heading))
            .then_with(|| a.entry.hash.cmp(&b.entry.hash))
    });
}

/// Keyword index: latin tokens + CJK bigrams, BM25-lite scoring.
pub struct KeywordIndex {
    entries: Vec<MemoryEntry>,
    /// token → postings, where each posting carries a heading-weighted tf.
    postings: HashMap<String, Vec<Posting>>,
    /// Token count per entry, used as BM25's document length.
    lengths: Vec<usize>,
    avg_len: f64,
}

#[derive(Debug, Clone, Copy)]
struct Posting {
    entry: usize,
    /// Body term frequency + 2 × heading term frequency.
    tf: u32,
}

impl Default for KeywordIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl KeywordIndex {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            postings: HashMap::new(),
            lengths: Vec::new(),
            avg_len: 1.0,
        }
    }

    /// Derive entries from documents and build the postings map.
    pub fn build(documents: &[DocumentSnapshot]) -> Self {
        let entries: Vec<MemoryEntry> = documents.iter().flat_map(parse_document).collect();
        let mut postings: HashMap<String, Vec<Posting>> = HashMap::new();
        let mut lengths = Vec::with_capacity(entries.len());

        for (idx, entry) in entries.iter().enumerate() {
            let body = term_counts(tokenize(&entry.content));
            let heading = entry
                .heading
                .as_deref()
                .map(|h| term_counts(tokenize(h)))
                .unwrap_or_default();

            let length: usize =
                body.values().sum::<u32>() as usize + heading.values().sum::<u32>() as usize;
            lengths.push(length.max(1));

            // Merge body and heading counts, weighting heading hits ×2.
            let mut merged: HashMap<String, (u32, u32)> = HashMap::new();
            for (token, count) in body {
                merged.entry(token).or_insert((0, 0)).0 += count;
            }
            for (token, count) in heading {
                merged.entry(token).or_insert((0, 0)).1 += count;
            }
            for (token, (body_tf, heading_tf)) in merged {
                postings.entry(token).or_default().push(Posting {
                    entry: idx,
                    tf: body_tf + 2 * heading_tf,
                });
            }
        }

        let avg_len = if lengths.is_empty() {
            1.0
        } else {
            lengths.iter().sum::<usize>() as f64 / lengths.len() as f64
        };

        Self {
            entries,
            postings,
            lengths,
            avg_len: avg_len.max(1.0),
        }
    }

    /// Top-`limit` hits for a single CJK character, counted as substrings
    /// (heading hits weighted ×2). This is the unigram fallback.
    fn search_single_char(
        &self,
        ch: char,
        limit: usize,
        category: Option<&MemoryCategory>,
    ) -> Vec<ScoredEntry> {
        let mut scored = Vec::new();
        for entry in &self.entries {
            if !passes_filter(entry, category) {
                continue;
            }
            let body = entry.content.matches(ch).count() as f64;
            let heading = entry
                .heading
                .as_deref()
                .map(|h| h.matches(ch).count())
                .unwrap_or(0) as f64;
            let score = body + 2.0 * heading;
            if score > 0.0 {
                scored.push(ScoredEntry {
                    entry: entry.clone(),
                    score,
                });
            }
        }
        sort_scored(&mut scored);
        scored.truncate(limit);
        scored
    }

    fn search_impl(
        &self,
        query: &str,
        limit: usize,
        category: Option<&MemoryCategory>,
    ) -> Vec<ScoredEntry> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        if let Some(ch) = single_cjk_char(query) {
            return self.search_single_char(ch, limit, category);
        }

        let tokens = tokenize(query);
        if tokens.is_empty() {
            return Vec::new();
        }

        const K1: f64 = 1.2;
        const B: f64 = 0.75;
        let n = self.entries.len() as f64;
        // Deduplicate and visit tokens in sorted order: float addition is not
        // associative, and a `HashSet`'s randomized iteration order could make
        // near-tie scores depend on the process. Sorting keeps repeated
        // searches bit-for-bit identical.
        let mut unique: Vec<&String> = tokens.iter().collect();
        unique.sort();
        unique.dedup();
        let mut scores: HashMap<usize, f64> = HashMap::new();

        for token in unique {
            let Some(list) = self.postings.get(token) else {
                continue;
            };
            let df = list.len() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for posting in list {
                let len = self.lengths[posting.entry] as f64;
                let tf = posting.tf as f64;
                let denom = tf + K1 * (1.0 - B + B * len / self.avg_len);
                *scores.entry(posting.entry).or_insert(0.0) += idf * (tf * (K1 + 1.0)) / denom;
            }
        }

        let mut scored: Vec<ScoredEntry> = scores
            .into_iter()
            .filter_map(|(idx, score)| {
                let entry = self.entries.get(idx)?.clone();
                passes_filter(&entry, category).then_some(ScoredEntry { entry, score })
            })
            .collect();

        sort_scored(&mut scored);
        scored.truncate(limit);
        scored
    }
}

#[async_trait]
impl MemoryIndex for KeywordIndex {
    async fn rebuild(&mut self, documents: &[DocumentSnapshot]) -> Result<()> {
        *self = Self::build(documents);
        Ok(())
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
        category: Option<&MemoryCategory>,
    ) -> Vec<ScoredEntry> {
        self.search_impl(query, limit, category)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(category: MemoryCategory, name: Option<&str>, content: &str) -> DocumentSnapshot {
        let source = source_path(&category, name);
        DocumentSnapshot {
            category,
            name: name.map(str::to_string),
            source,
            content: content.to_string(),
        }
    }

    fn core(content: &str) -> DocumentSnapshot {
        doc(MemoryCategory::Core, None, content)
    }

    // -- tokenization -------------------------------------------------------

    #[test]
    fn test_tokenize_latin_lowercases_and_splits() {
        let tokens = tokenize("Deploy the Service, v2!");
        assert_eq!(tokens, vec!["deploy", "the", "service", "v2"]);
    }

    #[test]
    fn test_tokenize_drops_short_latin_tokens() {
        assert_eq!(tokenize("a b c d"), Vec::<String>::new());
        assert_eq!(tokenize("ok a"), vec!["ok"]);
    }

    #[test]
    fn test_tokenize_chinese_produces_bigrams() {
        assert_eq!(tokenize("部署流程"), vec!["部署", "署流", "流程"]);
    }

    #[test]
    fn test_tokenize_single_chinese_char_is_unigram() {
        assert_eq!(tokenize("部"), vec!["部"]);
    }

    #[test]
    fn test_tokenize_mixed_language_keeps_both_halves() {
        let tokens = tokenize("部署 deploy 流程");
        assert!(tokens.contains(&"部署".to_string()));
        assert!(tokens.contains(&"流程".to_string()));
        assert!(tokens.contains(&"deploy".to_string()));
    }

    // -- parsing ------------------------------------------------------------

    #[test]
    fn test_parse_blank_line_separates_entries() {
        let entries = parse_document(&core("# Title\n\nFirst fact.\n\nSecond fact.\n"));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "First fact.");
        assert_eq!(entries[1].content, "Second fact.");
    }

    #[test]
    fn test_parse_records_nearest_heading() {
        let entries = parse_document(&core("# Top\n\nIntro.\n\n## Detail\n\nBody.\n"));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].heading.as_deref(), Some("Top"));
        assert_eq!(entries[1].heading.as_deref(), Some("Detail"));
    }

    #[test]
    fn test_parse_heading_line_produces_no_entry() {
        let entries = parse_document(&core("# Only a heading\n"));
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_empty_document_produces_no_entries() {
        assert!(parse_document(&core("")).is_empty());
        assert!(parse_document(&core("   \n\n  \n")).is_empty());
    }

    #[test]
    fn test_parse_hash_without_space_is_not_heading() {
        let entries = parse_document(&core("#hashtag content here\n"));
        assert_eq!(entries.len(), 1);
        assert!(entries[0].heading.is_none());
    }

    #[test]
    fn test_parse_consecutive_lines_form_one_entry() {
        let entries = parse_document(&core("line one\nline two\nline three\n"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "line one\nline two\nline three");
    }

    #[test]
    fn test_parse_code_fence_is_one_entry_not_split_by_blank_lines() {
        let entries = parse_document(&core(
            "Before.\n\n```bash\necho one\n\necho two\n```\n\nAfter.\n",
        ));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].content, "echo one\n\necho two");
    }

    #[test]
    fn test_parse_code_fence_content_is_indexed_without_marker_tokens() {
        let index = KeywordIndex::build(&[core("```bash\ncargo test\n```\n")]);
        assert_eq!(index.search_impl("cargo", 10, None).len(), 1);
        // The fence marker and its info string are not indexed.
        assert!(index.search_impl("bash", 10, None).is_empty());
    }

    #[test]
    fn test_parse_skill_frontmatter_is_stripped() {
        let skill = doc(
            MemoryCategory::Skill,
            Some("deploy"),
            "---\nname: deploy\ndescription: Deploy steps.\n---\n\nRun the deploy script.\n",
        );
        let entries = parse_document(&skill);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "Run the deploy script.");
        assert!(entries[0].heading.is_none());
    }

    #[test]
    fn test_parse_skill_without_frontmatter_is_kept() {
        let skill = doc(MemoryCategory::Skill, Some("plain"), "Just a body.\n");
        let entries = parse_document(&skill);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "Just a body.");
    }

    #[test]
    fn test_parse_non_skill_frontmatter_is_not_stripped() {
        let entries = parse_document(&core("---\nplain: doc\n---\n\nBody.\n"));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "---\nplain: doc\n---");
        assert_eq!(entries[1].content, "Body.");
    }

    #[test]
    fn test_entry_hash_ignores_whitespace_differences() {
        let a = parse_document(&core("alpha beta"));
        let b = parse_document(&core("alpha\n   beta"));
        assert_eq!(a[0].hash, b[0].hash);
    }

    // -- ranking ------------------------------------------------------------

    #[test]
    fn test_search_returns_only_matching_entries() {
        let index = KeywordIndex::build(&[
            core("The deploy pipeline runs nightly."),
            core("Cats are independent animals."),
        ]);
        let hits = index.search_impl("deploy pipeline", 10, None);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].entry.content.contains("deploy pipeline"));
    }

    #[test]
    fn test_search_relevance_ranks_repeated_term_first() {
        let index = KeywordIndex::build(&[
            core("Rust rust rust systems language."),
            core("Rust appears once here."),
        ]);
        let hits = index.search_impl("rust", 10, None);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].entry.content.contains("systems language"));
    }

    #[test]
    fn test_search_heading_hit_outranks_body_hit() {
        let index = KeywordIndex::build(&[core(
            "## Deployment\n\nNothing relevant.\n\nA paragraph about deployment.\n",
        )]);
        let hits = index.search_impl("deployment", 10, None);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entry.heading.as_deref(), Some("Deployment"));
    }

    #[test]
    fn test_search_is_order_stable_across_repeats() {
        let index = KeywordIndex::build(&[
            core("alpha beta"),
            doc(MemoryCategory::User, None, "alpha beta"),
            doc(MemoryCategory::Agent, None, "alpha beta"),
        ]);
        let first = index.search_impl("alpha", 10, None);
        let second = index.search_impl("alpha", 10, None);
        let order = |hits: &[ScoredEntry]| {
            hits.iter()
                .map(|h| h.entry.source.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&first), order(&second));
    }

    #[test]
    fn test_search_multi_token_order_is_repeatable() {
        let index = KeywordIndex::build(&[
            core("alpha beta gamma"),
            doc(MemoryCategory::User, None, "alpha beta"),
            doc(MemoryCategory::Agent, None, "beta gamma"),
        ]);
        let order = |hits: &[ScoredEntry]| {
            hits.iter()
                .map(|h| h.entry.source.clone())
                .collect::<Vec<_>>()
        };
        let first = index.search_impl("alpha beta gamma", 10, None);
        for _ in 0..10 {
            let again = index.search_impl("alpha beta gamma", 10, None);
            assert_eq!(order(&first), order(&again));
        }
    }

    #[test]
    fn test_search_ties_break_by_category_then_source() {
        let index = KeywordIndex::build(&[
            doc(MemoryCategory::User, None, "alpha beta"),
            doc(MemoryCategory::Agent, None, "alpha beta"),
        ]);
        let hits = index.search_impl("alpha", 10, None);
        // User precedes Agent in the canonical category order.
        assert_eq!(hits[0].entry.category, MemoryCategory::User);
        assert_eq!(hits[1].entry.category, MemoryCategory::Agent);
    }

    #[test]
    fn test_search_or_semantics_keeps_partial_matches() {
        let index = KeywordIndex::build(&[core("alpha only"), core("bravo only")]);
        let hits = index.search_impl("alpha bravo", 10, None);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn test_search_category_filter_narrows_results() {
        let index = KeywordIndex::build(&[
            core("shared keyword"),
            doc(MemoryCategory::User, None, "shared keyword"),
        ]);
        let hits = index.search_impl("shared", 10, Some(&MemoryCategory::User));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.category, MemoryCategory::User);
    }

    #[test]
    fn test_search_limit_truncates() {
        let docs: Vec<_> = (0..5)
            .map(|i| core(&format!("keyword number {i}")))
            .collect();
        let index = KeywordIndex::build(&docs);
        assert_eq!(index.search_impl("keyword", 2, None).len(), 2);
    }

    #[test]
    fn test_search_empty_query_returns_nothing() {
        let index = KeywordIndex::build(&[core("some content")]);
        assert!(index.search_impl("", 10, None).is_empty());
        assert!(index.search_impl("   ", 10, None).is_empty());
    }

    #[test]
    fn test_search_no_match_returns_empty() {
        let index = KeywordIndex::build(&[core("some content")]);
        assert!(index.search_impl("absentword", 10, None).is_empty());
    }

    // -- CJK search ---------------------------------------------------------

    #[test]
    fn test_search_chinese_query_matches_chinese_content() {
        let index =
            KeywordIndex::build(&[core("部署流程需要先构建镜像。"), core("无关的一段记忆。")]);
        let hits = index.search_impl("部署", 10, None);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].entry.content.contains("部署"));
    }

    #[test]
    fn test_search_single_chinese_char_falls_back_to_unigram() {
        let index = KeywordIndex::build(&[core("部署流程"), core("无关内容")]);
        let hits = index.search_impl("部", 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.content, "部署流程");
    }

    #[test]
    fn test_search_mixed_language_query_matches_both() {
        let index = KeywordIndex::build(&[core("部署 deploy 流程"), core("unrelated text")]);
        let hits = index.search_impl("部署 deploy", 10, None);
        assert_eq!(hits.len(), 1);
    }

    // -- trait surface ------------------------------------------------------

    #[tokio::test]
    async fn test_rebuild_replaces_previous_state() {
        let mut index = KeywordIndex::new();
        index.rebuild(&[core("alpha")]).await.unwrap();
        assert_eq!(index.search("alpha", 10, None).len(), 1);
        index.rebuild(&[core("bravo")]).await.unwrap();
        assert!(index.search("alpha", 10, None).is_empty());
        assert_eq!(index.search("bravo", 10, None).len(), 1);
    }

    #[test]
    fn test_fingerprint_changes_when_content_changes() {
        let a = fingerprint(&[core("alpha")]);
        let b = fingerprint(&[core("alpha ")]);
        assert_ne!(a, b);
        assert_eq!(a, fingerprint(&[core("alpha")]));
    }
}
