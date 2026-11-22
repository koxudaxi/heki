//! What a library call runs of the values it is handed (R3; `MUT101`'s "Kept to the return"):
//! whether a call of a library the ecosystem catalog covers may run code of the project. The
//! catalog's `[[invokes]]` table says, per call, the special methods it runs on each argument's
//! class, on its items' classes, and the arguments it calls; a value runs code of the project
//! there only when its class may define that method in the project (a project class, a builtin
//! one a project subclass may replace it in under R2, or a class heki does not know). A call the
//! table lists as `lazy` returns an iterator that runs the same code again while it is iterated.

use crate::resolver::{ProjectDecls, Symbol, Val};
use heki_kb::{EffectiveEcosystemKb, InvokedValue};

/// Library classes whose instances are callables (a library call handed one may call project
/// code through it, and calling one may too).
const CALLABLE_CLASSES: &[&str] = &[
    "functools.partial",
    "builtins.function",
    "builtins.method",
    "builtins.builtin_function_or_method",
    "types.FunctionType",
    "types.MethodType",
    "types.LambdaType",
];

/// Library functions that run code of their own choosing whatever they are handed: source text
/// (`eval`, `exec`), a module's body on import, a pickle's reconstructors.
const RUNS_CODE: &[&str] = &[
    "builtins.eval",
    "builtins.exec",
    "builtins.__import__",
    "importlib.import_module",
    "importlib.reload",
    "pickle.load",
    "pickle.loads",
    "runpy.run_module",
    "runpy.run_path",
];

/// The builtin classes whose instances iterate their own items (`for x in v` runs no other
/// code when no project subclass replaces their iteration).
const BUILTIN_ITERABLES: &[&str] = &[
    "builtins.list",
    "builtins.tuple",
    "builtins.dict",
    "builtins.set",
    "builtins.frozenset",
    "builtins.str",
    "builtins.bytes",
    "builtins.bytearray",
    "builtins.range",
    "builtins.dict_keys",
    "builtins.dict_values",
    "builtins.dict_items",
];

/// The builtin calls that return an instance of a builtin class itself, never of a subclass.
pub(crate) const BUILT_BY: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "builtins.frozenset",
    "builtins.tuple",
    "builtins.sorted",
    "builtins.str",
    "builtins.list.copy",
    "builtins.dict.copy",
    "builtins.set.copy",
    "builtins.dict.fromkeys",
    "builtins.str.split",
    "builtins.str.rsplit",
    "builtins.str.splitlines",
    "builtins.str.join",
];

/// The special methods iterating a value runs on its class: `__iter__`, `__next__` of an
/// iterator that is its own, and `__getitem__` of a sequence without `__iter__`.
pub(crate) const ITERATION: &[&str] = &["__iter__", "__next__", "__getitem__"];

/// A value a library call is handed, as the calling function knows it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Handed<'v> {
    /// Its inferred value.
    pub val: Option<&'v Val>,
    /// Built in the calling function as a builtin value of its exact class: a constant, a
    /// display, a string, a comparison's result, a generator expression (its body is the
    /// function's own scope, followed through its own call site).
    pub built: bool,
    /// The elements of a display built in the calling function.
    pub elems: Vec<Handed<'v>>,
    /// A callable the call runs that heki follows through a call site of its own at the call.
    pub followed: bool,
}

/// Whether the call of the library function `symbol`, handed the receiver `recv`, the
/// positional arguments `positional`, and the keyword arguments `keywords` (`star`: some are
/// unpacked), may run code of the project. A function outside the catalog may; so may one the
/// catalog does not describe `[[invokes]]` for, when it is handed a value that may run code of
/// the project ([`inert`]); one it describes runs only the code its entry names. `iterated`:
/// the iterator a `lazy` call returned is iterated, and every callable it calls runs there, not
/// at its own site.
#[allow(clippy::too_many_arguments)]
pub(crate) fn runs_project_code(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    symbol: &str,
    recv: Option<&Handed<'_>>,
    positional: &[Handed<'_>],
    keywords: &[(Option<&str>, Handed<'_>)],
    star: bool,
    iterated: bool,
) -> bool {
    if RUNS_CODE.contains(&symbol) || !catalogued(kb, symbol) || star {
        return true;
    }
    let mut handed = recv
        .into_iter()
        .chain(positional)
        .chain(keywords.iter().map(|(_, h)| h));
    let Some(entry) = kb.invoked(symbol) else {
        return handed.any(|h| !inert(project, h));
    };
    if iterated && !entry.lazy {
        return false;
    }
    // A method of a library class runs a project subclass's own replacement under R2; through
    // `super()` the method resolved is the library's own.
    if let (Some(r), Some((_, method))) = (recv, symbol.rsplit_once('.'))
        && !matches!(r.val, Some(Val::Super(_)))
        && may_define(project, r, method, true)
    {
        return true;
    }
    let runs = |h: &Handed<'_>, names: &dyn Fn(&InvokedValue) -> bool| {
        entry
            .values
            .iter()
            .filter(|v| names(v))
            .any(|v| value_runs(project, h, v, iterated))
    };
    recv.is_some_and(|r| runs(r, &|v| v.receiver))
        || positional.iter().enumerate().any(|(i, h)| {
            let i = u8::try_from(i).unwrap_or(u8::MAX);
            runs(h, &|v| v.names(Some(i), None))
        })
        || keywords
            .iter()
            .any(|(k, h)| runs(h, &|v| v.names(None, *k)))
}

/// Whether `symbol` is a function of a library the ecosystem catalog covers: an entry of its
/// own, an `[[invokes]]` entry, or a member of a package the catalog lists as opaque.
fn catalogued(kb: &EffectiveEcosystemKb, symbol: &str) -> bool {
    kb.lookup(symbol).is_some()
        || kb.invoked(symbol).is_some()
        || kb.is_opaque_to_layer3(symbol.split('.').next().unwrap_or(symbol))
}

/// Whether what the call runs of `h`, as `v` names it, may be code of the project.
fn value_runs(project: &ProjectDecls, h: &Handed<'_>, v: &InvokedValue, iterated: bool) -> bool {
    (v.calls && (iterated || !h.followed) && calls_project_code(project, h))
        || v.methods.iter().any(|m| may_define(project, h, m, false))
        || v.item_methods
            .iter()
            .any(|m| items_may_define(project, h, m))
}

/// Whether one of the special methods `names` Python runs on the class of `h` (a context
/// manager's, an iterable's) may be code of the project.
pub(crate) fn special_methods_run_project_code(
    project: &ProjectDecls,
    h: &Handed<'_>,
    names: &[&str],
) -> bool {
    names.iter().any(|m| may_define(project, h, m, false))
}

/// Whether iterating `h` runs only its builtin class's own iteration: a builtin container,
/// string, range, or view whose iteration no project subclass replaces (R2), or a display built
/// in the calling function.
pub(crate) fn iterates_builtin(project: &ProjectDecls, h: &Handed<'_>) -> bool {
    let builtin = |class: &str| BUILTIN_ITERABLES.contains(&class);
    h.built
        || match h.val {
            Some(Val::Inst(class)) => {
                builtin(class) && !special_methods_run_project_code(project, h, ITERATION)
            }
            Some(Val::Union(classes)) => {
                classes.iter().all(|c| builtin(c))
                    && !special_methods_run_project_code(project, h, ITERATION)
            }
            _ => false,
        }
}

/// Whether the special method `name` (`"*"`: any attribute) that Python looks up on the class
/// of `h` (on its metaclass for a class object) may be code of the project. `by_name`: a
/// method looked up by its name, which `__getattribute__` sees too.
fn may_define(project: &ProjectDecls, h: &Handed<'_>, name: &str, by_name: bool) -> bool {
    if h.built {
        return false;
    }
    let instance = |class: &str| {
        if name == "*" {
            !project.value_classes(class).is_empty()
        } else {
            !project.method_not_overridden(class, name, by_name)
        }
    };
    let class_object = |class: &str| {
        project.is_project_class(class)
            && (name == "*"
                || project.has_custom_metaclass(class)
                || !project.method_not_overridden(class, name, by_name))
    };
    match h.val {
        Some(Val::Inst(class)) => instance(class),
        Some(Val::Union(classes)) => classes.iter().any(|c| instance(c)),
        Some(Val::Class(class)) => class_object(class),
        Some(Val::Ref(symbol)) => {
            class_object(symbol) || (name == "*" && project_named(project, symbol))
        }
        Some(Val::Super(_)) | None => true,
    }
}

/// Whether the special method `name` of the items of `h` may be code of the project: items of a
/// string or bytes are builtin values, those of a display built in the calling function are its
/// elements, and those of any other value are of classes heki does not know.
fn items_may_define(project: &ProjectDecls, h: &Handed<'_>, name: &str) -> bool {
    if h.built {
        return h.elems.iter().any(|e| may_define(project, e, name, false));
    }
    let class = match h.val {
        Some(Val::Inst(class)) => class,
        // A class object has items only through its metaclass's iteration (an enum's members).
        Some(Val::Class(class) | Val::Ref(class)) => {
            return project.is_project_class(class) && project.has_custom_metaclass(class);
        }
        _ => return true,
    };
    let builtin_items = matches!(
        class.as_str(),
        "builtins.str" | "builtins.bytes" | "builtins.bytearray"
    );
    // A builtin dict or set hands its keys on with the hashes it stored for them, and a dict
    // its entries as pairs it does not unpack through their own iteration.
    let stored = match class.as_str() {
        "builtins.dict" => matches!(
            name,
            "__hash__" | "__eq__" | "__iter__" | "__next__" | "__getitem__"
        ),
        "builtins.set" | "builtins.frozenset" | "builtins.dict_keys" => {
            matches!(name, "__hash__" | "__eq__")
        }
        _ => false,
    } && project.method_not_overridden(class, "__iter__", false);
    !(builtin_items || stored)
}

/// Whether `symbol` names something of the project (a module, class, function, or global).
fn project_named(project: &ProjectDecls, symbol: &str) -> bool {
    !matches!(project.symbol(symbol), Symbol::External)
}

/// Whether calling `h` may run code of the project: a project function or class, a callable
/// library object (a `functools.partial` holds what it calls), an instance whose `__call__` may
/// be the project's, or a callable heki does not know.
fn calls_project_code(project: &ProjectDecls, h: &Handed<'_>) -> bool {
    if h.built {
        return false;
    }
    match h.val {
        Some(Val::Ref(symbol)) => project_named(project, symbol),
        Some(Val::Class(class)) => project.is_project_class(class),
        Some(Val::Inst(class)) if CALLABLE_CLASSES.contains(&class.as_str()) => true,
        Some(Val::Inst(_) | Val::Union(_)) => may_define(project, h, "__call__", false),
        Some(Val::Super(_)) | None => true,
    }
}

/// Whether handing `h` to a library call runs no project code because of it, whatever the call
/// does with it: it is built in the calling function from such values, or its class is
/// established as a library class that is not callable and that no project class derives from
/// (R2).
fn inert(project: &ProjectDecls, h: &Handed<'_>) -> bool {
    if h.built {
        return h
            .elems
            .iter()
            .all(|e| e.built || of_library_class(project, e));
    }
    of_library_class(project, h)
}

/// Whether the class of `h` is established as a library class that is not callable and that no
/// project class derives from (R2).
fn of_library_class(project: &ProjectDecls, h: &Handed<'_>) -> bool {
    let library = |class: &str| {
        !project.is_project_class(class)
            && !CALLABLE_CLASSES.contains(&class)
            && project.value_classes(class).is_empty()
    };
    match h.val {
        Some(Val::Inst(class)) => library(class),
        Some(Val::Union(classes)) => classes.iter().all(|c| library(c)),
        _ => false,
    }
}
