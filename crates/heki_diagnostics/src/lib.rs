//! Diagnostics, Ruff/`t-linter`-compatible output formats, inline `# heki: ignore[...]` parsing,
//! and line-number-independent Ratchet baselines (ADR-015 / ADR-017).

mod fix;
mod style;
mod suppression;

pub use fix::{PythonFixPass, apply_fixes, fix_python_source, unified_diff};
pub use style::Stylesheet;
pub use suppression::{InlineIgnores, Suppression, SuppressionForm, parse_inline_ignores};

use heki_ast::Span;
use heki_env::Severity;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use style::paint;
use thiserror::Error;

/// Paths are written as every report writes them ([`path_text`]): one that is not valid UTF-8
/// (possible on Linux) is written lossily instead of failing the whole report.
fn report_path<S: serde::Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&path_text(path))
}

/// Related jump location (e.g., each hop in an `EFF101` or `MUT201` call chain for LSP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RelatedLocation {
    /// The file, as reports print paths.
    #[serde(serialize_with = "report_path")]
    #[schemars(with = "String")]
    pub path: PathBuf,
    /// The 1-based line.
    pub line: u32,
    /// The 1-based byte column.
    pub column: u32,
    /// Byte span in `path` (the LSP server converts it with the file's own text).
    pub span: Span,
    /// What the location is: the call of a hop, for instance.
    pub message: String,
}

/// Where the configuration defines a contract a report is of, or the entry of it the report
/// names: where a configuration file writes it, or the place the contract's `source` or
/// `entry-sources` names instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ContractLocation {
    /// The file, as reports print paths.
    #[serde(serialize_with = "report_path")]
    #[schemars(with = "String")]
    pub path: PathBuf,
    /// The 1-based line.
    pub line: u32,
    /// The 1-based byte column (1 for a place `source` or `entry-sources` names).
    pub column: u32,
    /// What is there: the contract (``write-owners contract `fields` ``), or the entry of it
    /// (``` `targets` entry `pkg.m.Model.fields` ```), with the group that holds it.
    pub message: String,
}

/// The entry of a contract that the access of a report matched: the key of the contract's table
/// whose list holds it and its place in that list, in configuration order (an `@group` entry
/// stands for the entries the group writes out there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContractEntry {
    /// The contract, as [`Diagnostic::contracts`] gives it.
    pub contract: u32,
    pub key: &'static str,
    pub position: u32,
}

impl ContractEntry {
    #[must_use]
    pub fn new(contract: usize, key: &'static str, position: usize) -> Self {
        let place = |p: usize| u32::try_from(p).unwrap_or(u32::MAX);
        Self {
            contract: place(contract),
            key,
            position: place(position),
        }
    }
}

/// A public project function that does what a contract report shows from a place its contract
/// allows, and the file defining it (the report's help names it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AllowedExample {
    /// Dotted fully qualified name (`pkg.store.Store.add`).
    pub fqn: String,
    /// The file that defines it, as reports print paths.
    #[serde(serialize_with = "report_path")]
    #[schemars(with = "String")]
    pub path: PathBuf,
}

/// A site an effect boundary with `unresolved = "report"` cannot judge, one of those its
/// `unresolved-calls` or `unresolved-writes` report stands for.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(untagged)]
pub enum UnresolvedSite {
    Call(UnresolvedCall),
    Write(UnresolvedWrite),
}

/// A call whose code heki does not follow: a call it resolves to no function, or a call of a
/// function whose body it does not model or only a stub declares.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[schemars(deny_unknown_fields)]
pub struct UnresolvedCall {
    /// The file, as reports print paths.
    #[serde(serialize_with = "report_path")]
    #[schemars(with = "String")]
    pub path: PathBuf,
    /// The 1-based line of the call.
    pub line: u32,
    /// The 1-based byte column of the call.
    pub column: u32,
    /// The function holding the call, by fully qualified name; the scope function itself, at
    /// its definition, when its own body is the one heki does not follow.
    pub function: String,
    /// The method or value called (`get` of `table.get(...)`, `handler` of `handler(...)`),
    /// `null` for a value computed in place (`make()(...)`); for a body heki does not follow,
    /// that function, by fully qualified name.
    pub callee: Option<String>,
    /// Why heki does not follow it: the reasons `heki recommend` gives for a call it resolves
    /// to no function (`unannotated-parameter`, `callable-value`, `call-result`,
    /// `unknown-member`, ...), `unmodeled-body` for a body heki does not model (`eval`, a body
    /// too large), or `stub-body` for a function only a stub declares.
    pub reason: String,
    /// The parameter the reason is about (`unannotated-parameter`, `unresolved-annotation`).
    pub parameter: Option<String>,
}

/// A write, on an object of unknown class, of an attribute the boundary names.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[schemars(deny_unknown_fields)]
pub struct UnresolvedWrite {
    /// The file, as reports print paths.
    #[serde(serialize_with = "report_path")]
    #[schemars(with = "String")]
    pub path: PathBuf,
    /// The 1-based line of the write.
    pub line: u32,
    /// The 1-based byte column of the write.
    pub column: u32,
    /// The function holding the write, by fully qualified name.
    pub function: String,
    /// The attribute written; `null` for a name computed at run time (`setattr(obj, name, v)`).
    pub attribute: Option<String>,
    /// How it is written, as `heki inspect` names write kinds (`direct_attr`, `set_attr`,
    /// `container_mut`, ...).
    pub kind: String,
}

impl UnresolvedSite {
    pub fn path_mut(&mut self) -> &mut PathBuf {
        match self {
            Self::Call(c) => &mut c.path,
            Self::Write(w) => &mut w.path,
        }
    }
}

/// Minimal byte-span text edit (preserves formatting and comments).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TextEdit {
    /// The bytes to replace.
    pub span: Span,
    /// The text that takes their place.
    pub replacement: String,
}

/// Whether a report can be wrong about the code (ADR-018). A report's claim is a conjunction of
/// premises (its rule page lists them under "What a report states").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Certainty {
    /// Every premise of the claim was established, under the assumptions R1-R5, and the rule's
    /// reports were checked on the measurement corpus with none wrong: a wrong one is a bug.
    Exact,
    /// A premise of the claim may be unknown, which does not stop the report: it can be wrong
    /// about the code.
    Heuristic,
}

impl Certainty {
    pub const fn word(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Heuristic => "heuristic",
        }
    }

    /// The certainty of a report whose own premises give `self`, from a rule whose reports are
    /// at most `rule`: a premise the report did not establish makes it heuristic, and so does
    /// a rule that is not certified.
    #[must_use]
    pub const fn at_most(self, rule: Self) -> Self {
        match (self, rule) {
            (Self::Exact, Self::Exact) => Self::Exact,
            _ => Self::Heuristic,
        }
    }

    /// The SARIF `reportingDescriptor.properties.precision` GitHub code scanning reads: the
    /// scale of CodeQL's `@precision` (`very-high`, `high`, `medium`, `low`).
    const fn sarif_precision(self) -> &'static str {
        match self {
            Self::Exact => "very-high",
            Self::Heuristic => "medium",
        }
    }

    /// The label every output adds to a report: only heuristic reports get one.
    pub const fn label(self) -> Option<&'static str> {
        match self {
            Self::Exact => None,
            Self::Heuristic => Some(Self::Heuristic.word()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Diagnostic {
    /// The file, as `heki check` prints paths: relative to the working directory, with forward
    /// slashes.
    #[serde(serialize_with = "report_path")]
    #[schemars(with = "String")]
    pub path: PathBuf,
    /// The 1-based line of the report.
    pub line: u32,
    /// The 1-based byte column of the report.
    pub column: u32,
    /// The byte span of the reported code in `path`.
    pub span: Span,
    /// The qualified name of the function the report is in; empty at module level.
    pub qualname: String,
    /// The code of the rule that reports (`EFF101`).
    pub rule_code: String,
    /// The name of the rule (`async-blocking-call`).
    pub rule_slug: String,
    /// What the report is about: the call, name, or module it names.
    pub target: String,
    /// The severity of the report, from its rule or the configuration.
    pub severity: Severity,
    /// The certainty of the report, written by every output format: its rule's, or heuristic
    /// where the report did not establish a premise its rule's exact reports have (an `OPT`
    /// report whose rewrite is not proven).
    pub certainty: Certainty,
    /// One sentence saying what happens, without advice.
    pub message: String,
    /// What to do about it (`= help:` in the full output); `None` when the rule has no advice
    /// for the report.
    #[serde(default)]
    pub help: Option<String>,
    /// What shows the claim holds, when the rule keeps it.
    pub evidence: Option<String>,
    /// What keeps the rewrite of an `OPT` report from being safe; none otherwise.
    pub blocker: Option<String>,
    /// The other places the report is about: the hops of a call chain, a definition.
    pub related: Vec<RelatedLocation>,
    /// Where the configuration defines the contracts a contract report is of: each contract,
    /// then the entry of it the report names as its target. Written to JSON only when not
    /// empty; SARIF and the language server add them to the related locations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract_locations: Vec<ContractLocation>,
    /// The edits of the report's fix, as byte spans of `path`; none when the report has no fix.
    pub fix: Option<Vec<TextEdit>>,
    /// Distinct violation sites the diagnostic stands for (at least 1): a contract report at the
    /// caller of a private helper covers every write, read, or call inside it, and an effect
    /// boundary report every forbidden site its scope function reaches. Ratchet baselines count
    /// sites, so a second violation behind a recorded report is a regression. Written to JSON
    /// only when above 1.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub sites: u32,
    /// Every site of an effect boundary's `unresolved-calls` or `unresolved-writes` report
    /// (`sites` counts them), ordered by path, line, and column: those the walk from the scope
    /// function records, not the code behind them, which heki does not follow. Written to JSON
    /// only when not empty; no other output writes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_sites: Vec<UnresolvedSite>,
    /// What the report shares with others that output may fold into one: the blocking root cause
    /// and effect of an `async def` boundary (`EFF101` / `EFF105`), with the blocking call a
    /// helper on its chain makes, so that sites reaching the API through different helpers stay
    /// apart (each is a fix of its own). Folding happens after
    /// suppressions and baselines, which see every report (`heki_rules::fold_root_causes`).
    #[serde(skip)]
    pub fold_key: Option<String>,
    /// Why a heuristic report of an exact rule is not exact: the premises of its claim it did
    /// not establish and the conditions of the exact tier that are no premise, each with its
    /// cause; filled only when the run asks for them (`heki recommend` counts them), and written
    /// by no output format.
    #[serde(skip)]
    pub open_records: Vec<OpenRecord>,
    /// The contracts the report is of, by their place in the configuration's array of its
    /// rule's contract kind (`write-owners` for `MUT201` and `MUT205`, `read-owners` for
    /// `MUT203` and `MUT206`, `call-owners` for `MUT204` and `MUT207`, `effect-boundaries` for
    /// `EFF201`, `invalidation` for `MUT208`, `import-boundaries` for `IMP201` and `IMP202`,
    /// `readonly-returns` for `MUT202`: those under whose sharing the change is one), sorted:
    /// one report at an access several contracts find stands for each of them. Empty for the
    /// other rules; written by no output format.
    #[serde(skip)]
    pub contracts: Vec<u32>,
    /// For each of [`Self::contracts`] whose entry the access matched by its pattern rather than
    /// by the entry's text (`EFF201`, `MUT208`, `IMP201`), that entry: the first the access
    /// matches, as the report's target names it. [`Self::contract_locations`] points at it, and
    /// at the entry whose text is the target for the other contracts. Written by no output
    /// format.
    #[serde(skip)]
    pub contract_entries: Vec<ContractEntry>,
    /// The public project functions that do what a contract report shows from a place its
    /// contract allows, at most three, by fully qualified name: its help names them. Empty for the other
    /// reports and when there is none; written to JSON only when not empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_examples: Vec<AllowedExample>,
    /// The report stands for a group of modules, which its `target` lists (`IMP101`): its
    /// baseline key names no file or function, and an entry of a group holding every one of
    /// them covers it, wherever the report sits. Written by no output format.
    #[serde(skip)]
    pub group: bool,
    /// The target a Ratchet baseline keys the report by, when it is not `target`: an exact report
    /// naming the call every implementation runs (`EFF101`, `EFF103`) keys by the call of the
    /// shortest chain, as its heuristic tier names it, so that a change of tier stays within one
    /// entry. Written by no output format.
    #[serde(skip)]
    pub key_target: Option<String>,
    /// For a report of a name a `from` import takes from a module (`IMP201`, whose `target` is
    /// `m.n`), the module `m`: a baseline entry of the module, as baselines keyed these reports
    /// before they named the import, covers the names it has no entry for, until
    /// `--update-baseline` rewrites it per name. Written by no output format.
    #[serde(skip)]
    pub key_module: Option<String>,
    /// A contract of the report has `fail = true`, or `lint.fail` names its rule: the report
    /// fails the run whatever its tier and severity ([`Self::fails_run`]). Set once the contracts
    /// of a report are known (`heki_rules::check_project`); written by no output format.
    #[serde(skip)]
    pub fails_run_by_config: bool,
}

/// Why a heuristic report of an exact rule is not exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenRecord {
    /// A premise of its claim is open: the claim may be false.
    Premise(OpenPremise),
    /// A condition of the exact tier that is no premise of the claim is open: the claim holds
    /// either way.
    Condition(OpenCondition),
}

impl From<OpenPremise> for OpenRecord {
    fn from(premise: OpenPremise) -> Self {
        Self::Premise(premise)
    }
}

/// What a condition of the exact tier that is no premise of the claim is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Axis {
    /// Whether the change pays off (Safety × Profitability: the claim is about safety).
    Profitability,
}

/// A condition of the exact tier that is no premise of the claim, left open by a report, and
/// why (`heki_rules::premises::ConditionCause`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenCondition {
    pub axis: Axis,
    /// The condition's name (`heki_rules::premises::Condition`).
    pub condition: &'static str,
    /// The cause, unique within its condition.
    pub cause: &'static str,
    /// Where the condition is open: a loop.
    pub at: Option<RelatedLocation>,
    /// What the cause's statement leaves out.
    pub detail: Option<String>,
}

/// A premise of an exact claim that a report left open, and why
/// (`heki_rules::premises::OpenCause`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenPremise {
    /// The premise's name in the premise table.
    pub premise: &'static str,
    /// The cause, unique within its premise.
    pub cause: &'static str,
    /// Where the premise is open: the call of a chain's hop, or a loop.
    pub at: Option<RelatedLocation>,
    /// What the cause's statement leaves out: the function or class it is about.
    pub detail: Option<String>,
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

    /// Whether the report makes a run fail (exit code 1) when `fail_on` is the least severity
    /// that does: an exact `error`, or any report at `warning`. A heuristic report counts as a
    /// warning whatever its severity: it can be wrong about the code, and only a certain bug
    /// that can be critical stops a run by default (ADR-019). A report of a contract with
    /// `fail = true`, or of a rule `lint.fail` names, fails it whatever `fail_on` is.
    pub fn fails_run(&self, fail_on: Severity) -> bool {
        if self.fails_run_by_config {
            return true;
        }
        match fail_on {
            Severity::Warning => true,
            Severity::Error => {
                self.severity == Severity::Error && self.certainty == Certainty::Exact
            }
        }
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
    /// GitHub Actions workflow commands, for a run that fails on reports at `fail_on`.
    Github {
        fail_on: Severity,
    },
    Sarif,
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

/// A file the run could not check: it could not be read or parsed. SARIF carries it as a tool
/// execution notification; the other formats leave it to the error lines the caller writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFailure {
    pub path: PathBuf,
    /// The 1-based line and byte column of a syntax error.
    pub position: Option<(u32, u32)>,
    pub message: String,
}

/// Render diagnostics, and in SARIF the `failures` of the run. `source_line(path, line)` returns
/// the 1-based source line for `full`. Only the text formats (`full` and `concise`) take
/// `styles`; the others are read by programs and never hold escape codes.
pub fn render(
    diags: &[Diagnostic],
    failures: &[FileFailure],
    format: OutputFormat,
    styles: &Stylesheet,
    source_line: &dyn Fn(&Path, u32) -> Option<String>,
) -> String {
    match format {
        OutputFormat::Full => render_full(diags, styles, source_line),
        OutputFormat::Concise => render_concise(diags, styles),
        OutputFormat::Json => {
            // Diagnostics hold strings, numbers, and paths (written lossily by `report_path`).
            let mut s = serde_json::to_string_pretty(diags).expect("diagnostics serialize");
            s.push('\n');
            s
        }
        OutputFormat::Github { fail_on } => render_github(diags, fail_on),
        OutputFormat::Sarif => {
            let mut s = serde_json::to_string_pretty(&sarif(diags, failures, source_line))
                .expect("a `serde_json::Value` always serializes");
            s.push('\n');
            s
        }
    }
}

fn render_full(
    diags: &[Diagnostic],
    styles: &Stylesheet,
    source_line: &dyn Fn(&Path, u32) -> Option<String>,
) -> String {
    let mut out = String::new();
    let equals = paint(styles.gutter, '=');
    for d in diags {
        let path = path_text(&d.path);
        let severity = styles.severity(d.severity);
        let label = d.certainty.label().map(|l| format!(", {l}"));
        let _ = writeln!(
            out,
            "{} {} ({}) [{}{}] {}",
            styles.location(&path, d.line, d.column),
            paint(styles.rule_code, &d.rule_code),
            d.rule_slug,
            paint(severity, d.severity.word()),
            label.unwrap_or_default(),
            d.message
        );
        if let Some(text) = source_line(&d.path, d.line) {
            write_excerpt(&mut out, styles, severity, &text, d.line, d.column);
        }
        for r in &d.related {
            let _ = writeln!(
                out,
                "  {equals} {}: {} {}",
                paint(styles.related, "related"),
                styles.location(&path_text(&r.path), r.line, r.column),
                r.message
            );
        }
        if let Some(help) = &d.help {
            let _ = writeln!(out, "  {equals} {}: {help}", paint(styles.help, "help"));
        }
        out.push('\n');
    }
    out
}

/// The source line `text` (line `line`) with a caret under byte column `column`, painted in
/// `caret`.
fn write_excerpt(
    out: &mut String,
    styles: &Stylesheet,
    caret: anstyle::Style,
    text: &str,
    line: u32,
    column: u32,
) {
    let bar = paint(styles.gutter, '|');
    let gutter = line.to_string().len();
    let _ = writeln!(out, "{:gutter$} {bar}", "");
    let _ = writeln!(
        out,
        "{} {}",
        paint(styles.gutter, format_args!("{line} |")),
        text.trim_end()
    );
    let _ = writeln!(
        out,
        "{:gutter$} {bar} {}{}",
        "",
        caret_padding(text, column),
        paint(caret, '^')
    );
}

/// A failure of a command ([`heki_env::Problem`]) in the layout of a report: `error:`, where
/// the cause is, the message, the source line under it, then what to do (`= help:`) and the
/// page of the documentation (`= docs:`).
pub fn render_problem(
    problem: &heki_env::Problem,
    styles: &Stylesheet,
    source_line: &dyn Fn(&Path, u32) -> Option<String>,
) -> String {
    let mut out = String::new();
    let equals = paint(styles.gutter, '=');
    let _ = write!(out, "{}: ", paint(styles.error, "error"));
    let excerpt = problem.location.as_ref().and_then(|at| {
        let path = path_text(&at.path);
        match at.position {
            Some((line, column)) => {
                let _ = write!(out, "{} ", styles.location(&path, line, column));
                source_line(&at.path, line).map(|text| (text, line, column))
            }
            None => {
                let _ = write!(
                    out,
                    "{}{} ",
                    paint(styles.path, path),
                    paint(styles.separator, ':')
                );
                None
            }
        }
    });
    let _ = writeln!(out, "{}", problem.message);
    if let Some((text, line, column)) = excerpt {
        write_excerpt(&mut out, styles, styles.error, &text, line, column);
    }
    for help in &problem.help {
        let _ = writeln!(out, "  {equals} {}: {help}", paint(styles.help, "help"));
    }
    if let Some(docs) = problem.docs {
        let _ = writeln!(
            out,
            "  {equals} {}: {}",
            paint(styles.help, "docs"),
            docs.url()
        );
    }
    out
}

/// Line `line` (1-based) of the file at `path`, read from the disk.
pub fn file_line(path: &Path, line: u32) -> Option<String> {
    let text = heki_ast::read_source(path).ok()?;
    heki_ast::LineIndex::from_source(&text)
        .line_text(&text, line)
        .map(str::to_owned)
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

fn render_concise(diags: &[Diagnostic], styles: &Stylesheet) -> String {
    let mut out = String::new();
    for d in diags {
        let label = d.certainty.label().map(|l| format!(" [{l}]"));
        let _ = writeln!(
            out,
            "{} {}{} {}",
            styles.location(&path_text(&d.path), d.line, d.column),
            paint(styles.rule_code, &d.rule_code),
            label.unwrap_or_default(),
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

/// One workflow command per report. Its level is the report's severity when the report fails the
/// run at `fail_on` ([`Diagnostic::fails_run`]), and `warning` when it does not: GitHub shows an
/// `error` annotation as a failure, which such a report does not cause.
fn render_github(diags: &[Diagnostic], fail_on: Severity) -> String {
    let mut out = String::new();
    for d in diags {
        let level = if d.fails_run(fail_on) {
            d.severity
        } else {
            Severity::Warning
        };
        let label = d.certainty.label().map(|l| format!(" ({l})"));
        let mut body = d.message.clone();
        for r in &d.related {
            let _ = write!(
                body,
                "\nrelated: {}:{}:{}: {}",
                path_text(&r.path),
                r.line,
                r.column,
                r.message
            );
        }
        let _ = writeln!(
            out,
            "::{} file={},line={},col={},title={}::{}",
            level.word(),
            gh_escape(&path_text(&d.path), true),
            d.line,
            d.column,
            gh_escape(
                &format!("heki {}{}", d.rule_code, label.unwrap_or_default()),
                true
            ),
            gh_escape(&body, false)
        );
    }
    out
}

/// SARIF 2.1.0 log with one run (`runs[0].tool.driver.name = "heki"`). Columns are in UTF-16
/// code units (SARIF's default `columnKind`), converted from the byte columns of diagnostics
/// with `source_line(path, line)`. A result's help goes into its property bag
/// (`properties.help`): `reportingDescriptor.help` is one text per rule, while help may name
/// the reported object, and a result has no help property of its own. Certainty goes into the
/// property bags as GitHub code scanning reads them: a rule descriptor's `precision` is that of
/// the rule's least certain report, and a heuristic descriptor and a heuristic result get the
/// tag `heuristic`. The run's one invocation lists each file it could not check as an `error`
/// notification, and then did not execute successfully.
pub fn sarif(
    diags: &[Diagnostic],
    failures: &[FileFailure],
    source_line: &dyn Fn(&Path, u32) -> Option<String>,
) -> serde_json::Value {
    use serde_json::json;
    let mut rules: BTreeMap<&str, (&str, Certainty)> = BTreeMap::new();
    for d in diags {
        rules
            .entry(&d.rule_code)
            .and_modify(|(_, c)| *c = c.at_most(d.certainty))
            .or_insert((&d.rule_slug, d.certainty));
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
            let places = (d.related.iter())
                .map(|r| (&r.path, r.line, r.column, &r.message))
                .chain(
                    (d.contract_locations.iter()).map(|c| (&c.path, c.line, c.column, &c.message)),
                );
            let related: Vec<serde_json::Value> = places
                .enumerate()
                .map(|(i, (path, line, column, message))| {
                    let mut loc = location(path, line, column);
                    loc["id"] = json!(i);
                    loc["message"] = json!({ "text": message });
                    loc
                })
                .collect();
            let mut result = json!({
                "ruleId": d.rule_code,
                "level": d.severity.word(),
                "message": { "text": d.message },
                "locations": [location(&d.path, d.line, d.column)],
                "relatedLocations": related,
                "partialFingerprints": {
                    "hekiBaselineKey/v1": format!("{}|{}|{}|{}", path_text(&d.path), d.qualname, d.rule_code, d.target)
                }
            });
            if let Some(help) = &d.help {
                result["properties"]["help"] = json!(help);
            }
            if let Some(label) = d.certainty.label() {
                result["properties"]["tags"] = json!([label]);
            }
            result
        })
        .collect();
    let notifications: Vec<serde_json::Value> = failures
        .iter()
        .map(|f| {
            let location = match f.position {
                Some((line, column)) => location(&f.path, line, column),
                None => json!({
                    "physicalLocation": { "artifactLocation": { "uri": path_uri(&f.path) } }
                }),
            };
            json!({
                "level": "error",
                "message": { "text": f.message },
                "locations": [location],
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
                    "rules": rules.iter().map(|(code, (slug, certainty))| {
                        let mut properties = json!({ "precision": certainty.sarif_precision() });
                        if let Some(label) = certainty.label() {
                            properties["tags"] = json!([label]);
                        }
                        json!({
                            "id": code,
                            "name": slug,
                            "helpUri": heki_env::problem::rule_page(slug),
                            "properties": properties,
                        })
                    }).collect::<Vec<_>>()
                }
            },
            "invocations": [{
                "executionSuccessful": failures.is_empty(),
                "toolExecutionNotifications": notifications,
            }],
            "results": results
        }]
    })
}

// ------------------------------------------------------------------------------ ratchet

/// Line-number-independent Ratchet baseline key (generalized from `dcg`'s `generation_store_allowlist.toml`).
/// The key of a group report ([`Diagnostic::group`]) names no file or function (both are
/// empty): its `target` lists the modules of the group.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BaselineKey {
    pub path: String,
    pub qualname: String,
    pub rule_code: String,
    pub target: String,
}

impl BaselineKey {
    /// Whether the key is of a group of modules, which is of no one file.
    pub fn is_group(&self) -> bool {
        self.path.is_empty()
    }

    /// The modules a group key's `target` lists.
    pub fn members(&self) -> BTreeSet<&str> {
        self.target.split(", ").collect()
    }

    /// Whether this group key, of the same rule, lists every module of the group key `other`.
    pub fn holds(&self, other: &BaselineKey) -> bool {
        self.is_group()
            && self.rule_code == other.rule_code
            && other.members().is_subset(&self.members())
    }

    /// The modules of this group key that none of the group keys `others` lists.
    pub fn members_outside(&self, others: &[BaselineKey]) -> Vec<&str> {
        let listed: BTreeSet<&str> = others.iter().flat_map(Self::members).collect();
        self.members()
            .into_iter()
            .filter(|m| !listed.contains(m))
            .collect()
    }

    /// The group keys of `counts` of this group key's rule that share a module with it.
    fn meeting<'c>(
        &self,
        counts: &'c BTreeMap<BaselineKey, Tally>,
    ) -> impl Iterator<Item = &'c BaselineKey> {
        let members = self.members();
        (counts.keys())
            .filter(move |k| k.is_group() && k.rule_code == self.rule_code)
            .filter(move |k| !k.members().is_disjoint(&members))
    }

    /// The reports of `counts` that this entry matched: those of its key, or for a group entry,
    /// the groups that share a module with it.
    pub fn found_in(&self, counts: &BTreeMap<BaselineKey, Tally>) -> Tally {
        if self.is_group() {
            self.meeting(counts).map(|k| counts[k]).sum()
        } else {
            counts.get(self).copied().unwrap_or_default()
        }
    }
}

impl std::fmt::Display for BaselineKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.is_group() {
            write!(f, "{}::{} ", self.path, self.qualname)?;
        }
        write!(f, "{} `{}`", self.rule_code, self.target)
    }
}

/// What the counts of a Ratchet baseline count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CountUnit {
    /// Violation sites ([`Diagnostic::sites`]).
    #[default]
    Sites,
    /// Diagnostics: files written before baselines counted sites.
    Diagnostics,
}

impl CountUnit {
    /// What a count counts, for messages (`2 site(s)`).
    pub fn noun(self) -> &'static str {
        match self {
            Self::Sites => "site(s)",
            Self::Diagnostics => "diagnostic(s)",
        }
    }
}

/// The format of a Ratchet baseline, by its `version`. A file keeps the meaning of its format
/// until `--update-baseline` rewrites it in the one this heki writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BaselineVersion {
    /// No `version`, or 1: counts diagnostics.
    V1,
    /// Counts violation sites, without telling the heuristic ones apart: any of them may be
    /// exact.
    V2,
    /// Counts violation sites, and how many of them were heuristic ([`BASELINE_VERSION`]).
    #[default]
    V3,
}

impl BaselineVersion {
    /// The format of a file whose `version` is `number`, if this heki reads it.
    fn of(number: Option<u32>) -> Option<Self> {
        match number {
            None | Some(1) => Some(Self::V1),
            Some(2) => Some(Self::V2),
            Some(BASELINE_VERSION) => Some(Self::V3),
            Some(_) => None,
        }
    }

    /// The on-disk `version`.
    pub fn number(self) -> u32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
            Self::V3 => BASELINE_VERSION,
        }
    }

    /// What the counts of this format count.
    pub fn unit(self) -> CountUnit {
        match self {
            Self::V1 => CountUnit::Diagnostics,
            Self::V2 | Self::V3 => CountUnit::Sites,
        }
    }

    /// Whether an entry records how many of its sites were heuristic.
    pub fn records_heuristic(self) -> bool {
        self == Self::V3
    }

    /// Whether a report whose shortest chain ends in another call than the one it names
    /// ([`Diagnostic::key_target`]) is keyed by the chain's call. Earlier files keyed it by the
    /// call it names.
    fn keys_by_chain_call(self) -> bool {
        self == Self::V3
    }
}

/// The baseline format this version writes: counts are violation sites, with the heuristic ones
/// among them.
pub const BASELINE_VERSION: u32 = 3;

/// The sites (or diagnostics) of a baseline key: an entry's allowance, or what a run found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Tally {
    pub count: u32,
    /// How many of `count` were heuristic reports.
    pub heuristic: u32,
}

impl Tally {
    /// The sites of `count` that were exact. The heuristic tier states the weaker claim of the
    /// same premises, so an allowance for a heuristic site never covers an exact one.
    pub fn exact(self) -> u32 {
        self.count.saturating_sub(self.heuristic)
    }

    /// Whether `found` needs more than this allowance: more sites, or more exact ones.
    pub fn exceeded_by(self, found: Self) -> bool {
        found.count > self.count || found.exact() > self.exact()
    }

    /// Whether this allowance holds more than `found` needs: `found` does not exceed it and has
    /// fewer sites, or, when the baseline records heuristic sites, fewer exact ones (an exact
    /// report that became heuristic).
    fn shrinks_to(self, found: Self, version: BaselineVersion) -> bool {
        let fewer_exact = version.records_heuristic() && found.exact() < self.exact();
        !self.exceeded_by(found) && (found.count < self.count || fewer_exact)
    }
}

impl std::ops::Add for Tally {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            count: self.count + other.count,
            heuristic: self.heuristic + other.heuristic,
        }
    }
}

impl std::iter::Sum for Tally {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), |a, b| a + b)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatchetBaseline {
    pub counts: BTreeMap<BaselineKey, Tally>,
    pub version: BaselineVersion,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RatchetComparison {
    pub regressions: Vec<Regression>,
    pub stale_entries: Vec<StaleEntry>,
}

/// A report key with more reports than its entry allows (or more exact ones), or a group no
/// entry holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Regression {
    pub key: BaselineKey,
    pub allowed: Tally,
    pub found: Tally,
    /// The recorded groups a group no entry holds shares a module with: the groups it joins.
    pub joins: Vec<BaselineKey>,
}

/// An entry that allows more than it matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleEntry {
    pub key: BaselineKey,
    pub allowed: Tally,
    /// [`BaselineKey::found_in`] the current reports.
    pub found: Tally,
    /// The groups made of a group entry's modules only, which the entry can shrink to.
    pub within: Vec<BaselineKey>,
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
        "baseline {path} has version {version}; this heki reads versions 1 to {BASELINE_VERSION}"
    )]
    Version { path: PathBuf, version: u32 },
    #[error("baseline {path} lists {key} more than once; keep one entry")]
    Duplicate { path: PathBuf, key: BaselineKey },
}

impl heki_env::Explain for BaselineError {
    fn explain(&self) -> heki_env::Problem {
        // `--update-baseline` reads an existing baseline first, so it fails on these two as well.
        let rewrite = "repair the file, or delete or rename it and record it again with `heki check --update-baseline`";
        let (path, message, help) = match self {
            Self::Read { path, cause } => (
                path,
                format!("cannot read the baseline: {cause}"),
                "name an existing file with `--baseline` or `baseline`, or record one with `--update-baseline`",
            ),
            Self::Write { path, cause } => (
                path,
                format!("cannot write the baseline: {cause}"),
                "make the file and its directory writable",
            ),
            Self::Toml { path, cause } => (
                path,
                format!(
                    "invalid baseline: {}",
                    cause.message().trim().replace('\n', "; ")
                ),
                rewrite,
            ),
            Self::Version { path, version } => (
                path,
                format!(
                    "the baseline has version {version}; this heki reads versions 1 to {BASELINE_VERSION}"
                ),
                rewrite,
            ),
            Self::Duplicate { path, key } => (
                path,
                format!("the baseline lists {key} more than once"),
                "keep one of the entries",
            ),
        };
        heki_env::Problem::new(message)
            .at_file(path)
            .help(help)
            .docs(heki_env::Docs::Baselines)
    }
}

/// On-disk shape: `version`, then `[[entries]]` sorted by key (minimal git diffs).
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct BaselineFile {
    /// The format of the file. Absent (or 1) in files written before baselines counted sites
    /// ([`BaselineVersion`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<u32>,
    /// The reports the baseline allows, sorted by path, qualified name, rule code, and target.
    #[serde(default)]
    entries: Vec<BaselineEntry>,
}

/// The entry of a group names no file or function. One written before a group was keyed by its
/// modules names those of the import closing the cycle its report showed.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct BaselineEntry {
    /// The file of the report, relative to the project root with forward slashes; left out for a
    /// group of modules.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    path: String,
    /// The qualified name of the function the report is in; left out at module level and for a
    /// group of modules.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    qualname: String,
    /// The code of the rule that reports (`EFF101`).
    rule_code: String,
    /// What the report is about, as the report names it.
    target: String,
    /// How many reports the entry allows.
    count: u32,
    /// How many of `count` were heuristic: left out when none were, and in files written before
    /// version 3.
    #[serde(default, skip_serializing_if = "is_zero")]
    heuristic: u32,
}

/// The JSON Schema of a baseline file of `version`, for its contents read as JSON (the file is
/// TOML, and a TOML parser gives the same values). Each version keeps the meaning its files
/// were written with: version 1 counts diagnostics and has no `version`, or 1; version 2 counts
/// sites; version 3 also counts the heuristic ones.
pub fn baseline_schema(version: BaselineVersion) -> serde_json::Value {
    let number = version.number();
    let mut schema = heki_env::published_schema::<BaselineFile>(
        &format!("baseline-{number}"),
        &format!("heki Ratchet baseline, version {number}"),
    );
    let (version_note, count_note) = match version {
        BaselineVersion::V1 => (
            "Absent or 1: the counts are diagnostics.",
            "How many diagnostics the entry allows.",
        ),
        BaselineVersion::V2 => (
            "2: the counts are violation sites.",
            "How many violation sites the entry allows.",
        ),
        BaselineVersion::V3 => (
            "3: the counts are violation sites, and `heuristic` says how many of them were heuristic.",
            "How many violation sites the entry allows.",
        ),
    };
    schema["properties"]["version"] = serde_json::json!({
        "description": version_note,
        "const": number,
        "type": "integer",
    });
    if version != BaselineVersion::V1 {
        schema["required"] = serde_json::json!(["entries", "version"]);
    }
    let entry = &mut schema["$defs"]["BaselineEntry"]["properties"];
    entry["count"]["description"] = count_note.into();
    if !version.records_heuristic() {
        entry
            .as_object_mut()
            .expect("an entry has properties")
            .remove("heuristic");
    }
    schema
}

#[allow(clippy::trivially_copy_pass_by_ref)] // `skip_serializing_if` passes a reference.
fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl RatchetBaseline {
    /// A baseline of site counts ([`baseline_counts`] of [`BaselineVersion::V3`]), in the format
    /// this heki writes.
    pub fn from_counts(counts: BTreeMap<BaselineKey, Tally>) -> Self {
        Self {
            counts: counts.into_iter().filter(|(_, c)| c.count > 0).collect(),
            version: BaselineVersion::V3,
        }
    }

    /// Compare current diagnostic counts against baseline (monotonic shrink). A group is
    /// covered by an entry of its rule holding all of its modules, whatever import its report
    /// sits at. An entry of a group is stale when it holds groups other than the one of exactly
    /// its modules (it can shrink to them), or when no group shares a module with it; a group
    /// that shares modules with it and that it does not hold is a regression that names it.
    /// Another key regresses when it has more sites than its entry allows, or more exact ones.
    pub fn compare(&self, current: &BTreeMap<BaselineKey, Tally>) -> RatchetComparison {
        let mut out = RatchetComparison::default();
        for (key, &cur) in current {
            // A group is held by its modules: its rule has one tier (`IMP101` is heuristic), so
            // its heuristic sites are recorded and never compared.
            if key.is_group() {
                let members = key.members();
                let joins: Vec<&BaselineKey> = key.meeting(&self.counts).collect();
                if !joins.iter().any(|e| members.is_subset(&e.members())) {
                    out.regressions.push(Regression {
                        key: key.clone(),
                        allowed: Tally::default(),
                        found: cur,
                        joins: joins.into_iter().cloned().collect(),
                    });
                }
                continue;
            }
            let allowed = self.counts.get(key).copied().unwrap_or_default();
            if allowed.exceeded_by(cur) {
                out.regressions.push(Regression {
                    key: key.clone(),
                    allowed,
                    found: cur,
                    joins: Vec::new(),
                });
            }
        }
        for (key, &allowed) in &self.counts {
            let found = key.found_in(current);
            let (stale, within) = if key.is_group() {
                let members = key.members();
                let within: Vec<BaselineKey> = (key.meeting(current))
                    .filter(|g| g.members().is_subset(&members))
                    .cloned()
                    .collect();
                let exact = matches!(&within[..], [g] if g.members() == members);
                (!exact && (found.count == 0 || !within.is_empty()), within)
            } else {
                (allowed.shrinks_to(found, self.version), Vec::new())
            };
            if stale {
                out.stale_entries.push(StaleEntry {
                    key: key.clone(),
                    allowed,
                    found,
                    within,
                });
            }
        }
        out
    }

    /// The key this baseline holds the report `d` under: its own, or, for a report of a name an
    /// import takes (`Diagnostic::key_module`) that this baseline has no entry for, the entry of
    /// the module, as baselines wrote it before reports named the import.
    pub fn key_of(&self, d: &Diagnostic, path: String) -> BaselineKey {
        let key = baseline_key(d, path, self.version);
        match &d.key_module {
            Some(module) if !self.counts.contains_key(&key) => {
                let of_module = BaselineKey {
                    target: module.clone(),
                    ..key.clone()
                };
                if self.counts.contains_key(&of_module) {
                    of_module
                } else {
                    key
                }
            }
            _ => key,
        }
    }

    /// The counts of `diags` per key of this baseline ([`Self::key_of`]), in its unit.
    pub fn counts_of(
        &self,
        diags: &[Diagnostic],
        key_path: &dyn Fn(&Path) -> String,
    ) -> BTreeMap<BaselineKey, Tally> {
        counted_by(diags, key_path, self.version.unit(), &|d, path| {
            self.key_of(d, path)
        })
    }

    /// What a check against this baseline reports of `diags`: every diagnostic of a key whose
    /// count (in the baseline's unit) grew past its entry. `key_path` gives the baseline path of
    /// a diagnostic's file. Returns the comparison the counts were kept by.
    pub fn keep_regressions(
        &self,
        diags: &mut Vec<Diagnostic>,
        key_path: &dyn Fn(&Path) -> String,
    ) -> RatchetComparison {
        let cmp = self.compare(&self.counts_of(diags, key_path));
        let regressed: BTreeSet<&BaselineKey> = cmp.regressions.iter().map(|r| &r.key).collect();
        diags.retain(|d| regressed.contains(&self.key_of(d, key_path(&d.path))));
        cmp
    }

    /// Read a baseline. `group_rule` tells the rules whose reports stand for groups: their
    /// entries written with the file and function of a report are read as entries of groups.
    pub fn from_toml_str(
        text: &str,
        path: &Path,
        group_rule: fn(&str) -> bool,
    ) -> Result<Self, BaselineError> {
        let file: BaselineFile = toml::from_str(text).map_err(|cause| BaselineError::Toml {
            path: path.to_path_buf(),
            cause,
        })?;
        let version = BaselineVersion::of(file.version).ok_or_else(|| BaselineError::Version {
            path: path.to_path_buf(),
            version: file.version.unwrap_or_default(),
        })?;
        // heki writes one entry per key; two (a hand merge of two baselines) would add their
        // allowances up, while each recorded the same findings.
        let mut counts = BTreeMap::new();
        for e in file.entries {
            let group = e.path.is_empty() || group_rule(&e.rule_code);
            let (file, qualname) = if group {
                (String::new(), String::new())
            } else {
                (e.path, e.qualname)
            };
            let key = BaselineKey {
                path: file,
                qualname,
                rule_code: e.rule_code,
                target: e.target,
            };
            if counts.contains_key(&key) {
                return Err(BaselineError::Duplicate {
                    path: path.to_path_buf(),
                    key,
                });
            }
            let tally = Tally {
                count: e.count,
                heuristic: e.heuristic,
            };
            counts.insert(key, tally);
        }
        Ok(Self { counts, version })
    }

    pub fn to_toml_string(&self) -> String {
        let file = BaselineFile {
            version: Some(self.version.number()),
            entries: self
                .counts
                .iter()
                .map(|(k, &Tally { count, heuristic })| BaselineEntry {
                    path: k.path.clone(),
                    qualname: k.qualname.clone(),
                    rule_code: k.rule_code.clone(),
                    target: k.target.clone(),
                    count,
                    heuristic,
                })
                .collect(),
        };
        let body = toml::to_string(&file).unwrap_or_default();
        format!(
            "# heki Ratchet baseline: counts may only shrink (`heki check --update-baseline`).\n{body}"
        )
    }

    /// [`Self::from_toml_str`] of the file at `path`.
    pub fn load(path: &Path, group_rule: fn(&str) -> bool) -> Result<Self, BaselineError> {
        let text = std::fs::read_to_string(path).map_err(|cause| BaselineError::Read {
            path: path.to_path_buf(),
            cause,
        })?;
        Self::from_toml_str(&text, path, group_rule)
    }

    pub fn save(&self, path: &Path) -> Result<(), BaselineError> {
        std::fs::write(path, self.to_toml_string()).map_err(|cause| BaselineError::Write {
            path: path.to_path_buf(),
            cause,
        })
    }
}

/// Baseline key of one diagnostic (`path` = project-relative POSIX path) in a file of `version`.
pub fn baseline_key(d: &Diagnostic, path: String, version: BaselineVersion) -> BaselineKey {
    // Where a group's report sits is no part of the group.
    let (path, qualname) = if d.group {
        (String::new(), String::new())
    } else {
        (path, d.qualname.clone())
    };
    BaselineKey {
        path,
        qualname,
        rule_code: d.rule_code.clone(),
        target: d
            .key_target
            .as_ref()
            .filter(|_| version.keys_by_chain_call())
            .unwrap_or(&d.target)
            .clone(),
    }
}

/// Count violation sites (or diagnostics) per baseline key, and the heuristic ones among them,
/// the way a file of `version` counts and keys them.
pub fn baseline_counts(
    diags: &[Diagnostic],
    key_path: &dyn Fn(&Path) -> String,
    version: BaselineVersion,
) -> BTreeMap<BaselineKey, Tally> {
    counted_by(diags, key_path, version.unit(), &|d, path| {
        baseline_key(d, path, version)
    })
}

fn counted_by(
    diags: &[Diagnostic],
    key_path: &dyn Fn(&Path) -> String,
    unit: CountUnit,
    key: &dyn Fn(&Diagnostic, String) -> BaselineKey,
) -> BTreeMap<BaselineKey, Tally> {
    let mut out: BTreeMap<BaselineKey, Tally> = BTreeMap::new();
    for d in diags {
        let count = match unit {
            CountUnit::Sites => d.sites.max(1),
            CountUnit::Diagnostics => 1,
        };
        let heuristic = if d.certainty == Certainty::Heuristic {
            count
        } else {
            0
        };
        let tally = out.entry(key(d, key_path(&d.path))).or_default();
        *tally = *tally + Tally { count, heuristic };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `github` format of a run with the default `fail-on`.
    const GITHUB: OutputFormat = OutputFormat::Github {
        fail_on: Severity::Error,
    };

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
            certainty: Certainty::Exact,
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
            contract_locations: Vec::new(),
            fix: None,
            sites: 1,
            unresolved_sites: Vec::new(),
            fold_key: None,
            open_records: Vec::new(),
            contracts: Vec::new(),
            contract_entries: Vec::new(),
            allowed_examples: Vec::new(),
            group: false,
            key_target: None,
            key_module: None,
            fails_run_by_config: false,
        }
    }

    /// `count` sites, `heuristic` of them heuristic.
    fn sites(count: u32, heuristic: u32) -> Tally {
        Tally { count, heuristic }
    }

    #[test]
    fn ratchet_detects_regressions_and_shrinkable_entries() {
        let k1 = key("src/a.py", "Parser.run", "DataModel.fields");
        let k2 = key("src/b.py", "Parser.old", "DataType.data_types");
        let baseline = RatchetBaseline {
            counts: BTreeMap::from([(k1.clone(), sites(1, 0)), (k2.clone(), sites(2, 0))]),
            version: BaselineVersion::V3,
        };
        let current = BTreeMap::from([(k1.clone(), sites(2, 0)), (k2.clone(), sites(1, 0))]);
        let cmp = baseline.compare(&current);
        assert_eq!(
            cmp.regressions,
            vec![Regression {
                key: k1,
                allowed: sites(1, 0),
                found: sites(2, 0),
                joins: Vec::new()
            }]
        );
        assert_eq!(
            cmp.stale_entries,
            vec![StaleEntry {
                key: k2,
                allowed: sites(2, 0),
                found: sites(1, 0),
                within: Vec::new()
            }]
        );
    }

    #[test]
    fn an_entry_allows_the_tiers_it_recorded() {
        let k = key("src/a.py", "Parser.run", "DataModel.fields");
        let compare = |version, allowed: Tally, found: Tally| {
            let baseline = RatchetBaseline {
                counts: BTreeMap::from([(k.clone(), allowed)]),
                version,
            };
            let cmp = baseline.compare(&BTreeMap::from([(k.clone(), found)]));
            (!cmp.regressions.is_empty(), !cmp.stale_entries.is_empty())
        };
        // (regressed, can shrink)
        let cases = [
            // A recorded heuristic site does not allow an exact one.
            (sites(1, 1), sites(1, 0), (true, false)),
            (sites(2, 1), sites(2, 0), (true, false)),
            (sites(2, 1), sites(2, 1), (false, false)),
            // An exact site that became heuristic is covered, and the entry can shrink to it.
            (sites(1, 0), sites(1, 1), (false, true)),
            (sites(2, 1), sites(1, 0), (false, true)),
            (sites(2, 1), sites(3, 3), (true, false)),
        ];
        for (allowed, found, expected) in cases {
            assert_eq!(
                compare(BaselineVersion::V3, allowed, found),
                expected,
                "{allowed:?} {found:?}"
            );
        }
        // An entry written before version 3 lets any of its sites be exact, and it holds no more
        // than it matched when they are heuristic now.
        assert_eq!(
            compare(BaselineVersion::V2, sites(1, 0), sites(1, 0)),
            (false, false)
        );
        assert_eq!(
            compare(BaselineVersion::V2, sites(1, 0), sites(1, 1)),
            (false, false)
        );
        assert_eq!(
            compare(BaselineVersion::V2, sites(1, 0), sites(2, 1)),
            (true, false)
        );
        // A hand-edited entry with more heuristic sites than sites allows no exact one.
        assert_eq!(sites(1, 2).exact(), 0);
    }

    #[test]
    fn ratchet_toml_round_trip_is_sorted() {
        let no_group: fn(&str) -> bool = |_| false;
        let baseline = RatchetBaseline::from_counts(BTreeMap::from([
            (key("src/z.py", "Z.run", "M.f"), sites(1, 1)),
            (key("src/a.py", "A.run", "M.f"), sites(3, 0)),
            (key("src/zero.py", "Z.none", "M.f"), sites(0, 0)),
        ]));
        let text = baseline.to_toml_string();
        assert!(text.find("src/a.py").unwrap() < text.find("src/z.py").unwrap());
        assert!(!text.contains("src/zero.py"));
        // The count of heuristic sites is left out when there are none.
        assert_eq!(text.matches("heuristic = ").count(), 1, "{text}");
        assert!(text.contains("count = 1\nheuristic = 1\n"), "{text}");
        let back = RatchetBaseline::from_toml_str(&text, Path::new("b.toml"), no_group).unwrap();
        assert_eq!(back, baseline);
        assert!(
            RatchetBaseline::from_toml_str(
                "[[entries]]\npath = 1\n",
                Path::new("b.toml"),
                no_group
            )
            .is_err()
        );
        assert!(
            RatchetBaseline::from_toml_str("bogus = 1\n", Path::new("b.toml"), no_group).is_err()
        );
    }

    #[test]
    fn baseline_counts_group_by_key() {
        let d = diag(1, "MUT201", "m");
        let mut two_sites = d.clone();
        two_sites.sites = 2;
        let mut heuristic = d.clone();
        heuristic.certainty = Certainty::Heuristic;
        let diags = [d.clone(), d, two_sites, heuristic];
        let counts = |version| {
            baseline_counts(&diags, &|p| path_text(p), version)
                .values()
                .copied()
                .collect::<Vec<_>>()
        };
        // Sites: a report standing for two sites counts twice; diagnostics: once.
        assert_eq!(counts(BaselineVersion::V3), vec![sites(5, 1)]);
        assert_eq!(counts(BaselineVersion::V1), vec![sites(4, 1)]);
    }

    #[test]
    fn baseline_versions_set_the_count_unit() {
        let path = Path::new("b.toml");
        let entry = "[[entries]]\npath = \"a.py\"\nqualname = \"f\"\nrule_code = \"MUT201\"\ntarget = \"M.f\"\ncount = 1\n";
        // Files written before baselines counted sites keep counting diagnostics.
        for old in [entry.to_owned(), format!("version = 1\n{entry}")] {
            let b = RatchetBaseline::from_toml_str(&old, path, |_| false).unwrap();
            assert_eq!(b.version, BaselineVersion::V1);
            assert_eq!(b.version.unit().noun(), "diagnostic(s)");
            assert!(!b.version.records_heuristic());
            assert!(b.to_toml_string().contains("version = 1\n"));
        }
        for (number, version) in [(2, BaselineVersion::V2), (3, BaselineVersion::V3)] {
            let b = RatchetBaseline::from_toml_str(
                &format!("version = {number}\n{entry}"),
                path,
                |_| false,
            )
            .unwrap();
            assert_eq!(b.version, version);
            assert_eq!(b.version.unit().noun(), "site(s)");
            assert_eq!(b.version.records_heuristic(), number == 3);
            assert!(
                b.to_toml_string()
                    .contains(&format!("version = {number}\n"))
            );
        }
        let twice = format!(
            "version = 3\n{entry}{}",
            entry.replace("count = 1", "count = 4294967295")
        );
        assert_eq!(
            RatchetBaseline::from_toml_str(&twice, path, |_| false)
                .unwrap_err()
                .to_string(),
            "baseline b.toml lists a.py::f MUT201 `M.f` more than once; keep one entry"
        );
        let err = RatchetBaseline::from_toml_str(&format!("version = 7\n{entry}"), path, |_| false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "baseline b.toml has version 7; this heki reads versions 1 to 3"
        );
    }

    #[test]
    fn diagnostics_write_sites_to_json_only_above_one() {
        let mut d = diag(1, "MUT201", "m");
        let one = render(
            std::slice::from_ref(&d),
            &[],
            OutputFormat::Json,
            &Stylesheet::plain(),
            &|_, _| None,
        );
        assert!(!one.contains("\"sites\""), "{one}");
        d.sites = 3;
        let three = render(
            std::slice::from_ref(&d),
            &[],
            OutputFormat::Json,
            &Stylesheet::plain(),
            &|_, _| None,
        );
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
        let out = render(
            &[d],
            &[],
            OutputFormat::Json,
            &Stylesheet::plain(),
            &|_, _| None,
        );
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
        let full = render(
            std::slice::from_ref(&d),
            &[],
            OutputFormat::Full,
            &Stylesheet::plain(),
            &line,
        );
        assert!(
            full.contains(&format!("\n  | \t{}^\n", " ".repeat(13))),
            "{full:?}"
        );
        let s: serde_json::Value = serde_json::from_str(&render(
            &[d],
            &[],
            OutputFormat::Sarif,
            &Stylesheet::plain(),
            &line,
        ))
        .unwrap();
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
    fn github_body_lists_related_locations_on_one_command_line() {
        let mut d = diag(2, "EFF101", "calls `f`");
        d.related = vec![
            RelatedLocation {
                path: PathBuf::from("src/b\r\n::error::x.py"),
                line: 3,
                column: 1,
                span: Span { start: 0, end: 0 },
                message: "100% a\r\n::error file=z::forged".into(),
            },
            RelatedLocation {
                path: PathBuf::from("src/c.py"),
                line: 4,
                column: 2,
                span: Span { start: 0, end: 0 },
                message: "then `g`".into(),
            },
        ];
        let none = |_: &Path, _: u32| None;
        let out = render(&[d], &[], GITHUB, &Stylesheet::plain(), &none);
        assert_eq!(
            out,
            "::error file=src/a.py,line=2,col=5,title=heki EFF101::calls `f`\
             %0Arelated: src/b%0D%0A::error::x.py:3:1: 100%25 a%0D%0A::error file=z::forged\
             %0Arelated: src/c.py:4:2: then `g`\n"
        );
        assert_eq!(out.lines().count(), 1);
        assert!(!out.contains('\r'));
    }

    #[test]
    fn github_without_related_entries_prints_the_message_alone() {
        let mut d = diag(2, "EFF101", "a: b, c");
        d.related.clear();
        d.path = PathBuf::from("src/x,y:z.py");
        let none = |_: &Path, _: u32| None;
        assert_eq!(
            render(&[d], &[], GITHUB, &Stylesheet::plain(), &none),
            "::error file=src/x%2Cy%3Az.py,line=2,col=5,title=heki EFF101::a: b, c\n"
        );
    }

    #[test]
    fn formats_render_every_style() {
        let mut diags = vec![diag(2, "MUT201", "write, outside: owners\nsecond")];
        diags[0].help = Some("use the store".into());
        let line = |_: &Path, n: u32| (n == 2).then(|| "    model.fields.append(x)".to_owned());
        let full = render(&diags, &[], OutputFormat::Full, &Stylesheet::plain(), &line);
        assert!(full.starts_with("src/a.py:2:5: MUT201 (write-owner-violation) [error] write"));
        assert!(full.contains("2 |     model.fields.append(x)"));
        assert!(full.contains("  |     ^"));
        assert!(
            full.ends_with("  = related: src/b.py:3:1: calls helper\n  = help: use the store\n\n"),
            "{full}"
        );
        assert_eq!(
            render(
                &diags,
                &[],
                OutputFormat::Concise,
                &Stylesheet::plain(),
                &line
            ),
            "src/a.py:2:5: MUT201 write, outside: owners\nsecond\n"
        );
        assert_eq!(
            render(&diags, &[], GITHUB, &Stylesheet::plain(), &line),
            "::error file=src/a.py,line=2,col=5,title=heki MUT201::write, outside: owners%0Asecond%0Arelated: src/b.py:3:1: calls helper\n"
        );
        let json: Vec<Diagnostic> = serde_json::from_str(&render(
            &diags,
            &[],
            OutputFormat::Json,
            &Stylesheet::plain(),
            &line,
        ))
        .unwrap();
        assert_eq!(json, diags);
        let s: serde_json::Value = serde_json::from_str(&render(
            &diags,
            &[],
            OutputFormat::Sarif,
            &Stylesheet::plain(),
            &line,
        ))
        .unwrap();
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
    fn only_heuristic_reports_are_labelled_and_sarif_gives_every_rule_a_precision() {
        let mut heuristic = diag(2, "EFF102", "m");
        heuristic.rule_slug = "loop-unpooled-client".into();
        heuristic.certainty = Certainty::Heuristic;
        heuristic.related.clear();
        let mut exact = diag(3, "MUT201", "m");
        exact.related.clear();
        let diags = [heuristic, exact];
        let none = |_: &Path, _: u32| None;
        assert_eq!(
            render(&diags, &[], OutputFormat::Full, &Stylesheet::plain(), &none),
            "src/a.py:2:5: EFF102 (loop-unpooled-client) [error, heuristic] m\n\n\
             src/a.py:3:5: MUT201 (write-owner-violation) [error] m\n\n"
        );
        assert_eq!(
            render(
                &diags,
                &[],
                OutputFormat::Concise,
                &Stylesheet::plain(),
                &none
            ),
            "src/a.py:2:5: EFF102 [heuristic] m\nsrc/a.py:3:5: MUT201 m\n"
        );
        // A heuristic report fails the run only at `fail-on = "warning"`.
        assert_eq!(
            render(&diags, &[], GITHUB, &Stylesheet::plain(), &none),
            "::warning file=src/a.py,line=2,col=5,title=heki EFF102 (heuristic)::m\n\
             ::error file=src/a.py,line=3,col=5,title=heki MUT201::m\n"
        );
        let failing_on_warnings = OutputFormat::Github {
            fail_on: Severity::Warning,
        };
        assert!(
            render(
                &diags,
                &[],
                failing_on_warnings,
                &Stylesheet::plain(),
                &none
            )
            .starts_with("::error file=src/a.py,line=2,col=5,title=heki EFF102 (heuristic)::m\n")
        );
        let json: serde_json::Value = serde_json::from_str(&render(
            &diags,
            &[],
            OutputFormat::Json,
            &Stylesheet::plain(),
            &none,
        ))
        .unwrap();
        assert_eq!(
            [&json[0]["certainty"], &json[1]["certainty"]],
            ["heuristic", "exact"]
        );
        // `heki rule` prints the word the JSON output writes.
        for certainty in [Certainty::Exact, Certainty::Heuristic] {
            assert_eq!(serde_json::json!(certainty), certainty.word());
        }
        // A rule whose reports differ in certainty: its descriptor takes the least certain one,
        // and only the heuristic result is tagged.
        let mut proven = diag(4, "OPT110", "m");
        proven.rule_slug = "loop-linear-membership".into();
        proven.related.clear();
        let mut unproven = proven.clone();
        unproven.line = 5;
        unproven.certainty = Certainty::Heuristic;
        let diags = [diags[0].clone(), diags[1].clone(), proven, unproven];
        let s: serde_json::Value = serde_json::from_str(&render(
            &diags,
            &[],
            OutputFormat::Sarif,
            &Stylesheet::plain(),
            &none,
        ))
        .unwrap();
        assert_eq!(
            s["runs"][0]["tool"]["driver"]["rules"],
            serde_json::json!([
                {
                    "id": "EFF102",
                    "name": "loop-unpooled-client",
                    "helpUri": "https://heki.koxudaxi.dev/rules/loop-unpooled-client/",
                    "properties": { "precision": "medium", "tags": ["heuristic"] }
                },
                {
                    "id": "MUT201",
                    "name": "write-owner-violation",
                    "helpUri": "https://heki.koxudaxi.dev/rules/write-owner-violation/",
                    "properties": { "precision": "very-high" }
                },
                {
                    "id": "OPT110",
                    "name": "loop-linear-membership",
                    "helpUri": "https://heki.koxudaxi.dev/rules/loop-linear-membership/",
                    "properties": { "precision": "medium", "tags": ["heuristic"] }
                }
            ])
        );
        let tags: Vec<_> = (0..4)
            .map(|i| &s["runs"][0]["results"][i]["properties"]["tags"])
            .collect();
        let heuristic = serde_json::json!(["heuristic"]);
        assert_eq!(
            tags,
            [
                &heuristic,
                &serde_json::Value::Null,
                &serde_json::Value::Null,
                &heuristic
            ]
        );
    }

    #[test]
    fn a_report_is_exact_only_when_its_premises_and_its_rule_are() {
        use Certainty::{Exact, Heuristic};
        assert_eq!(Exact.at_most(Exact), Exact);
        assert_eq!(Exact.at_most(Heuristic), Heuristic);
        assert_eq!(Heuristic.at_most(Exact), Heuristic);
        assert_eq!(Heuristic.at_most(Heuristic), Heuristic);
    }

    #[test]
    fn a_report_without_help_has_no_help_line_and_a_null_field() {
        let diags = vec![diag(2, "MUT201", "m")];
        let full = render(
            &diags,
            &[],
            OutputFormat::Full,
            &Stylesheet::plain(),
            &|_, _| None,
        );
        assert!(!full.contains("= help:"), "{full}");
        let json: serde_json::Value = serde_json::from_str(&render(
            &diags,
            &[],
            OutputFormat::Json,
            &Stylesheet::plain(),
            &|_, _| None,
        ))
        .unwrap();
        assert_eq!(
            json[0].get("help"),
            Some(&serde_json::Value::Null),
            "{json}"
        );
        let s: serde_json::Value = serde_json::from_str(&render(
            &diags,
            &[],
            OutputFormat::Sarif,
            &Stylesheet::plain(),
            &|_, _| None,
        ))
        .unwrap();
        assert!(
            s["runs"][0]["results"][0].get("properties").is_none(),
            "{s}"
        );
    }
}
