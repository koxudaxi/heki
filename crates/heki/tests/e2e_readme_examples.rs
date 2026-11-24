//! The examples at the top of `README.md` and `docs/index.md`, run through the binary: each
//! example's files are written to a project of their own, and the `heki check` command of the
//! console block that follows them must print exactly the rest of the block.

mod common;

use common::{Project, repo_file};

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

/// The `--diff` example of the `collections.deque` rewrite, shown in the README, docs/index.md,
/// and docs/fixes.md: its console block, from the command to the end of the output.
fn deque_example(doc: &str) -> &str {
    let start = doc
        .find("$ heki check --diff --include-size-dependent jobs.py\n")
        .expect("the deque example");
    let end = doc[start..].find("\n```").expect("the end of its block");
    &doc[start..=start + end]
}

#[test]
fn the_deque_example_prints_what_the_pages_show() {
    let readme = repo_file("README.md");
    let block = deque_example(&readme);
    for page in ["docs/index.md", "docs/fixes.md"] {
        assert_eq!(deque_example(&repo_file(page)), block, "{page}");
    }
    let (command, expected) = block.split_once('\n').unwrap();
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
