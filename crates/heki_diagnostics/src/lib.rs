//! Diagnostics, Ruff/`t-linter`-compatible output formats, inline `# heki: ignore[...]` parsing,
//! and line-number-independent Ratchet baselines (ADR-015 / ADR-017).

mod fix;

pub use fix::{apply_fixes, unified_diff};

use heki_ast::{LineIndex, Span};
use heki_env::Severity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Related jump location (e.g., each hop in an `EFF101` or `MUT201` call chain for LSP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedLocation {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
    /// Byte span in `path` (the LSP server converts it with the file's own text).
    pub span: Span,
    pub message: String,
}

/// Minimal byte-span text edit (preserves formatting and comments).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub span: Span,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
    pub span: Span,
    pub qualname: String,
    pub rule_code: String,
    pub rule_slug: String,
    pub target: String,
    pub severity: Severity,
    pub message: String,
    pub evidence: Option<String>,
    pub blocker: Option<String>,
    pub related: Vec<RelatedLocation>,
    pub fix: Option<Vec<TextEdit>>,
}

impl Diagnostic {
    /// Deterministic report order: path, position, rule, target.
    pub fn sort_key(&self) -> (&Path, u32, u32, &str, &str) {
        (
            &self.path,
            self.line,
            self.column,
            &self.rule_code,
            &self.target,
        )
    }
}

/// Sort diagnostics into report order.
pub fn sort_diagnostics(diags: &mut [Diagnostic]) {
    diags.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
}

// ------------------------------------------------------------------------------ output formats

/// `--output-format` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Full,
    Concise,
    Json,
    Github,
    Sarif,
}

fn severity_word(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
    }
}

fn display_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Render diagnostics. `source_line(path, line)` returns the 1-based source line for `full`.
pub fn render(
    diags: &[Diagnostic],
    format: OutputFormat,
    source_line: &dyn Fn(&Path, u32) -> Option<String>,
) -> String {
    match format {
        OutputFormat::Full => render_full(diags, source_line),
        OutputFormat::Concise => render_concise(diags),
        OutputFormat::Json => {
            let mut s = serde_json::to_string_pretty(diags).unwrap_or_else(|_| "[]".into());
            s.push('\n');
            s
        }
        OutputFormat::Github => render_github(diags),
        OutputFormat::Sarif => {
            let mut s = serde_json::to_string_pretty(&sarif(diags)).unwrap_or_else(|_| "{}".into());
            s.push('\n');
            s
        }
    }
}

fn render_full(diags: &[Diagnostic], source_line: &dyn Fn(&Path, u32) -> Option<String>) -> String {
    let mut out = String::new();
    for d in diags {
        let _ = writeln!(
            out,
            "{}:{}:{}: {} ({}) [{}] {}",
            display_path(&d.path),
            d.line,
            d.column,
            d.rule_code,
            d.rule_slug,
            severity_word(d.severity),
            d.message
        );
        if let Some(text) = source_line(&d.path, d.line) {
            let gutter = d.line.to_string().len();
            let _ = writeln!(out, "{:gutter$} |", "");
            let _ = writeln!(out, "{} | {}", d.line, text.trim_end());
            let caret_pad = usize::try_from(d.column.saturating_sub(1)).unwrap_or(0);
            let _ = writeln!(out, "{:gutter$} | {:caret_pad$}^", "", "");
        }
        for r in &d.related {
            let _ = writeln!(
                out,
                "  = related: {}:{}:{}: {}",
                display_path(&r.path),
                r.line,
                r.column,
                r.message
            );
        }
        out.push('\n');
    }
    out
}

fn render_concise(diags: &[Diagnostic]) -> String {
    let mut out = String::new();
    for d in diags {
        let _ = writeln!(
            out,
            "{}:{}:{}: {} {}",
            display_path(&d.path),
            d.line,
            d.column,
            d.rule_code,
            d.message
        );
    }
    out
}

/// GitHub workflow-command escaping (`%`, CR, LF; plus `:` / `,` inside properties).
fn gh_escape(s: &str, property: bool) -> String {
    let mut out = s
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A");
    if property {
        out = out.replace(':', "%3A").replace(',', "%2C");
    }
    out
}

fn render_github(diags: &[Diagnostic]) -> String {
    let mut out = String::new();
    for d in diags {
        let _ = writeln!(
            out,
            "::{} file={},line={},col={},title={}::{}",
            severity_word(d.severity),
            gh_escape(&display_path(&d.path), true),
            d.line,
            d.column,
            gh_escape(&format!("heki {}", d.rule_code), true),
            gh_escape(&d.message, false)
        );
    }
    out
}

/// SARIF 2.1.0 log with one run (`runs[0].tool.driver.name = "heki"`).
pub fn sarif(diags: &[Diagnostic]) -> serde_json::Value {
    use serde_json::json;
    let mut rules: BTreeMap<&str, &str> = BTreeMap::new();
    for d in diags {
        rules.insert(&d.rule_code, &d.rule_slug);
    }
    let location = |path: &Path, line: u32, column: u32| {
        json!({
            "physicalLocation": {
                "artifactLocation": { "uri": display_path(path) },
                "region": { "startLine": line, "startColumn": column }
            }
        })
    };
    let results: Vec<serde_json::Value> = diags
        .iter()
        .map(|d| {
            let related: Vec<serde_json::Value> = d
                .related
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let mut loc = location(&r.path, r.line, r.column);
                    loc["id"] = json!(i);
                    loc["message"] = json!({ "text": r.message });
                    loc
                })
                .collect();
            json!({
                "ruleId": d.rule_code,
                "level": severity_word(d.severity),
                "message": { "text": d.message },
                "locations": [location(&d.path, d.line, d.column)],
                "relatedLocations": related,
                "partialFingerprints": {
                    "hekiBaselineKey/v1": format!("{}|{}|{}|{}", display_path(&d.path), d.qualname, d.rule_code, d.target)
                }
            })
        })
        .collect();
    json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "heki",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rules.iter().map(|(code, slug)| json!({
                        "id": code,
                        "name": slug,
                    })).collect::<Vec<_>>()
                }
            },
            "results": results
        }]
    })
}

// ------------------------------------------------------------------------------ suppressions

/// Parse inline suppressions (`# heki: ignore`, `# heki: ignore[EFF101, async-blocking-call]`,
/// and `# noqa` / `# noqa: EFF101` compatibility) from one comment directive.
/// `Some(empty)` suppresses every rule.
pub fn parse_inline_ignores(comment: &str) -> Option<Vec<String>> {
    let trimmed = comment.trim_start_matches('#').trim();
    if let Some(rest) = trimmed.strip_prefix("heki:") {
        let rest = rest.trim();
        if let Some(after) = rest.strip_prefix("ignore") {
            let after = after.trim();
            if after.is_empty() {
                return Some(Vec::new()); // bare ignore = all rules
            }
            if let Some(inner) = after.strip_prefix('[').and_then(|s| s.split(']').next()) {
                let items = inner
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                return Some(items);
            }
        }
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "noqa" {
        return Some(Vec::new());
    }
    if lower.starts_with("noqa:") {
        let items: Vec<String> = trimmed["noqa:".len()..]
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        if !items.is_empty() {
            return Some(items);
        }
    }
    None
}

/// Line-based suppression map of one file (`line -> rules`, empty = all rules).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InlineIgnores {
    lines: BTreeMap<u32, Vec<String>>,
}

impl InlineIgnores {
    /// Build from the file's comment token spans (`heki_ast::ParsedModule::comment_spans`), so
    /// a `#` inside a string literal is never read as a comment. One comment may carry several
    /// directives (`# type: ignore  # heki: ignore[EFF101]`).
    pub fn from_comments(source: &str, comments: &[Span]) -> Self {
        let index = LineIndex::from_source(source);
        let mut lines: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for span in comments {
            let Some(text) = source.get(span.start as usize..span.end as usize) else {
                continue;
            };
            let line = index.line(span.start);
            for part in text.split('#').filter(|p| !p.trim().is_empty()) {
                if let Some(rules) = parse_inline_ignores(part) {
                    let entry = lines.entry(line).or_default();
                    if rules.is_empty() {
                        entry.push("*".into());
                    } else {
                        entry.extend(rules);
                    }
                }
            }
        }
        Self { lines }
    }

    /// True when `line` suppresses the rule (`code` or `slug`, case-insensitive).
    pub fn is_suppressed(&self, line: u32, code: &str, slug: &str) -> bool {
        self.lines.get(&line).is_some_and(|rules| {
            rules
                .iter()
                .any(|r| r == "*" || r.eq_ignore_ascii_case(code) || r.eq_ignore_ascii_case(slug))
        })
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

// ------------------------------------------------------------------------------ ratchet

/// Line-number-independent Ratchet baseline key (generalized from `dcg`'s `generation_store_allowlist.toml`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BaselineKey {
    pub path: String,
    pub qualname: String,
    pub rule_code: String,
    pub target: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatchetBaseline {
    pub counts: BTreeMap<BaselineKey, u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RatchetComparison {
    pub regressions: Vec<(BaselineKey, u32, u32)>,
    pub stale_entries: Vec<(BaselineKey, u32, u32)>,
}

#[derive(Debug, Error)]
pub enum BaselineError {
    #[error("failed to read baseline {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse baseline {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// On-disk shape: `[[entries]]` sorted by key (minimal git diffs).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineFile {
    #[serde(default)]
    entries: Vec<BaselineEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineEntry {
    path: String,
    qualname: String,
    rule_code: String,
    target: String,
    count: u32,
}

impl RatchetBaseline {
    pub fn from_counts(counts: BTreeMap<BaselineKey, u32>) -> Self {
        Self {
            counts: counts.into_iter().filter(|(_, c)| *c > 0).collect(),
        }
    }

    /// Compare current diagnostic counts against baseline (monotonic shrink).
    pub fn compare(&self, current: &BTreeMap<BaselineKey, u32>) -> RatchetComparison {
        let mut out = RatchetComparison::default();
        for (key, &cur) in current {
            let allowed = self.counts.get(key).copied().unwrap_or(0);
            if cur > allowed {
                out.regressions.push((key.clone(), allowed, cur));
            }
        }
        for (key, &allowed) in &self.counts {
            let cur = current.get(key).copied().unwrap_or(0);
            if cur < allowed {
                out.stale_entries.push((key.clone(), allowed, cur));
            }
        }
        out
    }

    pub fn from_toml_str(text: &str, path: &Path) -> Result<Self, BaselineError> {
        let file: BaselineFile = toml::from_str(text).map_err(|source| BaselineError::Toml {
            path: path.to_path_buf(),
            source,
        })?;
        let mut counts = BTreeMap::new();
        for e in file.entries {
            let key = BaselineKey {
                path: e.path,
                qualname: e.qualname,
                rule_code: e.rule_code,
                target: e.target,
            };
            *counts.entry(key).or_insert(0) += e.count;
        }
        Ok(Self { counts })
    }

    pub fn to_toml_string(&self) -> String {
        let file = BaselineFile {
            entries: self
                .counts
                .iter()
                .map(|(k, &count)| BaselineEntry {
                    path: k.path.clone(),
                    qualname: k.qualname.clone(),
                    rule_code: k.rule_code.clone(),
                    target: k.target.clone(),
                    count,
                })
                .collect(),
        };
        let body = toml::to_string(&file).unwrap_or_default();
        format!(
            "# heki Ratchet baseline: counts may only shrink (`heki check --update-baseline`).\n{body}"
        )
    }

    pub fn load(path: &Path) -> Result<Self, BaselineError> {
        let text = std::fs::read_to_string(path).map_err(|source| BaselineError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text, path)
    }

    pub fn save(&self, path: &Path) -> Result<(), BaselineError> {
        std::fs::write(path, self.to_toml_string()).map_err(|source| BaselineError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Baseline key of one diagnostic (`path` = project-relative POSIX path).
pub fn baseline_key(d: &Diagnostic, path: String) -> BaselineKey {
    BaselineKey {
        path,
        qualname: d.qualname.clone(),
        rule_code: d.rule_code.clone(),
        target: d.target.clone(),
    }
}

/// Count diagnostics per baseline key.
pub fn baseline_counts(
    diags: &[Diagnostic],
    key_path: &dyn Fn(&Path) -> String,
) -> BTreeMap<BaselineKey, u32> {
    let mut out = BTreeMap::new();
    for d in diags {
        *out.entry(baseline_key(d, key_path(&d.path))).or_insert(0) += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str, qualname: &str, target: &str) -> BaselineKey {
        BaselineKey {
            path: path.into(),
            qualname: qualname.into(),
            rule_code: "MUT201".into(),
            target: target.into(),
        }
    }

    fn diag(line: u32, code: &str, message: &str) -> Diagnostic {
        Diagnostic {
            path: PathBuf::from("src/a.py"),
            line,
            column: 5,
            span: Span { start: 0, end: 1 },
            qualname: "Parser.run".into(),
            rule_code: code.into(),
            rule_slug: "write-owner-violation".into(),
            target: "DataModel.fields".into(),
            severity: Severity::Error,
            message: message.into(),
            evidence: None,
            blocker: None,
            related: vec![RelatedLocation {
                path: PathBuf::from("src/b.py"),
                line: 3,
                column: 1,
                span: Span { start: 0, end: 0 },
                message: "calls helper".into(),
            }],
            fix: None,
        }
    }

    #[test]
    fn ratchet_detects_regressions_and_shrinkable_entries() {
        let k1 = key("src/a.py", "Parser.run", "DataModel.fields");
        let k2 = key("src/b.py", "Parser.old", "DataType.data_types");
        let baseline = RatchetBaseline {
            counts: BTreeMap::from([(k1.clone(), 1), (k2.clone(), 2)]),
        };
        let current = BTreeMap::from([(k1.clone(), 2), (k2.clone(), 1)]);
        let cmp = baseline.compare(&current);
        assert_eq!(cmp.regressions, vec![(k1, 1, 2)]);
        assert_eq!(cmp.stale_entries, vec![(k2, 2, 1)]);
    }

    #[test]
    fn ratchet_toml_round_trip_is_sorted() {
        let baseline = RatchetBaseline::from_counts(BTreeMap::from([
            (key("src/z.py", "Z.run", "M.f"), 1),
            (key("src/a.py", "A.run", "M.f"), 3),
            (key("src/zero.py", "Z.none", "M.f"), 0),
        ]));
        let text = baseline.to_toml_string();
        assert!(text.find("src/a.py").unwrap() < text.find("src/z.py").unwrap());
        assert!(!text.contains("src/zero.py"));
        let back = RatchetBaseline::from_toml_str(&text, Path::new("b.toml")).unwrap();
        assert_eq!(back, baseline);
        assert!(
            RatchetBaseline::from_toml_str("[[entries]]\npath = 1\n", Path::new("b.toml")).is_err()
        );
        assert!(RatchetBaseline::from_toml_str("bogus = 1\n", Path::new("b.toml")).is_err());
    }

    #[test]
    fn baseline_counts_group_by_key() {
        let d = diag(1, "MUT201", "m");
        let counts = baseline_counts(&[d.clone(), d], &|p| display_path(p));
        assert_eq!(counts.values().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn parses_heki_and_noqa_inline_suppressions() {
        assert_eq!(parse_inline_ignores("# heki: ignore"), Some(vec![]));
        assert_eq!(
            parse_inline_ignores("# heki: ignore[EFF101, MUT201]"),
            Some(vec!["EFF101".into(), "MUT201".into()])
        );
        assert_eq!(
            parse_inline_ignores("# noqa: EFF101"),
            Some(vec!["EFF101".into()])
        );
        assert_eq!(parse_inline_ignores("# noqa"), Some(vec![]));
        assert_eq!(parse_inline_ignores("# heki: nothing"), None);
        assert_eq!(parse_inline_ignores("# ordinary comment"), None);
    }

    #[test]
    fn inline_ignores_from_comment_tokens() {
        let src = "a = 1  # heki: ignore[EFF101]\nb = 2  # type: ignore  # noqa: MUT201, async-blocking-call\nc = 3  # heki: ignore\n";
        // Spans as the tokenizer reports them: from `#` to end of line.
        let mut spans = Vec::new();
        let mut offset = 0u32;
        for line in src.split_inclusive('\n') {
            let start = offset + u32::try_from(line.find('#').unwrap()).unwrap();
            let end = offset + u32::try_from(line.trim_end_matches('\n').len()).unwrap();
            spans.push(Span { start, end });
            offset += u32::try_from(line.len()).unwrap();
        }
        let ign = InlineIgnores::from_comments(src, &spans);
        assert!(ign.is_suppressed(1, "EFF101", "async-blocking-call"));
        assert!(!ign.is_suppressed(1, "MUT201", "write-owner-violation"));
        assert!(ign.is_suppressed(2, "MUT201", "write-owner-violation"));
        assert!(ign.is_suppressed(2, "EFF101", "ASYNC-BLOCKING-CALL"));
        assert!(ign.is_suppressed(3, "DATA101", "loop-carried-concat"));
        assert!(!ign.is_suppressed(4, "DATA101", "loop-carried-concat"));
    }

    #[test]
    fn formats_render_every_style() {
        let diags = vec![diag(2, "MUT201", "write, outside: owners\nsecond")];
        let line = |_: &Path, n: u32| (n == 2).then(|| "    model.fields.append(x)".to_owned());
        let full = render(&diags, OutputFormat::Full, &line);
        assert!(full.starts_with("src/a.py:2:5: MUT201 (write-owner-violation) [error] write"));
        assert!(full.contains("2 |     model.fields.append(x)"));
        assert!(full.contains("  |     ^"));
        assert!(full.contains("= related: src/b.py:3:1: calls helper"));
        assert_eq!(
            render(&diags, OutputFormat::Concise, &line),
            "src/a.py:2:5: MUT201 write, outside: owners\nsecond\n"
        );
        assert_eq!(
            render(&diags, OutputFormat::Github, &line),
            "::error file=src/a.py,line=2,col=5,title=heki MUT201::write, outside: owners%0Asecond\n"
        );
        let json: Vec<Diagnostic> =
            serde_json::from_str(&render(&diags, OutputFormat::Json, &line)).unwrap();
        assert_eq!(json, diags);
        let s: serde_json::Value =
            serde_json::from_str(&render(&diags, OutputFormat::Sarif, &line)).unwrap();
        assert_eq!(s["version"], "2.1.0");
        assert_eq!(s["runs"][0]["tool"]["driver"]["name"], "heki");
        assert_eq!(s["runs"][0]["results"][0]["ruleId"], "MUT201");
        assert_eq!(
            s["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]["startLine"],
            2
        );
        assert_eq!(
            s["runs"][0]["results"][0]["relatedLocations"][0]["message"]["text"],
            "calls helper"
        );
    }
}
