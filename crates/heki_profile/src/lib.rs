//! Optional offline profile dump loader (no runtime instrumentation / no process attachment).
//!
//! Supports collapsed stacks (CPython 3.15 Tachyon `--collapsed`: `tid:N;file.py:func:line;...
//! count`, and py-spy: `func (file.py:line);... count`) and Tachyon `--gecko` JSON dumps
//! (Firefox processed-profile format), for prioritizing diagnostics and promoting
//! `size-dependent` `OPT` fixes in hot functions (docs/ANALYSIS_AND_KB_SPEC.md section 4.4).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use thiserror::Error;

/// A function is hot when it is on the stack in at least this share of the samples.
pub const HOT_THRESHOLD_PCT: f64 = 1.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HotSite {
    /// File as recorded by the profiler (often a basename; empty when unknown).
    pub file: String,
    pub func: String,
    /// Share of samples with this function on top of the stack.
    pub self_pct: f64,
    /// Share of samples with this function anywhere on the stack.
    pub cum_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileData {
    pub sites: BTreeMap<(String, String), HotSite>,
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("unsupported or malformed profile dump: {0}")]
    Parse(String),
    #[error("failed to read profile dump `{path}`: {cause}")]
    Read { path: String, cause: std::io::Error },
}

/// `(file, func)` of one collapsed-stack frame, `None` for thread / process markers.
fn parse_frame(frame: &str) -> Option<(String, String)> {
    let frame = frame.trim();
    if frame.is_empty() || frame.starts_with("tid:") {
        return None;
    }
    // py-spy: `func (path/file.py:12)`
    if let Some(open) = frame.rfind(" (")
        && let Some(inner) = frame[open + 2..].strip_suffix(')')
    {
        let file = inner.rsplit_once(':').map_or(inner, |(f, _)| f);
        return Some((file.to_owned(), frame[..open].to_owned()));
    }
    // Tachyon: `file.py:func:12`
    let parts: Vec<&str> = frame.rsplitn(3, ':').collect();
    if let [line, func, file] = parts.as_slice()
        && line.chars().all(|c| c.is_ascii_digit())
    {
        return Some(((*file).to_owned(), (*func).to_owned()));
    }
    Some((String::new(), frame.to_owned()))
}

/// The sum of sample weights, an error when it does not fit a `u64` (it would wrap and make
/// every share wrong).
fn total_weight(weights: impl IntoIterator<Item = u64>) -> Result<u64, ProfileError> {
    weights.into_iter().try_fold(0u64, |sum, w| {
        sum.checked_add(w)
            .ok_or_else(|| ProfileError::Parse("sample weights overflow".into()))
    })
}

/// Fold per-sample stacks (`leaf` last) into percentages.
fn fold(stacks: Vec<(Vec<(String, String)>, u64)>) -> Result<ProfileData, ProfileError> {
    let total = total_weight(stacks.iter().map(|(_, n)| *n))?;
    let mut self_n: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut cum_n: BTreeMap<(String, String), u64> = BTreeMap::new();
    for (frames, n) in stacks {
        if let Some(leaf) = frames.last() {
            *self_n.entry(leaf.clone()).or_default() += n;
        }
        let distinct: BTreeSet<&(String, String)> = frames.iter().collect();
        for f in distinct {
            *cum_n.entry(f.clone()).or_default() += n;
        }
    }
    let mut sites = BTreeMap::new();
    if total > 0 {
        let pct = |n: u64| (n as f64) * 100.0 / (total as f64);
        for (key, cum) in cum_n {
            sites.insert(
                key.clone(),
                HotSite {
                    file: key.0.clone(),
                    func: key.1.clone(),
                    self_pct: pct(self_n.get(&key).copied().unwrap_or(0)),
                    cum_pct: pct(cum),
                },
            );
        }
    }
    Ok(ProfileData { sites })
}

/// Parse a collapsed stack dump (`frame;frame;leaf count` per line).
pub fn parse_collapsed(text: &str) -> Result<ProfileData, ProfileError> {
    let mut stacks = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((stack, count_str)) = trimmed.rsplit_once(' ') else {
            return Err(ProfileError::Parse(trimmed.to_string()));
        };
        let count: u64 = count_str
            .parse()
            .map_err(|_| ProfileError::Parse(trimmed.to_string()))?;
        stacks.push((stack.split(';').filter_map(parse_frame).collect(), count));
    }
    fold(stacks)
}

fn column<'a>(table: &'a Value, name: &str) -> Result<&'a Vec<Value>, ProfileError> {
    table
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| ProfileError::Parse(format!("gecko table without `{name}`")))
}

/// Entry `i` of a gecko table column as a row of a table of `len` rows: `Ok(None)` for `null`
/// when `nullable`; anything else that is no row is an error (a dropped sample or an empty
/// file name would silently change the shares).
fn reference(
    col: &[Value],
    i: usize,
    len: usize,
    what: &str,
    nullable: bool,
) -> Result<Option<usize>, ProfileError> {
    let bad = || ProfileError::Parse(format!("gecko `{what}` entry {i} refers to no row"));
    match col.get(i) {
        Some(Value::Null) if nullable => Ok(None),
        Some(v) => {
            let row = v
                .as_u64()
                .and_then(|r| usize::try_from(r).ok())
                .filter(|r| *r < len)
                .ok_or_else(bad)?;
            Ok(Some(row))
        }
        None => Err(bad()),
    }
}

/// Parse a Tachyon `--gecko` dump: `shared.stringArray`, and per thread `samples.stack`,
/// `stackTable.{frame,prefix}`, `frameTable.func`, `funcTable.{name,fileName}`.
pub fn parse_gecko(text: &str) -> Result<ProfileData, ProfileError> {
    let root: Value =
        serde_json::from_str(text).map_err(|e| ProfileError::Parse(format!("gecko JSON: {e}")))?;
    let strings = root
        .pointer("/shared/stringArray")
        .and_then(Value::as_array)
        .ok_or_else(|| ProfileError::Parse("gecko dump without `shared.stringArray`".into()))?;
    // A file name may be unknown (`null`); a function name may not.
    let string = |col: &[Value], i: usize, what: &str, nullable: bool| {
        Ok::<_, ProfileError>(
            reference(col, i, strings.len(), what, nullable)?
                .and_then(|s| strings[s].as_str())
                .unwrap_or_default()
                .to_owned(),
        )
    };
    let threads = root
        .get("threads")
        .and_then(Value::as_array)
        .ok_or_else(|| ProfileError::Parse("gecko dump without `threads`".into()))?;
    let mut stacks = Vec::new();
    for t in threads {
        let empty = Value::Null;
        let samples = t.get("samples").unwrap_or(&empty);
        let stack_t = t.get("stackTable").unwrap_or(&empty);
        let frame_t = t.get("frameTable").unwrap_or(&empty);
        let func_t = t.get("funcTable").unwrap_or(&empty);
        let (s_stack, st_frame, st_prefix) = (
            column(samples, "stack")?,
            column(stack_t, "frame")?,
            column(stack_t, "prefix")?,
        );
        let (fr_func, fn_name, fn_file) = (
            column(frame_t, "func")?,
            column(func_t, "name")?,
            column(func_t, "fileName")?,
        );
        let weights = samples.get("weight").and_then(Value::as_array);
        for i in 0..s_stack.len() {
            let weight = weights
                .and_then(|w| w.get(i))
                .and_then(Value::as_u64)
                .unwrap_or(1);
            let mut frames = Vec::new();
            let mut cur = reference(s_stack, i, st_frame.len(), "samples.stack", true)?;
            while let Some(si) = cur {
                if frames.len() > st_frame.len() {
                    return Err(ProfileError::Parse("cyclic gecko stack table".into()));
                }
                let fi = reference(st_frame, si, fr_func.len(), "stackTable.frame", false)?
                    .unwrap_or_default();
                let func = reference(fr_func, fi, fn_name.len(), "frameTable.func", false)?
                    .unwrap_or_default();
                frames.push((
                    string(fn_file, func, "funcTable.fileName", true)?,
                    string(fn_name, func, "funcTable.name", false)?,
                ));
                cur = reference(st_prefix, si, st_frame.len(), "stackTable.prefix", true)?;
            }
            frames.reverse();
            stacks.push((frames, weight));
        }
    }
    fold(stacks)
}

/// Read a dump, detecting the format (`{` starts a gecko JSON dump).
pub fn load(path: &Path) -> Result<ProfileData, ProfileError> {
    let text = std::fs::read_to_string(path).map_err(|cause| ProfileError::Read {
        path: path.display().to_string(),
        cause,
    })?;
    if text.trim_start().starts_with('{') {
        parse_gecko(&text)
    } else {
        parse_collapsed(&text)
    }
}

impl ProfileData {
    /// Highest cumulative share of a function of `path` named like `qualname` (the profiler's
    /// name may be the plain or qualified function name; files may be basenames).
    pub fn cum_pct(&self, path: &Path, qualname: &str) -> Option<f64> {
        let short = qualname.rsplit('.').next().unwrap_or(qualname);
        let path = path.to_string_lossy().replace('\\', "/");
        self.sites
            .values()
            .filter(|s| {
                let func_ok = s.func == qualname
                    || s.func == short
                    || s.func.rsplit('.').next() == Some(short) && qualname.ends_with(&s.func);
                let file = s.file.replace('\\', "/");
                let file_ok = file.is_empty()
                    || path == file
                    || path.ends_with(&format!("/{file}"))
                    || file.ends_with(&format!("/{path}"));
                func_ok && file_ok
            })
            .map(|s| s.cum_pct)
            .fold(None, |acc: Option<f64>, p| {
                Some(acc.map_or(p, |a| a.max(p)))
            })
    }

    /// Hot: on the stack in at least [`HOT_THRESHOLD_PCT`] of the samples.
    pub fn is_hot(&self, path: &Path, qualname: &str) -> bool {
        self.cum_pct(path, qualname)
            .is_some_and(|p| p >= HOT_THRESHOLD_PCT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapsed_formats_fold_self_and_cumulative_shares() {
        let tachyon = "tid:1;app.py:main:9;app.py:work:3 90\ntid:1;app.py:main:9 10\n";
        let p = parse_collapsed(tachyon).unwrap();
        let work = &p.sites[&("app.py".to_owned(), "work".to_owned())];
        assert_eq!((work.self_pct, work.cum_pct), (90.0, 90.0));
        let main = &p.sites[&("app.py".to_owned(), "main".to_owned())];
        assert_eq!((main.self_pct, main.cum_pct), (10.0, 100.0));
        assert!(p.is_hot(Path::new("src/pkg/app.py"), "Worker.work"));
        assert!(!p.is_hot(Path::new("src/pkg/other.py"), "work"));

        let pyspy = "main (src/app.py:9);work (src/app.py:3) 30\nidle 970\n";
        let p = parse_collapsed(pyspy).unwrap();
        assert!(p.is_hot(Path::new("src/app.py"), "work"));
        assert!(!p.is_hot(Path::new("src/app.py"), "missing"));
        assert!(parse_collapsed("no-count\n").is_err());
        assert!(parse_collapsed("a;b x\n").is_err());
    }

    #[test]
    fn gecko_references_to_missing_rows_are_an_error() {
        let dump = |stack: &str, frame: &str, func: &str, name: &str, file: &str| {
            format!(
                r#"{{"threads": [{{"samples": {{"stack": [{stack}]}}, "stackTable": {{"frame": [{frame}], "prefix": [null]}}, "frameTable": {{"func": [{func}]}}, "funcTable": {{"name": [{name}], "fileName": [{file}]}}}}], "shared": {{"stringArray": ["f"]}}}}"#
            )
        };
        for (d, what) in [
            (dump("7", "0", "0", "0", "null"), "samples.stack"),
            (dump("0", "3", "0", "0", "null"), "stackTable.frame"),
            (dump("0", "null", "0", "0", "null"), "stackTable.frame"),
            (dump("0", "0", "4", "0", "null"), "frameTable.func"),
            (dump("0", "0", "0", "5", "null"), "funcTable.name"),
            (dump("0", "0", "0", "0", "9"), "funcTable.fileName"),
        ] {
            assert!(
                matches!(parse_gecko(&d), Err(ProfileError::Parse(m)) if m.contains(what)),
                "{what}"
            );
        }
        // An idle sample (`null` stack) and an unknown file (`null`) are no errors.
        assert!(parse_gecko(&dump("null, 0", "0", "0", "0", "null")).is_ok());
    }

    #[test]
    fn weights_that_overflow_the_total_are_an_error() {
        let big = "a 9223372036854775808\nb 9223372036854775808\n";
        assert!(matches!(
            parse_collapsed(big),
            Err(ProfileError::Parse(m)) if m.contains("overflow")
        ));
        let gecko = r#"{"threads": [{"samples": {"stack": [0, 0], "weight": [18446744073709551615, 1]}, "stackTable": {"frame": [0], "prefix": [null]}, "frameTable": {"func": [0]}, "funcTable": {"name": [0], "fileName": [0]}}], "shared": {"stringArray": ["f"]}}"#;
        assert!(parse_gecko(gecko).is_err());
    }

    #[test]
    fn gecko_dump_walks_stack_prefixes() {
        let gecko = r#"{
          "meta": {"interval": 1},
          "threads": [{
            "samples": {"stack": [1, 1, 0, null], "time": [0, 1, 2, 3], "weight": null, "length": 4},
            "stackTable": {"frame": [0, 1], "prefix": [null, 0], "length": 2},
            "frameTable": {"func": [0, 1], "length": 2},
            "funcTable": {"name": [0, 1], "fileName": [2, 2], "length": 2}
          }],
          "shared": {"stringArray": ["main", "work", "/x/app.py"]}
        }"#;
        let p = parse_gecko(gecko).unwrap();
        let work = &p.sites[&("/x/app.py".to_owned(), "work".to_owned())];
        assert_eq!(work.cum_pct, 50.0);
        assert!(p.is_hot(Path::new("app.py"), "work"));
        assert!(parse_gecko("{}").is_err());
        assert!(parse_gecko("{\"shared\": {\"stringArray\": []}, \"threads\": [{}]}").is_err());
        assert!(parse_gecko("not json").is_err());
    }
}
