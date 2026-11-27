//! The configuration fold: the keys a `<contract>-defaults` table sets apply to every table of
//! its contract kind that does not set them, the files `include` lists join the main file, and an
//! `@name` entry stands for the entries of its group, so a configuration reports what the same
//! configuration written out in one file reports (end to end, through the real binary).

mod common;

use common::Project;
use serde_json::Value;

const MODULE: &str = r#"import json
import time


class Model:
    def __init__(self) -> None:
        self.fields: list[int] = []
        self.secret = ""
        self.count = 0


SHARED: list[int] = []


def shared() -> list[int]:
    return SHARED


def refresh(m: Model) -> None:
    pass


def helper() -> None:
    pass


def ok_write(m: Model) -> None:
    m.fields = [1]
    m.secret = "s"


def bad_write(m: Model) -> None:
    m.fields = [2]
    m.secret = "t"


def ok_read(m: Model) -> str:
    return m.secret + str(m.fields)


def bad_read(m: Model) -> str:
    return m.secret + str(m.fields)


def ok_call() -> None:
    helper()


def bad_call() -> None:
    helper()


def napper() -> None:
    time.sleep(1)


def napper_too() -> None:
    time.sleep(2)


def mutate_shared() -> None:
    shared().append(1)


def bump(m: Model) -> None:
    m.count = 1


def dump(x: object) -> str:
    return json.dumps(x)
"#;

const OTHER: &str = "import json\n\n\ndef load(x: str) -> object:\n    return json.loads(x)\n";

type Keys = &'static [(&'static str, &'static str)];

/// A contract kind: the keys its defaults table sets, and its tables with the keys each sets.
struct Kind {
    key: &'static str,
    defaults: Keys,
    tables: &'static [Keys],
}

const KINDS: &[Kind] = &[
    Kind {
        key: "write-owners",
        defaults: &[
            ("allowed-writers", r#"["pkg.m:ok_write"]"#),
            ("message", r#""write it through the store""#),
            ("attribution", r#""lexical""#),
        ],
        tables: &[
            &[
                ("name", r#""w1""#),
                ("targets", r#"["pkg.m:Model.fields"]"#),
            ],
            &[
                ("name", r#""w2""#),
                ("targets", r#"["pkg.m:Model.secret"]"#),
                ("message", r#""w2 has its own help""#),
            ],
        ],
    },
    Kind {
        key: "read-owners",
        defaults: &[
            ("allowed-readers", r#"["pkg.m:ok_read"]"#),
            ("message", r#""read it through the store""#),
        ],
        tables: &[
            &[
                ("name", r#""r1""#),
                ("targets", r#"["pkg.m:Model.secret"]"#),
            ],
            &[
                ("name", r#""r2""#),
                ("targets", r#"["pkg.m:Model.fields"]"#),
                ("allowed-readers", r#"["pkg.m:bad_read"]"#),
            ],
        ],
    },
    Kind {
        key: "call-owners",
        defaults: &[
            ("allowed-callers", r#"["pkg.m:ok_call"]"#),
            ("attribution", r#""lexical""#),
            ("subclasses", "false"),
        ],
        tables: &[&[("name", r#""c1""#), ("targets", r#"["pkg.m:helper"]"#)]],
    },
    Kind {
        key: "effect-boundaries",
        defaults: &[
            ("forbid", r#"["sleep"]"#),
            ("message", r#""do not sleep here""#),
        ],
        tables: &[
            &[("name", r#""e1""#), ("scopes", r#"["pkg.m:napper"]"#)],
            &[
                ("name", r#""e2""#),
                ("scopes", r#"["pkg.m:napper_too"]"#),
                ("message", r#""e2 has its own help""#),
            ],
        ],
    },
    Kind {
        key: "readonly-returns",
        defaults: &[("deep", "true")],
        tables: &[&[("name", r#""ro""#), ("producers", r#"["pkg.m:shared"]"#)]],
    },
    Kind {
        key: "invalidation",
        defaults: &[
            ("invalidators", r#"["pkg.m:refresh"]"#),
            ("message", r#""call refresh() after""#),
        ],
        tables: &[&[("name", r#""inv""#), ("targets", r#"["Model.count"]"#)]],
    },
    Kind {
        key: "import-boundaries",
        defaults: &[
            ("forbidden-imports", r#"["json"]"#),
            ("message", r#""use the codec module""#),
        ],
        tables: &[
            &[("name", r#""i1""#), ("importers", r#"["pkg.m"]"#)],
            &[("name", r#""i2""#), ("importers", r#"["pkg.n"]"#)],
        ],
    },
];

fn lines(keys: &[(&str, &str)]) -> String {
    keys.iter().map(|(k, v)| format!("{k} = {v}\n")).collect()
}

/// The tables of `kind`, each with the defaults it leaves out written in.
fn expanded(kind: &Kind) -> String {
    let mut out = String::new();
    for table in kind.tables {
        let mut keys: Vec<(&str, &str)> = table.to_vec();
        for default in kind.defaults {
            if !table.iter().any(|(k, _)| *k == default.0) {
                keys.push(*default);
            }
        }
        out.push_str(&format!("[[tool.heki.{}]]\n{}\n", kind.key, lines(&keys)));
    }
    out
}

/// The tables of `kind` without the defaults.
fn bare(kind: &Kind) -> String {
    (kind.tables.iter())
        .map(|table| format!("[[tool.heki.{}]]\n{}\n", kind.key, lines(table)))
        .collect()
}

/// The tables of `kind` as they are written with a defaults table.
fn with_defaults_table(kind: &Kind) -> String {
    format!(
        "[tool.heki.{}-defaults]\n{}\n{}",
        kind.key,
        lines(kind.defaults),
        bare(kind)
    )
}

fn project(name: &str, config: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    );
    p.write("pkg/__init__.py", "");
    p.write("pkg/m.py", MODULE);
    p.write("pkg/n.py", OTHER);
    p
}

/// Every report of `heki check`, as the JSON it prints.
fn reports(p: &Project) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{code}: {stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

/// `qualname target` of the reports of `code`, sorted.
fn of_rule(reports: &[Value], code: &str) -> Vec<String> {
    let mut found: Vec<String> = reports
        .iter()
        .filter(|d| d["rule_code"] == code)
        .map(|d| format!("{} {}", d["qualname"].as_str().unwrap(), d["target"]))
        .collect();
    found.sort();
    found
}

/// The JSON of `json` without the project directory it names, which differs between two projects.
fn without_root(json: &str) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("root");
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut value: Value = serde_json::from_str(json).unwrap();
    strip(&mut value);
    value
}

/// A configuration that fails to load: the exit code, and what `heki check` says.
fn checked(config: &str) -> (i32, String) {
    let p = project("defaults_checked", config);
    let (code, stdout, stderr) = p.check(&[]);
    (code, format!("{stdout}{stderr}"))
}

#[test]
fn every_contract_kind_has_a_defaults_table_and_the_reports_are_the_expanded_tables() {
    let mut kinds: Vec<&str> = KINDS.iter().map(|k| k.key).collect();
    let mut declared = heki_env::HekiConfig::CONTRACT_KEYS.to_vec();
    kinds.sort_unstable();
    declared.sort_unstable();
    assert_eq!(kinds, declared);
    let written_out: String = KINDS.iter().map(expanded).collect();
    let with_defaults: String = KINDS.iter().map(with_defaults_table).collect();
    let longhand = reports(&project("defaults_expanded", &written_out));
    let shorthand = reports(&project("defaults_table", &with_defaults));
    assert_eq!(shorthand, longhand);
    for code in [
        "MUT201", "MUT203", "MUT204", "EFF201", "MUT202", "MUT208", "IMP201",
    ] {
        assert!(
            !of_rule(&longhand, code).is_empty(),
            "{code}: {longhand:#?}"
        );
    }
    // A defaults table matters: the tables alone report something else.
    let tables_alone: String = KINDS.iter().map(bare).collect();
    assert_ne!(
        reports(&project("defaults_absent", &tables_alone)),
        longhand
    );
}

#[test]
fn each_kind_alone_reports_what_its_expanded_tables_report() {
    for kind in KINDS {
        let longhand = reports(&project("defaults_one_expanded", &expanded(kind)));
        let shorthand = reports(&project("defaults_one", &with_defaults_table(kind)));
        assert!(!longhand.is_empty(), "{}", kind.key);
        assert_eq!(shorthand, longhand, "{}", kind.key);
    }
}

#[test]
fn a_tables_own_key_overrides_the_default_and_a_list_replaces_it_whole() {
    let config = r#"[tool.heki.write-owners-defaults]
allowed-writers = ["pkg.m:ok_write"]

[[tool.heki.write-owners]]
name = "default"
targets = ["pkg.m:Model.fields"]

[[tool.heki.write-owners]]
name = "own"
targets = ["pkg.m:Model.secret"]
allowed-writers = ["pkg.m:bad_write"]
"#;
    let found = reports(&project("defaults_override", config));
    // `default` allows `ok_write` only; `own` allows `bad_write` only, not both.
    assert_eq!(
        of_rule(&found, "MUT201"),
        [
            "bad_write \"pkg.m:Model.fields\"",
            "ok_write \"pkg.m:Model.secret\""
        ]
    );
}

#[test]
fn an_empty_list_of_a_table_is_a_list_of_its_own() {
    let config = r#"[tool.heki.write-owners-defaults]
allowed-writers = ["pkg.m:ok_write"]

[[tool.heki.write-owners]]
name = "nobody"
targets = ["pkg.m:Model.fields"]
allowed-writers = []
"#;
    let found = reports(&project("defaults_empty_list", config));
    assert_eq!(
        of_rule(&found, "MUT201"),
        [
            "bad_write \"pkg.m:Model.fields\"",
            "ok_write \"pkg.m:Model.fields\""
        ]
    );
}

#[test]
fn a_defaults_table_without_tables_of_its_kind_is_no_error() {
    let (code, out) = checked("[tool.heki.call-owners-defaults]\nattribution = \"lexical\"\n");
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_heki_toml_holds_the_defaults_tables_at_its_top_level() {
    let p = project("defaults_heki_toml", "");
    p.write(
        "heki.toml",
        "[lint]\nselect = [\"EFF201\"]\n\n[effect-boundaries-defaults]\nforbid = [\"sleep\"]\n\n[[effect-boundaries]]\nname = \"e\"\nscopes = [\"pkg.m:napper\"]\n",
    );
    let found = reports(&p);
    assert_eq!(of_rule(&found, "EFF201").len(), 1, "{found:#?}");
}

#[test]
fn inspect_and_recommend_read_the_effective_values() {
    let written_out: String = KINDS.iter().map(expanded).collect();
    let with_defaults: String = KINDS.iter().map(with_defaults_table).collect();
    let longhand = project("defaults_inspect_expanded", &written_out);
    let shorthand = project("defaults_inspect_table", &with_defaults);
    assert_inspect_and_recommend_agree(&longhand, &shorthand);
}

/// `heki inspect` and `heki recommend` print the same JSON for both projects.
fn assert_inspect_and_recommend_agree(longhand: &Project, shorthand: &Project) {
    for symbol in [
        "pkg.m.bad_write",
        "pkg.m.napper",
        "pkg.m.bump",
        "pkg.m.dump",
    ] {
        let args = ["inspect", "--heuristic", "--output-format", "json", symbol];
        let (code, expected, err) = longhand.heki(&args);
        assert_eq!(code, 0, "{symbol}: {expected}{err}");
        let (code, found, err) = shorthand.heki(&args);
        assert_eq!(code, 0, "{symbol}: {found}{err}");
        assert_eq!(without_root(&found), without_root(&expected), "{symbol}");
    }
    let args = ["recommend", "--output-format", "json"];
    let (code, expected, err) = longhand.heki(&args);
    assert_eq!(code, 0, "{expected}{err}");
    let (code, found, err) = shorthand.heki(&args);
    assert_eq!(code, 0, "{found}{err}");
    assert_eq!(without_root(&found), without_root(&expected));
}

#[test]
fn name_and_targets_cannot_be_defaulted() {
    for (key, value) in [("name", "\"x\""), ("targets", "[\"pkg.m:helper\"]")] {
        let config = format!(
            "[tool.heki.call-owners-defaults]\nattribution = \"lexical\"\n{key} = {value}\n"
        );
        let (code, out) = checked(&config);
        assert_eq!(code, 2, "{out}");
        assert!(
            out.contains(&format!(
                "unknown key `{key}` in [tool.heki.call-owners-defaults]"
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "`{key}` names one contract and cannot be defaulted: set it in each table of [[tool.heki.call-owners]]"
            )),
            "{out}"
        );
        assert!(
            out.contains("pyproject.toml:6:1"),
            "the error shows the line: {out}"
        );
        assert!(out.contains("configuration/#call-owners"), "{out}");
    }
}

#[test]
fn an_unknown_key_of_a_defaults_table_names_the_keys_it_takes() {
    let (code, out) = checked("[tool.heki.write-owners-defaults]\nallowed-writer = [\"x\"]\n");
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("unknown key `allowed-writer` in [tool.heki.write-owners-defaults]"),
        "{out}"
    );
    assert!(out.contains("did you mean `allowed-writers`"), "{out}");
    let (code, out) = checked("[tool.heki.write-owners-defaults]\nallowed-readers = [\"x\"]\n");
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("`allowed-readers` is a key of [[tool.heki.read-owners]]: move it there"),
        "{out}"
    );
}

#[test]
fn a_value_the_key_does_not_take_is_found_in_the_defaults_table() {
    let (code, out) = checked("[tool.heki.write-owners-defaults]\nallowed-writers = \"x\"\n");
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("invalid `allowed-writers` in [tool.heki.write-owners-defaults]"),
        "{out}"
    );
    assert!(
        out.contains(
            "`allowed-writers` takes an array, for example: `allowed-writers = [\"pkg.parser.generation.*\"]`"
        ),
        "{out}"
    );
    assert!(out.contains("configuration/#write-owners"), "{out}");
}

#[test]
fn a_defaults_table_is_one_table() {
    let (code, out) = checked("[[tool.heki.write-owners-defaults]]\nmessage = \"x\"\n");
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains(
            "`write-owners-defaults` takes a table, for example: `[tool.heki.write-owners-defaults]`"
        ),
        "{out}"
    );
}

#[test]
fn a_defaults_table_at_the_top_level_of_a_pyproject_is_misplaced() {
    let p = Project::new("defaults_top_level");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[call-owners-defaults]\nattribution = \"lexical\"\n",
    );
    let (code, out, err) = p.check(&[]);
    assert_eq!(code, 2, "{out}{err}");
    assert!(
        err.contains("`call-owners-defaults` at the top level of a `pyproject.toml`"),
        "{err}"
    );
}

#[test]
fn a_default_a_contract_cannot_use_is_reported_at_the_defaults_table() {
    let config = "[tool.heki.write-owners-defaults]\nallowed-writers = [\"Store.[*\"]\n\n[[tool.heki.write-owners]]\nname = \"s\"\ntargets = [\"pkg.m:Model.fields\"]\n";
    let (code, out) = checked(config);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("invalid glob `Store.[*` in write-owners.allowed-writers"),
        "{out}"
    );
    assert!(out.contains("pyproject.toml:5:20"), "{out}");
}

/// `text`, written for a `pyproject.toml`, as a file `include` lists writes it: at its top level.
fn at_top_level(text: &str) -> String {
    text.replace("[tool.heki.", "[")
}

/// The table of `kind` at `position`, without the defaults.
fn kind_table(kind: &Kind, position: usize) -> String {
    format!(
        "[[tool.heki.{}]]\n{}\n",
        kind.key,
        lines(kind.tables[position])
    )
}

/// The tables of `kind` from position `from` on, without the defaults.
fn tables_from(kind: &Kind, from: usize) -> String {
    (kind.tables[from..].iter())
        .map(|table| format!("[[tool.heki.{}]]\n{}\n", kind.key, lines(table)))
        .collect()
}

/// The configuration of [`KINDS`] in three files, with groups: the main file holds the
/// `write-owners` defaults and first table and the `read-owners`, a group file the rest of the
/// `write-owners` (with the main file's defaults) and two more kinds, and a third file the other
/// kinds and the group `napper`, which the second file uses.
fn split_project(name: &str) -> Project {
    let [write, read, call, effect, readonly, invalidation, import] = KINDS else {
        panic!("seven contract kinds");
    };
    let main = format!(
        "[tool.heki]\ninclude = [\"contracts/owners.toml\", \"contracts/./boundaries.toml\"]\n\n\
         [tool.heki.groups]\nok-writers = [\"pkg.m:ok_write\"]\nreaders = [\"@ok-readers\"]\n\n\
         [tool.heki.write-owners-defaults]\n{}\n{}{}",
        lines(write.defaults).replace("[\"pkg.m:ok_write\"]", "[\"@ok-writers\"]"),
        kind_table(write, 0),
        with_defaults_table(read).replace("[\"pkg.m:ok_read\"]", "[\"@readers\"]"),
    );
    let owners = at_top_level(&format!(
        "{}{}{}",
        tables_from(write, 1),
        with_defaults_table(call),
        with_defaults_table(effect).replace("[\"pkg.m:napper\"]", "[\"@napper\"]"),
    ));
    let boundaries = at_top_level(&format!(
        "[tool.heki.groups]\nnapper = [\"pkg.m:napper\"]\nok-readers = [\"pkg.m:ok_read\"]\n\n{}{}{}",
        with_defaults_table(readonly),
        with_defaults_table(invalidation),
        with_defaults_table(import),
    ));
    let p = project(name, &main);
    p.write("contracts/owners.toml", &owners);
    p.write("contracts/boundaries.toml", &boundaries);
    p
}

#[test]
fn a_split_configuration_with_groups_reports_and_keys_a_baseline_as_its_one_file_form() {
    let written_out: String = KINDS.iter().map(expanded).collect();
    let longhand = project("fold_one_file", &written_out);
    let shorthand = split_project("fold_split");
    let args = ["--output-format", "json"];
    let (code, expected, err) = longhand.check(&args);
    assert_eq!(code, 1, "{expected}{err}");
    assert_eq!(shorthand.check(&args), (code, expected.clone(), err));
    for code in ["MUT201", "MUT203", "EFF201", "IMP201"] {
        let found: Vec<Value> = serde_json::from_str(&expected).unwrap();
        assert!(!of_rule(&found, code).is_empty(), "{code}: {expected}");
    }
    let update = ["--baseline", "heki-baseline.toml", "--update-baseline"];
    let expected = longhand.check(&update);
    assert_eq!(shorthand.check(&update), expected);
    assert_eq!(
        shorthand.read("heki-baseline.toml"),
        longhand.read("heki-baseline.toml")
    );
    assert_inspect_and_recommend_agree(&longhand, &shorthand);
}

#[test]
fn a_table_takes_its_own_key_else_its_files_default_else_the_main_files() {
    let main = r#"[tool.heki]
include = ["contracts/w.toml"]

[tool.heki.write-owners-defaults]
allowed-writers = ["pkg.m:ok_write"]
message = "help of the main file"

[[tool.heki.write-owners]]
name = "main"
targets = ["pkg.m:Model.fields"]
"#;
    let included = r#"[write-owners-defaults]
message = "help of the included file"

[[write-owners]]
name = "file-default"
targets = ["pkg.m:Model.secret"]

[[write-owners]]
name = "own"
targets = ["pkg.m:Model.count"]
allowed-writers = ["pkg.m:ok_write", "pkg.m:bump"]
message = "its own help"
"#;
    let written_out = r#"[[tool.heki.write-owners]]
name = "main"
targets = ["pkg.m:Model.fields"]
allowed-writers = ["pkg.m:ok_write"]
message = "help of the main file"

[[tool.heki.write-owners]]
name = "file-default"
targets = ["pkg.m:Model.secret"]
allowed-writers = ["pkg.m:ok_write"]
message = "help of the included file"

[[tool.heki.write-owners]]
name = "own"
targets = ["pkg.m:Model.count"]
allowed-writers = ["pkg.m:ok_write", "pkg.m:bump"]
message = "its own help"
"#;
    let split = project("fold_defaults_precedence", main);
    split.write("contracts/w.toml", included);
    let expected = reports(&project("fold_defaults_written_out", written_out));
    let found = reports(&split);
    assert_eq!(found, expected);
    let helps: Vec<&str> = (found.iter()).filter_map(|d| d["help"].as_str()).collect();
    for help in ["help of the main file", "help of the included file"] {
        assert!(helps.iter().any(|h| h.contains(help)), "{help}: {helps:?}");
    }
}

#[test]
fn an_unmatched_entry_names_its_group_and_file_and_an_unused_group_is_warned_about() {
    let main = r#"[tool.heki]
include = ["contracts/w.toml"]

[tool.heki.groups]
writers = ["pkg.m:ok_write", "@more-writers"]
unused = ["pkg.m:ok_write"]

[[tool.heki.write-owners]]
name = "main"
targets = ["pkg.m:Model.fields", "pkg.m:Model.gone"]
allowed-writers = ["@writers"]
"#;
    let included = r#"[groups]
more-writers = ["pkg.m:gone_writer"]

[[write-owners]]
name = "included"
targets = ["pkg.m:Model.secret", "pkg.m:Model.missing"]
allowed-writers = ["@more-writers"]
"#;
    let p = project("fold_unmatched", main);
    p.write("contracts/w.toml", included);
    let (_, stdout, stderr) = p.check(&[]);
    let warned: Vec<&str> = (stderr.lines())
        .filter_map(|l| l.strip_prefix("warning: "))
        .collect();
    for expected in [
        "the group `unused` (pyproject.toml:9) is used by no contract",
        "`targets` entry `pkg.m:Model.gone` of write-owners `main` names no attribute, method, or `__dict__` key of `pkg.m:Model` in the analyzed files",
        "`targets` entry `pkg.m:Model.missing` of write-owners `included` (from contracts/w.toml:6) names no attribute, method, or `__dict__` key of `pkg.m:Model` in the analyzed files",
    ] {
        assert!(warned.contains(&expected), "{expected}\n{stdout}{stderr}");
    }
    assert!(
        warned.iter().any(|w| w.starts_with(
            "`allowed-writers` entry `pkg.m:gone_writer` of write-owners `main` and `included` (from group `more-writers` at contracts/w.toml:2) names no function or method"
        )),
        "{stdout}{stderr}"
    );
    let (code, json, err) = p.heki(&["recommend", "--output-format", "json"]);
    assert_eq!(code, 0, "{json}{err}");
    let report: Value = serde_json::from_str(&json).unwrap();
    let from_group = (report["unmatched_entries"].as_array().unwrap().iter())
        .find(|e| e["entry"] == "pkg.m:gone_writer")
        .unwrap_or_else(|| panic!("{json}"));
    assert_eq!(
        from_group["written_at"],
        serde_json::json!([{"file": "contracts/w.toml", "line": 2, "group": "more-writers"}])
    );
    // Under `"error"`, an unused group fails the run as an unmatched entry does.
    let p = project(
        "fold_unused_group_fails",
        "[tool.heki.lint]\nunmatched-contract-entries = \"error\"\n\n[tool.heki.groups]\nunused = [\"x\"]\n",
    );
    let (code, stdout, stderr) = p.check(&[]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("error: the group `unused` (pyproject.toml:8) is used by no contract"),
        "{stderr}"
    );
}

#[test]
fn files_and_groups_that_do_not_fold_are_configuration_errors_at_their_line() {
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "[tool.heki.groups]\na = [\"x\"]\n\n[[tool.heki.write-owners]]\nname = \"w\"\ntargets = [\"pkg.m:Model.fields\"]\nallowed-writers = [\"@b\"]\n",
            "",
            "pyproject.toml:10:20: `@b` names no group",
            "configuration/#named-groups",
        ),
        (
            "[tool.heki.groups]\na = [\"@b\"]\nb = [\"x\", \"@a\"]\n",
            "",
            "pyproject.toml:6:11: a cycle between groups: `@a` -> `@b` -> `@a`",
            "configuration/#named-groups",
        ),
        (
            "[tool.heki.groups]\na = [\"@a\"]\n",
            "",
            "pyproject.toml:5:6: a cycle between groups: `@a` -> `@a`",
            "configuration/#named-groups",
        ),
        (
            "[tool.heki.groups]\na = []\n",
            "",
            "pyproject.toml:5:1: the group `a` is empty",
            "configuration/#named-groups",
        ),
        (
            "[tool.heki.write-owners-defaults]\nallowed-writers = [\"pkg.@a\"]\n",
            "",
            "pyproject.toml:5:20: the entry `pkg.@a` has `@` after its start",
            "configuration/#named-groups",
        ),
        (
            "[tool.heki]\ninclude = [\"c.toml\"]\n\n[tool.heki.groups]\na = [\"x\"]\n",
            "[groups]\na = [\"y\"]\n",
            "c.toml:2:1: the group `a` is defined twice; it is first defined at pyproject.toml:8",
            "configuration/#named-groups",
        ),
        (
            "[tool.heki]\ninclude = [\"pyproject.toml\"]\n",
            "",
            "pyproject.toml:5:12: `include` lists `pyproject.toml`, the main configuration file",
            "configuration/#several-configuration-files",
        ),
        (
            "[tool.heki]\ninclude = [\"c.toml\"]\n",
            "include = [\"d.toml\"]\n",
            "c.toml:1:1: `include` in an included file",
            "configuration/#several-configuration-files",
        ),
        (
            "[tool.heki]\ninclude = [\"missing.toml\"]\n",
            "",
            "pyproject.toml:5:12: cannot read the included file `missing.toml`",
            "configuration/#several-configuration-files",
        ),
        (
            "[tool.heki]\ninclude = [\"c.toml\", \"./c.toml\"]\n",
            "[groups]\na = [\"x\"]\n",
            "pyproject.toml:5:22: `include` lists `./c.toml` twice",
            "configuration/#several-configuration-files",
        ),
        (
            "[tool.heki]\ninclude = [\"*.toml\"]\n",
            "",
            "pyproject.toml:5:12: `include` entry `*.toml` is a glob",
            "configuration/#several-configuration-files",
        ),
        (
            "[tool.heki]\ninclude = [\"c.toml\"]\n",
            "[[write-owners]]\nname = \"w\"\ntargets = [\"pkg.m:Model.fields\"]\n\n[lint]\nselect = [\"MUT\"]\n",
            "c.toml:5:2: `lint` in an included file",
            "configuration/#several-configuration-files",
        ),
        (
            "[tool.heki]\ninclude = [\"c.toml\"]\n",
            "[[write-owners]]\nname = \"w\"\ntargets = \"pkg.m:Model.fields\"\n",
            "c.toml:3:11: invalid `targets` in [[write-owners]]",
            "configuration/#write-owners",
        ),
        (
            "[tool.heki]\ninclude = [\"c.toml\"]\n",
            "[[write-owners]]\nname = \"w\"\ntargets = [\"fields\"]\n",
            "c.toml:3:12: invalid write-owners target `fields`",
            "configuration/#write-owners",
        ),
        (
            "[tool.heki.groups]\nt = [\"fields\"]\n\n[[tool.heki.write-owners]]\nname = \"w\"\ntargets = [\"@t\"]\n",
            "",
            "pyproject.toml:5:6: invalid write-owners target `fields`",
            "configuration/#write-owners",
        ),
    ];
    for (main, included, error, docs) in cases {
        let p = project("fold_errors", main);
        if !included.is_empty() {
            p.write("c.toml", included);
        }
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{main}\n{stdout}{stderr}");
        assert!(
            stderr.contains(&format!("error: {error}")),
            "{error}\n{stderr}"
        );
        assert!(stderr.contains(docs), "{docs}\n{stderr}");
    }
}

#[test]
fn a_touched_included_file_shows_every_report_and_other_toml_files_are_passed_over() {
    let p = project(
        "fold_touched",
        "[tool.heki]\ninclude = [\"contracts/w.toml\"]\n",
    );
    p.write(
        "contracts/w.toml",
        "[[write-owners]]\nname = \"w\"\ntargets = [\"pkg.m:Model.fields\"]\nallowed-writers = [\"pkg.m:ok_write\"]\n",
    );
    p.write("other.toml", "[tool.other]\nx = 1\n");
    let (_, every, _) = p.check(&["--select", "MUT201"]);
    assert!(every.contains("pkg/m.py:"), "{every}");
    // As the pre-commit hook passes the staged TOML files.
    let (_, touched, stderr) = p.check(&[
        "--select",
        "MUT201",
        "--touched",
        "contracts/w.toml",
        "other.toml",
    ]);
    assert_eq!(touched, every);
    assert!(!stderr.contains("`--touched` names"), "{stderr}");
    let (_, none, stderr) = p.check(&["--select", "MUT201", "--touched", "other.toml"]);
    assert!(!none.contains("pkg/m.py:"), "{none}");
    assert!(!stderr.contains("`--touched` names"), "{stderr}");
}
