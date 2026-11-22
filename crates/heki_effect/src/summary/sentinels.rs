//! The local facts `MUT118` joins across the project: comparisons by identity of an attribute
//! with an `object()` sentinel, and `copy.deepcopy` calls of an object of a project class.

use super::Extractor;
use crate::resolver::{Callee, Val};
use crate::{DeepCopySite, SentinelTest};
use heki_ast::Span;
use heki_ir::{InstKind, VarId};

impl<'a> Extractor<'a> {
    /// Record `obj.attr is S` (or `is not`), where `attr_value` holds `obj.attr` and `other`
    /// the `object()` sentinel `S` ([`crate::EffectSummary::sentinel_tests`]): one test per
    /// project class `obj` may be an instance of.
    pub(super) fn sentinel_test(&mut self, attr_value: VarId, other: VarId, span: Span) {
        let Some(InstKind::AttrLoad { obj, attr, .. }) = self.root_def(attr_value) else {
            return;
        };
        let Some(sentinel) = self
            .global_name(other)
            .and_then(|fqn| self.cx.project.object_sentinel(&fqn))
        else {
            return;
        };
        let classes = self.project_classes(*obj);
        if classes.is_empty() {
            return;
        }
        let only = self.one_class(*obj);
        let tests = self.summary.sentinel_tests.get_or_insert_with(Box::default);
        for class_qualname in classes {
            tests.insert(SentinelTest {
                class_qualname,
                attr: attr.clone(),
                sentinel: sentinel.clone(),
                only,
                span,
            });
        }
    }

    /// Record a call of `callee` that is `copy.deepcopy` with one positional argument and no
    /// `memo`, of an object of a project class ([`crate::EffectSummary::deep_copies`]).
    pub(super) fn deep_copy(
        &mut self,
        callee: &Callee,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        star_from: Option<u8>,
        span: Span,
    ) {
        if !matches!(callee, Callee::External(fqn) if fqn == "copy.deepcopy") {
            return;
        }
        let ([arg], [], None) = (args, kwargs, star_from) else {
            return;
        };
        let classes = self.project_classes(*arg);
        if classes.is_empty() {
            return;
        }
        let only = self.one_class(*arg);
        self.summary
            .deep_copies
            .get_or_insert_with(Box::default)
            .insert(DeepCopySite {
                classes,
                only,
                span,
            });
    }

    /// Whether `var`'s object is an instance of one project class or, under R2, of a project
    /// subclass (by construction or annotation): no union, and no protocol, whose
    /// implementations are listed structurally.
    fn one_class(&self, var: VarId) -> bool {
        let project = self.cx.project;
        matches!(self.val(var), Some(Val::Inst(c))
            if project.is_project_class(c) && project.protocol_implementers(c).is_none())
    }

    /// The instruction defining the value `var` copies, through copies.
    fn root_def(&self, var: VarId) -> Option<&'a InstKind> {
        let mut cur = var;
        loop {
            let inst: &'a heki_ir::Inst = self.defs.get(&cur)?;
            match &inst.kind {
                InstKind::Copy { src, .. } => cur = *src,
                kind => return Some(kind),
            }
        }
    }

    /// The fqn of the module-level name `var` holds: a global of the module, a name one of its
    /// imports binds (and nothing in the module binds again), or a member of a module
    /// (`sentinels.MISSING`).
    fn global_name(&self, var: VarId) -> Option<String> {
        match self.root_def(var)? {
            InstKind::LoadGlobal { name, .. } => match self.cx.module.imports.get(name) {
                Some(_) => self.cx.module.fixed_import(name).cloned(),
                None => Some(format!("{}.{name}", self.cx.module.module_name)),
            },
            InstKind::AttrLoad { obj, attr, .. } => match self.val(*obj)? {
                Val::Ref(module) => Some(format!("{module}.{attr}")),
                _ => None,
            },
            _ => None,
        }
    }
}
