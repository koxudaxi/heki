//! The items of a call's result as a loop's origin (`EFF103`'s "Origin"): the rows a project
//! function returns, by what every `return` of it gives (`FunctionDecl::returns`), directly, in
//! another module, awaited, through `asyncio.to_thread`, or through `functools.partial`, are the
//! data's as the rows of the catalog call written in the loop are; and rows whose number the code
//! fixes (a `LIMIT` with a literal count, `fetchmany` of a constant, an aggregate without
//! `GROUP BY`) are no data origin, in the loop or in the function returning them.

mod common;

use common::Project;
use serde_json::Value;

/// `(line, code with its tier, message)` of every `EFF103` report in `pkg/app.py`.
fn reports(p: &Project) -> Vec<(u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--select", "EFF103", "--output-format", "json"]);
    assert!(code == 0 || code == 1, "exit {code}: {stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .filter(|d| d["path"] == "pkg/app.py")
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

const DB: &str = r#"import pathlib
import sqlite3

CONN = sqlite3.connect("app.db")


def authors(c: sqlite3.Connection) -> list[sqlite3.Row]:
    return c.execute("SELECT id FROM authors").fetchall()


def authors_cursor():
    return CONN.execute("SELECT id FROM authors")


def first_three():
    return CONN.execute("SELECT id FROM authors LIMIT 3").fetchall()


def either(flag: bool):
    if flag:
        return CONN.execute("SELECT id FROM a").fetchall()
    return CONN.execute("SELECT id FROM b").fetchmany(10)


def wrapped():
    return authors_cursor()


def recursive(n: int):
    if n:
        return recursive(n - 1)
    return CONN.execute("SELECT id FROM a").fetchall()


def ids():
    return [r[0] for r in CONN.execute("SELECT id FROM a")]


async def fetch_async():
    return CONN.execute("SELECT id FROM a").fetchall()


def book(i: int) -> object:
    return CONN.execute("SELECT * FROM b WHERE a = ?", (i,)).fetchone()


def mixed(flag: bool):
    if flag:
        return CONN.execute("SELECT id FROM a").fetchall()
    return CONN.execute("SELECT id FROM b")


def notes(p: pathlib.Path):
    return p.read_text()


def note_lines(p: pathlib.Path):
    return p.read_text().splitlines()


def fetch_some_text():
    return CONN.execute("SELECT id FROM a").fetchmany("3")
"#;

const APP: &str = r#"import asyncio
import functools
import pathlib

from pkg import db
from pkg.db import CONN, authors, book


def tp_cross_module() -> None:
    for r in db.authors(CONN):
        book(r[0])


def tp_cursor() -> None:
    for r in db.authors_cursor():
        book(r[0])


def tn_limit_helper() -> None:
    for r in db.first_three():
        book(r[0])


def tn_either() -> None:
    for r in db.either(True):
        book(r[0])


def tp_wrapped() -> None:
    for r in db.wrapped():
        book(r[0])


def tn_recursive() -> None:
    for r in db.recursive(2):
        book(r[0])


def tp_counted() -> None:
    for i in db.ids():
        book(i)


async def tp_awaited() -> None:
    for r in await db.fetch_async():
        book(r[0])


async def tp_to_thread() -> None:
    for r in await asyncio.to_thread(authors, CONN):
        book(r[0])


def tp_partial() -> None:
    get = functools.partial(authors, CONN)
    for r in get():
        book(r[0])


def tn_fetchmany() -> None:
    for r in CONN.execute("SELECT id FROM a").fetchmany(3):
        book(r[0])


def tn_count() -> None:
    for (n,) in CONN.execute("SELECT count(*) AS n FROM a WHERE x = 1"):
        book(n)


def tp_group() -> None:
    for (n,) in CONN.execute("SELECT count(*) FROM a GROUP BY x"):
        book(n)


def tp_limit_param(k: int) -> None:
    for r in CONN.execute("SELECT id FROM a LIMIT ?", (k,)):
        book(r[0])


def tp_inner_limit() -> None:
    for r in CONN.execute("SELECT id FROM a WHERE id IN (SELECT id FROM b LIMIT 3)"):
        book(r[0])


def tp_max_scalar() -> None:
    for r in CONN.execute("SELECT max(a, b) FROM t"):
        book(r[0])


def tn_mixed() -> None:
    for r in db.mixed(True):
        book(r[0])


def tp_text_lines(p: pathlib.Path) -> None:
    for line in db.notes(p).splitlines():
        book(int(line))


def tp_lines(p: pathlib.Path) -> None:
    for line in db.note_lines(p):
        book(int(line))


def tn_characters(p: pathlib.Path) -> None:
    for c in db.notes(p):
        book(int(c))


def tn_counted_filtered() -> None:
    for i in [i for i in db.ids() if i > 0]:
        book(i)


def tp_size_not_a_number() -> None:
    for r in db.fetch_some_text():
        book(r[0])
"#;

/// What every return of a helper gives decides the origin of a loop over its result.
#[test]
fn rows_a_function_returns_are_the_datas_and_rows_the_code_fixes_are_not() {
    let p = Project::new("returned_items");
    p.write("pkg/__init__.py", "")
        .write("pkg/db.py", DB)
        .write("pkg/app.py", APP);
    let exact = |what: &str| {
        format!(
            "every iteration over {what} sends a database statement: \
             `sqlite3.Connection.execute` (through `pkg.db.book`)"
        )
    };
    let heuristic = "`pkg.db.book` runs a database query (`sqlite3.Connection.execute`) on every \
                     loop iteration";
    let fetchall_of = |f: &str| {
        exact(&format!(
            "the rows of `sqlite3.Cursor.fetchall` that `pkg.db.{f}` returns"
        ))
    };
    let fetchmany_of = |f: &str| {
        exact(&format!(
            "the rows of `sqlite3.Cursor.fetchmany` that `pkg.db.{f}` returns"
        ))
    };
    let cursor_of = |f: &str| {
        exact(&format!(
            "the rows of `sqlite3.Connection.execute` that `pkg.db.{f}` returns"
        ))
    };
    let inline = exact("the rows of `sqlite3.Connection.execute`");
    let text_of = |f: &str| {
        exact(&format!(
            "the lines of the text of `pathlib.Path.read_text` that `pkg.db.{f}` returns"
        ))
    };
    let expected: Vec<(u64, &str, String)> = vec![
        (11, "EFF103", fetchall_of("authors")),
        (16, "EFF103", cursor_of("authors_cursor")),
        // `LIMIT 3` in the helper's query.
        (21, "EFF103 [heuristic]", heuristic.to_owned()),
        // One return gives at most ten rows (`fetchmany(10)`).
        (26, "EFF103 [heuristic]", heuristic.to_owned()),
        (31, "EFF103", cursor_of("wrapped")),
        // A helper calling itself gives no fact.
        (36, "EFF103 [heuristic]", heuristic.to_owned()),
        (
            41,
            "EFF103",
            "every iteration, once for each item of the rows of `sqlite3.Connection.execute` \
             that `pkg.db.ids` returns, sends a database statement: `sqlite3.Connection.execute` \
             (through `pkg.db.book`)"
                .to_owned(),
        ),
        (46, "EFF103", fetchall_of("fetch_async")),
        (51, "EFF103", fetchall_of("authors")),
        (57, "EFF103", fetchall_of("authors")),
        (62, "EFF103 [heuristic]", heuristic.to_owned()),
        (67, "EFF103 [heuristic]", heuristic.to_owned()),
        (72, "EFF103", inline.clone()),
        (77, "EFF103", inline.clone()),
        (82, "EFF103", inline.clone()),
        (87, "EFF103", inline),
        // One return gives rows, the other a cursor.
        (92, "EFF103 [heuristic]", heuristic.to_owned()),
        (97, "EFF103", text_of("notes")),
        (102, "EFF103", text_of("note_lines")),
        // Iterating text gives its characters.
        (107, "EFF103 [heuristic]", heuristic.to_owned()),
        // One item per row, then some of them: no count of the rows follows.
        (112, "EFF103 [heuristic]", heuristic.to_owned()),
        // A size that is no number fixes no count of rows.
        (117, "EFF103", fetchmany_of("fetch_some_text")),
    ];
    let expected: Vec<(u64, String, String)> = expected
        .into_iter()
        .map(|(line, code, message)| (line, code.to_owned(), message))
        .collect();
    assert_eq!(reports(&p), expected);
}
