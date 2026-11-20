//! The SQL a DB-API call runs, as far as the code fixes it: `EFF103` counts no query where a
//! schema or setting statement runs (it still blocks).

use super::Extractor;
use crate::resolver::Callee;
use heki_ir::{BinOpKind, BuiltinKind, InstKind, ModuleConstants, VarId};
use rustc_hash::{FxHashMap, FxHashSet};

/// DB-API (PEP 249) methods, and SQLAlchemy's `exec_driver_sql`, whose first argument is the
/// statement they run.
const SQL_EXECUTORS: &[&str] = &["execute", "executemany", "executescript", "exec_driver_sql"];

/// Leading keywords of statements that change a schema or a connection setting.
const DDL_KEYWORDS: &[&str] = &["CREATE", "ALTER", "DROP", "PRAGMA"];

/// Constructors of a SQLAlchemy statement from its text (`text("CREATE TABLE ...")`).
const SQL_TEXT_CONSTRUCTORS: &[&str] = &[
    "sqlalchemy.text",
    "sqlalchemy.sql.text",
    "sqlalchemy.sql.expression.text",
];

/// `str` methods whose result starts with the words their receiver starts with, in another case
/// or without the leading blanks.
const SAME_WORDS: &[&str] = &["strip", "lstrip", "rstrip", "upper", "lower", "casefold"];

/// The longest chain of steps the walk follows from a statement to the texts it is built from:
/// a string concatenated from more parts is not followed, so a generated one cannot exhaust the
/// stack.
const MAX_DEPTH: u32 = 64;

/// What the code fixes of one text a statement may be.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Text {
    /// The whole text.
    Whole(String),
    /// How the text starts: a literal followed by values only known at run time (`f"ALTER
    /// TABLE {name}"`, `"DROP TABLE %s" % name`).
    Head(String),
}

impl Text {
    /// The statement is DDL or a setting: its first word is a [`DDL_KEYWORDS`] keyword. A
    /// blank text runs no statement either. The first word of a head counts only when the head
    /// goes on past it (`CREATE{kind}` may be `CREATED`).
    fn is_ddl(&self) -> bool {
        let (text, whole) = match self {
            Text::Whole(t) => (t.trim_start(), true),
            Text::Head(h) => (h.trim_start(), false),
        };
        if whole && text.is_empty() {
            return true;
        }
        let word = match text.find(|c: char| !c.is_ascii_alphabetic()) {
            Some(end) => &text[..end],
            None if whole => text,
            None => return false,
        };
        DDL_KEYWORDS.iter().any(|k| word.eq_ignore_ascii_case(k))
    }

    /// The same text as the head of a longer one.
    fn into_head(self) -> Text {
        match self {
            Text::Whole(t) | Text::Head(t) => Text::Head(t),
        }
    }
}

/// The state of one walk from a statement ([`Extractor::statement_texts`]).
#[derive(Default)]
struct Walk {
    /// The phis the steps of the walk under way merge.
    merging: FxHashSet<VarId>,
    /// The steps from the statement to the value the walk is at.
    depth: u32,
    /// The answer for each value the walk has decided: a value reached again along another
    /// path (`sql = sql.upper() if flag else sql.lower()`) is decided once.
    decided: FxHashMap<VarId, Option<Vec<Text>>>,
}

impl Walk {
    /// `step` one step further from the statement, unless that passes [`MAX_DEPTH`].
    fn deeper<T>(&mut self, step: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        self.depth += 1;
        let out = step(self);
        self.depth -= 1;
        out
    }
}

impl Extractor<'_> {
    /// Whether the call of `symbol`, given `statement` first, runs a schema or setting statement
    /// (`cur.execute("CREATE TABLE IF NOT EXISTS ...")`): it is a statement executor, and every
    /// text the code lets the statement be is DDL. Other calls' first arguments are no
    /// statements (a Redis key named `created_at`).
    pub(super) fn runs_ddl(&self, symbol: &str, statement: Option<VarId>) -> bool {
        let executes = SQL_EXECUTORS.contains(&symbol.rsplit('.').next().unwrap_or(symbol));
        executes
            && statement
                .and_then(|v| self.statement_texts(v, &mut Walk::default()))
                .is_some_and(|texts| texts.iter().all(Text::is_ddl))
    }

    /// Every text `v` may hold, when the code fixes each (`None` when one is unknown), each
    /// once: the texts of every value the copies and phis from `v` merge
    /// ([`Self::source_texts`]), each phi once, so a cycle of copies and phis adds no text. A
    /// phi that an earlier step of the walk merges makes the texts unknown: the text may go
    /// around a cycle that changes it (`sql = sql + ", y"` in a loop).
    fn statement_texts(&self, v: VarId, walk: &mut Walk) -> Option<Vec<Text>> {
        if let Some(decided) = walk.decided.get(&v) {
            return decided.clone();
        }
        let texts = self.merged_texts(v, walk).map(|mut texts| {
            texts.sort();
            texts.dedup();
            texts
        });
        walk.decided.insert(v, texts.clone());
        texts
    }

    /// [`Self::statement_texts`] before it is kept, one step further from the statement.
    fn merged_texts(&self, v: VarId, walk: &mut Walk) -> Option<Vec<Text>> {
        walk.deeper(|walk| {
            let mut sources = Vec::new();
            let mut merged = Vec::new();
            let mut seen = FxHashSet::default();
            let mut stack = vec![v];
            while let Some(x) = stack.pop() {
                if !seen.insert(x) {
                    continue;
                }
                if let Some((_, phi)) = self.guard_inputs().phi(x) {
                    if walk.merging.contains(&x) {
                        return None;
                    }
                    merged.push(x);
                    stack.extend(phi.incoming.iter().map(|(_, input)| *input));
                } else if let Some(InstKind::Copy { src, .. }) = self.defs.get(&x).map(|i| &i.kind)
                {
                    stack.push(*src);
                } else {
                    sources.push(x);
                }
            }
            walk.merging.extend(merged.iter().copied());
            let texts: Option<Vec<Text>> = (sources.iter())
                .map(|s| self.source_texts(*s, walk))
                .collect::<Option<Vec<Vec<Text>>>>()
                .map(|texts| texts.into_iter().flatten().collect());
            for phi in &merged {
                walk.merging.remove(phi);
            }
            texts
        })
    }

    /// The texts the value `v` defines, which is no copy or phi: a constant (`const_str`, a
    /// module's string constant), the head of an f-string, of a `+` or `%` with a constant first
    /// operand, or of `.format(...)` on one, what a `str` method keeping the first word or
    /// SQLAlchemy's `text()` is given, none for a local read before it is bound, and each item a
    /// loop over constant texts yields.
    fn source_texts(&self, v: VarId, walk: &mut Walk) -> Option<Vec<Text>> {
        if let Some(s) = self.const_str(v) {
            return Some(vec![Text::Whole(s)]);
        }
        if let Some(head) = self.f.str_head(v) {
            return Some(vec![Text::Head(head.to_owned())]);
        }
        match &self.defs.get(&v)?.kind {
            InstKind::LoadGlobal { name, .. } => {
                Some(vec![Text::Whole(self.constants().strs.get(name)?.clone())])
            }
            // A local read before it is bound raises: no statement runs on that path.
            InstKind::LoadName { .. } if !self.f.is_module_body && !self.f.is_class_body => {
                Some(Vec::new())
            }
            InstKind::BinOp {
                op: BinOpKind::Add | BinOpKind::Mod,
                lhs,
                in_place: false,
                ..
            } => self.head_texts(*lhs, walk),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } => match method.as_str() {
                "format" => self.head_texts(*recv, walk),
                m if SAME_WORDS.contains(&m) && args.is_empty() && kwargs.is_empty() => {
                    self.statement_texts(*recv, walk)
                }
                _ => {
                    let callee = self.cx.resolve_method(self.val(*recv), method);
                    self.wrapped_statement(&callee, args, walk)
                }
            },
            InstKind::Call {
                callee_var, args, ..
            } => {
                let callee = self.cx.resolve_call(callee_var.and_then(|c| self.val(c)));
                self.wrapped_statement(&callee, args, walk)
            }
            // The iterator is a `MakeIter`, whose one operand is the iterable.
            InstKind::IterNext { iter, .. } => {
                let iterable = *self.defs.get(iter)?.kind.operands().first()?;
                self.item_texts(iterable, walk)
            }
            _ => None,
        }
    }

    /// The texts of `v` as heads of the longer texts built from it.
    fn head_texts(&self, v: VarId, walk: &mut Walk) -> Option<Vec<Text>> {
        Some(
            self.statement_texts(v, walk)?
                .into_iter()
                .map(Text::into_head)
                .collect(),
        )
    }

    /// The statement SQLAlchemy's `text(statement)` (the callee) builds from `args`.
    fn wrapped_statement(
        &self,
        callee: &Callee,
        args: &[VarId],
        walk: &mut Walk,
    ) -> Option<Vec<Text>> {
        match callee {
            Callee::External(symbol) if SQL_TEXT_CONSTRUCTORS.contains(&symbol.as_str()) => {
                self.statement_texts(*args.first()?, walk)
            }
            _ => None,
        }
    }

    /// Every item iterating `iterable` yields, when the code fixes each: the items of a tuple
    /// display or of a module's constant tuple, of a list or set display iterated where it is
    /// written, and the pieces `.split(sep)` / `.splitlines()` cut a whole text into.
    fn item_texts(&self, iterable: VarId, walk: &mut Walk) -> Option<Vec<Text>> {
        walk.deeper(|walk| self.items_by_definition(iterable, walk))
    }

    /// [`Self::item_texts`] from the definition of `iterable`.
    fn items_by_definition(&self, iterable: VarId, walk: &mut Walk) -> Option<Vec<Text>> {
        match &self.defs.get(&iterable)?.kind {
            // A list a name holds may have grown since its display.
            InstKind::Copy { src, .. } => match &self.defs.get(src)?.kind {
                InstKind::AllocBuiltin {
                    kind: BuiltinKind::Tuple,
                    ..
                }
                | InstKind::Copy { .. }
                | InstKind::LoadGlobal { .. } => self.item_texts(*src, walk),
                _ => None,
            },
            InstKind::AllocBuiltin {
                dst,
                kind: BuiltinKind::Tuple | BuiltinKind::List | BuiltinKind::Set,
                elems,
            } => {
                let mut out = Vec::new();
                for (i, e) in elems.iter().enumerate() {
                    if self.f.is_starred(*dst, i) {
                        return None;
                    }
                    out.extend(self.statement_texts(*e, walk)?);
                }
                Some(out)
            }
            InstKind::LoadGlobal { name, .. } => Some(
                (self.constants().str_tuples.get(name)?.iter())
                    .map(|s| Text::Whole(s.clone()))
                    .collect(),
            ),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } if kwargs.is_empty() => {
                let separator = match (method.as_str(), args.as_slice()) {
                    ("split", [sep]) => Some(self.const_str(*sep)?),
                    ("splitlines", []) => None,
                    _ => return None,
                };
                let mut out = Vec::new();
                for text in self.statement_texts(*recv, walk)? {
                    // A head may be cut anywhere after it.
                    let Text::Whole(whole) = text else {
                        return None;
                    };
                    // `split("")` raises before any statement runs.
                    let pieces: Vec<&str> = match &separator {
                        Some(sep) => whole.split(sep.as_str()).collect(),
                        None => lines(&whole).collect(),
                    };
                    out.extend(pieces.into_iter().map(|p| Text::Whole(p.to_owned())));
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// The constants of this module (`ModuleIr::constants`).
    fn constants(&self) -> &ModuleConstants {
        &self.cx.ir().constants
    }
}

/// The lines Python's `str.splitlines()` cuts `text` into: at `\n`, `\r`, `\r\n`, and the
/// other line boundaries it knows (`\v`, `\f`, `\x1c`-`\x1e`, `\x85`, `\u2028`, `\u2029`).
fn lines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let boundary = |c: char| {
            matches!(
                c,
                '\n' | '\r'
                    | '\x0b'
                    | '\x0c'
                    | '\x1c'
                    | '\x1d'
                    | '\x1e'
                    | '\u{85}'
                    | '\u{2028}'
                    | '\u{2029}'
            )
        };
        let Some(at) = rest.find(boundary) else {
            return Some(std::mem::take(&mut rest));
        };
        let line = &rest[..at];
        let width = if rest[at..].starts_with("\r\n") {
            2
        } else {
            rest[at..].chars().next().map_or(1, char::len_utf8)
        };
        rest = &rest[at + width..];
        Some(line)
    })
}
