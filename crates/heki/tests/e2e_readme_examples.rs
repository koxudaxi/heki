//! The examples at the top of `README.md` and `docs/index.md`, run through the binary: each
//! example's files are written to a project of their own, and the `heki check` command of the
//! console block that follows them must print exactly the rest of the block. The excerpt of
//! "Try it in 5 minutes" is run on the sample project in `tests/sample_service/`.

mod common;

use std::path::Path;

use common::{Project, repo_file, repo_root};

/// The examples: from the first `### ` heading to the sentence after the last one.
fn examples(doc: &str) -> &str {
    let start = doc.find("\n### ").expect("an example heading");
    let end = doc[start..]
        .find("\nRuff, ty, Pyrefly, pyright, and mypy")
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
fn the_readme_examples_print_what_the_readme_shows() {
    let readme = repo_file("README.md");
    let index = repo_file("docs/index.md");
    assert_eq!(
        examples(&readme),
        examples(&index),
        "README.md and docs/index.md show the same examples"
    );
    let sections: Vec<&str> = examples(&readme).split("\n### ").skip(1).collect();
    assert_eq!(sections.len(), 2, "{sections:#?}");
    for (i, section) in sections.iter().enumerate() {
        let p = Project::new(&format!("readme_example_{i}"));
        let mut console = None;
        for (lang, text) in fenced(section) {
            match lang {
                "python" => {
                    // The first line names the file: `# app/users.py`.
                    let path = text.lines().next().unwrap().strip_prefix("# ").unwrap();
                    p.write(path, text);
                }
                "console" => console = Some(text),
                other => panic!("unexpected block `{other}` in {section}"),
            }
        }
        let console = console.expect("a console block");
        let (command, expected) = console.split_once('\n').unwrap();
        let args: Vec<&str> = command
            .strip_prefix("$ heki ")
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(args[0], "check", "{command}");
        let (code, stdout, stderr) = p.heki(&args);
        // An example whose report is an exact error fails the run; a warning is printed, and
        // the run passes.
        let fails = expected.lines().any(|line| line.contains(") [error] "));
        assert_eq!(code, i32::from(fails), "{stderr}");
        assert_eq!(format!("{stdout}{stderr}"), expected, "{section}");
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
fn the_sample_service_prints_the_excerpt_the_pages_show() {
    let readme = sample_excerpt(&repo_file("README.md"));
    assert_eq!(
        readme,
        sample_excerpt(&repo_file("docs/index.md")),
        "README.md and docs/index.md show the same excerpt"
    );
    let p = Project::new("readme_sample_service");
    copy_project(&repo_root().join("tests/sample_service"), Path::new(""), &p);
    // Each command, with the lines of the block up to the next one.
    let mut commands: Vec<(&str, String)> = Vec::new();
    for line in readme.lines() {
        match (line.strip_prefix("$ "), commands.last_mut()) {
            (Some(command), _) => commands.push((command, String::new())),
            (None, Some((_, expected))) => expected.push_str(&format!("{line}\n")),
            (None, None) => panic!("the excerpt starts with a command: {readme}"),
        }
    }
    assert_eq!(commands.len(), 2, "{readme}");
    for (command, expected) in &commands {
        // `uvx heki==<version> check` runs the release the page pins: this build.
        assert!(command.starts_with("uvx heki=="), "{command}");
        let args: Vec<&str> = command.split_whitespace().skip(2).collect();
        let (code, stdout, stderr) = p.heki(&args);
        // Nothing fails the run on the sample: it is a project `heki check` passes.
        assert_eq!(code, 0, "{command}: {stderr}");
        assert_excerpt(expected, &format!("{stdout}{stderr}"), command);
    }
}
