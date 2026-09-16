# yrepo — repository instructions

`yrepo` is a library crate that parses + resolves YANG (`*.yang`). Public
surface: `Repository` (`upsert` / `upsert_many_files` / `remove` / `compile`)
and the `Library` snapshot.

## Build & test

- Test **both** feature modes: `cargo test` and `cargo test --features parallel`.
- `Repository::set_text_light(true)` opts into memory-light parsing
  (description/reference/organization/contact dropped from the Statement tree
  and their quoted runs from tokens; schema semantics unchanged) —
  `027_text_light.rs`.
- Keep `cargo fmt` clean and run clippy STRICTLY:
  `cargo +stable clippy --all-targets --all-features -- -D warnings`.
- The `parallel` cargo feature gates rayon (bulk file ingest via
  `upsert_many_files`, per-module compile phases). Off by default to keep the
  published dependency tree lean.

## Dev/audit tools live in `examples/`

`examples/{inspect,probe,perf}.rs` are yrepo-only tools. Build and run them
**with the `parallel` feature** for representative numbers:

```
cargo build --release --features parallel --examples
target/release/examples/inspect <dir>          # whole-tree diagnostic histogram
target/release/examples/probe <file…>          # per-file diagnostics + context
target/release/examples/perf [--repeat N] [--csv f.csv] <dir…>  # time + CPU + RSS
```
`examples/memstep.rs` is the stepwise NON-parallel memory probe: upserts one
file at a time, logs RSS/VmHWM every `--log-every` files with optional
`--sleep-ms` waterline observation and periodic `--compile-every` compiles,
`--start-at`/`--stop-at` bounds, logs also appended to `$MEMSTEP_LOG` (durable
"last words" when probing the memory edge). Build without `--features
parallel` for a single-threaded ingest path.
`examples/catmem.rs` measures CATALOG-only retention (`Catalog::scan`, header
fields only, transient parse) with interval RSS/VmHWM logs — the retention
model for serving very large trees.
`examples/closure.rs` parses+compiles only the closure (imports + includes)
of N root modules over a `CatalogIndex` via `yrepo::build_closure_repository`
— the serving-by-closure API (RSS/time logs, optional `--text-light`).
`examples/serveperf.rs` measures the serving pipeline standalone: parallel
batch catalog scan (`CatalogIndex::scan_many_files`) over a whole tree, then
materialize + compile the open closure of N roots (`build_closure_repository`,
text-light ON) — two-phase wall/RSS/VmHWM logs, `--limit K` for stepwise scale
curves, `--root-name` for a specific real module.
`examples/memcomp.rs` is the non-parallel retention DECOMPOSITION tool: after
sequentially ingesting a tree it totals statements, owned argument-string
bytes, token-string bytes and comments per document, for estimating string
duplication vs whole-source retention (rope-range redesign decisions).

## Bench baseline

Corpus: an explicit input — the workflow never assumes where it lives. Pass
`--corpus DIR` to `scripts/audit.sh` or set `YANG_CORPUS` (see the workspace
root `AGENTS.md` for the machine-local example). Baselines below are for the
YangModels `yang` tree (**2143 files / ~30.5 MB**), release build, `parallel`
feature (2026-09-06):

| tool | result |
| --- | --- |
| `inspect` | ~0.45–0.6 s; **1540** diagnostics (1219 errors / 321 warnings) with the local tree-sitter-yang grammar fixes (all unpublished; released 0.3.0 grammar: 2984) — top: augment-target-not-found 447, unresolved-grouping 301, duplicate-module 278 (warnings), unresolved-typedef 193, unresolved-identity 18, parse-error 58, duplicate-node 23, unresolved-leafref-path 100, unresolved-import 47, not-a-yang-document 39, unresolved-prefix 22, key-leaf-not-found 9, list-without-key 4 (warnings) |
| `perf` (single) | ~0.45–0.65 s wall / ~3.8–4.5 s CPU; RSS ~3 MB → **~721 MB peak** (VmHWM) |

Treat these as a regression guard: if parse/compile/retention changes move time
or RSS significantly, re-check with `perf` and keep the CSV. Peak RSS is driven
by per-document tree-sitter CST retention — the current biggest lever.

## Conventions

- Diagnostics: dangling `leafref` absolute `path` expressions are flagged
  (predicates stripped segment-wise, prefixes resolved via the origin module;
  reported once per authored site; relative paths are instance-context
  dependent (a grouping instantiated at different depths makes the arena
  parent chain diverge from authored intent), so validation never guesses
  them — the leafref engine resolves them from a concrete instance when
  available — `025_leafref_path.rs`).
- Diagnostics: duplicate sibling node names are flagged only for the
  record's OWN module and same physical file (authoring mistakes); cross-module
  augment collisions and multi-revision merge duplicates are not reported
  (`024_duplicate_nodes.rs`).
- Diagnostics: files whose first non-whitespace char is `<` (HTML/XML mislabeled
  `*.yang`) are reported once as `not-a-yang-document` — their meaningless
  whole-file `parse-error` is suppressed (regression test `016_html_not_yang.rs`).
  Duplicate modules with the same (name, revision) are reported as **warnings**
  (visible, non-blocking — the later copy is ignored); among equal copies a
  parse-clean copy wins over a broken one (`017_duplicate_prefers_clean.rs`);
  modules with different revisions coexist. `inspect` prints `errors:` /
  `warnings:` counts and a `whole-file parse collapses:` count (parse-error
  covering >=95% of a document — the PHASE 0 error-localization metric) in
  addition to the by-code histogram.
- Report user-content problems as **diagnostics**, never `Err`.
- Versioning: public-API changes → bump version + CHANGELOG. A change is only
  "breaking" if it affects something that actually shipped in a published
  version (never-released APIs can be dropped freely).
- **Publishing is CI-owned; the agent never runs `cargo publish`.** Cut a
  release by bumping the version + CHANGELOG and committing, then pushing a
  `v*` tag: `.github/workflows/cargo-publish.yml` publishes to crates.io and
  `github-release.yml` creates the GitHub release. The agent may create the
  local tag; the human pushes it. `cargo publish --dry-run` is for validation
  only.
- Parser behavior changes belong in `tree-sitter-yang`: fix `grammar.js` there
  and regenerate (use the `parser-regen` skill; never hand-edit `parser.c`),
  then patch `yrepo` to the fixed grammar and bump the dependency.
- Name-based (prefixed) references — unversioned imports, typedef/identity
  bases, prefixed `type` refs — resolve to the HIGHEST revision of a module
  (canonical-latest); an import pinned with `revision-date` resolves to that
  exact revision first. Each revision keeps its own symbol table. Internal
  (unprefixed) references resolve against the owning module instance's OWN
  table only — a non-canonical revision is validated on its own terms and
  never sees another revision's typedefs/identities (`021_typedef_own_revision.rs`,
  `023_identity_own_revision.rs`).
  Augment/deviation target paths are resolved from the DECLARING instance's
  prefix map and import pins, so a pinned import augments the pinned
  revision's tree (`022_augment_pinned_revision.rs`).
- The local `[patch.crates-io]` override pointing `tree-sitter-yang` at
  `../tree-sitter-yang` is a TEMPORARY dev convenience — never commit it;
  remove it once the grammar fixes are version-bumped and published, then
  switch `yrepo` to the released `tree-sitter-yang` version.
- Sync README / CHANGELOG / docs/architecture.md whenever behavior or the API
  changes.
- Grammar fixes in progress (unpublished; released 0.3.0 grammar: 2984):
  `max-elements unbounded;` parses again (`038_max_elements.rs`), unknown/vendor
  extension statements accept bare symbol arguments such as `m^-X`
  (`039_vendor_symbol_arg.rs`), the `units` statement accepts bare symbol
  arguments such as `meter^2.second-1` (`040_units_symbol_arg.rs`; cleared all
  IEEE 1906.1 modules), `enum` names accept bare strings with symbols such as
  `n+1` (`041_enum_bare_symbol_name.rs`; cleared coms-core, routing-types,
  igmp-mld, iana-* registry modules), the `default` statement accepts bare
  symbol arguments such as `00:00:15.0` or `syslogtypes:local7`
  (`042_default_symbol_arg.rs`; cleared ietf-netconf-time, ietf-syslog), and
  double-quoted strings accept arbitrary backslash escapes (`\*`, `\S`, `\.`,
  `043_escape_sequences.rs`; cleared ietf-netconf-acm, ietf-ipfix-psamp,
  DRAFT ietf-isis), and concatenated/trailing-whitespace `key`/`unique`
  arguments parse as opaque quoted strings (`047_key_unique_concat.rs`,
  grammar-side), and `range`/`length` concatenated quoted arguments parse
  as opaque strings too (`048_range_length_concat.rs`, grammar-side) — the latter unblocked whole-module collapses that cascaded
  not-a-yang-document and unresolved-import/grouping/augment noise. All are
  wired into yrepo via the local `[patch.crates-io]` override — keep that
  patch until the fixes are version-bumped/published.
  Residual corpus parse-errors (~60) are overwhelmingly content artifacts in
  `experimental/ietf-extracted-YANG-modules` — corrupted extraction text
  (unterminated or embedded quotes, orphan statements, MIB transcripts) and
  placeholder modules — not valid-YANG grammar gaps; sample drills confirm
  quote corruption rather than new constructs. See the `issue-hunter` skill.

## Workflow skills

- `issue-hunter` — audit/report over a YANG tree; ground every claim in a run
  of `examples/{inspect,probe,perf}` or `scripts/audit.sh`.
- `audit-and-fix` — the full audit → categorize → fix → re-audit loop;
  `scripts/audit.sh [--corpus DIR]` is the deterministic runner
  (report at `target/audit/report.txt`, delta in `target/audit/prev.txt`).
