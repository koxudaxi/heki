//! Diagnostics, Ruff/`t-linter`-compatible output formats, inline `# heki: ignore[...]` parsing,
//! and line-number-independent Ratchet baselines (ADR-015 / ADR-017).

mod fix;

pub use fix::{PythonFixPass, apply_fixes, fix_python_source, unified_diff};

use heki_ast::{LineIndex, Span};
use heki_env::Severity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Paths are written as text; one that is not valid UTF-8 (possible on Linux) is written
/// lossily instead of failing the whole report.
fn lossy_path<S: serde::Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.to_string_lossy())
}

/// Related jump location (e.g., each hop in an `EFF101` or `MUT201` call chain for LSP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedLocation {
    #[serde(serialize_with = "lossy_path")]
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
    #[serde(serialize_with = "lossy_path")]
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
    pub span: Span,
    pub qualname: String,
    pub rule_code: String,
    pub rule_slug: String,
    pub target: String,
    pub severity: Severity,
    /// One sentence saying what happens, without advice.
    pub message: String,
    /// What to do about it (`= help:` in the full output); `None` when the rule has no advice
    /// for the report.
    #[serde(default)]
    pub help: Option<String>,
    pub evidence: Option<String>,
    pub blocker: Option<String>,
    pub related: Vec<RelatedLocation>,
    pub fix: Option<Vec<TextEdit>>,
    /// Distinct violation sites the diagnostic stands for (at least 1): a contract report at the
    /// caller of a private helper covers every write, read, or call inside it, and an effect
    /// boundary report every forbidden site its scope function reaches. Ratchet baselines count
    /// sites, so a second violation behind a recorded report is a regression. Written to JSON
    /// only when above 1.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub sites: u32,
    /// What the report shares with others that output may fold into one: the blocking root cause
    /// and effect of an `async def` boundary (`EFF101` / `EFF105`). Folding happens after
    /// suppressions and baselines, which see every report (`heki_rules::fold_root_causes`).
    #[serde(skip)]
    pub fold_key: Option<String>,
}

fn one() -> u32 {
    1
}

#[allow(clippy::trivially_copy_pass_by_ref)] // `skip_serializing_if` passes a reference.
fn is_one(n: &u32) -> bool {
    *n == 1
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

/// How reports write a path: with `/` separators on every platform. Only Windows separates
/// with `\\`; elsewhere it is a character of the file name and stays.
pub fn path_text(p: &Path) -> String {
    let text = p.to_string_lossy();
    #[cfg(windows)]
    {
        text.replace('\\', "/")
    }
    #[cfg(not(windows))]
    text.into_owned()
}

/// `path` as a relative URI reference (SARIF `artifactLocation.uri`): every byte but the
/// unreserved characters and `/` percent-encoded, so `#`, `?`, `%`, and spaces stay part of
/// the path.
fn path_uri(path: &Path) -> String {
    let mut out = String::new();
    for b in path_text(path).bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
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
            // Diagnostics hold strings, numbers, and paths (written lossily by `lossy_path`).
            let mut s = serde_json::to_string_pretty(diags).expect("diagnostics serialize");
            s.push('\n');
            s
        }
        OutputFormat::Github => render_github(diags),
        OutputFormat::Sarif => {
            let mut s = serde_json::to_string_pretty(&sarif(diags, source_line))
                .expect("a `serde_json::Value` always serializes");
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
            path_text(&d.path),
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
            let _ = writeln!(out, "{:gutter$} | {}^", "", caret_padding(&text, d.column));
        }
        for r in &d.related {
            let _ = writeln!(
                out,
                "  = related: {}:{}:{}: {}",
                path_text(&r.path),
                r.line,
                r.column,
                r.message
            );
        }
        if let Some(help) = &d.help {
            let _ = writeln!(out, "  = help: {help}");
        }
        out.push('\n');
    }
    out
}

/// The text of `line` before 1-based byte column `column` (cut back to a character boundary).
fn before_column(line: &str, column: u32) -> &str {
    let mut end = usize::try_from(column.saturating_sub(1))
        .unwrap_or(usize::MAX)
        .min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// What puts the caret under byte column `column` of the printed `line`: a tab where the line
/// has one (the terminal expands both alike), else as many spaces as each character is wide.
fn caret_padding(line: &str, column: u32) -> String {
    before_column(line, column)
        .chars()
        .map(|c| match c {
            '\t' => "\t".to_owned(),
            c => " ".repeat(display_width(c)),
        })
        .collect()
}

/// Terminal columns a character takes: none for combining marks and zero-width characters,
/// two for East Asian wide and fullwidth characters and emoji, else one.
fn display_width(c: char) -> usize {
    const ZERO: &[(u32, u32)] = &[
        (0x0300, 0x036F),
        (0x0483, 0x0489),
        (0x0591, 0x05BD),
        (0x0610, 0x061A),
        (0x064B, 0x065F),
        (0x1AB0, 0x1AFF),
        (0x1DC0, 0x1DFF),
        (0x200B, 0x200F),
        (0x20D0, 0x20FF),
        (0xFE00, 0xFE0F),
        (0xFE20, 0xFE2F),
    ];
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115F),
        (0x2E80, 0x303E),
        (0x3041, 0x33FF),
        (0x3400, 0x4DBF),
        (0x4E00, 0x9FFF),
        (0xA000, 0xA4CF),
        (0xAC00, 0xD7A3),
        (0xF900, 0xFAFF),
        (0xFE30, 0xFE4F),
        (0xFF00, 0xFF60),
        (0xFFE0, 0xFFE6),
        (0x1F300, 0x1F64F),
        (0x1F900, 0x1F9FF),
        (0x20000, 0x2FFFD),
        (0x30000, 0x3FFFD),
    ];
    let within = |ranges: &[(u32, u32)]| {
        ranges
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&(c as u32)))
    };
    if within(ZERO) {
        0
    } else if within(WIDE) {
        2
    } else {
        1
    }
}

fn render_concise(diags: &[Diagnostic]) -> String {
    let mut out = String::new();
    for d in diags {
        let _ = writeln!(
            out,
            "{}:{}:{}: {} {}",
            path_text(&d.path),
            d.line,
            d.column,
            d.rule_code,
            d.message
        );
    }
    out
}

/// Text as a GitHub Actions log line or command message holds it: `%`, CR, and LF escaped, so
/// it never starts a line of its own (a line starting with `::` is a workflow command).
pub fn github_escape(s: &str) -> String {
    gh_escape(s, false)
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
            gh_escape(&path_text(&d.path), true),
            d.line,
            d.column,
            gh_escape(&format!("heki {}", d.rule_code), true),
            gh_escape(&d.message, false)
        );
    }
    out
}

/// SARIF 2.1.0 log with one run (`runs[0].tool.driver.name = "heki"`). Columns are in UTF-16
/// code units (SARIF's default `columnKind`), converted from the byte columns of diagnostics
/// with `source_line(path, line)`. A result's help goes into its property bag
/// (`properties.help`): `reportingDescriptor.help` is one text per rule, while help may name
/// the reported object, and a result has no help property of its own.
pub fn sarif(
    diags: &[Diagnostic],
    source_line: &dyn Fn(&Path, u32) -> Option<String>,
) -> serde_json::Value {
    use serde_json::json;
    let mut rules: BTreeMap<&str, &str> = BTreeMap::new();
    for d in diags {
        rules.insert(&d.rule_code, &d.rule_slug);
    }
    let location = |path: &Path, line: u32, column: u32| {
        let column = source_line(path, line).map_or(column, |text| {
            u32::try_from(before_column(&text, column).encode_utf16().count() + 1).unwrap_or(column)
        });
        json!({
            "physicalLocation": {
                "artifactLocation": { "uri": path_uri(path) },
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
            let mut result = json!({
                "ruleId": d.rule_code,
                "level": severity_word(d.severity),
                "message": { "text": d.message },
                "locations": [location(&d.path, d.line, d.column)],
                "relatedLocations": related,
                "partialFingerprints": {
                    "hekiBaselineKey/v1": format!("{}|{}|{}|{}", path_text(&d.path), d.qualname, d.rule_code, d.target)
                }
            });
            if let Some(help) = &d.help {
                result["properties"] = json!({ "help": help });
            }
            result
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
/// `Some(empty)` suppresses every rule: only the bare forms do. A bracketed list without its
/// closing `]` or without a rule (`# heki: ignore[`, `# heki: ignore[]`) suppresses nothing.
pub fn parse_inline_ignores(comment: &str) -> Option<Vec<String>> {
    let trimmed = comment.trim_start_matches('#').trim();
    if let Some(rest) = trimmed.strip_prefix("heki:") {
        let after = rest.trim().strip_prefix("ignore")?.trim();
        if after.is_empty() {
            return Some(Vec::new()); // bare ignore = all rules
        }
        let (inner, _) = after.strip_prefix('[')?.split_once(']')?;
        let items: Vec<String> = inner
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        return (!items.is_empty()).then_some(items);
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

/// What the counts of a Ratchet baseline count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CountUnit {
    /// Violation sites ([`Diagnostic::sites`]): the files this version writes (`version = 2`).
    #[default]
    Sites,
    /// Diagnostics: files written before baselines counted sites (no `version`, or 1). They
    /// keep their meaning until `--update-baseline` rewrites them.
    Diagnostics,
}

impl CountUnit {
    /// The on-disk `version` of a baseline with this unit.
    fn version(self) -> u32 {
        match self {
            Self::Diagnostics => 1,
            Self::Sites => BASELINE_VERSION,
        }
    }

    /// What a count counts, for messages (`2 site(s)`).
    pub fn noun(self) -> &'static str {
        match self {
            Self::Sites => "site(s)",
            Self::Diagnostics => "diagnostic(s)",
        }
    }
}

/// The baseline format this version writes: counts are violation sites.
pub const BASELINE_VERSION: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatchetBaseline {
    pub counts: BTreeMap<BaselineKey, u32>,
    pub unit: CountUnit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RatchetComparison {
    pub regressions: Vec<(BaselineKey, u32, u32)>,
    pub stale_entries: Vec<(BaselineKey, u32, u32)>,
}

#[derive(Debug, Error)]
pub enum BaselineError {
    #[error("failed to read baseline {path}: {cause}")]
    Read {
        path: PathBuf,
        cause: std::io::Error,
    },
    #[error("failed to write baseline {path}: {cause}")]
    Write {
        path: PathBuf,
        cause: std::io::Error,
    },
    #[error("failed to parse baseline {path}: {cause}")]
    Toml {
        path: PathBuf,
        cause: toml::de::Error,
    },
    #[error(
        "baseline {path} has version {version}; this heki reads versions 1 and {BASELINE_VERSION}"
    )]
    Version { path: PathBuf, version: u32 },
    #[error(
        "baseline {path} lists {}::{} {} `{}` more than once; keep one entry", key.path, key.qualname, key.rule_code, key.target
    )]
    Duplicate { path: PathBuf, key: BaselineKey },
}

/// On-disk shape: `version`, then `[[entries]]` sorted by key (minimal git diffs).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineFile {
    /// Absent (or 1) in files written before baselines counted sites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<u32>,
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
    /// A baseline of site counts ([`baseline_counts`] with [`CountUnit::Sites`]).
    pub fn from_counts(counts: BTreeMap<BaselineKey, u32>) -> Self {
        Self {
            counts: counts.into_iter().filter(|(_, c)| *c > 0).collect(),
            unit: CountUnit::Sites,
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

    /// What a check against this baseline reports of `diags`: every diagnostic of a key whose
    /// count (in the baseline's unit) grew past its entry. `key_path` gives the baseline path of
    /// a diagnostic's file. Returns the comparison the counts were kept by.
    pub fn keep_regressions(
        &self,
        diags: &mut Vec<Diagnostic>,
        key_path: &dyn Fn(&Path) -> String,
    ) -> RatchetComparison {
        let cmp = self.compare(&baseline_counts(diags, key_path, self.unit));
        let regressed: std::collections::BTreeSet<&BaselineKey> =
            cmp.regressions.iter().map(|(k, _, _)| k).collect();
        diags.retain(|d| regressed.contains(&baseline_key(d, key_path(&d.path))));
        cmp
    }

    pub fn from_toml_str(text: &str, path: &Path) -> Result<Self, BaselineError> {
        let file: BaselineFile = toml::from_str(text).map_err(|cause| BaselineError::Toml {
            path: path.to_path_buf(),
            cause,
        })?;
        let unit = match file.version {
            None | Some(1) => CountUnit::Diagnostics,
            Some(BASELINE_VERSION) => CountUnit::Sites,
            Some(version) => {
                return Err(BaselineError::Version {
                    path: path.to_path_buf(),
                    version,
                });
            }
        };
        // heki writes one entry per key; two (a hand merge of two baselines) would add their
        // allowances up, while each recorded the same findings.
        let mut counts = BTreeMap::new();
        for e in file.entries {
            let key = BaselineKey {
                path: e.path,
                qualname: e.qualname,
                rule_code: e.rule_code,
                target: e.target,
            };
            if counts.contains_key(&key) {
                return Err(BaselineError::Duplicate {
                    path: path.to_path_buf(),
                    key,
                });
            }
            counts.insert(key, e.count);
        }
        Ok(Self { counts, unit })
    }

    pub fn to_toml_string(&self) -> String {
        let file = BaselineFile {
            version: Some(self.unit.version()),
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
        let text = std::fs::read_to_string(path).map_err(|cause| BaselineError::Read {
            path: path.to_path_buf(),
            cause,
        })?;
        Self::from_toml_str(&text, path)
    }

    pub fn save(&self, path: &Path) -> Result<(), BaselineError> {
        std::fs::write(path, self.to_toml_string()).map_err(|cause| BaselineError::Write {
            path: path.to_path_buf(),
            cause,
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

/// Count violation sites (or diagnostics) per baseline key.
pub fn baseline_counts(
    diags: &[Diagnostic],
    key_path: &dyn Fn(&Path) -> String,
    unit: CountUnit,
) -> BTreeMap<BaselineKey, u32> {
    let mut out = BTreeMap::new();
    for d in diags {
        let n = match unit {
            CountUnit::Sites => d.sites.max(1),
            CountUnit::Diagnostics => 1,
        };
        *out.entry(baseline_key(d, key_path(&d.path))).or_insert(0) += n;
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
            help: None,
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
            sites: 1,
            fold_key: None,
        }
    }

    #[test]
    fn ratchet_detects_regressions_and_shrinkable_entries() {
        let k1 = key("src/a.py", "Parser.run", "DataModel.fields");
        let k2 = key("src/b.py", "Parser.old", "DataType.data_types");
        let baseline = RatchetBaseline {
            counts: BTreeMap::from([(k1.clone(), 1), (k2.clone(), 2)]),
            unit: CountUnit::Sites,
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
        let mut two_sites = d.clone();
        two_sites.sites = 2;
        let diags = [d.clone(), d, two_sites];
        let counts = |unit| {
            baseline_counts(&diags, &|p| path_text(p), unit)
                .values()
                .copied()
                .collect::<Vec<_>>()
        };
        // Sites: a report standing for two sites counts twice; diagnostics: once.
        assert_eq!(counts(CountUnit::Sites), vec![4]);
        assert_eq!(counts(CountUnit::Diagnostics), vec![3]);
    }

    #[test]
    fn baseline_versions_set_the_count_unit() {
        let path = Path::new("b.toml");
        let entry = "[[entries]]\npath = \"a.py\"\nqualname = \"f\"\nrule_code = \"MUT201\"\ntarget = \"M.f\"\ncount = 1\n";
        // Files written before baselines counted sites keep counting diagnostics.
        for old in [entry.to_owned(), format!("version = 1\n{entry}")] {
            let b = RatchetBaseline::from_toml_str(&old, path).unwrap();
            assert_eq!(b.unit, CountUnit::Diagnostics);
            assert_eq!(b.unit.noun(), "diagnostic(s)");
            assert!(b.to_toml_string().contains("version = 1\n"));
        }
        let new = RatchetBaseline::from_toml_str(&format!("version = 2\n{entry}"), path).unwrap();
        assert_eq!(new.unit, CountUnit::Sites);
        assert_eq!(new.unit.noun(), "site(s)");
        assert!(new.to_toml_string().contains("version = 2\n"));
        let twice = format!(
            "version = 2\n{entry}{}",
            entry.replace("count = 1", "count = 4294967295")
        );
        assert_eq!(
            RatchetBaseline::from_toml_str(&twice, path)
                .unwrap_err()
                .to_string(),
            "baseline b.toml lists a.py::f MUT201 `M.f` more than once; keep one entry"
        );
        let err = RatchetBaseline::from_toml_str(&format!("version = 7\n{entry}"), path)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "baseline b.toml has version 7; this heki reads versions 1 and 2"
        );
    }

    #[test]
    fn diagnostics_write_sites_to_json_only_above_one() {
        let mut d = diag(1, "MUT201", "m");
        let one = render(std::slice::from_ref(&d), OutputFormat::Json, &|_, _| None);
        assert!(!one.contains("\"sites\""), "{one}");
        d.sites = 3;
        let three = render(std::slice::from_ref(&d), OutputFormat::Json, &|_, _| None);
        assert!(three.contains("\"sites\": 3"), "{three}");
        let back: Vec<Diagnostic> = serde_json::from_str(&one).unwrap();
        assert_eq!(back[0].sites, 1);
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
        for unfinished in [
            "# heki: ignore[",
            "# heki: ignore[EFF101",
            "# heki: ignore[]",
            "# heki: ignore[ , ]",
        ] {
            assert_eq!(parse_inline_ignores(unfinished), None, "{unfinished}");
        }
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
    fn comment_spans_outside_the_source_are_ignored() {
        // Spans come from the parser of the same source; one that does not fit it (out of
        // range, reversed) suppresses nothing instead of panicking.
        let src = "a = 1  # heki: ignore\n";
        let spans = [Span { start: 7, end: 400 }, Span { start: 9, end: 8 }];
        let ign = InlineIgnores::from_comments(src, &spans);
        assert!(!ign.is_suppressed(1, "EFF101", "async-blocking-call"));
    }

    #[cfg(unix)]
    #[test]
    fn json_reports_write_non_utf8_paths_lossily() {
        // Linux file names may be any bytes (APFS rejects them, so no end-to-end test on
        // macOS): such a path is written lossily instead of the report degrading to `[]`.
        use std::os::unix::ffi::OsStrExt;
        let mut d = diag(1, "MUT201", "m");
        d.path = PathBuf::from(std::ffi::OsStr::from_bytes(b"src/\xff.py"));
        d.related[0].path = d.path.clone();
        let out = render(&[d], OutputFormat::Json, &|_, _| None);
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json[0]["path"], "src/\u{FFFD}.py");
        assert_eq!(json[0]["related"][0]["path"], "src/\u{FFFD}.py");
    }

    #[test]
    fn carets_and_sarif_columns_after_multibyte_characters() {
        // U+00E9 takes two bytes, one terminal column, and one UTF-16 unit; U+8A9E three bytes,
        // two columns, one unit; U+1F600 four bytes, two columns, two units. A tab stays a tab.
        let text = "\tx = \"\u{e9}\u{8a9e}\u{1f600}\"; time.sleep(1)";
        let mut d = diag(4, "EFF101", "m");
        d.column = u32::try_from(text.find("time").unwrap()).unwrap() + 1;
        d.related.clear();
        let line = |_: &Path, n: u32| (n == 4).then(|| text.to_owned());
        let full = render(std::slice::from_ref(&d), OutputFormat::Full, &line);
        assert!(
            full.contains(&format!("\n  | \t{}^\n", " ".repeat(13))),
            "{full:?}"
        );
        let s: serde_json::Value =
            serde_json::from_str(&render(&[d], OutputFormat::Sarif, &line)).unwrap();
        assert_eq!(
            s["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]["startColumn"],
            14
        );
        assert_eq!(
            caret_padding("\u{e9}", 99),
            " ",
            "a column past the line end"
        );
        // A line that changed since it was analyzed (`--watch` reads it again to print it).
        assert_eq!(
            caret_padding("\u{e9}x", 2),
            "",
            "a column inside a character"
        );
        assert_eq!(display_width('\u{301}'), 0);
    }

    #[test]
    fn formats_render_every_style() {
        let mut diags = vec![diag(2, "MUT201", "write, outside: owners\nsecond")];
        diags[0].help = Some("use the store".into());
        let line = |_: &Path, n: u32| (n == 2).then(|| "    model.fields.append(x)".to_owned());
        let full = render(&diags, OutputFormat::Full, &line);
        assert!(full.starts_with("src/a.py:2:5: MUT201 (write-owner-violation) [error] write"));
        assert!(full.contains("2 |     model.fields.append(x)"));
        assert!(full.contains("  |     ^"));
        assert!(
            full.ends_with("  = related: src/b.py:3:1: calls helper\n  = help: use the store\n\n"),
            "{full}"
        );
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
        assert_eq!(
            s["runs"][0]["results"][0]["properties"]["help"],
            "use the store"
        );
    }

    #[test]
    fn a_report_without_help_has_no_help_line_and_a_null_field() {
        let diags = vec![diag(2, "MUT201", "m")];
        let full = render(&diags, OutputFormat::Full, &|_, _| None);
        assert!(!full.contains("= help:"), "{full}");
        let json: serde_json::Value =
            serde_json::from_str(&render(&diags, OutputFormat::Json, &|_, _| None)).unwrap();
        assert_eq!(
            json[0].get("help"),
            Some(&serde_json::Value::Null),
            "{json}"
        );
        let s: serde_json::Value =
            serde_json::from_str(&render(&diags, OutputFormat::Sarif, &|_, _| None)).unwrap();
        assert!(
            s["runs"][0]["results"][0].get("properties").is_none(),
            "{s}"
        );
    }
}
