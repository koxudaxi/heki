//! `heki dump-facts` (hidden): the path and loop facts of `heki_ir::facts` for every function of
//! one module, as JSON, for the tests that check them through the binary.
//!
//! The reader is the one every rule has without resolving calls: the module's bindings, module
//! members named in the catalog, no attribute of any other object, and every `with` statement's
//! `__exit__` as one that may suppress.

use heki_ast::{LineIndex, Span};
use heki_ir::walk::{Answer, Budget};
use heki_ir::{
    Carries, FnIndex, FunctionId, InstKind, ItemOrigins, ItemSource, ModuleBindings, ModuleIr,
    Must, PathReader, Paths, Progress,
};
use heki_kb::EffectiveEcosystemKb;
use serde::Serialize;

#[derive(Serialize)]
pub struct ModuleFacts {
    functions: Vec<FunctionFacts>,
}

#[derive(Serialize)]
struct FunctionFacts {
    qualname: String,
    line: u32,
    calls: Vec<CallFacts>,
    loops: Vec<LoopFacts>,
}

#[derive(Serialize)]
struct CallFacts {
    line: u32,
    column: u32,
    text: String,
    on_every_return: Steps,
    /// One entry per loop around the call, innermost last.
    on_every_iteration: Vec<IterationFacts>,
}

#[derive(Serialize)]
struct Steps {
    called: Answer<Must>,
    returned: Answer<Must>,
}

#[derive(Serialize)]
struct IterationFacts {
    loop_line: u32,
    called: Answer<Must>,
    returned: Answer<Must>,
}

#[derive(Serialize)]
struct LoopFacts {
    line: u32,
    fixed_trips: Answer<u32>,
    origin: Answer<Vec<OriginFacts>>,
}

/// An item origin, a call source by its text.
#[derive(Serialize)]
struct OriginFacts {
    source: String,
    carries: Carries,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    text: bool,
}

pub fn module(ir: &ModuleIr, src: &str, kb: &EffectiveEcosystemKb) -> ModuleFacts {
    let lines = LineIndex::from_source(src);
    let bindings = ModuleBindings::of(ir);
    let member = |module: &str, attr: &str| kb.lookup(&format!("{module}.{attr}")).is_some();
    let reader = PathReader {
        module_member: &member,
        ..PathReader::with_bindings(&bindings)
    };
    let origins = ItemOrigins::new(ir);
    let functions = ir
        .functions
        .iter()
        .filter(|f| !f.is_module_body && !f.is_class_body)
        .map(|f| {
            let ix = FnIndex::new(f, &ir.constants);
            let budget = Budget::of_function("dump-facts", f);
            let paths = Paths::new(&ix, &reader, &budget);
            let loop_line = |li: usize| {
                let lp = &f.loops[li];
                let header = f.block(lp.header);
                let span = header
                    .insts
                    .first()
                    .map(|i| i.span)
                    .or(header.term_span)
                    .unwrap_or(f.span);
                lines.line(span.start)
            };
            let mut calls = Vec::new();
            for bb in &f.blocks {
                for (i, inst) in bb.insts.iter().enumerate() {
                    if !matches!(
                        inst.kind,
                        InstKind::Call { .. } | InstKind::MethodCall { .. }
                    ) {
                        continue;
                    }
                    let at = (bb.id, i);
                    let (line, column) = lines.line_col(inst.span.start);
                    let mut around: Vec<usize> = (0..f.loops.len())
                        .filter(|li| f.loops[*li].body.contains(&bb.id))
                        .collect();
                    around.sort_by_key(|li| f.loops[*li].depth);
                    calls.push(CallFacts {
                        line,
                        column,
                        text: text(src, inst.span),
                        on_every_return: Steps {
                            called: paths.on_every_return(at, Progress::Called),
                            returned: paths.on_every_return(at, Progress::Returned),
                        },
                        on_every_iteration: around
                            .into_iter()
                            .map(|li| IterationFacts {
                                loop_line: loop_line(li),
                                called: paths.on_every_iteration(li, at, Progress::Called),
                                returned: paths.on_every_iteration(li, at, Progress::Returned),
                            })
                            .collect(),
                    });
                }
            }
            let loops = f
                .loops
                .iter()
                .enumerate()
                .map(|(li, lp)| LoopFacts {
                    line: loop_line(li),
                    fixed_trips: ix.fixed_trips(lp, &mut |_| None),
                    origin: origins.of_loop(&budget, f.id, li).map(|set| {
                        set.into_iter()
                            .map(|o| OriginFacts {
                                source: source_text(ir, src, f.id, o.function, o.source),
                                carries: o.carries,
                                text: o.text,
                            })
                            .collect()
                    }),
                })
                .collect();
            FunctionFacts {
                qualname: f.qualname.clone(),
                line: lines.line(f.span.start),
                calls,
                loops,
            }
        })
        .collect();
    ModuleFacts { functions }
}

/// The source text of `span` on one line, cut at 60 characters.
fn text(src: &str, span: Span) -> String {
    let raw = src
        .get(span.start as usize..span.end as usize)
        .unwrap_or_default();
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(60).collect()
}

/// A source as the tests read it: a call by its text, and the function it belongs to when that
/// is not the loop's own (a generator expression reads its creator's).
fn source_text(
    ir: &ModuleIr,
    src: &str,
    loop_function: FunctionId,
    function: FunctionId,
    source: ItemSource,
) -> String {
    let owner = ir.function(function);
    let text = match source {
        ItemSource::Param(p) => format!("param {p}"),
        ItemSource::ParamPart(p) => format!("part of param {p}"),
        ItemSource::Receiver => "receiver".to_owned(),
        ItemSource::Fixed => "fixed".to_owned(),
        ItemSource::Call { block, index } => {
            let span = owner.block(block).insts[index as usize].span;
            format!("call {}", text(src, span))
        }
    };
    if function == loop_function {
        text
    } else {
        format!("{text} of {}", owner.qualname)
    }
}
