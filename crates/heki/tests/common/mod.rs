//! Shared helpers for the end-to-end tests: a throwaway project directory, the real `heki`
//! binary, the time after which a test calls it hung, and an LSP client (`lsp`).

#![allow(dead_code)]

pub mod lsp;
pub mod schema;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

/// How long a test waits for `heki` (a run, a reply or report of the language server, a report
/// of `--watch`) before it fails as hung. It bounds a hang, never a cost: a debug build on a
/// loaded CI runner takes many times longer than on an idle machine, so a limit close to the
/// usual time fails correct runs. `e2e_analysis_bounds.rs` bounds the cost of a shape by its
/// growth from a smaller one instead.
pub const HANG: Duration = Duration::from_secs(300);

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

impl Project {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("heki_e2e_{name}_{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
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
        let out = Command::new(env!("CARGO_BIN_EXE_heki"))
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

/// `rules` with the heuristic sibling of each effect rule among them (`EFF111` for `EFF101`,
/// `EFF113` for `EFF103`): a report of the claim the exact rule does not establish is its
/// sibling's.
#[allow(dead_code)]
pub fn with_siblings<'r>(rules: &[&'r str]) -> Vec<&'r str> {
    let mut out = Vec::new();
    for rule in rules {
        out.push(*rule);
        match *rule {
            "EFF101" => out.push("EFF111"),
            "EFF103" => out.push("EFF113"),
            _ => {}
        }
    }
    out
}

/// [`with_siblings`] of a comma-separated selection.
#[allow(dead_code)]
pub fn select_with_siblings(select: &str) -> String {
    with_siblings(&select.split(',').collect::<Vec<_>>()).join(",")
}
