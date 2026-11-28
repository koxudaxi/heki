//! `heki init` through the binary: the files it writes from the options and from the answers, the
//! files it leaves, and that `heki check` reads what it wrote.

mod common;

use common::{Project, fresh_dir, heki_command, repo_file, repo_root, wait_or_hang};
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::Stdio;

/// An exact `EFF101` error: `heki check` fails on it.
const BLOCKING: &str = "import time\n\nasync def handler():\n    time.sleep(1)\n";
const DOCS: &str =
    "https://heki.koxudaxi.dev/getting-started/#set-up-a-repository-heki-init-experimental";
const HEADER: &str = "# heki settings: https://heki.koxudaxi.dev/configuration/\n";
const NEXT: &str = "Next: `heki check`, and `heki recommend` for what else fits the project.\n";
/// The last line of the comment above the keys `heki recommend` proposes.
const PROPOSED_END: &str = "`heki recommend` shows the evidence of each.\n";
const BEGIN: &str = "<!-- heki:begin -->";
const END: &str = "<!-- heki:end -->";

/// Every file below `dir` with its bytes, by its path relative to `dir`.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = format!("{rel}{}", path.file_name().unwrap().to_str().unwrap());
            if path.is_dir() {
                walk(&path, &format!("{rel}/"), out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, "", &mut out);
    out
}

/// `heki init <args>` in `p`, with `answers` on the standard input, read as from a terminal.
fn answered(p: &Project, args: &[&str], answers: &str) -> (i32, String, String) {
    let mut child = heki_command()
        .arg("init")
        .args(args)
        .current_dir(&p.dir)
        .env("HEKI_INIT_TERMINAL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Closed once written: a question past the answers reads the end of the input.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// The project of `tests/sample_service`, on which `heki recommend` proposes contracts and an
/// exact type suggestion.
fn sample_service(name: &str) -> Project {
    let p = Project::new(name);
    let source = repo_root().join("tests/sample_service");
    for (rel, bytes) in files(&source) {
        p.write(&rel, &String::from_utf8(bytes).unwrap());
    }
    p
}

#[test]
fn yes_writes_the_configuration_and_a_baseline_of_the_reports_now() {
    let p = Project::new("init_yes");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\nrequires-python = \">=3.12\"\n",
    );
    p.write("app.py", BLOCKING);
    assert_eq!(p.heki(&["check"]).0, 1);
    let (code, stdout, stderr) = p.heki(&["init", "--yes", "--no-recommend"]);
    assert_eq!((code, stdout.as_str()), (0, ""), "{stderr}");
    assert_eq!(
        p.read("heki.toml"),
        format!(
            "{HEADER}target-version = \"py312\"  # the oldest Python that `requires-python` allows\nbaseline = \"heki-baseline.toml\"\n"
        )
    );
    assert_eq!(
        stderr,
        format!(
            "Created heki.toml\nCreated heki-baseline.toml (1 entries, 1 diagnostic(s))\nGive heki.toml and heki-baseline.toml an owner in CODEOWNERS (.github/CODEOWNERS), so that a maintainer reviews every change to the walls.\n{NEXT}"
        )
    );
    // The baseline is the one `heki check --update-baseline` writes, and the reports it records
    // no longer fail the run.
    let baseline = p.read("heki-baseline.toml");
    std::fs::remove_file(p.dir.join("heki-baseline.toml")).unwrap();
    assert_eq!(p.heki(&["check", "--update-baseline"]).0, 0);
    assert_eq!(p.read("heki-baseline.toml"), baseline);
    let (code, _, stderr) = p.heki(&["check"]);
    assert_eq!(code, 0, "{stderr}");

    // A second run finds the configuration and changes nothing.
    let before = files(&p.dir);
    let (code, _, stderr) = p.heki(&["init", "--yes"]);
    assert_eq!(code, 2);
    assert_eq!(
        stderr,
        format!(
            "error: heki.toml: the project has a heki configuration already\n  = help: pass `--force` to replace it with the one `heki init` writes, or add the keys `heki recommend` lists to it by hand\n  = docs: {DOCS}\n"
        )
    );
    assert_eq!(files(&p.dir), before);
}

#[test]
fn the_proposed_keys_are_commented_out_and_hold_once_uncommented() {
    let p = sample_service("init_recommend");
    let before = p.heki(&["check"]);
    let (code, _, stderr) = p.heki(&["init", "--yes"]);
    assert_eq!(code, 0, "{stderr}");
    let text = p.read("heki.toml");
    assert!(
        text.starts_with(&format!(
            "{HEADER}target-version = \"py311\"  # the oldest Python that `requires-python` allows\n\n# The keys `heki recommend` proposes, commented out."
        )),
        "{text}"
    );
    assert!(
        text.contains(
            "\n# [[write-owners]]\n# name = \"Order.* writers\"\n# targets = [\"app.models:Order.*\"]\n"
        ),
        "{text}"
    );
    for line in [
        "heki.toml holds the keys heki recommend proposes, commented out: review them (`heki recommend` shows the evidence of each) and remove the `#` of those you keep.\n",
        "heki recommend found 1 exact type suggestion group(s): `heki recommend --focus types --diff | git apply` adds the annotations (heki init changes no Python file).\n",
    ] {
        assert!(stderr.contains(line), "{line}: {stderr}");
    }
    // `heki check` reports what it reported before.
    assert_eq!(p.heki(&["check"]), before);
    // Uncommented, the keys are contracts the code keeps.
    let (head, proposed) = text.split_once(PROPOSED_END).unwrap();
    let uncommented: String = proposed
        .lines()
        .map(|line| format!("{}\n", line.strip_prefix("# ").unwrap_or(line)))
        .collect();
    p.write("heki.toml", &format!("{head}{PROPOSED_END}{uncommented}"));
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!((code, stdout.as_str()), (0, ""), "{stderr}");
    assert!(stderr.starts_with("All checks passed"), "{stderr}");
}

#[test]
fn a_dry_run_writes_nothing() {
    let p = Project::new("init_dry_run");
    p.write("app.py", BLOCKING);
    p.write("AGENTS.md", "# Agents\n");
    p.write(".github/CODEOWNERS", "* @team\n");
    let before = files(&p.dir);
    let (code, stdout, stderr) = p.heki(&[
        "init",
        "--yes",
        "--no-recommend",
        "--dry-run",
        "--skill",
        "codex",
        "--agents-md",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(files(&p.dir), before);
    assert!(
        stdout.starts_with(&format!(
            "--- a/heki.toml\n+++ b/heki.toml\n@@ -0,0 +1,2 @@\n+{HEADER}"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains("--- a/AGENTS.md\n+++ b/AGENTS.md\n")
            && stdout.contains(&format!(" # Agents\n+\n+{BEGIN}\n")),
        "{stdout}"
    );
    assert_eq!(
        stderr,
        format!(
            "Would create heki.toml\nWould create heki-baseline.toml (1 entries, 1 diagnostic(s))\nWould update AGENTS.md\nWould install the heki Agent Skill (experimental) at .agents/skills/heki\nAdd heki.toml and heki-baseline.toml to .github/CODEOWNERS, so that a maintainer reviews every change to the walls.\n{NEXT}"
        )
    );
}

#[test]
fn an_existing_configuration_is_replaced_only_with_force() {
    let p = Project::new("init_existing");
    p.write("heki.toml", "[lint]\nselect = [\"EFF\"]\n");
    p.write(".heki.toml", "[lint]\nselect = [\"MUT\"]\n");
    let before = files(&p.dir);
    let (code, _, stderr) = p.heki(&["init", "--yes"]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("error: heki.toml: the project has a heki configuration already\n"),
        "{stderr}"
    );
    // A `[tool.heki]` table would be read after `heki.toml`: refused even with `--force`.
    let (code, _, stderr) = p.heki(&["init", "--yes", "--force", "--config", "pyproject.toml"]);
    assert_eq!(code, 2);
    assert_eq!(
        stderr,
        "error: heki.toml: heki reads heki.toml before pyproject.toml, so a [tool.heki] table would not be read\n  = help: write the configuration to heki.toml (`--config heki.toml`), or remove heki.toml first\n  = docs: https://heki.koxudaxi.dev/configuration/#where-heki-finds-the-configuration\n"
    );
    assert_eq!(files(&p.dir), before);
    let (code, _, stderr) = p.heki(&["init", "--yes", "--force", "--no-recommend"]);
    assert_eq!(code, 0, "{stderr}");
    // Without `requires-python`, the default target version stays the default.
    assert_eq!(p.read("heki.toml"), HEADER);
    assert!(
        stderr.starts_with(
            "Updated heki.toml\nnote: heki reads heki.toml first, so .heki.toml is no longer read: remove it\n"
        ),
        "{stderr}"
    );
}

#[test]
fn the_pyproject_table_is_added_or_written_in_place_of_the_old_one() {
    // Added after the rest of the file.
    let p = Project::new("init_pyproject_add");
    let (code, _, stderr) = p.heki(&[
        "init",
        "--yes",
        "--no-recommend",
        "--config",
        "pyproject.toml",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        p.read("pyproject.toml"),
        format!("[project]\nname = \"t\"\n\n{HEADER}[tool.heki]\n")
    );
    assert!(stderr.starts_with("Updated pyproject.toml\n"), "{stderr}");
    assert_eq!(p.heki(&["check"]).0, 0);

    let p = Project::new("init_pyproject");
    let text = "# The project.\n[project]\nname = \"t\"\nrequires-python = \">=3.13\"\n\n[tool.heki]\nbaseline = \"old.toml\"\n\n[tool.heki.lint]\nextend-select = [\"EFF105\"]\n\n[tool.ruff]\nline-length = 100  # kept\n";
    p.write("pyproject.toml", text);
    let (code, _, stderr) = p.heki(&["init", "--yes"]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("error: pyproject.toml: the project has a heki configuration already\n"),
        "{stderr}"
    );
    assert_eq!(p.read("pyproject.toml"), text);
    // The default answer keeps the configuration in the file that holds it.
    let (code, _, stderr) = p.heki(&["init", "--yes", "--force", "--no-recommend"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        p.read("pyproject.toml"),
        format!(
            "# The project.\n[project]\nname = \"t\"\nrequires-python = \">=3.13\"\n\n[tool.ruff]\nline-length = 100  # kept\n\n{HEADER}[tool.heki]\ntarget-version = \"py313\"  # the oldest Python that `requires-python` allows\n"
        )
    );
    assert!(!p.dir.join("heki.toml").exists());
    // `heki.toml` is read first: the table stays, and is no longer read.
    let (code, _, stderr) = p.heki(&[
        "init",
        "--yes",
        "--force",
        "--no-recommend",
        "--config",
        "heki.toml",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.starts_with("Created heki.toml\nnote: heki reads heki.toml first, so the [tool.heki] table of pyproject.toml is no longer read: remove it\n"),
        "{stderr}"
    );
}

#[test]
fn a_question_left_open_without_a_terminal_fails_at_once() {
    let p = Project::new("init_no_terminal");
    // A pipe nobody writes to: a read would wait for ever.
    let mut child = heki_command()
        .arg("init")
        .current_dir(&p.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let status = wait_or_hang(&mut child, "heki init");
    drop(stdin);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "error: `heki init` needs an answer, and the standard input is not a terminal: Write the configuration to heki.toml (n: the [tool.heki] table of pyproject.toml)?\n  = help: pass `--yes` to take the default answers, or answer this question with `--config`\n  = docs: {DOCS}\n"
        )
    );
    assert_eq!(files(&p.dir).len(), 1);
    // Options that answer every question need no terminal.
    let (code, _, stderr) = p.heki(&[
        "init",
        "--config",
        "heki.toml",
        "--no-recommend",
        "--no-baseline",
        "--skill",
        "codex",
    ]);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn the_baseline_question_is_asked_only_when_heki_check_reports_something() {
    let p = Project::new("init_no_reports");
    p.write("app.py", "def f():\n    return 1\n");
    let (code, _, stderr) = answered(&p, &["--no-recommend"], "\n\n");
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.starts_with("heki init (experimental) in .: Enter takes the answer in capitals.\nWrite the configuration to heki.toml (n: the [tool.heki] table of pyproject.toml)? [Y/n] Set up coding agents (the Agent Skill, AGENTS.md)? [y/N] Created heki.toml\n"),
        "{stderr}"
    );
    assert!(!p.dir.join("heki-baseline.toml").exists());
    assert_eq!(p.read("heki.toml"), HEADER);
    // Without a `pyproject.toml`, the configuration goes into `heki.toml` unasked.
    let p = Project {
        dir: fresh_dir("init_no_pyproject"),
    };
    p.write("app.py", "def f():\n    return 1\n");
    let (code, _, stderr) = answered(&p, &["--no-recommend"], "\n");
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.starts_with("heki init (experimental) in .: Enter takes the answer in capitals.\nSet up coding agents (the Agent Skill, AGENTS.md)? [y/N] Created heki.toml\n"),
        "{stderr}"
    );
}

#[test]
fn the_answers_choose_what_is_written() {
    let p = Project::new("init_answers");
    p.write("app.py", BLOCKING);
    std::fs::create_dir_all(p.dir.join(".claude")).unwrap();
    // The table of `pyproject.toml` (after an answer that is neither), no recommend, the baseline
    // by default, the agents: the skill for Claude Code by default (`.claude/` is there), not for
    // Codex, and a new `AGENTS.md`.
    let (code, stdout, stderr) = answered(&p, &[], "maybe\nn\nno\n\nY\n\n\nyes\n");
    assert_eq!((code, stdout.as_str()), (0, ""), "{stderr}");
    let prompts = [
        "heki init (experimental) in .: Enter takes the answer in capitals.\n",
        "Write the configuration to heki.toml (n: the [tool.heki] table of pyproject.toml)? [Y/n] Answer y or n.\n",
        "Write the configuration to heki.toml (n: the [tool.heki] table of pyproject.toml)? [Y/n] ",
        "Run heki recommend and add the keys it proposes to pyproject.toml, commented out? [Y/n] ",
        "heki check reports 1 diagnostic(s) now: record them in a Ratchet baseline, heki-baseline.toml? [Y/n] ",
        "Set up coding agents (the Agent Skill, AGENTS.md)? [y/N] ",
        "Install the heki Agent Skill for Claude Code in .claude/skills/heki? [Y/n] ",
        "Install the heki Agent Skill for Codex in .agents/skills/heki? [y/N] ",
        "Create AGENTS.md with a heki section? [y/N] ",
        "Updated pyproject.toml\nCreated heki-baseline.toml (1 entries, 1 diagnostic(s))\nCreated AGENTS.md\nInstalled the heki Agent Skill (experimental) at .claude/skills/heki\n",
    ]
    .concat();
    assert!(stderr.starts_with(&prompts), "{stderr}");
    assert_eq!(
        p.read("pyproject.toml"),
        format!(
            "[project]\nname = \"t\"\n\n{HEADER}[tool.heki]\nbaseline = \"heki-baseline.toml\"\n"
        )
    );
    assert_eq!(
        p.read(".claude/skills/heki/SKILL.md"),
        repo_file("skills/heki/SKILL.md")
    );
    assert!(!p.dir.join(".agents").exists());
    let agents = p.read("AGENTS.md");
    assert!(
        agents.starts_with(&format!("{BEGIN}\n## heki\n")) && agents.ends_with(&format!("{END}\n")),
        "{agents}"
    );
    assert_eq!(p.heki(&["check"]).0, 0);

    // The input ends before the questions do: nothing is written.
    let p = Project::new("init_answers_end");
    let before = files(&p.dir);
    let (code, _, stderr) = answered(&p, &[], "\n");
    assert_eq!(code, 2);
    assert!(
        stderr.ends_with(&format!(
            "error: the input ended before an answer: nothing was written\n  = help: answer each question, or pass `--yes` to take the default answers\n  = docs: {DOCS}\n"
        )),
        "{stderr}"
    );
    assert_eq!(files(&p.dir), before);
}

#[test]
fn the_agents_section_is_written_once() {
    let p = Project::new("init_agents_md");
    p.write("AGENTS.md", "# Rules\n\nBe nice.\n");
    // It reads `AGENTS.md`: the section is not written twice into the agent's instructions.
    p.write("CLAUDE.md", "@AGENTS.md\n");
    let init = |extra: &[&str]| {
        let mut args = vec!["init", "--yes", "--no-recommend", "--agents-md"];
        args.extend_from_slice(extra);
        p.heki(&args)
    };
    let (code, _, stderr) = init(&[]);
    assert_eq!(code, 0, "{stderr}");
    let written = p.read("AGENTS.md");
    assert!(
        written.starts_with(&format!("# Rules\n\nBe nice.\n\n{BEGIN}\n## heki\n"))
            && written.ends_with(&format!("{END}\n")),
        "{written}"
    );
    assert_eq!(p.read("CLAUDE.md"), "@AGENTS.md\n");
    assert!(stderr.contains("\nUpdated AGENTS.md\n"), "{stderr}");
    // The maintainer edits the section and writes on after it: a run leaves it as it is.
    let edited = written.replace("- Do not silence", "- Never silence") + "\n## After\n";
    p.write("AGENTS.md", &edited);
    std::fs::remove_file(p.dir.join("heki.toml")).unwrap();
    let (code, _, stderr) = init(&[]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(p.read("AGENTS.md"), edited);
    let kept = "AGENTS.md has the heki section already (`--force` writes it again)\n";
    assert!(stderr.contains(kept), "{stderr}");
    // `--force` writes the section again in its place.
    let (code, _, stderr) = init(&["--force"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(p.read("AGENTS.md"), format!("{written}\n## After\n"));
    assert!(stderr.contains("\nUpdated AGENTS.md\n"), "{stderr}");
    let (code, _, stderr) = init(&["--force"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains(kept), "{stderr}");
    assert_eq!(p.read("AGENTS.md").matches(BEGIN).count(), 1);
    // A section without its last line is not replaced.
    p.write("AGENTS.md", &format!("{BEGIN}\nmine\n"));
    let (code, _, stderr) = init(&["--force"]);
    assert_eq!(code, 2);
    assert_eq!(
        stderr,
        format!(
            "error: AGENTS.md: the heki section has no line `{END}` after `{BEGIN}`\n  = help: add `{END}` where the section ends, or remove the section\n  = docs: {DOCS}\n"
        )
    );
    // An instruction file that is not text stops the run.
    std::fs::write(p.dir.join("AGENTS.md"), b"\xff\xfe").unwrap();
    let (code, _, stderr) = init(&["--force"]);
    assert_eq!(code, 2);
    assert!(stderr.starts_with("error: "), "{stderr}");
}

#[test]
fn the_section_goes_where_the_agents_read_it() {
    // Only `CLAUDE.md`: no `AGENTS.md` is made.
    let p = Project::new("init_claude_md");
    p.write("app.py", "x = 1\n");
    p.write("CLAUDE.md", "Notes.");
    let (code, _, stderr) = p.heki(&["init", "--yes", "--agents-md"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        p.read("CLAUDE.md")
            .starts_with(&format!("Notes.\n\n{BEGIN}\n")),
        "{}",
        p.read("CLAUDE.md")
    );
    assert!(!p.dir.join("AGENTS.md").exists());
    assert!(
        stderr.contains("\nheki recommend proposed no keys.\n"),
        "{stderr}"
    );
    // Neither file: a new `AGENTS.md` of the section alone.
    let p = Project::new("init_new_agents_md");
    let (code, _, stderr) = p.heki(&["init", "--yes", "--no-recommend", "--agents-md"]);
    assert_eq!(code, 0, "{stderr}");
    let text = p.read("AGENTS.md");
    assert!(
        text.starts_with(&format!("{BEGIN}\n## heki\n")) && text.ends_with(&format!("{END}\n")),
        "{text}"
    );
    // `--yes` alone sets up no agent.
    let p = Project::new("init_no_agents");
    p.write("AGENTS.md", "# Agents\n");
    assert_eq!(p.heki(&["init", "--yes", "--no-recommend"]).0, 0);
    assert_eq!(p.read("AGENTS.md"), "# Agents\n");
    assert!(!p.dir.join(".claude").exists() && !p.dir.join(".agents").exists());
}

#[test]
fn the_skill_is_installed_for_each_agent_named() {
    let p = Project::new("init_skills");
    let (code, _, stderr) = p.heki(&[
        "init",
        "--yes",
        "--no-recommend",
        "--skill",
        "claude-code",
        "--skill",
        "codex",
        "--skill",
        "codex",
    ]);
    assert_eq!(code, 0, "{stderr}");
    for dir in [".claude/skills/heki", ".agents/skills/heki"] {
        for file in ["SKILL.md", "references/adopt.md"] {
            assert_eq!(
                p.read(&format!("{dir}/{file}")),
                repo_file(&format!("skills/heki/{file}"))
            );
        }
        let line = format!("Installed the heki Agent Skill (experimental) at {dir}\n");
        assert!(stderr.contains(&line), "{stderr}");
    }
    assert_eq!(stderr.matches("Installed").count(), 2, "{stderr}");
    // An installed skill is replaced only with `--force`: nothing is written before the refusal.
    std::fs::remove_file(p.dir.join("heki.toml")).unwrap();
    let (code, _, stderr) = p.heki(&["init", "--yes", "--no-recommend", "--skill", "codex"]);
    assert_eq!(code, 2);
    assert_eq!(
        stderr,
        "error: .agents/skills/heki: the skill is already installed\n  = help: pass `--force` to replace it with the skill of this heki\n  = docs: https://heki.koxudaxi.dev/coding-agents/#install-the-skill\n"
    );
    assert!(!p.dir.join("heki.toml").exists());
    p.write(".agents/skills/heki/SKILL.md", "edited");
    let (code, _, stderr) = p.heki(&[
        "init",
        "--yes",
        "--no-recommend",
        "--skill",
        "codex",
        "--force",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        p.read(".agents/skills/heki/SKILL.md"),
        repo_file("skills/heki/SKILL.md")
    );

    // A file where the skill goes stays, even with `--force`.
    let p = Project::new("init_skill_taken");
    p.write(".claude/skills/heki", "a file");
    let args = ["init", "--yes", "--no-recommend", "--force", "--skill"];
    let (code, _, stderr) = p.heki(&[&args[..], &["claude-code"][..]].concat());
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with(
            "error: .claude/skills/heki: a file or a link is where the skill goes\n  = help: remove it\n"
        ),
        "{stderr}"
    );
    assert_eq!(p.read(".claude/skills/heki"), "a file");
    // The skills directory cannot be made: `.agents` is a file.
    p.write(".agents", "a file");
    let (code, _, stderr) = p.heki(&[&args[..], &["codex"][..]].concat());
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("error: .agents/skills/heki: cannot write the skill: "),
        "{stderr}"
    );
}

#[test]
fn files_that_cannot_be_analyzed_leave_the_baseline_out() {
    let p = Project::new("init_syntax_error");
    p.write("app.py", BLOCKING);
    p.write("broken.py", "def (:\n");
    let (code, _, stderr) = p.heki(&["init", "--yes"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(p.read("heki.toml").starts_with(HEADER));
    assert!(!p.read("heki.toml").contains("baseline"));
    assert!(!p.dir.join("heki-baseline.toml").exists());
    assert!(
        stderr.contains(
            "error: baseline `heki-baseline.toml` not written: 1 file(s) could not be analyzed\n"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("broken.py"), "{stderr}");
}

#[test]
fn a_baseline_there_is_replaced_only_with_force() {
    let p = Project::new("init_baseline_there");
    p.write("app.py", BLOCKING);
    p.write("heki-baseline.toml", "old");
    let (code, _, stderr) = p.heki(&["init", "--yes", "--no-recommend"]);
    assert_eq!(code, 2);
    assert_eq!(
        stderr,
        format!(
            "error: heki-baseline.toml: the file is there already\n  = help: pass `--force` to replace it\n  = docs: {DOCS}\n"
        )
    );
    assert!(!p.dir.join("heki.toml").exists());
    assert_eq!(p.read("heki-baseline.toml"), "old");
    p.write(
        ".github/CODEOWNERS",
        "/heki.toml @team\n/heki-baseline.toml @team\n",
    );
    let (code, _, stderr) = p.heki(&["init", "--yes", "--baseline", "--no-recommend", "--force"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "Created heki.toml\nUpdated heki-baseline.toml (1 entries, 1 diagnostic(s))\n.github/CODEOWNERS gives heki.toml and heki-baseline.toml an owner.\n{NEXT}"
        )
    );
    assert_eq!(p.heki(&["check"]).0, 0);
}

#[test]
fn files_that_cannot_be_read_or_written_stop_the_run() {
    // A directory where the configuration goes.
    let p = Project::new("init_unwritable");
    std::fs::create_dir_all(p.dir.join("heki.toml")).unwrap();
    let (code, _, stderr) = p.heki(&["init", "--yes", "--no-recommend"]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("error: heki.toml: cannot write the file: "),
        "{stderr}"
    );
    // A directory where the baseline goes: the configuration that names it is not written.
    let p = Project::new("init_unwritable_baseline");
    p.write("app.py", BLOCKING);
    std::fs::create_dir_all(p.dir.join("heki-baseline.toml")).unwrap();
    let (code, _, stderr) = p.heki(&["init", "--yes", "--no-recommend", "--force"]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("error: heki-baseline.toml: cannot write the file: "),
        "{stderr}"
    );
    assert!(!p.dir.join("heki.toml").exists());
    // A `pyproject.toml` that is not TOML, or not text, stops the run before any question.
    let p = Project::new("init_bad_pyproject");
    p.write("pyproject.toml", "[project\n");
    let (code, _, stderr) = p.heki(&["init"]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("error: pyproject.toml: cannot read the TOML: "),
        "{stderr}"
    );
    std::fs::write(p.dir.join("pyproject.toml"), b"\xff\xfe").unwrap();
    let (code, _, stderr) = p.heki(&["init"]);
    assert_eq!(code, 2);
    assert!(stderr.starts_with("error: pyproject.toml: "), "{stderr}");
    assert_eq!(files(&p.dir).len(), 1);
}

#[test]
fn each_question_needs_an_answer_or_an_option() {
    let p = Project::new("init_open_questions");
    p.write("app.py", BLOCKING);
    let open = |args: &[&str]| {
        let (code, _, stderr) = p.heki(&[&["init", "--config", "heki.toml"][..], args].concat());
        assert_eq!(code, 2, "{stderr}");
        stderr
    };
    assert_eq!(
        open(&["--no-recommend"]),
        format!(
            "error: `heki init` needs an answer, and the standard input is not a terminal: heki check reports 1 diagnostic(s) now: record them in a Ratchet baseline, heki-baseline.toml?\n  = help: pass `--yes` to take the default answers, or answer this question with `--baseline` or `--no-baseline`\n  = docs: {DOCS}\n"
        )
    );
    assert_eq!(
        open(&["--no-recommend", "--no-baseline"]),
        format!(
            "error: `heki init` needs an answer, and the standard input is not a terminal: Set up coding agents (the Agent Skill, AGENTS.md)?\n  = help: pass `--yes` to take the default answers, or answer this question with `--skill` and `--agents-md`\n  = docs: {DOCS}\n"
        )
    );
    assert_eq!(files(&p.dir).len(), 2);

    // The instruction files the project has take the section, by default.
    let p = Project::new("init_open_sections");
    p.write("AGENTS.md", "# Agents\n");
    p.write("CLAUDE.md", "# Claude\n");
    let (code, _, stderr) = answered(&p, &["--no-recommend"], "\ny\nn\nn\n\n");
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("Codex in .agents/skills/heki? [Y/n] Add a heki section to AGENTS.md and CLAUDE.md? [Y/n] Created heki.toml\nUpdated AGENTS.md\nUpdated CLAUDE.md\n"),
        "{stderr}"
    );
    for file in ["AGENTS.md", "CLAUDE.md"] {
        assert_eq!(p.read(file).matches(BEGIN).count(), 1);
    }
}
