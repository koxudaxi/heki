//! The text form of an [`InspectReport`]: at most one line per fact, a chain longer than four
//! hops shown by its first two and last two hops.

use super::{Hop, InspectReport, Location};
use std::fmt::Write;

/// The last component of a path (`pkg/mod.py` -> `mod.py`).
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A name without its module: the part after the module prefix, or the last two components.
fn short(fqn: &str, module: &str) -> String {
    match fqn.strip_prefix(module).and_then(|q| q.strip_prefix('.')) {
        Some(q) => q.to_owned(),
        None => {
            let parts: Vec<&str> = fqn.rsplitn(3, '.').collect();
            parts
                .iter()
                .take(2)
                .rev()
                .copied()
                .collect::<Vec<_>>()
                .join(".")
        }
    }
}

/// A called name as the text form shows it: its last two dotted components
/// (`Class.method`, `module.function`, `os.scandir`).
fn callee_text(name: &str) -> String {
    let parts: Vec<&str> = name.rsplitn(3, '.').collect();
    parts
        .iter()
        .take(2)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join(".")
}

fn hop_text(h: &Hop) -> String {
    let candidates = if h.candidates > 1 {
        format!("  (1 of {} candidates)", h.candidates)
    } else {
        String::new()
    };
    let place = format!("{}:{}", file_name(&h.path), h.line);
    format!("{place:<20}{}{candidates}", callee_text(&h.calls))
}

/// One line per hop, the middle of a long chain folded.
fn chain_lines(out: &mut String, indent: &str, chain: &[Hop]) {
    if chain.len() <= 4 {
        for h in chain {
            let _ = writeln!(out, "{indent}{}", hop_text(h));
        }
        return;
    }
    for h in &chain[..2] {
        let _ = writeln!(out, "{indent}{}", hop_text(h));
    }
    let _ = writeln!(
        out,
        "{indent}... {} more (--output-format json lists every hop)",
        chain.len() - 4
    );
    for h in &chain[chain.len() - 2..] {
        let _ = writeln!(out, "{indent}{}", hop_text(h));
    }
}

/// A chain on one line: `file:line callee -> ... -> file:line` of the change.
fn inline_chain(chain: &[Hop], at: Option<&Location>) -> String {
    let mut parts: Vec<String> = chain
        .iter()
        .map(|h| {
            format!(
                "{}:{} {}",
                file_name(&h.path),
                h.line,
                callee_text(&h.calls)
            )
        })
        .collect();
    if parts.len() > 3 {
        let last = parts.split_off(parts.len() - 1);
        parts.truncate(1);
        parts.push("...".to_owned());
        parts.extend(last);
    }
    if let Some(at) = at {
        parts.push(format!("{}:{}", file_name(&at.path), at.line));
    }
    parts.join(" -> ")
}

pub fn render_text(r: &InspectReport) -> String {
    let mut out = String::new();
    let f = &r.function;
    let _ = writeln!(out, "{}  ({})", f.qualname, f.module);
    let decorators: String = f.decorators.iter().map(|d| format!("  @{d}")).collect();
    let _ = writeln!(
        out,
        "  {}:{}-{}  {}{decorators}",
        f.path, f.line, f.end_line, f.kind
    );
    let reasons = r.purity.because.join(", ");
    let _ = if reasons.is_empty() {
        writeln!(out, "  {}", r.purity.verdict)
    } else {
        writeln!(out, "  {}: {reasons}", r.purity.verdict)
    };
    unknown_reach_text(&mut out, r);
    let _ = writeln!(out, "  assumes: {}", r.purity.assumes.join("; "));
    if r.effects.is_empty() {
        out.push_str("effects   none\n");
    } else {
        out.push_str("effects\n");
        for e in &r.effects {
            let how = if e.direct {
                "directly".to_owned()
            } else {
                format!("{} calls down", e.chain.len())
            };
            let _ = writeln!(
                out,
                "  {} ({}), {how}, {}",
                e.effect,
                e.runs,
                guards_text(e)
            );
            chain_lines(&mut out, "    ", &e.chain);
            for s in &e.via_subclasses {
                let _ = writeln!(out, "    through subclass {}:", s.class);
                chain_lines(&mut out, "      ", &s.chain);
            }
        }
    }
    changes_text(&mut out, r);
    let stored = if r.retains.stored.is_empty() {
        "stores no parameter".to_owned()
    } else {
        format!("stores {}", r.retains.stored.join(", "))
    };
    let kept = match &r.retains.kept {
        Some(k) if k.is_empty() => "keeps none".to_owned(),
        Some(k) => format!("keeps {}", k.join(", ")),
        None => "what it keeps is not computed (needs an ownership contract)".to_owned(),
    };
    let _ = writeln!(out, "retains   {stored}; {kept}");
    let ret = &r.returns;
    let origins = match &ret.origins {
        None => "a constant or a value of unknown origin".to_owned(),
        Some(o) => {
            let mut what = Vec::new();
            if o.new {
                what.push("a new object".to_owned());
            }
            if o.held {
                what.push("state that outlives the call".to_owned());
            }
            what.extend(o.params.iter().map(|p| format!("`{p}`")));
            what.extend(o.elements_of.iter().map(|p| format!("an element of `{p}`")));
            what.join(" or ")
        }
    };
    let _ = writeln!(out, "returns   {}: {origins}", ret.kind);
    if let Some(s) = &ret.shared_with {
        let _ = writeln!(out, "  shared with read-only producer {}", s.producer);
    }
    if !ret.iterated_effects.is_empty() {
        let _ = writeln!(
            out,
            "  iterating it runs: {}",
            ret.iterated_effects.join(", ")
        );
    }
    let c = &r.callers;
    if c.entries.is_empty() {
        out.push_str("callers   none in the checked files\n");
    } else {
        let more = if c.truncated { " (truncated)" } else { "" };
        let _ = writeln!(out, "callers   {}{more}", c.count);
        for e in &c.entries {
            let mut notes = Vec::new();
            if e.in_loop {
                notes.push("in a loop".to_owned());
            }
            if e.awaited {
                notes.push("awaited".to_owned());
            }
            if e.role != "call" {
                notes.push(e.role.to_owned());
            }
            if e.candidates > 1 {
                notes.push(format!("1 of {} candidates", e.candidates));
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!("  ({})", notes.join(", "))
            };
            let _ = writeln!(
                out,
                "  {}{}  {}:{}{notes}",
                "  ".repeat(e.depth - 1),
                short(&e.fqn, &f.module),
                file_name(&e.path),
                e.line
            );
        }
    }
    if r.reports.is_empty() {
        out.push_str("reports   none\n");
    } else {
        out.push_str("reports\n");
        for d in &r.reports {
            let baseline = d.baseline.map(|b| format!(" [{b}]")).unwrap_or_default();
            let _ = writeln!(
                out,
                "  {}:{}  {} {}{baseline}",
                d.diagnostic.path.to_string_lossy(),
                d.diagnostic.line,
                d.diagnostic.rule_code,
                d.diagnostic.message
            );
        }
    }
    if r.contracts.is_empty() {
        out.push_str("contracts none\n");
    } else {
        out.push_str("contracts\n");
        for c in &r.contracts {
            let _ = writeln!(out, "  {} {}: {}", c.kind, c.name, c.roles.join(", "));
        }
    }
    for e in &r.errors {
        let _ = writeln!(out, "error: {e}");
    }
    out
}

/// The functions heki cannot follow, one line each with the chain to it.
fn unknown_reach_text(out: &mut String, r: &InspectReport) {
    let reach = &r.purity.unknown_reach;
    if reach.entries.is_empty() {
        return;
    }
    let more = if reach.truncated {
        format!(" (the first {})", reach.entries.len())
    } else {
        String::new()
    };
    let _ = writeln!(out, "  cannot follow{more}");
    let names: Vec<String> = reach
        .entries
        .iter()
        .map(|u| short(&u.fqn, &r.function.module))
        .collect();
    let width = names.iter().map(String::len).max().unwrap_or(0).min(40);
    for (name, u) in names.iter().zip(&reach.entries) {
        let how = if u.chain.is_empty() {
            "itself".to_owned()
        } else {
            inline_chain(&u.chain, None)
        };
        let _ = writeln!(out, "    {name:<width$}  {how}");
    }
}

fn guards_text(e: &super::EffectInfo) -> String {
    let mut parts: Vec<String> = e
        .guards
        .iter()
        .map(|g| {
            let subject = match &g.attr {
                Some(a) => format!("{}.{a}", g.param),
                None => g.param.clone(),
            };
            let test = match (g.test, &g.has) {
                ("truthy", _) => subject,
                ("is_none", _) => format!("{subject} is None"),
                (_, Some(name)) => format!("hasattr({subject}, {name:?})"),
                _ => format!("isinstance({subject}, {})", g.classes.join(" | ")),
            };
            if g.holds {
                format!("only when {test}")
            } else {
                format!("only unless {test}")
            }
        })
        .collect();
    if let Some(p) = &e.once_per {
        parts.push(format!("once per {}", p.join(", ")));
    }
    if parts.is_empty() {
        "whenever it runs".to_owned()
    } else {
        parts.join(", ")
    }
}

fn changes_text(out: &mut String, r: &InspectReport) {
    let c = &r.changes;
    let mut lines: Vec<(String, String)> = Vec::new();
    let method = r.function.class.is_some() && r.function.kind == "method";
    for p in &c.params {
        let how = if p.direct {
            p.at.as_ref().map_or_else(
                || "directly".to_owned(),
                |at| format!("directly: {}:{}", file_name(&at.path), at.line),
            )
        } else {
            inline_chain(&p.chain, p.at.as_ref())
        };
        // A method's own `self` attributes are listed below, each with its chain.
        let listed =
            |what: &String| method && p.index == 0 && c.self_attrs.iter().any(|a| &a.attr == what);
        for what in p.what.iter().filter(|w| !listed(w)) {
            let name = if what.is_empty() {
                p.param.clone()
            } else if what == heki_effect::ELEMENTS_PATH {
                format!("{}[...]", p.param)
            } else {
                format!("{}.{what}", p.param)
            };
            lines.push((name, how.clone()));
        }
        // A container held in an item of the parameter, changed in place.
        for slot in p.slots.iter().filter(|s| s.starts_with('[')) {
            lines.push((format!("{}{slot}", p.param), format!("in place, {how}")));
        }
    }
    for a in &c.self_attrs {
        let name = match &a.key {
            Some(k) => format!("self.{}[{k:?}]", a.attr),
            None => format!("self.{}", a.attr),
        };
        let how = if a.direct {
            "directly".to_owned()
        } else {
            inline_chain(&a.chain, a.at.as_ref())
        };
        lines.push((name, how));
    }
    for i in &c.invalidates {
        lines.push((format!("self.{i}"), "deletes the cached value".to_owned()));
    }
    for g in &c.globals {
        let name = match &g.cell_of {
            Some(owner) => format!("{} (closure of {owner})", g.name),
            None => format!("global {}", g.name),
        };
        let how = if g.direct {
            format!("directly: {}:{}", file_name(&g.at.path), g.at.line)
        } else {
            inline_chain(&g.chain, Some(&g.at))
        };
        lines.push((name, how));
    }
    match &c.class_attrs {
        Some(writes) => {
            for w in writes.iter().filter(|w| !w.fresh) {
                lines.push((format!("{}.{}", w.class, w.attr), w.kind.to_owned()));
            }
        }
        None => lines.push((
            "other objects".to_owned(),
            "not computed (an ownership contract, MUT108, or MUT112 propagates them)".to_owned(),
        )),
    }
    if lines.is_empty() {
        out.push_str("changes   none\n");
        return;
    }
    out.push_str("changes\n");
    let width = lines
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(0)
        .min(40);
    for (name, how) in lines {
        let line = format!("  {name:<width$}  {how}");
        let _ = writeln!(out, "{}", line.trim_end());
    }
}
