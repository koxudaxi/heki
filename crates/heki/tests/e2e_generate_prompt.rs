//! `heki check --generate-prompt` end to end: the Markdown prompt and its JSON object, which
//! hold the configuration, the rules that apply, the diagnostics of the run, the question, and
//! how to answer; the JSON Schema of the object; the cap on the diagnostics; and a run that
//! writes nothing.

mod common;

use common::{Project, schema};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const API: &str = "import time


def load():
    time.sleep(1)


async def handler():
    load()


def build(xs):
    out = []
    for x in xs:
        out = out + [x]
    return out
";

const STORE: &str = "class Store:
    def __init__(self):
        self.items = []


def add(store: Store):
    store.items.append(1)
";

/// An `OPT101` report whose rewrite is not proven: a heuristic report of an exact rule.
const BLOCKED: &str = "def build(items, sink):
    x = []
    for i in items:
        x = x + [i]
        sink(x)
    return x
";

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki]
src = ["src"]

[tool.heki.lint]
extend-select = ["EFF105"]
ignore = ["OPT110"]
fail-on = "warning"

[tool.heki.lint.per-file-ignores]
"tests/*" = ["EFF101"]

[[tool.heki.write-owners]]
name = "store"
targets = ["app.store.Store.items"]
allowed-writers = ["Store.*"]
message = "use Store.add()"

[[tool.heki.readonly-returns]]
name = "shared"
producers = ["app.store.shared"]
"#;

/// A project with a source root, a selection, two contracts, and two diagnostics in `app/api.py`.
fn configured(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG)
        .write("src/app/__init__.py", "")
        .write("src/app/api.py", API)
        .write("src/app/store.py", STORE);
    p
}

/// `heki check <args> --generate-prompt [question]`, which must exit with 0: stdout.
fn prompt(p: &Project, args: &[&str]) -> String {
    let all = [&["check"][..], args].concat();
    let (code, out, err) = p.heki(&all);
    assert_eq!(code, 0, "{out}\n{err}");
    out
}

/// The JSON object of the prompt, validated against the schema the binary prints.
fn prompt_json(p: &Project, args: &[&str]) -> Value {
    let all = [args, &["--output-format", "json"]].concat();
    let out = prompt(p, &all);
    let object: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let errors = schema::errors(&schema(p), &object);
    assert!(errors.is_empty(), "{errors:#?}\n{object:#}");
    object
}

fn schema(p: &Project) -> Value {
    let (code, out, err) = p.heki(&["check", "--output-format-json-schema", "generate-prompt"]);
    assert_eq!((code, err.as_str()), (0, ""));
    serde_json::from_str(&out).unwrap()
}

/// The `concise` lines of `heki check <args>`, as the user runs it.
fn concise(p: &Project, args: &[&str]) -> Vec<String> {
    let all = [&["check", "--output-format", "concise"][..], args].concat();
    let (_, out, _) = p.heki(&all);
    out.lines().map(str::to_owned).collect()
}

/// The section of the Markdown prompt under `## heading`, up to the next `## `.
fn section<'t>(text: &'t str, heading: &str) -> &'t str {
    let start = text
        .find(&format!("\n## {heading}\n\n"))
        .unwrap_or_else(|| panic!("no `## {heading}`: {text}"));
    let body = &text[start + heading.len() + 6..];
    body.find("\n## ").map_or(body, |end| &body[..end])
}

/// Every rule of `heki rule --all --output-format json`, by code.
fn rule_records(p: &Project) -> Vec<Value> {
    let (code, out, _) = p.heki(&["rule", "--all", "--output-format", "json"]);
    assert_eq!(code, 0);
    serde_json::from_str(&out).unwrap()
}

/// The first paragraph of a rule's "What a report states", on one line.
fn first_claim_paragraph(doc: &str) -> String {
    let (_, rest) = doc.split_once("\n## What a report states\n").unwrap();
    let paragraph = rest.trim_start().split("\n\n").next().unwrap();
    paragraph.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_prompt_holds_the_configuration_rules_diagnostics_question_and_guidance() {
    let p = configured("prompt_text");
    let text = prompt(
        &p,
        &[
            "--generate-prompt",
            "Why is handler reported, and where does the fix go?",
        ],
    );
    assert!(
        text.starts_with("# Consulting a model about heki\n\n"),
        "{text}"
    );
    assert_eq!(
        section(&text, "Question"),
        "Why is handler reported, and where does the fix go?\n"
    );
    assert_eq!(
        section(&text, "Configuration"),
        r#"- Project root: `.`
- Configuration file: `pyproject.toml`
- Target Python version: `py312`
- Source roots: `src` (the `src` setting)
- `select`: the default rules
- `extend-select`: `EFF105`
- `ignore`: `OPT110`
- `per-file-ignores`: `tests/*`: `EFF101`
- `lint.heuristic`: `false`
- `fail-on`: `warning`
- Baseline: none

### Contracts

```toml
[[tool.heki.readonly-returns]]
name = "shared"
deep = false
producers = ["app.store.shared"]

[[tool.heki.write-owners]]
name = "store"
allowed-writers = ["Store.*"]
message = "use Store.add()"
targets = ["app.store.Store.items"]
```
"#
    );

    // Each rule that applies, with what `heki rule` says of it.
    let rules = section(&text, "Rules that apply");
    for r in rule_records(&p) {
        let code = r["code"].as_str().unwrap();
        let line = format!(
            "- `{code}` (`{}`, {}, {}): {}\n",
            r["slug"].as_str().unwrap(),
            r["certainty"].as_str().unwrap(),
            r["activation"].as_str().unwrap(),
            first_claim_paragraph(r["doc"].as_str().unwrap())
        );
        // `ignore` removes `OPT110`; `EFF105` is opt-in and selected; a heuristic rule that is
        // not opt-in runs only with `lint.heuristic`.
        let applies = match code {
            "OPT110" => false,
            "EFF105" => true,
            _ => r["default_enabled"] == true,
        };
        assert_eq!(rules.contains(&line), applies, "{code}: {rules}");
    }
    assert!(
        rules.ends_with(
            "\nThese heuristic rules are off; `--heuristic` (`lint.heuristic = true`) runs them: \
             `EFF102`, `EFF111`, `EFF113`, `MUT118`, `OPT112`.\n"
        ),
        "{rules}"
    );

    // The diagnostics as `heki check --output-format concise` prints them: `EFF101` and
    // `OPT101` in `app/api.py`, and the contract's `MUT201` in `app/store.py`.
    let lines = concise(&p, &[]);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(
        section(&text, "Diagnostics"),
        format!(
            "`heki check` reported 3 diagnostic(s) in 3 file(s):\n\n```text\n{}\n```\n",
            lines.join("\n")
        )
    );
    let guidance = section(&text, "How to answer");
    for words in [
        "at its root cause",
        "Do not silence a diagnostic",
        "A `heuristic` diagnostic can be wrong about the code",
        "Rely on proven fixes only",
        "write a contract",
        "https://heki.koxudaxi.dev/configuration/#contracts",
    ] {
        assert!(guidance.contains(words), "{words}: {guidance}");
    }

    // Any format but JSON prints the Markdown prompt; the paths come before the question.
    let concise_format = prompt(
        &p,
        &[
            "src",
            "--output-format",
            "concise",
            "--generate-prompt",
            "Why is handler reported, and where does the fix go?",
        ],
    );
    assert_eq!(concise_format, text);
}

#[test]
fn the_json_object_holds_the_same_content_and_follows_its_schema() {
    let p = configured("prompt_json");
    let question = "Why is handler reported?";
    let object = prompt_json(&p, &["--generate-prompt", question]);
    let text = prompt(&p, &["--generate-prompt", question]);
    assert_eq!(object["version"], 1);
    assert_eq!(object["question"], question);
    assert_eq!(
        object["configuration"],
        json!({
            "root": ".",
            "config_file": "pyproject.toml",
            "target_version": "py312",
            "source_roots": ["src"],
            "source_roots_from": "src",
            "selection": {
                "select": null,
                "extend_select": ["EFF105"],
                "ignore": ["OPT110"],
                "per_file_ignores": {"tests/*": ["EFF101"]},
                "heuristic": false,
                "fail_on": "warning",
            },
            "baseline": null,
            "contracts": {
                "call-owners": [],
                "effect-boundaries": [],
                "invalidation": [],
                "read-owners": [],
                "readonly-returns": [
                    {"name": "shared", "producers": ["app.store.shared"], "deep": false},
                ],
                "write-owners": [{
                    "name": "store",
                    "targets": ["app.store.Store.items"],
                    "allowed-writers": ["Store.*"],
                    "message": "use Store.add()",
                }],
            },
        })
    );
    // Run from below the project root, the root is a path outside the working directory, which
    // `heki check` prints as it is.
    let out = common::heki_command()
        .args(["check", "--generate-prompt", "--output-format", "json"])
        .current_dir(p.dir.join("src"))
        .output()
        .unwrap();
    let below: Value = serde_json::from_slice(&out.stdout).unwrap();
    let root = below["configuration"]["root"].as_str().unwrap();
    let name = p.dir.file_name().unwrap().to_str().unwrap();
    assert!(root.ends_with(name) && root != name, "{root}");
    // The rules of the text, in its order, with the same facts.
    let rules = section(&text, "Rules that apply");
    let listed: Vec<String> = object["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "- `{}` (`{}`, {}, {}): {}",
                r["code"].as_str().unwrap(),
                r["slug"].as_str().unwrap(),
                r["certainty"].as_str().unwrap(),
                r["activation"].as_str().unwrap(),
                r["claim"].as_str().unwrap()
            )
        })
        .collect();
    let text_rules: Vec<&str> = rules.lines().filter(|l| l.starts_with("- `")).collect();
    assert_eq!(listed, text_rules);
    assert_eq!(
        object["heuristic_rules_off"],
        json!(["EFF102", "EFF111", "EFF113", "MUT118", "OPT112"])
    );
    let lines = concise(&p, &[]);
    let diagnostics = &object["diagnostics"];
    let shown: Vec<String> = diagnostics["shown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            let label = if d["certainty"] == "heuristic" {
                " [heuristic]"
            } else {
                ""
            };
            format!(
                "{}:{}:{}: {}{label} {}",
                d["path"].as_str().unwrap(),
                d["line"],
                d["column"],
                d["rule_code"].as_str().unwrap(),
                d["message"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(shown, lines);
    assert_eq!(
        (
            &diagnostics["files_checked"],
            &diagnostics["total"],
            &diagnostics["omitted"],
            &diagnostics["limit_bytes"],
            &diagnostics["heuristic_reports_withheld"],
            &diagnostics["files_not_analyzed"],
        ),
        (
            &json!(3),
            &json!(3),
            &json!(0),
            &json!(16384),
            &json!(0),
            &json!([])
        )
    );
    let guidance = section(&text, "How to answer");
    let items: Vec<String> = object["guidance"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| format!("- {}", g.as_str().unwrap()))
        .collect();
    assert_eq!(guidance.trim_end(), items.join("\n"));

    // The schema check fails an object the schema does not describe.
    let schema = schema(&p);
    let mut broken = object.clone();
    broken["diagnostics"]["shown"][0]["line"] = json!("9");
    broken["rules"][0]["activation"] = json!("sometimes");
    broken["configuration"]["extra"] = json!(true);
    broken.as_object_mut().unwrap().remove("guidance");
    assert_eq!(
        schema::errors(&schema, &broken),
        [
            "/: no `guidance`",
            "/configuration: `extra` is not allowed",
            "/diagnostics/shown/0/line: \"9\" is not of type integer",
            "/rules/0/activation: \"sometimes\" is not one of [\"default\",\"opt-in\",\"contract\"]",
        ]
    );
}

#[test]
fn a_run_without_a_question_contracts_configuration_or_diagnostics_says_so() {
    let p = Project::new("prompt_bare");
    std::fs::remove_file(p.dir.join("pyproject.toml")).unwrap();
    p.write("src/pkg/__init__.py", "")
        .write("src/pkg/ok.py", "def f(x):\n    return x\n");
    let text = prompt(&p, &["--generate-prompt", "--heuristic"]);
    assert_eq!(
        section(&text, "Question"),
        "No question was given: explain what the diagnostics below mean and how to fix each one \
         at its root.\n"
    );
    let configuration = section(&text, "Configuration");
    for line in [
        "- Configuration file: none (the defaults)\n",
        "- Source roots: `src` (detected)\n",
        "- `lint.heuristic`: `true`\n",
        "- `fail-on`: `error`\n",
        "\n### Contracts\n\nNo contract is declared.\n",
    ] {
        assert!(configuration.contains(line), "{line}: {configuration}");
    }
    // With `--heuristic`, no heuristic rule is off.
    assert!(
        !section(&text, "Rules that apply").contains("are off"),
        "{text}"
    );
    assert_eq!(
        section(&text, "Diagnostics"),
        "`heki check` reported no diagnostic in 2 file(s).\n"
    );
    let object = prompt_json(&p, &["--generate-prompt", ""]);
    assert_eq!(object["question"], Value::Null);
    assert_eq!(object["configuration"]["config_file"], Value::Null);
    assert_eq!(object["configuration"]["source_roots_from"], "detected");

    // A project whose modules are named from its root.
    let p = Project::new("prompt_flat");
    p.write("app.py", "def f(x):\n    return x\n");
    let text = prompt(
        &p,
        &["--generate-prompt", "--select", "EFF101,OPT101,EFF105"],
    );
    assert!(
        text.contains(
            "- Source roots: none: module names start at the project root\n- `select`: \
             `EFF101`, `OPT101`, `EFF105`\n"
        ),
        "{text}"
    );
    let object = prompt_json(
        &p,
        &["--generate-prompt", "--select", "EFF101,OPT101,EFF105"],
    );
    let codes: Vec<&str> = object["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["EFF101", "EFF105", "OPT101"]);
}

#[test]
fn the_diagnostics_are_capped_with_a_note_counting_the_rest() {
    let p = Project::new("prompt_cap");
    let functions: String = (0..400)
        .map(|i| {
            format!(
                "def build_{i}(xs):\n    out = []\n    for x in xs:\n        out = out + [x]\n    return out\n\n\n"
            )
        })
        .collect();
    p.write("app.py", &functions);
    let lines = concise(&p, &[]);
    assert_eq!(lines.len(), 400);
    let object = prompt_json(&p, &["--generate-prompt"]);
    let diagnostics = &object["diagnostics"];
    let shown = diagnostics["shown"].as_array().unwrap().len();
    let omitted = diagnostics["omitted"].as_u64().unwrap() as usize;
    assert_eq!(
        (diagnostics["total"].as_u64(), shown + omitted),
        (Some(400), 400)
    );
    // Every line that fits, and not one more.
    let bytes = |n: usize| lines[..n].iter().map(|l| l.len() + 1).sum::<usize>();
    assert!(bytes(shown) <= 16384 && bytes(shown + 1) > 16384, "{shown}");

    let text = prompt(&p, &["--generate-prompt"]);
    let section = section(&text, "Diagnostics");
    assert_eq!(
        section,
        format!(
            "`heki check` reported 400 diagnostic(s) in 1 file(s):\n\n```text\n{}\n```\n\n{omitted} \
             more diagnostic(s) are not shown: the prompt holds 16384 bytes of them. `heki check \
             --output-format concise` lists every one.\n",
            lines[..shown].join("\n")
        )
    );
}

#[test]
fn a_baseline_withheld_heuristic_reports_and_files_heki_cannot_parse_are_said() {
    let p = Project::new("prompt_baseline");
    p.write("app.py", API);
    let (code, _, err) = p.heki(&["check", "--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    p.write("more.py", API)
        .write("blocked.py", BLOCKED)
        .write("broken.py", "def f(:\n");
    let text = prompt(&p, &["--baseline", "b.toml", "--generate-prompt"]);
    let configuration = section(&text, "Configuration");
    assert!(
        configuration
            .contains("- Baseline: `b.toml` (only the reports it does not allow are shown)\n"),
        "{configuration}"
    );
    let regressions = concise(&p, &["--baseline", "b.toml"]);
    assert!(
        regressions.len() == 2 && regressions.iter().all(|l| l.starts_with("more.py:")),
        "{regressions:?}"
    );
    let diagnostics = section(&text, "Diagnostics");
    assert!(
        diagnostics.starts_with(&format!(
            "`heki check` reported 2 diagnostic(s) in 3 file(s), among the reports the baseline \
             does not allow:\n\n```text\n{}\n```\n",
            regressions.join("\n")
        )),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains(
            "\nnote: 1 heuristic report(s) are not shown; enable them with `--heuristic` or \
             `lint.heuristic = true`\n"
        ),
        "{diagnostics}"
    );
    assert!(
        diagnostics.ends_with(
            "\nheki could not analyze these files, so nothing they do is known:\n\n- broken.py:1:7: \
             syntax error: Expected a parameter or the end of the parameter list (2 error(s))\n"
        ),
        "{diagnostics}"
    );
    let object = prompt_json(&p, &["--baseline", "b.toml", "--generate-prompt"]);
    assert_eq!(object["configuration"]["baseline"], "b.toml");
    assert_eq!(object["diagnostics"]["heuristic_reports_withheld"], 1);
    let not_analyzed = object["diagnostics"]["files_not_analyzed"]
        .as_array()
        .unwrap();
    assert!(
        not_analyzed.len() == 1
            && not_analyzed[0]
                .as_str()
                .unwrap()
                .starts_with("broken.py:1:"),
        "{not_analyzed:?}"
    );
}

/// Every file below `dir` with its contents.
fn tree(dir: &std::path::Path) -> BTreeSet<(String, Vec<u8>)> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(tree(&path));
        } else {
            out.insert((path.display().to_string(), std::fs::read(&path).unwrap()));
        }
    }
    out
}

#[test]
fn the_prompt_writes_no_file_and_does_not_combine_with_the_options_that_do() {
    let p = configured("prompt_no_writes");
    p.write(
        "pyproject.toml",
        &format!("{CONFIG}\n[tool.heki.lint.severity]\nOPT101 = \"error\"\n"),
    );
    let before = tree(&p.dir);
    prompt(&p, &["--generate-prompt", "--extend-select", "MUT103"]);
    prompt_json(&p, &["--generate-prompt"]);
    assert_eq!(tree(&p.dir), before);
    for option in [
        "--fix",
        "--diff",
        "--watch",
        "--update-baseline",
        "--include-size-dependent",
    ] {
        let (code, out, err) = p.heki(&["check", option, "--generate-prompt"]);
        assert_eq!((code, out.as_str()), (2, ""), "{option}: {err}");
        assert!(err.contains("cannot be used with"), "{option}: {err}");
    }
    let (code, _, err) = p.heki(&["check", "--profile", "x.collapsed", "--generate-prompt"]);
    assert!(code == 2 && err.contains("cannot be used with"), "{err}");
    assert_eq!(tree(&p.dir), before);
    // A configuration error is an error, as for `heki check`.
    let (code, out, err) = p.heki(&["check", "--select", "NOPE1", "--generate-prompt"]);
    assert_eq!((code, out.as_str()), (2, ""), "{err}");
    assert!(err.contains("unknown rule selector `NOPE1`"), "{err}");
}
