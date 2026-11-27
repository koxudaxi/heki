//! The modules a call importing or finding a module by name names, as far as the code fixes
//! them: the import sites of `importlib.import_module` and `__import__`
//! ([`crate::imports::ImportSite`]), and the project's own modules, which `reads-environment`
//! effect boundaries forbid no import of.

use super::statements::{Text, Walk};
use super::{Call, Extractor, Passed, Val, is_dotted_name};
use crate::environment::absolute_module_name;
use crate::imports::{ImportForm, ImportHandlers, ImportSite, Named, Spelling, Timing};
use crate::resolver;
use heki_ir::walk::Answer;
use heki_ir::{BlockId, InstKind, LastBindings, VarId};
use heki_kb::ArgSlot;
use std::sync::Arc;

/// An argument by position and keyword ([`resolver::passed_var`]).
type Slot<'s> = (Option<u8>, Option<&'s str>);

/// `importlib`'s arguments: the name, and the package a relative one is anchored at.
const IMPORTLIB_NAME: Slot<'static> = (Some(0), Some("name"));
const IMPORTLIB_PACKAGE: Slot<'static> = (Some(1), Some("package"));

fn slot(arg: &ArgSlot) -> Slot<'_> {
    (arg.arg, arg.param.as_deref())
}

impl Extractor<'_> {
    /// Whether a call of a function the catalog tags `environment = "import"`, passed `passed`,
    /// imports or finds a module of the project: every module it may name
    /// ([`Self::imported_texts`]) is one that the import finds in the project (`"pkg.config"`, a
    /// module constant; `ProjectDecls::holds_module`), or starts with a package holding only
    /// the project's modules and a dot (`f"pkg.parser.{name}"`, `"pkg.parser." + name`;
    /// `ProjectDecls::holds_modules_starting_with`).
    pub(super) fn imports_project_module(&self, passed: &Passed<'_>) -> bool {
        let project = self.cx.project;
        self.imported_texts(passed, IMPORTLIB_NAME, Some(IMPORTLIB_PACKAGE), None)
            .is_some_and(|(names, _)| {
                names.iter().all(|name| match name {
                    Named::Under(head) => project.holds_modules_starting_with(head),
                    Named::Module(module) => project.holds_module(module),
                })
            })
    }

    /// Record the import site of a call of `symbol` in block `bb` when the catalog says it
    /// loads a module by name (`heki_kb::EcosystemEntry::loads_module`).
    pub(super) fn import_by_name(&mut self, bb: BlockId, symbol: &str, call: &Call<'_>) {
        let Some(loads) = self
            .cx
            .catalog(symbol)
            .and_then(|e| e.loads_module.as_deref())
        else {
            return;
        };
        let (modules, spelling) = match self.imported_texts(
            &call.passed(),
            slot(&loads.name),
            loads.package.as_ref().map(slot),
            loads.level.as_ref().map(slot),
        ) {
            Some((names, spelling)) => (Answer::Known(names), spelling),
            None => (Answer::Open, Spelling::Absolute),
        };
        self.push_import_site(bb, call, ImportForm::ByName, (modules, spelling));
    }

    /// Record the import site of an unresolved call `name` in block `bb` that may be one
    /// importing a module by name: a function or method named like a catalog entry with
    /// `loads_module` (`loader.import_module(...)` on an object of unknown class), or a value
    /// that is one of several named objects, some of them such an entry
    /// (`load = importlib.import_module if flag else print`). Its modules are open: the call may
    /// be another function.
    pub(super) fn maybe_import_by_name(&mut self, bb: BlockId, name: &str, call: &Call<'_>) {
        let named = |fqn: &String| {
            self.cx
                .catalog(fqn)
                .is_some_and(|e| e.loads_module.is_some())
        };
        let held = match call.value.and_then(|v| self.val(v)) {
            Some(Val::OneOf(fqns)) => fqns.iter().any(named),
            _ => false,
        };
        let last = name.rsplit('.').next().unwrap_or(name);
        if held || (is_dotted_name(name) && self.cx.kb.may_load_module(last)) {
            self.push_import_site(
                bb,
                call,
                ImportForm::MaybeByName,
                (Answer::Open, Spelling::Absolute),
            );
        }
    }

    /// Record a call in block `bb` as an import site of `form`.
    fn push_import_site(
        &mut self,
        bb: BlockId,
        call: &Call<'_>,
        form: ImportForm,
        (modules, spelling): (Answer<Vec<Named>>, Spelling),
    ) {
        let in_module_body = self.key.qualname == "<module>";
        // A class body's own paths say nothing of whether its class statement runs.
        let unconditional =
            !self.f.is_class_body && LastBindings::of(self.shape).always_runs(self.shape, bb);
        self.imports.push(ImportSite {
            span: call.span,
            qualname: self.key.qualname.clone(),
            form,
            modules,
            spelling,
            timing: Answer::Known(if in_module_body {
                Timing::Body
            } else {
                Timing::Function
            }),
            handled: (self.import_handlers)
                .get_or_init(|| ImportHandlers::new(self.shape, &self.cx.ir().constants.shadowed))
                .taken_at(bb),
            unconditional,
            lazy: false,
            lazy_allowed: false,
        });
    }

    /// Every module a call importing or finding a module by name may name, made absolute: each
    /// text the code fixes for its argument `name` (a module constant included), a relative
    /// one anchored at each text the argument `package` may hold
    /// ([`absolute_module_name`]); a text known only by how it starts names the modules under
    /// it. With them, how the call spells them: a module is named relatively when only relative
    /// texts anchored at the module's own name give it. `None` when a text is unknown, a
    /// relative name has no package to be anchored at or climbs past its top-level package, or
    /// the call passes a `level` other than `0`.
    fn imported_texts(
        &self,
        passed: &Passed<'_>,
        name: Slot<'_>,
        package: Option<Slot<'_>>,
        level: Option<Slot<'_>>,
    ) -> Option<(Vec<Named>, Spelling)> {
        let at = |slot| resolver::passed_var(passed.args, passed.kwargs, passed.star_from, slot);
        if let Some(level) = level.and_then(at)
            && self.shape.int_const(level) != Some(0)
        {
            return None;
        }
        let names = at(name).and_then(|v| self.statement_texts(v, &mut Walk::exact()))?;
        let relative = names.iter().any(|t| t.text().starts_with('.'));
        let (packages, anchored_here): (Vec<Option<Arc<str>>>, bool) = if relative {
            let package = package.and_then(at)?;
            let texts = self.package_texts(package)?;
            let anchored_here = self.own_name(package).is_some();
            (texts.into_iter().map(Text::whole).collect(), anchored_here)
        } else {
            (vec![None], false)
        };
        let mut out = Vec::new();
        for name in &names {
            let head = matches!(name, Text::Head(_));
            let spelled_relative = anchored_here && name.text().starts_with('.');
            for package in &packages {
                let module = absolute_module_name(name.text(), package.as_deref(), head)?;
                let named = if head {
                    Named::Under(module)
                } else {
                    Named::Module(module)
                };
                out.push((named, spelled_relative));
            }
        }
        // An absolute text sorts first among those naming one module, and is the one kept: the
        // module is named relatively only when no absolute text names it.
        out.sort();
        out.dedup_by(|later, kept| later.0 == kept.0);
        let (named, relative): (Vec<Named>, Vec<bool>) = out.into_iter().unzip();
        Some((named, Spelling::of_each(relative)))
    }

    /// Every text the `package` argument of an import may hold: the texts the code fixes, or the
    /// module's own name ([`Self::own_name`]).
    fn package_texts(&self, package: VarId) -> Option<Vec<Text>> {
        match self.own_name(package) {
            Some(named) => Some(vec![Text::Whole(Arc::from(named))]),
            None => self.statement_texts(package, &mut Walk::exact()),
        }
    }

    /// The package `value` names when it reads the module's own `__name__` or `__package__`,
    /// where the module binds no such global. Run as the main module, its `__name__` is
    /// `"__main__"`, and run from a path, its `__package__` is empty or `None`: a relative name
    /// raises against either, so an import goes on only with the module's own names.
    fn own_name(&self, value: VarId) -> Option<&str> {
        let module = self.cx.module;
        let global = match self.defs.get(&self.shape.copy_root(value)).map(|i| &i.kind) {
            Some(InstKind::LoadGlobal { name, .. }) if !module.unknown_globals.contains(name) => {
                name.as_str()
            }
            _ => return None,
        };
        let own = self.cx.module_name();
        match global {
            "__name__" => Some(own),
            "__package__" if module.is_package => Some(own),
            "__package__" => Some(own.rsplit_once('.').map_or("", |(parent, _)| parent)),
            _ => None,
        }
    }
}
