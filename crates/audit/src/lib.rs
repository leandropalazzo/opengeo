//! Epic 32 — site audit crawler and answer-engine readiness / citation-readiness heuristic engine.
//!
//! The boundary is intentionally narrow: crawling fetches owned pages, while
//! scoring is a pure deterministic pass over `(url, html)` fixtures. The rules
//! are documented inline so operators can inspect the exact AEO / citation-readiness
//! heuristics Anseo applies.
//!
//! Rule taxonomy:
//! - Identity (AEO: be findable) — schema.org/JSON-LD, canonical URL, site name
//! - Extractability (AEO: be extractable) — answer blocks, question headings, semantic sections
//! - Corroboration (AEO: be credible) — outbound links, named sources, source context

pub mod central_log;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use anseo_recommendations::kind::RecommendationKind;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("invalid audit target `{0}`")]
    InvalidTarget(String),
    #[error("could not read `{path}`: {source}")]
    ReadFile {
        path: String,
        source: std::io::Error,
    },
    #[error("could not fetch `{url}`: {source}")]
    Fetch { url: String, source: reqwest::Error },
    #[error("audit target returned HTTP {status}: {url}")]
    HttpStatus { url: String, status: u16 },
    #[error("skipping non-HTML response ({content_type}): {url}")]
    NotHtml { url: String, content_type: String },
}

#[derive(Debug, Clone)]
pub struct AuditOptions {
    pub max_pages: usize,
    pub timeout: Duration,
}

impl Default for AuditOptions {
    fn default() -> Self {
        Self {
            max_pages: 25,
            timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInput {
    pub url: String,
    pub html: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditCategory {
    Identity,
    Extractability,
    Corroboration,
}

impl AuditCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Extractability => "extractability",
            Self::Corroboration => "corroboration",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditFinding {
    pub rule_id: String,
    pub category: AuditCategory,
    pub severity: Severity,
    pub status: FindingStatus,
    pub score: u8,
    pub message: String,
    pub recommendation_kind: String,
    pub evidence: Vec<String>,
}

impl AuditFinding {
    pub fn is_violation(&self) -> bool {
        self.status != FindingStatus::Pass
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageAudit {
    pub url: String,
    pub title: Option<String>,
    pub score: u8,
    pub findings: Vec<AuditFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    pub target: String,
    pub overall_score: u8,
    pub pages: Vec<PageAudit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateSummary>,
}

impl AuditReport {
    pub fn with_gate(mut self, gate: GateSummary) -> Self {
        self.gate = Some(gate);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateSummary {
    pub passed: bool,
    pub fail_on: Vec<String>,
    pub failed_findings: Vec<GateFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateFinding {
    pub page_url: String,
    pub rule_id: String,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailOn {
    SeverityAtLeast(Severity),
    Rule(String),
}

impl FailOn {
    pub fn parse(value: &str) -> Self {
        Severity::parse(value)
            .map(Self::SeverityAtLeast)
            .unwrap_or_else(|| Self::Rule(value.trim().to_string()))
    }

    pub fn label(&self) -> String {
        match self {
            Self::SeverityAtLeast(sev) => sev.as_str().to_string(),
            Self::Rule(rule) => rule.clone(),
        }
    }

    fn matches(&self, finding: &AuditFinding) -> bool {
        match self {
            Self::SeverityAtLeast(sev) => finding.severity >= *sev,
            Self::Rule(rule) => finding.rule_id == *rule,
        }
    }
}

pub fn evaluate_gate(report: &AuditReport, fail_on: &[FailOn]) -> GateSummary {
    let mut failed_findings = Vec::new();
    if !fail_on.is_empty() {
        for page in &report.pages {
            for finding in &page.findings {
                if finding.is_violation() && fail_on.iter().any(|gate| gate.matches(finding)) {
                    failed_findings.push(GateFinding {
                        page_url: page.url.clone(),
                        rule_id: finding.rule_id.clone(),
                        severity: finding.severity,
                        message: finding.message.clone(),
                    });
                }
            }
        }
    }

    GateSummary {
        passed: failed_findings.is_empty(),
        fail_on: fail_on.iter().map(FailOn::label).collect(),
        failed_findings,
    }
}

/// Site-level metadata produced by the crawler that supplements per-page HTML facts.
#[derive(Debug, Clone, Default)]
pub struct CrawlMeta {
    /// `true` when `GET <root>/llms.txt` returns a 2xx response.
    pub llms_txt_present: bool,
}

pub async fn crawl_and_audit(
    target: &str,
    options: AuditOptions,
) -> Result<AuditReport, AuditError> {
    let (pages, meta) = Crawler::new(options).crawl(target).await?;
    Ok(audit_pages(target, pages, meta))
}

pub fn audit_pages(target: &str, mut pages: Vec<PageInput>, meta: CrawlMeta) -> AuditReport {
    pages.sort_by(|a, b| a.url.cmp(&b.url));
    pages.dedup_by(|a, b| a.url == b.url);

    let pages: Vec<PageAudit> = pages
        .into_iter()
        .map(|p| audit_page(p, meta.llms_txt_present))
        .collect();
    let overall_score = if pages.is_empty() {
        0
    } else {
        round_score(
            pages.iter().map(|p| u32::from(p.score)).sum::<u32>(),
            pages.len() as u32,
        )
    };

    AuditReport {
        target: target.to_string(),
        overall_score,
        pages,
        gate: None,
    }
}

pub struct Crawler {
    options: AuditOptions,
    client: reqwest::Client,
}

impl Crawler {
    pub fn new(options: AuditOptions) -> Self {
        let client = reqwest::Client::builder()
            .timeout(options.timeout)
            .user_agent("anseo-audit/0.6")
            .build()
            .expect("reqwest client builder");
        Self { options, client }
    }

    pub async fn crawl(&self, target: &str) -> Result<(Vec<PageInput>, CrawlMeta), AuditError> {
        let fetched = self.fetch(target).await?;
        let pages = if looks_like_sitemap(target, &fetched.html) {
            self.crawl_sitemap(target, &fetched.html).await?
        } else {
            let Some(root) = parse_web_url(&fetched.url) else {
                return Ok((vec![fetched], CrawlMeta::default()));
            };

            let mut pages = BTreeMap::new();
            let mut seen = BTreeSet::new();
            let mut queue = VecDeque::from([root.clone()]);
            seen.insert(normalize_url(root.clone()));

            while let Some(url) = queue.pop_front() {
                if pages.len() >= self.options.max_pages {
                    break;
                }
                let page = match self.fetch(url.as_str()).await {
                    Ok(page) => page,
                    Err(_) => continue,
                };
                let links = owned_links(&root, &url, &page.html);
                pages.insert(normalize_url(url.clone()), page);
                for link in links {
                    let normalized = normalize_url(link.clone());
                    if seen.insert(normalized) {
                        queue.push_back(link);
                    }
                }
            }

            pages.into_values().collect()
        };

        // Probe for llms.txt at the site root (site-level AEO signal).
        let llms_txt_present = self.probe_llms_txt(target).await;
        Ok((pages, CrawlMeta { llms_txt_present }))
    }

    async fn probe_llms_txt(&self, target: &str) -> bool {
        let Some(root) = parse_web_url(target) else {
            return false;
        };
        let llms_url = format!("{}/llms.txt", root.origin().ascii_serialization());
        self.client
            .get(&llms_url)
            .send()
            .await
            .ok()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    async fn crawl_sitemap(&self, target: &str, xml: &str) -> Result<Vec<PageInput>, AuditError> {
        let root = parse_web_url(target);
        let mut urls = sitemap_locs(xml);
        urls.sort();
        urls.dedup();

        let mut pages = Vec::new();
        for loc in urls.into_iter().take(self.options.max_pages) {
            if let (Some(root), Some(candidate)) = (root.as_ref(), parse_web_url(&loc)) {
                if !same_origin(root, &candidate) {
                    continue;
                }
            }
            if let Ok(page) = self.fetch(&loc).await {
                pages.push(page);
            }
        }
        Ok(pages)
    }

    async fn fetch(&self, target: &str) -> Result<PageInput, AuditError> {
        if let Some(path) = local_path(target) {
            let html = std::fs::read_to_string(&path).map_err(|source| AuditError::ReadFile {
                path: path.display().to_string(),
                source,
            })?;
            return Ok(PageInput {
                url: path.display().to_string(),
                html,
            });
        }

        let url = Url::parse(target).map_err(|_| AuditError::InvalidTarget(target.to_string()))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(AuditError::InvalidTarget(target.to_string()));
        }

        let resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|source| AuditError::Fetch {
                url: target.to_string(),
                source,
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AuditError::HttpStatus {
                url: target.to_string(),
                status: status.as_u16(),
            });
        }
        // Only audit HTML pages; skip JS, CSS, fonts, images, and other assets.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        if !content_type.is_empty() && !content_type.starts_with("text/html") {
            return Err(AuditError::NotHtml {
                url: url.to_string(),
                content_type,
            });
        }
        let final_url = resp.url().to_string();
        let html = resp.text().await.map_err(|source| AuditError::Fetch {
            url: final_url.clone(),
            source,
        })?;
        Ok(PageInput {
            url: final_url,
            html,
        })
    }
}

fn audit_page(page: PageInput, llms_txt_present: bool) -> PageAudit {
    let title = extract_tag_text(&page.html, "title").map(clean_text);
    let doc = DocumentFacts::from_html(&page.url, &page.html);
    let findings = vec![
        rule_schema_org(&doc),
        rule_canonical_url(&doc),
        rule_site_name(&doc),
        rule_llms_txt(&doc, llms_txt_present),
        rule_answer_blocks(&doc),
        rule_question_headings(&doc),
        rule_semantic_sections(&doc),
        rule_outbound_links(&doc),
        rule_named_sources(&doc),
        rule_source_context(&doc),
    ];

    let score = round_score(
        findings.iter().map(|f| u32::from(f.score)).sum::<u32>(),
        findings.len() as u32,
    );

    PageAudit {
        url: page.url,
        title,
        score,
        findings,
    }
}

struct DocumentFacts {
    lower_html: String,
    text: String,
    title: Option<String>,
    headings: Vec<String>,
    paragraph_count: usize,
    outbound_links: Vec<String>,
    /// `true` when the page declares an AI-crawler policy via `<meta name="robots">`.
    ai_robots_meta: bool,
}

impl DocumentFacts {
    fn from_html(url: &str, html: &str) -> Self {
        let lower_html = html.to_ascii_lowercase();
        let text = clean_text(strip_tags(html));
        let title = extract_tag_text(html, "title").map(clean_text);
        let headings = ["h1", "h2", "h3"]
            .iter()
            .flat_map(|tag| extract_all_tag_text(html, tag))
            .map(clean_text)
            .filter(|s| !s.is_empty())
            .collect();
        let paragraph_count = count_tag(&lower_html, "p");
        let outbound_links = outbound_links(url, html);
        // Detect AI-specific crawler directives in <meta name="robots"> content.
        // Any of these values signals that the operator has explicitly declared
        // an AI crawl policy, which satisfies the identity.llms_txt rule.
        let ai_robots_meta = lower_html.contains("name=\"robots\"")
            && contains_any(
                &lower_html,
                &[
                    "noai",
                    "noimageai",
                    "google-extended",
                    "gptbot",
                    "ccbot",
                    "anthropicbot",
                    "perplexitybot",
                ],
            );
        Self {
            lower_html,
            text,
            title,
            headings,
            paragraph_count,
            outbound_links,
            ai_robots_meta,
        }
    }
}

// Rule: identity.schema_org_json_ld
// Transparent heuristic: pages receive identity credit when they expose
// machine-readable entity data through JSON-LD or schema.org markup. AI answer
// engines can extract organization/product identity more reliably from those
// structured hints than from prose alone.
fn rule_schema_org(doc: &DocumentFacts) -> AuditFinding {
    let pass = contains_any(&doc.lower_html, &["application/ld+json", "schema.org"]);
    finding(
        "identity.schema_org_json_ld",
        AuditCategory::Identity,
        Severity::High,
        pass,
        "Structured identity data is present.",
        "Add JSON-LD or schema.org markup that names the entity being audited.",
        structural_kind(),
        evidence_if(pass, "found JSON-LD/schema.org marker"),
    )
}

// Rule: identity.canonical_url
// Transparent heuristic: canonical URLs reduce duplicate-page ambiguity for
// crawlers and citation systems. A missing canonical is a medium-severity
// identity weakness, not a content failure.
fn rule_canonical_url(doc: &DocumentFacts) -> AuditFinding {
    let pass = contains_any(&doc.lower_html, &["rel=\"canonical\"", "rel='canonical'"]);
    finding(
        "identity.canonical_url",
        AuditCategory::Identity,
        Severity::Medium,
        pass,
        "Canonical URL is declared.",
        "Declare a canonical URL so engines can resolve duplicate page variants.",
        structural_kind(),
        evidence_if(pass, "found rel=canonical"),
    )
}

// Rule: identity.site_name
// Transparent heuristic: a page should expose a stable site/entity name via
// `og:site_name` or a recognizable title separator. This helps engines connect
// the document to the named source they may cite.
fn rule_site_name(doc: &DocumentFacts) -> AuditFinding {
    let title_has_site = doc
        .title
        .as_deref()
        .map(|t| t.contains(" | ") || t.contains(" - "))
        .unwrap_or(false);
    let pass = doc.lower_html.contains("og:site_name") || title_has_site;
    finding(
        "identity.site_name",
        AuditCategory::Identity,
        Severity::Medium,
        pass,
        "Stable site name signal is present.",
        "Add `og:site_name` or include the site/entity name in the page title.",
        structural_kind(),
        evidence_if(pass, "found og:site_name or title site separator"),
    )
}

// Rule: identity.llms_txt [experimental]
// Transparent heuristic: either a site-level /llms.txt file (200 response) or
// per-page AI-crawler meta directives (noai, gptbot, etc.) declares that the
// operator has considered their AI-crawler policy. Absence is a low-severity
// gap — the llms.txt standard is still a community proposal (llmstxt.org) with
// no formal crawler compliance from major providers.
fn rule_llms_txt(doc: &DocumentFacts, llms_txt_present: bool) -> AuditFinding {
    let pass = llms_txt_present || doc.ai_robots_meta;
    finding(
        "identity.llms_txt",
        AuditCategory::Identity,
        Severity::Low,
        pass,
        "AI-crawler policy is declared (llms.txt or meta robots).",
        "Consider adding /llms.txt to declare your AI-crawler policy [experimental].",
        structural_kind(),
        evidence_if(
            pass,
            if llms_txt_present {
                "found /llms.txt (200)"
            } else {
                "found AI-crawler meta robots directive"
            },
        ),
    )
}

// Rule: extractability.answer_blocks
// Transparent heuristic: answer engines prefer compact extractable answer
// blocks. We credit explicit answer/summary/TLDR containers and list-like
// structures because they can be lifted without rewriting the whole page.
fn rule_answer_blocks(doc: &DocumentFacts) -> AuditFinding {
    let pass = contains_any(
        &doc.lower_html,
        &[
            "class=\"answer",
            "class='answer",
            "id=\"answer",
            "id='answer",
            "summary",
            "tldr",
            "<ol",
            "<dl",
        ],
    ) || doc.text.to_ascii_lowercase().contains("the answer");
    finding(
        "extractability.answer_blocks",
        AuditCategory::Extractability,
        Severity::High,
        pass,
        "Extractable answer block is present.",
        "Add a concise answer, summary, TLDR, ordered list, or definition list.",
        structural_kind(),
        evidence_if(pass, "found answer-like block or list structure"),
    )
}

// Rule: extractability.question_headings
// Transparent heuristic: question-form headings make intent boundaries obvious
// to answer engines. A heading ending in `?` or starting with a common question
// word counts as an extractable question section.
fn rule_question_headings(doc: &DocumentFacts) -> AuditFinding {
    let matched = doc
        .headings
        .iter()
        .find(|h| is_question_heading(h))
        .cloned();
    let pass = matched.is_some();
    finding(
        "extractability.question_headings",
        AuditCategory::Extractability,
        Severity::Medium,
        pass,
        "Question-style heading is present.",
        "Use question-form H1/H2/H3 headings for answerable sections.",
        structural_kind(),
        matched.into_iter().collect(),
    )
}

// Rule: extractability.semantic_sections
// Transparent heuristic: at least two headings and two paragraphs indicate that
// the page has a scannable section hierarchy instead of one undifferentiated
// content blob.
fn rule_semantic_sections(doc: &DocumentFacts) -> AuditFinding {
    let pass = doc.headings.len() >= 2 && doc.paragraph_count >= 2;
    finding(
        "extractability.semantic_sections",
        AuditCategory::Extractability,
        Severity::Low,
        pass,
        "Semantic section structure is present.",
        "Use multiple headings and paragraphs so content can be chunked cleanly.",
        structural_kind(),
        evidence_if(
            pass,
            &format!(
                "{} headings, {} paragraphs",
                doc.headings.len(),
                doc.paragraph_count
            ),
        ),
    )
}

// Rule: corroboration.outbound_links
// Transparent heuristic: outbound links to other domains show corroboration
// pathways. We do not judge authority; we only require that named external
// evidence exists for AI systems and humans to inspect.
fn rule_outbound_links(doc: &DocumentFacts) -> AuditFinding {
    let pass = !doc.outbound_links.is_empty();
    finding(
        "corroboration.outbound_links",
        AuditCategory::Corroboration,
        Severity::High,
        pass,
        "Outbound corroboration links are present.",
        "Link to external sources that corroborate claims on this page.",
        citation_kind(),
        doc.outbound_links.iter().take(3).cloned().collect(),
    )
}

// Rule: corroboration.named_sources
// Transparent heuristic: prose should name source context ("Source:",
// "according to", "references", etc.) so engines can tell claims apart from
// unsupported marketing copy.
fn rule_named_sources(doc: &DocumentFacts) -> AuditFinding {
    let lower_text = doc.text.to_ascii_lowercase();
    let pass = contains_any(
        &lower_text,
        &[
            "source:",
            "according to",
            "references",
            "bibliography",
            "cited by",
        ],
    );
    finding(
        "corroboration.named_sources",
        AuditCategory::Corroboration,
        Severity::Medium,
        pass,
        "Named source language is present.",
        "Name sources in surrounding prose, for example `Source:` or `According to ...`.",
        citation_kind(),
        evidence_if(pass, "found named-source phrase"),
    )
}

// Rule: corroboration.source_context
// Transparent heuristic: an outbound link is stronger when nearby HTML labels it
// as a source, reference, study, report, or citation. This keeps the rule
// deterministic without trying to infer source quality.
fn rule_source_context(doc: &DocumentFacts) -> AuditFinding {
    let pass = !doc.outbound_links.is_empty()
        && contains_any(
            &doc.lower_html,
            &["source", "reference", "study", "report", "citation"],
        );
    finding(
        "corroboration.source_context",
        AuditCategory::Corroboration,
        Severity::Medium,
        pass,
        "Outbound links have source context.",
        "Label corroborating links as sources, references, studies, reports, or citations.",
        citation_kind(),
        evidence_if(pass, "found source-context label near page links"),
    )
}

#[allow(clippy::too_many_arguments)]
fn finding(
    rule_id: &str,
    category: AuditCategory,
    severity: Severity,
    pass: bool,
    pass_message: &str,
    fail_message: &str,
    recommendation_kind: RecommendationKind,
    evidence: Vec<String>,
) -> AuditFinding {
    let status = if pass {
        FindingStatus::Pass
    } else if severity == Severity::High {
        FindingStatus::Fail
    } else {
        FindingStatus::Warn
    };
    AuditFinding {
        rule_id: rule_id.to_string(),
        category,
        severity,
        status,
        score: if pass { 100 } else { 0 },
        message: if pass { pass_message } else { fail_message }.to_string(),
        recommendation_kind: recommendation_kind.as_str().to_string(),
        evidence,
    }
}

fn structural_kind() -> RecommendationKind {
    RecommendationKind::StructuralContentSuggestion
}

fn citation_kind() -> RecommendationKind {
    RecommendationKind::CitationQualityUplift
}

fn round_score(total: u32, count: u32) -> u8 {
    (total + (count / 2))
        .checked_div(count)
        .unwrap_or(0)
        .min(100) as u8
}

fn evidence_if(pass: bool, value: &str) -> Vec<String> {
    if pass {
        vec![value.to_string()]
    } else {
        Vec::new()
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn local_path(target: &str) -> Option<std::path::PathBuf> {
    if let Ok(url) = Url::parse(target) {
        if url.scheme() == "file" {
            return url.to_file_path().ok();
        }
        return None;
    }
    let path = Path::new(target);
    path.exists().then(|| path.to_path_buf())
}

fn parse_web_url(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn normalize_url(mut url: Url) -> String {
    url.set_fragment(None);
    url.to_string()
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

fn looks_like_sitemap(target: &str, body: &str) -> bool {
    target.ends_with(".xml") || body.to_ascii_lowercase().contains("<urlset")
}

fn sitemap_locs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = xml.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(start_rel) = lower[cursor..].find("<loc>") {
        let start = cursor + start_rel + "<loc>".len();
        let Some(end_rel) = lower[start..].find("</loc>") else {
            break;
        };
        let end = start + end_rel;
        out.push(xml[start..end].trim().to_string());
        cursor = end + "</loc>".len();
    }
    out
}

/// Returns `false` for URLs that are obviously static assets rather than HTML
/// pages: Next.js build chunks, stylesheets, fonts, images, and manifests.
/// Keeps the crawl queue clean so operators see only meaningful page results.
fn is_crawlable_url(url: &Url) -> bool {
    let path = url.path().to_lowercase();
    // Skip Next.js build output and other common static asset directories.
    if path.starts_with("/_next/") || path.starts_with("/static/") {
        return false;
    }
    // Skip by file extension (strip query string first).
    let path_no_qs = path.split('?').next().unwrap_or(&path);
    const SKIP_EXT: &[&str] = &[
        ".js",
        ".mjs",
        ".cjs",
        ".css",
        ".woff",
        ".woff2",
        ".ttf",
        ".eot",
        ".otf",
        ".png",
        ".jpg",
        ".jpeg",
        ".gif",
        ".svg",
        ".ico",
        ".webp",
        ".avif",
        ".webmanifest",
        ".zip",
        ".gz",
        ".tar",
        ".mp4",
        ".mp3",
        ".webm",
        ".pdf",
    ];
    !SKIP_EXT.iter().any(|ext| path_no_qs.ends_with(ext))
}

fn owned_links(root: &Url, page_url: &Url, html: &str) -> Vec<Url> {
    let mut links: Vec<Url> = href_values(html)
        .into_iter()
        .filter_map(|href| page_url.join(&href).ok())
        .filter(|url| same_origin(root, url))
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .filter(is_crawlable_url)
        .collect();
    links.sort_by_key(|url| normalize_url(url.clone()));
    links.dedup_by_key(|url| normalize_url(url.clone()));
    links
}

fn outbound_links(page_url: &str, html: &str) -> Vec<String> {
    let Some(root) = parse_web_url(page_url) else {
        return Vec::new();
    };
    let mut links: Vec<String> = href_values(html)
        .into_iter()
        .filter_map(|href| root.join(&href).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .filter(|url| !same_origin(&root, url))
        .map(normalize_url)
        .collect();
    links.sort();
    links.dedup();
    links
}

fn href_values(html: &str) -> Vec<String> {
    attr_values(html, "href")
}

fn attr_values(html: &str, attr: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let needle = format!("{attr}=");
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = lower[cursor..].find(&needle) {
        let mut start = cursor + rel + needle.len();
        let bytes = html.as_bytes();
        while start < bytes.len() && bytes[start].is_ascii_whitespace() {
            start += 1;
        }
        if start >= bytes.len() {
            break;
        }
        let quote = bytes[start];
        let (value_start, value_end) = if quote == b'\'' || quote == b'"' {
            let value_start = start + 1;
            let Some(end_rel) = html[value_start..].find(quote as char) else {
                break;
            };
            (value_start, value_start + end_rel)
        } else {
            let value_start = start;
            let value_end = html[value_start..]
                .find(|c: char| c.is_ascii_whitespace() || c == '>')
                .map(|rel| value_start + rel)
                .unwrap_or(html.len());
            (value_start, value_end)
        };
        out.push(html[value_start..value_end].trim().to_string());
        cursor = value_end.saturating_add(1);
    }
    out
}

fn count_tag(lower_html: &str, tag: &str) -> usize {
    lower_html.matches(&format!("<{tag}")).count()
}

fn extract_tag_text(html: &str, tag: &str) -> Option<String> {
    extract_all_tag_text(html, tag).into_iter().next()
}

fn extract_all_tag_text(html: &str, tag: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(open_rel) = lower[cursor..].find(&open) {
        let tag_start = cursor + open_rel;
        let Some(content_rel) = lower[tag_start..].find('>') else {
            break;
        };
        let content_start = tag_start + content_rel + 1;
        let Some(close_rel) = lower[content_start..].find(&close) else {
            break;
        };
        let content_end = content_start + close_rel;
        out.push(strip_tags(&html[content_start..content_end]));
        cursor = content_end + close.len();
    }
    out
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn clean_text(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_question_heading(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.ends_with('?') {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    [
        "what ", "how ", "why ", "who ", "when ", "where ", "is ", "can ", "does ",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strong_page(url: &str) -> PageInput {
        PageInput {
            url: url.to_string(),
            html: r#"
                <html>
                  <head>
                    <title>How Anseo Works | Anseo</title>
                    <link rel="canonical" href="https://example.com/how-anseo-works">
                    <meta property="og:site_name" content="Anseo">
                    <script type="application/ld+json">{"@context":"https://schema.org"}</script>
                  </head>
                  <body>
                    <h1>How does OpenGEO improve citation readiness?</h1>
                    <p class="answer">The answer is structured monitoring with clear sources.</p>
                    <h2>References</h2>
                    <p>According to the public report, this method improves evidence trails.</p>
                    <a href="https://research.example/report">Source report</a>
                  </body>
                </html>
            "#
            .to_string(),
        }
    }

    #[test]
    fn fixed_input_scores_deterministically() {
        let pages = vec![
            strong_page("https://example.com/b"),
            strong_page("https://example.com/a"),
        ];
        let a = audit_pages("https://example.com", pages.clone(), CrawlMeta::default());
        let b = audit_pages("https://example.com", pages, CrawlMeta::default());
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap()
        );
        // strong_page passes 9 of 10 rules (identity.llms_txt is warn by default).
        assert_eq!(a.overall_score, 90);
        assert_eq!(a.pages[0].url, "https://example.com/a");
    }

    #[test]
    fn sparse_page_fires_rules_across_citation_trinity() {
        let report = audit_pages(
            "https://example.com",
            vec![PageInput {
                url: "https://example.com".to_string(),
                html: "<html><head><title>Acme</title></head><body><p>Buy now.</p></body></html>"
                    .to_string(),
            }],
            CrawlMeta::default(),
        );
        let page = &report.pages[0];
        assert!(page.score < 50);
        assert!(page
            .findings
            .iter()
            .any(|f| { f.category == AuditCategory::Identity && f.status != FindingStatus::Pass }));
        assert!(page.findings.iter().any(|f| {
            f.category == AuditCategory::Extractability && f.status != FindingStatus::Pass
        }));
        assert!(page.findings.iter().any(|f| {
            f.category == AuditCategory::Corroboration && f.status != FindingStatus::Pass
        }));
    }

    #[test]
    fn findings_map_to_recommendation_kind_taxonomy() {
        let report = audit_pages(
            "https://example.com",
            vec![strong_page("https://example.com")],
            CrawlMeta::default(),
        );
        let kinds: BTreeSet<&str> = report.pages[0]
            .findings
            .iter()
            .map(|f| f.recommendation_kind.as_str())
            .collect();
        assert!(kinds.contains("structural_content_suggestion"));
        assert!(kinds.contains("citation_quality_uplift"));
    }

    #[test]
    fn fail_on_matches_severity_threshold_or_rule() {
        let report = audit_pages(
            "https://example.com",
            vec![PageInput {
                url: "https://example.com".to_string(),
                html: "<html><body>thin page</body></html>".to_string(),
            }],
            CrawlMeta::default(),
        );
        let high_gate = evaluate_gate(&report, &[FailOn::parse("high")]);
        assert!(!high_gate.passed);
        assert!(high_gate
            .failed_findings
            .iter()
            .all(|f| f.severity == Severity::High));

        let rule_gate = evaluate_gate(&report, &[FailOn::parse("identity.canonical_url")]);
        assert!(!rule_gate.passed);
        assert_eq!(
            rule_gate.failed_findings[0].rule_id,
            "identity.canonical_url"
        );
    }

    #[test]
    fn crawlable_url_filters_static_assets() {
        let html_page = Url::parse("https://example.com/about").unwrap();
        assert!(is_crawlable_url(&html_page));

        // Next.js build chunks must be excluded.
        let next_js = Url::parse("https://example.com/_next/static/chunks/main.js").unwrap();
        assert!(!is_crawlable_url(&next_js));

        let next_css = Url::parse("https://example.com/_next/static/css/styles.css").unwrap();
        assert!(!is_crawlable_url(&next_css));

        let next_font = Url::parse("https://example.com/_next/static/media/font.woff2").unwrap();
        assert!(!is_crawlable_url(&next_font));

        // Icon, manifest, and other static assets must be excluded.
        let icon = Url::parse("https://example.com/icon.png").unwrap();
        assert!(!is_crawlable_url(&icon));

        let manifest = Url::parse("https://example.com/manifest.webmanifest").unwrap();
        assert!(!is_crawlable_url(&manifest));

        // Query strings on non-HTML extensions are also excluded.
        let icon_qs = Url::parse("https://example.com/apple-icon.png?v=2").unwrap();
        assert!(!is_crawlable_url(&icon_qs));
    }

    #[test]
    fn owned_links_excludes_static_assets() {
        let root = Url::parse("https://example.com/").unwrap();
        let page = Url::parse("https://example.com/").unwrap();
        // Typical Next.js HTML head contains stylesheet, manifest, and icon links.
        let links = owned_links(
            &root,
            &page,
            r#"
            <link rel="stylesheet" href="/_next/static/css/main.css">
            <link rel="manifest" href="/manifest.webmanifest">
            <link rel="apple-touch-icon" href="/apple-icon.png">
            <a href="/about">About</a>
            <a href="/methodology">Methodology</a>
            "#,
        );
        let urls: Vec<String> = links.into_iter().map(normalize_url).collect();
        // Only real HTML pages should survive.
        assert_eq!(
            urls,
            vec![
                "https://example.com/about",
                "https://example.com/methodology",
            ]
        );
    }

    // --- identity.llms_txt tests (AC 1–4, story 50.1) ---

    #[test]
    fn rule_llms_txt_passes_when_llms_txt_present() {
        let doc = DocumentFacts::from_html(
            "https://example.com",
            "<html><body>no ai meta here</body></html>",
        );
        let finding = rule_llms_txt(&doc, true);
        assert_eq!(finding.rule_id, "identity.llms_txt");
        assert_eq!(finding.status, FindingStatus::Pass);
        assert_eq!(finding.category, AuditCategory::Identity);
        assert!(finding.evidence[0].contains("/llms.txt"));
    }

    #[test]
    fn rule_llms_txt_passes_when_ai_robots_meta_present() {
        let html = r#"<html><head>
            <meta name="robots" content="noimageai, noindex">
        </head><body></body></html>"#;
        let doc = DocumentFacts::from_html("https://example.com", html);
        assert!(doc.ai_robots_meta, "should detect noimageai in meta robots");
        let finding = rule_llms_txt(&doc, false);
        assert_eq!(finding.status, FindingStatus::Pass);
        assert!(finding.evidence[0].contains("meta robots"));
    }

    #[test]
    fn rule_llms_txt_warns_when_neither_signal_present() {
        let doc = DocumentFacts::from_html(
            "https://example.com",
            "<html><head><meta name=\"robots\" content=\"noindex\"></head><body></body></html>",
        );
        // noindex is not an AI-specific directive
        assert!(!doc.ai_robots_meta);
        let finding = rule_llms_txt(&doc, false);
        assert_eq!(
            finding.status,
            FindingStatus::Warn,
            "Low severity → Warn not Fail"
        );
        assert_eq!(finding.severity, Severity::Low);
        assert!(finding.message.contains("[experimental]"));
    }

    #[test]
    fn audit_pages_produces_ten_findings_per_page() {
        let report = audit_pages(
            "https://example.com",
            vec![strong_page("https://example.com")],
            CrawlMeta::default(),
        );
        assert_eq!(
            report.pages[0].findings.len(),
            10,
            "audit_page must call all 10 rules"
        );
        assert!(
            report.pages[0]
                .findings
                .iter()
                .any(|f| f.rule_id == "identity.llms_txt"),
            "identity.llms_txt must appear in findings"
        );
    }

    #[test]
    fn owned_link_extraction_stays_on_origin_and_is_ordered() {
        let root = Url::parse("https://example.com/").unwrap();
        let page = Url::parse("https://example.com/docs/index.html").unwrap();
        let links = owned_links(
            &root,
            &page,
            r#"
            <a href="/b">b</a>
            <a href="/a#frag">a</a>
            <a href="https://other.example/x">x</a>
            <a href="/a">a duplicate</a>
            "#,
        );
        let urls: Vec<String> = links.into_iter().map(normalize_url).collect();
        assert_eq!(
            urls,
            vec!["https://example.com/a", "https://example.com/b",]
        );
    }
}
