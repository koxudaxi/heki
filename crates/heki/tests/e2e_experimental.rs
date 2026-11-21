//! Experimental features as users see them: `heki rule`, and the documentation generator that
//! writes their page and checks the hand-written pages against them (end to end, through the
//! real binary).

mod common;

use common::{Project, python, repo_root};
use serde_json::{Value, json};
use std::process::Command;

const POLICY: &str = "Experimental features can still change in behavior or precision, or be \
    removed, in any release, and each change is listed in the release notes. An opt-in rule that is \
    not experimental is stable and only off by default.";

#[test]
fn rule_command_says_that_a_rule_is_experimental() {
    let p = Project::new("experimental_rule");
    let (code, doc, _) = p.heki(&["rule", "MUT101"]);
    assert_eq!(code, 0);
    let summary = "# cached-property-stale-mutation (MUT101)\n\nMutating an attribute read by \
        `@cached_property` without invalidating the cache.\n\n";
    let heuristic = "This rule is heuristic: a premise of its claim can be open (\"What a report \
        states\" lists them), so a report can be wrong about the code. It runs only when `select` \
        or `extend-select` names its code or name.\n\n";
    let note = "This rule is experimental. Its precision outside a few code bases is still low.";
    assert!(
        doc.starts_with(&format!(
            "{summary}{heuristic}{note} {POLICY}\n\n## What it does\n"
        )),
        "{doc}"
    );
    let (_, reader, _) = p.heki(&["rule", "MUT111"]);
    let note = "This rule is experimental. It reports the readers of the stale writes `MUT101` \
        finds, so it shares `MUT101`'s precision.";
    assert!(
        reader.contains(&format!(".\n\n{note} {POLICY}\n\n## What it does\n")),
        "{reader}"
    );
    let (_, stable, _) = p.heki(&["rule", "MUT107"]);
    assert!(!stable.to_lowercase().contains("experimental"), "{stable}");

    let json = |rule: &str| -> Value {
        let (code, out, _) = p.heki(&["rule", rule, "--output-format", "json"]);
        assert_eq!(code, 0);
        serde_json::from_str(&out).unwrap()
    };
    let entry = &json("cached-property-stale-mutation")["experimental"];
    assert_eq!(
        (&entry["id"], &entry["kind"], &entry["since"]),
        (&json!("rule.MUT101"), &json!("rule"), &json!("0.1.0"))
    );
    assert_eq!(json("MUT107")["experimental"], Value::Null);
}

/// A copy of the generator and the hand-written pages it checks, so that the test can edit them.
struct DocsTree(Project);

impl DocsTree {
    fn new(name: &str) -> Self {
        let p = Project::new(name);
        let root = repo_root();
        for rel in [
            "scripts/build_docs.py",
            "zensical.toml",
            "README.md",
            "ARCHITECTURE.md",
            "CHANGELOG.md",
        ] {
            p.write(rel, &std::fs::read_to_string(root.join(rel)).unwrap());
        }
        for page in std::fs::read_dir(root.join("docs")).unwrap() {
            let path = page.unwrap().path();
            if path.extension().is_some_and(|e| e == "md") {
                let rel = format!("docs/{}", path.file_name().unwrap().to_str().unwrap());
                p.write(&rel, &std::fs::read_to_string(&path).unwrap());
            }
        }
        Self(p)
    }

    /// `build_docs.py [args]`: (exit code, stdout).
    fn build_docs(&self, args: &[&str]) -> (i32, String) {
        let out = Command::new(python())
            .arg("scripts/build_docs.py")
            .args(["--heki", env!("CARGO_BIN_EXE_heki")])
            .args(args)
            .current_dir(&self.0.dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.is_empty(), "{stdout}{stderr}");
        (out.status.code().unwrap(), stdout)
    }

    /// Replaces `old` in `rel`, runs the check, and puts the page back.
    fn check_with(&self, rel: &str, old: &str, new: &str) -> (i32, String) {
        let original = self.0.read(rel);
        assert!(original.contains(old), "{rel}: no `{old}`");
        self.0.write(rel, &original.replacen(old, new, 1));
        let result = self.build_docs(&["--check"]);
        self.0.write(rel, &original);
        result
    }
}

#[test]
fn docs_check_fails_on_a_stale_experimental_page() {
    let docs = DocsTree::new("experimental_page");
    assert_eq!(docs.build_docs(&[]).0, 0);
    let page = docs.0.read("docs/experimental.md");
    assert!(
        page.contains(
            "| `rule.MUT101` | rule | [`MUT101`](rules/cached-property-stale-mutation.md) | \
             0.1.0 | - |"
        ),
        "{page}"
    );
    assert!(
        page.contains("| `cli-option.profile` | cli-option | `heki check --profile` | 0.1.1 | - |"),
        "{page}"
    );
    let index = docs.0.read("docs/rules/index.md");
    assert!(
        index.contains(&format!(
            " for.\n- \u{1f9ea} Experimental rule. {POLICY} See "
        )),
        "{index}"
    );
    let rule_page = docs.0.read("docs/rules/cached-property-stale-mutation.md");
    assert!(
        rule_page.contains("· Heuristic · Opt-in · Experimental · ")
            && rule_page.contains("!!! warning \"Experimental\"\n\n    Its precision"),
        "{rule_page}"
    );
    assert!(
        !docs
            .0
            .read("docs/rules/hash-attr-mutation.md")
            .contains("Experimental")
    );
    assert_eq!(docs.build_docs(&["--check"]).0, 0);

    let (code, out) = docs.check_with("docs/experimental.md", "0.1.0", "0.0.9");
    assert_eq!(code, 1);
    assert!(
        out.starts_with("Generated docs are out of date") && out.contains("experimental.md"),
        "{out}"
    );
}

#[test]
fn docs_check_fails_where_a_page_marks_a_rule_differently_from_the_registry() {
    let docs = DocsTree::new("experimental_marks");
    assert_eq!(docs.build_docs(&[]).0, 0);
    let heading =
        "Experimental features are marked differently from the registry (edit them by hand):";

    // The row of an experimental rule says so.
    let (code, out) = docs.check_with(
        "README.md",
        "| Opt-in, experimental: a `@cached_property` left stale",
        "| Opt-in: a `@cached_property` left stale",
    );
    assert_eq!(code, 1);
    assert!(
        out.starts_with(heading)
            && out.contains("README.md:")
            && out.contains(": the row of MUT101 does not say experimental"),
        "{out}"
    );
    // A rule named in a link, in the copy of the README on the site.
    let (code, out) = docs.check_with(
        "docs/index.md",
        "| Opt-in, experimental: a `@cached_property` left stale",
        "| Opt-in: a `@cached_property` left stale",
    );
    assert_eq!(code, 1);
    assert!(
        out.contains(": the row of MUT101 does not say experimental"),
        "{out}"
    );
    // The row of a stable rule does not, also among other rules in its cell.
    let (code, out) = docs.check_with(
        "README.md",
        "| A database query, or a new HTTP client, a loop can run on every iteration |",
        "| An experimental database query, or a new HTTP client, a loop can run on every iteration |",
    );
    assert_eq!(code, 1);
    assert!(
        out.contains(": the row of EFF102, EFF113 says experimental"),
        "{out}"
    );
    let (code, out) = docs.check_with(
        "ARCHITECTURE.md",
        "| **MUT101** | Opt-in (experimental, heuristic) |",
        "| **MUT101** | Opt-in (heuristic) |",
    );
    assert_eq!(code, 1);
    assert!(out.contains("ARCHITECTURE.md:"), "{out}");
    // A rule named in the text of a cell is not the row's rule.
    let (code, out) = docs.check_with(
        "ARCHITECTURE.md",
        "| **MUT106** | Opt-in (heuristic) |",
        "| **MUT106** | Opt-in (heuristic; see `MUT101`) |",
    );
    assert_eq!(code, 0, "{out}");

    // The changelog section of the release an entry names as `since` calls its target
    // experimental: a rule by its code, an option by the command and the option.
    let (code, out) = docs.check_with(
        "CHANGELOG.md",
        "(experimental: its precision",
        "(its precision",
    );
    assert_eq!(code, 1);
    assert!(
        out.starts_with(heading)
            && out.contains("CHANGELOG.md: [0.1.0] does not call MUT101 experimental"),
        "{out}"
    );
    let (code, out) = docs.check_with(
        "CHANGELOG.md",
        "- `MUT111` and `heki check --profile` are experimental:",
        "- `MUT111` is experimental, and so is the option:",
    );
    assert_eq!(code, 1);
    assert!(
        out.contains("CHANGELOG.md: [0.1.1] does not call heki check --profile experimental")
            && !out.contains("MUT111"),
        "{out}"
    );
}

#[test]
fn docs_check_fails_where_a_page_marks_a_rule_heuristic_differently_from_the_rules() {
    let docs = DocsTree::new("heuristic_marks");
    assert_eq!(docs.build_docs(&[]).0, 0);
    let heading = "Heuristic rules are marked differently from the rules (edit them by hand):";
    let index = docs.0.read("docs/rules/index.md");
    assert!(
        index.contains("| EFF101 | [async-blocking-call](async-blocking-call.md) | ")
            && index.contains(" | ≈ |\n")
            && index.contains(" | ≈ 📜 |\n")
            && index.contains(" | ≈ 🔌 🧪 |\n"),
        "{index}"
    );
    let page = docs.0.read("docs/rules/async-blocking-call.md");
    assert!(
        page.contains("· Heuristic · Off by default · ")
            && page.contains("!!! note \"Heuristic\"\n\n    This rule is heuristic: ")
            && page.contains("Of 50 sampled reports (round 3, 2026-11-21), 82% were right"),
        "{page}"
    );
    let contract = docs.0.read("docs/rules/write-owner-violation.md");
    assert!(
        contract.contains("· Heuristic · Runs when its contract is declared · ")
            && contract
                .contains("It runs when its contract is declared. It has not been measured yet."),
        "{contract}"
    );

    // The row of a heuristic rule says so, in the guides, the README, and the measurements.
    for (rel, old, new) in [
        (
            "README.md",
            "| A blocking call that a call in an `async def` always runs, several helpers below | `EFF101` | heuristic |",
            "| A blocking call that a call in an `async def` always runs, several helpers below | `EFF101` | exact |",
        ),
        (
            "docs/precision.md",
            "| `EFF101` | heuristic | round 3",
            "| `EFF101` | exact | round 3",
        ),
        (
            "ARCHITECTURE.md",
            "| **EFF101** | Off (heuristic) |",
            "| **EFF101** | On |",
        ),
    ] {
        let (code, out) = docs.check_with(rel, old, new);
        assert_eq!(code, 1, "{rel}");
        assert!(
            out.starts_with(heading)
                && out.contains(&format!("{rel}:"))
                && out.contains(": the row of EFF101 does not say heuristic"),
            "{rel}: {out}"
        );
    }
    // A heuristic rule's measurement is read into its page: a stale page fails.
    let (code, out) = docs.check_with(
        "docs/precision.md",
        "| 50 | 3 / 13 / 25 / 9 | 82% |",
        "| 50 | 3 / 13 / 26 / 8 | 84% |",
    );
    assert_eq!(code, 1);
    assert!(
        out.starts_with("Generated docs are out of date") && out.contains("async-blocking-call.md"),
        "{out}"
    );
}
