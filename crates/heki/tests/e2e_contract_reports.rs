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

/// Helpers handed `Model.items`, or the model holding it, that change it themselves, through a
/// further helper, or only through `clear_allowed`.
const HANDED_FIELD: &str = r#"class Model:
    def __init__(self) -> None:
        self.items: list[int] = []


def clear_allowed(xs):
    xs.clear()


def only_allowed(xs):
    clear_allowed(xs)


def also_own(xs):
    xs.append(1)
    clear_allowed(xs)


def through_other(xs):
    other(xs)


def other(xs):
    xs.pop()


def holder_only_allowed(m):
    clear_allowed(m.items)


def holder_own(m):
    m.items.clear()


def render(m: Model) -> None:
    only_allowed(m.items)


def render_own(m: Model) -> None:
    also_own(m.items)


def render_other(m: Model) -> None:
    through_other(m.items)


def render_holder(m: Model) -> None:
    holder_only_allowed(m)


def render_holder_own(m: Model) -> None:
    holder_own(m)
"#;

#[test]
fn a_helper_handed_a_field_writes_it_only_where_no_allowed_callee_does() {
    let p = project(
        "boundary_allowed_helper",
        "[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"app.render*\"]\nforbid = [\"writes-targets\"]\nwrite-targets = [\"Model.items\"]\nallowed-callees = [\"app.clear_allowed\"]\n",
    );
    p.write("app.py", HANDED_FIELD);
    let target = || "pure:Model.items".to_owned();
    assert_eq!(
        reports(&p, "EFF201"),
        vec![(40, target(), 1), (44, target(), 1), (52, target(), 1)]
    );
}

#[test]
fn a_helper_handed_a_field_writes_it_only_where_no_allowed_writer_does() {
    let p = project(
        "owner_allowed_helper",
        "[[tool.heki.write-owners]]\nname = \"owner\"\ntargets = [\"Model.items\"]\nallowed-writers = [\"app.clear_allowed\"]\n",
    );
    p.write("app.py", HANDED_FIELD);
    let target = || "Model.items".to_owned();
    assert_eq!(
        reports(&p, "MUT201"),
        vec![(40, target(), 1), (44, target(), 1), (52, target(), 1)]
    );
}

/// Helpers handed `Model.items` (or the model) that leave every change to `clear_allowed`: after
/// logging through a project function, recursively, besides changing another attribute, or
/// besides handing on a bound method of the list that the callee only keeps. The others change it
/// themselves, through a further helper handed the attribute, or where the generator a helper
/// returns is iterated.
const HANDED_FURTHER: &str = r#"class Model:
    def __init__(self) -> None:
        self.items: list[int] = []
        self.tags: list[str] = []


def clear_allowed(xs):
    xs.clear()


def note(message):
    return message


def wipe(xs):
    xs.clear()


def logged(xs):
    note("clearing")
    clear_allowed(xs)


def walk(xs, depth):
    if depth:
        walk(xs, depth - 1)
    clear_allowed(xs)


def holder_wipes(m):
    wipe(m.items)


def holder_other(m):
    wipe(m.tags)
    clear_allowed(m.items)


def _drain(xs):
    while xs:
        yield xs.pop()


def drained(xs):
    return _drain(xs)


def drain_all(xs):
    for _ in drained(xs):
        pass


def settle(xs):
    clear_allowed(xs)
    note(xs.append)


def render_logged(m: Model) -> None:
    logged(m.items)


def render_walk(m: Model) -> None:
    walk(m.items, 3)


def render_holder_wipes(m: Model) -> None:
    holder_wipes(m)


def render_holder_other(m: Model) -> None:
    holder_other(m)


def render_drain(m: Model) -> None:
    drain_all(m.items)


def render_settle(m: Model) -> None:
    settle(m.items)
"#;

#[test]
fn a_helper_handed_a_field_writes_it_where_it_or_a_callee_it_hands_the_field_changes_it() {
    let p = project(
        "owner_handed_further",
        "[[tool.heki.write-owners]]\nname = \"owner\"\ntargets = [\"Model.items\"]\nallowed-writers = [\"app.clear_allowed\"]\n",
    );
    p.write("app.py", HANDED_FURTHER);
    let target = || "Model.items".to_owned();
    assert_eq!(
        reports(&p, "MUT201"),
        vec![(67, target(), 1), (75, target(), 1)]
    );
}

const RESETS: &str = r#"class Base:
    pass


class A(Base):
    def reset(self) -> None: ...


class B(Base):
    def reset(self) -> None: ...


class C(Base):
    def reset(self) -> None: ...


def on_base(obj: Base) -> None:
    obj.reset()


def on_a(obj: A) -> None:
    obj.reset()
"#;

#[test]
fn a_call_whose_candidates_are_all_targets_of_one_contract_is_reported_once() {
    let owners = |targets: &str| {
        format!(
            "[[tool.heki.call-owners]]\nname = \"resets\"\ntargets = [{targets}]\nallowed-callers = [\"app.owner\"]\n"
        )
    };
    // `C.reset` is a candidate too, so `on_base` may call a function no entry names.
    let p = project("call_union_partial", &owners("\"A.reset\", \"B.reset\""));
    p.write("app.py", RESETS);
    assert_eq!(reports(&p, "MUT204"), vec![(22, "A.reset".to_owned(), 1)]);
    // Every candidate is named by some entry: one report for the entries the candidates match.
    let p = project(
        "call_union_whole",
        &owners("\"A.reset\", \"B.reset\", \"C.reset\""),
    );
    p.write("app.py", RESETS);
    assert_eq!(
        reports(&p, "MUT204"),
        vec![
            (18, "A.reset|B.reset|C.reset".to_owned(), 1),
            (22, "A.reset".to_owned(), 1),
        ]
    );
    let (_, stdout, _) = p.check(&["--select", "MUT204", "--output-format", "concise"]);
    assert!(
        stdout.contains("`on_base` calls `A.reset` or `B.reset` or `C.reset` outside the call owners of `resets`"),
        "{stdout}"
    );
    // Glob entries each name the candidate they match.
    let p = project(
        "call_union_globs",
        &owners("\"A.res*\", \"B.res*\", \"C.res*\""),
    );
    p.write("app.py", RESETS);
    let (_, stdout, _) = p.check(&["--select", "MUT204", "--output-format", "concise"]);
    assert!(
        stdout.contains("`on_base` calls `app.A.reset` or `app.B.reset` or `app.C.reset` outside the call owners of `resets`"),
        "{stdout}"
    );
    // The `calls` form of an effect boundary shares the matcher.
    let p = project(
        "boundary_call_union",
        "[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"app.on_base\"]\nforbid = [\"calls\"]\ncall-targets = [\"A.reset\", \"B.reset\", \"C.reset\"]\n",
    );
    p.write("app.py", RESETS);
    assert_eq!(
        reports(&p, "EFF201"),
        vec![(18, "pure:calls:A.reset|B.reset|C.reset".to_owned(), 1)]
    );
}

const RECURSIVE_WRITERS: &str = r#"class Model:
    def __init__(self) -> None:
        self.value = 0

    def touch(self) -> None:
        pass


def _walk(m: Model, n: int) -> None:
    m.value = n
    m.touch()
    if n > 0:
        _walk(m, n - 1)


callback = _walk


def _ping(m: Model, n: int) -> None:
    m.value = n
    if n > 0:
        _pong(m, n - 1)


def _pong(m: Model, n: int) -> None:
    if n > 0:
        _ping(m, n - 1)


def _step(m: Model, n: int) -> None:
    m.value = n
    if n > 0:
        _step(m, n - 1)


def run(m: Model) -> None:
    _step(m, 3)
"#;

#[test]
fn a_private_recursion_no_other_function_calls_reports_its_own_accesses() {
    let config = "[[tool.heki.write-owners]]\nname = \"owner\"\ntargets = [\"Model.value\"]\nallowed-writers = [\"app.Store.*\"]\n\n[[tool.heki.read-owners]]\nname = \"readers\"\ntargets = [\"Model.value\"]\nallowed-readers = [\"app.Store.*\"]\n\n[[tool.heki.call-owners]]\nname = \"touch\"\ntargets = [\"Model.touch\"]\nallowed-callers = [\"app.Store.*\"]\n";
    let p = project("private_recursion", config);
    p.write("app.py", RECURSIVE_WRITERS);
    // `_walk` and `_ping` are called only from their own recursion: they report; `_step` hands
    // its write to `run`, at the call.
    let lines = |rule: &str| -> Vec<u64> { reports(&p, rule).iter().map(|r| r.0).collect() };
    assert_eq!(lines("MUT201"), vec![10, 20, 37]);
    assert_eq!(lines("MUT204"), vec![11]);
    // A recursion without reads reports none.
    assert_eq!(lines("MUT203"), Vec::<u64>::new());
}

#[test]
fn a_mutates_params_report_counts_every_change_it_reaches() {
    let p = project(
        "boundary_param_changes",
        "[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"app.scope\"]\nforbid = [\"mutates-params\"]\nallowed-callees = [\"app.allowed\"]\n",
    );
    let src = "def helper(ys):\n    ys.pop()\n\n\ndef allowed(ys):\n    ys.sort()\n\n\ndef scope(xs):\n    xs.append(1)\n    helper(xs)\n    allowed(xs)\n";
    p.write("app.py", src);
    let target = || "pure:mutates-params".to_owned();
    assert_eq!(reports(&p, "EFF201"), vec![(9, target(), 2)]);
    // A second change in the scope function is a new site for the Ratchet baseline.
    p.write(
        "app.py",
        &src.replace("    xs.append(1)\n", "    xs.append(1)\n    xs.clear()\n"),
    );
    assert_eq!(reports(&p, "EFF201"), vec![(9, target(), 3)]);
}
