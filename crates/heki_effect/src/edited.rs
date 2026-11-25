//! A file analyzed again with edits of its parameter annotations applied, against the facts the
//! index holds for it: the annotation trial of `heki recommend` (ADR-023, decision 4).
//!
//! The comparison is Early Cutoff's ([`EffectSummary::same_facts`]) widened to every local fact
//! a file has: its functions' summaries, call sites and references, declarations, flows, and
//! opt-in tables. The positions Early Cutoff ignores stay ignored; every other position of the
//! edited file is compared at its offset in the original file ([`OffsetMap`]). What the edit
//! states by itself ([`Restated`]) is left out: the annotations' own facts, the changed
//! parameters' read-only and mutable bits, and the names the added imports bind. Of an edited
//! function's annotation another file reads only those bits, as their union (`MUT110`'s test of
//! a callee), which a mutable head turned read-only keeps: when no other fact of the file
//! differs, no other file's summary does (ADR-017).

use crate::imports::ModuleImports;
use crate::resolver::{FunctionDecl, ModuleDecls};
use crate::{CallSite, EffectSummary, FileSummary, FuncKey, OptInTables};
use heki_ast::Span;
use rustc_hash::FxHashMap;
use serde::Serialize;
use serde::ser::{self, SerializeMap, SerializeSeq, SerializeStruct};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Where the offsets of a file with edits applied were in the original file.
#[derive(Debug, Clone, Default)]
pub struct OffsetMap(Vec<Placed>);

/// One applied edit: its text's offsets in the edited file, the start of the span it replaced,
/// and what to add to an edited offset after it to get the original one.
#[derive(Debug, Clone, Copy)]
struct Placed {
    start: u32,
    end: u32,
    original: u32,
    shift: i64,
}

impl OffsetMap {
    /// `source` with `edits` applied (spans of `source`; equal edits once, insertions at one
    /// offset in the order of their text), and the map of its offsets.
    pub fn apply(source: &str, edits: &[(Span, String)]) -> (String, Self) {
        let mut edits: Vec<&(Span, String)> = edits.iter().collect();
        edits.sort_by(|a, b| (a.0.start, a.0.end, &a.1).cmp(&(b.0.start, b.0.end, &b.1)));
        edits.dedup();
        let mut text = String::with_capacity(source.len() + 64);
        let mut placed = Vec::with_capacity(edits.len());
        let mut from = 0usize;
        for (span, replacement) in edits {
            // An edit overlapping the one before starts where that one ends.
            text.push_str(source.get(from..span.start as usize).unwrap_or_default());
            let start = text.len();
            text.push_str(replacement);
            from = from.max(span.end as usize);
            placed.push(Placed {
                start: offset(start),
                end: offset(text.len()),
                original: span.start,
                shift: i64::from(span.end) - text.len() as i64,
            });
        }
        text.push_str(&source[from..]);
        (text, Self(placed))
    }

    /// The offset in the original file of `offset` in the edited one: a position inside an
    /// edit's text is the start of the span it replaced.
    pub fn original(&self, offset: u32) -> u32 {
        let after = self.0.partition_point(|p| p.start <= offset);
        let Some(p) = after.checked_sub(1).map(|i| self.0[i]) else {
            return offset;
        };
        if offset < p.end {
            return p.original;
        }
        u32::try_from(i64::from(offset) + p.shift).unwrap_or(0)
    }

    /// Whether `offset` of the edited file is inside the text of an edit.
    pub fn in_edit(&self, offset: u32) -> bool {
        let after = self.0.partition_point(|p| p.start <= offset);
        after.checked_sub(1).is_some_and(|i| offset < self.0[i].end)
    }
}

fn offset(at: usize) -> u32 {
    u32::try_from(at).unwrap_or(u32::MAX)
}

/// What an edit of a file's parameter annotations states by itself, which
/// [`FileSummary::edit_changes`] leaves out.
#[derive(Debug, Clone, Default)]
pub struct Restated {
    /// By function (qualified name), the parameters whose annotation the edit changes: bit `i`
    /// for parameter `i`.
    pub params: BTreeMap<String, u64>,
    /// The names the import statements the edit adds bind.
    pub imports: BTreeSet<String>,
    /// The first names of the annotation heads it replaces and of their replacements (`list`,
    /// `Sequence`, `t` of `t.Sequence`): a class body that defines the function loads them,
    /// and `ClassDecl::code_attrs` counts the names a class body loads.
    pub head_names: BTreeSet<String>,
    /// What the heads it replaces and their replacements name (`collections.abc.Mapping`): the
    /// module hands such an object to the function's annotations, which
    /// `ModuleDecls::grown_globals` counts.
    pub head_targets: BTreeSet<String>,
}

/// A local fact of the edited file that differs from the original file's.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FactChange {
    /// The function whose facts differ (qualified name); `None` for the module's own facts
    /// (its classes, globals, and imports).
    pub function: Option<String>,
    /// The fields or tables that differ.
    pub what: Vec<String>,
}

impl FileSummary {
    /// The local facts of `edited`, this file's module analyzed with edits of its parameter
    /// annotations applied (`map`), that differ from this file's, apart from what the edits
    /// restate: one per function and one for the module, in order.
    pub fn edit_changes(
        &self,
        edited: &FileSummary,
        restated: &Restated,
        map: &OffsetMap,
    ) -> Vec<FactChange> {
        // Every field is named: a new one is compared or left out here.
        let FileSummary {
            path: _,
            module_name: _,
            content_hash: _,
            // Final facts: propagation recomputes them from the local ones.
            functions: _,
            local_functions,
            call_sites,
            reference_sites,
            decls,
            // What summarizing read, for the language server's invalidation: no fact.
            decl_reads: _,
            flows,
            cache_flows,
            external_accesses,
            opt_in_tables,
        } = edited;
        let mut changes = Changes::default();
        let comparable = |s: &Arc<EffectSummary>| EffectSummary {
            // The annotations' own facts: what the edit writes.
            container_params: None,
            ..s.without_locations()
        };
        for (key, now, then) in by_function(&self.local_functions, local_functions) {
            let fields = differing_fields(&now.map(comparable), &then.map(comparable), map);
            changes.add(Some(key), fields);
        }
        for (name, now, then) in [
            ("call_sites", &self.call_sites, call_sites),
            ("reference_sites", &self.reference_sites, reference_sites),
        ] {
            for (caller, a, b) in by_caller(now, then) {
                if !same(&a, &b, map) {
                    changes.add(Some(caller), vec![name.to_owned()]);
                }
            }
        }
        changes.tables("flows", &self.flows, flows, map);
        changes.tables("cache_flows", &self.cache_flows, cache_flows, map);
        changes.tables(
            "external_accesses",
            &self.external_accesses,
            external_accesses,
            map,
        );
        changes.decls(&self.decls, decls, restated, map);
        let none = OptInTables::default();
        let tables = |t: &Option<Box<OptInTables>>| t.as_deref().unwrap_or(&none).clone();
        changes.opt_in(
            tables(&self.opt_in_tables),
            tables(opt_in_tables),
            restated,
            map,
        );
        changes
            .0
            .into_iter()
            .map(|(function, what)| FactChange {
                function,
                what: what.into_iter().collect(),
            })
            .collect()
    }
}

/// The differences found so far, by function.
#[derive(Default)]
struct Changes(BTreeMap<Option<String>, BTreeSet<String>>);

impl Changes {
    fn add(&mut self, function: Option<&str>, what: Vec<String>) {
        if !what.is_empty() {
            let entry = self.0.entry(function.map(str::to_owned)).or_default();
            entry.extend(what);
        }
    }

    /// A table of facts by function.
    fn tables<T: Serialize>(
        &mut self,
        name: &str,
        now: &FxHashMap<FuncKey, T>,
        then: &FxHashMap<FuncKey, T>,
        map: &OffsetMap,
    ) {
        for (key, a, b) in by_function(now, then) {
            if !same(&a, &b, map) {
                self.add(Some(key), vec![name.to_owned()]);
            }
        }
    }

    /// The declarations: the functions' by function, the rest the module's. The changed
    /// parameters' bits and the added imports are what the edit says.
    fn decls(
        &mut self,
        now: &ModuleDecls,
        then: &ModuleDecls,
        restated: &Restated,
        map: &OffsetMap,
    ) {
        let (mut now, mut then) = (now.clone(), then.clone());
        let (functions_now, functions_then) = (
            std::mem::take(&mut now.functions),
            std::mem::take(&mut then.functions),
        );
        // The added imports' own entries: the binding, and what the module's tables of its
        // globals say of it. An edit imports only a name the module does not bind.
        for name in &restated.imports {
            then.imports.remove(name);
            then.global_types.remove(name);
            then.global_capacities.remove(name);
            if let Some(rebound) = &mut then.rebound_imports {
                rebound.remove(name);
            }
        }
        if then
            .rebound_imports
            .as_deref()
            .is_some_and(BTreeSet::is_empty)
        {
            then.rebound_imports = None;
        }
        for class in now.classes.values_mut().chain(then.classes.values_mut()) {
            (class.code_attrs).retain(|name| !restated.head_names.contains(name));
        }
        for decls in [&mut now, &mut then] {
            (decls.grown_globals).retain(|fqn| !restated.head_targets.contains(fqn));
        }
        let fields = differing_fields(&now, &then, map);
        self.add(None, fields.iter().map(|f| format!("decls.{f}")).collect());
        let keys: BTreeSet<&String> = functions_now.keys().chain(functions_then.keys()).collect();
        for qualname in keys {
            let mask = !restated.params.get(qualname).copied().unwrap_or(0);
            let unrestated = |decls: &BTreeMap<String, Box<FunctionDecl>>| {
                decls.get(qualname).map(|d| FunctionDecl {
                    readonly_params: d.readonly_params & mask,
                    mutable_params: d.mutable_params & mask,
                    ..(**d).clone()
                })
            };
            let fields = differing_fields(
                &unrestated(&functions_now),
                &unrestated(&functions_then),
                map,
            );
            self.add(
                Some(qualname),
                fields.iter().map(|f| format!("decls.{f}")).collect(),
            );
        }
    }

    /// The opt-in tables, without the annotations' own facts and the added import sites.
    fn opt_in(
        &mut self,
        mut now: OptInTables,
        mut then: OptInTables,
        restated: &Restated,
        map: &OffsetMap,
    ) {
        for t in [&mut now, &mut then] {
            // What the edit writes, and final facts propagation recomputes.
            t.annotation_facts.clear();
            t.fresh_returns.resolved.clear();
        }
        self.tables(
            "fresh_returns",
            &std::mem::take(&mut now.fresh_returns.local),
            &std::mem::take(&mut then.fresh_returns.local),
            map,
        );
        self.tables(
            "blind_spots",
            &std::mem::take(&mut now.blind_spots),
            &std::mem::take(&mut then.blind_spots),
            map,
        );
        self.tables(
            "unawaited_handoffs",
            &std::mem::take(&mut now.unawaited_handoffs),
            &std::mem::take(&mut then.unawaited_handoffs),
            map,
        );
        then.imports = (then.imports.take())
            .map(|i| Box::new(i.without_sites(|s| map.in_edit(s.span.start), &restated.imports)));
        let fields = differing_fields(&now, &then, map);
        self.add(None, fields);
    }
}

/// The functions of two tables in order, each with its entry in both.
fn by_function<'a, T>(
    now: &'a FxHashMap<FuncKey, T>,
    then: &'a FxHashMap<FuncKey, T>,
) -> Vec<(&'a str, Option<&'a T>, Option<&'a T>)> {
    let mut keys: Vec<&FuncKey> = now.keys().chain(then.keys()).collect();
    keys.sort();
    keys.dedup();
    (keys.into_iter())
        .map(|k| (k.qualname.as_str(), now.get(k), then.get(k)))
        .collect()
}

/// The sites of two lists by caller, in order, each in source order.
fn by_caller<'a>(
    now: &'a [CallSite],
    then: &'a [CallSite],
) -> Vec<(&'a str, Vec<&'a CallSite>, Vec<&'a CallSite>)> {
    let mut callers: BTreeMap<&str, (Vec<&CallSite>, Vec<&CallSite>)> = BTreeMap::new();
    for s in now {
        callers.entry(&s.caller.qualname).or_default().0.push(s);
    }
    for s in then {
        callers.entry(&s.caller.qualname).or_default().1.push(s);
    }
    (callers.into_iter()).map(|(c, (a, b))| (c, a, b)).collect()
}

/// Whether `now` (of the original file) and `then` (of the edited one, at offsets `map` takes
/// back) are equal.
fn same<T: Serialize + ?Sized>(now: &T, then: &T, map: &OffsetMap) -> bool {
    match (Canon::tokens(now, None), Canon::tokens(then, Some(map))) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The fields of the structs `now` and `then` that differ, as [`same`] compares them: every
/// field of the one that is there when the other is `None`.
fn differing_fields<T: Serialize>(now: &T, then: &T, map: &OffsetMap) -> Vec<String> {
    let (Some(a), Some(b)) = (Canon::tokens(now, None), Canon::tokens(then, Some(map))) else {
        return vec!["fields".to_owned()];
    };
    if a == b {
        return Vec::new();
    }
    let (a, b) = (fields(&a), fields(&b));
    let names: BTreeSet<&str> = a.keys().chain(b.keys()).copied().collect();
    (names.into_iter())
        .filter(|n| a.get(n) != b.get(n))
        .map(str::to_owned)
        .collect()
}

/// The fields of a struct's tokens (also of `Some` struct), each with the tokens of its value.
fn fields(tokens: &[Tok]) -> BTreeMap<&'static str, &[Tok]> {
    let tokens = tokens.strip_prefix(&[Tok::Some]).unwrap_or(tokens);
    let mut out = BTreeMap::new();
    let inner = tokens
        .get(1..tokens.len().saturating_sub(1))
        .unwrap_or_default();
    let mut depth = 0usize;
    let mut current: Option<(&'static str, usize)> = None;
    for (i, t) in inner.iter().enumerate() {
        match t {
            Tok::Field(name) if depth == 0 => {
                if let Some((n, from)) = current.replace((name, i + 1)) {
                    out.insert(n, &inner[from..i]);
                }
            }
            Tok::Open(_) => depth += 1,
            Tok::Close => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if let Some((n, from)) = current {
        out.insert(n, &inner[from..]);
    }
    out
}

/// One step of a value as [`Canon`] writes it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Tok {
    Open(&'static str),
    Close,
    Field(&'static str),
    Variant(&'static str),
    Some,
    None,
    Unit,
    Bool(bool),
    Int(i128),
    Float(u64),
    Char(char),
    Str(Box<str>),
    Bytes(Box<[u8]>),
}

/// A value written as tokens, with every `Span` in it at its offsets in the original file
/// (`map`) and the entries of every map in one order, whichever order the map keeps.
struct Canon<'m> {
    out: Vec<Tok>,
    map: Option<&'m OffsetMap>,
}

impl Canon<'_> {
    /// `None` when the value cannot be written.
    fn tokens<T: Serialize + ?Sized>(value: &T, map: Option<&OffsetMap>) -> Option<Vec<Tok>> {
        let mut canon = Canon {
            out: Vec::new(),
            map,
        };
        value.serialize(&mut canon).ok()?;
        Some(canon.out)
    }

    fn push(&mut self, t: Tok) -> Result<(), Unwritable> {
        self.out.push(t);
        Ok(())
    }
}

/// A value a `Serialize` implementation refuses to write.
#[derive(Debug)]
struct Unwritable;

impl std::fmt::Display for Unwritable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the value cannot be written")
    }
}

impl std::error::Error for Unwritable {}

impl ser::Error for Unwritable {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Self
    }
}

/// A compound value being written: a sequence, a tuple, a map (the token each entry starts at),
/// or a struct (a `Span`'s first token).
struct Compound<'a, 'm> {
    canon: &'a mut Canon<'m>,
    entries: Vec<usize>,
    span_at: Option<usize>,
}

impl<'a, 'm> Compound<'a, 'm> {
    fn open(canon: &'a mut Canon<'m>, kind: &'static str) -> Self {
        canon.out.push(Tok::Open(kind));
        Self {
            canon,
            entries: Vec::new(),
            span_at: None,
        }
    }

    fn element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unwritable> {
        value.serialize(&mut *self.canon)
    }

    fn close(self) -> Result<(), Unwritable> {
        self.canon.push(Tok::Close)
    }
}

type Done = Result<(), Unwritable>;

impl<'a, 'm> ser::Serializer for &'a mut Canon<'m> {
    type Ok = ();
    type Error = Unwritable;
    type SerializeSeq = Compound<'a, 'm>;
    type SerializeTuple = Compound<'a, 'm>;
    type SerializeTupleStruct = Compound<'a, 'm>;
    type SerializeTupleVariant = Compound<'a, 'm>;
    type SerializeMap = Compound<'a, 'm>;
    type SerializeStruct = Compound<'a, 'm>;
    type SerializeStructVariant = Compound<'a, 'm>;

    fn serialize_bool(self, v: bool) -> Done {
        self.push(Tok::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> Done {
        self.push(Tok::Int(v.into()))
    }
    fn serialize_i16(self, v: i16) -> Done {
        self.push(Tok::Int(v.into()))
    }
    fn serialize_i32(self, v: i32) -> Done {
        self.push(Tok::Int(v.into()))
    }
    fn serialize_i64(self, v: i64) -> Done {
        self.push(Tok::Int(v.into()))
    }
    fn serialize_i128(self, v: i128) -> Done {
        self.push(Tok::Int(v))
    }
    fn serialize_u8(self, v: u8) -> Done {
        self.push(Tok::Int(v.into()))
    }
    fn serialize_u16(self, v: u16) -> Done {
        self.push(Tok::Int(v.into()))
    }
    fn serialize_u32(self, v: u32) -> Done {
        self.push(Tok::Int(v.into()))
    }
    fn serialize_u64(self, v: u64) -> Done {
        self.push(Tok::Int(v.into()))
    }
    fn serialize_f32(self, v: f32) -> Done {
        self.push(Tok::Float(f64::from(v).to_bits()))
    }
    fn serialize_f64(self, v: f64) -> Done {
        self.push(Tok::Float(v.to_bits()))
    }
    fn serialize_char(self, v: char) -> Done {
        self.push(Tok::Char(v))
    }
    fn serialize_str(self, v: &str) -> Done {
        self.push(Tok::Str(v.into()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Done {
        self.push(Tok::Bytes(v.into()))
    }
    fn serialize_none(self) -> Done {
        self.push(Tok::None)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Done {
        self.out.push(Tok::Some);
        value.serialize(self)
    }
    fn serialize_unit(self) -> Done {
        self.push(Tok::Unit)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Done {
        self.push(Tok::Unit)
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Done {
        self.push(Tok::Variant(variant))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, value: &T) -> Done {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &T,
    ) -> Done {
        self.out.push(Tok::Variant(variant));
        value.serialize(self)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Compound<'a, 'm>, Unwritable> {
        Ok(Compound::open(self, "seq"))
    }
    fn serialize_tuple(self, _: usize) -> Result<Compound<'a, 'm>, Unwritable> {
        Ok(Compound::open(self, "tuple"))
    }
    fn serialize_tuple_struct(
        self,
        name: &'static str,
        _: usize,
    ) -> Result<Compound<'a, 'm>, Unwritable> {
        Ok(Compound::open(self, name))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Compound<'a, 'm>, Unwritable> {
        self.out.push(Tok::Variant(variant));
        Ok(Compound::open(self, "tuple"))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Compound<'a, 'm>, Unwritable> {
        Ok(Compound::open(self, "map"))
    }
    fn serialize_struct(
        self,
        name: &'static str,
        _: usize,
    ) -> Result<Compound<'a, 'm>, Unwritable> {
        let at = self.out.len();
        let mut c = Compound::open(self, name);
        if name == "Span" {
            c.span_at = Some(at);
        }
        Ok(c)
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Compound<'a, 'm>, Unwritable> {
        self.out.push(Tok::Variant(variant));
        Ok(Compound::open(self, "struct"))
    }
}

impl SerializeSeq for Compound<'_, '_> {
    type Ok = ();
    type Error = Unwritable;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Done {
        self.element(value)
    }
    fn end(self) -> Done {
        self.close()
    }
}

impl ser::SerializeTuple for Compound<'_, '_> {
    type Ok = ();
    type Error = Unwritable;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Done {
        self.element(value)
    }
    fn end(self) -> Done {
        self.close()
    }
}

impl ser::SerializeTupleStruct for Compound<'_, '_> {
    type Ok = ();
    type Error = Unwritable;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Done {
        self.element(value)
    }
    fn end(self) -> Done {
        self.close()
    }
}

impl ser::SerializeTupleVariant for Compound<'_, '_> {
    type Ok = ();
    type Error = Unwritable;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Done {
        self.element(value)
    }
    fn end(self) -> Done {
        self.close()
    }
}

impl SerializeMap for Compound<'_, '_> {
    type Ok = ();
    type Error = Unwritable;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Done {
        self.entries.push(self.canon.out.len());
        self.element(key)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Done {
        self.element(value)
    }
    fn end(self) -> Done {
        // A hash map writes its entries in the order of its buckets: sorted, two equal maps
        // write the same tokens.
        if let Some(&first) = self.entries.first() {
            let tail = self.canon.out.split_off(first);
            let ends = (self.entries.iter().skip(1).map(|e| e - first)).chain([tail.len()]);
            let mut from = 0;
            let mut entries: Vec<&[Tok]> = Vec::with_capacity(self.entries.len());
            for end in ends {
                entries.push(&tail[from..end]);
                from = end;
            }
            entries.sort();
            self.canon.out.extend(entries.concat());
        }
        self.close()
    }
}

impl SerializeStruct for Compound<'_, '_> {
    type Ok = ();
    type Error = Unwritable;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Done {
        self.canon.out.push(Tok::Field(key));
        self.element(value)
    }
    fn end(self) -> Done {
        if let (Some(at), Some(map)) = (self.span_at, self.canon.map) {
            // `Open`, `start`, its offset, `end`, its offset.
            for i in [at + 2, at + 4] {
                if let Some(Tok::Int(v)) = self.canon.out.get_mut(i)
                    && let Ok(o) = u32::try_from(*v)
                {
                    *v = map.original(o).into();
                }
            }
        }
        self.close()
    }
}

impl ser::SerializeStructVariant for Compound<'_, '_> {
    type Ok = ();
    type Error = Unwritable;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Done {
        self.canon.out.push(Tok::Field(key));
        self.element(value)
    }
    fn end(self) -> Done {
        self.close()
    }
}

impl ModuleImports {
    /// The import facts without the sites `dropped` picks (an edit added them) and the names
    /// `bound` (their imports bind them): the indices of the sites after them as before.
    pub(crate) fn without_sites(
        &self,
        dropped: impl Fn(&crate::imports::ImportSite) -> bool,
        bound: &BTreeSet<String>,
    ) -> Self {
        let gone: Vec<bool> = self.sites.iter().map(&dropped).collect();
        // Site `k` of the edited file is site `k - (sites dropped before it)` of the original.
        let before = |k: u32| {
            let k = k as usize;
            let dropped = gone.iter().take(k).filter(|g| **g).count();
            u32::try_from(k - dropped).unwrap_or(u32::MAX)
        };
        let sites: Vec<_> = (self.sites.iter().zip(&gone))
            .filter(|(_, g)| !**g)
            .map(|(s, _)| s.clone())
            .collect();
        let len = u32::try_from(sites.len()).unwrap_or(u32::MAX);
        Self {
            // A name bound only after every site is not listed.
            bound_before: (self.bound_before.iter())
                .filter(|(name, _)| !bound.contains(*name))
                .map(|(name, k)| (name.clone(), before(*k)))
                .filter(|(_, k)| *k < len)
                .collect(),
            sites,
            any_bound_before: self.any_bound_before.map(before),
            code_before: self.code_before.map(before),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::ser::Serializer;

    #[derive(Serialize)]
    enum Shape {
        Unit,
        Newtype(u8),
        Tuple(i8, i16),
        Struct { x: i32 },
    }

    #[derive(Serialize)]
    struct Marker;

    #[derive(Serialize)]
    struct Wrapped(i64);

    #[derive(Serialize)]
    struct Pair(u16, u32);

    struct Bytes;

    impl Serialize for Bytes {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(b"ab")
        }
    }

    struct Refused;

    impl Serialize for Refused {
        fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(ser::Error::custom("refused"))
        }
    }

    #[derive(Serialize)]
    struct Every {
        flag: bool,
        small: (i8, i16, i32, i64, i128),
        unsigned: (u8, u16, u32, u64),
        float: (f32, f64),
        text: (char, &'static str, Bytes),
        options: (Option<u8>, Option<u8>),
        units: ((), Marker, Wrapped, Pair),
        shapes: Vec<Shape>,
        table: FxHashMap<u8, u8>,
        span: Span,
    }

    fn every(table: &[(u8, u8)], span: u32) -> Every {
        Every {
            flag: true,
            small: (-1, -2, -3, -4, -5),
            unsigned: (1, 2, 3, 4),
            float: (0.5, 0.25),
            text: ('c', "s", Bytes),
            options: (Some(1), None),
            units: ((), Marker, Wrapped(7), Pair(8, 9)),
            shapes: vec![
                Shape::Unit,
                Shape::Newtype(1),
                Shape::Tuple(2, 3),
                Shape::Struct { x: 4 },
            ],
            table: table.iter().copied().collect(),
            span: Span {
                start: span,
                end: span + 4,
            },
        }
    }

    #[test]
    fn edited_values_compare_at_their_original_places_in_any_map_order() {
        // `list` at 22 replaced with `Sequence`, four bytes longer, and an import inserted at 0.
        let source = "import x\n\ndef f(rows: list): ...\n";
        let edits = [
            (Span { start: 22, end: 26 }, "Sequence".to_owned()),
            (Span { start: 0, end: 0 }, "import y\n".to_owned()),
            (Span { start: 0, end: 0 }, "import y\n".to_owned()),
        ];
        let (text, map) = OffsetMap::apply(source, &edits);
        assert_eq!(text, "import y\nimport x\n\ndef f(rows: Sequence): ...\n");
        assert_eq!(
            [0, 4, 9, 31, 35, 39].map(|o| map.original(o)),
            [0, 0, 0, 22, 22, 26]
        );
        assert!(map.in_edit(4) && !map.in_edit(9) && !map.in_edit(39));
        let now = every(&[(1, 2), (3, 4)], 30);
        let then = every(&[(3, 4), (1, 2)], 43);
        assert!(same(&now, &then, &map));
        let moved = every(&[(1, 2), (3, 4)], 44);
        assert_eq!(differing_fields(&now, &moved, &map), ["span"]);
    }

    #[test]
    fn a_value_that_cannot_be_written_is_never_the_same() {
        let map = OffsetMap::default();
        assert!(!same(&Refused, &Refused, &map));
        assert_eq!(differing_fields(&Refused, &Refused, &map), ["fields"]);
        assert_eq!(Unwritable.to_string(), "the value cannot be written");
    }
}
