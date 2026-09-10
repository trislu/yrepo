# Memory findings (giant-corpus goal, 2026-09-06)

Method: non-parallel stepwise probes (`examples/memstep.rs`, one file at a
time, RSS/VmHWM interval logs, `$MEMSTEP_LOG` for the exhaustion edge) plus
retention decomposition (`examples/memcomp.rs`). All numbers are DEBUG-build
sequential-process measurements on synthetic trees (real corpus numbers to be
collected separately when a giant sample is available; commit messages carry
no corpus statistics).

## Synthetic micro modules (2000 files, 314 B avg, 628 KB total)

| metric | with CST (before drop) | CST dropped | delta |
| --- | --- | --- | --- |
| ingest RSS at 2000 files | 49 152 KB | 40 144 KB | −18% |
| per-file ingest slope | ≈ 23.0 KB/file | ≈ 18.1 KB/file | −4.9 KB/file |
| peak incl. compile | 62 276 KB | 61 028 KB | −1.2 MB |

Decomposition (post-drop): source 628 KB; statements 38 000;
`Argument.logical` owned bytes = 32.5% of source; token text bytes = 81.5%;
owned duplicate (arg+token) = **114%** of source.

## Realistic-shaped synthetic (1500 files, 949 B avg, 1.42 MB total)

Decomposition: statements 44/file; arg bytes 45% of source; token bytes 85%;
owned duplicate = **130%** of source (tokens 201/file).
Waterline: ingest RSS ≈ 40 KB/file (42× source); compile adds a transient
≈ +22 MB at 1500 files (peak 86 MB), which scales with the arena/library.

## Real modules — ingest waterline (RFC dir subset sampling)

memstep (sequential, post-CST-drop) on 200 of 486 real RFC files: RSS grows
≈ 140–350 KB/file as later (larger) modules load — order of magnitude
**~0.1–0.35 MB per ~20 KB module** (~10× source). Even the smallest reading
makes a 163k-file full compile fundamentally multi-10-GB: holding full parse
views + built schema arenas for every file in one process cannot fit WSL RAM.
This is the load-bearing conclusion: rope-range (~≤15%) and CST-drop are
secondary; the necessary lever is RETENTION/SCOPE — compile (and parse) only
the closure of modules actually queried/open, with an index-only catalog for
the rest. See `rope-range-inventory.md` and the language-server design note
(closure-scoped compile) to follow.

## Real modules (RFC dir subset: 486 files, 19.5 KB avg, 9.5 MB total)

Decomposition: statements 260/file; `Argument.logical` owned bytes = **64%**
of source; token text = 78% of source (927 tokens/file); owned duplicate =
**142%** of source; comments 5/file. Real content therefore shows HIGHER
string duplication than the synthetic sets, so the rope-range redesign's
ceiling on realistic trees is above the synthetic estimate (string bytes +
one heap allocation per argument and per token).

## Catalog-only retention experiment (views dropped, header+source kept)

Local temporary patch (parse returns no statement/token/comment views), same
200 real RFC files: RSS at 200 files = 10 740 KB vs 49 408 KB with full
views → catalog-only retention is ≈ **22%** of full-parse retention
(**−78%**, ~34 KB/file vs ~235 KB/file; still ≈1.7× source because the full
source text is retained per document). Even ~34 KB/file over a 163k tree
(~5.5 GB) is too large to hold in full, so a catalog variant that also drops
the retained source (only extracted header fields per document) is the next
candidate to measure; on-demand re-read/re-parse supplies text when a
document is opened. Full-views compile (schema arenas for every module)
remains fundamentally out of scope for one process.

## No-source catalog experiment (full build then free)

Second local experiment: keep header fields but ALSO drop the retained source
after a FULL parse+build. Because the statement views were still constructed
before being freed, the allocator's retained high-water dominates: RSS at 200
real RFC files = 17 536 KB — HIGHER than the header+source-light path
(10 740 KB) and 3× lower than full retention (49 408 KB). Conclusion: the
catalog path must NOT build full statement/token views at all; it needs a
LEAN HEADER SCANNER (parse the CST only down to the module header: name,
revision, imports/includes, prefix, namespace) whose per-file footprint should
be a few KB — the next prototype to implement and measure.

## Catalog record prototype (header fields only, transient parse)

New `yrepo::Catalog` + `Catalog::scan` keep ONLY header facts (name,
revision, prefix, imports, includes, parse status); the parse is transient.
Real RFC subset (486 files): RSS settles ≈ 6.4 MB total (peak 6.9 MB),
≈ **7 KB/file** catalog retention. Extrapolating ~163k files ≈ ~1.2 GB —
feasible as an in-process catalog (before any closure compiles), unlike
full-parse retention. Next: trim remaining catalog fields if needed, then the
language-server catalog-wide scan + on-demand closure compile.

## Text-only statement share (description/reference/organization/contact)

memcomp on 486 real RFC modules: text-like statements are 44 962 of 126 552
(35%); their owned argument bytes are **51.3% of source and 79.8% of all
argument-string bytes** (4.87 MB of 6.10 MB). On the realistic synthetic set
the share is 20.6%/45.6%. The token stream stores the same quoted strings
again (token text ≈ arg bytes + quotes), so removing text-only statements
from BOTH the Statement tree and the token stream could drop roughly
arg(4.9 MB) + token(≈5 MB) per 9.5 MB-source subset plus ~45k statement
nodes. No LSP feature reads description/reference/… bodies (hover/goto/
completion/rename ignore them), so an opt-in "text-light" parse mode is the
candidate next optimization — default OFF to keep existing consumers/tests.
Closure-scope compile prototype (`examples/closure.rs`): 20 real roots →
41-file closure parsed+compiled at 22.5 MB RSS (vs 486-file full retention),
validating the serve-by-closure model.

## Text-light parse mode (implemented, opt-in)

`Repository::set_text_light(true)` drops description/reference/organization/
contact from the Statement tree and their quoted runs from the token stream;
schema resolution and LSP semantics are identical (tests 027_text_light).
Real RFC subset memstep at 200 files: 41 344 KB vs 49 408 KB full (**−16%
ingest retention**), in addition to fewer statement nodes. Effect is
meaningful but bounded: remaining token/statement structure and full-parse
costs still dominate — text-light belongs in the serving path (catalog +
closure + light) rather than standing alone.

## CatalogIndex + closure repository (implemented: serve-by-closure API)

`yrepo::CatalogIndex` indexes scanned `Catalog` records by module name and by
document url; `canonical(name)` picks the highest-revision parse-clean entry
(the same rule `compile` applies to duplicate modules).
`yrepo::build_closure_repository(index, roots, light, read)` parses exactly
`roots` plus everything reachable through the catalog's import edges (module
names) AND include edges (submodule names — submodules are separate documents
folded into their parent at compile time), reading each document on demand via
a caller-supplied `read` (url -> source). Names missing from the catalog are
skipped (a dangling import surfaces as a diagnostic, never an error).

`examples/closure.rs` now runs this path over a real-module subset (the
YangModels `standard/ietf/RFC` tree, 486 files), RELEASE build:

| probe | modules+submodules | diags | RSS |
| --- | --- | --- | --- |
| catalog scan (whole 486-file tree) | — | — | ≈ 6.4 MB |
| roots=20 (default) | 41 + 0 | 5 | 22.4 MB |
| roots=50 | 75 + 0 | 6 | 30.4 MB |
| roots=100 | 119 + 1 | 10 | 48.9 MB |
| roots=200 | 204 + 12 | 25 | 81.4 MB |
| roots=200, text-light | 204 + 12 | 25 (same) | 73.1 MB (**−10%**) |

Readings confirm the serving model: memory grows with the modules actually
compiled (roughly linear ~0.3–0.4 MB/module through this closure, dominated by
statement/token structure + arena), NOT with the underlying tree size — the
whole 486-file tree costs only ~7 KB/file to keep indexed as a catalog.
Text-light applies its ~10% saving at closure-compile time too, with identical
diagnostics.

## Real giant tree — direct measurement (165k-file population, restored)

YangModels-scale population restored (165 521 `.yang`, ~4.9 GB; the vendor
collection alone is 163 378 files but only ~5.4k distinct module names —
huge per-name duplication explains the low per-file retention):

| probe | result |
| --- | --- |
| `catmem` sequential catalog, full tree | 228 MB RSS retained, 170 s wall (LTO release profile), ~1.2 KB/file avg (large-module early files ~4.6 KB/file, later small/duplicate files pull the mean down) |
| language server (sequential `fill_catalog`), full tree | 218 s wall, 255 MB RSS; opening a real RFC module adds ~0.4 MB; diagnostic pull 0.6 s, 0 errors |
| `serveperf` parallel catalog scan, full tree | 23.5 s wall (LTO release profile, ~7x over sequential), but RSS/VmHWM ~1.5–2.4 GB run-dependent — the 16-way transient parse high-water, NOT retained catalog (allocator keeps freed CST arenas) |

Conclusion: the serving catalog for the full giant population is ~230 MB
retained — comfortably in-process — and the language server serves a real
module from it with flat memory. The parallel scan is a wall-time win for
one-shot tools only; the RESIDENT server keeps the sequential scan
(`fill_catalog`) so its RSS stays ~255 MB instead of carrying a multi-GB
parse high-water.

## Post-fix catalog re-baseline (header-only scan, 2026-09-11)

The catalog path no longer builds a full document: `Catalog::scan` parses in
`ParseMode::HeaderOnly` (root + header statements only; no statement tree,
tokens, comments or errors) and the full path's quoted-fragment recovery is
now O(tokens + fragments) via a hash set. Measured with the new
`examples/scanbench.rs` on the restored population (165 521 files / 3 319 MiB,
release, `parallel`):

| mode | wall | VmHWM | user / sys |
| --- | --- | --- | --- |
| seq | 98.0 s | 174 MB | 96.4 / 1.7 s |
| par | 12.7 s | 914 MB | 198.4 / 3.5 s |
| par-canon (LS-style urls) | 13.0 s | 919 MB | 197.9 / 7.0 s |

Worst single file (5.96 MB Cisco NX module): `Catalog::scan` 4.97 s before →
**0.19 s** after with the same harness; the scan is now parse-bound (raw
grammar parse ≈0.14–0.16 s). The language server end-to-end (gnu + mimalloc,
fixed yrepo) scans the same tree in **13.0 s at 3.6 % sys** (previously
330–366 s at ~69 % sys on the static musl artifact). Earlier sequential
numbers in this document (170 s / 218 s) are superseded by the table above.

## Interpretation

1. Dropping the retained tree-sitter CST (no consumer; committed) cut ingest
   ~18% on the micro set — real but not the whole story.
2. Owned-string duplication (arg + token text) is ~1.1–1.3× source, plus one
   heap allocation per token/argument. The rope-range redesign (byte `Range`
   into the retained source, extract on demand) bounds its saving at roughly
   the duplicate bytes + allocator headers: ≈ 10–15% of ingest RSS on these
   samples, with public-API breakage on `Argument.logical`/`Token.text` (and
   callers that copy them out today). Worth prototyping, not a silver bullet.
3. ~90–95% of per-file RSS is per-statement/per-token STRUCTURE (recursive
   `Statement` owning `Vec<Statement>` children, token structs, per-node
   allocations), not text. The larger levers are layout/arena and
   retention policy (e.g., an index-only workspace scan that keeps light
   views instead of full parse views for non-open documents), plus re-parsing
   on demand instead of keeping every document live.

## Open questions / next candidates

- Allocator-level profile (allocation counts/sizes per parse) to confirm the
  structural split before choosing rope-range vs arena/layout work.
- Retention policy for the language-server giant-workspace scan: full parse
  of every file at open time is the current design; an index-only mode (keep
  header/name/imports, drop statement/token/comment views for closed docs)
  would bound memory for 163k-scale trees.
- Re-run memstep/memcomp on the real-shaped corpus and at the giant scale
  when available; log per 10k files with sleeps for waterline curves.
