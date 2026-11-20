//! Incremental LSP server for `heki server` / `heki lsp`, and the engine behind
//! `heki check --watch` (ADR-017, ARCHITECTURE.md section 8).
//!
//! - The background workspace index keeps `heki_effect::ProjectIndex` (lightweight
//!   `FileSummary` + reverse call graph) and drops IR bodies.
//! - `didChange` (250 ms debounce) re-analyzes only the edited file; `ProjectIndex::update_file`
//!   reports Early Cutoff, and the affected files are re-analyzed on `didSave`.
//! - Call-chain hops become `DiagnosticRelatedInformation`; `OPT` edits become `quickfix` /
//!   `refactor.rewrite.heki` code actions; every diagnostic offers `# heki: ignore[CODE]`.

pub mod convert;
pub mod engine;
pub mod server;

pub use engine::{EditOutcome, EngineError, EngineStats, Selectors, Workspace};
pub use server::run_stdio;

#[cfg(test)]
mod invariant_tests {
    /// ADR-017: the resident state (`Workspace`, the server's `State`) never holds IR or
    /// semantic models — only summaries, metadata, and open documents' text.
    #[test]
    fn resident_state_holds_no_ir() {
        for (file, structs) in [
            (
                include_str!("engine.rs"),
                &["pub struct Workspace", "struct FileMeta"][..],
            ),
            (
                include_str!("server.rs"),
                &["struct State", "struct Doc"][..],
            ),
        ] {
            for name in structs {
                let start = file.find(&format!("{name} {{")).expect(name);
                let body = &file[start..start + file[start..].find("\n}").unwrap()];
                for forbidden in [
                    "ModuleIr",
                    "IrFunction",
                    "ParsedFile",
                    "SemanticModel",
                    "Project ",
                ] {
                    assert!(
                        !body.contains(forbidden),
                        "ADR-017 VIOLATION: `{name}` keeps `{forbidden}` resident"
                    );
                }
            }
        }
    }
}
