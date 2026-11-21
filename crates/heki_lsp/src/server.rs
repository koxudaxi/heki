//! `heki server` / `heki lsp`: the stdio LSP transport over [`Workspace`]
//! (ARCHITECTURE.md section 8, docs/lsp.md).
//!
//! - `initialized`: index the workspace in the background, then publish diagnostics of every
//!   open document; register file watchers when the client supports them.
//! - `didOpen` / `didChange` / `didSave` / `didClose` change the documents in message order;
//!   the analyses run after, on the documents as they are then. Closing a document with
//!   unsaved changes re-analyzes the file from the disk.
//! - `didChange`: after a 250 ms debounce, re-analyze only the edited document (its summary
//!   update uses Early Cutoff); files it affects are queued.
//! - `didSave`: re-analyze the document, then every queued file and, in turn, what those
//!   affect (`Workspace::refresh_all`), and publish their diagnostics.
//! - `workspace/didChangeWatchedFiles`: files changed on the disk are re-analyzed like saved
//!   documents, deleted ones leave the project, and a change of the configuration or of an
//!   ignore file re-indexes it.
//! - `codeAction`: `OPT` fixes and `# heki: ignore[CODE]` insertions.
//! - `heki/stats`: engine counters (`EngineStats`), used to verify Early Cutoff, the number of
//!   open documents, and the work still in flight (`Activity`), which lets a client wait until
//!   every analysis has been published without timing assumptions.
//!
//! Publications are created under the state lock and sent after it; an older publication of a
//! file that a newer one replaced is dropped (`Backend::send`), so the client always ends with
//! the newest diagnostics of each file.

use crate::convert;
use crate::engine::{CONFIG_FILES, Selectors, Workspace};
use heki_diagnostics::Diagnostic;
use heki_effect::ModuleLayout;
use heki_rules::project::{IGNORE_FILES, ProjectOptions, canonical, find_root};
use rustc_hash::FxHashMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tower_lsp::jsonrpc::{Request, Result as RpcResult};
use tower_lsp::lsp_types::{
    CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    FileChangeType, FileEvent, FileSystemWatcher, GlobPattern, InitializeParams, InitializeResult,
    InitializedParams, MessageType, Registration, SaveOptions, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Url,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// Delay between the last `didChange` and the re-analysis of the document.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

struct Doc {
    uri: Url,
    text: String,
    version: i32,
}

#[derive(Default)]
struct State {
    /// The selection options of `heki server` over the configuration.
    selectors: Selectors,
    root: Option<PathBuf>,
    /// The client registers file watchers on request (`workspace/didChangeWatchedFiles`).
    watch_files: bool,
    workspace: Option<Workspace>,
    docs: FxHashMap<PathBuf, Doc>,
    /// Last published heki diagnostics per file (code actions read them).
    diags: FxHashMap<PathBuf, Published>,
    /// The client applies versioned document edits (`workspaceEdit.documentChanges`).
    versioned_edits: bool,
    /// Files to re-analyze on the next save.
    pending: BTreeSet<PathBuf>,
    /// Debounce tokens per document.
    generation: FxHashMap<PathBuf, u64>,
    /// The latest publication created for each file (`Publish::seq`): an older one that has
    /// not gone out yet is dropped, so the client ends with the newest diagnostics.
    latest_publication: FxHashMap<PathBuf, u64>,
    publications: u64,
}

/// The diagnostics last published for a file, and the version of the document they were
/// computed on (`None`: the file on the disk). Their fixes and suppressions are edits of that
/// text only.
struct Published {
    version: Option<i32>,
    diags: Vec<Diagnostic>,
}

/// Work the server has accepted but not finished (`heki/stats`: `in_flight`, `published`).
///
/// Every notification handler starts a [`Work`] before its first `await`, and notifications
/// are polled in the order they arrive, so a `heki/stats` request sent after a notification
/// always counts that notification's work. A unit ends after its diagnostics are published
/// (a debounced edit after its re-analysis, or when a newer edit supersedes it). When
/// `in_flight` is 0, nothing more is published until the client sends something new, and the
/// client has seen everything once it has received `published` notifications in total.
#[derive(Default)]
struct Activity {
    in_flight: AtomicUsize,
    published: AtomicU64,
    /// Publications go out one at a time (see `Backend::send`).
    gate: tokio::sync::Mutex<()>,
}

/// One unit of accepted work; dropping it (after its publications, or on a panic) ends it.
struct Work(Arc<Activity>);

impl Work {
    fn start(activity: &Arc<Activity>) -> Self {
        activity.in_flight.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(activity))
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One `publishDiagnostics` notification.
struct Publish {
    /// The file it is for, and its place among the publications created (`State::stamp`).
    path: PathBuf,
    seq: u64,
    uri: Url,
    diagnostics: Vec<tower_lsp::lsp_types::Diagnostic>,
    version: Option<i32>,
}

/// Absolute, symlink-resolved path of a document (the parent is resolved for unsaved files;
/// a path whose directory does not exist either is kept as it is).
fn normalize(path: &Path) -> PathBuf {
    canonical(path).unwrap_or_else(|_| {
        path.parent()
            .zip(path.file_name())
            .and_then(|(parent, name)| canonical(parent).ok().map(|p| p.join(name)))
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
        text_of(&self.docs, path)
    }

    /// The sequence number of a new publication for `path`, now its latest.
    fn stamp(&mut self, path: &Path) -> u64 {
        self.publications += 1;
        self.latest_publication
            .insert(path.to_path_buf(), self.publications);
        self.publications
    }

    fn uri_of(&self, path: &Path) -> Option<Url> {
        match self.docs.get(path) {
            Some(d) => Some(d.uri.clone()),
            None => Url::from_file_path(path).ok(),
        }
    }

    fn publish_for(&mut self, path: &Path, mut diags: Vec<Diagnostic>) -> Option<Publish> {
        let uri = self.uri_of(path)?;
        // Per file: the document always shows its own boundaries (docs/lsp.md).
        heki_rules::fold_root_causes(&mut diags);
        // Positions are converted with the text; a file without diagnostics needs none (it may
        // be gone).
        let text = if diags.is_empty() {
            String::new()
        } else {
            self.text_of(path)?
        };
        let lsp = diags
            .iter()
            .map(|d| convert::diagnostic(d, &text, &|p| self.text_of(p)))
            .collect();
        // Analyses run on the documents as they are under the state lock, so the version now
        // is the version analyzed.
        let version = self.docs.get(path).map(|d| d.version);
        self.diags
            .insert(path.to_path_buf(), Published { version, diags });
        Some(Publish {
            path: path.to_path_buf(),
            seq: self.stamp(path),
            uri,
            diagnostics: lsp,
            version,
        })
    }

    /// Index the workspace (again, when the configuration or an ignore file changed), then
    /// publish the diagnostics of every open document, of every file published before, and,
    /// when it is indexed again, of every file that entered the project (as a created file's
    /// are). Returns the number of indexed files (`None`: the configuration is invalid; nothing
    /// is analyzed).
    fn load(&mut self) -> (Vec<Publish>, Vec<String>, Option<usize>) {
        let root = self.root.clone().unwrap_or_else(|| PathBuf::from("."));
        let texts: FxHashMap<PathBuf, String> = self
            .docs
            .iter()
            .map(|(p, d)| (p.clone(), d.text.clone()))
            .collect();
        let paths = [root];
        let loaded = Workspace::load(
            &ProjectOptions {
                paths: &paths,
                working_dir: Some(&paths[0]),
                overrides: Some(&texts),
                ..ProjectOptions::default()
            },
            &self.selectors,
            None,
        );
        self.pending.clear();
        let mut files: BTreeSet<PathBuf> =
            self.docs.keys().chain(self.diags.keys()).cloned().collect();
        let indexed_before: Option<BTreeSet<PathBuf>> = self
            .workspace
            .as_ref()
            .map(|ws| ws.indexed().cloned().collect());
        let (mut ws, errors) = match loaded {
            Ok((ws, errors)) => (ws, errors.iter().map(ToString::to_string).collect()),
            Err(e) => {
                self.workspace = None;
                let pubs = files.iter().filter_map(|p| self.clear(p)).collect();
                return (pubs, vec![e.to_string()], None);
            }
        };
        let n = ws.stats().indexed_files;
        if let Some(before) = &indexed_before {
            files.extend(ws.indexed().filter(|p| !before.contains(*p)).cloned());
        }
        let checked: Vec<(PathBuf, Vec<Diagnostic>)> = files
            .into_iter()
            .map(|p| {
                let diags = match self.text_of(&p) {
                    Some(text) if ws.accepts(&p) => ws.diagnose(&p, &text).unwrap_or_default(),
                    _ => Vec::new(),
                };
                (p, diags)
            })
            .collect();
        self.workspace = Some(ws);
        let pubs = checked
            .into_iter()
            .filter_map(|(p, diags)| self.publish_for(&p, diags))
            .collect();
        (pubs, errors, Some(n))
    }

    /// Empty diagnostics for a file that left the project.
    fn clear(&mut self, path: &Path) -> Option<Publish> {
        self.diags.remove(path);
        Some(Publish {
            uri: self.uri_of(path)?,
            path: path.to_path_buf(),
            seq: self.stamp(path),
            diagnostics: Vec::new(),
            version: None,
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
        let outcome = ws.edit(path, text);
        // Analyzed against the current index: no queued re-analysis of it is left to do.
        self.pending.remove(path);
        self.pending.extend(outcome.affected);
        (
            self.publish_for(path, outcome.diagnostics),
            outcome.error.as_ref().map(ToString::to_string),
        )
    }

    /// Re-analyze every queued file and, in turn, what those re-analyses affect; a queued
    /// file that is neither open nor on the disk any more left the project.
    fn flush_pending(&mut self) -> (Vec<Publish>, Vec<String>) {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let ws = self
            .workspace
            .as_mut()
            .expect("files are queued only by an analysis of the loaded workspace");
        let docs = &self.docs;
        let results = ws.refresh_all(pending, &|p| text_of(docs, p));
        let (mut pubs, mut errors) = (Vec::new(), Vec::new());
        for (path, outcome) in results {
            match outcome {
                Some(outcome) => {
                    errors.extend(outcome.error.as_ref().map(ToString::to_string));
                    pubs.extend(self.publish_for(&path, outcome.diagnostics));
                }
                None => pubs.extend(self.clear(&path)),
            }
        }
        (pubs, errors)
    }

    /// Files changed on the disk (`workspace/didChangeWatchedFiles`): a created or changed
    /// file is re-analyzed like a saved document (an open document with its own text), a
    /// deleted one leaves the project; then everything they affect is re-analyzed.
    fn files_changed(&mut self, changes: Vec<FileEvent>) -> (Vec<Publish>, Vec<String>) {
        // The configuration, the summary snapshot, or an ignore file changed: everything is
        // analyzed again (an ignore file changes which files belong to the project), as
        // `heki check --watch` does.
        let workspace_root = self.root.as_deref().unwrap_or(Path::new("."));
        let config_root = find_root(workspace_root, workspace_root);
        let baseline = self
            .workspace
            .as_ref()
            .and_then(|ws| ws.baseline_path().map(Path::to_path_buf));
        let reloads = |p: &Path| {
            CONFIG_FILES.iter().any(|n| config_root.join(n) == p)
                || p.file_name()
                    .is_some_and(|n| IGNORE_FILES.iter().any(|i| n == *i))
                || baseline.as_deref() == Some(p)
        };
        if changes
            .iter()
            .any(|c| path_of(&c.uri).is_some_and(|p| reloads(&p)))
            || self.layout_changed()
        {
            let (pubs, errors, _) = self.load();
            return (pubs, errors);
        }
        let (mut pubs, mut errors) = (Vec::new(), Vec::new());
        let changes: Vec<(PathBuf, FileChangeType)> = changes
            .into_iter()
            .filter_map(|c| Some((path_of(&c.uri)?, c.typ)))
            .filter(|(path, _)| self.workspace.as_ref().is_some_and(|ws| ws.accepts(path)))
            .collect();
        for (path, typ) in changes {
            // An open document's text is the file's until it is closed (`State::closed`), also
            // when the file leaves the disk.
            if typ == FileChangeType::DELETED && self.docs.contains_key(&path) {
                continue;
            }
            if typ == FileChangeType::DELETED {
                let ws = self.workspace.as_mut().expect("checked above");
                let affected = ws.remove(&path);
                self.pending.extend(affected);
                pubs.extend(self.clear(&path));
            } else if let Some(text) = self.text_of(&path) {
                let (p, e) = self.reanalyze(&path, &text);
                pubs.extend(p);
                errors.extend(e);
            }
        }
        let (more, more_errors) = self.flush_pending();
        pubs.extend(more);
        errors.extend(more_errors);
        (pubs, errors)
    }

    /// Whether a package marker that appeared or disappeared (`src/__init__.py`, a root
    /// `__init__.py`) switched the module layout the workspace was indexed with
    /// (`ModuleLayout`): module names under `src/`, or relative imports climbing to the root,
    /// change with it.
    fn layout_changed(&self) -> bool {
        self.workspace
            .as_ref()
            .is_some_and(|ws| ModuleLayout::detect(&ws.env.root, &ws.env.cfg) != ws.env.layout)
    }

    /// A document was closed (`text`: what it held): the file on the disk is its text again.
    /// When they differ (an unsaved edit was discarded), the index forgets the editor's text: the
    /// file is re-analyzed from the disk, or leaves the project when it is not on the disk, and
    /// so is everything that affects. The document's diagnostics leave with it.
    fn closed(
        &mut self,
        path: &Path,
        uri: Url,
        text: Option<String>,
    ) -> (Vec<Publish>, Vec<String>) {
        let (mut pubs, mut errors) = (Vec::new(), Vec::new());
        let disk = heki_ast::read_source(path).ok();
        let indexed = self.workspace.as_ref().is_some_and(|ws| ws.accepts(path));
        if text.is_some() && text != disk && indexed {
            let ws = self.workspace.as_mut().expect("checked above");
            match &disk {
                Some(on_disk) => {
                    let outcome = ws.edit(path, on_disk);
                    self.pending.remove(path);
                    self.pending.extend(outcome.affected);
                    errors.extend(outcome.error.as_ref().map(ToString::to_string));
                }
                None => {
                    let affected = ws.remove(path);
                    self.pending.extend(affected);
                }
            }
            let (more, more_errors) = self.flush_pending();
            pubs.extend(more);
            errors.extend(more_errors);
        }
        self.diags.remove(path);
        pubs.push(Publish {
            seq: self.stamp(path),
            path: path.to_path_buf(),
            uri,
            diagnostics: Vec::new(),
            version: None,
        });
        (pubs, errors)
    }
}

/// An open document's text, else the file on the disk.
fn text_of(docs: &FxHashMap<PathBuf, Doc>, path: &Path) -> Option<String> {
    match docs.get(path) {
        Some(d) => Some(d.text.clone()),
        None => heki_ast::read_source(path).ok(),
    }
}

/// `workspace/didChangeWatchedFiles` registration for Python files and the configuration.
fn watched_files_registration() -> Registration {
    let watcher = |glob: String| FileSystemWatcher {
        glob_pattern: GlobPattern::String(glob),
        kind: None,
    };
    let options = DidChangeWatchedFilesRegistrationOptions {
        watchers: vec![
            watcher("**/*.{py,pyi}".into()),
            watcher(format!("**/{{{}}}", CONFIG_FILES.join(","))),
            watcher(format!("**/{{{}}}", IGNORE_FILES.join(","))),
            // Ratchet baselines (any name; the configuration says which).
            watcher("**/*.toml".into()),
        ],
    };
    Registration {
        id: "heki-watched-files".into(),
        method: "workspace/didChangeWatchedFiles".into(),
        register_options: serde_json::to_value(options).ok(),
    }
}

pub struct Backend {
    client: Client,
    state: Arc<Mutex<State>>,
    activity: Arc<Activity>,
    /// The client sent `shutdown`: the process exits with 0 (else 1) when the session ends.
    shut_down: Arc<AtomicBool>,
}

impl Backend {
    fn new(client: Client, selectors: Selectors) -> Self {
        Self {
            client,
            state: Arc::new(Mutex::new(State {
                selectors,
                ..State::default()
            })),
            activity: Arc::default(),
            shut_down: Arc::default(),
        }
    }

    /// Log `errors` and send `pubs`. Publications are created under the state lock but sent
    /// after it, so one gate sends them one at a time and drops any that a newer publication
    /// of the same file replaced: the client ends with the newest diagnostics of each file.
    async fn send(
        work: &Work,
        client: &Client,
        state: &Mutex<State>,
        pubs: Vec<Publish>,
        errors: Vec<String>,
    ) {
        for e in errors {
            client
                .log_message(MessageType::WARNING, format!("heki: {e}"))
                .await;
        }
        let _gate = work.0.gate.lock().await;
        let current = |p: &Publish| lock(state).latest_publication.get(&p.path) == Some(&p.seq);
        for p in pubs.into_iter().filter(current) {
            client
                .publish_diagnostics(p.uri, p.diagnostics, p.version)
                .await;
            work.0.published.fetch_add(1, Ordering::SeqCst);
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

    /// `heki/stats`: engine counters plus whether the index is ready, how many documents are
    /// open (a client can tell when an earlier `didOpen` has been applied), and the work in
    /// flight (see [`Activity`]).
    async fn stats(&self) -> RpcResult<serde_json::Value> {
        // `in_flight` first: work that has ended has counted its publications already.
        let in_flight = self.activity.in_flight.load(Ordering::SeqCst);
        let published = self.activity.published.load(Ordering::SeqCst);
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
        v["in_flight"] = serde_json::json!(in_flight);
        v["published"] = serde_json::json!(published);
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
            .or_else(|| std::env::current_dir().ok().map(|dir| normalize(&dir)));
        let workspace = params.capabilities.workspace;
        let watch_files = workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files)
            .and_then(|w| w.dynamic_registration)
            .unwrap_or(false);
        let versioned_edits = workspace
            .and_then(|w| w.workspace_edit)
            .and_then(|e| e.document_changes)
            .unwrap_or(false);
        let mut st = lock(&self.state);
        st.root = root;
        st.watch_files = watch_files;
        st.versioned_edits = versioned_edits;
        drop(st);
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
        let work = Work::start(&self.activity);
        if lock(&self.state).watch_files {
            // Files created, changed, or deleted outside the editor (`git checkout`, code
            // generators) reach the index through these events. A client that refuses the
            // registration sends none; its documents are still analyzed.
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client
                    .register_capability(vec![watched_files_registration()])
                    .await;
            });
        }
        let state = Arc::clone(&self.state);
        let client = self.client.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let (pubs, errors, indexed) = Self::blocking(&state, State::load).await;
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
            Self::send(&work, &client, &state, pubs, errors).await;
        });
    }

    async fn shutdown(&self) -> RpcResult<()> {
        self.shut_down.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let work = Work::start(&self.activity);
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        // Documents change in message order (a `didChange` or `didClose` right after this one
        // applies after it); only the analysis runs later, on the document as it is then.
        lock(&self.state).docs.insert(
            path.clone(),
            Doc {
                uri: params.text_document.uri,
                text: params.text_document.text,
                version: params.text_document.version,
            },
        );
        let (pubs, errors) = Self::blocking(&self.state, move |st| {
            let text = st.docs.get(&path).map(|d| d.text.clone());
            let (p, e) = text
                .map(|text| st.reanalyze(&path, &text))
                .unwrap_or_default();
            (p.into_iter().collect(), e.into_iter().collect())
        })
        .await;
        Self::send(&work, &self.client, &self.state, pubs, errors).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let work = Work::start(&self.activity);
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
            Self::send(&work, &client, &state, pubs, errors).await;
        });
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let work = Work::start(&self.activity);
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        if let (Some(text), Some(doc)) = (params.text, lock(&self.state).docs.get_mut(&path)) {
            doc.text = text;
        }
        let (pubs, errors) = Self::blocking(&self.state, move |st| {
            if st.layout_changed() {
                let (pubs, errors, _) = st.load();
                return (pubs, errors);
            }
            let Some(text) = st.text_of(&path) else {
                return (Vec::new(), Vec::new());
            };
            let (p, e) = st.reanalyze(&path, &text);
            let (mut pubs, mut errors): (Vec<Publish>, Vec<String>) =
                (p.into_iter().collect(), e.into_iter().collect());
            let (more, more_errors) = st.flush_pending();
            pubs.extend(more);
            errors.extend(more_errors);
            (pubs, errors)
        })
        .await;
        Self::send(&work, &self.client, &self.state, pubs, errors).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let work = Work::start(&self.activity);
        let (pubs, errors) =
            Self::blocking(&self.state, move |st| st.files_changed(params.changes)).await;
        Self::send(&work, &self.client, &self.state, pubs, errors).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let work = Work::start(&self.activity);
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        let closed = lock(&self.state).docs.remove(&path).map(|d| d.text);
        let uri = params.text_document.uri;
        let (pubs, errors) =
            Self::blocking(&self.state, move |st| st.closed(&path, uri, closed)).await;
        Self::send(&work, &self.client, &self.state, pubs, errors).await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let Some(path) = path_of(&params.text_document.uri) else {
            return Ok(None);
        };
        let st = lock(&self.state);
        let (Some(doc), Some(published)) = (st.docs.get(&path), st.diags.get(&path)) else {
            return Ok(None);
        };
        // Edits computed on another version of the document (an edit is waiting for its
        // debounced analysis) would land on the wrong text.
        if published.version != Some(doc.version) {
            return Ok(None);
        }
        let lsp: Vec<_> = published
            .diags
            .iter()
            .map(|d| convert::diagnostic(d, &doc.text, &|p| st.text_of(p)))
            .collect();
        let target = convert::EditTarget {
            uri: &doc.uri,
            version: st.versioned_edits.then_some(doc.version),
        };
        let actions =
            convert::code_actions(&target, &doc.text, &published.diags, &lsp, &params.range);
        Ok(Some(
            actions
                .into_iter()
                .map(CodeActionOrCommand::CodeAction)
                .collect(),
        ))
    }
}

/// The service, ending the session at the `exit` notification: tower-lsp stops serving only at
/// the next message or the end of stdin, and a client may keep stdin open after `exit`.
struct EndAtExit<S> {
    inner: S,
    exit: Arc<tokio::sync::Notify>,
}

impl<S: tower_service::Service<Request>> tower_service::Service<Request> for EndAtExit<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let exit = request.method() == "exit";
        let handled = self.inner.call(request);
        if exit {
            self.exit.notify_one();
        }
        handled
    }
}

/// Serve LSP over stdin / stdout until `exit` (or the end of stdin), with `selectors` over the
/// configuration of the workspace. Returns whether the session ended as the protocol asks, after
/// `shutdown`: the exit code is then 0, else 1.
pub fn run_stdio(selectors: Selectors) -> std::io::Result<bool> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let shut_down = runtime.block_on(async {
        let (service, socket) = LspService::build(|client| Backend::new(client, selectors))
            .custom_method("heki/stats", Backend::stats)
            .finish();
        let shut_down = Arc::clone(&service.inner().shut_down);
        let exit = Arc::new(tokio::sync::Notify::new());
        let service = EndAtExit {
            inner: service,
            exit: Arc::clone(&exit),
        };
        tokio::select! {
            () = Server::new(tokio::io::stdin(), tokio::io::stdout(), socket).serve(service) => {}
            () = exit.notified() => {}
        }
        shut_down.load(Ordering::SeqCst)
    });
    // `tokio::io::stdin` reads on a blocking thread; do not wait for it.
    runtime.shutdown_background();
    Ok(shut_down)
}
