//! Rule selection and inline suppressions as Ruff users expect them: an empty `select`,
//! `per-file-ignores` globs, and suppressions of reports that root-cause grouping folds
//! (end to end, through the real binary).

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

/// Twelve `async def`s of one file calling `time.sleep`: more than the grouping threshold (10)
/// even without one of them.
fn many_blocking_callers(suppress_first: bool) -> String {
    let mut src = String::from("import time\n");
    for i in 0..12 {
        let comment = if i == 0 && suppress_first {
            "  # heki: ignore[EFF101]"
        } else {
            ""
        };
        src.push_str(&format!(
            "\n\nasync def handler_{i}():\n    time.sleep(1){comment}\n"
        ));
    }
    src
}

#[test]
fn a_suppressed_call_does_not_hide_the_calls_grouped_with_it() {
    let p = Project::new("suppress_grouped");
    p.write("svc.py", &many_blocking_callers(false));
    let all = concise(&p, &[]);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(
        all[0].starts_with("svc.py:5:5: EFF101 `async def handler_0`")
            && all[0]
                .ends_with("(+11 more async call sites reaching the same blocking root cause)"),
        "{all:?}"
    );

    // The suppressed call leaves the group; the next call stands for the other ten.
    p.write("svc.py", &many_blocking_callers(true));
    let rest = concise(&p, &[]);
    assert_eq!(rest.len(), 1, "{rest:?}");
    assert!(
        rest[0].starts_with("svc.py:9:5: EFF101 `async def handler_1`")
            && rest[0]
                .ends_with("(+10 more async call sites reaching the same blocking root cause)"),
        "{rest:?}"
    );
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

#[test]
fn per_file_ignores_globs_match_across_directories_as_in_ruff() {
    let p = Project::new("per_file_globs");
    p.write("scripts/top.py", BLOCKING)
        .write("scripts/tools/worker.py", BLOCKING)
        .write("src/app.py", BLOCKING)
        .write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nper-file-ignores = { \"scripts/*.py\" = [\"EFF101\"] }\n",
        );
    let out = concise(&p, &[]);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].starts_with("src/app.py:5:5: EFF101"), "{out:?}");
}
