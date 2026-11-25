//! What a decorator leaves of the function it decorates: the one table the premises about
//! decorated functions read. A decorator is known by the object its head is bound to in the
//! function's module ([`ModuleDecls::decorator_symbol`]), not by its spelling: a `final` the
//! project defines, or one of another library, may replace the function. Any decorator the
//! table does not name may replace the function, so a premise it touches is open.

use crate::ModuleDecls;

/// One decorator heki knows, and which premises about the function it keeps.
pub(crate) struct Decorator {
    pub name: &'static str,
    /// The modules that define it. `None` for `cached_property`, a getter by its name wherever
    /// it is imported from: only the catalog's `[[cached_properties]]` make one an exact
    /// claim's, and it keeps no other premise.
    pub modules: Option<&'static [&'static str]>,
    /// Calling the function still runs its body as written: a generator function returns a
    /// generator, an `async def` a coroutine, a plain `def` what its body returns
    /// (`FunctionDecl::called_as_defined`).
    pub keeps_call: bool,
    /// Every call runs the body and returns what it returns, a new object each time
    /// ([`keeps_returned_value`]); a cache hands out one object instead.
    pub keeps_return: bool,
    /// Reading the attribute on an instance runs the function (a getter).
    pub getter: bool,
    /// A `functools` cache beside it still keeps the function's own results (`MUT114`): it
    /// returns the object it is handed, or calls it as defined.
    pub beside_cache: bool,
}

/// `typing.override` and `typing.final` only set an attribute on the object they are handed
/// (`__override__`, `__final__`) and return it; `typing_extensions` defines the same functions.
const TYPING: &[&str] = &["typing", "typing_extensions"];

pub(crate) const DECORATORS: &[Decorator] = &[
    Decorator {
        name: "staticmethod",
        modules: Some(&["builtins"]),
        keeps_call: true,
        keeps_return: true,
        getter: false,
        beside_cache: true,
    },
    Decorator {
        name: "classmethod",
        modules: Some(&["builtins"]),
        keeps_call: true,
        keeps_return: true,
        getter: false,
        beside_cache: true,
    },
    Decorator {
        name: "abstractmethod",
        modules: Some(&["abc"]),
        keeps_call: true,
        keeps_return: true,
        getter: false,
        beside_cache: false,
    },
    Decorator {
        name: "override",
        modules: Some(TYPING),
        keeps_call: true,
        keeps_return: true,
        getter: false,
        beside_cache: true,
    },
    Decorator {
        name: "final",
        modules: Some(TYPING),
        keeps_call: true,
        keeps_return: true,
        getter: false,
        beside_cache: true,
    },
    Decorator {
        name: "property",
        modules: Some(&["builtins"]),
        keeps_call: false,
        keeps_return: true,
        getter: true,
        beside_cache: false,
    },
    Decorator {
        name: "cached_property",
        modules: None,
        keeps_call: false,
        keeps_return: false,
        getter: true,
        beside_cache: false,
    },
];

/// The decorator of the table that `decorator` (its source text, on a function of the module
/// `decls` describes) is: the one its head is bound to there, written without a call
/// (`@final()` decorates with what calling `final` returns).
fn entry(decorator: &str, decls: &ModuleDecls) -> Option<&'static Decorator> {
    if decorator.contains('(') {
        return None;
    }
    let symbol = decls.decorator_symbol(decorator)?;
    let (module, name) = symbol.rsplit_once('.')?;
    DECORATORS
        .iter()
        .find(|d| d.name == name && d.modules.is_none_or(|m| m.contains(&module)))
}

/// Whether `decorator` (its source text, on a function of the module `decls` describes) is a
/// decorator of the table that has `keep`.
pub(crate) fn is(decorator: &str, decls: &ModuleDecls, keep: impl Fn(&Decorator) -> bool) -> bool {
    entry(decorator, decls).is_some_and(keep)
}

/// Whether every decorator of `decorators` (their source text, on a function of the module
/// `decls` describes) has `keep`.
pub(crate) fn all_keep(
    decorators: &[String],
    decls: &ModuleDecls,
    keep: impl Fn(&Decorator) -> bool,
) -> bool {
    decorators.iter().all(|d| is(d, decls, &keep))
}

/// Whether calling a function decorated with `decorators` returns what its body returns: a
/// decorator may hand out one cached object (`@lru_cache`, `@cached_property`), so only the
/// decorators that call the body each time keep its result.
pub(crate) fn keeps_returned_value(decorators: &[String], decls: &ModuleDecls) -> bool {
    all_keep(decorators, decls, |d| d.keeps_return)
}
