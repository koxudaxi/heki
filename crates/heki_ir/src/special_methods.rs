//! The special methods Python looks up on the classes of an instruction's operands to run it:
//! the code an operator, a comparison, a conversion, a subscript, a display, or a truth test
//! runs besides its explicit calls. Whether one of them is code of the project is the
//! consumer's question (the classes the operand may be, under its own assumptions), and so is
//! what a builtin type's own method runs on the items it holds (a container's comparison, hash,
//! or `repr()` runs its items').

use crate::{
    BasicBlock, BinOpKind, BuiltinKind, CompareOpKind, Conversion, DictEntry, InstKind, IrFunction,
    Terminator, UnaryOpKind, VarId,
};

/// The special methods Python looks up to run an instruction: `methods` on the class of
/// `value`, and `item_methods` on the class of each item `value` holds. `keyed`: the
/// instruction looks a key up in `value` (a subscript), which a dict or a set compares with each
/// key it holds under an equal hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpecialMethods {
    pub value: VarId,
    pub methods: &'static [&'static str],
    pub item_methods: &'static [&'static str],
    pub keyed: bool,
}

/// A key of a mapping or a set is hashed and compared with the keys stored.
const HASHED: &[&str] = &["__hash__", "__eq__"];
/// A subscript's key: hashed for a mapping, converted to an index for a sequence.
const KEY: &[&str] = &["__hash__", "__eq__", "__index__"];
/// Iterating a value: its `__iter__` and the iterator's `__next__`, or `__getitem__`.
const ITERATED: &[&str] = &["__iter__", "__next__", "__getitem__"];
/// What `%` formatting runs on its right operand (and on each item of a tuple there): the
/// reflected operator, the conversions of `%s`, `%r`, `%b`, and the numeric formats, and the
/// mapping lookup of `%(name)s`.
const FORMATTED: &[&str] = &[
    "__rmod__",
    "__str__",
    "__repr__",
    "__bytes__",
    "__buffer__",
    "__index__",
    "__int__",
    "__float__",
    "__getitem__",
    "keys",
];
const FORMATTED_ITEM: &[&str] = &[
    "__str__",
    "__repr__",
    "__bytes__",
    "__buffer__",
    "__index__",
    "__int__",
    "__float__",
];
/// A truth test: `__bool__`, or `__len__` without it.
const TRUTH: &[&str] = &["__bool__", "__len__"];
/// What reading a value as a mapping runs (`**m`, `dict.update`): `keys()` and `__getitem__`,
/// or iterating it as pairs.
const MAPPING: &[&str] = &["keys", "__getitem__", "__iter__", "__next__"];
/// What an in-place `+=` runs on its right operand besides `__radd__`: a list extends itself by
/// iterating it, a `bytearray` reads its buffer.
const EXTENDED: &[&str] = &[
    "__radd__",
    "__iter__",
    "__next__",
    "__getitem__",
    "__buffer__",
];
/// What an in-place `|=` runs on its right operand besides `__ror__`: a dict updates itself from
/// a mapping or from pairs, each iterated, whose keys it hashes.
const UPDATED: &[&str] = &["__ror__", "keys", "__getitem__", "__iter__", "__next__"];
const UPDATED_ITEM: &[&str] = &["__hash__", "__eq__", "__iter__", "__next__", "__getitem__"];
/// What a subscript's value runs: an unpacking (`a, b = v`, lowered to subscripts) iterates
/// it.
const SUBSCRIPTED: &[&str] = &[
    "__getitem__",
    "__class_getitem__",
    "__missing__",
    "__iter__",
    "__next__",
];

/// Per operator, in `BinOpKind` order: the in-place method, the method, and the reflected one.
const OPERATORS: [[&str; 3]; 13] = [
    ["__iadd__", "__add__", "__radd__"],
    ["__isub__", "__sub__", "__rsub__"],
    ["__imul__", "__mul__", "__rmul__"],
    ["__itruediv__", "__truediv__", "__rtruediv__"],
    ["__ifloordiv__", "__floordiv__", "__rfloordiv__"],
    ["__imod__", "__mod__", "__rmod__"],
    ["__ipow__", "__pow__", "__rpow__"],
    ["__ior__", "__or__", "__ror__"],
    ["__iand__", "__and__", "__rand__"],
    ["__ixor__", "__xor__", "__rxor__"],
    ["__ilshift__", "__lshift__", "__rlshift__"],
    ["__irshift__", "__rshift__", "__rrshift__"],
    ["__imatmul__", "__matmul__", "__rmatmul__"],
];

impl BinOpKind {
    /// The methods the left operand's class runs: `__iadd__` then `__add__` for `x += y`,
    /// `__add__` for `x + y`; a sequence repeated by the left operand converts it to an index.
    fn left_methods(self, in_place: bool) -> &'static [&'static str] {
        match (self, in_place) {
            (Self::Mult, false) => &["__mul__", "__index__"],
            (Self::Mult, true) => &["__imul__", "__mul__", "__index__"],
            _ => {
                let row = &OPERATORS[self as usize];
                if in_place { &row[..2] } else { &row[1..2] }
            }
        }
    }

    /// The methods the right operand's class runs, and those of its items: the reflected one
    /// (`__radd__`), and what the builtin operator of a left operand reads of it (`%`
    /// formatting, a bytes-like concatenation's buffer, a sequence's repetition count, an
    /// in-place list extension or dict update).
    fn right_methods(self, in_place: bool) -> (&'static [&'static str], &'static [&'static str]) {
        match (self, in_place) {
            (Self::Mod, _) => (FORMATTED, FORMATTED_ITEM),
            (Self::Add, false) => (&["__radd__", "__buffer__"], &[]),
            (Self::Add, true) => (EXTENDED, &[]),
            (Self::BitOr, true) => (UPDATED, UPDATED_ITEM),
            (Self::Mult, _) => (&["__rmul__", "__index__"], &[]),
            _ => (&OPERATORS[self as usize][2..], &[]),
        }
    }
}

impl CompareOpKind {
    /// The methods each side's class runs: the comparison and its reflection (`a < b` runs
    /// `a.__lt__`, then `b.__gt__`); `!=` falls back to `__eq__`.
    fn sides(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            Self::Eq => (&["__eq__"], &["__eq__"]),
            Self::NotEq => (&["__ne__", "__eq__"], &["__ne__", "__eq__"]),
            Self::Lt => (&["__lt__"], &["__gt__"]),
            Self::Gt => (&["__gt__"], &["__lt__"]),
            Self::LtE => (&["__le__"], &["__ge__"]),
            Self::GtE => (&["__ge__"], &["__le__"]),
            Self::Is | Self::IsNot | Self::In | Self::NotIn => (&[], &[]),
        }
    }
}

impl IrFunction {
    /// The special methods running `kind` looks up on its operands' classes, a call's argument
    /// unpacking included. Attribute access, what a call calls, iteration steps, `await`,
    /// `yield`, class statements, and imports run code that is not a special method of an
    /// operand and are not listed: their consumers decide them.
    pub fn special_methods(&self, kind: &InstKind) -> Vec<SpecialMethods> {
        let on = |value: VarId, methods: &'static [&'static str]| SpecialMethods {
            value,
            methods,
            item_methods: &[],
            keyed: false,
        };
        let looked_up_in = |value: VarId, methods: &'static [&'static str]| SpecialMethods {
            keyed: true,
            ..on(value, methods)
        };
        match kind {
            InstKind::BinOp {
                op,
                lhs,
                rhs,
                in_place,
                ..
            } => {
                let (methods, item_methods) = op.right_methods(*in_place);
                vec![
                    on(*lhs, op.left_methods(*in_place)),
                    SpecialMethods {
                        value: *rhs,
                        methods,
                        item_methods,
                        keyed: false,
                    },
                ]
            }
            InstKind::UnaryOp { op, operand, .. } => {
                let methods: &'static [&'static str] = match op {
                    UnaryOpKind::Not => TRUTH,
                    UnaryOpKind::Invert => &["__invert__"],
                    UnaryOpKind::UAdd => &["__pos__"],
                    UnaryOpKind::USub => &["__neg__"],
                };
                vec![on(*operand, methods)]
            }
            InstKind::Compare { op, lhs, rhs, .. } => match op {
                CompareOpKind::In | CompareOpKind::NotIn => vec![
                    on(*lhs, HASHED),
                    SpecialMethods {
                        value: *rhs,
                        methods: &["__contains__", "__iter__", "__next__", "__getitem__"],
                        item_methods: &["__eq__"],
                        keyed: false,
                    },
                ],
                _ => {
                    let (left, right) = op.sides();
                    vec![on(*lhs, left), on(*rhs, right)]
                }
            },
            InstKind::Convert {
                value, conversion, ..
            } => {
                let methods: &'static [&'static str] = match conversion {
                    Conversion::Str => &["__str__", "__repr__"],
                    Conversion::Repr | Conversion::Ascii => &["__repr__"],
                };
                vec![on(*value, methods)]
            }
            // `object.__format__` runs `str()`.
            InstKind::Format { value, .. } => {
                vec![on(*value, &["__format__", "__str__", "__repr__"])]
            }
            InstKind::SubscriptLoad {
                obj,
                index,
                is_slice,
                ..
            } => vec![
                looked_up_in(*obj, SUBSCRIPTED),
                subscript_key(*index, *is_slice),
            ],
            InstKind::SubscriptStore { obj, index, .. } => {
                vec![
                    looked_up_in(*obj, &["__setitem__"]),
                    subscript_key(*index, false),
                ]
            }
            InstKind::SubscriptDelete { obj, index, .. } => {
                vec![
                    looked_up_in(*obj, &["__delitem__"]),
                    subscript_key(*index, false),
                ]
            }
            InstKind::AllocBuiltin { kind, elems, .. } => {
                let set = *kind == BuiltinKind::Set;
                elems
                    .iter()
                    .map(|&e| match (self.is_unpacked(e), set) {
                        (true, true) => SpecialMethods {
                            value: e,
                            methods: ITERATED,
                            item_methods: HASHED,
                            keyed: false,
                        },
                        (true, false) => on(e, ITERATED),
                        (false, true) => on(e, HASHED),
                        (false, false) => on(e, &[]),
                    })
                    .filter(|s| !s.methods.is_empty())
                    .collect()
            }
            InstKind::DictLiteral { entries, .. } => entries
                .iter()
                .map(|entry| match entry {
                    DictEntry::KeyValue { key, .. } => on(*key, HASHED),
                    DictEntry::Unpack { dict } => SpecialMethods {
                        value: *dict,
                        methods: MAPPING,
                        item_methods: HASHED,
                        keyed: false,
                    },
                })
                .collect(),
            // The outermost iterable's `iter()` runs where the generator is made.
            InstKind::MakeGenExpr { outer_iter, .. } => {
                vec![on(*outer_iter, &["__iter__", "__getitem__"])]
            }
            // A call's argument unpacking runs before the call: `*xs` iterates `xs`, `**m`
            // reads `m` as a mapping and hashes its keys.
            InstKind::Call {
                dst, args, kwargs, ..
            }
            | InstKind::MethodCall {
                dst, args, kwargs, ..
            } => args
                .iter()
                .enumerate()
                .filter(|(i, _)| self.is_starred(*dst, *i))
                .map(|(_, &a)| on(a, ITERATED))
                .chain(
                    kwargs
                        .iter()
                        .filter(|(name, _)| name.is_none())
                        .map(|&(_, m)| SpecialMethods {
                            value: m,
                            methods: MAPPING,
                            item_methods: HASHED,
                            keyed: false,
                        }),
                )
                .collect(),
            InstKind::EnterWith {
                mgr,
                is_async: false,
                ..
            } => vec![on(*mgr, &["__enter__"])],
            InstKind::ExitWith {
                mgr,
                is_async: false,
            } => vec![on(*mgr, &["__exit__"])],
            InstKind::Const { .. }
            | InstKind::LoadGlobal { .. }
            | InstKind::LoadDeref { .. }
            | InstKind::LoadName { .. }
            | InstKind::StoreGlobal { .. }
            | InstKind::StoreDeref { .. }
            | InstKind::Copy { .. }
            | InstKind::Delete { .. }
            | InstKind::BuildString { .. }
            | InstKind::AttrLoad { .. }
            | InstKind::AttrStore { .. }
            | InstKind::AttrDelete { .. }
            | InstKind::MakeIter { .. }
            | InstKind::IterNext { .. }
            | InstKind::MakeClosure { .. }
            | InstKind::Await { .. }
            | InstKind::Yield { .. }
            | InstKind::YieldFrom { .. }
            | InstKind::EnterWith { .. }
            | InstKind::ExitWith { .. }
            | InstKind::CatchException { .. }
            | InstKind::Import { .. } => Vec::new(),
        }
    }
}

impl BasicBlock {
    /// The special methods the block's terminator looks up: a branch tests its condition's
    /// truth, but a loop's branch on its next item (made by the block's `IterNext`) tests only
    /// whether there was one.
    pub fn end_special_methods(&self) -> Option<SpecialMethods> {
        match self.terminator {
            Terminator::Branch { cond, .. } => {
                let steps = self
                    .insts
                    .iter()
                    .any(|i| matches!(i.kind, InstKind::IterNext { dst, .. } if dst == cond));
                (!steps).then_some(SpecialMethods {
                    value: cond,
                    methods: TRUTH,
                    item_methods: &[],
                    keyed: false,
                })
            }
            Terminator::Jump(_)
            | Terminator::Return(_)
            | Terminator::Raise(_)
            | Terminator::Unreachable => None,
        }
    }
}

/// A subscript's key, or the bounds of a slice (a tuple of them, each converted to an index).
fn subscript_key(index: VarId, is_slice: bool) -> SpecialMethods {
    if is_slice {
        SpecialMethods {
            value: index,
            methods: &[],
            item_methods: &["__index__"],
            keyed: false,
        }
    } else {
        SpecialMethods {
            value: index,
            methods: KEY,
            item_methods: HASHED,
            keyed: false,
        }
    }
}

#[cfg(test)]
mod tests;
