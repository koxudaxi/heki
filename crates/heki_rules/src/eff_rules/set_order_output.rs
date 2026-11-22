//! `EFF112` (`set-order-output`, opt-in): the order of a set whose elements' hashes change
//! between runs (`str`, `bytes`, tuples of them) reaches an output in that order, across a call:
//! a function returns a value it builds in a set's order and its caller writes it, or a
//! function hands such a value to a call that writes it. A flow inside one function is a
//! linter's (one expression and its type decide it) and is not reported.
//!
//! The facts are `heki_effect::order_flow`'s: the report is decided from the summaries of the
//! function and of its callees, at the function where the ordered value meets the output.

use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::order_flow::{OrderEnd, OrderOrigin, OrderSource, Resolver};
use heki_effect::{FuncKey, SharedChain};

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("EFF112", unit) {
        return;
    }
    let summary_of = |k: &FuncKey| cx.index.summary(k);
    let resolver = Resolver::new(&summary_of);
    let path = &unit.input.path;
    for f in &unit.input.ir.functions {
        let key = CheckContext::key(unit, f);
        let Some(facts) = cx.index.summary(&key).and_then(|s| s.set_order.as_deref()) else {
            continue;
        };
        let carried = resolver.carried(&key, path, facts);
        for (flow, carries) in facts.flows.iter().zip(&carried) {
            // A value that came back from a call crosses a boundary wherever it is written;
            // one taken here crosses only into a call that writes it. A parameter's order is
            // its callers'.
            let (at, returned) = match &flow.source {
                OrderSource::Take(take) => (take.span, false),
                OrderSource::Result { span, .. } => (*span, true),
                OrderSource::Param(_) => continue,
            };
            let Some(origin) = &carries.origin else {
                continue;
            };
            let chain = flow
                .ends
                .iter()
                .filter(|end| returned || matches!(end, OrderEnd::Arg { .. }))
                .filter_map(|end| resolver.output_chain(&key, path, end))
                .min_by_key(Vec::len);
            if let Some(chain) = chain {
                report(cx, unit, &key, at, origin, &chain, out);
            }
        }
    }
}

fn report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    key: &FuncKey,
    at: Span,
    origin: &OrderOrigin,
    chain: &SharedChain,
    out: &mut Vec<Diagnostic>,
) {
    let output = chain
        .last()
        .expect("an output chain ends at the output call");
    let origin_path = cx
        .index
        .file_of(&origin.function)
        .unwrap_or(&unit.input.path)
        .to_path_buf();
    let origin_line = cx
        .related_at(&origin_path, origin.take.span, String::new())
        .line;
    let output_line = cx.related_at(&output.path, output.span, String::new()).line;
    let written = match output.callee.strip_prefix("builtins.") {
        Some(class) if class.ends_with("Error") || class.ends_with("Exception") => {
            format!("the message of a raised `{class}`")
        }
        _ => format!("`{}`", output.callee),
    };
    let through = match chain.as_slice() {
        [first, _, ..] => format!(" through `{}`", first.callee),
        _ => String::new(),
    };
    let d = cx
        .emit(
            out,
            "EFF112",
            unit,
            at,
            &key.qualname,
            format!(
                "{}:{}->{}",
                origin.function.qualname, origin_line, output.callee
            ),
            format!(
                "the order of a set of strings, taken by {} at line {origin_line} in `{}`, reaches {written} (line {output_line}){through}: it changes between runs, as string hashes are randomized per process",
                origin.take.how.describe(),
                origin.function.qualname,
            ),
            Some(
                "sort the elements before they reach the output (`sorted(...)`), or have the writer sort them (`sort_keys=True`)".to_owned(),
            ),
            &[],
        )
        .expect("`run` checked that EFF112 runs on the unit");
    let mut related = vec![cx.related_at(
        &origin_path,
        origin.take.span,
        format!(
            "`{}` takes the order of a set here",
            origin.function.qualname
        ),
    )];
    related.extend(origin.via.iter().map(|hop| {
        let message = format!(
            "`{}` calls `{}`, which returns it",
            hop.caller.qualname, hop.callee
        );
        cx.related_at(&hop.path, hop.span, message)
    }));
    let (last, calls) = chain.split_last().expect("checked above");
    related.extend(calls.iter().map(|hop| {
        let message = format!("`{}` calls `{}`", hop.caller.qualname, hop.callee);
        cx.related_at(&hop.path, hop.span, message)
    }));
    related.push(cx.related_at(
        &last.path,
        last.span,
        format!(
            "`{}` writes it with `{}`",
            last.caller.qualname, last.callee
        ),
    ));
    d.related = related;
}
