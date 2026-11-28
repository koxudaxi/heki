//! What `--help` adds after the options (`-h` stays short), for a person or a coding agent:
//! where the documentation is (the command's reference first), the exit codes, examples, and
//! the output a program reads.

use heki_env::Docs;
use std::fmt::Write as _;

/// The part of one command's `--help` after its options.
pub(crate) struct LongHelp {
    /// The command's section of the CLI reference (`None` for `heki` itself).
    reference: Option<Docs>,
    exit_codes: &'static [(u8, &'static str)],
    examples: &'static [(&'static str, &'static str)],
    /// The commands whose output a program reads, and what it holds.
    machine_output: &'static [(&'static str, &'static str)],
}

/// The pages every command's `--help` links.
const LINKS: &[(&str, Docs)] = &[
    ("Documentation", Docs::Home),
    ("Getting started", Docs::GettingStarted),
    ("Configuration", Docs::Configuration),
    ("Rules", Docs::Rules),
    ("Coding agents", Docs::CodingAgents),
    ("llms.txt", Docs::LlmsTxt),
    ("Agent Skill", Docs::AgentSkill),
];

const ERROR_EXIT: (u8, &str) = (
    2,
    "a configuration, path, read, or syntax error; the error says what to do and links the documentation",
);

const CHECK_EXITS: &[(u8, &str)] = &[
    (0, "no diagnostic fails the run"),
    (
        1,
        "a diagnostic at the `fail-on` severity (an exact `error` by default; a heuristic one counts as a `warning`), a report of a contract with `fail = true`, a refused `--update-baseline`, or pending `--diff` changes",
    ),
    ERROR_EXIT,
];

const JSON_CHECK: (&str, &str) = (
    "heki check --output-format json",
    "the diagnostics; when the configuration or a path stops the run, {\"error\": {\"message\", \"help\", \"docs\", ...}}",
);

pub(crate) const HEKI: LongHelp = LongHelp {
    reference: None,
    exit_codes: CHECK_EXITS,
    examples: &[
        ("heki check", "check the project in the working directory"),
        (
            "heki check src tests --select EFF,MUT",
            "check two directories with two rule families",
        ),
        ("heki check --fix", "apply the fixes heki proves safe"),
        ("heki rule EFF101", "the documentation of one rule"),
        (
            "heki skill install --agent claude-code",
            "install the Agent Skill for Claude Code (experimental)",
        ),
    ],
    machine_output: &[
        JSON_CHECK,
        (
            "heki rule <CODE> --output-format json",
            "one rule's metadata",
        ),
        (
            "heki inspect <SYMBOL> --output-format json",
            "what heki knows about one function (experimental)",
        ),
    ],
};

pub(crate) const CHECK: LongHelp = LongHelp {
    reference: Some(Docs::CliCheck),
    exit_codes: CHECK_EXITS,
    examples: &[
        ("heki check src tests", "check two directories"),
        (
            "heki check app/api.py",
            "analyze the file's project, report only what touches the file",
        ),
        (
            "heki check --select EFF,MUT101 --ignore OPT110",
            "choose the rules for one run",
        ),
        (
            "heki check --touched app/users.py app/api.py",
            "analyze the project, report only what touches the two files (pre-commit, a pull request)",
        ),
        (
            "heki check --diff",
            "show the fixes heki proves safe; `--fix` applies them",
        ),
        (
            "heki check --baseline heki-baseline.toml --update-baseline",
            "record the current diagnostics; later runs report only new ones",
        ),
    ],
    machine_output: &[
        JSON_CHECK,
        (
            "heki check --output-format sarif",
            "SARIF 2.1.0, with each rule's page as `helpUri`",
        ),
        (
            "heki check --generate-prompt \"<question>\" --output-format json",
            "the project's diagnostics and configuration for a model (experimental)",
        ),
    ],
};

pub(crate) const SERVER: LongHelp = LongHelp {
    reference: Some(Docs::CliServer),
    exit_codes: &[
        (
            0,
            "the client ended the session with `shutdown`, then `exit`",
        ),
        (1, "the session ended without `shutdown`"),
    ],
    examples: &[("heki server", "serve LSP over stdin and stdout")],
    machine_output: &[],
};

pub(crate) const RULE: LongHelp = LongHelp {
    reference: Some(Docs::CliRule),
    exit_codes: &[
        (0, "the rule's documentation is printed"),
        (2, "no rule has that code or name"),
    ],
    examples: &[
        ("heki rule EFF101", "a rule by its code"),
        ("heki rule async-blocking-call", "a rule by its name"),
        ("heki rule --all", "every rule, with its code and name"),
    ],
    machine_output: &[(
        "heki rule --all --output-format json",
        "every rule's metadata",
    )],
};

pub(crate) const SNAPSHOT: LongHelp = LongHelp {
    reference: Some(Docs::CliSnapshot),
    exit_codes: &[(0, "the snapshot is printed or written"), ERROR_EXIT],
    examples: &[(
        "heki snapshot --write",
        "save the summaries `MUT103` compares with",
    )],
    machine_output: &[("heki snapshot", "the summaries as JSON")],
};

pub(crate) const ASSUMPTIONS: LongHelp = LongHelp {
    reference: Some(Docs::CliAssumptions),
    exit_codes: &[(0, "the hazards are listed"), ERROR_EXIT],
    examples: &[("heki assumptions src", "the hazards of one directory")],
    machine_output: &[("heki assumptions --json", "the hazards as a JSON array")],
};

pub(crate) const INSPECT: LongHelp = LongHelp {
    reference: Some(Docs::CliInspect),
    exit_codes: &[
        (0, "the report of each function is printed"),
        (
            2,
            "a symbol names no single function (the error lists the candidates), or a file could not be analyzed",
        ),
    ],
    examples: &[
        (
            "heki inspect pkg.users.fetch",
            "a function by its full name",
        ),
        (
            "heki inspect src/pkg/users.py:42 --callers",
            "the function at a line, with its callers",
        ),
        (
            "printf '%s\\n' pkg.users.fetch 'pkg.orders.*' | heki inspect -",
            "every function the symbols name, from one analysis",
        ),
    ],
    machine_output: &[
        (
            "heki inspect <SYMBOL> --output-format json",
            "the report as one object; on exit code 2, `error` with the candidates",
        ),
        (
            "heki inspect - --output-format json",
            "an array: the object of each function the symbols on standard input name, in their order",
        ),
    ],
};

pub(crate) const RECOMMEND: LongHelp = LongHelp {
    reference: Some(Docs::CliRecommend),
    exit_codes: &[(0, "the advice is printed"), ERROR_EXIT],
    examples: &[
        (
            "heki recommend",
            "advice for the project in the working directory",
        ),
        (
            "heki recommend --focus types",
            "every standard type the code already keeps, as suggestions",
        ),
    ],
    machine_output: &[
        (
            "heki recommend --output-format json",
            "the advice as one object",
        ),
        (
            "heki recommend --focus types --diff | git apply",
            "the exact suggestions as one unified diff of annotations",
        ),
        (
            "heki recommend --output-format-json-schema",
            "the JSON Schema of that object",
        ),
    ],
};

pub(crate) const VERSION: LongHelp = LongHelp {
    reference: Some(Docs::CliVersion),
    exit_codes: &[(0, "the version is printed")],
    examples: &[
        ("heki version", "the version of this heki"),
        (
            "heki version --changelog",
            "the version, then the changelog section of this release: the rules, keys, and options it adds or changes",
        ),
    ],
    machine_output: &[],
};

pub(crate) const SKILL_INSTALL: LongHelp = LongHelp {
    reference: Some(Docs::CliSkillInstall),
    exit_codes: &[
        (0, "the skill is installed"),
        (
            2,
            "the skill is already there (pass `--overwrite`), or its directory cannot be written",
        ),
    ],
    examples: &[
        (
            "heki skill install --agent claude-code",
            "into .claude/skills/heki of the working directory",
        ),
        (
            "heki skill install --agent codex --scope user",
            "into ~/.agents/skills/heki, for every project",
        ),
        (
            "heki skill install --agent claude-code --overwrite",
            "replace an installed skill with this heki's",
        ),
    ],
    machine_output: &[],
};

impl LongHelp {
    pub(crate) fn render(&self) -> String {
        let mut out = String::from("Documentation:\n");
        let reference = self.reference.map(|d| ("This command", d));
        for (name, docs) in reference.iter().chain(LINKS) {
            let _ = writeln!(out, "  {name:<16} {}", docs.url());
        }
        out.push_str("\nExit codes:\n");
        for (code, meaning) in self.exit_codes {
            let _ = writeln!(out, "  {code}  {meaning}");
        }
        out.push_str("\nExamples:\n");
        for (command, what) in self.examples {
            let _ = writeln!(out, "  {command}\n      {what}");
        }
        if !self.machine_output.is_empty() {
            out.push_str("\nOutput for programs:\n");
            for (command, what) in self.machine_output {
                let _ = writeln!(out, "  {command}\n      {what}");
            }
        }
        out.trim_end().to_owned()
    }
}
