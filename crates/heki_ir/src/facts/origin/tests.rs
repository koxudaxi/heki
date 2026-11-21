use super::*;
use crate::build_module_ir;
use heki_ast::parse_module;
use heki_env::{PythonVersion, RuntimeProfile};
use heki_semantic::analyze_module;

fn build(src: &str, version: PythonVersion) -> ModuleIr {
    let parsed = parse_module(src).expect("valid python");
    let profile = RuntimeProfile::from(version);
    let semantic = analyze_module(&parsed, src, profile);
    build_module_ir(&parsed, src, &semantic, profile)
}

/// The origins of each loop of function `name` whose header is on a line holding `marker`, in
/// block order, written `param0`, `part0`, `receiver`, `fixed`, or `call:<text>`, with `/count`
/// and `/text` for what carries only a count or pieces of text; `open` when not established.
fn origins_at(src: &str, name: &str, marker: &str, version: PythonVersion) -> Vec<String> {
    let ir = build(src, version);
    let f = ir
        .functions
        .iter()
        .find(|f| f.name == name)
        .expect("function");
    let origins = ItemOrigins::new(&ir);
    let budget = Budget::of_function("test", f);
    let line_of = |at: u32| src[..at as usize].lines().count();
    let mut out = Vec::new();
    for (li, lp) in f.loops.iter().enumerate() {
        let header = f.block(lp.header);
        let Some(first) = header.insts.first() else {
            continue;
        };
        let line = src.lines().nth(line_of(first.span.start) - 1).unwrap_or("");
        if !line.contains(marker) {
            continue;
        }
        let text = match origins.of_loop(&budget, f.id, li) {
            Answer::Open => "open".to_owned(),
            Answer::Known(set) => set
                .iter()
                .map(|o| {
                    let mut s = match o.source {
                        ItemSource::Param(p) => format!("param{p}"),
                        ItemSource::ParamPart(p) => format!("part{p}"),
                        ItemSource::Receiver => "receiver".to_owned(),
                        ItemSource::Fixed => "fixed".to_owned(),
                        ItemSource::Call { block, index } => {
                            let span = f.block(block).insts[index as usize].span;
                            format!("call:{}", &src[span.start as usize..span.end as usize])
                        }
                    };
                    if o.carries == Carries::Count {
                        s.push_str("/count");
                    }
                    if o.text {
                        s.push_str("/text");
                    }
                    s
                })
                .collect::<Vec<_>>()
                .join(" "),
        };
        out.push(text);
    }
    out
}

fn origins(src: &str, name: &str, marker: &str) -> Vec<String> {
    origins_at(src, name, marker, PythonVersion::PY315)
}

#[test]
fn items_pass_through_the_wrappers_that_keep_them_and_counts_through_the_rest() {
    let src = r#"
NAMES = ("a", "b")

def f(rows, payload, ids, conn, path, json):
    for x in rows:  # L
        pass
    for x in sorted(rows):  # L
        pass
    for i, x in enumerate(list(filter(None, rows))):  # L
        pass
    for k, v in payload.items():  # L
        pass
    for x in payload["rows"][1:]:  # L
        pass
    for a, b in zip(rows, ids):  # L
        pass
    for x in map(str, rows):  # L
        pass
    for i in range(len(rows)):  # L
        pass
    for i in range(10):  # L
        pass
    for x in NAMES:  # L
        pass
    for row in conn.execute("q"):  # L
        pass
    for line in path.read_text().strip().splitlines():  # L
        pass
    for part in "a;b".split(";"):  # L
        pass
    for part in payload.name.split("."):  # L
        pass
    for x in json.loads(payload):  # L
        pass
    for x in ids or []:  # L
        pass
    for batch in rows:
        for x in batch:  # L
            pass
    for row in conn.fetchall():
        for cell in row:  # L
            pass
    for x in map(str, rows)[0]:  # L
        pass
    for x in UNKNOWN:  # L
        pass
    for line in os.popen(cmd).readlines():  # L
        pass
    for line in os.popen(cmd).read().splitlines():  # L
        pass
"#;
    assert_eq!(
        origins(src, "f", "# L"),
        vec![
            "param0",
            "param0",
            "param0",
            "param1",
            "part1",
            "param0 param2",
            "param0/count",
            "param0/count",
            "fixed",
            "fixed",
            "call:conn.execute(\"q\")",
            "call:path.read_text()/text",
            "fixed",
            "part1/text",
            "call:json.loads(payload)",
            "param2",
            "part0",
            "open",
            "open",
            "open",
            // The reader resolves these calls through the catalog: lines of a process's output.
            "call:os.popen(cmd).readlines()",
            "call:os.popen(cmd).read()/text",
        ]
    );
}

#[test]
fn a_local_named_like_a_module_constant_is_not_the_constant() {
    let src = r#"
NAMES = ("a", "b")

def g(rows):
    NAMES = rows
    del NAMES
    for x in NAMES:  # L
        pass
"#;
    assert_eq!(origins(src, "g", "# L"), vec!["open"]);
}

#[test]
fn comprehensions_and_appended_lists_keep_the_items_they_produce() {
    let src = r#"
class Store:
    def run(self, rows, other):
        kept = [x for x in rows if x]
        for x in kept:  # L
            pass
        mapped = [x.id for x in rows]
        for x in mapped:  # L
            pass
        for x in (r for r in rows if r):  # L
            pass
        for x in (str(r) for r in rows):  # L
            pass
        out = []
        for r in rows:
            out.append(r)
        for x in out:  # L
            pass
        both = []
        for r in rows:
            both.append(r)
        for r in other:
            both.append(r)
        for x in both:  # L
            pass
        passed = []
        other.fill(passed)
        for x in passed:  # L
            pass
        todo = [rows]
        for x in todo:  # L
            todo.append(x)
        for x in self.rows:  # L
            pass
        for x in [*rows, 1]:  # L
            pass
"#;
    assert_eq!(
        origins(src, "run", "# L"),
        vec![
            "param1",
            "param1/count",
            "param1",
            "param1/count",
            "param1",
            "param1 param2",
            "open",
            "open",
            "receiver",
            "param1 fixed",
        ]
    );
}

#[test]
fn a_comprehension_function_before_312_returns_the_list_it_fills() {
    let src = r#"
def f(rows):
    kept = [x for x in rows if x]
    for x in kept:  # L
        pass
"#;
    assert_eq!(
        origins_at(src, "f", "# L", PythonVersion::PY311),
        vec!["param0"]
    );
}

#[test]
fn a_phi_takes_only_the_inputs_along_edges_that_can_run() {
    let src = r#"
_IS_SYNC = False

def f(rows, other):
    xs = other if _IS_SYNC else rows
    for x in xs:  # L
        pass
    ys = other if rows else rows
    for x in ys:  # L
        pass
    zs = rows
    while zs:
        zs = zs[1:]
    for x in zs:  # L
        pass
"#;
    assert_eq!(
        origins(src, "f", "# L"),
        vec!["param0", "param0 param1", "param0"]
    );
}

#[test]
fn steps_compose_in_the_order_python_applies_them() {
    let items = Steps::default();
    assert_eq!(
        items.apply(ItemSource::Param(2)).map(|o| o.source),
        Some(ItemSource::Param(2))
    );
    let part = items.with(Steps::PART);
    assert_eq!(
        part.apply(ItemSource::Param(2)).map(|o| o.source),
        Some(ItemSource::ParamPart(2))
    );
    assert_eq!(
        part.apply(ItemSource::Call {
            block: BlockId(0),
            index: 0
        }),
        None
    );
    // A count of an item is a count; an item of a count is unknown.
    assert!(part.counted().is_none());
    assert!(items.with(Steps::PIECES).counted().is_none());
    let counted = items.counted().unwrap();
    assert_eq!(
        counted
            .with(Steps::PART)
            .apply(ItemSource::Fixed)
            .map(|o| o.carries),
        Some(Carries::Count)
    );
    assert_eq!(
        items
            .with(Steps::TEXT)
            .apply(ItemSource::Fixed)
            .map(|o| o.text),
        Some(false)
    );
}

#[test]
fn every_other_shape_names_its_sources_or_none() {
    let src = r#"
async def f(rows, payload, ids, n, open_, p, fetch, flag):
    for x in await fetch():  # L
        pass
    for k in {"a": 1, **payload}:  # L
        pass
    grown = {"a": 1}
    grown["b"] = 2
    for k in grown:  # L
        pass
    for x in rows + ids:  # L
        pass
    for x in rows * 2:  # L
        pass
    for x in zip(*rows):  # L
        pass
    for i in range(1, len(rows)):  # L
        pass
    for i in range(n):  # L
        pass
    with open_(p) as fh:
        for line in fh:  # L
            pass
    copied = list(rows)
    copied.append(1)
    for x in copied:  # L
        pass
    single = []
    single.append(n)
    for x in single:  # L
        pass
    pairs = [v for k, v in payload.items()]
    for x in pairs:  # L
        pass
    drained = []
    while rows:
        drained.append(rows.pop())
    for x in drained:  # L
        pass
    merged = []
    if flag:
        merged = []
    for r in rows:
        merged.append(r)
    for x in merged:  # L
        pass
    while len(rows):  # L
        pass
"#;
    assert_eq!(
        origins(src, "f", "# L"),
        vec![
            "call:fetch()",
            "param1 fixed",
            "open",
            "param0 param2",
            "open",
            "open",
            "param0/count",
            "open",
            "call:open_(p)",
            "call:list(rows)",
            "fixed",
            // A component of an item is one of the items' parts: the dict's values.
            "param1",
            "open",
            "param0",
            "open",
        ]
    );
}

#[test]
fn a_loop_of_the_module_body_reads_module_constants() {
    let src = "NAMES = (\"a\", \"b\")\nfor name in NAMES:  # L\n    pass\n";
    assert_eq!(origins(src, "<module>", "# L"), vec!["fixed"]);
}
