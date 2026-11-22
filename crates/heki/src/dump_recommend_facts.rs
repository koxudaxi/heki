//! `heki dump-recommend-facts` (hidden): the facts `heki recommend` reads besides the rules, as
//! JSON, for the tests that check them through the binary and for measuring what they cost:
//! the calls heki is blind to, with their counts per project ([`heki_rules::blind_spots`]).
//!
//! The index is the one `heki check` builds for the same configuration and selection, with the
//! facts of [`heki_rules::recommend_facts`].

use crate::DumpRecommendFactsArgs;
use crate::OutputFormat;
use crate::check::{finish, selection, shown_errors, shown_text};
use anyhow::Result;
use heki_ast::LineIndex;
use heki_effect::analyze_project_for;
use heki_rules::blind_spots::{
    BlindSite, BlindSpots, CallCounts, UncataloguedPackage, UnresolvedCalls,
};
use heki_rules::project::{Project, ProjectOptions};
use rustc_hash::FxHashMap;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

#[derive(Serialize)]
struct Facts<'a> {
    blind_spots: BlindSpotFacts<'a>,
}

/// The lists are left out with `--totals`.
#[derive(Serialize)]
struct BlindSpotFacts<'a> {
    totals: Totals,
    #[serde(skip_serializing_if = "Option::is_none")]
    uncatalogued: Option<Vec<Package<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unresolved: Option<Vec<Unresolved<'a>>>,
}

#[derive(Serialize)]
struct Totals {
    functions: usize,
    uncatalogued: CallCounts,
    uncatalogued_packages: usize,
    uncatalogued_symbols: usize,
    registered_calls: usize,
    unresolved: CallCounts,
    unresolved_by_reason: BTreeMap<&'static str, usize>,
}

#[derive(Serialize)]
struct Package<'a> {
    package: &'a str,
    registered: bool,
    #[serde(flatten)]
    counts: CallCounts,
    symbols: Vec<Symbol<'a>>,
}

#[derive(Serialize)]
struct Symbol<'a> {
    symbol: &'a str,
    #[serde(flatten)]
    counts: CallCounts,
    sites: Vec<Site>,
}

#[derive(Serialize)]
struct Unresolved<'a> {
    function: String,
    path: String,
    reason: &'static str,
    parameter: Option<&'a str>,
    names: &'a [&'a str],
    #[serde(flatten)]
    counts: CallCounts,
    sites: Vec<Site>,
}

#[derive(Serialize)]
struct Site {
    path: String,
    line: u32,
    column: u32,
    function: String,
}

pub(crate) fn run(args: &DumpRecommendFactsArgs) -> Result<ExitCode> {
    let project = Project::load(&ProjectOptions {
        paths: &args.paths,
        config: args.analysis.config.as_deref(),
        working_dir: None,
        target_version: args.analysis.target_version.as_deref(),
        overrides: None,
        removed: None,
    })?;
    let selection = selection(&args.analysis, &project)?;
    let env = &project.env;
    let index = analyze_project_for(
        &project.inputs(),
        &project.unread,
        &env.kb,
        heki_rules::summary_contracts(&env.cfg, &env.kb)?,
        heki_rules::typed_writes(&env.cfg, &selection),
        heki_rules::recommend_facts(&selection),
    );
    let lines: FxHashMap<&Path, LineIndex> = project
        .files
        .iter()
        .map(|f| (f.path.as_path(), LineIndex::from_source(&f.source)))
        .collect();
    let site = |s: &BlindSite<'_>| {
        let (line, column) = lines[s.path].line_col(s.span.start);
        Site {
            path: shown_text(s.path),
            line,
            column,
            function: s.function.fqn(),
        }
    };
    let spots = BlindSpots::of(&index);
    let totals = spots.totals();
    let facts = Facts {
        blind_spots: BlindSpotFacts {
            totals: Totals {
                functions: totals.functions,
                uncatalogued: totals.uncatalogued,
                uncatalogued_packages: totals.uncatalogued_packages,
                uncatalogued_symbols: totals.uncatalogued_symbols,
                registered_calls: totals.registered_calls,
                unresolved: totals.unresolved,
                unresolved_by_reason: totals.unresolved_by_reason,
            },
            uncatalogued: (!args.totals).then(|| {
                (spots.uncatalogued.iter())
                    .map(|p: &UncataloguedPackage<'_>| Package {
                        package: p.package,
                        registered: p.registered,
                        counts: p.counts,
                        symbols: (p.symbols.iter())
                            .map(|s| Symbol {
                                symbol: s.symbol,
                                counts: s.counts,
                                sites: s.sites.iter().map(site).collect(),
                            })
                            .collect(),
                    })
                    .collect()
            }),
            unresolved: (!args.totals).then(|| {
                (spots.unresolved.iter())
                    .map(|u: &UnresolvedCalls<'_>| Unresolved {
                        function: u.function.fqn(),
                        path: shown_text(u.path),
                        reason: u.reason.as_str(),
                        parameter: u.reason.parameter(),
                        names: &u.names,
                        counts: u.counts,
                        sites: u.sites.iter().map(site).collect(),
                    })
                    .collect()
            }),
        },
    };
    // Written as it is serialized: the text of a large project's facts is never held at once.
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &facts)?;
    writeln!(out)?;
    drop(out);
    finish(
        OutputFormat::Full,
        &shown_errors(&project.errors),
        ExitCode::SUCCESS,
    )
}
