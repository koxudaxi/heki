//! Shared helpers for the end-to-end tests: a throwaway project directory and the real `heki`
//! binary.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

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

    pub fn write(&self, rel: &str, text: &str) -> &Self {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
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

    /// `heki check <args>`.
    pub fn check(&self, args: &[&str]) -> (i32, String, String) {
        let mut all = vec!["check"];
        all.extend_from_slice(args);
        self.heki(&all)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
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
