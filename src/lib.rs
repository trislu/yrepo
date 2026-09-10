//! # yrepo
//!
//! LSP-friendly YANG repository: parse, resolve, and query YANG modules
//! (`*.yang`).
//!
//! The two public entry points ([D1]):
//! * [`Repository`] — manages the open documents of a workspace
//!   (`upsert`/`remove` by url) and compiles them.
//! * [`Library`] — the resolved snapshot returned by
//!   [`Repository::compile`], queried for modules/submodules/symbols/nodes.
//!
//! User-content problems are reported as [`Diagnostic`]s and never as
//! `Result::Err` ([D3]).
//!
//! ```
//! use yrepo::Repository;
//!
//! let mut repo = Repository::new();
//! repo.upsert("/mods/m.yang", "module m { namespace \"urn:m\"; prefix m;\n  leaf x { type string; }\n}");
//! let outcome = repo.compile();
//! assert!(outcome.diagnostics.is_empty());
//! let lib = outcome.library.expect("module compiled");
//! let m = lib.module("m").expect("module found");
//! assert_eq!(m.top_nodes().len(), 1);
//! ```

mod catalog;
mod compile;
mod diag;
mod fragment;
mod grouping_topo;
mod library;
mod refidx;
mod schema;
mod syntax;
mod text;
mod value;
mod yang;

pub use crate::catalog::{
    Catalog, CatalogImport, CatalogIndex, PathIndex, build_closure_repository,
};
pub use crate::diag::{Diagnostic, DiagnosticCode, Location, Severity};
pub use crate::library::{IdentityStatus, Library, Outcome};
pub use crate::refidx::ReferenceIndex;
pub use crate::schema::{
    AppliedAugment, AppliedDeviation, DeviationOp, ExtensionDef, FeatureDef, Grouping, Identity,
    IdentityRef, IdentityResolution, ImportInfo, ModuleRecord, NodeId, NodeKind, SchemaNode,
    SubmoduleRecord, TypeCandidate, TypeCandidateKind, TypeResolution, TypeStep, Typedef,
};
pub use crate::syntax::{
    Argument, Comment, CommentKind, Statement, StatementEnd, StatementKind, Token, TokenKind,
    TokenSpot,
};
pub use crate::value::{TypeFacets, ValueType};
pub use crate::yang::{Import, Include, UnitKind};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::compile::BuildOutcome;
use crate::yang::Yang;

/// Manages the open documents of a workspace and compiles them into a
/// [`Library`].
pub struct Repository {
    docs: Vec<Yang>,
    by_url: HashMap<String, usize>,
    text_light: bool,
}

impl Default for Repository {
    fn default() -> Self {
        Self::new()
    }
}

impl Repository {
    pub fn new() -> Repository {
        Repository {
            docs: Vec::new(),
            by_url: HashMap::new(),
            text_light: false,
        }
    }

    /// Opt-in memory-light parsing: text-only statements
    /// (description/reference/organization/contact) are dropped from the
    /// Statement tree and their quoted runs from the token stream. Schema
    /// resolution and LSP semantics are unaffected; default OFF.
    pub fn set_text_light(&mut self, light: bool) {
        self.text_light = light;
    }

    /// Insert or replace the document at `url` with `source` and parse it.
    ///
    /// Never fails on malformed content — syntax problems surface later as
    /// [`Diagnostic`]s.
    pub fn upsert(&mut self, url: impl Into<String>, source: impl Into<String>) {
        let url = url.into();
        let parsed = Yang::new_opt(Arc::from(url.as_str()), source.into(), self.text_light);
        self.commit(url, parsed);
    }

    /// Read each file at `path` and insert or replace the document at `url`,
    /// letting the repository read the files itself.
    ///
    /// Each file is read **and** parsed in parallel when the `parallel` cargo
    /// feature is enabled (a plain sequential loop otherwise). Only the file a
    /// worker is currently processing is in memory, so the whole workspace is
    /// never buffered as text up front and the peak stays flat however many
    /// documents are ingested.
    ///
    /// A file that cannot be read (missing, unreadable, not valid UTF-8) is
    /// skipped, never an error. Documents are committed in `iter` order, so
    /// this produces exactly the same `Library` and diagnostics as the
    /// equivalent sequence of [`Repository::upsert`] calls (minus any document
    /// whose file could not be read). Returns how many documents were inserted
    /// or replaced.
    pub fn upsert_many_files<I, U, P>(&mut self, iter: I) -> usize
    where
        I: IntoIterator<Item = (U, P)>,
        U: Into<String>,
        P: AsRef<Path>,
    {
        let items: Vec<(String, PathBuf)> = iter
            .into_iter()
            .map(|(url, path)| (url.into(), path.as_ref().to_path_buf()))
            .collect();
        // Read + parse off-thread (feature `parallel`); results stay in `items`
        // order, with unreadable files dropped in place.
        let light = self.text_light;
        let parsed = crate::compile::map_par(&items, |(url, path)| {
            let source = std::fs::read_to_string(path).ok()?;
            Some((
                url.clone(),
                Yang::new_opt(Arc::from(url.as_str()), source, light),
            ))
        });
        let mut committed = 0usize;
        for (url, doc) in parsed.into_iter().flatten() {
            self.commit(url, doc);
            committed += 1;
        }
        committed
    }

    /// Store a parsed document at `url` (replace an existing entry or append).
    fn commit(&mut self, url: String, parsed: Yang) {
        match self.by_url.get(&url) {
            Some(&i) => self.docs[i] = parsed,
            None => {
                self.by_url.insert(url, self.docs.len());
                self.docs.push(parsed);
            }
        }
    }

    /// Remove the document at `url`. Returns `true` if it was present.
    pub fn remove(&mut self, url: &str) -> bool {
        if self.by_url.remove(url).is_some() {
            self.docs.retain(|d| d.url.as_ref() != url);
            // fix indices after removal
            for (j, doc) in self.docs.iter().enumerate() {
                self.by_url.insert(doc.url.to_string(), j);
            }
            true
        } else {
            false
        }
    }

    pub fn contains(&self, url: &str) -> bool {
        self.by_url.contains_key(url)
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// The urls of every open document (arbitrary order). Used by callers
    /// that reconcile a repository against a needed set (serving modes).
    pub fn urls(&self) -> Vec<&str> {
        self.by_url.keys().map(String::as_str).collect()
    }

    /// Compile the whole workspace into a fresh [`Library`] snapshot, with
    /// diagnostics for every document.
    ///
    /// The returned `Library` is `None` only when no module could be compiled
    /// (e.g. an empty workspace or no module document).
    pub fn compile(&self) -> Outcome {
        let refs: Vec<&Yang> = self.docs.iter().collect();
        let BuildOutcome {
            modules,
            submodules,
            diagnostics,
        } = compile::build(&refs);
        let library = if modules.is_empty() {
            None
        } else {
            Some(Arc::new(Library::from_parts(modules, submodules)))
        };
        Outcome {
            library,
            diagnostics,
        }
    }

    /// What is under the caret at `(row, col)` (0-based) in the document at
    /// `url`? Returns the narrowest enclosing statement and where the caret
    /// falls within it.
    pub fn token_at(&self, url: &str, row: usize, col: usize) -> Option<TokenHit> {
        let doc = self.docs.get(*self.by_url.get(url)?)?;
        let byte = doc.parsed.text.linecol_to_byte(row, col)?;
        let root = doc.root()?;
        let stmt = root.narrowest_at(byte)?;
        let (spot, _) = TokenSpot::of(stmt, byte);
        Some(TokenHit {
            statement: stmt.kind.clone(),
            spot,
        })
    }

    /// The root `module`/`submodule` statement of the document at `url` — the
    /// head of its whole statement tree. Walk it with
    /// [`Statement::children`] / [`Statement::preorder`]; every node carries
    /// its [`Statement::kind`], keyword/argument spans and logical argument
    /// text. Returns `None` when the url is unknown or the document is not a
    /// YANG module/submodule.
    ///
    /// This is the *syntactic* (unresolved) view — for cross-file, resolved
    /// semantics use [`Repository::compile`] and the resulting [`Library`].
    pub fn statement(&self, url: &str) -> Option<&Statement> {
        let doc = self.docs.get(*self.by_url.get(url)?)?;
        doc.root()
    }

    /// The narrowest statement under the caret at `(row, col)` (0-based) in the
    /// document at `url` — a reference into the live document tree, unlike
    /// [`Repository::token_at`], so callers can read the statement's argument
    /// string (`.arg`), keyword/statement spans, and children. This is the
    /// entry point for precise goto/hover (caret in an `import`/`uses`/`type`/
    /// `augment` argument) and for statement completion.
    pub fn statement_at(&self, url: &str, row: usize, col: usize) -> Option<&Statement> {
        let doc = self.docs.get(*self.by_url.get(url)?)?;
        let byte = doc.parsed.text.linecol_to_byte(row, col)?;
        doc.root()?.narrowest_at(byte)
    }

    /// Comments in the document at `url`, in source order.
    ///
    /// The [`Statement`] tree models only statements, so comments (which live
    /// between statements and inside block bodies) are exposed here instead —
    /// format must never delete them, highlight colors them, and comment-out
    /// quick-fixes target them. Each [`Comment`] carries its byte `range` and
    /// raw `text` (markers included). Returns `None` when `url` is unknown.
    pub fn comments(&self, url: &str) -> Option<&[Comment]> {
        let doc = self.docs.get(*self.by_url.get(url)?)?;
        Some(&doc.parsed.comments)
    }

    /// Raw lexical tokens in the document at `url`, in source order.
    ///
    /// A superset of [`Repository::comments`]: every CST leaf the grammar
    /// produces that the [`Statement`] tree drops — keywords, identifiers,
    /// quoted-string runs, numeric literals, `true`/`false`, the `+` concat
    /// operator, comments and punctuation — each with a byte `range` and its
    /// raw `text`. Highlight can use this for grammar-precise literal coloring
    /// inside composite arguments (numbers in `range`/`value`, quoted runs,
    /// `+`, …). Returns `None` when `url` is unknown.
    pub fn tokens(&self, url: &str) -> Option<&[Token]> {
        let doc = self.docs.get(*self.by_url.get(url)?)?;
        Some(&doc.parsed.tokens)
    }
}

/// The answer to a caret (positional) query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenHit {
    /// The narrowest statement under the caret.
    pub statement: StatementKind,
    /// Whether the caret is over the keyword, an argument, or elsewhere.
    pub spot: TokenSpot,
}
