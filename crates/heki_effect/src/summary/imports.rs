//! The module a call of `importlib` imports, as far as the code fixes it: `reads-environment`
//! effect boundaries forbid no import of the project's own modules.

use super::statements::{Text, Walk};
use super::{Extractor, Passed};
use crate::environment::absolute_module_name;
use crate::resolver;
use heki_ir::{InstKind, VarId};
use std::sync::Arc;

impl Extractor<'_> {
    /// Whether a call of a function the catalog tags `environment = "import"`, passed `passed`,
    /// imports or finds a module of the project: every text its `name` may hold names one
    /// (`"pkg.config"`, a module constant), or starts with a package holding only the project's
    /// modules and a dot (`f"pkg.parser.{name}"`, `"pkg.parser." + name`). A relative name is
    /// anchored at every text `package` may hold.
    pub(super) fn imports_project_module(&self, passed: &Passed<'_>) -> bool {
        let slot = |at| resolver::passed_var(passed.args, passed.kwargs, passed.star_from, at);
        let Some(names) =
            slot((Some(0), Some("name"))).and_then(|v| self.statement_texts(v, &mut Walk::exact()))
        else {
            return false;
        };
        let relative = names.iter().any(|t| t.text().starts_with('.'));
        let packages: Vec<Option<Arc<str>>> = if relative {
            let Some(texts) = slot((Some(1), Some("package"))).and_then(|v| self.package_texts(v))
            else {
                return false;
            };
            texts.into_iter().map(Text::whole).collect()
        } else {
            vec![None]
        };
        let project = self.cx.project;
        names.iter().all(|name| {
            let head = matches!(name, Text::Head(_));
            packages.iter().all(|package| {
                match absolute_module_name(name.text(), package.as_deref(), head) {
                    Some(module) if head => project.holds_modules_starting_with(&module),
                    Some(module) => project.module(&module).is_some(),
                    None => false,
                }
            })
        })
    }

    /// Every text the `package` argument of an import may hold: the texts the code fixes, or the
    /// module's own `__name__` or `__package__` where the module binds no such global. Run as the
    /// main module, its `__name__` is `"__main__"`, and run from a path, its `__package__` is
    /// empty or `None`: a relative name raises against either, so the import goes on only with
    /// the module's own names.
    fn package_texts(&self, package: VarId) -> Option<Vec<Text>> {
        let module = self.cx.module;
        let global = match self
            .defs
            .get(&self.shape.copy_root(package))
            .map(|i| &i.kind)
        {
            Some(InstKind::LoadGlobal { name, .. }) if !module.unknown_globals.contains(name) => {
                name.as_str()
            }
            _ => "",
        };
        let own = self.cx.module_name();
        let named = match global {
            "__name__" => own,
            "__package__" if module.is_package => own,
            "__package__" => own.rsplit_once('.').map_or("", |(parent, _)| parent),
            _ => return self.statement_texts(package, &mut Walk::exact()),
        };
        Some(vec![Text::Whole(Arc::from(named))])
    }
}
