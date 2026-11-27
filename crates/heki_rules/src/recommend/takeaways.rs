//! The takeaways the text of `heki recommend` starts with: at most one line of each kind, each
//! repeating what a part further down says, with the command or the setting that acts on it
//! (docs/recommend.md, "The takeaways"). They are chosen from the report's own fields, so they add
//! no claim; a kind with nothing behind it has no line.

use super::blind_spots;
use super::contracts::{Confidence, ContractAdvice, ContractKind, MAX_BLOCKS_SHOWN};
use super::rules;
use super::{ISSUES_URL, RecommendReport, TextOptions};
use crate::Consequence;
use std::fmt::Write as _;

/// The reasons an annotation removes: the parameter has none, or it says `Any` or `object`.
const ANNOTATION_REASONS: [&str; 2] = ["unannotated-parameter", "any-value"];

/// The takeaways, in the order the text prints them, unnumbered.
pub(super) fn lines(report: &RecommendReport, text: TextOptions) -> Vec<String> {
    // A source root renames modules: the other counts, and the contract candidates, come from a
    // run with it.
    let (first, last) = match blind_spot(report) {
        Some(b) if b.source_root => (Some(b.line), None),
        b => (None, b.map(|b| b.line)),
    };
    (first.into_iter())
        .chain(safety_win(report))
        .chain(wall(&report.contracts, text.ask_fail))
        .chain(last)
        .collect()
}

/// The rule the block enables with the most exact reports here, with the command showing them;
/// without one, the exact report of a bug shown now that names the fewest other places.
fn safety_win(report: &RecommendReport) -> Option<String> {
    let best = (report.rules.iter().enumerate())
        .filter(|(_, r)| r.in_block && r.exact > 0 && r.consequence == Consequence::Bug)
        .min_by_key(|(i, r)| (std::cmp::Reverse(r.exact), *i))
        .map(|(_, r)| r);
    if let Some(rule) = best {
        let regressions = match &rule.baseline {
            Some(split) => format!(", {} regression(s) among its reports", split.regressions),
            None => String::new(),
        };
        return Some(format!(
            "Quickest safety win: {} adds {} exact report(s) of a bug here{regressions}: {} \
             (Rules worth enabling here)",
            rule.code,
            rule.exact,
            rules::check_command(&report.analyzed, &[&rule.code])
        ));
    }
    let shown = report.reports_now.easiest_exact.as_ref()?;
    // A report at module level is in no function.
    let function = (Some(shown.function.as_str()).filter(|f| !f.is_empty()))
        .map(|f| format!(" ({f})"))
        .unwrap_or_default();
    let which = match shown.among {
        1 => format!(
            "the one exact report of a bug shown now, through {} place(s)",
            shown.places
        ),
        n => format!(
            "of the {n} exact reports of a bug shown now the one through the fewest places ({})",
            shown.places
        ),
    };
    Some(format!(
        "Quickest safety win: fix {} at {}:{}{function}, {which}: {} (Reports now)",
        shown.code,
        shown.path,
        shown.line,
        rules::check_command(&report.analyzed, &[])
    ))
}

/// The block that, failing the run, guards the most code an edit can cross it from: of the
/// blocks the report shows by default, a high one before a medium one, then the most other
/// modules reading its targets, using their classes, or calling into its scopes, then the fewest
/// targets.
fn wall(contracts: &[ContractAdvice], ask_fail: bool) -> Option<String> {
    let mut shown_by_default = Vec::new();
    for kind in [
        ContractKind::WriteOwners,
        ContractKind::ReadOwners,
        ContractKind::EffectBoundaries,
    ] {
        let of_kind = contracts.iter().filter(|c| c.kind == kind);
        shown_by_default.extend(of_kind.take(MAX_BLOCKS_SHOWN));
    }
    let block = (shown_by_default.into_iter().enumerate())
        .min_by_key(|(i, c)| {
            (
                c.confidence != Confidence::High,
                std::cmp::Reverse(c.evidence.audience.count),
                c.targets.len(),
                *i,
            )
        })
        .map(|(_, c)| c)?;
    let target = match block.targets.len() {
        1 => block.targets[0].clone(),
        n => format!("{} and {} more", block.targets[0], n - 1),
    };
    // A class pattern names every attribute of the class.
    let them = if block.targets.len() > 1 || target.ends_with('*') {
        "them"
    } else {
        "it"
    };
    let audience = block.evidence.audience.count;
    let (crossing, failure) = match block.kind {
        ContractKind::WriteOwners => (
            format!("{audience} other module(s) read {target}, where an edit could write {them}"),
            "such a write",
        ),
        ContractKind::ReadOwners => (
            format!(
                "{audience} other module(s) use the class of {target}, where an edit could read \
                 {them}"
            ),
            "such a read",
        ),
        ContractKind::EffectBoundaries => {
            let mut scopes = format!(
                "{} function(s) of {target} reach no blocking call heki sees",
                block.evidence.functions.count
            );
            if audience > 0 {
                let _ = write!(scopes, ", called from {audience} other module(s)");
            }
            (scopes, "an edit that makes one reach a blocking call")
        }
    };
    let name = (block.entry.get("name").and_then(|v| v.as_str())).unwrap_or_default();
    let confidence = match block.confidence {
        Confidence::High => "high",
        Confidence::Medium => "medium",
    };
    let fail = if ask_fail {
        "with fail = true, heki check fails on"
    } else {
        "at fail-on = \"warning\", heki check fails on"
    };
    Some(format!(
        "Smallest wall for a coding agent: the {} table \"{name}\" ({confidence}): {crossing}; \
         {fail} {failure} (Contract candidates; the table is in the keys at the end)",
        block.kind.key()
    ))
}

/// The line on the largest part of "What heki cannot see" that a setting, a catalog row, or an
/// annotation removes, and whether it is a source root the block adds.
struct BlindSpot {
    line: String,
    source_root: bool,
}

/// A source root the block adds comes before the other parts; of the others, the one with the
/// most calls, in the order the section lists them on a tie.
fn blind_spot(report: &RecommendReport) -> Option<BlindSpot> {
    let section = &report.blind_spots;
    if let Some(line) = source_root(report) {
        return Some(BlindSpot {
            line,
            source_root: true,
        });
    }
    let t = &section.totals;
    let ecosystem = match report.table_prefix.as_str() {
        "" => "[[ecosystem]]".to_owned(),
        prefix => format!("[[{prefix}.ecosystem]]"),
    };
    let mut parts: Vec<(usize, String)> = Vec::new();
    if let Some(name) = blind_spots::first_uncatalogued(section) {
        parts.push((
            t.uncatalogued.calls,
            format!(
                "{} call(s) of {} library name(s) without a catalog entry, which heki takes to \
                 have no effect, such as {name}; an {ecosystem} entry for a name whose effects \
                 you know removes it",
                t.uncatalogued.calls, t.uncatalogued_symbols
            ),
        ));
    }
    if let Some(name) = blind_spots::first_catalog_may_miss(section) {
        let calls = section
            .catalog_may_miss
            .iter()
            .map(|s| s.counts.calls)
            .sum();
        parts.push((
            calls,
            format!(
                "{calls} call(s) of {} name(s) heki's catalog lacks in packages it covers, such \
                 as {name}; if they wait or send a query, report them at {ISSUES_URL} for a \
                 catalog entry",
                section.catalog_may_miss.len()
            ),
        ));
    }
    let annotated: usize = ANNOTATION_REASONS
        .iter()
        .filter_map(|r| t.unresolved_by_reason.get(*r))
        .sum();
    if annotated > 0 {
        parts.push((
            annotated,
            format!(
                "{annotated} call(s) heki does not resolve through a parameter without \
                 annotation or a value annotated `Any` or `object`; an annotation naming the \
                 class lets heki follow them"
            ),
        ));
    }
    let (_, part) = (parts.into_iter().enumerate())
        .filter(|(_, (calls, _))| *calls > 0)
        .min_by_key(|(i, (calls, _))| (std::cmp::Reverse(*calls), *i))
        .map(|(_, part)| part)?;
    Some(BlindSpot {
        line: format!("Biggest blind spot: {part} (What heki cannot see)"),
        source_root: false,
    })
}

/// The source roots the block adds, and the `src` key that adds them.
fn source_root(report: &RecommendReport) -> Option<String> {
    let src = (report.config_toml.lines()).find(|l| l.starts_with("src = "))?;
    let roots: Vec<&blind_spots::SourceRoot> = (report.blind_spots.source_roots.iter())
        .filter(|r| r.in_block)
        .collect();
    let imports: usize = roots.iter().map(|r| r.imports).sum();
    let directories: Vec<&str> = roots.iter().map(|r| r.directory.as_str()).collect();
    Some(format!(
        "Biggest blind spot: {imports} import(s) name files below {}, which heki does not take for \
         a source root; add {src} (keys at the end), then run heki recommend again: it renames \
         modules (What heki cannot see)",
        directories.join(", ")
    ))
}
