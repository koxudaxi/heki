//! Functions returning what a call returns of their parameter (`return same(x)`). Pass 1 records
//! the calls each such function returns the result of ([`ForwardedReturn`]); completing a module
//! decides [`FunctionDecl::returns_param`] of its such functions against what the callees
//! return in pass 1, through any number of calls ([`complete`]).

use super::{FunctionDecl, ModuleDecls, ProjectDecls, Symbol};
use crate::types::Ty;
use crate::{FuncKey, ModuleInput};
use heki_ir::walk::{Budget, Facts, Join, Sparse, Visit, fixpoint, reach};
use heki_ir::{InstKind, VarId};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// The owner of the budget of a module's [`complete`].
const OWNER: &str = "heki_effect::resolver::forwarded";

/// What every `return` of every definition of a function hands back when some return gives the
/// result of a call ([`ForwardingCall`]) and each other one gives a parameter, or an attribute of
/// one, as [`FunctionDecl::returns_param`] says it: the function returns that parameter when each
/// call returns its argument that the parameter binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardedReturn {
    /// What the returns giving no call's result give (`None`: every return gives one).
    pub direct: Option<(u8, Option<String>)>,
    /// The calls whose result a return gives, each once.
    pub calls: Box<[ForwardingCall]>,
    /// The function stores or deletes an attribute, or may run code between a call and the
    /// return giving its result: what the call returned of an attribute of its argument
    /// (`items = items_of(node); node.items = []; return items`, `items = items_of(node);
    /// reset(node); return items`) may no longer be the attribute.
    pub rebinds_attrs: bool,
}

/// A call whose result a function returns ([`ForwardedReturn`]): a module global called by name
/// (`same(x)`) or an attribute of an import (`ops.same(x)`), with the parameters of the function
/// its arguments are.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ForwardingCall {
    /// The name called, as a global of the function's module (`app.use.same`,
    /// `app.use.ops.same`), which [`ProjectDecls::named_ty`] reads through the module's imports.
    pub callee: Box<str>,
    /// The parameter each positional argument before any `*` unpacking is (`None`: another
    /// value).
    pub positional: Box<[Option<u8>]>,
    /// The keyword arguments that are parameters, and which: none in a call unpacking positional
    /// arguments, as a call binds the argument it hands back in the summaries.
    pub keywords: Box<[(Box<str>, u8)]>,
}

impl ForwardingCall {
    /// The call `kind` of a function of the module `decls` (whose instructions `defs` defines
    /// its values), each argument the parameter `param` says it is: `None` for any other
    /// instruction, a call of a builtin or a class of the module (it makes another object), and
    /// a call passing no parameter.
    pub(super) fn of(
        kind: &InstKind,
        defs: &FxHashMap<VarId, &InstKind>,
        decls: &ModuleDecls,
        param: &mut dyn FnMut(VarId) -> Option<u8>,
    ) -> Option<Self> {
        let global = |var: &VarId| match defs.get(var) {
            Some(InstKind::LoadGlobal { name, .. }) => Some(name.as_str()),
            _ => None,
        };
        // An import the module binds again may hold another object when the call runs.
        let imported = |name: &&str| decls.fixed_import(name).is_some();
        let own = |name: &&str| !decls.imports.contains_key(*name) && decls.binds(name);
        let module = &decls.module_name;
        let (callee, args, kwargs, star_from) = match kind {
            InstKind::Call {
                callee_var: Some(c),
                args,
                kwargs,
                star_from,
                ..
            } => {
                let name = global(c)
                    .filter(|n| (imported(n) || own(n)) && !decls.classes.contains_key(*n))?;
                (format!("{module}.{name}"), args, kwargs, *star_from)
            }
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                star_from,
                ..
            } => {
                let name = global(recv).filter(imported)?;
                (
                    format!("{module}.{name}.{method}"),
                    args,
                    kwargs,
                    *star_from,
                )
            }
            _ => return None,
        };
        let before_star = star_from.map_or(args.len(), |s| args.len().min(usize::from(s)));
        let positional: Box<[Option<u8>]> = args[..before_star].iter().map(|a| param(*a)).collect();
        let keywords: Box<[(Box<str>, u8)]> = if star_from.is_some() {
            Box::default()
        } else {
            (kwargs.iter())
                .filter_map(|(name, v)| Some((Box::from(name.as_deref()?), param(*v)?)))
                .collect()
        };
        (positional.iter().any(Option::is_some) || !keywords.is_empty()).then(|| Self {
            callee: callee.into(),
            positional,
            keywords,
        })
    }

    /// The parameter of the calling function the call binds to the parameter `k` of `callee`.
    fn binds(&self, callee: &FunctionDecl, k: u8) -> Option<u8> {
        let k = usize::from(k);
        match self.positional.get(k) {
            Some(param) => *param,
            None => (self.keywords.iter())
                .find(|(name, _)| callee.keyword_param(name) == Some(k))
                .map(|(_, param)| *param),
        }
    }
}

/// What a function of the fixpoint of [`complete`] returns: the same parameter (or attribute of
/// it) on every path, or anything else, once a path shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Returned {
    Param(u8, Option<String>),
    Other,
}

impl Join for Returned {
    fn join(&mut self, other: &Self) -> bool {
        let joined = (self == other).then(|| other.clone());
        std::mem::replace(self, joined.unwrap_or(Self::Other)) != *self
    }
}

/// What one call of a function of the fixpoint returns of the caller's parameters.
enum CallReturns {
    /// A callee returning its parameter (or an attribute of it), which the call binds to this
    /// parameter of the caller.
    Param(u8, Option<String>),
    /// A callee returning what calls of its own return (a node of the fixpoint), with the
    /// caller's parameter the call binds to each of its positional parameters.
    Node(usize, Box<[Option<u8>]>),
    /// Anything else: a class, a method, a function of no such fact, or a parameter the call
    /// binds to no parameter of the caller.
    Other,
}

/// A function of the fixpoint: its [`ForwardedReturn`], each call resolved.
struct Node {
    direct: Option<(u8, Option<String>)>,
    rebinds_attrs: bool,
    calls: Vec<CallReturns>,
}

/// [`FunctionDecl::returns_param`] of the module's functions returning what calls return
/// ([`FunctionDecl::forwards`]), against pass 1 `project`: parameter `i` (or an attribute of it)
/// when each call calls a project function returning its parameter `k`, itself or through calls
/// of its own, and binds `k` to `i`. The functions along the calls read each other's facts: a
/// fixpoint from no fact, so a cycle of calls (`a(x)` returning `b(x)`, which returns `a(x)`)
/// decides nothing, and past the visit limit or the module's budget (each visit of a function is
/// charged its calls) nothing is decided.
pub(super) fn complete(input: &ModuleInput<'_>, decls: &mut ModuleDecls, project: &ProjectDecls) {
    for (qualname, returned) in decided(input, decls, project) {
        if let Some(decl) = decls.functions.get_mut(&qualname) {
            decl.returns_param = Some(returned);
        }
    }
}

/// What [`complete`] decides of the module's functions, by qualname.
fn decided(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
    project: &ProjectDecls,
) -> Vec<(String, (u8, Option<String>))> {
    let roots: Vec<(&String, &ForwardedReturn)> = (decls.functions.iter())
        .filter_map(|(qualname, d)| Some((qualname, d.forwards.as_deref()?)))
        .collect();
    if roots.is_empty() {
        return Vec::new();
    }
    let budget = Budget::of_functions(OWNER, &input.ir.functions);
    let keys = roots.iter().map(|(qualname, forwards)| {
        let key = FuncKey {
            module: decls.module_name.clone(),
            qualname: (*qualname).clone(),
        };
        (key, *forwards)
    });
    let facts = decide(&budget, decls, project, keys.collect());
    let returned = |i: usize| match facts.as_ref()?.get(i)? {
        Returned::Param(param, attr) => Some((*param, attr.clone())),
        Returned::Other => None,
    };
    (roots.iter().enumerate())
        .filter_map(|(i, (qualname, _))| Some(((*qualname).clone(), returned(i)?)))
        .collect()
}

/// The facts of the functions `roots` (the first nodes) and those they reach along their calls:
/// `None` past the budget or the visit limit.
fn decide<'d>(
    budget: &Budget,
    decls: &'d ModuleDecls,
    project: &'d ProjectDecls,
    roots: Vec<(FuncKey, &'d ForwardedReturn)>,
) -> Option<Facts<Returned>> {
    let starts = 0..roots.len();
    let mut nodes: Vec<Option<Node>> = (0..roots.len()).map(|_| None).collect();
    let mut index: FxHashMap<FuncKey, usize> = FxHashMap::default();
    let mut keys: Vec<&ForwardedReturn> = Vec::with_capacity(roots.len());
    for (key, forwards) in roots {
        index.insert(key, keys.len());
        keys.push(forwards);
    }
    let expand = |i: usize, out: &mut Vec<usize>| {
        let forwards = keys[i];
        // Past the budget the node is not read, and nothing is decided without it.
        if !budget.take(forwards.calls.len()) {
            return;
        }
        let calls = (forwards.calls.iter())
            .map(|call| {
                let Some((key, callee)) = callee(project, call) else {
                    return CallReturns::Other;
                };
                let callee = decl_of(decls, project, &key).unwrap_or(callee);
                if let Some(callee_forwards) = callee.forwards.as_deref() {
                    let bound = (0..callee.positional_params)
                        .map(|k| call.binds(callee, k))
                        .collect();
                    let next = *index.entry(key).or_insert_with(|| {
                        keys.push(callee_forwards);
                        nodes.push(None);
                        keys.len() - 1
                    });
                    out.push(next);
                    return CallReturns::Node(next, bound);
                }
                let returned = callee.returns_param.as_ref();
                match returned.and_then(|(k, attr)| Some((call.binds(callee, *k)?, attr))) {
                    Some((param, attr)) => CallReturns::Param(param, attr.clone()),
                    None => CallReturns::Other,
                }
            })
            .collect();
        nodes[i] = Some(Node {
            direct: forwards.direct.clone(),
            rebinds_attrs: forwards.rebinds_attrs,
            calls,
        });
    };
    let walked = reach(budget, starts, expand, |_| Visit::<()>::Expand);
    // Past the budget, the fixpoint answers open too.
    let nodes: Vec<Node> =
        (nodes.into_iter().collect::<Option<_>>()).filter(|_| !walked.is_open())?;
    let mut readers: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, node) in nodes.iter().enumerate() {
        for call in &node.calls {
            if let CallReturns::Node(j, _) = call {
                readers[*j].push(i);
            }
        }
    }
    // A node is visited again only when the fact of a node it calls grows, at most twice each.
    let most_calls = (nodes.iter())
        .map(|n| {
            (n.calls.iter())
                .filter(|c| matches!(c, CallReturns::Node(..)))
                .count()
        })
        .max()
        .unwrap_or(0);
    let spec = Sparse {
        nodes: nodes.len(),
        readers: &|i, out| out.extend_from_slice(&readers[i]),
        max_visits: u32::try_from(1 + 2 * most_calls).unwrap_or(u32::MAX),
    };
    // Each visit inspects every call of the node, so a node with many calls that is visited
    // again for each callee resolving pays for them all: the calls are charged on each visit.
    let walked = fixpoint(budget, spec, |i, facts| {
        budget
            .take(nodes[i].calls.len())
            .then(|| returned(&nodes[i], facts))
            .flatten()
    });
    walked
        .map(Some)
        .open_means(None)
        .filter(|_| !budget.spent())
}

/// The declaration of `key`: the module's own functions are those of its file, since pass 1
/// `project` may hold those of another file of the same module name (a `.py` implementation next
/// to its `.pyi` stub).
fn decl_of<'d>(
    decls: &'d ModuleDecls,
    project: &'d ProjectDecls,
    key: &FuncKey,
) -> Option<&'d FunctionDecl> {
    if key.module == decls.module_name {
        decls.functions.get(&key.qualname).map(Box::as_ref)
    } else {
        project.function_decl(key)
    }
}

/// What `node` returns, as far as the facts of the nodes it calls tell (`None`: a node it calls
/// has none yet).
fn returned(node: &Node, facts: &Facts<Returned>) -> Option<Returned> {
    let mut returned = node.direct.clone();
    let mut waiting = false;
    for call in &node.calls {
        let this = match call {
            CallReturns::Param(param, attr) => (*param, attr.clone()),
            CallReturns::Other => return Some(Returned::Other),
            CallReturns::Node(j, bound) => match facts.get(*j) {
                None => {
                    waiting = true;
                    continue;
                }
                Some(Returned::Other) => return Some(Returned::Other),
                Some(Returned::Param(k, attr)) => match bound.get(usize::from(*k)) {
                    Some(Some(param)) => (*param, attr.clone()),
                    _ => return Some(Returned::Other),
                },
            },
        };
        if this.1.is_some() && node.rebinds_attrs {
            return Some(Returned::Other);
        }
        if *returned.get_or_insert_with(|| this.clone()) != this {
            return Some(Returned::Other);
        }
    }
    if waiting {
        return None;
    }
    returned.map(|(param, attr)| Returned::Param(param, attr))
}

/// The project function `call` calls, as `project` names it, and its key: not a method, whose
/// receiver or class a call by this name would not bind as a parameter, nor a function the
/// project may bind again, whose call may run another object ([`ProjectDecls::call_rebound`]).
fn callee<'p>(
    project: &'p ProjectDecls,
    call: &ForwardingCall,
) -> Option<(FuncKey, &'p FunctionDecl)> {
    let Ty::Ref(fqn) = project.named_ty(&call.callee) else {
        return None;
    };
    if project.call_rebound(&fqn) {
        return None;
    }
    match project.symbol(&fqn) {
        Symbol::Function(key, decl) if !decl.is_method => Some((key, decl)),
        _ => None,
    }
}
