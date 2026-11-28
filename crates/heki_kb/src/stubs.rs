//! The types typeshed's standard-library stubs declare for call results and attribute reads
//! (`kb/ecosystem/typeshed_stdlib.tsv`, written by `scripts/build_typeshed_kb.py` at a pinned
//! typeshed revision). Layer 2 data for the Advisory lane's receiver typing, reached only
//! through [`crate::EffectiveEcosystemKb::stubs`]: the Soundness lane never reads it (ADR-004).
//!
//! The rows keep their type text as written (the grammar is the script's docstring); the
//! Advisory lane decodes it with the receiver's type arguments.

use std::collections::HashMap;
use std::sync::OnceLock;

const TABLE: &str = include_str!("../../../kb/ecosystem/typeshed_stdlib.tsv");

/// The rows of the table by name, borrowing the embedded text.
#[derive(Debug, Default)]
pub struct StubTable {
    aliases: HashMap<&'static str, &'static str>,
    bases: HashMap<&'static str, &'static str>,
    calls: HashMap<&'static str, &'static str>,
    reads: HashMap<&'static str, &'static str>,
}

/// What the stubs declare a call returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StubCall {
    pub kind: StubKind,
    results: &'static str,
}

/// What a call row declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StubKind {
    /// A module function.
    Function,
    /// An instance method, which binds the instance it is called on.
    Method,
    /// A class or static method, which a call through the class object binds no instance to.
    OnClass,
}

impl StubCall {
    /// The declared results of the overloads, each with its subscript condition (`i:`, `s:`)
    /// if any; a call gives one of them.
    pub fn results(&self) -> impl Iterator<Item = &'static str> {
        self.results.split('\t')
    }
}

/// A member found along a class's MRO: `base` is the MRO entry that declares it (in the type
/// parameters of the class looked up), `None` for the class itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StubMember<T> {
    pub base: Option<&'static str>,
    pub found: T,
}

impl StubTable {
    pub(crate) fn embedded() -> &'static StubTable {
        static PARSED: OnceLock<StubTable> = OnceLock::new();
        PARSED.get_or_init(|| StubTable::parse(TABLE))
    }

    /// The digest input of the embedded table.
    pub(crate) fn text() -> &'static str {
        TABLE
    }

    fn parse(text: &'static str) -> StubTable {
        let mut table = StubTable::default();
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let Some((tag, rest)) = line.split_once('\t') else {
                continue;
            };
            let Some((name, value)) = rest.split_once('\t') else {
                continue;
            };
            let rows = match tag {
                "A" => &mut table.aliases,
                "C" => &mut table.bases,
                "F" => &mut table.calls,
                "R" => &mut table.reads,
                _ => continue,
            };
            rows.insert(name, value);
        }
        table
    }

    /// The spelling the table uses for the class or function `name` (a re-export, or a class's
    /// definition in a private module, names it otherwise).
    pub fn canonical<'a>(&self, name: &'a str) -> &'a str {
        self.aliases.get(name).copied().unwrap_or(name)
    }

    /// What calling the module function `fqn` returns.
    pub fn function(&self, fqn: &str) -> Option<StubCall> {
        self.call_row(self.canonical(fqn))
    }

    /// What calling the method `name` on an instance (or the class object) of `class` returns,
    /// from the first class along its MRO that declares it.
    pub fn method(&self, class: &str, name: &str) -> Option<StubMember<StubCall>> {
        self.along_mro(class, name, |key| self.call_row(key))
    }

    /// What reading the attribute or property `name` on an instance of `class` gives.
    pub fn attribute(&self, class: &str, name: &str) -> Option<StubMember<&'static str>> {
        self.along_mro(class, name, |key| self.reads.get(key).copied())
    }

    fn call_row(&self, key: &str) -> Option<StubCall> {
        let (kind, results) = self.calls.get(key)?.split_once('\t')?;
        let kind = match kind {
            "f" => StubKind::Function,
            "c" => StubKind::OnClass,
            _ => StubKind::Method,
        };
        Some(StubCall { kind, results })
    }

    /// The first of `class` and its MRO entries declaring `name`, looked up with `row`. A
    /// class or a base that declares `name` as the other kind of member (a method where an
    /// attribute is asked for) ends the walk: the name is bound there.
    fn along_mro<T>(
        &self,
        class: &str,
        name: &str,
        row: impl Fn(&str) -> Option<T>,
    ) -> Option<StubMember<T>> {
        let class = self.canonical(class);
        let declared = |owner: &str| {
            let key = format!("{owner}.{name}");
            let bound =
                self.calls.contains_key(key.as_str()) || self.reads.contains_key(key.as_str());
            (bound, row(&key))
        };
        match declared(class) {
            (_, Some(found)) => return Some(StubMember { base: None, found }),
            (true, None) => return None,
            (false, None) => {}
        }
        let bases = self.bases.get(class)?;
        for base in bases.split('\t') {
            let owner = base.split_once('[').map_or(base, |(head, _)| head);
            // A base is spelled by its definition module; its rows use the public name.
            match declared(self.canonical(owner)) {
                (_, Some(found)) => {
                    return Some(StubMember {
                        base: Some(base),
                        found,
                    });
                }
                (true, None) => return None,
                (false, None) => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> StubTable {
        StubTable::parse(concat!(
            "# header\n",
            "A\tm.Public\tm._impl.Base\n",
            "C\tm.Child\tm._impl.Base[~0, builtins.int]\n",
            "F\tm._impl.Base.get\tm\t~1\n",
            "F\tm._impl.Base.make\tc\tSelf\n",
            "F\tm.Child.items\tm\ti:~0\ts:Self\n",
            "F\tm.load\tf\tbuiltins.str\n",
            "R\tm._impl.Base.size\tbuiltins.int\n",
            "R\tm.Child.get\tbuiltins.str\n",
            "bad line\n",
            "F\tm.no_kind\n",
            "Z\tm.unknown_tag\tvalue\n",
        ))
    }

    #[test]
    fn rows_are_found_by_spelling_and_along_the_mro() {
        let t = table();
        assert_eq!(t.canonical("m.Public"), "m._impl.Base");
        assert_eq!(t.canonical("m.Other"), "m.Other");
        let load = t.function("m.load").unwrap();
        assert_eq!(load.kind, StubKind::Function);
        assert_eq!(load.results().collect::<Vec<_>>(), ["builtins.str"]);
        assert!(t.function("m.missing").is_none());
        let items = t.method("m.Child", "items").unwrap();
        assert_eq!(items.base, None);
        assert_eq!(
            items.found.results().collect::<Vec<_>>(),
            ["i:~0", "s:Self"]
        );
        let make = t.method("m.Child", "make").unwrap();
        assert_eq!(make.base, Some("m._impl.Base[~0, builtins.int]"));
        assert_eq!(make.found.kind, StubKind::OnClass);
        assert_eq!(items.found.kind, StubKind::Method);
        let size = t.attribute("m.Child", "size").unwrap();
        assert_eq!(size.found, "builtins.int");
        assert_eq!(t.attribute("m.Public", "size").unwrap().base, None);
        // `Child` binds `get` as an attribute: the base's method is not reached, and the other
        // way round for `items`.
        assert!(t.method("m.Child", "get").is_none());
        assert!(t.attribute("m.Child", "items").is_none());
        assert!(t.method("m.Child", "missing").is_none());
        assert!(t.method("m.Unknown", "get").is_none());
    }

    #[test]
    fn the_embedded_table_types_builtin_methods() {
        let t = StubTable::embedded();
        let strip = t.method("builtins.str", "strip").unwrap();
        assert_eq!(strip.found.results().collect::<Vec<_>>(), ["builtins.str"]);
        assert!(
            t.method("builtins.bool", "bit_length")
                .unwrap()
                .base
                .is_some()
        );
    }
}
