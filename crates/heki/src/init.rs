//! `heki init` (experimental): sets heki up in the project `heki check` finds from the working
//! directory, from a few questions or the options: the configuration (`heki.toml`, or the
//! `[tool.heki]` table of `pyproject.toml`) with `target-version` from `requires-python` and the
//! keys `heki recommend` proposes commented out, a Ratchet baseline of what `heki check` reports
//! now, the Agent Skill, and a heki section in the agents' instructions. It changes no Python
//! file, writes nothing before every question is answered and every file it would replace may be
//! replaced, and replaces a heki file only with `--force`. Exit codes: 0 written (or shown with
//! `--dry-run`), 2 refused, or a file that cannot be read, analyzed, or written.

use crate::check::{
    finish, not_written, print_diff, selection, shown_dir, shown_errors, shown_path, shown_text,
    stdout_styles,
};
use crate::{ColorChoice, InitArgs, OutputFormat, SkillAgent, failure, on_off, skill};
use anyhow::Result;
use heki_diagnostics::{BaselineVersion, Diagnostic, RatchetBaseline, baseline_counts};
use heki_env::{ConfigLayout, Docs, HekiConfig, Problem, PythonVersion};
use heki_rules::project::{
    Facts, FileError, Indexed, Project, ProjectOptions, baseline_key, find_root,
};
use heki_rules::recommend::{Advice, Request, block::Block, recommend};
use heki_rules::{RuleSelection, Selectors};
use std::cell::Cell;
use std::fmt::Write as _;
use std::io::{BufRead as _, IsTerminal as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub(crate) const CONFIG_FILE: &str = "heki.toml";
pub(crate) const PYPROJECT: &str = "pyproject.toml";
const BASELINE_FILE: &str = "heki-baseline.toml";
const AGENTS_MD: &str = "AGENTS.md";
const CLAUDE_MD: &str = "CLAUDE.md";
/// Where GitHub reads the code owners from, in its order.
const CODEOWNERS: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

/// Test hook: the answers are read from the standard input as from a terminal, which a test
/// cannot open. Not a setting.
const TERMINAL_ENV: &str = "HEKI_INIT_TERMINAL";

/// The lines around the heki section of an instruction file, so that a run finds the one it
/// wrote and never writes a second.
const BEGIN: &str = "<!-- heki:begin -->";
const END: &str = "<!-- heki:end -->";
const SECTION: &str = "<!-- heki:begin -->
## heki

- After changing Python files, run `heki check --touched <the files you changed> --output-format json`,
  and before you finish, `heki check` on the whole project: a change can cause a report in a file
  you did not touch.
- For each diagnostic, read its `help` and `heki rule <rule_code>`, and fix the root cause its
  `related` entries lead to. A `heuristic` one can be wrong about the code: confirm it first.
- When what a function does is in question, `heki inspect <function> --callers` shows who calls
  it, what it changes, and the effects it reaches.
- A report that names a contract means the code crossed a wall the maintainers declared: change
  the code as its help says.
- Do not silence a report with `# heki: ignore` or `# noqa`, and do not edit the contracts or the
  baseline, unless the maintainer asks.
<!-- heki:end -->";

/// What goes above the keys `heki recommend` proposes.
const PROPOSED: &str = "\
# The keys `heki recommend` proposes, commented out. A contract candidate is an invariant the
# code keeps today, not a decision: keep the walls you intend to keep, give each a `message` and
# `fail = true`, and remove the `#` of its lines. `heki recommend` shows the evidence of each.
";

pub(crate) fn run(args: &InitArgs) -> Result<ExitCode> {
    let plan = plan(args)?;
    write(&plan, args.dry_run)?;
    summarize(&plan, args.dry_run);
    finish(OutputFormat::Full, &plan.problems, ExitCode::SUCCESS)
}

/// What `heki init` writes, decided before it writes anything.
struct Plan {
    root: PathBuf,
    /// The configuration file, relative to the root.
    file: &'static str,
    /// The heki configurations the project has now, which `--force` lets it replace.
    existing: Vec<&'static str>,
    /// The configuration file, then the instruction files that take the heki section.
    texts: Vec<TextFile>,
    baseline: Option<NewBaseline>,
    /// Where the skill is installed.
    skills: Vec<PathBuf>,
    advice: Option<Advice>,
    /// The instruction files that keep the section they have.
    kept: Vec<PathBuf>,
    /// What makes the run exit with 2 though it writes the rest: files that cannot be analyzed.
    problems: Vec<Problem>,
}

/// The answers, the analysis, and the files to write, in the order of the questions; every file
/// that would be replaced is checked here.
fn plan(args: &InitArgs) -> Result<Plan> {
    let root = find_root(Path::new("."), &std::env::current_dir()?);
    let asker = Asker::new(args.yes, &root);
    let pyproject = PyProject::read(&root)?;
    let existing = configurations(&root, pyproject.as_ref());
    if let Some(first) = existing.first().filter(|_| !args.force) {
        return Err(Problem::new("the project has a heki configuration already")
            .at_file(&shown_path(&root.join(first)))
            .help("pass `--force` to replace it with the one `heki init` writes, or add the keys `heki recommend` lists to it by hand")
            .docs(Docs::Init)
            .into());
    }
    let file = config_file(args, &asker, &root, pyproject.as_ref(), &existing)?;
    let config_path = root.join(file);
    let run_recommend = match on_off(args.recommend, args.no_recommend) {
        Some(given) => given,
        None => asker.ask(
            &format!(
                "Run heki recommend and add the keys it proposes to {}, commented out?",
                shown_text(&config_path)
            ),
            true,
            "`--recommend` or `--no-recommend`",
        )?,
    };
    let before = match (&pyproject, file) {
        (Some(pyproject), PYPROJECT) => pyproject.without_settings(),
        _ => String::new(),
    };
    // `requires-python` as the loader reads it for a configuration file at the root.
    let target_version = ConfigLayout::TopLevel
        .parse(&root.join(CONFIG_FILE), "", &root)?
        .requires_python;
    let base = Config {
        layout: ConfigLayout::of(&config_path),
        before: &before,
        target_version,
        baseline: false,
        proposed: None,
    };
    let baseline_choice = on_off(args.baseline, args.no_baseline);
    let analysis = if run_recommend || baseline_choice != Some(false) {
        Some(analyze(&root, &config_path, &base.text())?)
    } else {
        None
    };
    let mut problems = Vec::new();
    let baseline = match (&analysis, baseline_choice) {
        (_, Some(false)) | (None, _) => None,
        (Some(analysis), choice) => baseline(&root, analysis, choice, &asker, &mut problems)?,
    };
    if let Some(baseline) = &baseline {
        refuse_replacing(&baseline.path, args.force)?;
    }
    let (agents, wants_section) = agents(args, &asker, &root)?;
    let mut skills = Vec::new();
    for agent in agents {
        skills.push(skill_destination(&root, agent, args.force)?);
    }
    let mut texts = Vec::new();
    let mut kept = Vec::new();
    if wants_section {
        sections(&root, args.force, &mut texts, &mut kept)?;
    }
    let advice = match analysis {
        Some(analysis) => {
            problems.extend(failure::of_files(&analysis.project.errors));
            if run_recommend {
                Some(advise(&root, analysis)?)
            } else {
                None
            }
        }
        None => None,
    };
    let text = Config {
        baseline: baseline.is_some(),
        proposed: advice.as_ref().map(|a| &a.block),
        ..base
    }
    .text();
    // What it writes is what heki will read.
    ConfigLayout::of(&config_path).parse(&config_path, &text, &root)?;
    let old = match &pyproject {
        Some(pyproject) if file == PYPROJECT => Some(pyproject.text.clone()),
        _ => std::fs::read_to_string(&config_path).ok(),
    };
    texts.insert(
        0,
        TextFile {
            path: config_path,
            old,
            new: text,
        },
    );
    Ok(Plan {
        root,
        file,
        existing,
        texts,
        baseline,
        skills,
        advice,
        kept,
        problems,
    })
}

/// The baseline of the reports of `analysis`, when it has some: as `choice` says, else the
/// answer to the third question. Files that cannot be analyzed leave the baseline out, with a
/// problem, as `heki check --update-baseline` does.
fn baseline(
    root: &Path,
    analysis: &Analysis,
    choice: Option<bool>,
    asker: &Asker,
    problems: &mut Vec<Problem>,
) -> Result<Option<NewBaseline>> {
    let path = root.join(BASELINE_FILE);
    let reports = analysis.diags.len();
    if reports == 0 {
        return Ok(None);
    }
    let errors = analysis.project.errors.len();
    if errors > 0 {
        problems.push(not_written("baseline", &shown_path(&path), errors));
        return Ok(None);
    }
    let wanted = match choice {
        Some(given) => given,
        None => asker.ask(
            &format!(
                "heki check reports {reports} diagnostic(s) now: record them in a Ratchet baseline, {}?",
                shown_text(&path)
            ),
            true,
            "`--baseline` or `--no-baseline`",
        )?,
    };
    Ok(wanted.then(|| new_baseline(path, &analysis.diags, &analysis.project)))
}

/// Writes the files of `plan`, or with `--dry-run` prints the text ones as diffs.
fn write(plan: &Plan, dry_run: bool) -> Result<()> {
    if dry_run {
        let styles = stdout_styles(ColorChoice::Auto);
        for f in &plan.texts {
            let old = f.old.as_deref().unwrap_or_default();
            print_diff(&shown_text(&f.path), old, &f.new, &styles)?;
        }
        return Ok(());
    }
    // The baseline goes first: the configuration names it, and `heki check` fails on a named
    // baseline it cannot load.
    if let Some(baseline) = &plan.baseline {
        write_text(&baseline.path, &baseline.text)?;
    }
    for f in &plan.texts {
        write_text(&f.path, &f.new)?;
    }
    for destination in &plan.skills {
        skill::write(destination).map_err(|e| {
            skill::unwritable(&shown_path(destination), &e, "make the directory writable")
        })?;
    }
    Ok(())
}

/// What was written (or would be), what the project should do next, on stderr.
fn summarize(plan: &Plan, dry_run: bool) {
    let verbs = Verbs::of(dry_run);
    let mut lines = Vec::new();
    for f in &plan.texts {
        let verb = verbs.file(f.old.is_some());
        lines.push(format!("{verb} {}", shown_text(&f.path)));
    }
    if let Some(baseline) = &plan.baseline {
        lines.insert(
            1,
            format!(
                "{} {} ({} entries, {} diagnostic(s))",
                verbs.file(baseline.replaces),
                shown_text(&baseline.path),
                baseline.entries,
                baseline.reports
            ),
        );
    }
    for destination in &plan.skills {
        lines.push(format!(
            "{} the heki Agent Skill (experimental) at {}",
            verbs.install,
            shown_text(destination)
        ));
    }
    for path in &plan.kept {
        lines.push(format!(
            "{} has the heki section already (`--force` writes it again)",
            shown_text(path)
        ));
    }
    let shown_config = shown_text(&plan.root.join(plan.file));
    for other in plan.existing.iter().filter(|name| **name != plan.file) {
        let shown = shown_text(&plan.root.join(other));
        let other = if *other == PYPROJECT {
            format!("the [tool.heki] table of {shown}")
        } else {
            shown
        };
        lines.push(format!(
            "note: heki reads {shown_config} first, so {other} is no longer read: remove it"
        ));
    }
    if let Some(advice) = &plan.advice {
        lines.extend(advice_lines(&shown_config, advice));
    }
    let mut owned = vec![plan.file];
    if plan.baseline.is_some() {
        owned.push(BASELINE_FILE);
    }
    lines.push(codeowners_line(&plan.root, &owned));
    lines.push("Next: `heki check`, and `heki recommend` for what else fits the project.".into());
    for line in lines {
        eprintln!("{line}");
    }
}

/// How the summary says what was done, or with `--dry-run` what would be.
struct Verbs {
    create: &'static str,
    update: &'static str,
    install: &'static str,
}

impl Verbs {
    fn of(dry_run: bool) -> Self {
        if dry_run {
            Self {
                create: "Would create",
                update: "Would update",
                install: "Would install",
            }
        } else {
            Self {
                create: "Created",
                update: "Updated",
                install: "Installed",
            }
        }
    }

    /// The verb of a file written, which `replaces` one there.
    fn file(&self, replaces: bool) -> &'static str {
        if replaces { self.update } else { self.create }
    }
}

/// A text file `heki init` writes: `old` is its text now, `None` for a new file.
struct TextFile {
    path: PathBuf,
    old: Option<String>,
    new: String,
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|e| failure::unwritable(&shown_path(path), &e))?;
    Ok(())
}

fn read_text(path: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(path).map_err(|e| FileError::unreadable(path, &e))?)
}

/// `path` may be written only with `--force` when it is there already.
fn refuse_replacing(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        return Err(Problem::new("the file is there already")
            .at_file(&shown_path(path))
            .help("pass `--force` to replace it")
            .docs(Docs::Init)
            .into());
    }
    Ok(())
}

/// Asks the questions the options leave open: on stderr, the answers read from stdin, or with
/// `--yes` the default answers.
struct Asker {
    yes: bool,
    terminal: bool,
    /// What the first question is preceded by, until it is asked.
    intro: Cell<Option<String>>,
}

impl Asker {
    fn new(yes: bool, root: &Path) -> Self {
        Self {
            yes,
            terminal: std::io::stdin().is_terminal() || std::env::var_os(TERMINAL_ENV).is_some(),
            intro: Cell::new(Some(format!(
                "heki init (experimental) in {}: Enter takes the answer in capitals.",
                shown_dir(root)
            ))),
        }
    }

    /// The answer to `question`, whose default is `default` and whose answer `options` give
    /// on the command line. A run whose standard input is not a terminal never waits for one.
    fn ask(&self, question: &str, default: bool, options: &str) -> Result<bool> {
        if self.yes {
            return Ok(default);
        }
        if !self.terminal {
            return Err(Problem::new(format!(
                "`heki init` needs an answer, and the standard input is not a terminal: {question}"
            ))
            .help(format!(
                "pass `--yes` to take the default answers, or answer this question with {options}"
            ))
            .docs(Docs::Init)
            .into());
        }
        if let Some(intro) = self.intro.take() {
            eprintln!("{intro}");
        }
        let choices = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            eprint!("{question} {choices} ");
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line)? == 0 {
                return Err(
                    Problem::new("the input ended before an answer: nothing was written")
                        .help("answer each question, or pass `--yes` to take the default answers")
                        .docs(Docs::Init)
                        .into(),
                );
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => eprintln!("Answer y or n."),
            }
        }
    }
}

/// The project's `pyproject.toml`, as written and as a document the settings table can be taken
/// out of.
struct PyProject {
    text: String,
    doc: toml_edit::DocumentMut,
}

impl PyProject {
    fn read(root: &Path) -> Result<Option<Self>> {
        let path = root.join(PYPROJECT);
        if !path.is_file() {
            return Ok(None);
        }
        let text = read_text(&path)?;
        let doc = text.parse().map_err(|e: toml_edit::TomlError| {
            Problem::new(format!(
                "cannot read the TOML: {}",
                e.message().trim().replace('\n', "; ")
            ))
            .at_file(&shown_path(&path))
            .docs(Docs::ConfigFiles)
        })?;
        Ok(Some(Self { text, doc }))
    }

    fn has_settings(&self) -> bool {
        (self.doc.get("tool")).is_some_and(|tool| tool.get("heki").is_some())
    }

    /// The text without the settings table.
    fn without_settings(&self) -> String {
        if !self.has_settings() {
            return self.text.clone();
        }
        let mut doc = self.doc.clone();
        (doc["tool"].as_table_like_mut())
            .expect("`tool` holds the settings")
            .remove("heki");
        doc.to_string()
    }
}

/// The heki configurations of the project root, in the order heki looks for them.
fn configurations(root: &Path, pyproject: Option<&PyProject>) -> Vec<&'static str> {
    HekiConfig::FILES
        .into_iter()
        .filter(|name| match *name {
            PYPROJECT => pyproject.is_some_and(PyProject::has_settings),
            name => root.join(name).is_file(),
        })
        .collect()
}

/// The file the configuration goes into: `--config`, else the answer to the first question,
/// asked when the project has a `pyproject.toml`, whose default is that file when its
/// `[tool.heki]` table holds the configuration now.
fn config_file(
    args: &InitArgs,
    asker: &Asker,
    root: &Path,
    pyproject: Option<&PyProject>,
    existing: &[&str],
) -> Result<&'static str> {
    let file = match (args.config.as_deref(), pyproject) {
        (Some(PYPROJECT), _) => PYPROJECT,
        (Some(_), _) | (None, None) => CONFIG_FILE,
        (None, Some(_)) => {
            let heki_toml = asker.ask(
                &format!(
                    "Write the configuration to {CONFIG_FILE} (n: the [tool.heki] table of {PYPROJECT})?"
                ),
                existing != [PYPROJECT],
                "`--config`",
            )?;
            if heki_toml { CONFIG_FILE } else { PYPROJECT }
        }
    };
    if let Some(first) = existing
        .iter()
        .find(|name| file == PYPROJECT && **name != PYPROJECT)
    {
        return Err(Problem::new(format!(
            "heki reads {first} before {PYPROJECT}, so a [tool.heki] table would not be read"
        ))
        .at_file(&shown_path(&root.join(first)))
        .help(format!(
            "write the configuration to {CONFIG_FILE} (`--config {CONFIG_FILE}`), or remove {first} first"
        ))
        .docs(Docs::ConfigFiles)
        .into());
    }
    Ok(file)
}

/// The configuration `heki init` writes.
struct Config<'a> {
    layout: ConfigLayout,
    /// What the file holds before the settings: the rest of a `pyproject.toml`.
    before: &'a str,
    target_version: Option<PythonVersion>,
    baseline: bool,
    /// The keys `heki recommend` proposes, written commented out.
    proposed: Option<&'a Block>,
}

impl Config<'_> {
    fn text(&self) -> String {
        let mut out = self.before.trim_end().to_owned();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let _ = writeln!(out, "# heki settings: {}", Docs::Configuration.url());
        if let Some(prefix) = self.layout.table_prefix() {
            let _ = writeln!(out, "[{prefix}]");
        }
        if let Some(version) = self.target_version {
            let _ = writeln!(
                out,
                "target-version = \"{}\"  # the oldest Python that `requires-python` allows",
                version.tag()
            );
        }
        if self.baseline {
            let _ = writeln!(out, "baseline = \"{BASELINE_FILE}\"");
        }
        if let Some(block) = self.proposed.filter(|b| !b.toml.is_empty()) {
            // The keys of the settings table itself go below this file's header of it.
            let parts: Vec<String> = [&block.settings, &block.tables]
                .into_iter()
                .filter(|part| !part.is_empty())
                .map(|part| commented(part))
                .collect();
            let _ = write!(out, "\n{PROPOSED}{}", parts.join("\n"));
        }
        out
    }
}

/// `toml` with each line commented out.
fn commented(toml: &str) -> String {
    toml.lines()
        .map(|line| format!("{}\n", format!("# {line}").trim_end()))
        .collect()
}

/// The project, the rule selection, and the reports of `heki check` with the configuration
/// `text` in the file `config`.
struct Analysis {
    project: Project,
    selection: RuleSelection,
    diags: Vec<Diagnostic>,
}

fn analyze(root: &Path, config: &Path, text: &str) -> Result<Analysis> {
    let paths = [root.to_path_buf()];
    let project = Project::load(&ProjectOptions {
        paths: &paths,
        config: Some(config),
        config_text: Some(text),
        ..ProjectOptions::default()
    })?;
    let selection = selection(&Selectors::default(), &project)?;
    let indexed = Indexed::build(project, selection, None, Facts::Check)?;
    let (diags, _) = indexed.diagnose()?;
    let Indexed {
        project,
        index,
        selection,
        ..
    } = indexed;
    drop(index);
    Ok(Analysis {
        project,
        selection,
        diags,
    })
}

/// A Ratchet baseline of every report, as `heki check --update-baseline` writes a new one.
struct NewBaseline {
    path: PathBuf,
    /// Whether a file is there now, which `--force` replaces.
    replaces: bool,
    text: String,
    entries: usize,
    reports: usize,
}

fn new_baseline(path: PathBuf, diags: &[Diagnostic], project: &Project) -> NewBaseline {
    let counts = baseline_counts(diags, &baseline_key(&project.files), BaselineVersion::V3);
    let baseline = RatchetBaseline::from_counts(counts);
    NewBaseline {
        replaces: path.exists(),
        path,
        text: baseline.to_toml_string(),
        entries: baseline.counts.len(),
        reports: diags.len(),
    }
}

/// The agents to install the skill for, and whether the instruction files get the heki section:
/// as `--skill` and `--agents-md` say, else from the answers.
fn agents(args: &InitArgs, asker: &Asker, root: &Path) -> Result<(Vec<SkillAgent>, bool)> {
    if !args.skill.is_empty() || args.agents_md {
        let mut skills = Vec::new();
        for agent in &args.skill {
            if !skills.contains(agent) {
                skills.push(*agent);
            }
        }
        return Ok((skills, args.agents_md));
    }
    let options = "`--skill` and `--agents-md`";
    if !asker.ask(
        "Set up coding agents (the Agent Skill, AGENTS.md)?",
        false,
        options,
    )? {
        return Ok((Vec::new(), false));
    }
    let mut skills = Vec::new();
    for (agent, name, reads) in [
        (
            SkillAgent::ClaudeCode,
            "Claude Code",
            [".claude", CLAUDE_MD],
        ),
        (SkillAgent::Codex, "Codex", [".agents", AGENTS_MD]),
    ] {
        let destination = shown_text(&skill::below(root, agent));
        let detected = reads.iter().any(|name| root.join(name).exists());
        let question = format!("Install the heki Agent Skill for {name} in {destination}?");
        if asker.ask(&question, detected, options)? {
            skills.push(agent);
        }
    }
    let files: Vec<&str> = (instruction_files(root)?.into_iter())
        .map(|(name, _)| name)
        .collect();
    let question = match files.as_slice() {
        [] => format!("Create {AGENTS_MD} with a heki section?"),
        files => format!("Add a heki section to {}?", files.join(" and ")),
    };
    Ok((skills, asker.ask(&question, !files.is_empty(), options)?))
}

/// Where the skill for `agent` goes, which only `--force` replaces.
fn skill_destination(root: &Path, agent: SkillAgent, force: bool) -> Result<PathBuf> {
    let destination = skill::below(root, agent);
    let shown = shown_path(&destination);
    match skill::place(&destination) {
        skill::Place::Taken => Err(skill::taken(&shown).help("remove it").into()),
        skill::Place::Installed if !force => Err(skill::installed(&shown)
            .help("pass `--force` to replace it with the skill of this heki")
            .into()),
        skill::Place::Installed | skill::Place::Free => Ok(destination),
    }
}

/// The agents' instruction files of the project that take the heki section, with their text:
/// `AGENTS.md` and `CLAUDE.md`, without a `CLAUDE.md` that imports `AGENTS.md` (an `@AGENTS.md`
/// line), whose agent reads the section there.
fn instruction_files(root: &Path) -> Result<Vec<(&'static str, String)>> {
    let mut files = Vec::new();
    for name in [AGENTS_MD, CLAUDE_MD] {
        let path = root.join(name);
        if path.is_file() {
            files.push((name, read_text(&path)?));
        }
    }
    files.retain(|(name, text)| {
        *name != CLAUDE_MD || !text.lines().any(|line| line.trim() == "@AGENTS.md")
    });
    Ok(files)
}

/// The instruction files with the heki section, into `texts`; a file that has it already is
/// left as it is unless `force`, into `kept`. Without such files, a new `AGENTS.md`.
fn sections(
    root: &Path,
    force: bool,
    texts: &mut Vec<TextFile>,
    kept: &mut Vec<PathBuf>,
) -> Result<()> {
    let found = instruction_files(root)?;
    let files: Vec<(&str, Option<String>)> = if found.is_empty() {
        vec![(AGENTS_MD, None)]
    } else {
        found
            .into_iter()
            .map(|(name, text)| (name, Some(text)))
            .collect()
    };
    for (name, old) in files {
        let path = root.join(name);
        let shown = shown_path(&path);
        let text = old.as_deref().unwrap_or_default();
        match with_section(text, force, &shown)? {
            Some(new) if new != text => texts.push(TextFile { path, old, new }),
            _ => kept.push(path),
        }
    }
    Ok(())
}

/// `text` with the heki section: appended, or with `force` in place of the one it has. `None`:
/// it has the section, and it stays.
fn with_section(text: &str, force: bool, shown: &Path) -> Result<Option<String>> {
    let Some(start) = text.find(BEGIN) else {
        let text = text.trim_end();
        let gap = if text.is_empty() { "" } else { "\n\n" };
        return Ok(Some(format!("{text}{gap}{SECTION}\n")));
    };
    if !force {
        return Ok(None);
    }
    let Some(length) = text[start..].find(END) else {
        return Err(Problem::new(format!(
            "the heki section has no line `{END}` after `{BEGIN}`"
        ))
        .at_file(shown)
        .help(format!(
            "add `{END}` where the section ends, or remove the section"
        ))
        .docs(Docs::Init)
        .into());
    };
    let end = start + length + END.len();
    Ok(Some(format!("{}{SECTION}{}", &text[..start], &text[end..])))
}

/// `heki recommend` on the analyzed project, as it runs with the configuration `heki init`
/// writes.
fn advise(root: &Path, analysis: Analysis) -> Result<Advice> {
    let paths = [root.to_path_buf()];
    let errors = shown_errors(&analysis.project.errors);
    Ok(recommend(Request {
        project: analysis.project,
        root: shown_dir(root),
        current: analysis.selection,
        snapshot: None,
        baseline: None,
        paths: &paths,
        analyzed_paths: &paths,
        check_options: Vec::new(),
        shown: &shown_text,
        errors,
        all: false,
        focus: None,
        diff: false,
    })?)
}

/// What the summary says of the advice: where its keys are, and the exact type suggestions,
/// which only `heki recommend` applies.
fn advice_lines(shown_config: &str, advice: &Advice) -> Vec<String> {
    let mut lines = vec![if advice.block.toml.is_empty() {
        "heki recommend proposed no keys.".to_owned()
    } else {
        format!(
            "{shown_config} holds the keys heki recommend proposes, commented out: review them \
             (`heki recommend` shows the evidence of each) and remove the `#` of those you keep."
        )
    }];
    let (exact, _) = advice.report.suggestions.totals();
    if exact > 0 {
        lines.push(format!(
            "heki recommend found {exact} exact type suggestion group(s): `heki recommend --focus \
             types --diff | git apply` adds the annotations (heki init changes no Python file)."
        ));
    }
    lines
}

/// The line on giving the heki files `owned` (relative to the root) an owner: the code owners
/// file of the project when it has one and does not name them yet.
fn codeowners_line(root: &Path, owned: &[&str]) -> String {
    let reason = "so that a maintainer reviews every change to the walls";
    let Some(path) = CODEOWNERS
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())
    else {
        return format!(
            "Give {} an owner in CODEOWNERS (.github/CODEOWNERS), {reason}.",
            owned.join(" and ")
        );
    };
    let named = std::fs::read_to_string(&path).unwrap_or_default();
    let missing: Vec<&str> = (owned.iter().copied())
        .filter(|file| !named.contains(file))
        .collect();
    if missing.is_empty() {
        return format!(
            "{} gives {} an owner.",
            shown_text(&path),
            owned.join(" and ")
        );
    }
    format!(
        "Add {} to {}, {reason}.",
        missing.join(" and "),
        shown_text(&path)
    )
}
