//! Rule selection and inline suppressions as Ruff users expect them (end to end, through the
//! real binary).

mod common;

use common::Project;

const BLOCKING: &str = "import time\n\n\nasync def handler():\n    time.sleep(1)\n";

fn concise(p: &Project, args: &[&str]) -> Vec<String> {
    let mut all = vec!["--output-format", "concise"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    stdout.lines().map(str::to_owned).collect()
}

#[test]
fn an_empty_select_starts_from_no_rules() {
    let p = Project::new("empty_select");
    p.write("app.py", BLOCKING).write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = []\nextend-select = [\"MUT106\"]\n",
    );
    assert_eq!(concise(&p, &[]), Vec::<String>::new());
    // Without the key, the default rules run.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT106\"]\n",
    );
    let out = concise(&p, &[]);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].starts_with("app.py:5:5: EFF101"), "{out:?}");
    // `--select` on the command line wins over an empty configured `select`.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = []\n",
    );
    assert_eq!(concise(&p, &["--select", "EFF101"]).len(), 1);
}
