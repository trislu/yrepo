# yrepo — yet another YANG repository

[![Rust CI](https://github.com/trislu/yrepo/actions/workflows/rust.yml/badge.svg)](https://github.com/trislu/yrepo/actions/workflows/rust.yml)
[![Cargo Publish](https://github.com/trislu/yrepo/actions/workflows/cargo-publish.yml/badge.svg)](https://github.com/trislu/yrepo/actions/workflows/cargo-publish.yml)
[![GitHub Release](https://github.com/trislu/yrepo/actions/workflows/github-release.yml/badge.svg)](https://github.com/trislu/yrepo/actions/workflows/github-release.yml)
[![Latest Version](https://img.shields.io/crates/v/yrepo.svg)](https://crates.io/crates/yrepo)
[![License](https://img.shields.io/crates/l/yrepo.svg)](LICENSE)

An [LSP](https://microsoft.github.io/language-server-protocol/)-friendly YANG
schema toolkit (parse + resolved semantic model), written in Rust. Currently
supports `*.yang` only.

## Two entry points

- **`Repository`** — manages the open documents of a workspace by `url`
  (`upsert` / `remove`) and compiles them.
- **`Library`** — the resolved snapshot returned by `Repository::compile()`:
  modules (with their folded submodules), the *effective/expanded* schema tree,
  symbol tables, and cross-module queries.

```rust
use yrepo::Repository;

let mut repo = Repository::new();
repo.upsert("/mods/m.yang", "module m { namespace \"urn:m\"; prefix m; leaf x { type string; } }");
let outcome = repo.compile();
for d in &outcome.diagnostics {
    println!("{}: {} {}", d.severity, d.code, d.message);
}
let lib = outcome.library.expect("library");
let m = lib.module("m").expect("module m");
println!("{} top-level nodes", m.top_nodes().len());
```

## Syntax view (statement tree)

For fold / format / highlight / goto-hover, the parsed document also exposes its
tree-sitter-free **statement tree** (no second parse needed):

```rust
use yrepo::{Repository, StatementKind};

let mut repo = Repository::new();
repo.upsert("/m.yang", "module m { namespace \"urn:m\"; prefix m; container c { leaf x { type string; } } }");

// Whole document tree, rooted at the `module`/`submodule` statement.
let root = repo.statement("/m.yang").expect("module m");
for stmt in root.preorder() {
    // kind, keyword span, argument text, `;` vs `{…}` terminator, children…
    let arg = stmt.arg.as_ref().map(|a| a.logical.as_str()).unwrap_or("");
    println!("{:?} {:?}", stmt.kind, arg);
}

// The narrowest statement under the caret — read its argument for goto/hover.
// Col 40 is the `container` keyword in the line above.
if let Some(stmt) = repo.statement_at("/m.yang", 0, 40) {
    assert_eq!(stmt.kind, StatementKind::Container);
}

// Comments are not part of the statement tree; they are exposed separately
// (in source order) so a formatter never deletes them.
let comments = repo.comments("/m.yang").unwrap();
assert!(comments.is_empty());
```

## Semantic queries & value typing

Beyond the syntax view, `Library` answers semantic queries over the resolved
model: module/symbol lookup, identity derivation, and the **effective/expanded
schema tree** (groupings instantiated per `uses`, shorthand `case`s
materialized, `rpc`/`action` `input`/`output` always present, cross-module
`augment`/`deviation` applied). For instance documents the tree is queried
through *data-visible* children (looking through `choice`/`case` wrappers) and
per-node **instance-module** namespaces, so XML/JSON mapping keys on the module
that owns a node's namespace rather than where it was defined.

Leaf **value typing** reduces a leaf/leaf-list's `type` through the typedef
chain to a scalar builtin and classifies it — `string` (with `length`/
`pattern`), integers (`int8`…`uint64`, with `range`), `decimal64`, `boolean`,
`empty`, `binary`, `enumeration`/`bits` (with members), plus `leafref`,
`identityref`, `instance-identifier`, and `union` (deliberately not checked —
a bare value can't be attributed to one member, RFC 7950 §9.12). Facets are
captured from the leaf **and** each typedef's `type` statement.

```rust
use yrepo::{IdentityStatus, Repository, ValueType};

let mut repo = Repository::new();
repo.upsert("/m.yang", r#"module m {
  yang-version 1.1; namespace "urn:m"; prefix m;
  identity base;
  identity child { base base; }
  typedef port { type uint16 { range "1..65535"; } }
  leaf p { type port; }
  leaf kind { type identityref { base base; } }
}"#);
let lib = repo.compile().library.expect("library");
let m = lib.module("m").expect("module m");
let p = m.nodes().iter().position(|n| n.name() == "p").unwrap();
match lib.value_type("m", p) {
    Some(ValueType::Integer { signed: false, bits: 16, ranges }) => {
        assert_eq!(ranges, vec!["1..65535"]);
    }
    _ => unreachable!(),
}
assert_eq!(
    lib.check_identityref("m", Some("base"), "m:child"),
    IdentityStatus::Ok
);
```

## Whole-tree references (beyond the open closure)

The serving model keeps full statement trees only for the *open closure*
(the modules you have open plus their imports/includes), so references to a
symbol defined in a library module (e.g. a typedef in `ietf-yang-types`) that
other modules merely *import* are invisible through the closure. `ReferenceIndex`
answers whole-tree references without expanding the data tree: it
statement-walks a file batch once (like the catalog scan) and retains only
compact definition/reference occurrences resolved to their target module.

```rust
use yrepo::ReferenceIndex;

let mut ix = ReferenceIndex::default();
ix.scan_many_files_with(&files, |p| Some(format!("file://{}", p.display())));
// Every `type t:speed` / `typedef speed` across the tree that names
// module "types", symbol "speed"; the declaration is included when true.
for (url, range) in ix.references("types", "speed", true) { /* … */ }
```

Semantics mirror the editor-side reference engine: `typedef`/`grouping`/
`identity`/`feature`/`extension` definitions (submodule symbols are owned by
their `belongs-to` parent module), `type`/`uses`/`base`/`if-feature`
references, builtin `type` names skipped, dangling-import targets dropped.
The index is a whole-batch builder (`scan_many_files…` replaces prior
contents); no statement trees or source text survive the scan.

## Notes

- User-content problems (parse errors, unresolved imports, bad keys, …) are
  reported as **diagnostics** — never as `Err`.
- Circular chains of imports and include cycles are reported as diagnostics
  (RFC 7950 §5.1 forbids circular chains of imports).
- Groupings are instantiated into the effective tree at each `uses` site, and
  cross-module `augment`/`deviation` targets are resolved onto it.

## Parallel parsing (optional)

Parsing and the per-module phases of `compile` can run across threads with
[rayon](https://docs.rs/rayon), gated behind the `parallel` cargo feature (off
by default to keep the dependency tree lean):

```toml
[dependencies]
yrepo = { version = "0.6", features = ["parallel"] }
```

With the feature enabled, `Repository::upsert_many_files` reads **and** parses
a whole batch of documents off-thread (one file in memory at a time) — prefer
it over looping `upsert` when opening many files at once (e.g. an LSP
workspace scan) — and `compile` runs the symbol scan, effective-tree expansion
and validation per module in parallel. Every parallel path preserves
document/module order, so the resulting `Library` and diagnostics are
identical to the sequential pipeline.

## Layout

```bash
src/
  lib.rs        # facade: Repository + re-exports
  text.rs       # raw source + line/offset access
  syntax.rs     # CST layer
  yang.rs       # syntactic per-doc model + header extraction
  compile.rs    # symbol scan, effective-tree expansion, augment/deviation
  schema.rs     # semantic model types (effective nodes, module records)
  library.rs    # Library + queries
  refidx.rs     # whole-tree reference index (ReferenceIndex)
  value.rs      # leaf value typing: TypeFacets capture + ValueType
  diag.rs       # Diagnostic / Severity / DiagnosticCode
tests/
  NNN_*.rs      # numbered integration tests
  sample_yang/  # fixture files
```

## License

This project is licensed under the [MIT License](LICENSE)
