//! The examples at the top of `README.md` and `docs/index.md`, run through the binary: the files
//! of all of them are written to one project, and `heki check` on it reports each example's rule.
//! The image at the top of the pages shows that run, and `build_docs.py --check` compares it with
//! the run. The excerpts of the sample project in `tests/sample_service/`, in "Try it in 5
//! minutes" and in `docs/getting-started.md`, are run on it.

mod common;

use std::path::Path;

use common::{Project, repo_file, repo_root};

/// The examples: from the first `### ` heading to the sentence after the last one.
fn examples(doc: &str) -> &str {
    let start = doc.find("\n### ").expect("an example heading");
    let end = doc[start..]
        .find("\nThe image at the top is `heki check` on these examples")
        .expect("the sentence after the examples");
    &doc[start..start + end]
}

/// The fenced blocks of one example: `(language, text)`.
fn fenced(example: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = example;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let (lang, body) = after.split_once('\n').unwrap();
        let close = body.find("\n```").unwrap();
        out.push((lang, &body[..=close]));
        rest = &body[close + 4..];
    }
    out
}

#[test]
fn the_readme_examples_report_what_the_image_shows() {
    let readme = repo_file("README.md");
    let index = repo_file("docs/index.md");
    assert_eq!(
        examples(&readme),
        examples(&index),
        "README.md and docs/index.md show the same examples"
    );
    let sections: Vec<&str> = examples(&readme).split("\n### ").skip(1).collect();
    // A bug found with no configuration, and a wall written as a contract.
    let rules = [
        "EFF101 (async-blocking-call) [error]",
        "MUT201 (write-owner-violation) [error, heuristic]",
    ];
    assert_eq!(sections.len(), rules.len(), "{sections:#?}");
    let p = Project::new("readme_examples");
    let mut written = Vec::new();
    for section in &sections {
        let mut files = Vec::new();
        for (lang, text) in fenced(section) {
            match lang {
                // The first line names the file: `# app/users.py`, `# heki.toml`.
                "python" | "toml" => {
                    let path = text.lines().next().unwrap().strip_prefix("# ").unwrap();
                    assert!(!written.contains(&path), "two examples write {path}");
                    p.write(path, text);
                    written.push(path);
                    files.push(path);
                }
                other => panic!("unexpected block `{other}` in {section}"),
            }
        }
        assert!(!files.is_empty(), "{section}");
    }
    let (code, stdout, stderr) = p.heki(&["check"]);
    // Both reports are errors: an exact one, and one of a contract with `fail = true`.
    assert_eq!(code, 1, "{stdout}{stderr}");
    let reports: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("app/"))
        .collect();
    assert_eq!(reports.len(), rules.len(), "{stdout}");
    for (report, rule) in reports.iter().zip(rules) {
        assert!(report.contains(&format!(": {rule} ")), "{rule}: {stdout}");
    }
}

/// The `--diff` example of the `collections.deque` rewrite in docs/fixes.md: its console block,
/// from the command to the end of the output.
fn deque_example(doc: &str) -> &str {
    let start = doc
        .find("$ heki check --diff --include-size-dependent jobs.py\n")
        .expect("the deque example");
    let end = doc[start..].find("\n```").expect("the end of its block");
    &doc[start..=start + end]
}

#[test]
fn the_deque_example_prints_what_the_pages_show() {
    let fixes = repo_file("docs/fixes.md");
    let (command, expected) = deque_example(&fixes).split_once('\n').unwrap();
    // The file before the rewrite: the diff's context and removed lines (a page may drop the
    // space of an empty context line).
    fn before(line: &str) -> Option<&str> {
        match line.chars().next() {
            None => Some(""),
            Some(' ' | '-') => Some(&line[1..]),
            Some(_) => None,
        }
    }
    let original: String = expected
        .lines()
        .skip_while(|l| !l.starts_with("@@ "))
        .skip(1)
        .take_while(|l| !l.starts_with("Would fix"))
        .filter_map(before)
        .map(|l| format!("{l}\n"))
        .collect();
    let p = Project::new("readme_deque_example");
    p.write("jobs.py", &original);
    let args: Vec<&str> = command
        .strip_prefix("$ heki ")
        .unwrap()
        .split_whitespace()
        .collect();
    let (code, stdout, stderr) = p.heki(&args);
    // A diff with something to apply exits with 1.
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(format!("{stdout}{stderr}"), expected);
}

/// The console block of "Try it in 5 minutes" that runs `heki recommend`, without the indentation
/// of the list item that holds it.
fn sample_excerpt(doc: &str) -> String {
    let command = doc
        .lines()
        .position(|line| line.trim_start().starts_with("$ ") && line.ends_with(" recommend"))
        .expect("the recommend command of the excerpt");
    let lines: Vec<&str> = doc.lines().collect();
    let open = (0..command)
        .rev()
        .find(|&i| lines[i].trim_start() == "```console")
        .expect("the console block of the excerpt");
    let indent = &lines[open][..lines[open].len() - "```console".len()];
    lines[open + 1..]
        .iter()
        .take_while(|line| line.trim_start() != "```")
        .map(|line| format!("{}\n", line.strip_prefix(indent).unwrap_or(line)))
        .collect()
}

/// Every file below `from/rel`, written to `p` at its path below `from`.
fn copy_project(from: &Path, rel: &Path, p: &Project) {
    for entry in std::fs::read_dir(from.join(rel)).unwrap() {
        let name = rel.join(entry.unwrap().file_name());
        if from.join(&name).is_dir() {
            copy_project(from, &name, p);
        } else {
            let text = std::fs::read_to_string(from.join(&name)).unwrap();
            p.write(name.to_str().unwrap(), &text);
        }
    }
}

/// The lines of `expected` are an excerpt of `output`: the runs of lines between the `...` lines
/// are lines of `output` in a row, in the order they come.
fn assert_excerpt(expected: &str, output: &str, command: &str) {
    let lines: Vec<&str> = output.lines().collect();
    let expected: Vec<&str> = expected.lines().collect();
    let mut from = 0;
    for run in expected
        .split(|line| *line == "...")
        .filter(|run| !run.is_empty())
    {
        let at = (from..=lines.len().saturating_sub(run.len()))
            .find(|&i| lines[i..i + run.len()] == *run)
            .unwrap_or_else(|| {
                panic!("`{command}` does not print, after line {from}:\n{run:#?}\n{output}")
            });
        from = at + run.len();
    }
}

#[test]
fn the_sample_service_prints_the_excerpts_the_pages_show() {
    let readme = sample_excerpt(&repo_file("README.md"));
    assert_eq!(
        readme,
        sample_excerpt(&repo_file("docs/index.md")),
        "README.md and docs/index.md show the same excerpt"
    );
    let p = Project::new("readme_sample_service");
    copy_project(&repo_root().join("tests/sample_service"), Path::new(""), &p);
    for excerpt in [
        readme,
        sample_excerpt(&repo_file("docs/getting-started.md")),
    ] {
        // Each command, with the lines of the block up to the next one.
        let mut commands: Vec<(&str, String)> = Vec::new();
        for line in excerpt.lines() {
            match (line.strip_prefix("$ "), commands.last_mut()) {
                (Some(command), _) => commands.push((command, String::new())),
                (None, Some((_, expected))) => expected.push_str(&format!("{line}\n")),
                (None, None) => panic!("the excerpt starts with a command: {excerpt}"),
            }
        }
        assert_eq!(commands.len(), 2, "{excerpt}");
        for (command, expected) in &commands {
            // `uvx heki check` (a try without installing) and `heki check` run this build.
            let args: Vec<&str> = command
                .strip_prefix("uvx ")
                .unwrap_or(command)
                .strip_prefix("heki ")
                .unwrap_or_else(|| panic!("{command}"))
                .split_whitespace()
                .collect();
            let (code, stdout, stderr) = p.heki(&args);
            // Nothing fails the run on the sample: it is a project `heki check` passes.
            assert_eq!(code, 0, "{command}: {stderr}");
            assert_excerpt(expected, &format!("{stdout}{stderr}"), command);
        }
    }
}
