use super::*;
use crate::{ModuleIr, build_module_ir};
use heki_ast::parse_module;
use heki_env::{PythonVersion, RuntimeProfile};
use heki_semantic::analyze_module;

fn build(src: &str) -> ModuleIr {
    let parsed = parse_module(src).expect("valid python");
    let profile = RuntimeProfile::from(PythonVersion::PY315);
    let semantic = analyze_module(&parsed, src, profile);
    build_module_ir(&parsed, src, &semantic, profile)
}

/// The special methods the body `stmt` of `def f(a, b)` looks up on `a`, `b`, and on what is
/// built from them, as `name: methods / item methods`, in order.
fn looked_up(stmt: &str) -> Vec<String> {
    let ir = build(&format!("def f(a, b):\n    {stmt}\n"));
    let f = ir.functions.iter().find(|f| f.name == "f").expect("f");
    let named = |s: &SpecialMethods| {
        let name = f.var_name(s.value).unwrap_or("_");
        let keyed = if s.keyed { " (keyed)" } else { "" };
        format!(
            "{name}: {} / {}{keyed}",
            s.methods.join(" "),
            s.item_methods.join(" ")
        )
    };
    f.blocks
        .iter()
        .flat_map(|bb| {
            bb.insts
                .iter()
                .flat_map(|i| f.special_methods(&i.kind))
                .chain(bb.end_special_methods())
        })
        .map(|s| named(&s))
        .collect()
}

#[test]
fn operators_comparisons_conversions_subscripts_displays_and_tests_look_up_special_methods() {
    let cases: &[(&str, &[&str])] = &[
        ("x = a + b", &["a: __add__ / ", "b: __radd__ __buffer__ / "]),
        (
            "a += b",
            &[
                "a: __iadd__ __add__ / ",
                "b: __radd__ __iter__ __next__ __getitem__ __buffer__ / ",
            ],
        ),
        (
            "a |= b",
            &[
                "a: __ior__ __or__ / ",
                "b: __ror__ keys __getitem__ __iter__ __next__ / __hash__ __eq__ __iter__ __next__ __getitem__",
            ],
        ),
        (
            "x = a * b",
            &["a: __mul__ __index__ / ", "b: __rmul__ __index__ / "],
        ),
        (
            "a *= b",
            &[
                "a: __imul__ __mul__ __index__ / ",
                "b: __rmul__ __index__ / ",
            ],
        ),
        ("x = a @ b", &["a: __matmul__ / ", "b: __rmatmul__ / "]),
        (
            "x = a % b",
            &[
                "a: __mod__ / ",
                "b: __rmod__ __str__ __repr__ __bytes__ __buffer__ __index__ __int__ __float__ __getitem__ keys / __str__ __repr__ __bytes__ __buffer__ __index__ __int__ __float__",
            ],
        ),
        ("x = not a", &["a: __bool__ __len__ / "]),
        ("x = ~a", &["a: __invert__ / "]),
        ("x = +a", &["a: __pos__ / "]),
        ("x = -a", &["a: __neg__ / "]),
        ("x = a == b", &["a: __eq__ / ", "b: __eq__ / "]),
        (
            "x = a != b",
            &["a: __ne__ __eq__ / ", "b: __ne__ __eq__ / "],
        ),
        ("x = a < b", &["a: __lt__ / ", "b: __gt__ / "]),
        ("x = a > b", &["a: __gt__ / ", "b: __lt__ / "]),
        ("x = a <= b", &["a: __le__ / ", "b: __ge__ / "]),
        ("x = a >= b", &["a: __ge__ / ", "b: __le__ / "]),
        ("x = a is b", &["a:  / ", "b:  / "]),
        (
            "x = a in b",
            &[
                "a: __hash__ __eq__ / ",
                "b: __contains__ __iter__ __next__ __getitem__ / __eq__",
            ],
        ),
        (
            "x = f'{a!s}'",
            &[
                "a: __str__ __repr__ / ",
                "_: __format__ __str__ __repr__ / ",
            ],
        ),
        (
            "x = f'{a!r}'",
            &["a: __repr__ / ", "_: __format__ __str__ __repr__ / "],
        ),
        ("x = f'{a}'", &["a: __format__ __str__ __repr__ / "]),
        (
            "x = a[b]",
            &[
                "a: __getitem__ __class_getitem__ __missing__ __iter__ __next__ /  (keyed)",
                "b: __hash__ __eq__ __index__ / __hash__ __eq__",
            ],
        ),
        (
            "x = a[1:b]",
            &[
                "a: __getitem__ __class_getitem__ __missing__ __iter__ __next__ /  (keyed)",
                "_:  / __index__",
            ],
        ),
        // An unpacking reads the value by subscripts of constant indices.
        (
            "x, y = a",
            &[
                "a: __getitem__ __class_getitem__ __missing__ __iter__ __next__ /  (keyed)",
                "_: __hash__ __eq__ __index__ / __hash__ __eq__",
                "a: __getitem__ __class_getitem__ __missing__ __iter__ __next__ /  (keyed)",
                "_: __hash__ __eq__ __index__ / __hash__ __eq__",
            ],
        ),
        (
            "x = f(*a, **b)",
            &[
                "a: __iter__ __next__ __getitem__ / ",
                "b: keys __getitem__ __iter__ __next__ / __hash__ __eq__",
            ],
        ),
        ("x = a.m(1, *b)", &["b: __iter__ __next__ __getitem__ / "]),
        ("x = f(a, k=b)", &[]),
        (
            "a[b] = 1",
            &[
                "a: __setitem__ /  (keyed)",
                "b: __hash__ __eq__ __index__ / __hash__ __eq__",
            ],
        ),
        (
            "del a[b]",
            &[
                "a: __delitem__ /  (keyed)",
                "b: __hash__ __eq__ __index__ / __hash__ __eq__",
            ],
        ),
        (
            "x = {a, *b}",
            &[
                "a: __hash__ __eq__ / ",
                ".unpacked: __iter__ __next__ __getitem__ / __hash__ __eq__",
            ],
        ),
        (
            "x = [a, *b]",
            &[".unpacked: __iter__ __next__ __getitem__ / "],
        ),
        (
            "x = {a: 1, **b}",
            &[
                "a: __hash__ __eq__ / ",
                "b: keys __getitem__ __iter__ __next__ / __hash__ __eq__",
            ],
        ),
        ("x = (y for y in a)", &["a: __iter__ __getitem__ / "]),
        (
            "with a:\n        pass",
            &["a: __enter__ / ", "a: __exit__ / "],
        ),
        ("if a:\n        pass", &["a: __bool__ __len__ / "]),
        ("for x in a:\n        pass", &[]),
        ("x = (a, b)", &[]),
    ];
    for (stmt, expected) in cases {
        assert_eq!(looked_up(stmt), *expected, "{stmt}");
    }
}
