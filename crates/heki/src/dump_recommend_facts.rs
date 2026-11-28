//! `heki dump-recommend-facts` (hidden): the facts `heki recommend` reads besides the rules, as
//! JSON, for the tests that check them through the binary and for measuring what they cost:
//! the calls heki is blind to, with their counts per project ([`heki_rules::blind_spots`]), and
//! the census of class attributes and modules contract candidates are drawn from
//! ([`heki_rules::census`]).
//!
//! The index is the one `heki check` builds for the same configuration and selection, with the
//! facts of [`heki_rules::recommend_facts`] ([`project::Facts::Recommend`]).

use crate::DumpRecommendFactsArgs;
use crate::OutputFormat;
use crate::check::{finish, selection, selectors, shown_text};
use anyhow::Result;
use heki_ast::LineIndex;
use heki_rules::blind_spots::{
    BlindSite, BlindSpots, CallCounts, UncataloguedPackage, UnresolvedCalls,
};
use heki_rules::census::{Accesses, Census, CensusTotals, Writes};
use heki_rules::project::{self, Indexed, Project, ProjectOptions};
use rustc_hash::FxHashMap;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

#[derive(Serialize)]
struct Facts<'a> {
    blind_spots: BlindSpotFacts<'a>,
    census: CensusFacts<'a>,
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

#[derive(Serialize)]
struct CensusFacts<'a> {
    totals: CensusTotals,
    #[serde(skip_serializing_if = "Option::is_none")]
    attributes: Option<Vec<Attribute<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    classes: Option<&'a [heki_rules::census::ClassUse<'a>]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modules: Option<Vec<Module<'a>>>,
}

#[derive(Serialize)]
struct Attribute<'a> {
    class: &'a str,
    attr: &'a str,
    writers: Vec<Writer>,
    initializers: Vec<Access>,
    readers: Vec<Access>,
}

#[derive(Serialize)]
struct Access {
    function: String,
    path: String,
    line: u32,
    sites: usize,
}

#[derive(Serialize)]
struct Writer {
    #[serde(flatten)]
    access: Access,
    rebinds: bool,
    in_place: bool,
    fresh: bool,
}

#[derive(Serialize)]
struct Module<'a> {
    module: &'a str,
    path: String,
    functions: usize,
    blocking: Vec<String>,
    unseen: Vec<String>,
}

pub(crate) fn run(args: &DumpRecommendFactsArgs) -> Result<ExitCode> {
    let project = Project::load(&ProjectOptions {
        paths: &args.paths,
        config: args.analysis.config.as_deref(),
        working_dir: None,
        target_version: args.analysis.target_version.as_deref(),
        ..ProjectOptions::default()
    })?;
    let selection = selection(&selectors(&args.analysis), &project)?;
    let Indexed { project, index, .. } =
        Indexed::build(project, selection, None, project::Facts::Recommend)?;
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
    let access = |a: &Accesses<'_>| Access {
        function: a.function.fqn(),
        path: shown_text(a.path),
        line: lines[a.path].line(a.first.start),
        sites: a.sites,
    };
    let writer = |w: &Writes<'_>| Writer {
        access: access(&w.accesses),
        rebinds: w.rebinds,
        in_place: w.in_place,
        fresh: w.fresh,
    };
    let census = Census::of(&index, &index.project_decls());
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
        census: CensusFacts {
            totals: census.totals(),
            attributes: (!args.totals).then(|| {
                (census.attributes.iter())
                    .map(|a| Attribute {
                        class: a.class,
                        attr: a.attr,
                        writers: a.writers.iter().map(writer).collect(),
                        initializers: a.initializers.iter().map(access).collect(),
                        readers: a.readers.iter().map(access).collect(),
                    })
                    .collect()
            }),
            classes: (!args.totals).then_some(census.classes.as_slice()),
            modules: (!args.totals).then(|| {
                (census.modules.iter())
                    .map(|m| Module {
                        module: m.module,
                        path: shown_text(m.path),
                        functions: m.functions.len(),
                        blocking: m.blocking.iter().map(|f| f.fqn()).collect(),
                        unseen: m.unseen.iter().map(|f| f.fqn()).collect(),
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
        &crate::failure::of_files(&project.errors),
        ExitCode::SUCCESS,
    )
}
