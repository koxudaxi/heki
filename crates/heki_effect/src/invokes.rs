//! What a library call runs of the values it is handed (R3; `MUT101`'s "Kept to the return"):
//! whether a call of a library the ecosystem catalog covers may run code of the project, and
//! whether the special methods and the attribute access an instruction runs on a value may. The
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
    "builtins.str.strip",
    "builtins.str.lstrip",
    "builtins.str.rstrip",
    "builtins.str.lower",
    "builtins.str.upper",
    "builtins.str.replace",
    "builtins.str.format",
    "builtins.str.startswith",
    "builtins.str.endswith",
    "builtins.len",
    "builtins.bool",
    "builtins.int",
    "builtins.float",
    "builtins.repr",
    "builtins.ascii",
    "builtins.format",
    "builtins.hash",
    "builtins.id",
    "builtins.isinstance",
    "builtins.issubclass",
    "builtins.hasattr",
    "builtins.callable",
];

/// The builtin calls of [`BUILT_BY`] whose result holds no value of another class: a string, a
/// list of strings, a number, a boolean.
pub(crate) const HOLDS_BUILTINS: &[&str] = &[
    "builtins.str",
    "builtins.str.split",
    "builtins.str.rsplit",
    "builtins.str.splitlines",
    "builtins.str.join",
    "builtins.str.strip",
    "builtins.str.lstrip",
    "builtins.str.rstrip",
    "builtins.str.lower",
    "builtins.str.upper",
    "builtins.str.replace",
    "builtins.str.format",
    "builtins.str.startswith",
    "builtins.str.endswith",
    "builtins.len",
    "builtins.bool",
    "builtins.int",
    "builtins.float",
    "builtins.repr",
    "builtins.ascii",
    "builtins.format",
    "builtins.hash",
    "builtins.id",
    "builtins.isinstance",
    "builtins.issubclass",
    "builtins.hasattr",
    "builtins.callable",
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
    /// The items of a value built in the calling function, when it knows them all: the
    /// elements of a display nothing else changes, none of a constant or a string (whose items
    /// are builtin values). `None`: items heki does not know (what a builtin call or a generator
    /// expression returns, a display used elsewhere).
    pub elems: Option<Vec<Handed<'v>>>,
    /// A callable the call runs that heki follows through a call site of its own at the call.
    pub followed: bool,
}

/// Whether the call of the library function `symbol`, handed the receiver `recv`, the
/// positional arguments `positional`, and the keyword arguments `keywords` (`star`: some are
/// unpacked), may run code of the project. A function outside the catalog may; so may one the
/// catalog does not describe `[[invokes]]` for, when it is handed a value that may run code of
/// the project ([`inert`]); one it describes runs only the code its entry names. `iterated`:
/// the iterator the call of a `lazy` entry returned is iterated, and every callable it calls
/// runs there, not at its own site.
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

/// Whether one of the special methods Python runs on the class of `h` (`methods`) or on the
/// classes of its items (`item_methods`) may be code of the project.
pub(crate) fn special_methods_run_project_code(
    project: &ProjectDecls,
    h: &Handed<'_>,
    methods: &[&str],
    item_methods: &[&str],
) -> bool {
    methods.iter().any(|m| may_define(project, h, m, false))
        || item_methods.iter().any(|m| items_may_define(project, h, m))
}

/// An access of an attribute ([`attribute_access_runs_project_code`]).
#[derive(Debug, Clone, Copy)]
pub(crate) enum AttrAccess {
    Read,
    /// A store or a delete.
    Write,
}

/// Attributes every object has, which reading runs no code of the class but its
/// `__getattribute__`.
const OBJECT_ATTRS: &[&str] = &["__class__", "__dict__"];

/// Whether accessing `attr` on `h` may run code of the project that is no property getter heki
/// resolves: on an instance of a project class, a read that finds no plain attribute and no
/// plain method, or a write that may run a property setter, `__setattr__`, `__delattr__`, a
/// descriptor, or code of a base heki does not know; on an instance of a library class, the
/// access as a call of that class's attribute (`__setattr__` for a write) under R3
/// ([`runs_project_code`]); on a project class object, what its metaclass or a descriptor of
/// the name may run. A module, a function, or a library class object holds its attributes in
/// a dictionary. A value of unknown class may run anything.
pub(crate) fn attribute_access_runs_project_code(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
    attr: &str,
    access: AttrAccess,
) -> bool {
    if h.built {
        return false;
    }
    let reads_plainly = |class: &str| {
        if OBJECT_ATTRS.contains(&attr) {
            !project.attr_read(class, attr).runs_hook(false)
        } else {
            project.plain_attr(class, attr) || project.method_read_runs_no_code(class, attr)
        }
    };
    let instance = |class: &str| {
        if !project.is_project_class(class) {
            let member = match access {
                AttrAccess::Read => attr,
                AttrAccess::Write => "__setattr__",
            };
            let symbol = format!("{class}.{member}");
            return runs_project_code(project, kb, &symbol, Some(h), &[], &[], false, false);
        }
        match access {
            AttrAccess::Read => !reads_plainly(class),
            AttrAccess::Write => {
                !project.attr_read(class, attr).bases_known
                    || project
                        .value_classes(class)
                        .iter()
                        .any(|c| project.attr_write_runs_code(c, attr))
            }
        }
    };
    match h.val {
        Some(Val::Inst(class)) => instance(class),
        Some(Val::Union(classes)) => classes.iter().any(|c| instance(c)),
        Some(Val::Class(class) | Val::Ref(class)) if project.is_project_class(class) => {
            match access {
                AttrAccess::Read => !reads_plainly(class),
                AttrAccess::Write => project.has_custom_metaclass(class),
            }
        }
        Some(Val::Class(_) | Val::Ref(_)) => false,
        Some(Val::Super(_)) | None => true,
    }
}

/// Whether iterating `h` runs only its builtin class's own iteration: a builtin container,
/// string, range, or view whose iteration no project subclass replaces (R2), or a display built
/// in the calling function.
pub(crate) fn iterates_builtin(project: &ProjectDecls, h: &Handed<'_>) -> bool {
    let builtin = |class: &str| BUILTIN_ITERABLES.contains(&class);
    h.built
        || match h.val {
            Some(Val::Inst(class)) => {
                builtin(class) && !special_methods_run_project_code(project, h, ITERATION, &[])
            }
            Some(Val::Union(classes)) => {
                classes.iter().all(|c| builtin(c))
                    && !special_methods_run_project_code(project, h, ITERATION, &[])
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
        return h
            .elems
            .as_ref()
            .is_none_or(|elems| elems.iter().any(|e| may_define(project, e, name, false)));
    }
    let class = match h.val {
        Some(Val::Inst(class)) => class,
        // A class object has items only through its metaclass's iteration (an enum's members).
        Some(Val::Class(class) | Val::Ref(class)) => {
            return project.is_project_class(class) && project.has_custom_metaclass(class);
        }
        _ => return true,
    };
    // A string's items are strings; a number holds none.
    let builtin_items = matches!(
        class.as_str(),
        "builtins.str"
            | "builtins.bytes"
            | "builtins.bytearray"
            | "builtins.int"
            | "builtins.float"
            | "builtins.bool"
            | "builtins.complex"
    ) && ITERATION
        .iter()
        .all(|m| project.method_not_overridden(class, m, false));
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
        return h.elems.as_ref().is_some_and(|elems| {
            elems
                .iter()
                .all(|e| e.built || of_library_class(project, e))
        });
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
