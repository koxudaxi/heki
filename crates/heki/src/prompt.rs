//! `heki check --generate-prompt [QUESTION]`: a prompt for consulting a language model about a
//! run, from the analysis `heki check` runs for the same configuration and selection: the
//! effective configuration, the rules that apply and what their reports state, the diagnostics
//! of the run, the question, and how to answer. `--output-format json` prints the same content as
//! one object, whose JSON Schema `--output-format-json-schema generate-prompt` prints. The
//! command writes no file and exits with 0 once the prompt is printed.

use crate::check::{
    Touched, baseline_path, check_selectors, fail_on, load_and_index, project_wide_files,
    show_paths, shown_dir, shown_errors, shown_text,
};
use crate::{CheckArgs, OutputFormat};
use anyhow::Result;
use heki_diagnostics::{Diagnostic, RatchetBaseline, Stylesheet, render};
use heki_env::{HekiConfig, Severity};
use heki_rules::project::{CommandPaths, Facts, ProjectEnv, baseline_key};
use heki_rules::{
    Activation, CORE_RULES, Certainty, EffectiveSelectors, RuleMeta, fold_root_causes,
    heuristic_hint,
};
use serde::ser::SerializeMap as _;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

/// The version of the JSON object: it changes when a field is removed or changes meaning.
const PROMPT_VERSION: u32 = 1;

/// The bytes of concise diagnostic lines a prompt holds. The diagnostics past them are counted,
/// not shown, so that the prompt of a large run still fits what a model reads.
const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;

const INTRO: &str = "# Consulting a model about heki

You are helping with a Python project that heki checks. heki is a static checker that follows \
calls across files and reports side effects and mutations. Each diagnostic states a claim about \
the code; its rule's documentation lists the premises of that claim and what to do instead. This \
prompt holds the configuration of one `heki check` run, the rules that ran, and the diagnostics \
they reported. heki's documentation for models: https://heki.koxudaxi.dev/llms.txt.
";

/// How to answer, the end of the prompt.
const GUIDANCE: &[&str] = &[
    "Fix each diagnostic at its root cause. `heki check --output-format json` lists every hop of \
     the call chain behind a diagnostic in `related`: change the function where the effect or the \
     change happens, not only the line reported.",
    "Do not silence a diagnostic with `# heki: ignore[...]`, `# noqa`, `ignore`, \
     `per-file-ignores`, or a baseline entry, unless the rule's documentation describes the case \
     as intended.",
    "Never make a run pass by weakening or widening a contract (`allowed-writers`, \
     `allowed-readers`, `allowed-callers`, `allowed`, `forbid`, and the other keys that say what \
     is allowed), by adding a suppression, or by growing a baseline: fix the code, and leave a \
     change of a contract to the maintainer.",
    "A `heuristic` diagnostic can be wrong about the code: check what its message says against \
     the code before changing anything, and say so when it does not hold.",
    "Rely on proven fixes only: `heki check --fix` applies the rewrites heki proved safe, and the \
     `blocker` of a diagnostic names what keeps its rewrite from being safe.",
    "To make heki enforce a rule of the code base that no type states (who may write, read, or \
     call something, what a scope must not reach, or which modules a module may import), write \
     a contract: an array of tables under `[tool.heki]` in `pyproject.toml`, at the top level of \
     a `heki.toml` (`write-owners`, `read-owners`, `call-owners`, `effect-boundaries`, \
     `readonly-returns`, `invalidation`, `import-boundaries`), whose names are dotted patterns or \
     globs. See https://heki.koxudaxi.dev/configuration/#contracts.",
    "When you draft a contract, write the thin wall the maintainers intend in `heki.toml`, with \
     `fail = true` on each table and a `message` that says what to do instead. Do not copy the \
     coupling the code has today into an `allowed-*` list: record the violations that already \
     exist in a baseline (`heki check --baseline <file> --update-baseline`), so that only new ones \
     fail.",
    "Answer with the files and functions to change, the change itself, and the `heki check` \
     command that confirms it.",
];

#[derive(Serialize)]
struct Prompt {
    version: u32,
    question: Option<String>,
    configuration: Configuration,
    rules: Vec<AppliedRule>,
    /// The heuristic rules that are off only because `lint.heuristic` is.
    heuristic_rules_off: Vec<&'static str>,
    diagnostics: Diagnostics,
    guidance: &'static [&'static str],
}

#[derive(Serialize)]
struct Configuration {
    /// The project root, as `heki check` prints paths.
    root: String,
    config_file: Option<String>,
    target_version: String,
    source_roots: Vec<String>,
    /// `src` when the `src` setting lists the source roots, `detected` when heki found them.
    source_roots_from: &'static str,
    /// `ModuleLayout::member_packages`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    member_packages: Vec<String>,
    selection: Selection,
    baseline: Option<String>,
    /// The analyzed files `--touched` names, and the files PATHS name in a project; absent
    /// without either.
    #[serde(skip_serializing_if = "Option::is_none")]
    touched: Option<Vec<String>>,
    /// Each contract key with its tables, each table with the keys it sets.
    contracts: BTreeMap<&'static str, Vec<Map<String, Value>>>,
}

/// The rule selection in effect: the command line's over the configuration's.
#[derive(Serialize)]
struct Selection {
    /// `None`: the default rules.
    select: Option<Vec<String>>,
    extend_select: Vec<String>,
    ignore: Vec<String>,
    per_file_ignores: BTreeMap<String, Vec<String>>,
    heuristic: bool,
    fail_on: Severity,
}

#[derive(Serialize)]
struct AppliedRule {
    code: &'static str,
    slug: &'static str,
    certainty: Certainty,
    activation: Activation,
    /// What a report states, without the premises the rule's documentation lists.
    claim: String,
}

#[derive(Serialize)]
struct Diagnostics {
    files_checked: usize,
    total: usize,
    shown: Vec<ShownDiagnostic>,
    /// The diagnostics past `limit_bytes` of concise lines.
    omitted: usize,
    limit_bytes: usize,
    /// The heuristic reports of exact rules withheld because `lint.heuristic` is off.
    heuristic_reports_withheld: usize,
    files_not_analyzed: Vec<String>,
}

#[derive(Serialize)]
struct ShownDiagnostic {
    path: String,
    line: u32,
    column: u32,
    rule_code: String,
    certainty: Certainty,
    message: String,
    #[serde(skip)]
    concise: String,
}

pub(crate) fn run(args: &CheckArgs, question: &str) -> Result<ExitCode> {
    let paths = CommandPaths::of(&args.paths);
    let indexed = load_and_index(
        &paths.analyzed,
        &args.analysis,
        &check_selectors(args),
        args.output_format,
        None,
        Facts::Check,
    )?;
    let (mut diags, withheld) = indexed.diagnose()?;
    let project = &indexed.project;
    let baseline = baseline_path(&args.analysis, &project.env);
    if let Some(path) = &baseline {
        RatchetBaseline::load(path, heki_rules::reports_groups)?
            .keep_regressions(&mut diags, &baseline_key(&project.files));
    }
    let wide = project_wide_files(&args.analysis, &project.env);
    let touched = Touched::of(&args.touched, &paths.files, project, &wide);
    if let Some(touched) = &touched {
        touched.warn(args.output_format);
        touched.keep(&mut diags, baseline.is_some());
    }
    fold_root_causes(&mut diags);
    show_paths(&mut diags);
    let selection = &indexed.selection;
    let prompt = Prompt {
        version: PROMPT_VERSION,
        question: Some(question.trim())
            .filter(|q| !q.is_empty())
            .map(str::to_owned),
        configuration: Configuration::of(
            args,
            &project.env,
            selection.heuristic(),
            baseline.as_deref(),
            touched.as_ref(),
        ),
        rules: CORE_RULES
            .iter()
            .filter(|r| selection.is_enabled(r.code))
            .map(AppliedRule::of)
            .collect(),
        heuristic_rules_off: selection.heuristic_off().iter().copied().collect(),
        diagnostics: Diagnostics::of(
            &diags,
            project.files.len(),
            Touched::shown(touched.as_ref(), &withheld),
            shown_errors(&project.errors),
        ),
        guidance: GUIDANCE,
    };
    if matches!(args.output_format, OutputFormat::Json) {
        println!("{}", serde_json::to_string_pretty(&prompt)?);
    } else {
        print!("{}", prompt.markdown()?);
    }
    Ok(ExitCode::SUCCESS)
}

impl Configuration {
    /// The configuration of `env` for the run of `args`, whose selection resolved
    /// `lint.heuristic` to `heuristic`, which compares with `baseline`, and which shows the
    /// reports that touch `touched`.
    fn of(
        args: &CheckArgs,
        env: &ProjectEnv,
        heuristic: bool,
        baseline: Option<&Path>,
        touched: Option<&Touched>,
    ) -> Self {
        Self {
            root: shown_dir(&env.root),
            config_file: env.config_file.as_deref().map(shown_text),
            target_version: env.runtime.python_version.tag(),
            source_roots: env.layout.source_roots.clone(),
            member_packages: env.layout.member_packages.clone(),
            source_roots_from: if env.cfg.src.is_some() {
                "src"
            } else {
                "detected"
            },
            selection: Selection::of(args, env, heuristic),
            baseline: baseline.map(shown_text),
            touched: touched.map(Touched::shown_files),
            contracts: contracts(&env.cfg),
        }
    }
}

impl Selection {
    fn of(args: &CheckArgs, env: &ProjectEnv, heuristic: bool) -> Self {
        let lint = &env.cfg.lint;
        let selectors = EffectiveSelectors::of(lint, &check_selectors(args));
        Self {
            select: selectors.select,
            extend_select: selectors.extend_select,
            ignore: selectors.ignore,
            per_file_ignores: lint.per_file_ignores.clone(),
            heuristic,
            fail_on: fail_on(args, lint),
        }
    }
}

/// The contracts of `cfg` by key, each table with the keys it sets: a key left at its default
/// (no value, an empty string or list) is left out.
fn contracts(cfg: &HekiConfig) -> BTreeMap<&'static str, Vec<Map<String, Value>>> {
    // A configuration holds strings, booleans, lists, and tables with string keys.
    let all = serde_json::to_value(cfg).expect("the configuration serializes");
    let set_keys = |table: &Value| {
        let mut keys = table.as_object().cloned().unwrap_or_default();
        keys.retain(|_, v| !is_unset(v));
        keys
    };
    HekiConfig::CONTRACT_KEYS
        .iter()
        .map(|&key| {
            let tables = all[key].as_array().into_iter().flatten();
            (key, tables.map(set_keys).collect())
        })
        .collect()
}

fn is_unset(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(keys) => keys.is_empty(),
        _ => false,
    }
}

impl AppliedRule {
    fn of(rule: &'static RuleMeta) -> Self {
        Self {
            code: rule.code,
            slug: rule.slug,
            certainty: rule.certainty,
            activation: rule.activation,
            claim: claim(rule),
        }
    }
}

/// The first paragraph of the rule's "What a report states" section, on one line.
fn claim(rule: &RuleMeta) -> String {
    let section = rule
        .doc
        .split_once("\n## What a report states\n")
        .map_or("", |(_, rest)| rest);
    let paragraph = section
        .trim_start()
        .split("\n\n")
        .next()
        .unwrap_or_default();
    paragraph.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Diagnostics {
    /// `diags` as the prompt shows them: in the order `heki check` prints them, while their
    /// concise lines fit [`MAX_DIAGNOSTIC_BYTES`].
    fn of(
        diags: &[Diagnostic],
        files_checked: usize,
        withheld: usize,
        files_not_analyzed: Vec<String>,
    ) -> Self {
        let mut shown = Vec::new();
        let mut bytes = 0;
        for d in diags {
            let concise = render(
                std::slice::from_ref(d),
                &[],
                heki_diagnostics::OutputFormat::Concise,
                &Stylesheet::plain(),
                &|_: &Path, _| None,
            );
            bytes += concise.len();
            if bytes > MAX_DIAGNOSTIC_BYTES {
                break;
            }
            shown.push(ShownDiagnostic {
                path: heki_diagnostics::path_text(&d.path),
                line: d.line,
                column: d.column,
                rule_code: d.rule_code.clone(),
                certainty: d.certainty,
                message: d.message.clone(),
                concise: concise.trim_end().to_owned(),
            });
        }
        Self {
            files_checked,
            total: diags.len(),
            omitted: diags.len() - shown.len(),
            shown,
            limit_bytes: MAX_DIAGNOSTIC_BYTES,
            heuristic_reports_withheld: withheld,
            files_not_analyzed,
        }
    }
}

/// `items` as Markdown code, or `none`.
fn code_list(items: &[impl AsRef<str>]) -> String {
    if items.is_empty() {
        return "none".to_owned();
    }
    let codes: Vec<String> = items.iter().map(|i| format!("`{}`", i.as_ref())).collect();
    codes.join(", ")
}

impl Prompt {
    fn markdown(&self) -> Result<String> {
        let mut out = INTRO.to_owned();
        out.push_str("\n## Question\n\n");
        out.push_str(self.question.as_deref().unwrap_or(
            "No question was given: explain what the diagnostics below mean and how to fix each \
             one at its root.",
        ));
        out.push('\n');
        write_configuration(&mut out, &self.configuration)?;
        write_rules(&mut out, &self.rules, &self.heuristic_rules_off);
        write_diagnostics(&mut out, &self.diagnostics, &self.configuration);
        out.push_str("\n## How to answer\n\n");
        for line in self.guidance {
            let _ = writeln!(out, "- {line}");
        }
        Ok(out)
    }
}

fn write_configuration(out: &mut String, c: &Configuration) -> Result<()> {
    let s = &c.selection;
    let source_roots = match (c.source_roots.is_empty(), c.source_roots_from) {
        (true, _) => "none: module names start at the project root".to_owned(),
        (false, "src") => format!("{} (the `src` setting)", code_list(&c.source_roots)),
        (false, _) => format!("{} (detected)", code_list(&c.source_roots)),
    };
    let select = s
        .select
        .as_deref()
        .map_or_else(|| "the default rules".to_owned(), code_list);
    let per_file: Vec<String> = s
        .per_file_ignores
        .iter()
        .map(|(glob, rules)| format!("`{glob}`: {}", code_list(rules)))
        .collect();
    let per_file = if per_file.is_empty() {
        "none".to_owned()
    } else {
        per_file.join("; ")
    };
    let mut lines = vec![
        format!("Project root: `{}`", c.root),
        format!(
            "Configuration file: {}",
            c.config_file
                .as_deref()
                .map_or_else(|| "none (the defaults)".to_owned(), |f| format!("`{f}`"))
        ),
        format!("Target Python version: `{}`", c.target_version),
        format!("Source roots: {source_roots}"),
    ];
    if !c.member_packages.is_empty() {
        lines.push(format!(
            "Workspace member packages, each named below its member: {}",
            code_list(&c.member_packages)
        ));
    }
    lines.extend([
        format!("`select`: {select}"),
        format!("`extend-select`: {}", code_list(&s.extend_select)),
        format!("`ignore`: {}", code_list(&s.ignore)),
        format!("`per-file-ignores`: {per_file}"),
        format!("`lint.heuristic`: `{}`", s.heuristic),
        format!("`fail-on`: `{}`", s.fail_on.word()),
        format!(
            "Baseline: {}",
            c.baseline.as_deref().map_or_else(
                || "none".to_owned(),
                |b| format!("`{b}` (only the reports it does not allow are shown)")
            )
        ),
    ]);
    if let Some(touched) = &c.touched {
        lines.push(format!(
            "Touched files: {} (only the reports in one of them, or with a hop of their call chain in one, are shown)",
            code_list(touched)
        ));
    }
    out.push_str("\n## Configuration\n\n");
    for line in lines {
        let _ = writeln!(out, "- {line}");
    }
    out.push_str("\n### Contracts\n\n");
    let toml = contracts_toml(&c.contracts)?;
    if toml.is_empty() {
        out.push_str("No contract is declared.\n");
    } else {
        let _ = write!(out, "```toml\n{toml}```\n");
    }
    Ok(())
}

/// The declared contracts as a `pyproject.toml` writes them (`[[tool.heki.write-owners]]`).
fn contracts_toml(contracts: &BTreeMap<&'static str, Vec<Map<String, Value>>>) -> Result<String> {
    let mut out = String::new();
    for (&key, tables) in contracts {
        let tables: Vec<NameFirst<'_>> = tables.iter().map(NameFirst).collect();
        if tables.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let heki = BTreeMap::from([(key, tables)]);
        let tool = BTreeMap::from([("heki", heki)]);
        out.push_str(&toml::to_string(&BTreeMap::from([("tool", tool)]))?);
    }
    Ok(out)
}

/// A contract table with its `name` first, as the documentation writes contracts.
struct NameFirst<'a>(&'a Map<String, Value>);

impl Serialize for NameFirst<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let (name, rest): (Vec<_>, Vec<_>) = self.0.iter().partition(|(key, _)| *key == "name");
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in name.into_iter().chain(rest) {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

fn write_rules(out: &mut String, rules: &[AppliedRule], off: &[&str]) {
    let _ = write!(
        out,
        "\n## Rules that apply\n\n`heki check` runs these {} rule(s). An `exact` rule \
         established every premise of a report's claim; a `heuristic` one may have left a \
         premise open, so its report can be wrong about the code. The activation says when a \
         rule runs: `default` without being named, `opt-in` only when named, `contract` when its \
         contract is declared. `heki rule <CODE>` prints a rule's documentation: what it checks, \
         the premises of its claim, and what to do instead.\n\n",
        rules.len()
    );
    for r in rules {
        let _ = writeln!(
            out,
            "- `{}` (`{}`, {}, {}): {}",
            r.code,
            r.slug,
            r.certainty.word(),
            r.activation.word(),
            r.claim
        );
    }
    if !off.is_empty() {
        let _ = writeln!(
            out,
            "\nThese heuristic rules are off; `--heuristic` (`lint.heuristic = true`) runs them: \
             {}.",
            code_list(off)
        );
    }
}

fn write_diagnostics(out: &mut String, d: &Diagnostics, c: &Configuration) {
    out.push_str("\n## Diagnostics\n\n");
    let scope = match (c.baseline.is_some(), c.touched.is_some()) {
        (false, false) => "",
        (true, false) => ", among the reports the baseline does not allow",
        (false, true) => ", among the reports that touch the touched files",
        (true, true) => {
            ", among the reports the baseline does not allow that touch the touched files"
        }
    };
    if d.total == 0 {
        let _ = writeln!(
            out,
            "`heki check` reported no diagnostic in {} file(s){scope}.",
            d.files_checked
        );
    } else {
        let _ = writeln!(
            out,
            "`heki check` reported {} diagnostic(s) in {} file(s){scope}:\n\n```text",
            d.total, d.files_checked
        );
        for shown in &d.shown {
            let _ = writeln!(out, "{}", shown.concise);
        }
        out.push_str("```\n");
    }
    if d.omitted > 0 {
        let _ = writeln!(
            out,
            "\n{} more diagnostic(s) are not shown: the prompt holds {} bytes of them. `heki \
             check --output-format concise` lists every one.",
            d.omitted, d.limit_bytes
        );
    }
    if let Some(note) = heuristic_hint(0, d.heuristic_reports_withheld) {
        let _ = writeln!(out, "\n{note}");
    }
    if !d.files_not_analyzed.is_empty() {
        out.push_str("\nheki could not analyze these files, so nothing they do is known:\n\n");
        for e in &d.files_not_analyzed {
            let _ = writeln!(out, "- {e}");
        }
    }
}
