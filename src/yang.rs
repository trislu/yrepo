//! `Yang`: the *syntactic* view of one parsed document ([D1]).
//!
//! Holds the parsed CST + `Statement` tree (via `syntax::ParsedDoc`) and the
//! extracted module/submodule header used by the resolution graph. No
//! cross-file semantics live here.

use std::ops::Range;
use std::sync::Arc;

use crate::syntax::{self, ParseError, Statement, StatementKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    Module,
    Submodule,
}

/// A top-level `import` statement.
#[derive(Debug, Clone)]
pub struct Import {
    pub module: String,
    pub prefix: String,
    pub revision: Option<String>,
    /// Whole `import` statement span (for diagnostics).
    pub range: Range<usize>,
    /// Argument (module name) span.
    pub arg: Range<usize>,
}

/// A top-level `include` statement.
#[derive(Debug, Clone)]
pub struct Include {
    pub name: String,
    pub revision: Option<String>,
    pub range: Range<usize>,
    pub arg: Range<usize>,
}

/// The parsed, header-extracted view of a single document.
pub struct Yang {
    pub url: Arc<str>,
    pub parsed: syntax::ParsedDoc,
    pub kind: Option<UnitKind>,
    pub name: Option<String>,
    pub namespace: Option<String>,
    /// The module's own prefix, or (for a submodule) the `belongs-to` prefix.
    pub own_prefix: Option<String>,
    /// For submodules: `(parent module name, prefix used for the parent)`.
    pub belongs_to: Option<(String, String)>,
    /// Latest `revision` date (ISO strings compare lexicographically).
    pub revision: Option<String>,
    pub imports: Vec<Import>,
    pub includes: Vec<Include>,
    pub parse_errors: Vec<ParseError>,
}

impl Yang {
    pub(crate) fn new(url: Arc<str>, source: String) -> Yang {
        Self::new_opt(url, source, false)
    }

    /// Like [`Yang::new`], but with `light` the Statement tree skips
    /// text-only statements and tokens drop their quoted runs (see
    /// `syntax::parse_opt`). Opt-in for memory-light serving.
    pub(crate) fn new_opt(url: Arc<str>, source: String, light: bool) -> Yang {
        let parsed = if light {
            syntax::parse_opt(source, true)
        } else {
            syntax::parse(source)
        };
        let header = extract_header(parsed.root.as_ref());
        let parse_errors = parsed.parse_errors.clone();
        Yang {
            url,
            parsed,
            kind: header.kind,
            name: header.name,
            namespace: header.namespace,
            own_prefix: header.own_prefix,
            belongs_to: header.belongs_to,
            revision: header.revision,
            imports: header.imports,
            includes: header.includes,
            parse_errors,
        }
    }

    pub fn source(&self) -> &str {
        self.parsed.text.source()
    }

    pub fn root(&self) -> Option<&Statement> {
        self.parsed.root.as_ref()
    }
}

fn arg_name(stmt: &Statement) -> Option<String> {
    stmt.arg.as_ref().map(|a| a.name().to_string())
}

pub(crate) struct Header {
    pub(crate) kind: Option<UnitKind>,
    pub(crate) name: Option<String>,
    pub(crate) namespace: Option<String>,
    pub(crate) own_prefix: Option<String>,
    pub(crate) belongs_to: Option<(String, String)>,
    pub(crate) revision: Option<String>,
    pub(crate) imports: Vec<Import>,
    pub(crate) includes: Vec<Include>,
}

/// Extract the module/submodule header facts from a statement root. Shared by
/// the full `Yang` path and the header-only `Catalog::scan` path, so both see
/// an identical `Statement` shape.
pub(crate) fn extract_header(root: Option<&Statement>) -> Header {
    let root = match root {
        Some(r) => r,
        None => {
            return Header {
                kind: None,
                name: None,
                namespace: None,
                own_prefix: None,
                belongs_to: None,
                revision: None,
                imports: Vec::new(),
                includes: Vec::new(),
            };
        }
    };

    let kind = match root.kind {
        StatementKind::Module => Some(UnitKind::Module),
        StatementKind::Submodule => Some(UnitKind::Submodule),
        _ => None,
    };
    let name = arg_name(root);

    let mut namespace = None;
    let mut module_prefix = None;
    let mut belongs_to = None;
    let mut revisions = Vec::new();
    let mut imports = Vec::new();
    let mut includes = Vec::new();

    for child in &root.children {
        match child.kind {
            StatementKind::Namespace => namespace = arg_name(child),
            StatementKind::Prefix => module_prefix = arg_name(child),
            StatementKind::Revision => {
                if let Some(d) = arg_name(child) {
                    revisions.push(d);
                }
            }
            StatementKind::BelongsTo => {
                let parent = arg_name(child);
                let mut pfx = None;
                for cc in &child.children {
                    if cc.kind == StatementKind::Prefix {
                        pfx = arg_name(cc);
                    }
                }
                if let Some(parent) = parent {
                    belongs_to = Some((parent, pfx.unwrap_or_default()));
                }
            }
            StatementKind::Import => {
                if let Some(module) = arg_name(child) {
                    let mut pfx = None;
                    let mut rev = None;
                    for cc in &child.children {
                        match cc.kind {
                            StatementKind::Prefix => pfx = arg_name(cc),
                            StatementKind::RevisionDate => rev = arg_name(cc),
                            _ => {}
                        }
                    }
                    if let Some(prefix) = pfx {
                        imports.push(Import {
                            module,
                            prefix,
                            revision: rev,
                            range: child.range.clone(),
                            arg: child
                                .arg
                                .as_ref()
                                .map(|a| a.range.clone())
                                .unwrap_or_else(|| child.range.clone()),
                        });
                    }
                }
            }
            StatementKind::Include => {
                if let Some(name) = arg_name(child) {
                    let mut rev = None;
                    for cc in &child.children {
                        if cc.kind == StatementKind::RevisionDate {
                            rev = arg_name(cc);
                        }
                    }
                    includes.push(Include {
                        name,
                        revision: rev,
                        range: child.range.clone(),
                        arg: child
                            .arg
                            .as_ref()
                            .map(|a| a.range.clone())
                            .unwrap_or_else(|| child.range.clone()),
                    });
                }
            }
            _ => {}
        }
    }

    let revision = revisions.iter().max().cloned();
    // A submodule's prefix, for references to the *parent* module, is the
    // `belongs-to` prefix.
    let own_prefix = match kind {
        Some(UnitKind::Module) => module_prefix,
        Some(UnitKind::Submodule) => belongs_to.as_ref().map(|(_, p)| p.clone()),
        None => module_prefix,
    };

    Header {
        kind,
        name,
        namespace,
        own_prefix,
        belongs_to,
        revision,
        imports,
        includes,
    }
}
