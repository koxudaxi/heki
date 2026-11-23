//! `EFF116` (`generator-never-iterated`, opt-in): a call of a project generator function whose
//! generator nothing iterates. Calling a generator function only creates the generator; its body
//! runs where the generator is iterated, so here it never runs:
//!
//! - the call is a statement, or only truth tests read its value (`if gen():`, `while gen():`,
//!   `assert gen()`, `not gen()`, `bool(gen())`), which a generator always passes;
//! - a library call that drops what its callable returns runs the function
//!   (`threading.Thread(target=gen)`, `atexit.register(gen)`), or tests it for truth
//!   (`filter(gen, xs)`).
//!
//! Whether the value is used is decided from the calling function's IR alone ([`unused_calls`]):
//! the call's result is unnamed and read by nothing but truth tests, so no flow analysis is
//! needed. The callees and whether they are every implementation the call can run come from the
//! resolver, and whether each is a generator function from its declaration
//! ([`heki_effect::FunctionDecl::is_generator`]), so a change of a function in another module
//! between a generator and a plain function re-checks its callers' files. The hand-off form reads
//! the facts `EFF115` reads (`OptInTables::unawaited_handoffs`).

use crate::mut_rules::CallInst;
use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::{Certainty, Diagnostic};
use heki_effect::{Callee, FuncKey, TypeCtx, UnawaitedHandoff, Val};
use heki_ir::walk::{FnShape, Use};
use heki_ir::{Inst, InstKind, Terminator, UnaryOpKind, VarId};

const CODE: &str = "EFF116";

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled(CODE, unit) {
        return;
    }
    calls(cx, unit, out);
    handoffs(cx, unit, out);
}

/// What reads the generator a call creates.
enum Read {
    /// Nothing: the call is a statement.
    Nothing,
    /// Truth tests only.
    TruthTests,
}

/// A call whose value is unnamed and read by nothing but truth tests.
struct UnusedCall<'f> {
    inst: &'f Inst,
    parts: CallInst<'f>,
    read: Read,
    /// The `bool(...)` calls among the readers, whose callee the resolver confirms.
    bool_calls: Vec<VarId>,
}

/// The calls of the generator functions in the functions of `unit` whose generator nothing
/// iterates.
fn calls(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    for f in &unit.input.ir.functions {
        let shape = FnShape::new(f);
        let unused = unused_calls(&shape);
        if unused.is_empty() {
            continue;
        }
        let types = tcx.infer(f);
        let qualname = CheckContext::key(unit, f).qualname;
        for call in unused {
            let read = |v: VarId| types.get(&v);
            if !call
                .bool_calls
                .iter()
                .all(|v| is_builtin_bool(&tcx, read(*v)))
            {
                continue;
            }
            let Callee::Project(keys, _) = call.parts.callee(&tcx, &types) else {
                continue;
            };
            let complete = tcx.inst_targets_complete(&call.inst.kind, read);
            let Some(generator) = Generators::of(cx, &keys, complete) else {
                continue;
            };
            let name = generator.name;
            let what = generator.what();
            let kind = generator.kind();
            let (message, help) = match call.read {
                Read::Nothing => (
                    format!(
                        "the call of `{name}` is a statement, and {what}: calling it only creates {kind}, so the body does not run"
                    ),
                    "iterate the generator (`for _ in ...`, `list(...)`), or make the function a plain `def` if its body is meant to run at the call",
                ),
                Read::TruthTests => (
                    format!(
                        "only a truth test reads what `{name}` returns, and {what}: {kind} is always true, and the body does not run"
                    ),
                    "iterate the generator, or test what it yields (`any(...)`, `next(..., None)`)",
                ),
            };
            emit(
                cx,
                unit,
                (call.inst.span, &qualname, name.to_owned()),
                message,
                help,
                generator.exact,
                out,
            );
        }
    }
}

/// The calls of `shape`'s function whose value is unnamed and that nothing reads but truth
/// tests: the condition of a branch (`if`, `while`, `assert`, a conditional expression), `not`,
/// or a one-argument call of a name `bool` (confirmed later, [`UnusedCall::bool_calls`]). An
/// awaited call is left to the type checkers: awaiting a generator raises `TypeError`.
fn unused_calls<'f>(shape: &FnShape<'f>) -> Vec<UnusedCall<'f>> {
    let f = shape.f;
    let mut out = Vec::new();
    for b in &f.blocks {
        for inst in &b.insts {
            let (InstKind::Call {
                dst,
                is_awaited: false,
                ..
            }
            | InstKind::MethodCall {
                dst,
                is_awaited: false,
                ..
            }) = &inst.kind
            else {
                continue;
            };
            let readers = shape.readers(*dst);
            let mut bool_calls = Vec::new();
            // A value bound to a name is left out, read or not.
            let unused = f.var_name(*dst).is_none()
                && readers.iter().all(|u| match truth_test(shape, *u, *dst) {
                    Some(TruthTest::Bool(callee)) => {
                        bool_calls.push(callee);
                        true
                    }
                    Some(TruthTest::Plain) => true,
                    None => false,
                });
            if !unused {
                continue;
            }
            out.push(UnusedCall {
                inst,
                parts: CallInst::of(&inst.kind).expect("a call instruction"),
                read: if readers.is_empty() {
                    Read::Nothing
                } else {
                    Read::TruthTests
                },
                bool_calls,
            });
        }
    }
    out
}

/// How a use tests a value for truth.
enum TruthTest {
    /// A branch on it, or `not` of it.
    Plain,
    /// A call `bool(v)`, by the variable holding the callee.
    Bool(VarId),
}

/// How the use `u` of `v` tests it for truth, if it does nothing else with it.
fn truth_test(shape: &FnShape<'_>, u: Use, v: VarId) -> Option<TruthTest> {
    let f = shape.f;
    match u {
        Use::Terminator(b) => {
            matches!(f.block(b).terminator, Terminator::Branch { cond, .. } if cond == v)
                .then_some(TruthTest::Plain)
        }
        Use::Inst(b, i) => match &f.block(b).insts[i as usize].kind {
            InstKind::UnaryOp {
                op: UnaryOpKind::Not,
                operand,
                ..
            } if *operand == v => Some(TruthTest::Plain),
            InstKind::Call {
                callee,
                callee_var: Some(callee_var),
                args,
                kwargs,
                ..
            } if callee == "bool" && args.as_slice() == [v] && kwargs.is_empty() => {
                Some(TruthTest::Bool(*callee_var))
            }
            _ => None,
        },
        Use::Phi(..) => None,
    }
}

/// Whether calling `callee` calls the builtin `bool`.
fn is_builtin_bool(tcx: &TypeCtx<'_>, callee: Option<&Val>) -> bool {
    matches!(tcx.resolve_call(callee), Callee::External(symbol) if symbol == "builtins.bool")
}

/// The functions handed to a library call that drops what they return or tests it for truth,
/// where one of them is a generator function.
fn handoffs(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    debug_assert!(
        cx.index.opt_in_facts().unawaited_handoffs,
        "`opt_in_facts` decides for EFF116"
    );
    let file = cx.index.files.get(&unit.input.path);
    for (func, handoffs) in file.into_iter().flat_map(|f| f.unawaited_handoffs()) {
        for handoff in handoffs.iter().filter(|h| !h.hands_back) {
            handoff_report(cx, unit, func, handoff, out);
        }
    }
}

fn handoff_report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    func: &FuncKey,
    handoff: &UnawaitedHandoff,
    out: &mut Vec<Diagnostic>,
) {
    let Some(generator) = Generators::of(cx, &handoff.callees, handoff.complete) else {
        return;
    };
    // A builtin is named without its module.
    let api = handoff
        .api
        .strip_prefix("builtins.")
        .unwrap_or(&handoff.api);
    let name = generator.name;
    let what = generator.what();
    let kind = generator.kind();
    let (message, help) = if handoff.tests_result {
        (
            format!(
                "`{api}` tests what `{name}` returns for truth, and {what}: {kind} is always true, and the body does not run"
            ),
            "pass a plain function that returns the value to test",
        )
    } else {
        (
            format!(
                "`{api}` drops what `{name}` returns, and {what}: calling it only creates {kind}, so the body does not run"
            ),
            "pass a plain function that iterates the generator, or make the function a plain `def`",
        )
    };
    emit(
        cx,
        unit,
        (handoff.span, &func.qualname, format!("{api}->{name}")),
        message,
        help,
        generator.exact,
        out,
    );
}

/// The functions a call runs, when every one is a generator function. A call that may run a
/// plain function instead is no report: what the call does with the result may be what that
/// function is for.
struct Generators<'k> {
    /// The first of them, by qualname.
    name: &'k str,
    /// How many others the call may run.
    others: usize,
    /// Every one is an `async def`: an async generator function.
    is_async: bool,
    /// The name of one of them is bound to more than one definition (`def` under `if` and
    /// `else`): the call runs one of them, and the declaration is the last one's.
    bound_twice: bool,
    /// The report's premises are established: every function the call runs is a generator
    /// function that no decorator replaces and whose name is bound once, and they are every
    /// implementation the call can run.
    exact: bool,
}

impl<'k> Generators<'k> {
    /// `None` unless every function of `keys` is a generator function. `complete`: `keys` are
    /// every implementation the call can run.
    fn of(cx: &CheckContext<'_>, keys: &'k [FuncKey], complete: bool) -> Option<Self> {
        let decls: Vec<_> = keys
            .iter()
            .map(|k| cx.decls.function_decl(k).filter(|d| d.is_generator))
            .collect::<Option<_>>()?;
        let first = keys.first()?;
        let summaries: Vec<_> = keys.iter().map(|k| cx.index.summary(k)).collect();
        let bound_twice = summaries
            .iter()
            .flatten()
            .any(|s| s.must_exec.as_deref().is_some_and(|m| m.bound_twice));
        Some(Self {
            name: &first.qualname,
            others: keys.len() - 1,
            is_async: decls.iter().all(|d| d.is_async),
            bound_twice,
            exact: complete && !bound_twice && summaries.iter().all(Option::is_some),
        })
    }

    /// What the report says the functions are.
    fn what(&self) -> String {
        let name = self.name;
        let what = if self.others == 0 {
            format!("`{name}` is {} function", self.kind())
        } else {
            format!(
                "`{name}` and the other implementations the call may run are {} functions",
                self.noun()
            )
        };
        match (self.bound_twice, self.others) {
            (false, _) => what,
            (true, 0) => format!(
                "{what}, and its name is bound to more than one definition, of which heki reads the last"
            ),
            (true, _) => format!(
                "{what}, and the name of one is bound to more than one definition, of which heki reads the last"
            ),
        }
    }

    /// `a generator` or `an async generator`: what calling the functions creates.
    fn kind(&self) -> &'static str {
        if self.is_async {
            "an async generator"
        } else {
            "a generator"
        }
    }

    /// [`Self::kind`] without its article.
    fn noun(&self) -> &'static str {
        if self.is_async {
            "async generator"
        } else {
            "generator"
        }
    }
}

fn emit(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    (span, qualname, target): (heki_ast::Span, &str, String),
    message: String,
    help: &str,
    exact: bool,
    out: &mut Vec<Diagnostic>,
) {
    let d = cx
        .emit(
            out,
            CODE,
            unit,
            span,
            qualname,
            target,
            message,
            Some(help.to_owned()),
            &[],
        )
        .expect("the rule runs on the unit");
    let tier = if exact {
        Certainty::Exact
    } else {
        Certainty::Heuristic
    };
    d.certainty = tier.at_most(d.certainty);
}
