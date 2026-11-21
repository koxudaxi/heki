//! What a decorator leaves of the function it decorates, by its last dotted name: the one table
//! the premises about decorated functions read. Any decorator the table does not name may
//! replace the function, so a premise it touches is open.

/// One decorator heki knows, and which premises about the function it keeps.
pub(crate) struct Decorator {
    pub name: &'static str,
    /// Calling the function still runs its body as written: a generator function returns a
    /// generator, an `async def` a coroutine, a plain `def` what its body returns
    /// (`FunctionDecl::called_as_defined`).
    pub keeps_call: bool,
    /// Every call runs the body and returns what it returns, a new object each time
    /// ([`keeps_returned_value`]); a cache hands out one object instead.
    pub keeps_return: bool,
    /// Reading the attribute on an instance runs the function (a getter).
    pub getter: bool,
    /// A `functools` cache beside it still keeps the function's own results (`MUT114`).
    pub beside_cache: bool,
}

pub(crate) const DECORATORS: &[Decorator] = &[
    Decorator {
        name: "staticmethod",
        keeps_call: true,
        keeps_return: true,
        getter: false,
        beside_cache: true,
    },
    Decorator {
        name: "classmethod",
        keeps_call: true,
        keeps_return: true,
        getter: false,
        beside_cache: true,
    },
    Decorator {
        name: "abstractmethod",
        keeps_call: true,
        keeps_return: true,
        getter: false,
        beside_cache: false,
    },
    Decorator {
        name: "override",
        keeps_call: true,
        keeps_return: true,
        getter: false,
        beside_cache: false,
    },
    Decorator {
        name: "property",
        keeps_call: false,
        keeps_return: true,
        getter: true,
        beside_cache: false,
    },
    Decorator {
        name: "cached_property",
        keeps_call: false,
        keeps_return: false,
        getter: true,
        beside_cache: false,
    },
];

/// The last dotted name of a decorator's head (`functools.cached_property` of
/// `@functools.cached_property`, `lru_cache` of `@lru_cache(maxsize=8)`).
pub(crate) fn last_name(decorator: &str) -> &str {
    let base = decorator.split('(').next().unwrap_or(decorator).trim();
    base.rsplit('.').next().unwrap_or(base)
}

/// The names of the decorators that have `keep`.
pub(crate) fn names(keep: impl Fn(&Decorator) -> bool) -> Vec<&'static str> {
    DECORATORS
        .iter()
        .filter(|d| keep(d))
        .map(|d| d.name)
        .collect()
}

/// Whether the decorator named `name` (a last dotted name) has `keep`.
pub(crate) fn keeps(name: &str, keep: impl Fn(&Decorator) -> bool) -> bool {
    DECORATORS.iter().any(|d| d.name == name && keep(d))
}

/// Whether every decorator of `decorators` (their source text) has `keep`.
pub(crate) fn all_keep(decorators: &[String], keep: impl Fn(&Decorator) -> bool) -> bool {
    decorators.iter().all(|d| keeps(last_name(d), &keep))
}

/// Whether calling a function decorated with `decorators` returns what its body returns: a
/// decorator may hand out one cached object (`@lru_cache`, `@cached_property`), so only the
/// decorators that call the body each time keep its result.
pub(crate) fn keeps_returned_value(decorators: &[String]) -> bool {
    all_keep(decorators, |d| d.keeps_return)
}
