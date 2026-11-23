//! The calls heki is blind to (`OptInTables::blind_spots`): a call it resolves to no function,
//! so nothing the callee does reaches the caller and no rule reports through it, and a call of a
//! library function or class the ecosystem catalog has no entry for, which heki takes to have no
//! effect (docs/scope.md, "When heki cannot tell"). Only `heki recommend` reads them, so they
//! are extracted only with [`crate::OptInFacts::blind_spots`]. Which functions run on an event
//! loop or below a loop is [`crate::ProjectIndex::blind_spot_reach`].

use crate::FuncKey;
use crate::resolver::TypeCtx;
use heki_ast::Span;
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};

/// One call heki is blind to, in its function's own body (a local fact, never propagated).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlindCall {
    pub span: Span,
    /// The loops around the call in its function (0: none).
    pub loop_depth: u16,
    pub is_awaited: bool,
    pub blind: Blind,
}

/// What heki does not see of a call.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Blind {
    /// A call heki resolves to no function. `name` is the method called (`client.get(...)`,
    /// also through `m = client.get` or a property read) or the dotted name of the value called
    /// (`handler(...)`); `None` for a value computed in place (`make()(...)`).
    Unresolved {
        name: Option<String>,
        reason: UnresolvedReason,
    },
    /// A call of `symbol`, spelled as heki resolves it (the exact spelling an ecosystem entry
    /// needs: entries are looked up without inheritance or aliases), a function or class of a
    /// package that is neither the project's nor the standard library's and that the catalog has
    /// no entry for. `registered`: the catalog lists the package, so only this name lacks one.
    Uncatalogued { symbol: String, registered: bool },
}

/// Why heki resolves a call to no function, from what it knows of the receiver (`obj` of
/// `obj.m(...)`) or of the value called (`f` of `f(...)`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    /// Parameter `param` (named `name`) of the calling function, which has no annotation.
    UnannotatedParameter { param: u8, name: String },
    /// Parameter `param` (named `name`), whose annotation gives heki no class (`type`, a type
    /// variable, a name heki does not find).
    UnresolvedAnnotation { param: u8, name: String },
    /// A value annotated, or narrowed to, `Any` or `object`.
    AnyValue,
    /// A value annotated `Callable`: heki does not know which function it holds.
    CallableValue,
    /// A name nothing heki sees binds: a star import of a module outside the project, or a
    /// name bound at run time (`globals()[name] = ...`).
    UnboundName,
    /// A value of a project protocol whose implementations heki cannot list.
    UnlistedProtocol,
    /// The result of a call heki gives no class.
    CallResult,
    /// An attribute heki gives no class.
    Attribute,
    /// The bound method `getattr(obj, name)` returns, for a name heki cannot evaluate.
    ComputedName,
    /// More implementations than heki follows ([`crate::resolver::CHA_CANDIDATE_LIMIT`]).
    TooManyImplementations,
    /// A value whose class, module, or name heki knows, but not the member called: a base class
    /// heki knows nothing of may define it, or it is in a module heki could not read.
    UnknownMember,
    /// Any other value heki gives no class: an item of a container, a global, a loop variable.
    UntypedValue,
}

impl UnresolvedReason {
    /// The reason's name, as the JSON payloads spell it.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnannotatedParameter { .. } => "unannotated-parameter",
            Self::UnresolvedAnnotation { .. } => "unresolved-annotation",
            Self::AnyValue => "any-value",
            Self::CallableValue => "callable-value",
            Self::UnboundName => "unbound-name",
            Self::UnlistedProtocol => "unlisted-protocol",
            Self::CallResult => "call-result",
            Self::Attribute => "attribute",
            Self::ComputedName => "computed-name",
            Self::TooManyImplementations => "too-many-implementations",
            Self::UnknownMember => "unknown-member",
            Self::UntypedValue => "untyped-value",
        }
    }

    /// The parameter's name, for a reason about a parameter.
    pub fn parameter(&self) -> Option<&str> {
        match self {
            Self::UnannotatedParameter { name, .. } | Self::UnresolvedAnnotation { name, .. } => {
                Some(name)
            }
            _ => None,
        }
    }
}

/// Whether a call of the library name `symbol` is blind ([`Blind::Uncatalogued`]), and if so
/// whether the catalog lists its package; `None` for a name of the project or of the standard
/// library, and for one the catalog has an entry for, effects or not.
pub(crate) fn uncatalogued(cx: &TypeCtx<'_>, symbol: &str) -> Option<bool> {
    let package = symbol.split('.').next().unwrap_or(symbol);
    if heki_env::is_stdlib_module(package)
        || cx.catalog(symbol).is_some()
        || cx.project.in_project_module(symbol)
    {
        return None;
    }
    Some(cx.kb.is_opaque_to_layer3(package))
}

/// Which functions run on an event loop and which below a loop
/// ([`crate::ProjectIndex::blind_spot_reach`]): what a blind call in them may hold up.
#[derive(Debug, Default)]
pub struct BlindSpotReach {
    /// Every `async def`, and every function a call in one of these runs on the same thread
    /// (not handed to a worker thread), transitively.
    pub on_event_loop: FxHashSet<FuncKey>,
    /// Every function a call inside a loop, or a builtin calling it once per item, runs, and
    /// every function a call in one of these runs, transitively.
    pub below_loop: FxHashSet<FuncKey>,
}
