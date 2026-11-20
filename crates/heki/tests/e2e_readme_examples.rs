//! The examples at the top of `README.md` and `docs/index.md`, run through the binary: each
//! example's files are written to a project of their own, and `heki check` must print exactly
//! the console block that follows them.

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
        let expected = console.strip_prefix("$ heki check\n").unwrap();
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 1, "{stderr}");
        assert_eq!(format!("{stdout}{stderr}"), expected, "{section}");
    }
}
