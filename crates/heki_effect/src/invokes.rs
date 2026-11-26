//! What a library call runs of the values it is handed (R3; `MUT101`'s "Kept to the return"):
//! whether a call of a library the ecosystem catalog covers may run code of the project, and
//! whether the special methods and the attribute access an instruction runs on a value may. The
//! catalog's `[[invokes]]` table says, per call, the special methods it runs on each argument's
//! class, on its items' classes, and the arguments it calls. A special method runs no code of
//! the project only where heki establishes what implements it on every class the value may be
//! (R2): a builtin type's (whose hashing, comparisons, and conversions to a string run those of
//! the items), or a class's outside the project, called as a library call handed the value and
//! the instruction's other operands (R3). A class of the project that binds it, and a value
//! whose class heki does not establish (an abstract class of the library, a protocol, `object`,
//! an unknown value), may run anything. A value of a nominal container ABC (`Sequence`,
//! `Mapping`, `collections.abc.Set`, their mutable ABCs) is one of its family's builtins or of
//! another class outside the project that implements it, taken to run what the builtin runs, or
//! of a project implementer (`ProjectDecls::implementers`), unless the project opens the family.
//! A call the table lists as `lazy` returns an iterator that runs the same code again while it
//! is iterated.

use crate::readonly::{ContainerFamily, ContainerKind};
use crate::resolver::{MethodOwner, ProjectDecls, Symbol, Val};
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

/// The builtin calls that return a new container of the builtin class itself, never of a
/// subclass, whose items heki does not establish: what they are handed, or what is added later.
pub(crate) const BUILT_CONTAINERS: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "builtins.frozenset",
    "builtins.tuple",
    "builtins.sorted",
    "builtins.list.copy",
    "builtins.dict.copy",
    "builtins.set.copy",
    "builtins.str.split",
    "builtins.str.rsplit",
    "builtins.str.splitlines",
];

/// The builtin calls that return a string, a number, or a boolean of the builtin class itself,
/// whatever they are handed: a builtin string method builds a new string, and `int()` and
/// `float()` copy a subclass's instance that a conversion method returns.
pub(crate) const BUILT_SCALARS: &[&str] = &[
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
    "builtins.hash",
    "builtins.id",
    "builtins.isinstance",
    "builtins.issubclass",
    "builtins.hasattr",
    "builtins.callable",
];

/// The builtin conversions to a string: they return what the argument's class's `__str__`,
/// `__repr__`, or `__format__` returns, which may be an instance of a subclass of `str`, so the
/// result is a string of the builtin class itself only when the argument is a builtin value
/// of its exact class (or there is none).
pub(crate) const CONVERSIONS: &[&str] = &[
    "builtins.str",
    "builtins.repr",
    "builtins.ascii",
    "builtins.format",
];

/// The special methods iterating a value runs on its class: `__iter__`, `__next__` of an
/// iterator that is its own, and `__getitem__` of a sequence without `__iter__`.
pub(crate) const ITERATION: &[&str] = &["__iter__", "__next__", "__getitem__"];

/// The modules of the library whose classes are abstract: a value of one may be of any class
/// that implements it (a generator for `typing.Iterator`, a class of the project registered or
/// recognized by its methods), so neither its class nor what implements its methods is
/// established; but see [`abstract_value`].
const ABSTRACT_MODULES: &[&str] = &[
    "typing",
    "typing_extensions",
    "collections.abc",
    "abc",
    "numbers",
    "importlib.abc",
];

/// Abstract classes of the library outside [`ABSTRACT_MODULES`], and `object`, which every
/// value is.
const ABSTRACT_CLASSES: &[&str] = &[
    "builtins.object",
    "contextlib.AbstractContextManager",
    "contextlib.AbstractAsyncContextManager",
    "os.PathLike",
    "io.IOBase",
    "io.RawIOBase",
    "io.BufferedIOBase",
    "io.TextIOBase",
];

/// Whether `class` is an abstract class of the library ([`ABSTRACT_MODULES`],
/// [`ABSTRACT_CLASSES`]) whose instances' classes heki does not establish: not a nominal
/// container ABC of a family the project leaves closed ([`ProjectDecls::established_abc`]).
fn abstract_value(project: &ProjectDecls, class: &str) -> bool {
    let abstract_class = ABSTRACT_CLASSES.contains(&class)
        || ABSTRACT_MODULES.iter().any(|m| {
            class
                .strip_prefix(m)
                .is_some_and(|rest| rest.starts_with('.'))
        });
    abstract_class && !project.established_abc(class)
}

/// The builtin types that find a key by its hash and then compare it with each key they hold
/// under an equal hash.
const HASHED_CONTAINERS: &[&str] = &[
    "builtins.dict",
    "builtins.set",
    "builtins.frozenset",
    "builtins.dict_keys",
    "builtins.dict_items",
];

/// Whether `class` is a builtin dict or set, derives from one, or is a nominal ABC of their
/// families (whose builtins a value of it is taken to be): it finds a key by its hash, then
/// compares it with each key it holds under an equal hash.
fn hashed_container(project: &ProjectDecls, class: &str) -> bool {
    HASHED_CONTAINERS
        .iter()
        .any(|h| class == *h || project.is_subclass(class, h))
        || ContainerFamily::nominal_abc(class).is_some_and(|(kind, _)| kind != ContainerKind::List)
}

/// What a builtin type's own special method `name` runs on the items a value of `class` holds:
/// a hash the items' hashes; a comparison the items' equality, to find the first pair that
/// differs, then that comparison either way round; a conversion to a string their `repr()`; and
/// a membership test or a set operation of a dict or a set the items' `__eq__`
/// ([`hashed_container`]).
fn on_items(project: &ProjectDecls, class: Option<&str>, name: &str) -> &'static [&'static str] {
    let hashed = || class.is_some_and(|c| hashed_container(project, c));
    match name {
        "__hash__" => &["__hash__"],
        "__eq__" | "__ne__" => &["__eq__"],
        "__lt__" | "__gt__" => &["__eq__", "__lt__", "__gt__"],
        "__le__" | "__ge__" => &["__eq__", "__le__", "__ge__"],
        "__repr__" | "__str__" | "__format__" => &["__repr__"],
        "__contains__" | "__or__" | "__ror__" | "__ior__" | "__and__" | "__rand__" | "__iand__"
        | "__sub__" | "__rsub__" | "__isub__" | "__xor__" | "__rxor__" | "__ixor__" => {
            if hashed() {
                &["__eq__"]
            } else {
                &[]
            }
        }
        _ => &[],
    }
}

/// Whether looking a key up in `h` (a subscript) may run code of the project besides the
/// methods its class runs: a dict or a set compares the key with the keys it holds
/// ([`hashed_container`]), whose classes may be the project's.
fn lookup_compares_items(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
) -> bool {
    let hashed = match h.val {
        Some(Val::Inst(class)) => hashed_container(project, class),
        Some(Val::Union(classes)) => classes.iter().any(|c| hashed_container(project, c)),
        _ => false,
    };
    hashed && items_may_define(project, kb, h, "__eq__")
}

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
/// runs there, not at its own site. A method of a nominal container ABC is read as the same
/// method of its family's builtin ([`abc_method`]).
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
    let builtin_method;
    let symbol = match abc_method(project, symbol) {
        AbcMethod::NotOne => symbol,
        AbcMethod::Open => return true,
        AbcMethod::Builtin(method) => {
            builtin_method = method;
            builtin_method.as_str()
        }
    };
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
        && replaced_in_project(project, kb, r, method)
    {
        return true;
    }
    let runs = |h: &Handed<'_>, names: &dyn Fn(&InvokedValue) -> bool| {
        entry
            .values
            .iter()
            .filter(|v| names(v))
            .any(|v| value_runs(project, kb, h, v, iterated))
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

/// What a method `symbol` of a nominal container ABC runs ([`abc_method`]).
enum AbcMethod {
    /// `symbol` is no method of one.
    NotOne,
    /// The family is open, or a project implementer may run other code for the method (it binds
    /// it, or inherits it from a class outside the project whose code it is, such as the ABC's
    /// own mixin method, which calls the implementer's methods by name).
    Open,
    /// The same method of the family's builtin, which runs what a value of the ABC runs.
    Builtin(String),
}

/// [`AbcMethod`] of `symbol`.
fn abc_method(project: &ProjectDecls, symbol: &str) -> AbcMethod {
    let (class, method) = symbol.rsplit_once('.').unwrap_or_default();
    let Some((kind, _)) = ContainerFamily::nominal_abc(class) else {
        return AbcMethod::NotOne;
    };
    if !project.established_abc(class)
        || project.special_method_owner(class, method) > MethodOwner::Builtin
    {
        return AbcMethod::Open;
    }
    AbcMethod::Builtin(format!("{}.{method}", kind.class()))
}

/// Whether `symbol` is a function of a library the ecosystem catalog covers: an entry of its
/// own, an `[[invokes]]` entry, or a member of a package the catalog lists as opaque.
fn catalogued(kb: &EffectiveEcosystemKb, symbol: &str) -> bool {
    kb.lookup(symbol).is_some()
        || kb.invoked(symbol).is_some()
        || kb.is_opaque_to_layer3(symbol.split('.').next().unwrap_or(symbol))
}

/// Whether what the call runs of `h`, as `v` names it, may be code of the project.
fn value_runs(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
    v: &InvokedValue,
    iterated: bool,
) -> bool {
    (v.calls && (iterated || !h.followed) && calls_project_code(project, kb, h))
        || v.methods.iter().any(|m| may_define(project, kb, h, m))
        || v.item_methods
            .iter()
            .any(|m| items_may_define(project, kb, h, m))
}

/// Whether one of the special methods `s` names may be code of the project, on the class of
/// the value `h` or of its items, or a key looked up in it compared with the keys it holds
/// ([`lookup_compares_items`]); a library's implementation is handed the instruction's other
/// operands too, which `others_inert` says are all [`inert`].
pub(crate) fn special_methods_run_project_code(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
    others_inert: bool,
    s: &heki_ir::SpecialMethods,
) -> bool {
    s.methods
        .iter()
        .any(|m| method_runs(project, kb, h, others_inert, m))
        || s.item_methods
            .iter()
            .any(|m| items_may_define(project, kb, h, m))
        || s.keyed && lookup_compares_items(project, kb, h)
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
/// plain method, or a write that may run code on a class it may be
/// (`ProjectDecls::attr_write_runs_no_code`: a property setter, `__setattr__`, `__delattr__`,
/// another descriptor, or code of a base heki does not know); on an instance of a library
/// class, the access as a call of that class's attribute (`__setattr__` for a write) under R3
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
    // A base outside the project whose `__setattr__` the catalog says stores on the instance
    // (R3: `pydantic.BaseModel`), which defines no `__getattribute__` and keeps the instance's
    // attributes in its own `__dict__`.
    let catalog_stores = |base: &str| {
        kb.lookup(&format!("{base}.__setattr__"))
            .is_some_and(|e| e.stores_on_instance)
    };
    let reads_plainly = |class: &str| {
        if OBJECT_ATTRS.contains(&attr) {
            project.object_attr_read_runs_no_code(class, attr, catalog_stores)
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
            AttrAccess::Write => !project.attr_write_runs_no_code(class, &[attr], catalog_stores),
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
        Some(Val::Super(_) | Val::OneOf(_)) | None => true,
    }
}

/// Whether `obj.__dict__` (and `vars(obj)`, which returns it) reads the instance's own
/// namespace, a builtin `dict`, running no code: `val`, what `obj` holds, is an instance whose
/// every class may be reads the attribute plainly ([`attribute_access_runs_project_code`]). A
/// class object's is a read-only proxy instead, and a value of unknown class may give anything.
pub(crate) fn reads_own_namespace(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    val: Option<&Val>,
) -> bool {
    matches!(val, Some(Val::Inst(_) | Val::Union(_)))
        && !attribute_access_runs_project_code(
            project,
            kb,
            &Handed {
                val,
                ..Handed::default()
            },
            "__dict__",
            AttrAccess::Read,
        )
}

/// Whether iterating `h` runs only its builtin class's own iteration: a builtin container,
/// string, range, or view, or a value of a nominal container ABC of a closed family, whose
/// iteration no project subclass or implementer replaces (R2), or a display built in the
/// calling function.
pub(crate) fn iterates_builtin(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
) -> bool {
    let builtin =
        |class: &str| BUILTIN_ITERABLES.contains(&class) || project.established_abc(class);
    let iterates_own = || !ITERATION.iter().any(|m| may_define(project, kb, h, m));
    h.built
        || match h.val {
            Some(Val::Inst(class)) => builtin(class) && iterates_own(),
            Some(Val::Union(classes)) => classes.iter().all(|c| builtin(c)) && iterates_own(),
            _ => false,
        }
}

/// Whether the special method `name` (`"*"`: any attribute) that Python looks up on the class
/// of `h` (on its metaclass for a class object) may run code of the project, the value handed
/// alone ([`method_runs`]).
fn may_define(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
    name: &str,
) -> bool {
    method_runs(project, kb, h, true, name)
}

/// Whether the special method `name` (`"*"`: any attribute) that Python looks up on the class of
/// `h` (on its metaclass for a class object) may run code of the project, the instruction's
/// other operands handed too (`others_inert`: each is [`inert`]). What may define it on the
/// classes the value may be (R2) decides ([`ProjectDecls::special_method_owner`]): `object`
/// runs nothing, a builtin type what it runs on the items ([`on_items`]); a library class's
/// method is a call of that library handed every operand (R3), which runs no code of the
/// project only when the catalog covers it and every operand is inert; a project class may run
/// anything. A value of an abstract class or a protocol, or of a class heki does not know, may
/// run anything.
fn method_runs(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
    others_inert: bool,
    name: &str,
) -> bool {
    if h.built {
        return on_items(project, instance_class(h), name)
            .iter()
            .any(|m| items_may_define(project, kb, h, m));
    }
    class_method_runs(project, kb, h, others_inert, name)
}

/// The class of `h` when it is an instance of one.
fn instance_class<'h>(h: &Handed<'h>) -> Option<&'h str> {
    match h.val {
        Some(Val::Inst(class)) => Some(class.as_str()),
        _ => None,
    }
}

/// [`method_runs`] of a value built nowhere heki sees: what its classes establish.
fn class_method_runs(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
    others_inert: bool,
    name: &str,
) -> bool {
    let instance = |class: &str| {
        if abstract_value(project, class) || project.protocol(class).is_some() {
            return true;
        }
        if name == "*" {
            return !project.value_classes(class).is_empty();
        }
        match project.special_method_owner(class, name) {
            MethodOwner::Project => true,
            MethodOwner::Object => false,
            MethodOwner::Builtin => on_items(project, Some(class), name)
                .iter()
                .any(|m| items_of_class(project, h, m)),
            MethodOwner::Library(library) => {
                !catalogued(kb, &format!("{library}.{name}"))
                    || !of_library_class(project, h)
                    || !others_inert
            }
        }
    };
    // A class object's special methods are its metaclass's: `type`'s own unless a class it may
    // be has a metaclass heki does not know, binds the name (`__class_getitem__`), or derives
    // from a class outside the project, which may bring a metaclass of its own.
    let class_object = |class: &str| {
        project.is_project_class(class)
            && (name == "*" || project.special_method_owner(class, name) > MethodOwner::Builtin)
    };
    match h.val {
        Some(Val::Inst(class)) => instance(class),
        Some(Val::Union(classes)) => classes.iter().any(|c| instance(c)),
        Some(Val::Class(class)) => class_object(class),
        Some(Val::Ref(symbol)) => {
            class_object(symbol) || (name == "*" && project_named(project, symbol))
        }
        Some(Val::Super(_) | Val::OneOf(_)) | None => true,
    }
}

/// Whether a call of a library class's method `name`, which looks the method up by name on
/// `h`, may run a project class's replacement of it instead (under R2, a project subclass's,
/// also through `__getattribute__`); on any other value, whether the method its class (its
/// metaclass for a class object) finds may run code of the project ([`may_define`]).
fn replaced_in_project(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
    name: &str,
) -> bool {
    let replaced = |class: &str| !project.method_not_overridden(class, name, true);
    match h.val {
        Some(Val::Inst(class)) if !h.built => replaced(class),
        Some(Val::Union(classes)) => classes.iter().any(|c| replaced(c)),
        _ => may_define(project, kb, h, name),
    }
}

/// Whether the special method `name` of the items of `h` may run code of the project: items of
/// a string, bytes, a number, `None`, or a range are builtin values, those of a display built in
/// the calling function are its elements, and those of any other value are of classes heki
/// does not know.
fn items_may_define(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    h: &Handed<'_>,
    name: &str,
) -> bool {
    if h.built {
        return h
            .elems
            .as_ref()
            .is_none_or(|elems| elems.iter().any(|e| element_runs(project, kb, e, name)));
    }
    items_of_class(project, h, name)
}

/// Whether the special method `name` of `e`, an element of a display built in the calling
/// function, may run code of the project: one built there too runs its builtin type's own,
/// which runs the methods of its items unless they are builtin values; any other as its classes
/// decide ([`class_method_runs`]).
fn element_runs(
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    e: &Handed<'_>,
    name: &str,
) -> bool {
    if e.built {
        return e.elems.is_none() && !on_items(project, instance_class(e), name).is_empty();
    }
    class_method_runs(project, kb, e, true, name)
}

/// [`items_may_define`] of a value built nowhere heki sees, by its class.
fn items_of_class(project: &ProjectDecls, h: &Handed<'_>, name: &str) -> bool {
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
            | "builtins.NoneType"
            | "builtins.range"
    ) && ITERATION
        .iter()
        .all(|m| project.method_not_overridden(class, m, false));
    // A builtin dict or set hands its keys on with the hashes it stored for them, and a dict
    // its entries as pairs it does not unpack through their own iteration. A key it holds is
    // still compared, with one of an equal hash.
    let stored = match class.as_str() {
        "builtins.dict" => matches!(name, "__hash__" | "__iter__" | "__next__" | "__getitem__"),
        "builtins.set" | "builtins.frozenset" | "builtins.dict_keys" => name == "__hash__",
        _ => false,
    } && project.method_not_overridden(class, "__iter__", false);
    !(builtin_items || stored)
}

/// The builtin types whose instances hold no values of other classes and compare, convert,
/// and operate to builtin values of their exact classes.
const SCALARS: &[&str] = &[
    "builtins.str",
    "builtins.bytes",
    "builtins.int",
    "builtins.float",
    "builtins.bool",
    "builtins.complex",
    "builtins.NoneType",
];

/// Whether the special methods `methods` Python looks up on the class of a value of the
/// inferred value `val` are `object`'s or a builtin type's on every class it may be (R2), so
/// that what they return is a builtin value of its exact class: a comparison a `bool`, a
/// conversion a `str`. `scalar`: a builtin type's must be a string's or a number's (an
/// ordering of two sequences gives what comparing their items gives).
pub(crate) fn interpreter_own(
    project: &ProjectDecls,
    val: Option<&Val>,
    methods: &[&str],
    scalar: bool,
) -> bool {
    let established = |class: &String| {
        !abstract_value(project, class)
            && project.protocol(class).is_none()
            && methods
                .iter()
                .all(|m| match project.special_method_owner(class, m) {
                    MethodOwner::Object => true,
                    MethodOwner::Builtin => !scalar || SCALARS.contains(&class.as_str()),
                    MethodOwner::Library(_) | MethodOwner::Project => false,
                })
    };
    match val {
        Some(Val::Inst(class)) => established(class),
        Some(Val::Union(classes)) => classes.iter().all(established),
        _ => false,
    }
}

/// Whether `h` may be an object whose `__dict__` keeps a value a property getter of the project
/// computed under the name `attr`, which a store or a deletion of `attr` through `h` replaces or
/// removes: unless `h` is an instance every class of which may be (R2) binds no property getter
/// named `attr` (a builtin value built in the calling function is one of its exact class). A
/// value of an abstract class or a protocol, a class object, and a value heki does not know
/// may.
pub(crate) fn may_keep_under(project: &ProjectDecls, h: &Handed<'_>, attr: &str) -> bool {
    let keeps = |class: &str| {
        abstract_value(project, class)
            || project.protocol(class).is_some()
            || project.binds_property(class, attr)
    };
    match h.val {
        Some(Val::Inst(class)) => keeps(class),
        Some(Val::Union(classes)) => classes.iter().any(|c| keeps(c)),
        _ => true,
    }
}

/// Whether `symbol` names something of the project (a module, class, function, or global).
fn project_named(project: &ProjectDecls, symbol: &str) -> bool {
    !matches!(project.symbol(symbol), Symbol::External)
}

/// Whether calling `h` may run code of the project: a project function or class, a callable
/// library object (a `functools.partial` holds what it calls), an instance whose `__call__` may
/// be the project's, or a callable heki does not know.
fn calls_project_code(project: &ProjectDecls, kb: &EffectiveEcosystemKb, h: &Handed<'_>) -> bool {
    if h.built {
        return false;
    }
    match h.val {
        Some(Val::Ref(symbol)) => project_named(project, symbol),
        Some(Val::OneOf(symbols)) => symbols.iter().any(|s| project_named(project, s)),
        Some(Val::Class(class)) => project.is_project_class(class),
        Some(Val::Inst(class)) if CALLABLE_CLASSES.contains(&class.as_str()) => true,
        Some(Val::Inst(_) | Val::Union(_)) => may_define(project, kb, h, "__call__"),
        Some(Val::Super(_)) | None => true,
    }
}

/// Whether handing `h` to a library call runs no project code because of it, whatever the call
/// does with it: it is built in the calling function from such values, or its class is
/// established as a library class that is not callable and that no project class derives from
/// (R2).
pub(crate) fn inert(project: &ProjectDecls, h: &Handed<'_>) -> bool {
    if h.built {
        return h.elems.as_ref().is_some_and(|elems| {
            elems
                .iter()
                .all(|e| e.built || of_library_class(project, e))
        });
    }
    of_library_class(project, h)
}

/// Whether the class of `h` is established as a library class that is not callable, not
/// abstract ([`abstract_value`]), and that no project class derives from or implements (R2).
fn of_library_class(project: &ProjectDecls, h: &Handed<'_>) -> bool {
    let library = |class: &str| {
        !project.is_project_class(class)
            && !CALLABLE_CLASSES.contains(&class)
            && !abstract_value(project, class)
            && project.value_classes(class).is_empty()
    };
    match h.val {
        Some(Val::Inst(class)) => library(class),
        Some(Val::Union(classes)) => classes.iter().all(|c| library(c)),
        _ => false,
    }
}
