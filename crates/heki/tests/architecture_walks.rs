//! Walks over IR ids, the call graph, or the class hierarchy go through `heki_ir::walk`
//! (AGENTS.md, invariant 8; docs/development.md, "Analysis cost"), whose engines keep each answer
//! per value, join facts with a pass limit, end on cycles, and spend one budget per owner.
//!
//! This test finds walks written by hand in the analysis crates (`heki_ir` outside
//! `heki_ir::walk`, `heki_effect`, `heki_rules`, `heki_soundness`):
//!
//! - W1: a function in a call cycle (direct or mutual recursion) that touches IR ids;
//! - W2: a worklist (`while let Some(..) = x.pop()`, `pop_front()`, `pop_first()`,
//!   `pop_last()`) in a function that touches IR ids;
//! - W3: a pass repeated until nothing changes: a `loop`, a `while`, or a `for` over a range
//!   whose end is no constant (`0..links`, `1..`), around a `for` over the `blocks`, `phis`, or
//!   `insts` of a function or its reverse postorder, or stopping on the facts `before` the pass,
//!   on a size (a `loop` or `while`), or on a flag each pass clears and sets (`changed`). Its
//!   cost is the passes times the function: the passes a chain laid against the order of the
//!   sweep takes grow with the chain;
//! - W4: a linear lookup that `heki_ir::walk::FnShape` answers from a table (`def_inst`,
//!   `const_int`, `const_str`, `const_bool`, `block_loop_depth`, `innermost_loop`,
//!   `is_in_loop`, `predecessors`, `rpo_blocks`, `.body.contains(..)`).
//!
//! Calls are resolved by name within a crate: through `self.` and `Self::` to the methods of
//! the impl, through a type's path to its methods, and other method calls to every method of
//! the name, so the call graph is wider than the real one. `walks.allow` lists the functions
//! allowed so far, one per line with the codes it allows and why. The test fails on a finding
//! the list does not allow, and on an allowed code that finds nothing any more: the list only
//! shrinks.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{Expr, ImplItem, Item, Member, Pat, TraitItem, Type};

const CRATES: [&str; 4] = ["heki_ir", "heki_effect", "heki_rules", "heki_soundness"];

/// Names whose presence in a function's signature or body means it handles IR ids.
const IR_IDS: [&str; 16] = [
    "VarId",
    "BlockId",
    "FunctionId",
    "ClassId",
    "FuncKey",
    "Inst",
    "PhiNode",
    "BasicBlock",
    "InstKind",
    "incoming",
    "phis",
    "operands",
    "successors",
    "subclasses",
    "mro",
    "callers",
];

/// Linear helpers of `IrFunction` and the private tables standing in for them.
const LINEAR: [&str; 9] = [
    "def_inst",
    "const_int",
    "const_str",
    "const_bool",
    "block_loop_depth",
    "innermost_loop",
    "is_in_loop",
    "predecessors",
    "rpo_blocks",
];

const POPS: [&str; 4] = ["pop", "pop_front", "pop_first", "pop_last"];

/// Methods of the standard library's collections, options, and iterators: a call of one on a
/// receiver other than `self` is not resolved by name.
const STD_METHODS: &[&str] = &[
    "all",
    "and_then",
    "any",
    "append",
    "as_deref",
    "as_ref",
    "as_slice",
    "as_str",
    "back",
    "binary_search",
    "borrow",
    "borrow_mut",
    "chain",
    "chars",
    "clear",
    "clone",
    "cloned",
    "cmp",
    "collect",
    "contains",
    "contains_key",
    "copied",
    "count",
    "dedup",
    "default",
    "difference",
    "drain",
    "ends_with",
    "entry",
    "enumerate",
    "eq",
    "expect",
    "extend",
    "filter",
    "filter_map",
    "find",
    "first",
    "flat_map",
    "fmt",
    "from",
    "front",
    "get",
    "get_mut",
    "get_or_init",
    "hash",
    "insert",
    "intersection",
    "into",
    "into_iter",
    "is_empty",
    "is_none",
    "is_none_or",
    "is_some",
    "is_some_and",
    "iter",
    "iter_mut",
    "join",
    "keys",
    "last",
    "len",
    "load",
    "lock",
    "map",
    "map_or",
    "max",
    "min",
    "next",
    "ok",
    "or_default",
    "or_insert",
    "or_insert_with",
    "partial_cmp",
    "peek",
    "pop",
    "pop_back",
    "pop_first",
    "pop_front",
    "pop_last",
    "position",
    "push",
    "push_back",
    "push_front",
    "push_str",
    "read",
    "remove",
    "replace",
    "resize",
    "retain",
    "rev",
    "set",
    "sort",
    "sort_unstable",
    "split",
    "split_off",
    "starts_with",
    "store",
    "sum",
    "swap",
    "take",
    "then",
    "then_some",
    "to_owned",
    "to_string",
    "trim",
    "truncate",
    "try_from",
    "union",
    "unwrap",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "values",
    "windows",
    "with",
    "write",
    "zip",
];

#[derive(Clone)]
enum Call {
    /// `f(..)`, `module::f(..)`, `Type::f(..)`, or `Self::f(..)`: the path.
    Path(Vec<String>),
    /// `self.f(..)`.
    OnSelf(String),
    /// `x.f(..)`.
    Method(String),
}

#[derive(Default)]
struct Scan {
    idents: HashSet<String>,
    calls: Vec<Call>,
    linear: BTreeSet<String>,
    worklist: bool,
    dense: bool,
    nested: Vec<syn::ItemFn>,
}

struct Function {
    /// `crate::module::Type::name` or `crate::module::name`.
    name: String,
    module: String,
    owner: Option<String>,
    simple: String,
    scan: Scan,
}

impl Scan {
    fn call(&mut self, call: Call) {
        let name = match &call {
            Call::Path(path) => path.last().expect("a path has a segment"),
            Call::OnSelf(name) | Call::Method(name) => name,
        };
        if LINEAR.contains(&name.as_str()) {
            self.linear.insert(name.clone());
        }
        self.calls.push(call);
    }

    /// Calls and names inside a macro's tokens, which are not parsed as expressions.
    fn tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        let punct = |i: usize, c: char| matches!(tokens.get(i), Some(TokenTree::Punct(p)) if p.as_char() == c);
        let ident = |i: usize| match tokens.get(i) {
            Some(TokenTree::Ident(id)) => Some(id.to_string()),
            _ => None,
        };
        for (i, token) in tokens.iter().enumerate() {
            let name = match token {
                TokenTree::Group(group) => {
                    self.tokens(group.stream());
                    continue;
                }
                TokenTree::Ident(id) => id.to_string(),
                _ => continue,
            };
            self.idents.insert(name.clone());
            let called = matches!(tokens.get(i + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis);
            if !called {
                continue;
            }
            if i >= 1 && punct(i - 1, '.') {
                let on_self = i >= 2
                    && ident(i - 2).as_deref() == Some("self")
                    && !(i >= 3 && punct(i - 3, '.'));
                if name == "contains" && i >= 2 && ident(i - 2).as_deref() == Some("body") {
                    self.linear.insert("body.contains".to_owned());
                }
                self.call(if on_self {
                    Call::OnSelf(name)
                } else {
                    Call::Method(name)
                });
                continue;
            }
            let mut path = vec![name];
            let mut at = i;
            while at >= 3 && punct(at - 1, ':') && punct(at - 2, ':') {
                let Some(segment) = ident(at - 3) else { break };
                path.insert(0, segment);
                at -= 3;
            }
            if at >= 1 && (ident(at - 1).as_deref() == Some("fn") || punct(at - 1, '!')) {
                continue;
            }
            self.call(Call::Path(path));
        }
    }
}

/// Whether `expr` reads a field named `blocks`, `phis`, or `insts`.
fn reads_ir_lists(expr: &Expr) -> bool {
    struct Fields(bool);
    impl<'ast> Visit<'ast> for Fields {
        fn visit_member(&mut self, m: &'ast Member) {
            if let Member::Named(id) = m {
                self.0 |= ["blocks", "phis", "insts"].iter().any(|n| id == n);
            }
        }
    }
    let mut fields = Fields(false);
    fields.visit_expr(expr);
    fields.0
}

/// Whether `expr` sweeps a whole function: reads its `blocks` or calls `rpo` / `rpo_blocks`.
fn sweeps_function(expr: &Expr) -> bool {
    struct Sweep(bool);
    impl<'ast> Visit<'ast> for Sweep {
        fn visit_member(&mut self, m: &'ast Member) {
            self.0 |= matches!(m, Member::Named(id) if id == "blocks");
        }
        fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
            self.0 |= m.method == "rpo" || m.method == "rpo_blocks";
            visit::visit_expr_method_call(self, m);
        }
    }
    let mut sweep = Sweep(false);
    sweep.visit_expr(expr);
    sweep.0
}

/// Whether `expr` is a constant: a literal, or a path to an `UPPER_CASE` constant.
fn is_constant(expr: &Expr) -> bool {
    match expr {
        Expr::Lit(_) => true,
        Expr::Path(p) => p.path.segments.last().is_some_and(|s| {
            let name = s.ident.to_string();
            name.chars().any(|c| c.is_ascii_uppercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        }),
        Expr::Paren(p) => is_constant(&p.expr),
        Expr::Binary(b) => is_constant(&b.left) && is_constant(&b.right),
        _ => false,
    }
}

/// Whether a `for` loop repeats its body a number of times that grows with the input: over a
/// range whose end is not a constant (`0..links`, `1..`), unless the body stops the loop variable
/// at a constant (`if pass > MAX_PASSES`).
fn repeats_unboundedly(f: &syn::ExprForLoop) -> bool {
    let Expr::Range(range) = &*f.expr else {
        return false;
    };
    if range.end.as_deref().is_some_and(is_constant) {
        return false;
    }
    let Pat::Ident(var) = &*f.pat else {
        return true;
    };
    struct Stop<'v>(&'v syn::Ident, bool);
    impl<'ast> Visit<'ast> for Stop<'_> {
        fn visit_expr_binary(&mut self, b: &'ast syn::ExprBinary) {
            let is_var = |e: &Expr| matches!(e, Expr::Path(p) if p.path.is_ident(self.0));
            self.1 |= (is_var(&b.left) && is_constant(&b.right))
                || (is_var(&b.right) && is_constant(&b.left));
            visit::visit_expr_binary(self, b);
        }
    }
    let mut stop = Stop(&var.ident, false);
    stop.visit_block(&f.body);
    !stop.1
}

/// How a loop may stop.
enum Stop<'e> {
    /// `loop`: on a test in its body.
    Body,
    /// `while cond`.
    While(&'e Expr),
    /// `for _ in 0..n`: after its count, or on a test in its body.
    Count,
}

/// Whether a loop repeats a pass until nothing changes: its body iterates over a function's
/// blocks, phis, or instructions or its reverse postorder, or the loop stops on a comparison with
/// the facts `before` the pass, on a size (a `loop` or `while`), or on a flag each pass clears
/// and sets (`changed = false`, then `changed = true` or `changed |= ..`).
fn dense_body(body: &syn::Block, stop: Stop<'_>) -> bool {
    #[derive(Default)]
    struct Dense {
        found: bool,
        size_test: bool,
        /// Names the body sets to `false`.
        cleared: BTreeSet<String>,
        /// Names the body sets to `true` or ors into.
        set: BTreeSet<String>,
        /// Names read by a `while` condition, or by the condition of an `if` that leaves the
        /// loop.
        tested: BTreeSet<String>,
    }
    fn names(expr: &Expr, out: &mut BTreeSet<String>) {
        struct Names<'o>(&'o mut BTreeSet<String>);
        impl<'ast> Visit<'ast> for Names<'_> {
            fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
                if let Some(id) = p.path.get_ident() {
                    self.0.insert(id.to_string());
                }
            }
        }
        Names(out).visit_expr(expr);
    }
    fn leaves(block: &syn::Block) -> bool {
        struct Leaves(bool);
        impl<'ast> Visit<'ast> for Leaves {
            fn visit_expr_break(&mut self, _: &'ast syn::ExprBreak) {
                self.0 = true;
            }
            fn visit_expr_return(&mut self, _: &'ast syn::ExprReturn) {
                self.0 = true;
            }
            fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
        }
        let mut leaves = Leaves(false);
        leaves.visit_block(block);
        leaves.0
    }
    fn bool_literal(expr: &Expr) -> Option<bool> {
        match expr {
            Expr::Lit(l) => match &l.lit {
                syn::Lit::Bool(b) => Some(b.value),
                _ => None,
            },
            _ => None,
        }
    }
    impl Dense {
        fn assigned(&mut self, name: String, value: Option<bool>) {
            match value {
                Some(false) => self.cleared.insert(name),
                Some(true) => self.set.insert(name),
                None => false,
            };
        }
    }
    impl<'ast> Visit<'ast> for Dense {
        fn visit_expr_for_loop(&mut self, f: &'ast syn::ExprForLoop) {
            self.found |= reads_ir_lists(&f.expr) || sweeps_function(&f.expr);
            visit::visit_expr_for_loop(self, f);
        }
        fn visit_expr_binary(&mut self, b: &'ast syn::ExprBinary) {
            let compares = matches!(b.op, syn::BinOp::Eq(_) | syn::BinOp::Ne(_));
            let is_len = |e: &Expr| matches!(e, Expr::MethodCall(m) if m.method == "len");
            let is_before = |e: &Expr| matches!(e, Expr::Path(p) if p.path.is_ident("before"));
            self.size_test |= compares
                && ((is_len(&b.left) && matches!(&*b.right, Expr::Path(_)))
                    || (is_len(&b.right) && matches!(&*b.left, Expr::Path(_))));
            self.found |= compares && (is_before(&b.left) || is_before(&b.right));
            if let (syn::BinOp::BitOrAssign(_), Expr::Path(p)) = (&b.op, &*b.left)
                && let Some(id) = p.path.get_ident()
            {
                self.set.insert(id.to_string());
            }
            visit::visit_expr_binary(self, b);
        }
        fn visit_expr_assign(&mut self, a: &'ast syn::ExprAssign) {
            if let Expr::Path(p) = &*a.left
                && let Some(id) = p.path.get_ident()
            {
                self.assigned(id.to_string(), bool_literal(&a.right));
            }
            visit::visit_expr_assign(self, a);
        }
        fn visit_local(&mut self, l: &'ast syn::Local) {
            let name = match &l.pat {
                Pat::Ident(p) => Some(&p.ident),
                Pat::Type(t) => match &*t.pat {
                    Pat::Ident(p) => Some(&p.ident),
                    _ => None,
                },
                _ => None,
            };
            if let (Some(name), Some(init)) = (name, &l.init) {
                self.assigned(name.to_string(), bool_literal(&init.expr));
            }
            visit::visit_local(self, l);
        }
        fn visit_expr_if(&mut self, i: &'ast syn::ExprIf) {
            if leaves(&i.then_branch) {
                names(&i.cond, &mut self.tested);
            }
            visit::visit_expr_if(self, i);
        }
        fn visit_item_fn(&mut self, _: &'ast syn::ItemFn) {}
    }
    let mut dense = Dense::default();
    if let Stop::While(cond) = stop {
        names(cond, &mut dense.tested);
    }
    dense.visit_block(body);
    let flag = (dense.cleared.intersection(&dense.set)).any(|name| dense.tested.contains(name));
    dense.found || flag || (dense.size_test && !matches!(stop, Stop::Count))
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_ident(&mut self, i: &'ast proc_macro2::Ident) {
        self.idents.insert(i.to_string());
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.nested.push(f.clone());
    }

    fn visit_item_impl(&mut self, _: &'ast syn::ItemImpl) {}

    fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {}

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let Expr::Path(p) = &*c.func {
            let path = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            self.call(Call::Path(path));
        }
        visit::visit_expr_call(self, c);
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        let name = m.method.to_string();
        let on_self = matches!(&*m.receiver, Expr::Path(p) if p.path.is_ident("self"));
        if name == "contains"
            && matches!(&*m.receiver, Expr::Field(f) if matches!(&f.member, Member::Named(id) if id == "body"))
        {
            self.linear.insert("body.contains".to_owned());
        }
        self.call(if on_self {
            Call::OnSelf(name)
        } else {
            Call::Method(name)
        });
        visit::visit_expr_method_call(self, m);
    }

    fn visit_expr_while(&mut self, w: &'ast syn::ExprWhile) {
        match &*w.cond {
            Expr::Let(l) => {
                let some = matches!(&*l.pat, Pat::TupleStruct(t) if t.path.is_ident("Some"));
                let pops =
                    matches!(&*l.expr, Expr::MethodCall(m) if POPS.iter().any(|p| m.method == p));
                self.worklist |= some && pops;
            }
            cond => self.dense |= dense_body(&w.body, Stop::While(cond)),
        }
        visit::visit_expr_while(self, w);
    }

    fn visit_expr_loop(&mut self, l: &'ast syn::ExprLoop) {
        self.dense |= dense_body(&l.body, Stop::Body);
        visit::visit_expr_loop(self, l);
    }

    fn visit_expr_for_loop(&mut self, f: &'ast syn::ExprForLoop) {
        self.dense |= repeats_unboundedly(f) && dense_body(&f.body, Stop::Count);
        visit::visit_expr_for_loop(self, f);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.tokens(m.tokens.clone());
        visit::visit_macro(self, m);
    }
}

fn is_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| match &a.meta {
        syn::Meta::List(list) => {
            list.path.is_ident("cfg")
                && list
                    .tokens
                    .to_string()
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|t| t == "test")
        }
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::NameValue(_) => false,
    })
}

fn type_name(ty: &Type) -> String {
    match ty {
        Type::Path(p) => p
            .path
            .segments
            .last()
            .map_or_else(String::new, |s| s.ident.to_string()),
        Type::Reference(r) => type_name(&r.elem),
        _ => String::new(),
    }
}

struct Collector<'a> {
    module: String,
    out: &'a mut Vec<Function>,
}

impl Collector<'_> {
    fn items(&mut self, items: &[Item]) {
        for item in items {
            match item {
                Item::Fn(f) if !is_test(&f.attrs) => {
                    self.function(&f.sig, &f.block, None, &self.module.clone())
                }
                Item::Impl(i) if !is_test(&i.attrs) => {
                    let owner = type_name(&i.self_ty);
                    for member in &i.items {
                        if let ImplItem::Fn(m) = member
                            && !is_test(&m.attrs)
                        {
                            self.function(&m.sig, &m.block, Some(&owner), &self.module.clone());
                        }
                    }
                }
                Item::Trait(t) if !is_test(&t.attrs) => {
                    let owner = t.ident.to_string();
                    for member in &t.items {
                        if let TraitItem::Fn(m) = member
                            && let Some(block) = &m.default
                        {
                            self.function(&m.sig, block, Some(&owner), &self.module.clone());
                        }
                    }
                }
                Item::Mod(m) if !is_test(&m.attrs) => {
                    if let Some((_, items)) = &m.content {
                        let inner = format!("{}::{}", self.module, m.ident);
                        let outer = std::mem::replace(&mut self.module, inner);
                        self.items(items);
                        self.module = outer;
                    }
                }
                _ => {}
            }
        }
    }

    /// One function, named under `prefix`, and the functions nested in its body.
    fn function(
        &mut self,
        sig: &syn::Signature,
        body: &syn::Block,
        owner: Option<&str>,
        prefix: &str,
    ) {
        let mut scan = Scan::default();
        scan.visit_signature(sig);
        scan.visit_block(body);
        let simple = sig.ident.to_string();
        let name = match owner {
            Some(owner) => format!("{prefix}::{owner}::{simple}"),
            None => format!("{prefix}::{simple}"),
        };
        for nested in std::mem::take(&mut scan.nested) {
            self.function(&nested.sig, &nested.block, None, &name);
        }
        self.out.push(Function {
            name,
            module: self.module.clone(),
            owner: owner.map(str::to_owned),
            simple,
            scan,
        });
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The analysis sources of a crate with their module paths, without its tests and without
/// `heki_ir::walk`.
fn sources(krate: &str) -> Vec<(String, PathBuf)> {
    let src = repo_root().join("crates").join(krate).join("src");
    let mut out = Vec::new();
    let mut dirs = vec![src.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(&src).unwrap();
            let parts: Vec<String> = rel
                .with_extension("")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let first = parts.first().map(String::as_str);
            if parts.iter().any(|p| p == "tests") || (krate == "heki_ir" && first == Some("walk")) {
                continue;
            }
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let module = std::iter::once(krate.to_owned())
                    .chain(parts.into_iter().filter(|p| p != "lib" && p != "mod"))
                    .collect::<Vec<_>>()
                    .join("::");
                out.push((module, path));
            }
        }
    }
    out.sort();
    out
}

fn functions(krate: &str) -> Vec<Function> {
    let mut out = Vec::new();
    for (module, path) in sources(krate) {
        let text = std::fs::read_to_string(&path).unwrap();
        let file = syn::parse_file(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        Collector {
            module,
            out: &mut out,
        }
        .items(&file.items);
    }
    out
}

/// The functions of one crate a call may reach, by index.
fn callees(fns: &[Function], caller: &Function, call: &Call) -> Vec<usize> {
    let pick = |keep: &dyn Fn(&Function) -> bool| -> Vec<usize> {
        (0..fns.len()).filter(|&i| keep(&fns[i])).collect()
    };
    let methods_named = |name: &str| pick(&|f| f.owner.is_some() && f.simple == name);
    match call {
        Call::OnSelf(name) => {
            let own = pick(&|f| f.owner == caller.owner && f.simple == *name);
            if own.is_empty() {
                methods_named(name)
            } else {
                own
            }
        }
        Call::Method(name) if STD_METHODS.contains(&name.as_str()) => Vec::new(),
        Call::Method(name) => methods_named(name),
        Call::Path(path) => {
            let name = path.last().expect("a path has a segment");
            let free = |module: Option<&str>| {
                pick(&|f| {
                    f.owner.is_none()
                        && f.simple == *name
                        && module.is_none_or(|m| f.module.ends_with(m))
                })
            };
            match path.len() {
                1 => {
                    let here = free(Some(&caller.module));
                    if here.is_empty() { free(None) } else { here }
                }
                _ => {
                    let qualifier = &path[path.len() - 2];
                    if CRATES.contains(&path[0].as_str()) && !caller.module.starts_with(&path[0]) {
                        Vec::new()
                    } else if qualifier == "Self" {
                        pick(&|f| f.owner == caller.owner && f.simple == *name)
                    } else if qualifier.starts_with(char::is_uppercase) {
                        pick(&|f| {
                            f.owner.as_deref() == Some(qualifier.as_str()) && f.simple == *name
                        })
                    } else if ["crate", "super", "self"].contains(&qualifier.as_str()) {
                        free(None)
                    } else {
                        free(Some(qualifier))
                    }
                }
            }
        }
    }
}

/// The component of each function in the call graph (Tarjan's), and whether it is a cycle:
/// more than one member, or an edge to itself.
fn components(edges: &[Vec<usize>]) -> (Vec<usize>, Vec<bool>) {
    let n = edges.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut component = vec![usize::MAX; n];
    let mut cyclic = vec![false; n];
    let mut next = 0;
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        let mut frames = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut at)) = frames.last_mut() {
            if let Some(&w) = edges[v].get(*at) {
                *at += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let at = stack.iter().rposition(|&x| x == v).unwrap();
                let members = stack.split_off(at);
                let cycle = members.len() > 1 || edges[v].contains(&v);
                for m in members {
                    on_stack[m] = false;
                    component[m] = v;
                    cyclic[m] = cycle;
                }
            }
        }
    }
    (component, cyclic)
}

/// The codes each function of the analysis crates is found with, each with what found it.
fn findings() -> BTreeMap<String, BTreeMap<&'static str, String>> {
    let mut out: BTreeMap<String, BTreeMap<&'static str, String>> = BTreeMap::new();
    for krate in CRATES {
        let fns = functions(krate);
        let edges: Vec<Vec<usize>> = fns
            .iter()
            .map(|f| {
                let mut to: Vec<usize> = f
                    .scan
                    .calls
                    .iter()
                    .flat_map(|c| callees(&fns, f, c))
                    .collect();
                to.sort_unstable();
                to.dedup();
                to
            })
            .collect();
        let (component, cyclic) = components(&edges);
        let short = |f: &Function| f.name[f.module.len() + 2..].to_owned();
        for (i, f) in fns.iter().enumerate() {
            let touches = IR_IDS.iter().any(|id| f.scan.idents.contains(*id));
            let mut codes = BTreeMap::new();
            if cyclic[i] && touches {
                let others: Vec<String> = (0..fns.len())
                    .filter(|&j| j != i && component[j] == component[i])
                    .map(|j| short(&fns[j]))
                    .collect();
                let through = match others.len() {
                    0 => "calls itself".to_owned(),
                    1..=3 => format!("through {}", others.join(", ")),
                    n => format!("through {} and {} more", others[..3].join(", "), n - 3),
                };
                codes.insert("W1", through);
            }
            if f.scan.worklist && touches {
                codes.insert("W2", "a worklist".to_owned());
            }
            if f.scan.dense {
                codes.insert("W3", "passes until nothing changes".to_owned());
            }
            if !f.scan.linear.is_empty() {
                let names: Vec<&str> = f.scan.linear.iter().map(String::as_str).collect();
                codes.insert("W4", names.join(", "));
            }
            if !codes.is_empty() {
                out.entry(f.name.clone()).or_default().extend(codes);
            }
        }
    }
    out
}

/// `walks.allow`: function, codes, and the reason, one function per line.
fn allowed() -> BTreeMap<String, BTreeSet<String>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/walks.allow");
    let text = std::fs::read_to_string(path).unwrap();
    let mut out = BTreeMap::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(codes), Some(_reason)) = (parts.next(), parts.next(), parts.next())
        else {
            panic!("`walks.allow`: `{line}` is not `function codes reason`");
        };
        let codes = codes.split(',').map(str::to_owned).collect();
        assert!(
            out.insert(name.to_owned(), codes).is_none(),
            "`walks.allow` lists `{name}` twice"
        );
    }
    out
}

#[test]
fn hand_written_walks_are_only_those_allowed() {
    let found = findings();
    let allowed = allowed();
    let mut unlisted = Vec::new();
    for (name, codes) in &found {
        let listed = allowed.get(name);
        let missing: Vec<String> = codes
            .iter()
            .filter(|(c, _)| listed.is_none_or(|l| !l.contains(**c)))
            .map(|(c, why)| format!("{c} ({why})"))
            .collect();
        if !missing.is_empty() {
            unlisted.push(format!("{name}  {}", missing.join("; ")));
        }
    }
    let mut stale = Vec::new();
    for (name, codes) in &allowed {
        let hits = found.get(name);
        let gone: Vec<&str> = codes
            .iter()
            .map(String::as_str)
            .filter(|c| hits.is_none_or(|h| !h.contains_key(c)))
            .collect();
        if !gone.is_empty() {
            stale.push(format!("{name}  {}", gone.join(",")));
        }
    }
    let mut problems = Vec::new();
    if !unlisted.is_empty() {
        problems.push(format!(
            "walks written by hand: move them onto `heki_ir::walk` (docs/development.md, \
             \"Analysis cost\"):\n{}",
            unlisted.join("\n")
        ));
    }
    if !stale.is_empty() {
        problems.push(format!(
            "`crates/heki/tests/walks.allow` allows what is no longer found: remove it:\n{}",
            stale.join("\n")
        ));
    }
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}
