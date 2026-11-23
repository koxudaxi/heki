//! What the command line says when it cannot do what it was asked: one layout for every
//! failure (`error:`, where, the source line, `= help:`, `= docs:`), the help that names the
//! fix, and the documentation section that explains it (end to end, through the real binary).

mod common;

use common::Project;
use serde_json::Value;

const BLOCKING: &str = "import time\n\n\nasync def handler():\n    time.sleep(1)\n";

const SITE: &str = "https://heki.koxudaxi.dev/";

/// A project whose `pyproject.toml` holds `config` after its `[project]` table.
fn project(name: &str, config: &str) -> Project {
    let p = Project::new(name);
    p.write("a.py", BLOCKING).write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    );
    p
}

/// `heki <args>` fails with exit code 2, writes nothing to stdout, and prints lines starting
/// with `lines` (in this order) to stderr.
fn fails_with(p: &Project, args: &[&str], lines: &[&str]) {
    let (code, stdout, stderr) = p.heki(args);
    assert_eq!(code, 2, "{args:?}: {stdout}{stderr}");
    assert!(stdout.is_empty(), "{args:?}: {stdout}");
    let text = format!("\n{stderr}");
    let mut rest = text.as_str();
    for line in lines {
        let at = rest
            .find(&format!("\n{line}"))
            .unwrap_or_else(|| panic!("{args:?}: no line `{line}` in order in:\n{stderr}"));
        rest = &rest[at + 1 + line.len()..];
    }
}

fn docs(anchor: &str) -> String {
    format!("  = docs: {SITE}{anchor}")
}

/// Each configuration mistake names where it is, shows the line, says what to write instead,
/// and links the section of the configuration page about the key.
#[test]
fn configuration_errors_name_the_fix_and_the_section() {
    let lint = docs("configuration/#rule-selection-toolhekilint");
    let cases: &[(&str, &[&str])] = &[
        // A key of another table names its table, with a line to copy.
        (
            "[tool.heki]\nselect = [\"MUT\"]\n",
            &[
                "error: pyproject.toml:5:1: unknown key `select` in [tool.heki]",
                "5 | select = [\"MUT\"]",
                "  | ^",
                "  = help: `select` is a key of [tool.heki.lint]: move it there",
                "  = help: for example: `select = [\"EFF\", \"MUT101\"]`",
                &lint,
            ],
        ),
        // A misspelled key: the nearest key of its table.
        (
            "[tool.heki.lint]\nselct = [\"MUT\"]\n",
            &[
                "error: pyproject.toml:5:1: unknown key `selct` in [tool.heki.lint]",
                "  = help: did you mean `select`?",
                &lint,
            ],
        ),
        // A guessed table: heki has no `contracts` table, its tables are at the top.
        (
            "[[tool.heki.contracts.effect-boundaries]]\nname = \"x\"\n",
            &[
                "error: pyproject.toml:4:13: unknown key `contracts` in [tool.heki]",
                "  = help: heki has no `contracts` table: its keys belong in [tool.heki] itself (`[[tool.heki.effect-boundaries]]`)",
                &docs("configuration/#effect-boundaries"),
            ],
        ),
        // A key like no other: the keys the table has.
        (
            "[tool.heki]\nnope = 1\n",
            &[
                "error: pyproject.toml:5:1: unknown key `nope` in [tool.heki]",
                "  = help: expected one of `baseline`, `call-owners`, `ecosystem`, `effect-boundaries`, `exclude`, `extend-exclude`, `invalidation`, `lint`, `read-owners`, `readonly-returns`, `src`, `target-version`, `write-owners`",
                &docs("configuration/#top-level-settings"),
            ],
        ),
        // A value of the wrong type: the type the key takes and a line to copy.
        (
            "[tool.heki.lint]\nselect = \"MUT\"\n",
            &[
                "error: pyproject.toml:5:10: invalid `select` in [tool.heki.lint]: invalid type: string \"MUT\", expected a sequence",
                "  = help: `select` takes an array, for example: `select = [\"EFF\", \"MUT101\"]`",
                &lint,
            ],
        ),
        (
            "[tool.heki]\nlint = 1\n",
            &[
                "  = help: `lint` takes a table, for example: `[tool.heki.lint]`",
                &lint,
            ],
        ),
        (
            "[[tool.heki.write-owners]]\nname = \"s\"\ntargets = \"Model.fields\"\n",
            &[
                "error: pyproject.toml:6:11: invalid `targets` in [[tool.heki.write-owners]]: invalid type: string \"Model.fields\", expected a sequence",
                "  = help: `targets` takes an array, for example: `targets = [\"pkg.model.DataModel.fields\"]`",
                &docs("configuration/#write-owners"),
            ],
        ),
        (
            "[tool.heki]\nsrc = [\"../x\"]\n",
            &[
                "error: pyproject.toml:5:8: invalid `src` entry `../x`: the directory is outside the project root",
                "  = help: list directories relative to the project root: `src = [\"src\"]`",
                &docs("configuration/#module-names-follow-the-source-roots"),
            ],
        ),
        (
            "[tool.heki]\ntarget-version = \"py39\"\n",
            &[
                "error: pyproject.toml:5:18: unknown target version `py39` (expected py311..py315)",
                "  = help: did you mean `py311` or `py312`?",
                &docs("configuration/#the-default-target-version"),
            ],
        ),
        (
            "[tool.heki]\nextend-exclude = [\"gen[erated\"]\n",
            &[
                "error: pyproject.toml:5:19: invalid exclude glob `gen[erated`: error parsing glob 'gen[erated': unclosed character class; missing ']'",
                &docs("configuration/#which-files-are-checked"),
            ],
        ),
        (
            "[tool.heki\n",
            &[
                "error: pyproject.toml:4:11: the configuration file is not valid TOML: invalid table header; expected `.`, `]`",
                "  = help: heki reads its settings from this file: fix the TOML here, or name another file with `--config`",
                &docs("configuration/#where-heki-finds-the-configuration"),
            ],
        ),
        (
            "[[ecosystem]]\nsymbol = \"m.f\"\n",
            &[
                "error: pyproject.toml:4:3: `ecosystem` at the top level of a `pyproject.toml`",
                "  = help: a `pyproject.toml` holds heki's settings under `[tool.heki]`: move `ecosystem` into `[tool.heki]` (or its table, `[tool.heki.ecosystem]`)",
                &docs("configuration/#which-tables-hold-the-settings"),
            ],
        ),
    ];
    for (i, (config, lines)) in cases.iter().enumerate() {
        let p = project(&format!("cfg_shape_{i}"), config);
        fails_with(&p, &["check"], lines);
    }
    // A `heki.toml` writes its tables without `tool.heki`.
    let p = Project::new("cfg_shape_heki_toml");
    p.write("a.py", BLOCKING)
        .write("heki.toml", "[tool.heki.lint]\nselect = [\"EFF\"]\n");
    fails_with(
        &p,
        &["check"],
        &[
            "error: heki.toml:1:2: `[tool.heki]` outside a `pyproject.toml`",
            "  = help: only a `pyproject.toml` holds the settings under `tool.heki`; this file holds them at the top level: write `[lint]` and `[[write-owners]]`, not `[tool.heki.lint]`",
        ],
    );
    p.write("heki.toml", "select = [\"EFF\"]\n");
    fails_with(
        &p,
        &["check"],
        &[
            "error: heki.toml:1:1: unknown key `select` in the top level",
            "  = help: `select` is a key of [lint]: move it there",
        ],
    );
}

/// A selector that names no rule: the nearest codes or the code of a misspelled rule name,
/// where it is written, and `heki rule --all`.
#[test]
fn unknown_rules_offer_the_nearest_ones() {
    let lint = docs("configuration/#rule-selection-toolhekilint");
    let all = "  = help: `heki rule --all` lists every rule, with its code and name";
    let p = project("rule_shapes", "");
    fails_with(
        &p,
        &["check", "--select", "async-blocking-cal"],
        &[
            "error: unknown rule selector `async-blocking-cal` in `--select`",
            "  = help: did you mean `EFF101` (async-blocking-call)?",
            all,
            &lint,
        ],
    );
    fails_with(
        &p,
        &["check", "--select", "MUT999"],
        &[
            "error: unknown rule selector `MUT999` in `--select`",
            "  = help: did you mean `MUT109` (write-without-invalidation)?",
        ],
    );
    for (config, lines) in [
        (
            "[tool.heki.lint]\nignore = [\"EFF1O1\"]\n",
            [
                "error: pyproject.toml:5:11: unknown rule selector `EFF1O1` in `lint.ignore`",
                "  = help: did you mean `EFF101` (async-blocking-call)?",
            ],
        ),
        (
            "[tool.heki.lint.per-file-ignores]\n\"tests/**\" = [\"EFFF\"]\n",
            [
                "error: pyproject.toml:5:15: unknown rule selector `EFFF` in `lint.per-file-ignores`",
                "  = help: did you mean `EFF`?",
            ],
        ),
        (
            "[tool.heki.lint.severity]\nEFF1011 = \"error\"\n",
            [
                "error: pyproject.toml:5:1: unknown rule selector `EFF1011` in `lint.severity`",
                "  = help: did you mean `EFF101` (async-blocking-call)?",
            ],
        ),
        (
            "[tool.heki.lint.per-file-ignores]\n\"a[.py\" = [\"EFF\"]\n",
            [
                "error: pyproject.toml:5:1: invalid glob `a[.py` in `lint.per-file-ignores`: error parsing glob 'a[.py': unclosed character class; missing ']'",
                "  = help: a key of `per-file-ignores` is a glob of project-relative paths, as `\"tests/**\"`",
            ],
        ),
    ] {
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\n\n{config}"),
        );
        fails_with(&p, &["check"], &[lines[0], lines[1], &lint]);
    }
    let rules = docs("rules/");
    fails_with(
        &p,
        &["rule", "async-blockng-call"],
        &[
            "error: unknown rule `async-blockng-call`",
            "  = help: did you mean `EFF101` (async-blocking-call)?",
            "  = help: a rule is named by its code (`EFF101`) or its name (`async-blocking-call`)",
            all,
            &rules,
        ],
    );
    let (code, stdout, _) = p.heki(&["rule", "NOPE1", "--output-format", "json"]);
    assert_eq!(code, 2);
    let error: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(error["error"]["message"], "unknown rule `NOPE1`");
    assert_eq!(error["error"]["docs"], format!("{SITE}rules/"));
}

/// The contracts' and the ecosystem entries' mistakes, at the value in the file.
#[test]
fn contract_errors_point_at_the_value() {
    let boundary = "[[tool.heki.effect-boundaries]]\nname = \"b\"\nscopes = [\"*.render\"]\n";
    let cases: Vec<(String, Vec<&str>, &str)> = vec![
        (
            format!("{boundary}forbid = [\"blockng\"]\n"),
            vec![
                "error: pyproject.toml:7:11: unknown `forbid` token `blockng` in effect-boundaries `b`",
                "  = help: did you mean `blocking` or `blocking-db`?",
            ],
            "configuration/#effect-boundaries",
        ),
        (
            format!("{boundary}forbid = [\"writes-targets\"]\n"),
            vec![
                "error: pyproject.toml:7:11: effect-boundaries `b` forbids `writes-targets` but sets no `write-targets`",
                "  = help: add `write-targets` to the boundary, or take `writes-targets` out of `forbid`",
            ],
            "configuration/#effect-boundaries",
        ),
        (
            format!(
                "{boundary}forbid = [\"writes-targets\"]\nwrite-targets = [\"M.*\"]\nallow-writes = [\"M.a\"]\n"
            ),
            vec![
                "error: pyproject.toml:9:1: effect-boundaries `b` sets `allow-writes` but no `write-universe`",
                "  = help: add `write-universe`, the attributes whose writes the boundary judges; `allow-writes` lists the ones of them it allows",
            ],
            "configuration/#effect-boundaries",
        ),
        (
            format!("{boundary}forbid = [\"blocking\"]\nimport-probes = \"report\"\n"),
            vec![
                "error: pyproject.toml:8:1: effect-boundaries `b` sets `import-probes = \"report\"` but does not forbid `reads-environment`",
                "  = help: add `reads-environment` to `forbid`, or remove `import-probes`",
            ],
            "configuration/#effect-boundaries",
        ),
        (
            "[[tool.heki.write-owners]]\nname = \"s\"\ntargets = [\"fields\"]\n".to_owned(),
            vec![
                "error: pyproject.toml:6:12: invalid write-owners target `fields` (expected `Class.attr`)",
                "  = help: write a target as a class and its attribute: `\"pkg.model.DataModel.fields\"`, `\"DataModel.*\"`",
            ],
            "configuration/#write-owners",
        ),
        (
            "[[tool.heki.write-owners]]\nname = \"s\"\ntargets = [\"M.f\"]\nunresolved = \"reprot\"\n".to_owned(),
            vec![
                "error: pyproject.toml:7:14: unknown `unresolved` value `reprot` in write-owners `s` (expected `ignore` or `report`)",
                "  = help: did you mean `report`?",
            ],
            "configuration/#write-owners",
        ),
        (
            "[[tool.heki.write-owners]]\nname = \"s\"\ntargets = [\"M.f\"]\nallowed-writers = [\"Store.[*\"]\n".to_owned(),
            vec![
                "error: pyproject.toml:7:20: invalid glob `Store.[*` in write-owners.allowed-writers: error parsing glob 'Store.[*': unclosed character class; missing ']'",
                "  = help: a pattern is a dotted name with `*`, `?`, or `[...]`: `\"pkg.parser.*\"`, `\"*.render\"`",
            ],
            "configuration/#write-owners",
        ),
        (
            "[[tool.heki.write-owners]]\nname = \"s\"\ntargets = [\"M.f\"]\nallowed-writers = [\"a:b:c\"]\n".to_owned(),
            vec![
                "error: pyproject.toml:7:20: invalid pattern `a:b:c` in write-owners.allowed-writers (expected `module:qualname`, one colon with a name on each side)",
                "  = help: write the module, one colon, and the name in it: `\"pkg.db:Session.commit\"`",
            ],
            "configuration/#write-owners",
        ),
        (
            "[[tool.heki.read-owners]]\nname = \"r\"\ntargets = ['M.attr[\"k\"]']\n".to_owned(),
            vec![
                "error: pyproject.toml:6:12: invalid read-owners target `M.attr[\"k\"]`: a container entry (`Class.attr[\"key\"]`) is a write target only; reads are of `Class.attr`",
                "  = help: a read target is `Class.attr`: drop the `[\"key\"]`, or make the entry a `write-owners` target",
            ],
            "configuration/#read-owners",
        ),
        (
            "[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"pkg.DataType(is_dict=)\"]\n".to_owned(),
            vec![
                "error: pyproject.toml:9:12: invalid argument list in call-owners.targets target `pkg.DataType(is_dict=)`: a value is no constant (expected `name(parameter=value, ...)` with `True`, `False`, `None`, an integer, or a quoted string)",
                "  = help: write the arguments as `name(parameter=value)`: `\"DataType(is_dict=True)\"`",
            ],
            "configuration/#call-owners",
        ),
        (
            "[[tool.heki.ecosystem]]\nsymbol = \"m.f\"\neffects = [\"blocking_nett\"]\n".to_owned(),
            vec![
                "error: pyproject.toml:6:12: invalid effect kind `blocking_nett` in user ecosystem override",
                "  = help: did you mean `blocking_net` or `blocking_sync`?",
            ],
            "configuration/#ecosystem-knowledge-toolhekiecosystem",
        ),
        (
            "[[tool.heki.ecosystem]]\nsymbol = \"m.f\"\ncopy-sanitizer = \"deap\"\n".to_owned(),
            vec![
                "error: pyproject.toml:6:18: invalid copy-sanitizer `deap` in user ecosystem override (`shallow` or `deep`)",
                "  = help: did you mean `deep`?",
            ],
            "configuration/#ecosystem-knowledge-toolhekiecosystem",
        ),
        (
            "[[tool.heki.ecosystem]]\nsymbol = \"m.f\"\none-at-a-time = \"nope\"\n".to_owned(),
            vec![
                "error: pyproject.toml:6:17: invalid one-at-a-time `nope` in user ecosystem override (`always`, `when_unbound`, or `unbinds`)",
                "  = help: expected one of `always`, `when_unbound`, `unbinds`",
            ],
            "configuration/#ecosystem-knowledge-toolhekiecosystem",
        ),
        (
            "[[tool.heki.ecosystem]]\nsymbol = \"m.C.client\"\nattribute-type = \"m.Chat\"\neffects = [\"blocking_net\"]\n".to_owned(),
            vec![
                "error: pyproject.toml:5:10: `attribute-type` in the user ecosystem override of `m.C.client` takes no other key: it describes reading an attribute, not calling it",
                "  = help: describe reading the attribute and calling what it gives in two entries: one with `attribute-type`, one with the call's keys for the class it names",
            ],
            "configuration/#ecosystem-knowledge-toolhekiecosystem",
        ),
    ];
    for (i, (config, mut lines, anchor)) in cases.into_iter().enumerate() {
        let p = project(&format!("contract_shape_{i}"), &config);
        let section = docs(anchor);
        lines.push(&section);
        fails_with(&p, &["check"], &lines);
    }
}

/// Paths, files, baselines, snapshots, and profiles: the file, and what to do with it.
#[test]
fn file_errors_say_what_to_do_with_the_file() {
    let p = project("file_shapes", "");
    fails_with(
        &p,
        &["check", "missing.py"],
        &[
            "error: path `missing.py` does not exist",
            "  = help: pass files or directories that exist, relative to the working directory: `heki check src tests`",
            &docs("cli-reference/#heki-check"),
        ],
    );
    fails_with(
        &p,
        &["check", "--target-version", "py39"],
        &[
            "error: unknown target version `py39` (expected py311..py315)",
            "  = help: heki checks Python 3.11 to 3.15: `target-version = \"py312\"`, or `--target-version py312`",
        ],
    );
    let baselines = docs("adoption/#ratchet-baselines");
    let rewrite =
        "  = help: `heki check --update-baseline` writes it again from the current reports";
    p.write("b.toml", "version = 9\n");
    fails_with(
        &p,
        &["check", "--baseline", "b.toml"],
        &[
            "error: b.toml: the baseline has version 9; this heki reads versions 1 and 2",
            rewrite,
            &baselines,
        ],
    );
    p.write("b.toml", "version = [\n");
    fails_with(
        &p,
        &["check", "--baseline", "b.toml"],
        &[
            "error: b.toml: invalid baseline: invalid array; expected `]`",
            rewrite,
        ],
    );
    fails_with(
        &p,
        &["check", "--update-baseline"],
        &[
            "error: `--update-baseline` needs a baseline file",
            "  = help: name it with `--baseline <path>`, or set `baseline` in the configuration",
            &baselines,
        ],
    );
    let snapshots = docs("adoption/#effect-snapshots-mut103");
    p.write(".heki-summary-snapshot.json", "{");
    fails_with(
        &p,
        &["check", "--extend-select", "MUT103"],
        &[
            "error: .heki-summary-snapshot.json: invalid summary snapshot: EOF while parsing an object at line 1 column 1",
            "  = help: `heki snapshot --write` writes the summary snapshot of the project again (`MUT103` compares with it)",
            &snapshots,
        ],
    );
    std::fs::remove_file(p.dir.join(".heki-summary-snapshot.json")).unwrap();
    let profiles = docs("fixes/#profiles");
    p.write("p.txt", "junk\n");
    fails_with(
        &p,
        &["check", "--profile", "p.txt"],
        &[
            "error: unsupported or malformed profile dump: junk",
            &profiles,
        ],
    );
    fails_with(
        &p,
        &["check", "--profile", "nope.txt"],
        &["error: nope.txt: cannot read the profile dump: ", &profiles],
    );
    // A file that does not parse is reported after the others are checked.
    p.write("b.py", "def f(:\n");
    let (code, _, stderr) = p.heki(&["check", "--output-format", "concise"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains(
            "error: b.py:1:7: syntax error: Expected a parameter or the end of the parameter list (2 error(s))\n  |\n1 | def f(:\n  |       ^\n  = help: fix the syntax error, or exclude the file with `extend-exclude`\n"
        ),
        "{stderr}"
    );
}

/// `--output-format json` writes the error as an object on stdout, the text on stderr.
#[test]
fn json_output_writes_the_error_object() {
    let p = project("json_error", "[tool.heki.lint]\nselect = [\"MUT999\"]\n");
    let (code, stdout, stderr) = p.heki(&["check", "--output-format", "json"]);
    assert_eq!(code, 2);
    let v: Value = serde_json::from_str(&stdout).unwrap();
    let error = &v["error"];
    assert_eq!(
        error["message"],
        "unknown rule selector `MUT999` in `lint.select`"
    );
    assert_eq!(
        (&error["path"], &error["line"], &error["column"]),
        (&"pyproject.toml".into(), &5.into(), &11.into())
    );
    assert_eq!(error["candidates"][0], "MUT109");
    assert_eq!(
        error["docs"],
        format!("{SITE}configuration/#rule-selection-toolhekilint")
    );
    assert_eq!(
        error["help"][2],
        "`heki rule --all` lists every rule, with its code and name"
    );
    assert!(
        stderr.starts_with("error: pyproject.toml:5:11: unknown rule selector"),
        "{stderr}"
    );
}

/// `heki inspect` offers the functions whose names look like the one it was given.
#[test]
fn inspect_offers_the_nearest_functions() {
    let p = project("inspect_shapes", "");
    let section = docs("coding-agents/#before-an-edit-heki-inspect-experimental");
    let (code, _, stderr) = p.heki(&["inspect", "a.handlr"]);
    assert_eq!(code, 2);
    for line in [
        "error: no function of the project is named `a.handlr`",
        "  = help: did you mean `a.handler`?",
        "  = help: name a function as `module.function`, `module.Class.method`, `path.py::NAME`, or `path.py:LINE`",
        &section,
        "  a.handler  a.py:4",
    ] {
        assert!(stderr.contains(&format!("{line}\n")), "{line}: {stderr}");
    }
    let (code, _, stderr) = p.heki(&["inspect", "zz.py:3"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("  = help: name a Python file `heki check` checks, relative to the working directory, or pass its directory as a path\n"),
        "{stderr}"
    );
}
