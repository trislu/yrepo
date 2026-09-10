//! Catalog-only ingestion (memory goal): a minimal per-document record with
//! just the header facts a resolver needs (name, revision, prefix, namespace,
//! imports, includes, parse status), used to index very large trees WITHOUT
//! retaining full parse views or source text. Full documents are re-parsed on
//! demand when a module is actually opened/queried (see docs/memory-findings.md).
//!
//! `Catalog::scan` parses the document (tree-sitter CST is transient and
//! dropped) and copies out the header fields. Only the strings on the
//! returned `Catalog` are retained.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The light per-document catalog record.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// Source document url.
    pub url: Arc<str>,
    /// Module or submodule name.
    pub name: String,
    /// Latest `revision` date, if present.
    pub revision: Option<String>,
    /// The module's own prefix (or, for a submodule, the belongs-to prefix).
    pub prefix: Option<String>,
    /// Imported modules, each with the import's local prefix and an optional
    /// `revision-date` pin (resolution must honor the pin when present).
    pub imports: Vec<CatalogImport>,
    /// Included submodule names.
    pub includes: Vec<String>,
    /// True when the document parsed without a whole-file collapse.
    pub parse_ok: bool,
}

/// One top-level `import` in a cataloged module.
#[derive(Debug, Clone)]
pub struct CatalogImport {
    /// Imported module name.
    pub module: String,
    /// The import's local prefix.
    pub prefix: String,
    /// `revision-date` pin, if the import statement pins one.
    pub revision: Option<String>,
}

impl Catalog {
    /// Parse `source` and retain only the header facts. The parse is
    /// header-only (`ParseMode::HeaderOnly`): it builds just the
    /// module/submodule root and the header statements `extract_header` reads
    /// — no full statement tree, comments, tokens or parse errors — so the
    /// scan cost stays close to the raw tree-sitter parse.
    pub fn scan(url: impl Into<Arc<str>>, source: impl Into<String>) -> Catalog {
        let url = url.into();
        let source = source.into();
        let parsed = crate::syntax::parse_with(source, crate::syntax::ParseMode::HeaderOnly);
        let header = crate::yang::extract_header(parsed.root.as_ref());
        let parse_ok = parsed.parse_ok;
        Catalog {
            url,
            name: header.name.unwrap_or_default(),
            revision: header.revision,
            prefix: header.own_prefix,
            imports: header
                .imports
                .iter()
                .map(|i| CatalogImport {
                    module: i.module.clone(),
                    prefix: i.prefix.clone(),
                    revision: i.revision.clone(),
                })
                .collect(),
            includes: header.includes.iter().map(|i| i.name.clone()).collect(),
            parse_ok,
        }
    }
}

/// An in-memory catalog of scanned documents, indexed by module name (for
/// import resolution) and by document url (for open-buffer lookup).
#[derive(Debug, Default)]
pub struct CatalogIndex {
    by_name: std::collections::HashMap<String, Vec<usize>>,
    entries: Vec<Catalog>,
    by_url: std::collections::HashMap<Arc<str>, usize>,
}

impl CatalogIndex {
    /// Add one scanned document (callers feed entries in any order).
    pub fn push(&mut self, c: Catalog) {
        let url = c.url.clone();
        let i = self.entries.len();
        if let Some(name) = (!c.name.is_empty()).then(|| c.name.clone()) {
            self.by_name.entry(name).or_default().push(i);
        }
        self.entries.push(c);
        self.by_url.insert(url, i);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read and catalog every file path in `paths` (a whole tree handed as
    /// one batch, mirroring `Repository::upsert_many_files`): files are read
    /// *and* transient-parsed off-thread when the `parallel` feature is on
    /// (a plain sequential loop otherwise), and each worker keeps only its
    /// in-flight file, so scan memory stays flat however large the tree.
    /// Url of an entry is its path string (callers that need canonical file
    /// urls use [`CatalogIndex::scan_many_files_with`]). Returns how many
    /// files were cataloged (unreadable files are skipped, never an error).
    pub fn scan_many_files<I, P>(&mut self, paths: I) -> usize
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.scan_many_files_with(paths, |p| Some(p.to_string_lossy().to_string()))
    }

    /// Like [`CatalogIndex::scan_many_files`], but the entry url for each file
    /// comes from `url_for` (e.g. canonical `file://` urls matching a
    /// language server's document keys instead of the raw path string). The
    /// mapping runs inside the same parallel workers as the read+scan.
    /// Returns how many files were cataloged.
    pub fn scan_many_files_with<I, P, F>(&mut self, paths: I, url_for: F) -> usize
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
        F: Fn(&Path) -> Option<String> + Send + Sync,
    {
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .map(|p| p.as_ref().to_path_buf())
            .collect();
        let scanned: Vec<Option<Catalog>> = crate::compile::map_par(&paths, |p| {
            let url = url_for(p)?;
            std::fs::read_to_string(p)
                .ok()
                .map(|text| Catalog::scan(url, text))
        });
        let mut n = 0usize;
        for record in scanned.into_iter().flatten() {
            self.push(record);
            n += 1;
        }
        n
    }

    /// The catalog entry whose url matches `url`.
    pub fn of_url(&self, url: &str) -> Option<&Catalog> {
        self.by_url.get(url).map(|&i| &self.entries[i])
    }

    /// The highest-revision entry named `name` (canonical-latest; parse-clean
    /// wins among equal revisions).
    pub fn canonical(&self, name: &str) -> Option<&Catalog> {
        self.resolve(name, None)
    }

    /// Resolve `name` the way a reference does: when `revision` is given and
    /// an entry with that exact (name, revision-date) exists, prefer it
    /// (parse-clean first among equal copies); otherwise fall back to the
    /// canonical-latest entry — mirroring `compile`, where an import pinned
    /// with `revision-date` resolves to that exact revision first.
    pub fn resolve(&self, name: &str, revision: Option<&str>) -> Option<&Catalog> {
        let idx = self.by_name.get(name)?;
        let best = |cands: &[usize]| {
            cands.iter().copied().max_by(|&a, &b| {
                let a = &self.entries[a];
                let b = &self.entries[b];
                let ra = a.revision.clone().unwrap_or_default();
                let rb = b.revision.clone().unwrap_or_default();
                ra.cmp(&rb).then_with(|| b.parse_ok.cmp(&a.parse_ok))
            })
        };
        let pick = if let Some(rev) = revision {
            let pinned: Vec<usize> = idx
                .iter()
                .copied()
                .filter(|&i| self.entries[i].revision.as_deref() == Some(rev))
                .collect();
            best(&pinned).or_else(|| best(idx))
        } else {
            best(idx)
        };
        pick.map(|i| &self.entries[i])
    }

    /// Every distinct module name in the index, sorted (deterministic).
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.by_name.keys().cloned().collect();
        names.sort();
        names
    }

    /// Resolve `name`/`revision` by scanning **only** `candidates` into this
    /// index first (lazy startup-catalog path): returns the winning url and
    /// how many candidate files were parsed. Nothing is parsed when the name
    /// is already resolvable; when the candidates yield no entry for `name`
    /// the result is `None` (the caller decides on a fallback). Resolution
    /// follows [`CatalogIndex::resolve`] exactly, so a lazily grown index and
    /// a whole-tree index pick the same winner.
    pub fn resolve_lazy<I, P, F>(
        &mut self,
        name: &str,
        revision: Option<&str>,
        candidates: I,
        url_for: F,
    ) -> (Option<String>, usize)
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
        F: Fn(&Path) -> Option<String> + Send + Sync,
    {
        if let Some(entry) = self.resolve(name, revision) {
            return (Some(entry.url.to_string()), 0);
        }
        let parsed = self.scan_many_files_with(candidates, url_for);
        let winner = self.resolve(name, revision).map(|e| e.url.to_string());
        (winner, parsed)
    }
}

/// A cheap path index built from a directory walk **without parsing**: maps
/// each basename (minus the `@revision-date` suffix) to its candidate files,
/// sorted for deterministic tie-breaks. This is what lets startup defer all
/// header parsing: a needed name is resolved later by parsing only its
/// candidates (see [`CatalogIndex::resolve_lazy`]).
#[derive(Debug, Default)]
pub struct PathIndex {
    by_name: std::collections::HashMap<String, Vec<PathBuf>>,
}

impl PathIndex {
    /// Build the index from file paths. Files whose basename is empty are
    /// skipped; the filename suffix `@YYYY-MM-DD…` is dropped (YANG
    /// identifiers cannot contain `@`, so the first `@` starts the suffix).
    pub fn build<I, P>(paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut by_name: std::collections::HashMap<String, Vec<PathBuf>> =
            std::collections::HashMap::new();
        for path in paths {
            let path = path.as_ref();
            if let Some(name) = module_name_key(path) {
                by_name.entry(name).or_default().push(path.to_path_buf());
            }
        }
        for candidates in by_name.values_mut() {
            candidates.sort();
        }
        PathIndex { by_name }
    }

    /// Candidate files for a module/submodule name (sorted; empty when the
    /// name has no filename match and a fallback is required).
    pub fn candidates(&self, name: &str) -> &[PathBuf] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Number of distinct names in the index.
    pub fn names_len(&self) -> usize {
        self.by_name.len()
    }

    /// Number of files in the index.
    pub fn file_count(&self) -> usize {
        self.by_name.values().map(Vec::len).sum()
    }
}

/// Basename (minus extension and `@revision-date` suffix) of a path.
fn module_name_key(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let base = stem.split('@').next().unwrap_or(stem);
    (!base.is_empty()).then(|| base.to_string())
}

/// Build a Repository that contains `roots` and the full reachable closure
/// through the catalog: import edges (module names) plus include edges
/// (submodule names — submodules are separate documents folded into their
/// parent at compile time). Documents are read on demand via `read`
/// (url -> source) and parsed with `light` (text-light mode) when set.
/// A name that cannot be resolved in the catalog is skipped, never an error
/// (mirrors import resolution: dangling imports surface as diagnostics).
pub fn build_closure_repository(
    index: &CatalogIndex,
    roots: &[String],
    light: bool,
    read: &dyn Fn(&str) -> Option<String>,
) -> crate::Repository {
    let mut repo = crate::Repository::new();
    repo.set_text_light(light);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: Vec<String> = roots.to_vec();
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(entry) = index.canonical(&name) else {
            continue; // unresolved import/include: not in this tree
        };
        let Some(source) = read(&entry.url) else {
            continue; // file missing on disk: open doc may still supply it later
        };
        repo.upsert(entry.url.clone().to_string(), source);
        for imp in &entry.imports {
            if !seen.contains(&imp.module) {
                queue.push(imp.module.clone());
            }
        }
        for sub in &entry.includes {
            if !seen.contains(sub) {
                queue.push(sub.clone());
            }
        }
    }
    repo
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_set(
        files: &[(&str, &str)],
    ) -> (CatalogIndex, std::collections::HashMap<String, String>) {
        let mut index = CatalogIndex::default();
        let mut sources = std::collections::HashMap::new();
        for (url, src) in files {
            sources.insert((*url).to_string(), (*src).to_string());
            index.push(Catalog::scan(*url, *src));
        }
        (index, sources)
    }

    #[test]
    fn index_canonical_picks_latest_clean_revision() {
        let (index, _) = scan_set(&[
            (
                "/m/m.yang",
                "module m { namespace \"urn:m\"; prefix m; revision 2020-01-01; leaf a { type string; } }",
            ),
            (
                "/m/m-old.yang",
                "module m { namespace \"urn:m\"; prefix m; revision 2019-01-01; leaf b { type string; } }",
            ),
        ]);
        assert_eq!(index.len(), 2);
        let canon = index.canonical("m").expect("m indexed");
        assert_eq!(canon.url.as_ref(), "/m/m.yang");
        assert_eq!(canon.revision.as_deref(), Some("2020-01-01"));
        assert_eq!(
            index
                .of_url("/m/m-old.yang")
                .expect("by url")
                .revision
                .as_deref(),
            Some("2019-01-01")
        );
    }

    #[test]
    fn resolve_prefers_pinned_revision_then_falls_back_to_canonical() {
        let (index, _) = scan_set(&[
            (
                "/m/m.yang",
                "module m { namespace \"urn:m\"; prefix m; revision 2021-01-01; }",
            ),
            (
                "/m/m-old.yang",
                "module m { namespace \"urn:m\"; prefix m; revision 2019-01-01; }",
            ),
            (
                "/m/m-mid.yang",
                "module m { namespace \"urn:m\"; prefix m; revision 2020-01-01; }",
            ),
        ]);
        // Unpinned -> highest revision.
        assert_eq!(
            index.resolve("m", None).expect("canonical").url.as_ref(),
            "/m/m.yang"
        );
        // Pinned to an existing revision -> that exact file.
        assert_eq!(
            index
                .resolve("m", Some("2019-01-01"))
                .expect("pinned")
                .url
                .as_ref(),
            "/m/m-old.yang"
        );
        // Pinned to a revision absent from the catalog -> canonical fallback.
        assert_eq!(
            index
                .resolve("m", Some("2000-01-01"))
                .expect("fallback")
                .url
                .as_ref(),
            "/m/m.yang"
        );
        assert!(index.resolve("ghost", None).is_none());
    }

    #[test]
    fn catalog_keeps_import_pins() {
        let (index, _) = scan_set(&[(
            "/r/p.yang",
            "module p { namespace \"urn:p\"; prefix p;\n  import q { prefix q; revision-date 2019-01-01; }\n  import r { prefix r; }\n}",
        )]);
        let entry = index.of_url("/r/p.yang").expect("p indexed");
        assert_eq!(entry.imports.len(), 2);
        assert_eq!(entry.imports[0].module, "q");
        assert_eq!(entry.imports[0].revision.as_deref(), Some("2019-01-01"));
        assert_eq!(entry.imports[1].module, "r");
        assert_eq!(entry.imports[1].revision, None);
    }

    #[test]
    fn closure_loads_import_and_include_transitively() {
        let files: &[(&str, &str)] = &[
            (
                "/r/a.yang",
                "module a { namespace \"urn:a\"; prefix a;\n  import b { prefix b; }\n  include a-sub;\n  leaf x { type string; }\n}",
            ),
            (
                "/r/b.yang",
                "module b { namespace \"urn:b\"; prefix b;\n  import c { prefix c; }\n  leaf y { type string; }\n}",
            ),
            (
                "/r/c.yang",
                "module c { namespace \"urn:c\"; prefix c;\n  leaf z { type string; }\n}",
            ),
            (
                "/r/a-sub.yang",
                "submodule a-sub { belongs-to a { prefix a; }\n  leaf hidden { type string; }\n}",
            ),
        ];
        let (index, sources) = scan_set(files);
        let repo = build_closure_repository(&index, &["a".to_string()], false, &|url| {
            sources.get(url).cloned()
        });
        let outcome = repo.compile();
        // import + include edges both walked: a, b, c compile; the submodule
        // is folded into a (not a separate module).
        let lib = outcome.library.expect("closure compiles");
        assert!(lib.module("a").is_some());
        assert!(lib.module("b").is_some());
        assert!(lib.module("c").is_some());
        assert!(lib.module("a-sub").is_none());
    }

    #[test]
    fn closure_skips_unresolvable_names() {
        let (index, sources) = scan_set(&[(
            "/r/d.yang",
            "module d { namespace \"urn:d\"; prefix d;\n  import ghost { prefix g; }\n  leaf w { type string; }\n}",
        )]);
        let repo = build_closure_repository(&index, &["d".to_string()], false, &|url| {
            sources.get(url).cloned()
        });
        let outcome = repo.compile();
        assert!(
            outcome.library.is_some(),
            "dangling import is a diagnostic, not a hard failure"
        );
    }

    #[test]
    fn scan_many_files_batch_catalogs_disk_tree() {
        use std::fs;
        use std::path::PathBuf;
        let dir = std::env::temp_dir().join(format!(
            "yrepo-catalog-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = dir.join(format!("m{i}.yang"));
                fs::write(
                    &p,
                    format!("module m{i} {{ namespace \"urn:m{i}\"; prefix m{i}; leaf l {{ type string; }} }}"),
                )
                .unwrap();
                p
            })
            .collect();
        let mut index = CatalogIndex::default();
        let n = index.scan_many_files(&files);
        assert_eq!(n, 3);
        assert!(index.canonical("m1").is_some());
        assert!(index.of_url(&files[0].to_string_lossy()).is_some());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_many_files_with_applies_the_url_mapping() {
        use std::fs;
        use std::path::PathBuf;
        let dir = std::env::temp_dir().join(format!(
            "yrepo-catalog-scan-with-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = dir.join(format!("w{i}.yang"));
                fs::write(
                    &p,
                    format!("module w{i} {{ namespace \"urn:w{i}\"; prefix w{i}; }}"),
                )
                .unwrap();
                p
            })
            .collect();
        let mut index = CatalogIndex::default();
        let n = index.scan_many_files_with(&files, |p| Some(format!("file://{}", p.display())));
        assert_eq!(n, 3);
        assert!(
            index
                .of_url(&format!("file://{}", files[1].display()))
                .is_some()
        );
        assert!(index.of_url(&files[1].to_string_lossy()).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    /// The header-only scan must keep the exact shape `extract_header` reads:
    /// every header field has to match the full (`Yang`) extraction.
    #[test]
    fn header_only_scan_matches_full_extraction() {
        let sources: &[(&str, &str)] = &[
            (
                "/m/plain.yang",
                "module plain { namespace \"urn:plain\"; prefix p; revision 2021-01-01; leaf a { type string; } }",
            ),
            (
                "/m/imports.yang",
                "module imports { namespace \"urn:i\"; prefix i;\n  import q { prefix q; revision-date 2019-01-01; }\n  import r { prefix r; }\n  include imported-sub;\n  revision 2022-05-05;\n}",
            ),
            (
                "/m/sub.yang",
                "submodule sub { belongs-to parent { prefix pa; }\n  import q { prefix q; }\n}",
            ),
            (
                "/m/concat.yang",
                "module concat { namespace \"urn:base/\" + \"urn:more\"; prefix c; }",
            ),
        ];
        for (url, src) in sources {
            let scanned = Catalog::scan(*url, *src);
            let full = crate::yang::Yang::new(std::sync::Arc::from(*url), (*src).to_string());
            assert_eq!(scanned.name, full.name.clone().unwrap_or_default(), "{url}");
            assert_eq!(scanned.revision, full.revision, "{url}");
            assert_eq!(scanned.prefix, full.own_prefix, "{url}");
            assert_eq!(scanned.parse_ok, full.parse_errors.is_empty(), "{url}");
            assert_eq!(scanned.imports.len(), full.imports.len(), "{url}");
            for (s, f) in scanned.imports.iter().zip(&full.imports) {
                assert_eq!(
                    (s.module.as_str(), s.prefix.as_str()),
                    (f.module.as_str(), f.prefix.as_str()),
                    "{url}"
                );
                assert_eq!(s.revision, f.revision, "{url}");
            }
            let scanned_includes: Vec<&str> = scanned.includes.iter().map(String::as_str).collect();
            let full_includes: Vec<&str> = full.includes.iter().map(|i| i.name.as_str()).collect();
            assert_eq!(scanned_includes, full_includes, "{url}");
        }
    }

    #[test]
    fn header_only_scan_reports_parse_status() {
        assert!(
            Catalog::scan(
                "/m/ok.yang",
                "module ok { namespace \"urn:ok\"; prefix o; }"
            )
            .parse_ok
        );
        assert!(
            !Catalog::scan("/m/bad.yang", "module bad { namespace \"urn:bad\"; prefix").parse_ok
        );
    }

    #[test]
    fn path_index_groups_basenames_and_strips_revision_suffix() {
        let index = PathIndex::build([
            "/w/m@2021-01-01.yang",
            "/w/m@2019-01-01.yang",
            "/w/other.yang",
        ]);
        assert_eq!(index.names_len(), 2);
        assert_eq!(index.file_count(), 3);
        let candidates = index.candidates("m");
        assert_eq!(candidates.len(), 2);
        assert!(
            candidates[0].to_string_lossy().contains("2019"),
            "candidates are sorted deterministically"
        );
        assert!(index.candidates("ghost").is_empty());
    }

    #[test]
    fn resolve_lazy_scans_only_candidates_and_matches_full_resolve() {
        use std::fs;
        use std::path::PathBuf;
        let dir = std::env::temp_dir().join(format!(
            "yrepo-lazy-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let write = |file: &str, src: &str| -> PathBuf {
            let p = dir.join(file);
            fs::write(&p, src).unwrap();
            p
        };
        let old = write(
            "m@2019-01-01.yang",
            "module m { namespace \"urn:m\"; prefix m; revision 2019-01-01; }",
        );
        let new = write(
            "m@2021-01-01.yang",
            "module m { namespace \"urn:m\"; prefix m; revision 2021-01-01; }",
        );
        let other = write(
            "other.yang",
            "module other { namespace \"urn:o\"; prefix o; }",
        );
        let paths = PathIndex::build([old.clone(), new.clone(), other]);
        let url_of = |p: &std::path::Path| Some(format!("file://{}", p.display()));

        let mut index = CatalogIndex::default();
        let (winner, parsed) = index.resolve_lazy("m", None, paths.candidates("m").iter(), url_of);
        assert_eq!(parsed, 2, "both candidates parsed once");
        assert!(winner.as_deref().unwrap().contains("2021"));

        // Cached: a second lookup parses nothing.
        let (again, parsed_again) =
            index.resolve_lazy("m", None, paths.candidates("m").iter(), url_of);
        assert_eq!(parsed_again, 0);
        assert_eq!(again, winner);

        // A pinned revision resolves without new parses too.
        let (pinned, parsed_pin) = index.resolve_lazy(
            "m",
            Some("2019-01-01"),
            paths.candidates("m").iter(),
            url_of,
        );
        assert!(pinned.as_deref().unwrap().contains("2019"));
        assert_eq!(parsed_pin, 0);

        // No filename candidates: no parse, no winner (caller falls back).
        let (missing, parsed_missing) =
            index.resolve_lazy("ghost", None, paths.candidates("ghost").iter(), url_of);
        assert!(missing.is_none());
        assert_eq!(parsed_missing, 0);
        fs::remove_dir_all(&dir).ok();
    }
}
