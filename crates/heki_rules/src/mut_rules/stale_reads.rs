//! `MUT111` (opt-in): functions that read a `@cached_property` a method of its class family can
//! leave stale (the writes `MUT101` finds, whatever is selected).
//!
//! A read is a typed read site in the function's own body (`self.prop`, `obj.prop`,
//! `getattr(obj, "prop")`, `obj.__dict__["prop"]`). The object may be an instance of the read's
//! class or of any project subclass of it. A stale write reaches the read when an instance of one
//! such class runs both the cached property and the writing method: the first definition of each
//! along its MRO, or one that the definitions before it reach through `super()` without
//! invalidating the property themselves. A property getter or a helper that reads the property is
//! the function reported; the functions calling it are not.

use super::{StaleWrite, invalidates};
use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::{FuncKey, ProjectDecls, ProjectIndex, TypedFieldReadSite};
use heki_semantic::mangling;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A read of an attribute that is a `@cached_property` of a class the read object may be an
/// instance of.
struct CachedRead<'a> {
    unit: &'a ModuleUnit<'a>,
    reader: &'a FuncKey,
    site: &'a TypedFieldReadSite,
    /// `(class, getter)`: the read's class and its project subclasses whose instances run a
    /// `@cached_property` getter for the attribute.
    props: Vec<(String, FuncKey)>,
}

/// What an instance of a project class runs.
struct Lineage {
    /// By the name the class holds each under (`_C__cache` of `def __cache`), the definitions
    /// along the class's MRO (its own first, then its project ancestors').
    defs: FxHashMap<String, Vec<FuncKey>>,
}

impl Lineage {
    fn of(decls: &ProjectDecls, class: &str) -> Self {
        let mut defs: FxHashMap<String, Vec<FuncKey>> = FxHashMap::default();
        let mro = std::iter::once(class.to_owned()).chain(decls.ancestors_of(class));
        for key in mro.flat_map(|c| decls.methods_of(&c)) {
            let name = mangling::member_name(&key.qualname).into_owned();
            defs.entry(name).or_default().push(key);
        }
        Self { defs }
    }

    /// The method an instance runs for `name`: the first definition along the MRO.
    fn first(&self, name: &str) -> Option<&FuncKey> {
        self.defs.get(name).and_then(|d| d.first())
    }

    /// Whether `method` runs on an instance and can leave the cached property `prop` stale
    /// there: it is the first definition of its name along the MRO, or each definition before it
    /// calls the next one on `self` (`super().name(...)`) and does not invalidate `prop` itself
    /// (as for a private helper whose every caller invalidates it, `MUT101`).
    fn runs_stale(&self, index: &ProjectIndex, method: &FuncKey, prop: &str) -> bool {
        let name = method
            .qualname
            .rsplit('.')
            .next()
            .unwrap_or(&method.qualname);
        let Some(defs) = self.defs.get(name) else {
            return false;
        };
        for pair in defs.windows(2) {
            if pair[0] == *method {
                return true;
            }
            let calls_next = index.file_of(&pair[0]).is_some_and(|path| {
                index.files[path].call_sites.iter().any(|site| {
                    site.caller == pair[0]
                        && site.recv_is_self
                        && site.project_callees().iter().any(|bc| bc.callee == pair[1])
                })
            });
            let covers = index
                .summary(&pair[0])
                .is_some_and(|s| invalidates(s, prop));
            if !calls_next || covers {
                return false;
            }
        }
        defs.last() == Some(method)
    }
}

/// The project classes (the writer's own and its project subclasses) an instance of which runs
/// `writer` and can have the cached property `prop` left stale by it (see
/// [`Lineage::runs_stale`]).
pub(super) fn classes_running(
    index: &ProjectIndex,
    decls: &ProjectDecls,
    writer: &FuncKey,
    prop: &str,
) -> BTreeSet<String> {
    let class = writer
        .qualname
        .rsplit_once('.')
        .map(|(owner, _)| format!("{}.{owner}", writer.module));
    class
        .into_iter()
        .flat_map(|class| std::iter::once(class.clone()).chain(decls.subclasses_of(&class)))
        .filter(|c| Lineage::of(decls, c).runs_stale(index, writer, prop))
        .collect()
}

/// The reads of cached properties in the checked functions.
#[derive(Default)]
pub(super) struct CachedReads<'a> {
    reads: Vec<CachedRead<'a>>,
    lineages: FxHashMap<String, Lineage>,
}

impl<'a> CachedReads<'a> {
    pub(super) fn of(cx: &CheckContext<'a>) -> Self {
        let mut out = Self::default();
        let sites = cx
            .units
            .iter()
            .filter(|unit| cx.enabled("MUT111", unit))
            .flat_map(|unit| {
                let path = unit.input.path.as_path();
                cx.index
                    .files
                    .get(path)
                    .into_iter()
                    .flat_map(|file| &file.local_functions)
                    .filter(move |(reader, _)| cx.index.file_of(reader) == Some(path))
                    .flat_map(move |(reader, s)| {
                        s.typed_field_read_sites
                            .iter()
                            .filter(|site| !site.class_object)
                            .map(move |site| (unit, reader, site))
                    })
            });
        for (unit, reader, site) in sites {
            let props = out.cached_properties(cx, &site.class_qualname, &site.attr);
            if !props.is_empty() {
                out.reads.push(CachedRead {
                    unit,
                    reader,
                    site,
                    props,
                });
            }
        }
        out
    }

    /// The `@cached_property` getters an instance of `class` or of a project subclass of it runs
    /// for the attribute `attr`, with that class.
    fn cached_properties(
        &mut self,
        cx: &CheckContext<'_>,
        class: &str,
        attr: &str,
    ) -> Vec<(String, FuncKey)> {
        let mut out = Vec::new();
        for c in std::iter::once(class.to_owned()).chain(cx.decls.subclasses_of(class)) {
            let lineage = self
                .lineages
                .entry(c.clone())
                .or_insert_with(|| Lineage::of(cx.decls, &c));
            if let Some(getter) = lineage.first(attr)
                && cx
                    .index
                    .summary(getter)
                    .is_some_and(|s| s.is_cached_property)
            {
                out.push((c, getter.clone()));
            }
        }
        out
    }

    /// `MUT111`'s exact claim about `read`: every class the read object may be (the read's
    /// class and each project subclass, R2) gets the property from a `@cached_property` getter
    /// and runs a write of it whose `MUT101` report is exact.
    fn exact_read(
        &self,
        cx: &CheckContext<'_>,
        read: &CachedRead<'_>,
        stale: &FxHashMap<&FuncKey, Vec<StaleWrite<'_, '_>>>,
    ) -> bool {
        let class = &read.site.class_qualname;
        std::iter::once(class.clone())
            .chain(cx.decls.subclasses_of(class))
            .all(|c| {
                let Some((_, getter)) = read.props.iter().find(|(pc, _)| *pc == c) else {
                    return false;
                };
                let lineage = &self.lineages[&c];
                stale.get(getter).into_iter().flatten().any(|w| {
                    w.exact && lineage.runs_stale(cx.index, &w.writer.key, &read.site.attr)
                })
            })
    }

    /// The classes defining the cached properties read.
    pub(super) fn property_classes(&self) -> FxHashSet<String> {
        self.reads
            .iter()
            .flat_map(|r| &r.props)
            .filter_map(|(_, getter)| {
                let (owner, _) = getter.qualname.rsplit_once('.')?;
                Some(format!("{}.{owner}", getter.module))
            })
            .collect()
    }

    /// One report per reading function and property name, at its first read a stale write
    /// reaches. The message names the first such write that runs on an instance of the read's
    /// own class when there is one, else the first; every write reaching one of its reads is a
    /// related location.
    pub(super) fn report(
        &self,
        cx: &CheckContext<'_>,
        stale: &FxHashMap<&FuncKey, Vec<StaleWrite<'_, '_>>>,
        out: &mut Vec<Diagnostic>,
    ) {
        /// Per write (by place): whether it runs on no instance of the read's own class.
        type Writes<'w> = BTreeMap<(&'w Path, Span, &'w FuncKey), (bool, &'w StaleWrite<'w, 'w>)>;
        let mut found: BTreeMap<(&FuncKey, &str), (Span, &ModuleUnit<'_>, Writes<'_>)> =
            BTreeMap::new();
        // Per reading function and property name, the reads whose every class runs an exact
        // stale write.
        let mut exact_reads: FxHashSet<(&FuncKey, &str, Span)> = FxHashSet::default();
        for read in &self.reads {
            if self.exact_read(cx, read, stale) {
                exact_reads.insert((read.reader, read.site.attr.as_str(), read.site.span));
            }
            let mut writes = Writes::new();
            for (class, getter) in &read.props {
                let lineage = &self.lineages[class];
                let foreign = *class != read.site.class_qualname;
                let reaching = stale
                    .get(getter)
                    .into_iter()
                    .flatten()
                    .filter(|w| lineage.runs_stale(cx.index, &w.writer.key, &read.site.attr));
                for w in reaching {
                    let slot = writes
                        .entry((w.writer.path, w.span, &w.writer.key))
                        .or_insert((foreign, w));
                    slot.0 &= foreign;
                }
            }
            if writes.is_empty() {
                continue;
            }
            let entry = found
                .entry((read.reader, read.site.attr.as_str()))
                .or_insert_with(|| (read.site.span, read.unit, Writes::new()));
            entry.0 = entry.0.min(read.site.span);
            for (place, (foreign, w)) in writes {
                entry.2.entry(place).or_insert((foreign, w)).0 &= foreign;
            }
        }
        for ((reader, attr), (span, unit, writes)) in found {
            // The write named is one on the read's own class when there is one, an exact one
            // first among those.
            let &(_, named) = writes
                .values()
                .min_by_key(|(foreign, w)| (*foreign, !w.exact))
                .expect("a read is recorded only with a stale write");
            let exact = named.exact && exact_reads.contains(&(reader, attr, span));
            let writer = &named.writer.key.qualname;
            let class_qualname = cx
                .index
                .summary(&named.prop.key)
                .and_then(|ps| ps.owner_class_qualname.as_deref())
                .unwrap_or_default();
            let related = writes
                .values()
                .map(|(_, w)| {
                    cx.related_at(
                        w.writer.path,
                        w.span,
                        format!(
                            "`{}` changes `self.{}` without invalidating `{attr}`",
                            w.writer.key.qualname, w.attr
                        ),
                    )
                })
                .collect();
            if let Some(d) = cx.emit(
                out,
                "MUT111",
                unit,
                span,
                &reader.qualname,
                format!("{class_qualname}.{attr}"),
                format!(
                    "`{}` reads `{attr}`, which `{writer}` can leave stale",
                    reader.qualname
                ),
                Some(format!(
                    "invalidate it in `{writer}` after the change, compute it without caching, or read it only before `{writer}` runs"
                )),
                &[],
            ) {
                d.related = related;
                d.certainty = crate::premises::tier("MUT111", exact).at_most(d.certainty);
            }
        }
    }
}
