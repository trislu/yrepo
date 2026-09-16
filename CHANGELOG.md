# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.2] - 2026-09-17

### Added

- `Library::check_identityref_in(leaf_module, base, value_module, value_local)`
  — the semantic `identityref` check for a value whose **module the caller has
  already resolved** (an XML namespace prefix/default namespace per RFC 7950
  §9.10.3, or an RFC 7951 `module:name`). It removes the prefix/module-name
  guessing `check_identityref` needs for a raw QName, so an instance encoding
  can resolve the value's module itself and then ask only about identity
  existence and base derivation. `check_identityref` keeps its QName behaviour
  and now delegates to it.
- `ModuleSummary` now carries the module's own `prefix` and its top-level
  `identity` declarations (`identities: Vec<IdentitySummary>`, each with its
  `base` resolved to `(module, local)` in the declaring file's prefix scope).
  `SummaryIndex` folds a submodule's identities into the parent module like the
  other top-level names, so the summary index can answer "which workspace
  identities derive from this base" without compiling the tree. Still
  parse-level and additive.

### Fixed

- **Submodule top-level names are folded into their parent module's summary.**
  A submodule has no namespace, so a namespace-keyed projection of
  `SummaryIndex` (the language server's Tier-1 schema suggestions) dropped the
  top-level data nodes, `rpc`s and `notification`s it declares, even though at
  run time they live in the parent module's namespace.
  `SummaryIndex::scan_many_files_with` now merges every submodule's names into
  the summary of the module named by its `belongs-to` (deduplicated, source
  order preserved). The fold is revision-agnostic: a name declared by any
  submodule of a module is added to every indexed revision of it, so it can only
  over-approximate a suggestion list, never invent an unrelated name. No public
  API change.

## [0.7.1] - 2026-09-11

### Added

- `SummaryIndex::summaries()` — iterator accessor over the indexed module
  summaries, needed by consumers that serve schema suggestions from summaries
  without a compiled library (netconf-language-server instance documents).
  Additive; no other behaviour change.

## [0.7.0] - 2026-09-11

### Added

- **`yrepo::ModuleSummary` + `yrepo::SummaryIndex`** — a parse-level module
  summary (name, namespace, revision, top-level data/rpc/notification names)
  extracted with the new `ParseMode::Summary`, without building the full
  statement tree, tokens or comments (`Full`/`Light`/`HeaderOnly` behaviour is
  unchanged). `SummaryIndex::scan_many_files_with` scans a tree in parallel
  with a caller-provided url mapping and `resolve_namespace` returns the
  highest-revision parse-clean entry. This is the yrepo half of the
  instance-document lazy schema design used by `netconf-language-server`
  (root/data-root completion and namespace lookup for XML/RFC 7951 JSON
  documents on very large workspaces).

## [0.6.0] - 2026-09-11

### Added

- **`yrepo::PathIndex`** — a parse-free path index built from a directory
  walk: files are grouped by basename with the `@revision-date` suffix
  stripped (`candidates`), plus a bounded, sorted fallback over names whose
  declared module name differs from the filename (`prefix_candidates`). This
  is what lets startup defer all header parsing to the names actually needed.
- **`CatalogIndex::resolve_lazy`** — grow the catalog on demand by scanning
  **only** the candidate paths for a name that is not yet resolvable; returns
  the winning url and how many files were parsed. Resolution follows
  `CatalogIndex::resolve` exactly, so a lazily grown index and a whole-tree
  index pick the same entry.

### Changed

- **`Catalog::scan` parses header-only** (`ParseMode::HeaderOnly`): it builds
  only the module/submodule root plus the statements header extraction reads —
  no full statement tree, token/comment streams or parse errors — so scan cost
  stays close to the raw tree-sitter parse. Retained catalog fields and
  `CatalogIndex::resolve` results are unchanged. On a 165 521-file corpus the
  worst single file (a 5.96 MB vendor module) drops from 4.97 s to 0.19 s.
- **Quoted-fragment recovery is linear**: the full parse's
  concatenated-argument recovery now checks a hash set of token ranges built
  once, so it is O(tokens + fragments) instead of re-scanning the token vector
  per fragment.

No public API break: the new items are additive and parse output for valid
modules is unchanged.

## [0.5.0] - 2026-09-09

### Added

- **`yrepo::ReferenceIndex`**: whole-tree find-references without expanding the
  open-closure data tree. `scan_many_files…` statement-walks a file batch once
  (in parallel under the `parallel` feature) and retains only compact
  definition/reference occurrences resolved to their target module;
  `references(module, local, include_declaration)` returns every
  `(url, byte-range)` hit across the indexed tree. Mirrors the editor-side
  reference engine: `typedef`/`grouping`/`identity`/`feature`/`extension`
  definitions (submodule symbols owned by the `belongs-to` parent),
  `type`/`uses`/`base`/`if-feature` references, builtin `type` names skipped,
  dangling-import targets dropped. Retains no statement trees or source text.

### Changed

- **Bundled grammar: `tree-sitter-yang` 0.4.1** — a `type string { … }` block
  with a vendor extension statement interspersed between `pattern`/`length`
  members no longer collapses the whole module (e.g. OpenConfig's
  `openconfig-yang-types.yang`). No API change; parse output for previously
  valid modules is unchanged.

### Fixed

- **Token stream omitted quoted fragments of concatenated arguments**: the
  grammar lexes the leading piece of a `namespace "…" + "…"`-style argument
  as a hidden token, so `Repository::tokens` never surfaced it. `parse` now
  recovers every quoted run and `+` concatenation operator inside statement
  argument spans (`augment_quoted_fragments`), skipping fragments the CST walk
  already produced. Full (non-light) documents only; text-light retention is
  unchanged.

## [0.4.0] - 2026-09-07

### Added

- **Catalog-only indexing for very large trees**:
  `yrepo::Catalog` (`Catalog::scan`) parses a document transiently and retains
  only its header facts (name, revision, prefix, imports, includes, parse
  status) — roughly 7 KB per file instead of full parse views.
- **`yrepo::CatalogIndex` + `yrepo::build_closure_repository`** (the
  serve-by-closure API): an index of scanned catalogs keyed by module name and
  document url, plus a repository builder that parses and compiles only the
  roots and their reachable closure (imports + includes resolved by name,
  documents read on demand via a caller-supplied reader).
- **Opt-in text-light parsing**: `Repository::set_text_light(true)` drops
  description/reference/organization/contact statements from the Statement
  tree and their quoted runs from the token stream. Schema resolution and LSP
  semantics are unaffected (tests `027_text_light.rs`); default OFF.

### Changed

- **Clearer diagnostic wording** (no API change): user-facing messages for
  unresolved imports/includes/belongs-to, unknown leaf types, recursive
  `uses`, and missing key leaves were reworded to drop internal "open"
  vocabulary and to name the affected list where relevant. Machine-readable
  `DiagnosticCode` values, severities, and codes are unchanged.

## [0.3.0] - 2026-09-06

### Added

- **Optional parallel parsing & compilation** behind the new `parallel` cargo
  feature (off by default): `Repository::upsert_many_files((url, path)…)`
  reads *and* parses a batch of files off-thread (one file in memory at a
  time) — for LSP workspace scans etc. — and the per-module phases of
  `compile` (symbol scan, effective-tree expansion, light validation) also run
  in parallel. Every parallel path preserves document/module order, so the
  resulting `Library` and diagnostics are identical to the sequential
  pipeline.

### Changed

- Batch document ingest is now **file-based only**
  (`Repository::upsert_many_files`). The in-memory
  `upsert_many((url, source)…)` batch helper existed only in unreleased
  development and was dropped before release; it never shipped in a published
  version, so this is **not** a breaking change. In-memory updates remain
  single-document via `Repository::upsert`.

## [0.2.0] - 2026-09-05

### Added

- **Effective-tree queries for instance documents** (data path ≠ schema path):
  `ModuleRecord::data_children` / `data_child` (instance-visible children
  through `choice`/`case` wrappers), `rpc_input`/`rpc_output` (always present),
  `SchemaNode::instance_module` (the module whose **namespace** owns a node —
  equals `origin_module` except for grouping-born nodes via `uses`), and
  `Library::modules_by_namespace` / `Library::schema_nodeid` (canonical
  wrapper-inclusive nodeid, instance-module prefixes).
- **Leaf value typing** (`src/value.rs`): `TypeFacets` captured from each
  `type` statement (leaf and typedef), and `Library::value_type` which reduces a
  leaf/leaf-list type through the typedef chain to a `ValueType` — a scalar
  builtin (`String`/`Integer`/`Decimal64`/`Boolean`/`Empty`/`Binary`/
  `Enumeration`/`Bits`) with the facets accumulated along the chain, or
  `Leafref`/`Identityref`/`InstanceIdentifier`/`Union`/`Unknown`.
- **Semantic `identityref` check**: `Library::check_identityref(module, base,
  value) -> IdentityStatus` (`Ok`/`UnknownIdentity`/`NotDerived`) — the value
  must name an existing identity that is the `base` or derived from it.

## [0.1.0] - 2026-09-05

Initial public release of `yrepo`, an LSP-friendly YANG schema toolkit
(parse + resolved semantic model) for `*.yang` documents.

### Added

- **Document management & compile**: `Repository::upsert` / `remove` /
  `contains` / `len` by `url`, and whole-workspace `Repository::compile` that
  returns diagnostics plus a snapshot `Library` (`Arc`), so each edit yields a
  consistent resolved view of the workspace.
- **Diagnostics, never `Err`** for user content: parse recovery plus
  unresolved import/include/belongs-to, import & include cycles, unresolved
  type/grouping/identity/prefix, and list-`key` validation.
- **Semantic model & resolution**:
  - modules (submodules folded in, each node keeping its defining file) with
    latest/exact `(name, revision)` lookup;
  - typedef chains resolved to builtins, identity derivation and
    identityref value sets, extension/feature definitions, prefix→module lookup;
  - effective/expanded schema tree: groupings instantiated per `uses`,
    shorthand `case`s materialized, `rpc`/`action` `input`/`output` always
    present, and cross-module `augment`/`deviation` applied
    order-independently.
- **Syntax view for LSP work**: a tree-sitter-free statement tree
  (`statement` / `statement_at`), comments, and the grammar-precise raw token
  stream for semantic tokens/highlighting.
- **Completion candidates**: builtin + local/imported typedefs, and
  identity candidates.

### Notes

- `*.yin` (XML), XPath/leafref evaluation, and RFC 7950 restriction-subset
  semantics are not yet implemented.
