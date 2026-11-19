//! Incremental LSP server architecture scaffold for `heki server` / `heki lsp`.
//!
//! Design (ADR-017):
//! - Background workspace index builds `heki_effect::ProjectIndex` (retaining lightweight
//!   `FileSummary` + reverse call graph, dropping IR bodies).
//! - On `didChange` (250ms debounce): re-parse/re-analyze only the edited file (~5-20ms).
//! - On `didSave` or summary change: `ProjectIndex::update_file` applies Early Cutoff so
//!   only affected callers on the reverse call graph are re-evaluated.
//! - Exposes call-chain hops as LSP `DiagnosticRelatedInformation` and safe/size-dependent
//!   `OPT` edits as Quick Fix / `refactor.rewrite.heki` Code Actions.
//!
//! Milestone M7 wires `tower-lsp` + `tokio` transport onto this engine.

use heki_effect::{FileSummary, IncrementalUpdate, ProjectIndex};
use heki_env::HekiConfig;

/// Stateful incremental engine backing the LSP server.
#[derive(Debug, Default)]
pub struct IncrementalEngine {
    pub config: HekiConfig,
    pub index: ProjectIndex,
}

impl IncrementalEngine {
    pub fn new(config: HekiConfig) -> Self {
        Self {
            config,
            index: ProjectIndex::default(),
        }
    }

    /// Apply a single-file edit and return Early Cutoff propagation metadata.
    pub fn apply_file_summary(&mut self, summary: FileSummary) -> IncrementalUpdate {
        self.index.update_file(summary)
    }
}
