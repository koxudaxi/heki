//! Optional offline profile dump loader (no runtime instrumentation / no process attachment).
//!
//! Supports collapsed stacks (CPython 3.15 Tachyon `--collapsed`: `tid:N;file.py:func:line;...
//! count`, and py-spy: `func (file.py:line);... count`) and Tachyon `--gecko` JSON dumps
//! (Firefox processed-profile format), for prioritizing diagnostics and promoting
//! `size-dependent` `OPT` fixes in hot functions (docs/ANALYSIS_AND_KB_SPEC.md section 4.4).
//!
//! A dump keeps its distinct stacks with their sample weights; shares are computed per project
//! function ([`ProfileData::attribute`]). A frame counts for the one project function it can
//! denote, and for none when it fits several (a plain `run` in a file with two `run` methods
//! and no line to tell them apart, a basename two packages share).

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// A function is hot when it is on the stack in at least this share of the samples.
pub const HOT_THRESHOLD_PCT: f64 = 1.0;

/// What a profiler recorded about one frame.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Frame {
    /// File as recorded by the profiler (often a basename; empty when unknown).
    file: String,
    /// Plain or qualified function name.
    func: String,
    /// The line the frame was executing, when recorded.
    line: Option<u32>,
}

/// The distinct sampled stacks of a dump (frame indices, root first) with their weights.
#[derive(Debug, Clone, Default)]
pub struct ProfileData {
    frames: Vec<Frame>,
    stacks: Vec<(Vec<u32>, u64)>,
    total: u64,
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("unsupported or malformed profile dump: {0}")]
    Parse(String),
    #[error("failed to read profile dump `{path}`: {cause}")]
    Read { path: String, cause: std::io::Error },
}

/// A project function a frame may denote.
#[derive(Debug, Clone)]
pub struct ProfiledFunction<'a> {
    pub path: &'a Path,
    pub qualname: &'a str,
    /// The lines of its definition, when known: a frame recorded with a line denotes it only
    /// when the line is among them.
    pub lines: Option<RangeInclusive<u32>>,
}

/// The sample share of each project function a profile attributes frames to.
#[derive(Debug, Clone, Default)]
pub struct HotFunctions {
    cum_pct: BTreeMap<(PathBuf, String), f64>,
}

impl HotFunctions {
    /// Share of the samples with the function `qualname` of `path` anywhere on the stack.
    pub fn cum_pct(&self, path: &Path, qualname: &str) -> Option<f64> {
        self.cum_pct
            .get(&(path.to_path_buf(), qualname.to_owned()))
            .copied()
    }

    /// Hot: on the stack in at least [`HOT_THRESHOLD_PCT`] of the samples.
    pub fn is_hot(&self, path: &Path, qualname: &str) -> bool {
        self.cum_pct(path, qualname)
            .is_some_and(|p| p >= HOT_THRESHOLD_PCT)
    }
}

/// The sum of sample weights, an error when it does not fit a `u64` (it would wrap and make
/// every share wrong).
fn total_weight(weights: impl IntoIterator<Item = u64>) -> Result<u64, ProfileError> {
    weights.into_iter().try_fold(0u64, |sum, w| {
        sum.checked_add(w)
            .ok_or_else(|| ProfileError::Parse("sample weights overflow".into()))
    })
}

/// Builds a [`ProfileData`], each distinct frame stored once.
#[derive(Default)]
struct Builder {
    data: ProfileData,
    frame_ids: BTreeMap<Frame, u32>,
}

impl Builder {
    fn frame(&mut self, frame: Frame) -> u32 {
        if let Some(&id) = self.frame_ids.get(&frame) {
            return id;
        }
        let id = u32::try_from(self.data.frames.len()).unwrap_or(u32::MAX);
        self.data.frames.push(frame.clone());
        self.frame_ids.insert(frame, id);
        id
    }

    fn stack(&mut self, frames: Vec<u32>, weight: u64) -> Result<(), ProfileError> {
        self.data.total = total_weight([self.data.total, weight])?;
        self.data.stacks.push((frames, weight));
        Ok(())
    }
}

/// The frame of one collapsed-stack entry, `None` for thread / process markers.
fn parse_frame(frame: &str) -> Option<Frame> {
    let frame = frame.trim();
    if frame.is_empty() || frame.starts_with("tid:") {
        return None;
    }
    let line_number = |s: &str| s.parse::<u32>().ok();
    // py-spy: `func (path/file.py:12)`
    if let Some(open) = frame.rfind(" (")
        && let Some(inner) = frame[open + 2..].strip_suffix(')')
    {
        let (file, line) = match inner.rsplit_once(':') {
            Some((f, l)) if line_number(l).is_some() => (f, line_number(l)),
            _ => (inner, None),
        };
        return Some(Frame {
            file: file.to_owned(),
            func: frame[..open].to_owned(),
            line,
        });
    }
    // Tachyon: `file.py:func:12`
    let parts: Vec<&str> = frame.rsplitn(3, ':').collect();
    if let [line, func, file] = parts.as_slice()
        && let Some(line) = line_number(line)
    {
        return Some(Frame {
            file: (*file).to_owned(),
            func: (*func).to_owned(),
            line: Some(line),
        });
    }
    Some(Frame {
        file: String::new(),
        func: frame.to_owned(),
        line: None,
    })
}

/// Parse a collapsed stack dump (`frame;frame;leaf count` per line).
pub fn parse_collapsed(text: &str) -> Result<ProfileData, ProfileError> {
    let mut b = Builder::default();
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
        let frames = stack
            .split(';')
            .filter_map(parse_frame)
            .map(|f| b.frame(f))
            .collect();
        b.stack(frames, count)?;
    }
    Ok(b.data)
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

/// Parse a Tachyon `--gecko` dump: `shared.stringArray`, and per thread `samples.stack` (and
/// `weight`), `stackTable.{frame,prefix}`, `frameTable.func` (and `line`),
/// `funcTable.{name,fileName}`.
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
    let mut b = Builder::default();
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
        let fr_line = frame_t.get("line").and_then(Value::as_array);
        let weights = samples.get("weight").and_then(Value::as_array);
        // A dump stores each stack once and lists its samples by index: sum their weights
        // first, so a stack is expanded once however often it was sampled.
        let mut by_stack: BTreeMap<Option<usize>, u64> = BTreeMap::new();
        for i in 0..s_stack.len() {
            let weight = weights
                .and_then(|w| w.get(i))
                .and_then(Value::as_u64)
                .unwrap_or(1);
            let stack = reference(s_stack, i, st_frame.len(), "samples.stack", true)?;
            let sum = by_stack.entry(stack).or_default();
            *sum = total_weight([*sum, weight])?;
        }
        for (stack, weight) in by_stack {
            let mut frames = Vec::new();
            let mut cur = stack;
            while let Some(si) = cur {
                if frames.len() > st_frame.len() {
                    return Err(ProfileError::Parse("cyclic gecko stack table".into()));
                }
                let fi = reference(st_frame, si, fr_func.len(), "stackTable.frame", false)?
                    .unwrap_or_default();
                let func = reference(fr_func, fi, fn_name.len(), "frameTable.func", false)?
                    .unwrap_or_default();
                let frame = Frame {
                    file: string(fn_file, func, "funcTable.fileName", true)?,
                    func: string(fn_name, func, "funcTable.name", false)?,
                    line: fr_line
                        .and_then(|l| l.get(fi))
                        .and_then(Value::as_u64)
                        .and_then(|l| u32::try_from(l).ok()),
                };
                frames.push(b.frame(frame));
                cur = reference(st_prefix, si, st_frame.len(), "stackTable.prefix", true)?;
            }
            frames.reverse();
            b.stack(frames, weight)?;
        }
    }
    Ok(b.data)
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

/// A frame's function name names `qualname` on a dotted boundary: `run` and `A.run` name
/// `A.run` (and `f.<locals>.A.run`), never `BA.run`.
fn names_function(func: &str, qualname: &str) -> bool {
    qualname == func
        || qualname
            .strip_suffix(func)
            .is_some_and(|outer| outer.ends_with('.'))
}

/// A frame's file names `path` on a path boundary (a basename, a project-relative, or an
/// absolute path); an unrecorded file names every path.
fn names_file(file: &str, path: &str) -> bool {
    let file = file.replace('\\', "/");
    file.is_empty()
        || path == file
        || path.ends_with(&format!("/{file}"))
        || file.ends_with(&format!("/{path}"))
}

impl ProfileData {
    /// The share of the samples each of `functions` is on the stack in, counting only the
    /// frames that denote exactly one of them (typing stubs never run and denote nothing).
    pub fn attribute<'a>(
        &self,
        functions: impl IntoIterator<Item = ProfiledFunction<'a>>,
    ) -> HotFunctions {
        let functions: Vec<(String, ProfiledFunction<'a>)> = functions
            .into_iter()
            .filter(|f| f.path.extension().is_none_or(|e| e != "pyi"))
            .map(|f| (f.path.to_string_lossy().replace('\\', "/"), f))
            .collect();
        let mut by_last_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (i, (_, f)) in functions.iter().enumerate() {
            let last = f.qualname.rsplit('.').next().unwrap_or(f.qualname);
            by_last_name.entry(last).or_default().push(i);
        }
        let denotes = |frame: &Frame, (path, f): &(String, ProfiledFunction<'_>)| {
            let line_fits = match (&f.lines, frame.line) {
                (Some(lines), Some(line)) => lines.contains(&line),
                _ => true,
            };
            line_fits && names_function(&frame.func, f.qualname) && names_file(&frame.file, path)
        };
        let owner: Vec<Option<usize>> = self
            .frames
            .iter()
            .map(|frame| {
                let last = frame.func.rsplit('.').next().unwrap_or(&frame.func);
                let mut fits = by_last_name
                    .get(last)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|&i| denotes(frame, &functions[i]));
                let first = fits.next();
                if fits.next().is_some() { None } else { first }
            })
            .collect();
        // Weights add up to at most the checked total.
        let mut cum = vec![0u64; functions.len()];
        for (frames, weight) in &self.stacks {
            let on_stack: BTreeSet<usize> =
                frames.iter().filter_map(|&f| owner[f as usize]).collect();
            for i in on_stack {
                cum[i] += weight;
            }
        }
        let mut cum_pct = BTreeMap::new();
        for (i, n) in cum.into_iter().enumerate().filter(|(_, n)| *n > 0) {
            let f = &functions[i].1;
            cum_pct.insert(
                (f.path.to_path_buf(), f.qualname.to_owned()),
                (n as f64) * 100.0 / (self.total as f64),
            );
        }
        HotFunctions { cum_pct }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function<'a>(
        path: &'a str,
        qualname: &'a str,
        lines: RangeInclusive<u32>,
    ) -> ProfiledFunction<'a> {
        ProfiledFunction {
            path: Path::new(path),
            qualname,
            lines: Some(lines),
        }
    }

    #[test]
    fn collapsed_formats_fold_cumulative_shares_per_function() {
        let fns = [
            function("src/pkg/app.py", "main", 1..=10),
            function("src/pkg/app.py", "Worker.work", 11..=20),
            function("src/pkg/other.py", "work", 1..=5),
        ];
        let tachyon = "tid:1;app.py:main:9;app.py:Worker.work:13 90\ntid:1;app.py:main:9 10\n";
        let hot = parse_collapsed(tachyon).unwrap().attribute(fns.clone());
        let app = Path::new("src/pkg/app.py");
        assert_eq!(hot.cum_pct(app, "Worker.work"), Some(90.0));
        assert_eq!(hot.cum_pct(app, "main"), Some(100.0));
        assert!(!hot.is_hot(Path::new("src/pkg/other.py"), "work"));

        let pyspy = "main (src/pkg/app.py:9);work (src/pkg/app.py:13) 30\nidle 970\n";
        let hot = parse_collapsed(pyspy).unwrap().attribute(fns);
        assert!(hot.is_hot(app, "Worker.work"));
        assert_eq!(hot.cum_pct(app, "main"), Some(3.0));
        assert!(parse_collapsed("no-count\n").is_err());
        assert!(parse_collapsed("a;b x\n").is_err());
    }

    #[test]
    fn frames_count_only_for_the_one_function_they_denote() {
        let fns = [
            function("app.py", "A.run", 1..=5),
            function("app.py", "BA.run", 6..=10),
            function("one/models.py", "build", 1..=3),
            function("two/models.py", "build", 1..=3),
            function("app.pyi", "A.run", 1..=1),
        ];
        let hot = |dump: &str| parse_collapsed(dump).unwrap().attribute(fns.clone());
        let app = Path::new("app.py");
        // A qualified name matches on a dotted boundary; the stub never runs.
        let h = hot("app.py:A.run:3 10\n");
        assert_eq!(h.cum_pct(app, "A.run"), Some(100.0));
        assert_eq!(h.cum_pct(app, "BA.run"), None);
        // A plain name with a line picks the method holding it; without a line, neither.
        let h = hot("app.py:run:8 10\nrun (app.py) 10\n");
        assert_eq!(h.cum_pct(app, "BA.run"), Some(50.0));
        assert_eq!(h.cum_pct(app, "A.run"), None);
        // A basename two packages share denotes neither; a longer path one of them.
        assert_eq!(
            hot("models.py:build:2 10\n").cum_pct(Path::new("one/models.py"), "build"),
            None
        );
        assert_eq!(
            hot("/srv/one/models.py:build:2 10\n").cum_pct(Path::new("one/models.py"), "build"),
            Some(100.0)
        );
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
    fn gecko_dump_walks_each_stack_once() {
        let gecko = r#"{
          "meta": {"interval": 1},
          "threads": [{
            "samples": {"stack": [1, 1, 0, null], "time": [0, 1, 2, 3], "weight": null, "length": 4},
            "stackTable": {"frame": [0, 1], "prefix": [null, 0], "length": 2},
            "frameTable": {"func": [0, 1], "line": [2, null], "length": 2},
            "funcTable": {"name": [0, 1], "fileName": [2, 2], "length": 2}
          }],
          "shared": {"stringArray": ["main", "work", "/x/app.py"]}
        }"#;
        let p = parse_gecko(gecko).unwrap();
        // Two stacks and the idle sample, each expanded once.
        assert_eq!(p.stacks.len(), 3);
        assert_eq!(p.total, 4);
        let hot = p.attribute([
            function("app.py", "main", 1..=4),
            function("app.py", "work", 5..=9),
        ]);
        assert_eq!(hot.cum_pct(Path::new("app.py"), "work"), Some(50.0));
        assert_eq!(hot.cum_pct(Path::new("app.py"), "main"), Some(75.0));
        assert!(parse_gecko("{}").is_err());
        assert!(parse_gecko("{\"shared\": {\"stringArray\": []}, \"threads\": [{}]}").is_err());
        assert!(parse_gecko("not json").is_err());
    }

    #[test]
    fn gecko_samples_of_one_stack_add_up() {
        let samples = "1, ".repeat(9_999) + "0";
        let gecko = format!(
            r#"{{"threads": [{{"samples": {{"stack": [{samples}]}}, "stackTable": {{"frame": [0, 1], "prefix": [null, 0]}}, "frameTable": {{"func": [0, 1]}}, "funcTable": {{"name": [0, 1], "fileName": [2, 2]}}}}], "shared": {{"stringArray": ["main", "work", "app.py"]}}}}"#
        );
        let p = parse_gecko(&gecko).unwrap();
        assert_eq!(p.stacks.len(), 2);
        let hot = p.attribute([
            ProfiledFunction {
                path: Path::new("app.py"),
                qualname: "main",
                lines: None,
            },
            ProfiledFunction {
                path: Path::new("app.py"),
                qualname: "work",
                lines: None,
            },
        ]);
        assert_eq!(hot.cum_pct(Path::new("app.py"), "work"), Some(99.99));
        assert_eq!(hot.cum_pct(Path::new("app.py"), "main"), Some(100.0));
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
}
