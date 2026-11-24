//! The boundary equivalence oracle: moving the value an effect depends on into a helper's
//! parameter, a factory's return, a module constant, an offloaded call, or a `functools.partial`
//! object does not change what heki reports about the effect. Each row of [`ROWS`] is written in
//! every form of [`FORMS`], and every form must report the row's rule with the code and
//! certainty the inline form reports, or stay silent where the inline form does. The forms that
//! do not are today's gaps, listed in [`GAPS`] with what they report; the list is exact, so a
//! gap that closes fails the test until its line is removed.

mod common;

use common::Project;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One expression an effect depends on, and the code that uses it.
struct Row {
    name: &'static str,
    /// The rule whose reports the forms must agree on.
    rule: &'static str,
    /// The calls that make the value: their own reports are not the use's (a connection opened
    /// in the `async def` waits there too).
    made_by: &'static [&'static str],
    /// Whether the code using the value runs in an `async def` (`EFF101`) or a plain function
    /// (`EFF103`).
    on_event_loop: bool,
    /// The module's imports and definitions.
    setup: &'static str,
    /// The expression, and the class a parameter or a return holding it is annotated with.
    value: &'static str,
    annotation: &'static str,
    /// The statement using the value, `{v}` standing for it.
    usage: &'static str,
}

const SQLITE: &str = "import sqlite3\n";
const SQLITE_FILE: &str = "import sqlite3\n\nCONN = sqlite3.connect(\"app.db\")\n";
const SQLITE_MEMORY: &str = "import sqlite3\n\nMEM = sqlite3.connect(\":memory:\")\n";
const SQLITE_NAME: &str = "import sqlite3\n\nDB_NAME = \"app.db\"\n";
const SQLITE_PATH: &str = "import sqlite3\nfrom pathlib import Path\n\n\
    DB_PATH = Path(__file__).resolve().parent / \"app.db\"\n";
const BOOK_COUNTS: &str = "import sqlite3\n\nCONN = sqlite3.connect(\"app.db\")\n\n\n\
    def count_books(author: int) -> int:\n    \
    return CONN.execute(\"SELECT COUNT(*) FROM b WHERE a = ?\", (author,)).fetchone()[0]\n";
const POOL: &str =
    "from concurrent.futures import ThreadPoolExecutor\n\nPOOL = ThreadPoolExecutor()\n";
const EXECUTE: &str = "return {v}.execute(\"SELECT 1\")";
const BOOKS_OF_EACH: &str = "return [count_books(r[0]) for r in {v}]";

/// The rows. A row whose inline form is silent must stay silent in every form (a connection in
/// memory, a zero sleep, a `data:` URL, an unbounded queue, a shutdown that does not wait).
const ROWS: &[Row] = &[
    Row {
        name: "net-request",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: "import requests\n",
        value: "\"https://example.com/\"",
        annotation: "str",
        usage: "return requests.get({v}, timeout=5)",
    },
    Row {
        name: "sqlite-file",
        rule: "EFF101",
        made_by: &["sqlite3.connect"],
        on_event_loop: true,
        setup: SQLITE,
        value: "sqlite3.connect(\"app.db\")",
        annotation: "sqlite3.Connection",
        usage: EXECUTE,
    },
    Row {
        name: "sqlite-file-global",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: SQLITE_FILE,
        value: "CONN",
        annotation: "sqlite3.Connection",
        usage: EXECUTE,
    },
    Row {
        name: "sqlite-memory",
        rule: "EFF101",
        made_by: &["sqlite3.connect"],
        on_event_loop: true,
        setup: SQLITE,
        value: "sqlite3.connect(\":memory:\")",
        annotation: "sqlite3.Connection",
        usage: EXECUTE,
    },
    Row {
        name: "sqlite-memory-global",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: SQLITE_MEMORY,
        value: "MEM",
        annotation: "sqlite3.Connection",
        usage: EXECUTE,
    },
    Row {
        name: "sqlite-file-name-constant",
        rule: "EFF101",
        made_by: &["sqlite3.connect"],
        on_event_loop: true,
        setup: SQLITE_NAME,
        value: "sqlite3.connect(DB_NAME)",
        annotation: "sqlite3.Connection",
        usage: EXECUTE,
    },
    Row {
        name: "sqlite-file-path-constant",
        rule: "EFF101",
        made_by: &["sqlite3.connect"],
        on_event_loop: true,
        setup: SQLITE_PATH,
        value: "sqlite3.connect(DB_PATH)",
        annotation: "sqlite3.Connection",
        usage: EXECUTE,
    },
    Row {
        name: "sleep",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: "import time\n",
        value: "5",
        annotation: "float",
        usage: "time.sleep({v})",
    },
    Row {
        name: "sleep-zero",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: "import time\n",
        value: "0",
        annotation: "float",
        usage: "time.sleep({v})",
    },
    Row {
        name: "sleep-from-environment",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: "import os\nimport time\n",
        value: "float(os.environ.get(\"DELAY\", \"1\"))",
        annotation: "float",
        usage: "time.sleep({v})",
    },
    Row {
        name: "urlopen",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: "import urllib.request\n",
        value: "\"https://example.com/\"",
        annotation: "str",
        usage: "return urllib.request.urlopen({v})",
    },
    Row {
        name: "urlopen-data",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: "import urllib.request\n",
        value: "\"data:text/plain,hi\"",
        annotation: "str",
        usage: "return urllib.request.urlopen({v})",
    },
    Row {
        name: "bounded-queue-put",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: "import queue\n",
        value: "queue.Queue(maxsize=1)",
        annotation: "queue.Queue",
        usage: "{v}.put(1)",
    },
    Row {
        name: "unbounded-queue-put",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: "import queue\n",
        value: "queue.Queue()",
        annotation: "queue.Queue",
        usage: "{v}.put(1)",
    },
    Row {
        name: "shutdown-wait",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: POOL,
        value: "True",
        annotation: "bool",
        usage: "POOL.shutdown(wait={v})",
    },
    Row {
        name: "shutdown-no-wait",
        rule: "EFF101",
        made_by: &[],
        on_event_loop: true,
        setup: POOL,
        value: "False",
        annotation: "bool",
        usage: "POOL.shutdown(wait={v})",
    },
    Row {
        name: "query-rows",
        rule: "EFF103",
        made_by: &[],
        on_event_loop: false,
        setup: BOOK_COUNTS,
        value: "CONN.execute(\"SELECT id FROM a\").fetchall()",
        annotation: "list",
        usage: BOOKS_OF_EACH,
    },
    Row {
        name: "query-cursor",
        rule: "EFF103",
        made_by: &[],
        on_event_loop: false,
        setup: BOOK_COUNTS,
        value: "CONN.execute(\"SELECT id FROM a\")",
        annotation: "sqlite3.Cursor",
        usage: BOOKS_OF_EACH,
    },
];

/// Where the value comes from, relative to the code using it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Form {
    /// Written where it is used.
    Inline,
    /// Passed to a helper whose parameter is annotated with the value's class.
    AnnotatedHelper,
    /// Passed down two such helpers.
    TwoHelpers,
    /// Passed to a helper whose parameter has no annotation.
    UnannotatedHelper,
    /// Returned by a function annotated to return the value's class.
    Factory,
    /// Returned by a function without a return annotation.
    UnannotatedFactory,
    /// Bound to a module constant.
    ModuleConstant,
    /// Returned by a function `asyncio.to_thread` runs (the code using it in an `async def`).
    ToThread,
    /// Returned by a factory called through `functools.partial`.
    Partial,
}

const FORMS: [Form; 9] = [
    Form::Inline,
    Form::AnnotatedHelper,
    Form::TwoHelpers,
    Form::UnannotatedHelper,
    Form::Factory,
    Form::UnannotatedFactory,
    Form::ModuleConstant,
    Form::ToThread,
    Form::Partial,
];

impl Form {
    fn name(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::AnnotatedHelper => "annotated-helper",
            Self::TwoHelpers => "two-helpers",
            Self::UnannotatedHelper => "unannotated-helper",
            Self::Factory => "factory",
            Self::UnannotatedFactory => "unannotated-factory",
            Self::ModuleConstant => "module-constant",
            Self::ToThread => "to-thread",
            Self::Partial => "partial",
        }
    }

    /// The module of `row` in this form: its setup, the functions the form adds, and the
    /// function `entry` running the use.
    fn module(self, row: &Row) -> String {
        let used = |v: &str| row.usage.replace("{v}", v);
        let entry = |body: &str| {
            let kind = if row.on_event_loop || self == Self::ToThread {
                "async def"
            } else {
                "def"
            };
            format!("\n\n{kind} entry():\n    {body}\n")
        };
        let (value, annotation) = (row.value, row.annotation);
        let defined = match self {
            Self::Inline => entry(&used(value)),
            Self::AnnotatedHelper => format!(
                "\n\ndef use(v: {annotation}):\n    {}\n{}",
                used("v"),
                entry(&format!("return use({value})"))
            ),
            Self::TwoHelpers => format!(
                "\n\ndef use(v: {annotation}):\n    {}\n\n\ndef outer(v: {annotation}):\n    \
                 return use(v)\n{}",
                used("v"),
                entry(&format!("return outer({value})"))
            ),
            Self::UnannotatedHelper => format!(
                "\n\ndef use(v):\n    {}\n{}",
                used("v"),
                entry(&format!("return use({value})"))
            ),
            Self::Factory => format!(
                "\n\ndef make() -> {annotation}:\n    return {value}\n{}",
                entry(&used("make()"))
            ),
            Self::UnannotatedFactory => format!(
                "\n\ndef make():\n    return {value}\n{}",
                entry(&used("make()"))
            ),
            Self::ModuleConstant => format!("\nSUBJECT = {value}\n{}", entry(&used("SUBJECT"))),
            Self::ToThread => format!(
                "\n\ndef make() -> {annotation}:\n    return {value}\n{}",
                entry(&format!(
                    "v = await asyncio.to_thread(make)\n    {}",
                    used("v")
                ))
            ),
            Self::Partial => format!(
                "\n\ndef make() -> {annotation}:\n    return {value}\n{}",
                entry(&format!("v = functools.partial(make)()\n    {}", used("v")))
            ),
        };
        format!("import asyncio\nimport functools\n{}{defined}", row.setup)
    }

    fn file(self) -> String {
        format!("{}.py", self.name().replace('-', "_"))
    }
}

/// The forms that report otherwise than the inline form of their row today, with what they
/// report (`-`: nothing). Each closes with the fact named above its group.
const GAPS: &[(&str, &str, &str)] = &[
    // A parameter is typed by its annotation only; the argument at each call decides the
    // catalog's cases and states the helper's calls leave to it. An unannotated parameter gives
    // no class, so nothing is reported for a connection.
    ("sqlite-file", "unannotated-helper", "-"),
    ("sqlite-file-global", "unannotated-helper", "-"),
    ("sqlite-file-name-constant", "unannotated-helper", "-"),
    ("sqlite-file-path-constant", "unannotated-helper", "-"),
    ("bounded-queue-put", "unannotated-helper", "-"),
    // A call's result is typed by the function's return annotation only, and carries neither
    // the state of the value returned nor the literal it is: a connection's file is not known
    // (heuristic, or nothing without an annotation), a queue's capacity is not known (nothing),
    // and a returned zero or `data:` URL is an argument heki cannot evaluate, which counts as one
    // the call waits on (assumption R6).
    ("sqlite-file", "factory", "EFF101 [heuristic]"),
    ("sqlite-file", "unannotated-factory", "-"),
    ("sqlite-file", "to-thread", "EFF101 [heuristic]"),
    ("sqlite-file", "partial", "EFF101 [heuristic]"),
    ("sqlite-file-global", "factory", "EFF101 [heuristic]"),
    ("sqlite-file-global", "unannotated-factory", "-"),
    ("sqlite-file-global", "to-thread", "EFF101 [heuristic]"),
    ("sqlite-file-global", "partial", "EFF101 [heuristic]"),
    ("sqlite-file-name-constant", "factory", "EFF101 [heuristic]"),
    ("sqlite-file-name-constant", "unannotated-factory", "-"),
    (
        "sqlite-file-name-constant",
        "to-thread",
        "EFF101 [heuristic]",
    ),
    ("sqlite-file-name-constant", "partial", "EFF101 [heuristic]"),
    ("sqlite-file-path-constant", "unannotated-factory", "-"),
    ("sqlite-memory", "factory", "EFF101 [heuristic]"),
    ("sqlite-memory", "to-thread", "EFF101 [heuristic]"),
    ("sqlite-memory", "partial", "EFF101 [heuristic]"),
    ("sqlite-memory-global", "factory", "EFF101 [heuristic]"),
    ("sqlite-memory-global", "to-thread", "EFF101 [heuristic]"),
    ("sqlite-memory-global", "partial", "EFF101 [heuristic]"),
    ("sleep-zero", "factory", "EFF101"),
    ("sleep-zero", "unannotated-factory", "EFF101"),
    ("sleep-zero", "to-thread", "EFF101"),
    ("sleep-zero", "partial", "EFF101"),
    ("urlopen-data", "factory", "EFF101"),
    ("urlopen-data", "unannotated-factory", "EFF101"),
    ("urlopen-data", "to-thread", "EFF101"),
    ("urlopen-data", "partial", "EFF101"),
    ("bounded-queue-put", "factory", "-"),
    ("bounded-queue-put", "unannotated-factory", "-"),
    ("bounded-queue-put", "to-thread", "-"),
    ("bounded-queue-put", "partial", "-"),
    ("shutdown-no-wait", "factory", "EFF101 [heuristic]"),
    (
        "shutdown-no-wait",
        "unannotated-factory",
        "EFF101 [heuristic]",
    ),
    ("shutdown-no-wait", "to-thread", "EFF101 [heuristic]"),
    ("shutdown-no-wait", "partial", "EFF101 [heuristic]"),
    // A `Path` module constant read inside a function is not known to name a file, while the
    // module body's own call on it is: here the inline form is the heuristic one.
    ("sqlite-file-path-constant", "module-constant", "EFF101"),
    // The items a loop steps through are the data's ("Origin") when they come from a parameter,
    // a catalog call, or a project function returning such items (also through
    // `asyncio.to_thread` or `functools.partial`): the rows a module constant holds leave
    // "Origin" open.
    ("query-rows", "module-constant", "EFF103 [heuristic]"),
    ("query-cursor", "module-constant", "EFF103 [heuristic]"),
];

/// What each form of `row` reports under the row's rule, apart from the calls making the value:
/// its codes with their tier ([`common::tiered_code`]), or `-`. The forms of one row are the
/// modules of one project, fewer than the reports of one rule and root cause that output folds
/// into one (`ROOT_CAUSE_GROUP_THRESHOLD`), so each form keeps its own report.
fn outcomes(row: &Row) -> BTreeMap<Form, String> {
    let p = Project::new(&format!("boundary_equivalence_{}", row.name));
    for form in FORMS {
        p.write(&form.file(), &form.module(row));
    }
    let (code, stdout, stderr) = p.check(&["--select", row.rule, "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{}: {stdout}{stderr}", row.name);
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    FORMS
        .into_iter()
        .map(|form| {
            let file = form.file();
            let found: BTreeSet<&str> = diags
                .iter()
                .filter(|d| {
                    d["path"] == file.as_str()
                        && d["rule_code"] == row.rule
                        && !row.made_by.contains(&d["target"].as_str().unwrap())
                })
                .map(common::tiered_code)
                .collect();
            let outcome = if found.is_empty() {
                "-".to_owned()
            } else {
                found.into_iter().collect::<Vec<_>>().join(", ")
            };
            (form, outcome)
        })
        .collect()
}

#[test]
fn every_form_reports_what_the_inline_form_reports_but_the_listed_gaps() {
    let mut wrong = Vec::new();
    for row in ROWS {
        let outcomes = outcomes(row);
        let inline = &outcomes[&Form::Inline];
        for (form, got) in &outcomes {
            let listed = GAPS
                .iter()
                .find(|(r, f, _)| *r == row.name && *f == form.name())
                .map(|(.., outcome)| *outcome);
            let at = format!("{} / {}", row.name, form.name());
            match listed {
                None if got != inline => wrong.push(format!(
                    "{at}: reports {got}, the inline form {inline}: a new gap"
                )),
                Some(listed) if got == inline => wrong.push(format!(
                    "{at}: reports {got} as the inline form does: remove its line from GAPS \
                     (listed as {listed})"
                )),
                Some(listed) if got != listed => wrong.push(format!(
                    "{at}: reports {got}, GAPS lists {listed} (the inline form: {inline})"
                )),
                _ => {}
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn the_gaps_name_rows_and_forms_of_the_table_once() {
    let mut seen = BTreeSet::new();
    for (row, form, _) in GAPS {
        assert!(ROWS.iter().any(|r| r.name == *row), "no row {row}");
        assert!(
            FORMS.iter().any(|f| f.name() == *form) && *form != Form::Inline.name(),
            "no form {form} to list"
        );
        assert!(seen.insert((row, form)), "{row} / {form} twice");
    }
}
