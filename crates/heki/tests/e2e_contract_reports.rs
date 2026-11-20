//! Contract rules (`MUT109`, `MUT201`, `MUT204`, `EFF201`): which configured names match, which
//! violations are reported, and what each report counts (end to end, through the real binary).

mod common;

use common::Project;
use serde_json::Value;

fn project(name: &str, config: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    );
    p
}

/// `(line, target, sites)` of every diagnostic of `rule`.
fn reports(p: &Project, rule: &str) -> Vec<(u64, String, u64)> {
    let (code, stdout, stderr) = p.check(&["--select", rule, "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            assert_eq!(d["rule_code"], rule, "{d:#}");
            (
                d["line"].as_u64().unwrap(),
                d["target"].as_str().unwrap().to_owned(),
                d["sites"].as_u64().unwrap_or(1),
            )
        })
        .collect()
}

const MODEL_FIELDS: &str = "class Model:\n    def __init__(self) -> None:\n        self.fields: list[int] = []\n\n\ndef outside(m: Model) -> None:\n    m.fields = [1]\n";

#[test]
fn write_owner_targets_use_every_glob_form_on_dotted_suffixes() {
    for (target, reported) in [
        ("Mode?.fields", true),
        ("Mode[lx].fields", true),
        ("{Model,Other}.fields", true),
        ("Model.fields", true),
        ("Mode?.items", false),
        ("Other?.fields", false),
    ] {
        let p = project(
            "owner_globs",
            &format!(
                "[[tool.heki.write-owners]]\nname = \"owner\"\ntargets = [\"{target}\"]\nallowed-writers = [\"pkg.models.Store.*\"]\n"
            ),
        );
        p.write("pkg/__init__.py", "")
            .write("pkg/models.py", MODEL_FIELDS);
        let expected = if reported {
            vec![(7, target.to_owned(), 1)]
        } else {
            vec![]
        };
        assert_eq!(reports(&p, "MUT201"), expected, "{target}");
    }
}

const MODEL_VALUE: &str = "class Model:\n    def __init__(self) -> None:\n        self.value = 0\n";

#[test]
fn a_boundary_forbidding_reads_and_writes_of_one_field_reports_both() {
    let p = project(
        "boundary_read_and_write",
        "[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"app.render\"]\nforbid = [\"reads-targets\", \"writes-targets\"]\nread-targets = [\"Model.value\"]\nwrite-targets = [\"Model.value\"]\n",
    );
    p.write(
        "app.py",
        &format!(
            "{MODEL_VALUE}\n\ndef helper(m: Model) -> None:\n    x = m.value\n    m.value = x + 1\n\n\ndef render(m: Model) -> None:\n    helper(m)"
        ),
    );
    // Both at the call of `helper`, each its own report and baseline entry.
    assert_eq!(
        reports(&p, "EFF201"),
        vec![
            (12, "pure:Model.value".to_owned(), 1),
            (12, "pure:reads:Model.value".to_owned(), 1),
        ]
    );
}

#[test]
fn two_invalidation_contracts_on_one_write_are_two_reports() {
    let p = project(
        "two_invalidations",
        "[[tool.heki.invalidation]]\nname = \"one\"\ntargets = [\"Model.value\"]\ninvalidators = [\"app.notify_a\"]\n\n[[tool.heki.invalidation]]\nname = \"two\"\ntargets = [\"Model.value\"]\ninvalidators = [\"app.notify_b\"]\n",
    );
    let src = format!(
        "{MODEL_VALUE}\n\ndef notify_a() -> None: ...\n\n\ndef notify_b() -> None: ...\n\n\ndef change(m: Model) -> None:\n    m.value = 1\n"
    );
    p.write("app.py", &src);
    assert_eq!(
        reports(&p, "MUT109"),
        vec![
            (13, "one:Model.value".to_owned(), 1),
            (13, "two:Model.value".to_owned(), 1),
        ]
    );
    // Announcing the change to one contract leaves the other's report.
    p.write(
        "app.py",
        &src.replace("    m.value = 1\n", "    m.value = 1\n    notify_b()\n"),
    );
    assert_eq!(
        reports(&p, "MUT109"),
        vec![(13, "one:Model.value".to_owned(), 1)]
    );
}
