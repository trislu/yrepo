//! Syntax layer: the only place that touches tree-sitter ([D2]).
//!
//! We parse a document into a `ParsedDoc` that owns the raw CST (`Tree`) and a
//! derived, tree-sitter-independent `Statement` tree. The `Statement` tree is
//! what every downstream layer consumes; the raw `Tree` is kept for
//! positional (caret) queries.
//!
//! [D2]: see `docs/architecture.md` decision log.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use tree_sitter::{Node, Parser};

use crate::text::Text;

/// Re-exported so callers do not need to depend on tree-sitter types.
pub use tree_sitter_yang::LANGUAGE;

pub(crate) fn new_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&LANGUAGE.into())
        .expect("tree-sitter-yang grammar is valid");
    parser
}

/// The YANG keyword (statement) a node represents.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatementKind {
    Module,
    Submodule,
    Namespace,
    Prefix,
    YangVersion,
    Import,
    Include,
    BelongsTo,
    Revision,
    RevisionDate,
    Organization,
    Contact,
    Description,
    Reference,
    Container,
    Leaf,
    LeafList,
    List,
    Choice,
    Case,
    Anyxml,
    Anydata,
    Uses,
    Grouping,
    Typedef,
    Type,
    Identity,
    Base,
    Augment,
    UsesAugment,
    Refine,
    Deviation,
    DeviateAdd,
    DeviateDelete,
    DeviateReplace,
    DeviateNotSupported,
    Rpc,
    Action,
    Notification,
    Input,
    Output,
    Config,
    Mandatory,
    Presence,
    Default,
    MinElements,
    MaxElements,
    OrderedBy,
    Status,
    When,
    IfFeature,
    Must,
    Units,
    FractionDigits,
    Range,
    Length,
    Pattern,
    Enum,
    Bit,
    Path,
    RequireInstance,
    Value,
    Position,
    Unique,
    Key,
    Feature,
    Extension,
    Argument,
    YinElement,
    ErrorMessage,
    ErrorAppTag,
    Modifier,
    /// An extension statement we do not model (carries the raw node type).
    Unknown(String),
}

impl StatementKind {
    pub fn is_data_node(&self) -> bool {
        matches!(
            self,
            StatementKind::Container
                | StatementKind::Leaf
                | StatementKind::LeafList
                | StatementKind::List
                | StatementKind::Choice
                | StatementKind::Case
                | StatementKind::Anyxml
                | StatementKind::Anydata
                | StatementKind::Action
                | StatementKind::Notification
        )
    }

    pub fn is_schema_body_stmt(&self) -> bool {
        matches!(
            self,
            StatementKind::Container
                | StatementKind::Leaf
                | StatementKind::LeafList
                | StatementKind::List
                | StatementKind::Choice
                | StatementKind::Anyxml
                | StatementKind::Anydata
                | StatementKind::Uses
                | StatementKind::Action
                | StatementKind::Notification
        )
    }
}

impl StatementKind {
    /// Derive the kind from a tree-sitter node type, e.g. `"container_stmt"`.
    pub fn from_node_type(node_type: &str) -> Option<Self> {
        let base = node_type.strip_suffix("_stmt")?;
        use StatementKind as K;
        Some(match base {
            "module" => K::Module,
            "submodule" => K::Submodule,
            "namespace" => K::Namespace,
            "prefix" => K::Prefix,
            "yang_version" => K::YangVersion,
            "import" => K::Import,
            "include" => K::Include,
            "belongs_to" => K::BelongsTo,
            "revision" => K::Revision,
            "revision_date" => K::RevisionDate,
            "organization" => K::Organization,
            "contact" => K::Contact,
            "description" => K::Description,
            "reference" => K::Reference,
            "container" => K::Container,
            "leaf" => K::Leaf,
            "leaf_list" => K::LeafList,
            "list" => K::List,
            "choice" => K::Choice,
            "case" => K::Case,
            "anyxml" => K::Anyxml,
            "anydata" => K::Anydata,
            "uses" => K::Uses,
            "grouping" => K::Grouping,
            "typedef" => K::Typedef,
            "type" => K::Type,
            "identity" => K::Identity,
            "base" => K::Base,
            "augment" => K::Augment,
            "uses_augment" => K::UsesAugment,
            "refine" => K::Refine,
            "deviation" => K::Deviation,
            "deviate_add" => K::DeviateAdd,
            "deviate_delete" => K::DeviateDelete,
            "deviate_replace" => K::DeviateReplace,
            "deviate_not_supported" => K::DeviateNotSupported,
            "rpc" => K::Rpc,
            "action" => K::Action,
            "notification" => K::Notification,
            "input" => K::Input,
            "output" => K::Output,
            "config" => K::Config,
            "mandatory" => K::Mandatory,
            "presence" => K::Presence,
            "default" => K::Default,
            "min_elements" => K::MinElements,
            "max_elements" => K::MaxElements,
            "ordered_by" => K::OrderedBy,
            "status" => K::Status,
            "when" => K::When,
            "if_feature" => K::IfFeature,
            "must" => K::Must,
            "units" => K::Units,
            "fraction_digits" => K::FractionDigits,
            "range" => K::Range,
            "length" => K::Length,
            "pattern" => K::Pattern,
            "enum" => K::Enum,
            "bit" => K::Bit,
            "path" => K::Path,
            "require_instance" => K::RequireInstance,
            "value" => K::Value,
            "position" => K::Position,
            "unique" => K::Unique,
            "key" => K::Key,
            "feature" => K::Feature,
            "extension" => K::Extension,
            "argument" => K::Argument,
            "yin_element" => K::YinElement,
            "error_message" => K::ErrorMessage,
            "error_app_tag" => K::ErrorAppTag,
            "modifier" => K::Modifier,
            _ => K::Unknown(base.to_string()),
        })
    }
}

/// The logical argument of a statement: dequoted and `+`-fragments joined
/// (D4), together with the byte range it occupied in the source.
#[derive(Debug, Clone)]
pub struct Argument {
    pub range: Range<usize>,
    pub logical: String,
}

impl Argument {
    /// The argument as a bare identifier (already dequoted).
    pub fn name(&self) -> &str {
        self.logical.trim()
    }

    /// The argument normalized for path-like use: whitespace and quotes removed.
    pub fn path(&self) -> String {
        let mut out = String::with_capacity(self.logical.len());
        for c in self.logical.chars() {
            if !c.is_whitespace() && c != '"' && c != '\'' {
                out.push(c);
            }
        }
        out
    }
}

/// Where and how a statement terminates in the source.
///
/// Captured at parse time so syntax consumers (folding, formatting,
/// comment-out quick-fixes) can locate the exact terminator without re-scanning
/// the source text — [`Statement::range`] covers the whole grammar node, which
/// may extend past the terminator over inter-statement whitespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementEnd {
    /// The statement is a leaf ending in `;`. `semi` is the byte span of the
    /// `;` token itself.
    Semicolon { semi: Range<usize> },
    /// The statement carries a `{ … }` body (possibly empty). `open`/`close`
    /// are the byte spans of the two brace tokens; the body content lives
    /// strictly between `open.end` and `close.start`.
    Braces {
        open: Range<usize>,
        close: Range<usize>,
    },
}

impl StatementEnd {
    /// The byte span the statement truly occupies: keyword start .. terminator
    /// end (past the `;`, or past the closing `}`).
    pub fn span(&self, keyword_start: usize) -> Range<usize> {
        let end = match self {
            StatementEnd::Semicolon { semi } => semi.end,
            StatementEnd::Braces { close, .. } => close.end,
        };
        keyword_start..end
    }
}

/// One statement of a YANG document, independent of tree-sitter.
#[derive(Debug, Clone)]
pub struct Statement {
    pub kind: StatementKind,
    /// Whole grammar-node span (keyword .. end of the statement node). The end
    /// may extend past the `;`/`}` over trailing inter-statement whitespace;
    /// use [`Statement::end`] / [`Statement::span`] for the exact terminator.
    pub range: Range<usize>,
    /// Span of the keyword token, when present.
    pub keyword: Option<Range<usize>>,
    pub arg: Option<Argument>,
    /// How the statement terminates (`;` leaf vs `{ … }` block), when the
    /// parse recovered a terminator.
    pub end: Option<StatementEnd>,
    /// Nested statements, in source order.
    pub children: Vec<Statement>,
}

impl Statement {
    /// Direct children of the given kinds.
    pub fn find(&self, kinds: &[StatementKind]) -> Vec<&Statement> {
        self.children
            .iter()
            .filter(|c| kinds.contains(&c.kind))
            .collect()
    }

    pub fn find_one(&self, kind: StatementKind) -> Option<&Statement> {
        self.children.iter().find(|c| c.kind == kind)
    }

    /// Deepest statement whose span contains `byte`.
    pub fn narrowest_at(&self, byte: usize) -> Option<&Statement> {
        if !self.range.contains(&byte) {
            return None;
        }
        // Prefer the deepest child that also contains the byte.
        let mut best = self;
        loop {
            let mut deeper = None;
            for c in &best.children {
                if c.range.contains(&byte) {
                    deeper = Some(c);
                    break;
                }
            }
            match deeper {
                Some(c) => best = c,
                None => break,
            }
        }
        Some(best)
    }

    /// True when this statement has a `{ … }` body (including an empty one).
    pub fn is_block(&self) -> bool {
        matches!(self.end, Some(StatementEnd::Braces { .. }))
    }

    /// Byte span of the block body content, braces excluded: `open.end ..
    /// close.start`. `None` for `;`-terminated (leaf) statements.
    pub fn body(&self) -> Option<Range<usize>> {
        match &self.end {
            Some(StatementEnd::Braces { open, close }) => Some(open.end..close.start),
            _ => None,
        }
    }

    /// The byte span the statement truly occupies, keyword start .. terminator
    /// end (see [`StatementEnd::span`]). Falls back to [`Statement::range`]
    /// when no terminator was recovered.
    pub fn span(&self) -> Range<usize> {
        let start = self.keyword.as_ref().map_or(self.range.start, |k| k.start);
        self.end
            .as_ref()
            .map(|e| e.span(start))
            .unwrap_or_else(|| self.range.clone())
    }

    /// Pre-order (depth-first) iteration over this statement and all of its
    /// descendants, in source order.
    pub fn preorder(&self) -> impl Iterator<Item = &Statement> {
        let mut stack = vec![self];
        std::iter::from_fn(move || {
            let next = stack.pop()?;
            stack.extend(next.children.iter().rev());
            Some(next)
        })
    }
}

/// A source comment: a `//` line comment or a `/* … */` block comment.
///
/// The statement tree models only statements ([D2]); comments live *between*
/// statements (and inside block bodies), so they are exposed separately as a
/// document-ordered list via [`Repository::comments`]. Both the byte `range`
/// (including the `//` / `/* … */` markers) and the raw `text` are provided so
/// format, comment-out quick-fixes and highlight can preserve or re-indent
/// them verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Byte span of the whole comment, markers included.
    pub range: Range<usize>,
    /// Whether this is a `//` line comment or a `/* … */` block comment.
    pub kind: CommentKind,
    /// The raw comment text, markers included.
    pub text: String,
}

/// Whether a [`Comment`] is a `//` line comment or a `/* … */` block comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    /// `//` to end of line.
    Line,
    /// `/* … */`, which may span several lines.
    Block,
}

/// A category for one raw lexical token of the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenKind {
    /// A `//` line comment or `/* … */` block comment (same leaves as [`Comment`]).
    Comment,
    /// A statement keyword (e.g. `container`, `leaf`, `import`).
    Keyword,
    /// An identifier (module/prefix/node names, and parts of `prefix:name` refs).
    Identifier,
    /// A quoted string run (`"…"` or `'…'`); range includes the quotes.
    String,
    /// An unquoted numeric literal (integer or decimal).
    Number,
    /// `true` / `false`.
    Boolean,
    /// The `+` string-concatenation operator.
    Operator,
    /// Any other leaf (punctuation like `;` `{` `}` `:` `/`, dates, …).
    Other,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        use TokenKind::*;
        match self {
            Comment => "comment",
            Keyword => "keyword",
            Identifier => "identifier",
            String => "string",
            Number => "number",
            Boolean => "boolean",
            Operator => "operator",
            Other => "other",
        }
    }
}

/// One raw lexical token of the grammar, in source order.
///
/// The grammar's CST leaves that the [`Statement`] tree deliberately drops:
/// keywords, identifiers, quoted-string runs, numeric literals, `true`/`false`,
/// the `+` concatenation operator, comments, and punctuation. See
/// [`Repository::tokens`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte span of the token in the document.
    pub range: Range<usize>,
    /// The token's raw text (quoted-string ranges include the quotes).
    pub text: String,
}

/// Result of parsing one source document.
pub struct ParsedDoc {
    pub text: Text,
    /// Root module/submodule statement, if the document is a YANG document.
    pub root: Option<Statement>,
    /// Comments in source order (the statement tree does not model them).
    pub comments: Vec<Comment>,
    /// Raw lexical tokens in source order (superset of [`Comment`]s).
    pub tokens: Vec<Token>,
    /// Syntax problems recovered by tree-sitter (never fatal).
    pub parse_errors: Vec<ParseError>,
    /// True when the CST carried no `ERROR`/`MISSING` node. Always available,
    /// including in [`ParseMode::HeaderOnly`] where `parse_errors` is not
    /// collected (it is exactly `parse_errors.is_empty()` elsewhere).
    pub parse_ok: bool,
    /// Names extracted by a [`ParseMode::Summary`] parse (`None` in every
    /// other mode): the root's direct top-level schema-body children.
    pub(crate) summary: Option<SummaryScan>,
}

/// Names of a module/submodule root's direct top-level schema-body children,
/// collected by a [`ParseMode::Summary`] parse without building any deeper
/// statement. See [`crate::summary::ModuleSummary`].
#[derive(Debug, Clone, Default)]
pub(crate) struct SummaryScan {
    /// Top-level data node names (`container`/`leaf`/`leaf-list`/`list`/
    /// `anyxml`/`anydata`), in source order.
    pub(crate) top_data: Vec<String>,
    /// Top-level `rpc` names, in source order.
    pub(crate) rpcs: Vec<String>,
    /// Top-level `notification` names, in source order.
    pub(crate) notifications: Vec<String>,
    /// Top-level `identity` declarations as `(name, base argument)`, in source
    /// order. The base is still the raw argument (a prefix/bare name); the
    /// summary resolves it against the declaring file's prefix scope.
    pub(crate) identities: Vec<(String, Option<String>)>,
}

/// A recovered syntax error.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub range: Range<usize>,
    pub message: String,
}

/// How much of a document a parse extracts.
///
/// The catalog path only needs the header facts (`extract_header`), so it
/// parses in [`ParseMode::HeaderOnly`] and never pays for the full statement
/// tree, the token/comment streams, or the quoted-fragment recovery that only
/// token consumers need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseMode {
    /// Full extraction: statement tree, comments, tokens, parse errors.
    Full,
    /// Text-light: full statement tree minus the prose-only statements, and
    /// tokens without their quoted-string runs. No fragment recovery.
    Light,
    /// Catalog header only: the module/submodule root plus the header
    /// statements `extract_header` reads (name, prefix, namespace,
    /// belongs-to, imports with revision-date, includes, revision). No
    /// comments, tokens, or parse errors, and no fragment recovery.
    HeaderOnly,
    /// Module summary: the [`ParseMode::HeaderOnly`] root, plus the names of
    /// the root's direct top-level schema-body children — top-level data nodes
    /// (`container`/`leaf`/`leaf-list`/`list`/`anyxml`/`anydata`) and
    /// top-level `rpc`/`notification`. No comments, tokens, parse errors or
    /// deeper statements, and no fragment recovery.
    Summary,
}

pub(crate) fn parse(source: String) -> ParsedDoc {
    parse_with(source, ParseMode::Full)
}

/// Like [`parse`], but with `light` set the Statement tree skips the pure-text
/// statements (description/reference/organization/contact) and the token
/// stream drops their quoted-string runs. Text-light documents are much
/// lighter to retain while every schema/LSP feature behaves identically;
/// default OFF (opt-in, e.g. giant-workspace catalog+closure serving).
pub(crate) fn parse_opt(source: String, light: bool) -> ParsedDoc {
    parse_with(
        source,
        if light {
            ParseMode::Light
        } else {
            ParseMode::Full
        },
    )
}

pub(crate) fn parse_with(source: String, mode: ParseMode) -> ParsedDoc {
    let text = Text::new(Arc::from(source.as_str()));
    let mut parser = new_parser();
    // The raw CST is only needed while the views below are extracted; the
    // Statement/comments/tokens/parse-error views retain everything later
    // phases and the language server consume. Dropping the Tree here removes
    // the largest per-document retention (a whole tree-sitter CST per doc).
    let tree = parser
        .parse(&source, None)
        .expect("tree-sitter parse yields a tree");

    if mode == ParseMode::HeaderOnly || mode == ParseMode::Summary {
        // Catalog/summary scan: only the root + header statements survive; no
        // comments, tokens or errors are collected. `has_error` is an O(1)
        // subtree flag and equals "an ERROR/MISSING node exists". Summary mode
        // additionally records the root's direct top-level data/rpc/
        // notification names from the transient CST.
        let root_node = tree.root_node();
        let parse_ok = !root_node.has_error();
        let top = find_top_module(root_node);
        let summary = (mode == ParseMode::Summary).then(|| {
            top.map(|n| scan_summary_names(n, &text))
                .unwrap_or_default()
        });
        let root = top.map(|n| build_header_statement(n, &text));
        drop(tree);
        return ParsedDoc {
            text,
            root,
            comments: Vec::new(),
            tokens: Vec::new(),
            parse_errors: Vec::new(),
            parse_ok,
            summary,
        };
    }

    let parse_errors = collect_errors(tree.root_node(), &text);
    let parse_ok = parse_errors.is_empty();
    let comments = collect_comments(tree.root_node(), &text);
    let light = mode == ParseMode::Light;
    let excluded = if light {
        text_statement_ranges(tree.root_node())
    } else {
        Vec::new()
    };
    let mut tokens = collect_tokens(tree.root_node(), &text);
    let root = find_top_module(tree.root_node()).map(|n| build_statement(n, &text, light));
    if light {
        // Text-light retention model: drop the prose-only quoted runs. Keep
        // the fragment augmentation off too — nothing highlights light views.
        tokens.retain(|t| !excluded.iter().any(|r| r.contains(&t.range.start)));
    } else {
        // Full documents: recover quoted fragments the CST-leaf walk missed
        // (hidden-token pieces of concatenated arguments).
        augment_quoted_fragments(&root, &text, &mut tokens);
    }
    drop(tree);
    ParsedDoc {
        text,
        root,
        comments,
        tokens,
        parse_errors,
        parse_ok,
        summary: None,
    }
}

/// Find the `module_stmt`/`submodule_stmt` node, descending through an
/// optional grammar wrapper (e.g. a top-level `yang` node).
fn find_top_module(node: Node) -> Option<Node> {
    match node.kind() {
        "module_stmt" | "submodule_stmt" => Some(node),
        _ => {
            for i in 0..node.child_count() {
                if let Some(c) = node.child(i as u32)
                    && let Some(found) = find_top_module(c)
                {
                    return Some(found);
                }
            }
            None
        }
    }
}

/// CST statement kinds that carry only prose text (no schema semantics).
fn is_text_statement_kind(kind: &str) -> bool {
    matches!(
        kind,
        "description_stmt" | "reference_stmt" | "organization_stmt" | "contact_stmt"
    )
}

/// Byte ranges of every text-only statement in `node`'s subtree (for the
/// text-light token filter).
fn text_statement_ranges(node: Node) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    fn visit(n: Node, out: &mut Vec<std::ops::Range<usize>>) {
        if is_text_statement_kind(n.kind()) {
            out.push(n.start_byte()..n.end_byte());
            return;
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i as u32) {
                visit(c, out);
            }
        }
    }
    visit(node, &mut out);
    out
}

/// Direct CST children of `node`, in order.
fn children_of(node: Node) -> Vec<Node> {
    (0..node.child_count())
        .filter_map(|i| node.child(i as u32))
        .collect()
}

/// The statement fields that do not depend on how children were selected
/// (kind, keyword span, argument, terminator) plus the already-built child
/// statements.
fn statement_parts(
    node: Node,
    children: &[Node],
    text: &Text,
    child_stmts: Vec<Statement>,
) -> Statement {
    let node_type = node.kind().to_string();
    let kind = StatementKind::from_node_type(&node_type)
        .unwrap_or(StatementKind::Unknown(node_type.clone()));

    let keyword = {
        let kw = format!("{}_keyword", node_type.trim_end_matches("_stmt"));
        // Exact `<stmt>_keyword` first; fall back to any `*_keyword` child.
        // The grammar reuses shared keyword rules for some statements (e.g.
        // `deviate_add_stmt`/`uses_augment_stmt` both alias the plain
        // `deviate`/`augment` keyword), so the exact name is not always present.
        children
            .iter()
            .find(|c| c.kind() == kw)
            .or_else(|| children.iter().find(|c| c.kind().ends_with("_keyword")))
            .map(|c| c.start_byte()..c.end_byte())
            .or_else(|| {
                // Extension statements (`unknown_stmt`) have no `*_keyword`
                // child; their head `prefix:name` acts as the keyword.
                if node_type != "unknown_stmt" {
                    return None;
                }
                let start = children.iter().find(|c| c.kind() == "prefix")?.start_byte();
                let end = children
                    .iter()
                    .find(|c| c.kind() == "identifier")?
                    .end_byte();
                Some(start..end)
            })
    };

    let arg = find_arg(node, children, text);
    let end = statement_end(children);

    Statement {
        kind,
        range: node.start_byte()..node.end_byte(),
        keyword,
        arg,
        end,
        children: child_stmts,
    }
}

fn build_statement(node: Node, text: &Text, light: bool) -> Statement {
    let children = children_of(node);
    let child_stmts: Vec<Statement> = children
        .iter()
        .filter(|c| c.kind().ends_with("_stmt"))
        .filter(|c| !(light && is_text_statement_kind(c.kind())))
        .map(|c| build_statement(*c, text, light))
        .collect();
    statement_parts(node, &children, text, child_stmts)
}

/// CST statement kinds that live in a module/submodule header and are read by
/// `yang::extract_header`.
fn is_header_statement_kind(kind: &str) -> bool {
    matches!(
        kind,
        "namespace_stmt"
            | "prefix_stmt"
            | "belongs_to_stmt"
            | "import_stmt"
            | "include_stmt"
            | "revision_stmt"
    )
}

/// Header sub-statements: the `prefix`/`revision-date` bodies an `import`,
/// `include` or `belongs-to` may carry.
fn is_header_substatement_kind(kind: &str) -> bool {
    matches!(kind, "prefix_stmt" | "revision_date_stmt")
}

/// Header-only build of the root statement: keep the module/submodule root
/// and, of its children, only the header statements (`extract_header` reads
/// name, prefix, namespace, belongs-to, imports, includes, revision), and of
/// those only their `prefix`/`revision-date` sub-statements. The produced
/// `Statement` shape is identical to [`build_statement`]'s, so header
/// extraction sees exactly the same fields without building the whole tree.
fn build_header_statement(node: Node, text: &Text) -> Statement {
    let children = children_of(node);
    let child_stmts: Vec<Statement> = children
        .iter()
        .filter(|c| c.kind().ends_with("_stmt") && is_header_statement_kind(c.kind()))
        .map(|c| {
            let sub = children_of(*c);
            let sub_stmts: Vec<Statement> = sub
                .iter()
                .filter(|s| s.kind().ends_with("_stmt") && is_header_substatement_kind(s.kind()))
                .map(|s| statement_parts(*s, &children_of(*s), text, Vec::new()))
                .collect();
            statement_parts(*c, &sub, text, sub_stmts)
        })
        .collect();
    statement_parts(node, &children, text, child_stmts)
}

/// Walk the *direct* CST statement children of a module/submodule root and
/// record top-level data-node, `rpc` and `notification` names ([`ParseMode::Summary`]).
///
/// Only the root's immediate `*_stmt` children are inspected: nothing deeper is
/// built, so a summary parse stays close to a header-only parse in cost.
fn scan_summary_names(root: Node, text: &Text) -> SummaryScan {
    let mut out = SummaryScan::default();
    for child in children_of(root) {
        let kind = child.kind();
        if !kind.ends_with("_stmt") {
            continue;
        }
        match kind {
            "container_stmt" | "leaf_stmt" | "leaf_list_stmt" | "list_stmt" | "anyxml_stmt"
            | "anydata_stmt" => push_arg(&mut out.top_data, child, text),
            "rpc_stmt" => push_arg(&mut out.rpcs, child, text),
            "notification_stmt" => push_arg(&mut out.notifications, child, text),
            "identity_stmt" => {
                let Some(name) = arg_of(child, text) else {
                    continue;
                };
                let base = children_of(child)
                    .into_iter()
                    .find(|c| c.kind() == "base_stmt")
                    .and_then(|b| arg_of(b, text));
                out.identities.push((name, base));
            }
            _ => {}
        }
    }
    out
}

/// The non-empty argument of a statement node, if any.
fn arg_of(node: Node, text: &Text) -> Option<String> {
    find_arg(node, &children_of(node), text)
        .map(|a| a.name().to_string())
        .filter(|n| !n.is_empty())
}

/// Push a statement's argument onto `bucket`, if it has a non-empty one.
fn push_arg(bucket: &mut Vec<String>, node: Node, text: &Text) {
    if let Some(name) = arg_of(node, text) {
        bucket.push(name);
    }
}

/// Recover how a statement terminates from its direct CST children.
///
/// The grammar emits the body braces `{`/`}` (for block statements) or the `;`
/// as anonymous direct children of the `*_stmt` node. A statement whose parse
/// did not recover a terminator (missing `;`/`}` on error) yields `None`.
fn statement_end(children: &[Node]) -> Option<StatementEnd> {
    let mut semi: Option<Range<usize>> = None;
    let mut open: Option<Range<usize>> = None;
    let mut close: Option<Range<usize>> = None;
    for c in children {
        if c.is_missing() || c.is_error() {
            continue;
        }
        let span = c.start_byte()..c.end_byte();
        match c.kind() {
            ";" => semi = Some(span),
            "{" => open = Some(span),
            "}" => close = Some(span),
            _ => {}
        }
    }
    match (open, close) {
        (Some(open), Some(close)) => Some(StatementEnd::Braces { open, close }),
        (None, None) => semi.map(|semi| StatementEnd::Semicolon { semi }),
        // Only one brace recovered: leave terminator unknown rather than guess.
        _ => None,
    }
}

/// Find the argument of a statement.
///
/// tree-sitter exposes the argument as the grammar `arg` field when declared;
/// otherwise it is the child whose type ends in `_arg_str`.
fn find_arg(node: Node, children: &[Node], text: &Text) -> Option<Argument> {
    let arg_node = node
        .child_by_field_name("arg")
        .filter(|a| !a.is_missing() && !a.is_error())
        .or_else(|| {
            children
                .iter()
                .find(|c| c.kind().ends_with("_arg_str"))
                .copied()
        });
    let arg_node = arg_node?;
    let range = arg_node.start_byte()..arg_node.end_byte();
    let raw = text.slice(range.clone());
    let logical = logical_argument(raw);
    Some(Argument { range, logical })
}

/// Reconstruct the *logical* argument text: strip surrounding quotes, join
/// `+`-concatenated fragments, drop whitespace that lives *outside* quotes.
///
/// Quoting semantics follow RFC 7950 §6.1.3: **single-quoted** strings perform
/// no escape processing (a backslash is an ordinary character), while
/// **double-quoted** strings recognize the `\n` `\t` `\"` `\\` escapes.
fn logical_argument(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // Single-quoted run: copy verbatim up to the closing quote;
                // no escape processing (RFC 7950 §6.1.3).
                for c2 in chars.by_ref() {
                    if c2 == '\'' {
                        break;
                    }
                    out.push(c2);
                }
            }
            '"' => {
                // Double-quoted run: recognize the YANG escape sequences.
                while let Some(c2) = chars.next() {
                    if c2 == '"' {
                        break;
                    }
                    if c2 == '\\' {
                        if let Some(e) = chars.next() {
                            out.push(match e {
                                'n' => '\n',
                                't' => '\t',
                                'r' => '\r',
                                '"' => '"',
                                '\'' => '\'',
                                '\\' => '\\',
                                _ => e,
                            });
                        }
                    } else {
                        out.push(c2);
                    }
                }
            }
            '+' => {
                // String-concatenation operator (only meaningful between
                // quoted fragments); drop it and surrounding whitespace.
                while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
                    chars.next();
                }
            }
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

fn collect_errors(node: Node, text: &Text) -> Vec<ParseError> {
    let mut out = Vec::new();
    collect_errors_inner(node, text, &mut out);
    out
}

fn collect_errors_inner(node: Node, text: &Text, out: &mut Vec<ParseError>) {
    if node.is_error() || node.is_missing() {
        let range = node.start_byte()..node.end_byte();
        let what = if node.is_missing() {
            "missing"
        } else {
            "unexpected"
        };
        let snippet = text.slice(range.clone());
        let detail = snippet.trim();
        out.push(ParseError {
            range,
            message: if detail.is_empty() {
                format!("parse error: {what} token")
            } else {
                format!("parse error: {what} {detail:?}")
            },
        });
        return;
    }
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i as u32) {
            collect_errors_inner(c, text, out);
        }
    }
}

/// Collect every comment in document order.
///
/// Comments come from the grammar's `comment` rule (a `//` or `/* … */` token)
/// and appear as nodes in the CST even though the derived [`Statement`] tree
/// drops them. A full pre-order walk of the CST therefore yields every comment —
/// inside blocks, between top-level statements, and outside the module
/// entirely — exactly as the grammar (and so its string handling) sees them: a
/// `//` inside a quoted argument is part of the string token, never a comment
/// node.
fn collect_comments(node: Node, text: &Text) -> Vec<Comment> {
    let mut out = Vec::new();
    collect_comments_inner(node, text, &mut out);
    out
}

fn collect_comments_inner(node: Node, text: &Text, out: &mut Vec<Comment>) {
    if node.kind() == "comment" {
        let range = node.start_byte()..node.end_byte();
        let raw = text.slice(range.clone());
        let kind = if raw.starts_with("//") {
            CommentKind::Line
        } else {
            CommentKind::Block
        };
        out.push(Comment {
            range,
            kind,
            text: raw.to_string(),
        });
        return;
    }
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i as u32) {
            collect_comments_inner(c, text, out);
        }
    }
}

/// Reserved words used as unquoted statement *arguments* (status/deviate/
/// ordered-by/range-modifier values). Colored like keywords.
const RESERVED_ARG_WORDS: &[&str] = &[
    "add",
    "current",
    "delete",
    "deprecated",
    "invert-match",
    "max",
    "min",
    "not-supported",
    "obsolete",
    "replace",
    "system",
    "unbounded",
    "user",
];

/// Collect every CST leaf as a raw lexical [`Token`], in source order.
///
/// This is the fine-grained stream the [`Statement`] tree drops: keywords,
/// identifiers, quoted-string runs, numeric literals, `true`/`false`, the `+`
/// concat operator, comments and punctuation. Whitespace is not a CST node, so
/// it never appears; a `//` inside a quoted argument is part of the string
/// token, never a comment (the grammar's own string handling).
fn collect_tokens(node: Node, text: &Text) -> Vec<Token> {
    let mut out = Vec::new();
    collect_tokens_inner(node, text, &mut out);
    out
}

fn collect_tokens_inner(node: Node, text: &Text, out: &mut Vec<Token>) {
    if node.is_missing() {
        return;
    }
    // A quoted run is one lexical unit (range includes the quotes); never
    // descend into it — otherwise its inner tokens would look like identifiers
    // or numbers and mis-color the string's content.
    if node.kind() == "quoted_string" {
        let range = node.start_byte()..node.end_byte();
        if range.start == range.end {
            return;
        }
        let raw = text.slice(range.clone());
        out.push(Token {
            kind: TokenKind::String,
            range,
            text: raw.to_string(),
        });
        return;
    }
    if node.child_count() == 0 {
        if node.is_error() {
            return;
        }
        let range = node.start_byte()..node.end_byte();
        if range.start == range.end {
            return;
        }
        let raw = text.slice(range.clone());
        out.push(Token {
            kind: classify_token(node.kind(), raw),
            range,
            text: raw.to_string(),
        });
        return;
    }
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i as u32) {
            collect_tokens_inner(c, text, out);
        }
    }
}

/// Merge a recovered quoted-run / `+`-operator token unless an identical range
/// is already present (the leaf walk usually produced the trailing fragments
/// of a concatenated argument; only the missing pieces are added).
///
/// `seen` is the O(1) membership set of every token range already collected,
/// built once by [`augment_quoted_fragments`], so recovery stays O(tokens +
/// fragments) instead of re-scanning the token vector per fragment.
fn push_fragment(
    tokens: &mut Vec<Token>,
    seen: &mut HashSet<(usize, usize)>,
    kind: TokenKind,
    start: usize,
    end: usize,
    text: &Text,
) {
    if start >= end {
        return;
    }
    if !seen.insert((start, end)) {
        return;
    }
    let raw = text.slice(start..end).to_string();
    tokens.push(Token {
        kind,
        range: start..end,
        text: raw,
    });
}

/// Some quoted-string fragments are lexed by the grammar as *hidden* tokens
/// that the CST-leaf walk above never sees (e.g. the leading fragment of a
/// `namespace "…" + "…"` argument, whose first piece comes from a hidden
/// `_uri_dq`-style token). Recover every quoted run and `+` concatenation
/// operator inside each statement's argument span from the raw source text,
/// skipping fragments the leaf walk already produced (identical ranges).
/// Quoting follows RFC 7950 §6.1.3: single-quoted runs end at the next `'`;
/// double-quoted runs honor backslash escapes; a `+` outside quotes is the
/// concatenation operator.
///
/// Only called for parses that actually consume the token stream
/// ([`ParseMode::Full`]): the catalog/header-only and text-light paths skip it
/// entirely, and the membership check is an O(1) `HashSet` lookup.
fn augment_quoted_fragments(root: &Option<Statement>, text: &Text, tokens: &mut Vec<Token>) {
    let Some(root) = root else {
        return;
    };
    // Ranges the CST-leaf walk already produced, so only missing pieces are
    // added (one O(n) build instead of a linear scan per fragment).
    let mut seen: HashSet<(usize, usize)> = tokens
        .iter()
        .map(|t| (t.range.start, t.range.end))
        .collect();
    for stmt in root.preorder() {
        let Some(arg) = &stmt.arg else {
            continue;
        };
        let raw = text.slice(arg.range.clone());
        let mut i = 0usize;
        while i < raw.len() {
            let c = raw[i..].chars().next().unwrap();
            let start = arg.range.start + i;
            if c == '\'' || c == '"' {
                let quote = c;
                i += c.len_utf8();
                let mut closed = None;
                while i < raw.len() {
                    let ch = raw[i..].chars().next().unwrap();
                    if quote == '"' && ch == '\\' {
                        // Escape: skip the escaped character.
                        i += ch.len_utf8();
                        if i < raw.len() {
                            i += raw[i..].chars().next().unwrap().len_utf8();
                        }
                        continue;
                    }
                    i += ch.len_utf8();
                    if ch == quote {
                        closed = Some(i);
                        break;
                    }
                }
                let end = closed.unwrap_or(raw.len());
                push_fragment(
                    tokens,
                    &mut seen,
                    TokenKind::String,
                    start,
                    arg.range.start + end,
                    text,
                );
                i = end;
                continue;
            }
            if c == '+' {
                i += c.len_utf8();
                push_fragment(
                    tokens,
                    &mut seen,
                    TokenKind::Operator,
                    start,
                    arg.range.start + i,
                    text,
                );
                continue;
            }
            i += c.len_utf8();
        }
    }
}

/// Classify one CST leaf by its node kind (for anonymous leaves the kind *is*
/// the literal text) with text fallbacks for numbers and quoted URI tokens.
fn classify_token(kind: &str, text: &str) -> TokenKind {
    match kind {
        "comment" => TokenKind::Comment,
        "identifier" => TokenKind::Identifier,
        "integer_value" | "decimal_value" => TokenKind::Number,
        "boolean" | "true" | "false" => TokenKind::Boolean,
        "+" => TokenKind::Operator,
        _ if kind.ends_with("_keyword") || RESERVED_ARG_WORDS.contains(&kind) => TokenKind::Keyword,
        _ if is_quoted(text) => TokenKind::String,
        _ if looks_like_number(kind) || looks_like_number(text) => TokenKind::Number,
        _ => TokenKind::Other,
    }
}

/// Does this raw leaf text look like a quoted string (quotes included)? Used
/// for monolithic tokens such as the namespace URI scanner.
fn is_quoted(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
}

/// Heuristic: does this string look like an unquoted numeric literal?
fn looks_like_number(s: &str) -> bool {
    let s = s
        .strip_prefix('+')
        .or_else(|| s.strip_prefix('-'))
        .unwrap_or(s);
    let (int_part, frac_part) = match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    digits(int_part) && frac_part.map(digits).unwrap_or(true)
}

/// A token hit by a caret query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSpot {
    /// The caret is over (or inside the span of) a statement keyword.
    Keyword,
    /// The caret is inside a statement argument.
    Argument,
    /// The caret is inside some other token of the statement.
    Other,
}

impl TokenSpot {
    pub fn of(stmt: &Statement, byte: usize) -> (TokenSpot, &Statement) {
        let s = stmt.narrowest_at(byte).unwrap_or(stmt);
        let spot = if s
            .keyword
            .as_ref()
            .map(|r| r.contains(&byte))
            .unwrap_or(false)
        {
            TokenSpot::Keyword
        } else if s
            .arg
            .as_ref()
            .map(|a| a.range.contains(&byte))
            .unwrap_or(false)
        {
            TokenSpot::Argument
        } else {
            TokenSpot::Other
        };
        (spot, s)
    }
}

#[cfg(test)]
mod tests {
    use super::{TokenKind, logical_argument, parse};

    #[test]
    fn logical_argument_unquoted_identifier() {
        assert_eq!(logical_argument("hostname"), "hostname");
    }

    #[test]
    fn logical_argument_strips_quotes() {
        assert_eq!(logical_argument("\"urn:simple\""), "urn:simple");
        assert_eq!(logical_argument("'urn:single'"), "urn:single");
    }

    /// Arguments may be split across `+`-concatenated quoted fragments; the
    /// logical argument is the joined text.
    #[test]
    fn logical_argument_joins_concatenated_fragments() {
        assert_eq!(logical_argument("\"/a:b\" + \"/c:d\""), "/a:b/c:d");
        assert_eq!(logical_argument("\"abc\"+\"def\""), "abcdef");
    }

    #[test]
    fn tokens_include_every_concatenated_quoted_fragment() {
        // The grammar lexes the *leading* fragment of a concatenated quoted
        // argument as a hidden token (namespace's first URI piece), so the
        // CST-leaf walk alone misses it. parse() must still surface each
        // quoted run as a String and the `+` as an Operator (regression).
        let p = parse("module n { namespace \"urn:base/\" + \"urn:more\"; }\n".to_string());
        let text: Vec<String> = p
            .tokens
            .iter()
            .filter(|t| matches!(t.kind, TokenKind::String | TokenKind::Operator))
            .map(|t| t.text.clone())
            .collect();
        assert!(
            text.iter().any(|t| t == "\"urn:base/\""),
            "leading fragment missing: {text:?}"
        );
        assert!(
            text.iter().any(|t| t == "\"urn:more\""),
            "trailing fragment missing: {text:?}"
        );
        assert!(
            text.iter().any(|t| t == "+"),
            "concatenation operator missing: {text:?}"
        );
    }

    #[test]
    fn logical_argument_keeps_plus_inside_quotes() {
        assert_eq!(logical_argument("\"C++ rocks\""), "C++ rocks");
    }

    /// RFC 7950 §6.1.3: single-quoted strings perform no escape processing, so
    /// a backslash is an ordinary character and must be preserved (as in the
    /// regexes of ietf `pattern` statements).
    #[test]
    fn logical_argument_single_quoted_keeps_backslash() {
        assert_eq!(logical_argument(r"'a\.b'"), r"a\.b");
        assert_eq!(logical_argument(r"'\d+'"), r"\d+");
    }

    #[test]
    fn logical_argument_single_quoted_backslash_n_is_literal() {
        // `\n` in a single-quoted string is two literal characters, not a newline.
        assert_eq!(logical_argument(r"'a\nb'"), r"a\nb");
    }

    #[test]
    fn logical_argument_double_quoted_processes_escapes() {
        // Double-quoted strings recognize \n \t \" \\ (RFC 7950 §6.1.3).
        assert_eq!(logical_argument("\"a\\nb\\t\\\"c\\\\d\""), "a\nb\t\"c\\d");
    }

    /// Escape handling is per fragment: `+`-concatenated pieces keep their own
    /// quoting style.
    #[test]
    fn logical_argument_escape_style_is_per_fragment() {
        assert_eq!(logical_argument("'a\\b' + \"c\\nd\""), "a\\bc\nd");
    }
}
