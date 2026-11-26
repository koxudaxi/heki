//! Shared helpers for the end-to-end tests: a throwaway project directory, the real `heki`
//! binary, the time after which a test calls it hung, and an LSP client (`lsp`).

#![allow(dead_code)]

pub mod lsp;
pub mod panels;
pub mod schema;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How long a test waits for `heki` (a run, a reply or report of the language server, a report
/// of `--watch`) before it fails as hung. It bounds a hang, never a cost: a debug build on a
/// loaded CI runner takes many times longer than on an idle machine, so a limit close to the
/// usual time fails correct runs. `e2e_analysis_bounds.rs` bounds the cost of a shape by its
/// growth from a smaller one instead.
pub const HANG: Duration = Duration::from_secs(300);

/// The opt-in `OPT` rules with a size-dependent rewrite. `--heuristic` does not select them, so
/// a run meant to take every rule `--heuristic` selects and every Soundness-lane rule names them
/// too.
pub const SIZE_DEPENDENT_OPT_RULES: &str = "OPT103,OPT110,OPT111,OPT120,OPT121,OPT130";

/// The exit status of `child`, which is killed, failing the test, once it runs past [`HANG`].
pub fn wait_or_hang(child: &mut Child, what: &str) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > HANG {
            let _ = child.kill();
            panic!("{what} ran past {HANG:?}: it hangs");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub struct Project {
    pub dir: PathBuf,
}

/// A new empty directory under the temporary directory, named after `name`, that no other
/// directory of a running test shares. The process id keeps apart tests run as processes of
/// their own (nextest), and a count keeps apart the directories of one process: `cargo test` runs
/// the tests of a binary as threads of one process, and tests may share a name (a helper making
/// the same project for several tests), so a directory named by both alone would be removed by
/// one test while another works in it.
pub fn fresh_dir(name: &str) -> PathBuf {
    static MADE: AtomicUsize = AtomicUsize::new(0);
    let n = MADE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("heki_e2e_{name}_{}_{n}", std::process::id()));
    // Left behind by an earlier process that had the same id.
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl Project {
    pub fn new(name: &str) -> Self {
        let dir = fresh_dir(name);
        // Marks the project root.
        std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"t\"\n").unwrap();
        Self { dir }
    }

    /// Writes `rel` in one step: the text goes to a temporary file that is then renamed over
    /// `rel`, so a `--watch` poll never sees the truncated file of a plain write.
    pub fn write(&self, rel: &str, text: &str) -> &Self {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let tmp = p.with_file_name(format!(
            ".{}.heki-test-write",
            p.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, &p).unwrap();
        self
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.join(rel)).unwrap()
    }

    /// Run `heki <args>` in the project directory: `(exit code, stdout, stderr)`.
    pub fn heki(&self, args: &[&str]) -> (i32, String, String) {
        let out = heki_command()
            .args(args)
            .current_dir(&self.dir)
            .output()
            .unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }

    /// `heki check --heuristic --fail-on warning <args>`: the rules these tests exercise are
    /// heuristic, and without it they run only when named (`--no-heuristic` in `args` turns it
    /// off again); many of them report warnings, and with `--fail-on warning` exit code 1 says
    /// that a run reported something.
    pub fn check(&self, args: &[&str]) -> (i32, String, String) {
        let mut all = vec!["check", "--heuristic", "--fail-on", "warning"];
        all.extend_from_slice(args);
        self.heki(&all)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `path` as heki's reports write it: with `/` separators on every platform.
/// The real `heki` binary, run as from a terminal wherever the tests run: without the variables
/// that choose `heki check`'s output format (`GITHUB_ACTIONS` is set in CI).
pub fn heki_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heki"));
    command
        .env_remove("GITHUB_ACTIONS")
        .env_remove("HEKI_OUTPUT_FORMAT");
    command
}

pub fn report_path(path: &Path) -> String {
    path.to_str()
        .unwrap()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn repo_file(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel)).unwrap()
}

/// The CPython used by the differential tests (`HEKI_TEST_PYTHON`, default `python3.15`).
/// The tests fail rather than skip when it is missing: the M6 gate runs on real CPython.
pub fn python() -> String {
    std::env::var("HEKI_TEST_PYTHON").unwrap_or_else(|_| "python3.15".into())
}

/// Run a Python script in `dir` and return its stdout (panics on a non-zero exit).
pub fn run_python(dir: &Path, script: &str) -> String {
    let py = python();
    let out = Command::new(&py)
        .arg(script)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{py}` (set HEKI_TEST_PYTHON): {e}"));
    assert!(
        out.status.success(),
        "{py} {script} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The code of the report `d` (`heki check --output-format json`) with its certainty where the
/// effect tests tell the tiers of one rule apart ([`with_tier`]).
pub fn tiered_code(d: &serde_json::Value) -> &str {
    with_tier(
        d["rule_code"].as_str().expect("a report has a code"),
        d["certainty"].as_str().expect("a report has a certainty"),
    )
}

/// `code` followed by ` [heuristic]`, as the concise output labels the report, for a heuristic
/// report of `EFF101` or `EFF103`, whose two tiers the effect tests tell apart; `code` alone
/// otherwise.
pub fn with_tier<'c>(code: &'c str, certainty: &str) -> &'c str {
    match (code, certainty) {
        ("EFF101", "heuristic") => "EFF101 [heuristic]",
        ("EFF103", "heuristic") => "EFF103 [heuristic]",
        _ => code,
    }
}

/// The code of a label [`with_tier`] writes: `EFF101` for `EFF101 [heuristic]`.
pub fn code_of(label: &str) -> &str {
    label.trim_end_matches(" [heuristic]")
}

/// `rules` with the label of the heuristic tier of `EFF101` and `EFF103` after each of them
/// ([`with_tier`]): the marks of a fixture name the tier its report must have
/// (`# EFF101 [heuristic]`).
pub fn with_tiers<'r>(rules: &[&'r str]) -> Vec<&'r str> {
    let mut out = Vec::new();
    for rule in rules {
        out.push(*rule);
        match *rule {
            "EFF101" => out.push("EFF101 [heuristic]"),
            "EFF103" => out.push("EFF103 [heuristic]"),
            _ => {}
        }
    }
    out
}

/// The note on what the contract candidates are; `fail_on` when no other note says that
/// `fail-on = "warning"` makes contract reports fail the run.
pub fn recommend_candidates_note(fail_on: bool) -> String {
    let fail_on = if fail_on {
        ", and set `fail-on = \"warning\"` so that contract reports fail `heki check`"
    } else {
        ""
    };
    format!(
        "the contract candidates are invariants the code keeps today, not architectural intent: \
         adopt only the boundaries you intend to keep, give each an actionable `message`{fail_on}; \
         consider a CODEOWNERS-protected heki.toml, and put the legacy violations of a stricter \
         wall in a Ratchet baseline, not in an `allowed-*` list."
    )
}
