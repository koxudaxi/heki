//! `heki server` / `heki lsp`: the stdio LSP transport over [`Workspace`]
//! (ARCHITECTURE.md section 8, docs/lsp.md).
//!
//! - `initialized`: index the workspace in the background, then publish diagnostics of every
//!   open document.
//! - `didChange`: after a 250 ms debounce, re-analyze only the edited document (its summary
//!   update uses Early Cutoff); files it affects are queued.
//! - `didSave`: re-analyze the document, then every queued file (callers of changed summaries,
//!   importers of changed declarations), and publish their diagnostics.
//! - `codeAction`: `OPT` fixes and `# heki: ignore[CODE]` insertions.
//! - `heki/stats`: engine counters (`EngineStats`), used to verify Early Cutoff, and the number
//!   of open documents.

use crate::convert;
use crate::engine::{Selectors, Workspace};
use heki_diagnostics::Diagnostic;
use heki_rules::project::ProjectOptions;
use rustc_hash::FxHashMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::{
    CodeActionKind, CodeActionOptions, CodeActionParams, CodeActionProviderCapability,
    CodeActionResponse, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, MessageType, SaveOptions, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Url,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// Delay between the last `didChange` and the re-analysis of the document.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// Time the client gets after `shutdown` to send `exit` and close stdin.
const EXIT_GRACE: Duration = Duration::from_secs(2);

struct Doc {
    uri: Url,
    text: String,
    version: i32,
}

#[derive(Default)]
struct State {
    root: Option<PathBuf>,
    workspace: Option<Workspace>,
    docs: FxHashMap<PathBuf, Doc>,
    /// Last published heki diagnostics per file (code actions read them).
    diags: FxHashMap<PathBuf, Vec<Diagnostic>>,
    /// Files to re-analyze on the next save.
    pending: BTreeSet<PathBuf>,
    /// Debounce tokens per document.
    generation: FxHashMap<PathBuf, u64>,
}

/// One `publishDiagnostics` notification.
struct Publish {
    uri: Url,
    diagnostics: Vec<tower_lsp::lsp_types::Diagnostic>,
    version: Option<i32>,
}

/// Absolute, symlink-resolved path of a document (the parent is resolved for unsaved files;
/// a path whose directory does not exist either is kept as it is).
fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .zip(path.file_name())
            .and_then(|(parent, name)| std::fs::canonicalize(parent).ok().map(|p| p.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

fn path_of(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok().map(|p| normalize(&p))
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl State {
    fn text_of(&self, path: &Path) -> Option<String> {
        match self.docs.get(path) {
            Some(d) => Some(d.text.clone()),
            None => std::fs::read_to_string(path).ok(),
        }
    }

    fn uri_of(&self, path: &Path) -> Option<Url> {
        match self.docs.get(path) {
            Some(d) => Some(d.uri.clone()),
            None => Url::from_file_path(path).ok(),
        }
    }

    fn publish_for(&mut self, path: &Path, diags: Vec<Diagnostic>) -> Option<Publish> {
        let uri = self.uri_of(path)?;
        let text = self.text_of(path)?;
        let lsp = diags
            .iter()
            .map(|d| convert::diagnostic(d, &text, &|p| self.text_of(p)))
            .collect();
        self.diags.insert(path.to_path_buf(), diags);
        Some(Publish {
            uri,
            diagnostics: lsp,
            version: self.docs.get(path).map(|d| d.version),
        })
    }

    /// Re-analyze one document (or saved file) and queue the files it affects.
    fn reanalyze(&mut self, path: &Path, text: &str) -> (Option<Publish>, Option<String>) {
        let Some(ws) = self.workspace.as_mut() else {
            return (None, None);
        };
        if !ws.accepts(path) {
            return (self.publish_for(path, Vec::new()), None);
        }
        match ws.edit(path, text) {
            Ok(outcome) => {
                self.pending.extend(outcome.affected);
                (self.publish_for(path, outcome.diagnostics), None)
            }
            Err(e) => (self.publish_for(path, Vec::new()), Some(e)),
        }
    }

    /// Re-analyze every queued file except `skip`.
    fn flush_pending(&mut self, skip: &Path) -> (Vec<Publish>, Vec<String>) {
        let pending = std::mem::take(&mut self.pending);
        let (mut pubs, mut errors) = (Vec::new(), Vec::new());
        for p in pending.into_iter().filter(|p| p != skip) {
            let Some(text) = self.text_of(&p) else {
                continue;
            };
            let ws = self
                .workspace
                .as_mut()
                .expect("files are queued only by an analysis of the loaded workspace");
            match ws.refresh(&p, &text) {
                Ok(d) => pubs.extend(self.publish_for(&p, d)),
                Err(e) => errors.push(e),
            }
        }
        (pubs, errors)
    }
}

pub struct Backend {
    client: Client,
    state: Arc<Mutex<State>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    async fn send(client: &Client, pubs: Vec<Publish>, errors: Vec<String>) {
        for e in errors {
            client
                .log_message(MessageType::WARNING, format!("heki: {e}"))
                .await;
        }
        for p in pubs {
            client
                .publish_diagnostics(p.uri, p.diagnostics, p.version)
                .await;
        }
    }

    /// Run `f` on the state in a blocking task. A panic in `f` yields `T::default()` (nothing
    /// to publish).
    async fn blocking<T: Default + Send + 'static>(
        state: &Arc<Mutex<State>>,
        f: impl FnOnce(&mut State) -> T + Send + 'static,
    ) -> T {
        let state = Arc::clone(state);
        tokio::task::spawn_blocking(move || f(&mut lock(&state)))
            .await
            .unwrap_or_default()
    }

    /// `heki/stats`: engine counters plus whether the index is ready and how many documents
    /// are open (a client can tell when an earlier `didOpen` has been applied).
    async fn stats(&self) -> RpcResult<serde_json::Value> {
        let st = lock(&self.state);
        let mut v = match &st.workspace {
            Some(ws) => {
                let mut v = serde_json::to_value(ws.stats()).unwrap_or_default();
                v["ready"] = serde_json::Value::Bool(true);
                v
            }
            None => serde_json::json!({ "ready": false }),
        };
        v["open_documents"] = serde_json::json!(st.docs.len());
        Ok(v)
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        #[allow(deprecated)]
        let root = params
            .workspace_folders
            .as_ref()
            .and_then(|f| f.first())
            .map(|f| f.uri.clone())
            .or(params.root_uri)
            .and_then(|u| path_of(&u))
            .or_else(|| std::env::current_dir().ok());
        lock(&self.state).root = root;
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::new(convert::REWRITE_KIND),
                        ]),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "heki".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let state = Arc::clone(&self.state);
        let client = self.client.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let (pubs, errors, indexed) = Self::blocking(&state, |st| {
                let root = st.root.clone().unwrap_or_else(|| PathBuf::from("."));
                let texts: FxHashMap<PathBuf, String> = st
                    .docs
                    .iter()
                    .map(|(p, d)| (p.clone(), d.text.clone()))
                    .collect();
                let paths = [root];
                let loaded = Workspace::load(
                    &ProjectOptions {
                        paths: &paths,
                        overrides: Some(&texts),
                        ..ProjectOptions::default()
                    },
                    &Selectors::default(),
                );
                match loaded {
                    Ok((ws, errors)) => {
                        let n = ws.stats().indexed_files;
                        let open: Vec<(PathBuf, Vec<Diagnostic>)> = st
                            .docs
                            .iter()
                            .map(|(p, doc)| {
                                let diags = if ws.accepts(p) {
                                    ws.diagnose(p, &doc.text).unwrap_or_default()
                                } else {
                                    Vec::new()
                                };
                                (p.clone(), diags)
                            })
                            .collect();
                        st.workspace = Some(ws);
                        let pubs = open
                            .into_iter()
                            .filter_map(|(p, diags)| st.publish_for(&p, diags))
                            .collect();
                        (pubs, errors, Some(n))
                    }
                    Err(e) => (Vec::new(), vec![e.to_string()], None),
                }
            })
            .await;
            if let Some(n) = indexed {
                client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "heki: indexed {n} file(s) in {} ms",
                            started.elapsed().as_millis()
                        ),
                    )
                    .await;
            }
            Self::send(&client, pubs, errors).await;
        });
    }

    async fn shutdown(&self) -> RpcResult<()> {
        // `exit` only ends the transport once the client closes stdin; a client that keeps it
        // open would leave the process behind, so end it shortly after `shutdown`.
        tokio::spawn(async {
            tokio::time::sleep(EXIT_GRACE).await;
            std::process::exit(0);
        });
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        let (pubs, errors) = Self::blocking(&self.state, move |st| {
            let text = params.text_document.text;
            st.docs.insert(
                path.clone(),
                Doc {
                    uri: params.text_document.uri,
                    text: text.clone(),
                    version: params.text_document.version,
                },
            );
            let (p, e) = st.reanalyze(&path, &text);
            (p.into_iter().collect(), e.into_iter().collect())
        })
        .await;
        Self::send(&self.client, pubs, errors).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        let generation = {
            let mut st = lock(&self.state);
            let Some(doc) = st.docs.get_mut(&path) else {
                return;
            };
            if params.text_document.version < doc.version {
                return;
            }
            if let Some(change) = params.content_changes.into_iter().last() {
                doc.text = change.text;
            }
            doc.version = params.text_document.version;
            let g = st.generation.entry(path.clone()).or_default();
            *g += 1;
            *g
        };
        let state = Arc::clone(&self.state);
        let client = self.client.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DEBOUNCE).await;
            let (pubs, errors) = Self::blocking(&state, move |st| {
                if st.generation.get(&path) != Some(&generation) {
                    return (Vec::new(), Vec::new());
                }
                let Some(text) = st.docs.get(&path).map(|d| d.text.clone()) else {
                    return (Vec::new(), Vec::new());
                };
                let (p, e) = st.reanalyze(&path, &text);
                (p.into_iter().collect(), e.into_iter().collect())
            })
            .await;
            Self::send(&client, pubs, errors).await;
        });
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        let (pubs, errors) = Self::blocking(&self.state, move |st| {
            if let (Some(text), Some(doc)) = (params.text, st.docs.get_mut(&path)) {
                doc.text = text;
            }
            let Some(text) = st.text_of(&path) else {
                return (Vec::new(), Vec::new());
            };
            let (p, e) = st.reanalyze(&path, &text);
            let (mut pubs, mut errors): (Vec<Publish>, Vec<String>) =
                (p.into_iter().collect(), e.into_iter().collect());
            let (more, more_errors) = st.flush_pending(&path);
            pubs.extend(more);
            errors.extend(more_errors);
            (pubs, errors)
        })
        .await;
        Self::send(&self.client, pubs, errors).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        {
            let mut st = lock(&self.state);
            st.docs.remove(&path);
            st.diags.remove(&path);
        }
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let Some(path) = path_of(&params.text_document.uri) else {
            return Ok(None);
        };
        let st = lock(&self.state);
        let (Some(doc), Some(diags)) = (st.docs.get(&path), st.diags.get(&path)) else {
            return Ok(None);
        };
        let lsp: Vec<_> = diags
            .iter()
            .map(|d| convert::diagnostic(d, &doc.text, &|p| st.text_of(p)))
            .collect();
        Ok(Some(convert::code_actions(
            &doc.uri,
            &doc.text,
            diags,
            &lsp,
            &params.range,
        )))
    }
}

/// Serve LSP over stdin / stdout until `exit`.
pub fn run_stdio() -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let (service, socket) = LspService::build(Backend::new)
            .custom_method("heki/stats", Backend::stats)
            .finish();
        Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
            .serve(service)
            .await;
    });
    // `tokio::io::stdin` reads on a blocking thread; do not wait for it.
    runtime.shutdown_background();
    Ok(())
}
