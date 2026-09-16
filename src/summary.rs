//! Parse-level module-summary extraction: header facts (name, namespace,
//! revision) plus the names of a module/submodule's direct top-level schema
//! body — data nodes, `rpc`s and `notification`s — WITHOUT compiling, building
//! a full statement tree, or retaining tokens/comments/parse errors.
//!
//! This is the cheap "what does this file declare?" scan for tree-wide
//! navigation (namespace → module lookup, outline/index building), the sibling
//! of the header-only [`crate::Catalog`] scan. See `syntax::ParseMode::Summary`.
//!
//! A submodule is summarized like a module (its header comes from
//! `belongs-to`), and [`SummaryIndex::scan_many_files_with`] then **folds** a
//! submodule's top-level names into the summary of the module it belongs to:
//! the submodule has no namespace of its own, so a namespace-keyed projection
//! would otherwise lose the nodes it declares.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::syntax::{self, ParseMode};

/// A parse-level summary of one module/submodule document: its header facts
/// plus the names of its direct top-level data nodes, `rpc`s and
/// `notification`s. No compiled schema, no full statement tree.
#[derive(Debug, Clone)]
pub struct ModuleSummary {
    /// Source document url.
    pub url: Arc<str>,
    /// Module or submodule name.
    pub name: String,
    /// Namespace URI (modules only; `None` for a submodule or when absent).
    pub namespace: Option<String>,
    /// Latest `revision` date, if present (ISO strings compare
    /// lexicographically).
    pub revision: Option<String>,
    /// Top-level data node names (`container`/`leaf`/`leaf-list`/`list`/
    /// `anyxml`/`anydata`), in source order.
    pub top_data: Vec<String>,
    /// Top-level `rpc` names, in source order.
    pub rpcs: Vec<String>,
    /// Top-level `notification` names, in source order.
    pub notifications: Vec<String>,
}

impl ModuleSummary {
    /// Parse `source` in summary mode and keep only the header fields and the
    /// top-level name lists. The tree-sitter CST is transient and dropped, and
    /// no tokens/comments/parse errors or deeper statements are built.
    pub fn scan(url: impl Into<Arc<str>>, source: impl Into<String>) -> ModuleSummary {
        let url = url.into();
        scan_parts(url, source.into()).0
    }
}

/// One summary parse: the retained [`ModuleSummary`] plus the private scan
/// metadata used to fold a submodule's names into its parent module.
fn scan_parts(url: Arc<str>, source: String) -> (ModuleSummary, ScanMeta) {
    let parsed = syntax::parse_with(source, ParseMode::Summary);
    let header = crate::yang::extract_header(parsed.root.as_ref());
    let scan = parsed.summary.unwrap_or_default();
    let meta = ScanMeta {
        parse_ok: parsed.parse_ok,
        // `belongs-to` is present only on a submodule; it names the module
        // whose summary absorbs this submodule's top-level names.
        belongs_to: header.belongs_to.as_ref().map(|(parent, _)| parent.clone()),
    };
    (
        ModuleSummary {
            url,
            name: header.name.unwrap_or_default(),
            namespace: header.namespace,
            revision: header.revision,
            top_data: scan.top_data,
            rpcs: scan.rpcs,
            notifications: scan.notifications,
        },
        meta,
    )
}

/// Private per-document scan metadata: the parse status used for tie-breaking
/// and the `belongs-to` parent that makes a submodule's names fold into its
/// module's summary.
#[derive(Debug, Clone, Default)]
struct ScanMeta {
    /// True when the summary parse saw no `ERROR`/`MISSING` node.
    parse_ok: bool,
    /// `Some(parent)` for a submodule (`belongs-to`), `None` for a module.
    belongs_to: Option<String>,
}

/// One indexed summary plus the parse status used only for tie-breaking.
#[derive(Debug, Clone)]
struct Entry {
    summary: ModuleSummary,
    meta: ScanMeta,
}

/// The top-level names every submodule of one parent module contributes,
/// gathered before they are folded into the parent's [`ModuleSummary`].
#[derive(Debug, Default)]
struct FoldedNames {
    top_data: Vec<String>,
    rpcs: Vec<String>,
    notifications: Vec<String>,
}

/// An in-memory index of [`ModuleSummary`] documents, indexed by namespace for
/// `namespace → module` resolution.
#[derive(Debug, Default)]
pub struct SummaryIndex {
    by_namespace: HashMap<String, Vec<usize>>,
    entries: Vec<Entry>,
}

impl SummaryIndex {
    /// Read and summarize every file path in `paths`, the summary-mode sibling
    /// of [`crate::CatalogIndex::scan_many_files_with`]: files are read *and*
    /// transient-parsed off-thread when the `parallel` feature is on (a plain
    /// sequential loop otherwise), and each worker keeps only its in-flight
    /// file, so scan memory stays flat however large the tree. The entry url
    /// for each file comes from `url_for`; unreadable files are skipped, never
    /// an error. Returns how many files were summarized.
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
        let scanned: Vec<Option<(ModuleSummary, ScanMeta)>> =
            crate::compile::map_par(&paths, |p| {
                let url = url_for(p)?;
                std::fs::read_to_string(p)
                    .ok()
                    .map(|text| scan_parts(url.into(), text))
            });
        let mut n = 0usize;
        for (summary, meta) in scanned.into_iter().flatten() {
            self.push_with(summary, meta);
            n += 1;
        }
        self.fold_submodules();
        n
    }

    /// Insert one summarized document (callers feed entries in any order).
    fn push_with(&mut self, summary: ModuleSummary, meta: ScanMeta) {
        let i = self.entries.len();
        if let Some(ns) = summary.namespace.clone().filter(|s| !s.is_empty()) {
            self.by_namespace.entry(ns).or_default().push(i);
        }
        self.entries.push(Entry { summary, meta });
    }

    /// Fold every submodule's top-level names into the summary of the module it
    /// `belongs-to`.
    ///
    /// A submodule has no namespace of its own, so a namespace-keyed projection
    /// (e.g. the language server's Tier-1 `ModuleInfo`) would drop the data
    /// nodes, `rpc`s and `notification`s it declares — even though at run time
    /// they live in the parent module's namespace. The fold is
    /// revision-agnostic: a name declared by any submodule of a module is added
    /// to every indexed revision of that module. That can only over-approximate
    /// a completion/suggestion list across revisions, never invent an
    /// unrelated module's name. The submodule entries themselves are kept
    /// unchanged (still namespace-less, so still not namespace-resolvable).
    fn fold_submodules(&mut self) {
        // Gather first (owned) so the mutable pass below does not borrow
        // `self.entries` immutably.
        let mut by_parent: HashMap<String, FoldedNames> = HashMap::new();
        for e in &self.entries {
            let Some(parent) = e.meta.belongs_to.as_ref() else {
                continue;
            };
            let slot = by_parent.entry(parent.clone()).or_default();
            slot.top_data.extend(e.summary.top_data.iter().cloned());
            slot.rpcs.extend(e.summary.rpcs.iter().cloned());
            slot.notifications
                .extend(e.summary.notifications.iter().cloned());
        }
        if by_parent.is_empty() {
            return;
        }
        for e in &mut self.entries {
            if e.meta.belongs_to.is_some() {
                continue; // only a module's summary absorbs submodules
            }
            let Some(folded) = by_parent.get(&e.summary.name) else {
                continue;
            };
            extend_unique(&mut e.summary.top_data, &folded.top_data);
            extend_unique(&mut e.summary.rpcs, &folded.rpcs);
            extend_unique(&mut e.summary.notifications, &folded.notifications);
        }
    }

    /// The summary of the highest-revision document declaring `namespace`
    /// (parse-clean wins among equal revisions). Returns `None` for an unknown
    /// or empty namespace.
    pub fn resolve_namespace(&self, namespace: &str) -> Option<&ModuleSummary> {
        let idx = self.by_namespace.get(namespace)?;
        idx.iter()
            .copied()
            .max_by(|&a, &b| {
                let a = &self.entries[a];
                let b = &self.entries[b];
                let ra = a.summary.revision.clone().unwrap_or_default();
                let rb = b.summary.revision.clone().unwrap_or_default();
                ra.cmp(&rb)
                    .then_with(|| b.meta.parse_ok.cmp(&a.meta.parse_ok))
            })
            .map(|i| &self.entries[i].summary)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every indexed summary, in scan (insertion) order. Lets a caller build
    /// an index-wide projection (e.g. the language server's `ModuleInfo`
    /// summaries for instance-document classification and root completion).
    pub fn summaries(&self) -> impl Iterator<Item = &ModuleSummary> {
        self.entries.iter().map(|e| &e.summary)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Append the names of `src` that are not already in `dst`, preserving order.
/// The result is a stable union used when folding submodule names into a parent
/// module summary.
fn extend_unique(dst: &mut Vec<String>, src: &[String]) {
    let mut seen: HashSet<String> = dst.iter().cloned().collect();
    for name in src {
        if seen.insert(name.clone()) {
            dst.push(name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::NodeKind;

    const MODULE: &str = r#"
module demo {
  namespace "urn:demo";
  prefix d;
  revision 2023-07-01;
  container c { leaf inner { type string; } }
  leaf l { type string; }
  list li { key "k"; leaf k { type string; } }
  leaf-list ll { type string; }
  anydata ad;
  rpc do { input { leaf n { type string; } } }
  notification note { leaf what { type string; } }
}
"#;

    const SUBMODULE: &str = r#"
submodule demo-sub {
  belongs-to demo { prefix d; }
  revision 2022-01-01;
  container sc { leaf inner { type string; } }
  rpc sdo;
}
"#;

    /// The summary header fields must match the full `Yang` extraction, and the
    /// top-level name lists must match the `Library`'s compiled top nodes.
    #[test]
    fn summary_matches_yang_header_and_library_top_names() {
        for (url, src) in [("/m/demo.yang", MODULE), ("/m/demo-sub.yang", SUBMODULE)] {
            let summary = ModuleSummary::scan(url, src);
            let full = crate::yang::Yang::new(Arc::from(url), src.to_string());
            assert_eq!(summary.name, full.name.clone().unwrap_or_default(), "{url}");
            assert_eq!(summary.namespace, full.namespace, "{url}");
            assert_eq!(summary.revision, full.revision, "{url}");
        }

        let mut repo = crate::Repository::new();
        repo.upsert("/m/demo.yang", MODULE);
        let outcome = repo.compile();
        let lib = outcome.library.expect("demo compiles");
        let m = lib.module("demo").expect("demo module");

        let mut data = Vec::new();
        let mut rpcs = Vec::new();
        let mut notifications = Vec::new();
        for &id in m.top_nodes() {
            let node = m.node(id).expect("top node");
            match node.kind() {
                NodeKind::Container
                | NodeKind::Leaf
                | NodeKind::LeafList
                | NodeKind::List
                | NodeKind::Anyxml
                | NodeKind::Anydata => data.push(node.name().to_string()),
                NodeKind::Rpc => rpcs.push(node.name().to_string()),
                NodeKind::Notification => notifications.push(node.name().to_string()),
                _ => {}
            }
        }
        data.sort();
        rpcs.sort();
        notifications.sort();

        let summary = ModuleSummary::scan("/m/demo.yang", MODULE);
        let mut top_data = summary.top_data.clone();
        let mut summary_rpcs = summary.rpcs.clone();
        let mut summary_notifications = summary.notifications.clone();
        top_data.sort();
        summary_rpcs.sort();
        summary_notifications.sort();
        assert_eq!(top_data, data);
        assert_eq!(summary_rpcs, rpcs);
        assert_eq!(summary_notifications, notifications);
        // Sanity: the sample really exercises every supported bucket.
        assert_eq!(
            summary.top_data,
            vec!["c", "l", "li", "ll", "ad"],
            "source order preserved"
        );
        assert_eq!(summary.rpcs, vec!["do"]);
        assert_eq!(summary.notifications, vec!["note"]);
    }

    /// A submodule is summarized the same way (no namespace, header from
    /// `belongs-to`), and its own top-level data nodes are recorded.
    #[test]
    fn summary_handles_submodule() {
        let summary = ModuleSummary::scan("/m/demo-sub.yang", SUBMODULE);
        assert_eq!(summary.name, "demo-sub");
        assert_eq!(summary.namespace, None);
        assert_eq!(summary.revision.as_deref(), Some("2022-01-01"));
        assert_eq!(summary.top_data, vec!["sc"]);
        assert_eq!(summary.rpcs, vec!["sdo"]);
        assert!(summary.notifications.is_empty());
    }

    /// Nested statements are never recorded: only the root's direct children.
    #[test]
    fn summary_records_only_top_level_names() {
        let summary = ModuleSummary::scan(
            "/m/nested.yang",
            "module nested { namespace \"urn:n\"; prefix n;\n  container outer { leaf inner { type string; } list deep { key k; leaf k { type string; } } }\n}",
        );
        assert_eq!(summary.top_data, vec!["outer"]);
        assert!(summary.rpcs.is_empty());
        assert!(summary.notifications.is_empty());
    }

    /// Submodules are namespace-less, so a namespace-keyed projection would
    /// drop the top-level nodes they declare. `scan_many_files_with` folds them
    /// into the summary of the module they `belongs-to` (deduplicated, source
    /// order preserved), for data nodes, `rpc`s and `notification`s alike.
    #[test]
    fn submodule_top_level_names_fold_into_parent() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!(
            "yrepo-summary-fold-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, src: &str| {
            let p = dir.join(name);
            fs::write(&p, src).unwrap();
            p
        };
        let parent = write(
            "p.yang",
            "module p { namespace \"urn:p\"; prefix p; container own { leaf x { type string; } } rpc pr; }",
        );
        let sub_a = write(
            "p-sub-a.yang",
            "submodule p-sub-a { belongs-to p { prefix p; } container a { leaf x { type string; } } notification an; }",
        );
        // Declares `own` again (dedup) and adds a list plus an rpc.
        let sub_b = write(
            "p-sub-b.yang",
            "submodule p-sub-b { belongs-to p { prefix p; } leaf own { type string; } list b { key k; leaf k { type string; } } rpc br; }",
        );
        // An orphan submodule whose parent is not in the index must not panic.
        let orphan = write(
            "ghost-sub.yang",
            "submodule ghost-sub { belongs-to ghost { prefix g; } leaf gone { type string; } }",
        );

        let mut index = SummaryIndex::default();
        index.scan_many_files_with([&parent, &sub_a, &sub_b, &orphan], |p| {
            Some(format!("file://{}", p.display()))
        });

        let m = index.resolve_namespace("urn:p").expect("urn:p resolves");
        assert_eq!(m.top_data, vec!["own", "a", "b"], "order + dedup");
        assert_eq!(m.rpcs, vec!["pr", "br"]);
        assert_eq!(m.notifications, vec!["an"]);
        // The submodule summaries are still present and namespace-less, so a
        // namespace lookup never returns one.
        assert!(index.resolve_namespace("").is_none());
        let sub = index
            .summaries()
            .find(|s| s.name == "p-sub-a")
            .expect("submodule summary kept");
        assert_eq!(sub.namespace, None);
        assert_eq!(sub.top_data, vec!["a"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_many_files_with_resolves_namespace_preferring_latest_clean() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!(
            "yrepo-summary-scan-{}-{}",
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
        // Same namespace: one clean at a lower revision, one clean at the
        // highest revision, and a broken copy at that same highest revision.
        let older = write(
            "dup@2019-01-01.yang",
            "module dup { namespace \"urn:dup\"; prefix d; revision 2019-01-01; leaf old { type string; } }",
        );
        let latest_clean = write(
            "dup@2021-01-01.yang",
            "module dup { namespace \"urn:dup\"; prefix d; revision 2021-01-01; leaf a { type string; } }",
        );
        let latest_broken = write(
            "dup-broken.yang",
            "module dup { namespace \"urn:dup\"; prefix d; revision 2021-01-01; leaf b { type string;",
        );
        let other = write(
            "other.yang",
            "module other { namespace \"urn:other\"; prefix o; leaf x { type string; } }",
        );
        let sub = write(
            "dup-sub.yang",
            "submodule dup-sub { belongs-to dup { prefix d; } leaf s { type string; } }",
        );
        let paths = vec![
            older.clone(),
            latest_clean.clone(),
            latest_broken.clone(),
            other,
            sub,
        ];

        let mut index = SummaryIndex::default();
        assert!(index.is_empty());
        let n = index.scan_many_files_with(&paths, |p| Some(format!("file://{}", p.display())));
        assert_eq!(n, 5);
        assert_eq!(index.len(), 5);

        let winner = index
            .resolve_namespace("urn:dup")
            .expect("urn:dup resolves");
        assert!(
            winner.url.contains("2021-01-01"),
            "highest revision wins: {}",
            winner.url
        );
        assert!(
            !winner.url.contains("broken"),
            "parse-clean copy wins the equal-revision tie: {}",
            winner.url
        );
        assert_eq!(winner.revision.as_deref(), Some("2021-01-01"));
        // The submodule that belongs to `dup` has folded its `leaf s` into the
        // parent summary (see `submodule_top_level_names_fold_into_parent`).
        assert_eq!(winner.top_data, vec!["a", "s"]);

        assert!(index.resolve_namespace("urn:other").is_some());
        // A submodule has no namespace and is never namespace-resolvable.
        assert!(index.resolve_namespace("").is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn summaries_yields_every_indexed_entry_in_scan_order() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!(
            "yrepo-summary-iter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, src: &str| {
            let p = dir.join(name);
            fs::write(&p, src).unwrap();
            p
        };
        let a = write(
            "a.yang",
            "module a { namespace \"urn:a\"; prefix a; leaf x { type string; } }",
        );
        let b = write(
            "b.yang",
            "module b { namespace \"urn:b\"; prefix b; container c { leaf y { type string; } } }",
        );
        let mut index = SummaryIndex::default();
        index.scan_many_files_with([&a, &b], |p| Some(format!("file://{}", p.display())));
        let names: Vec<&str> = index.summaries().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"], "scan order is preserved");
        assert_eq!(index.summaries().count(), index.len());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_and_unknown_namespace_resolve_to_none() {
        let empty = SummaryIndex::default();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(empty.resolve_namespace("urn:missing").is_none());
        assert!(empty.resolve_namespace("").is_none());

        let mut index = SummaryIndex::default();
        let n = {
            let dir = std::env::temp_dir().join(format!(
                "yrepo-summary-none-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("m.yang");
            std::fs::write(
                &p,
                "module m { namespace \"urn:m\"; prefix m; leaf a { type string; } }",
            )
            .unwrap();
            let n = index.scan_many_files_with([p], |p| Some(p.to_string_lossy().to_string()));
            std::fs::remove_dir_all(&dir).ok();
            n
        };
        assert_eq!(n, 1);
        assert!(index.resolve_namespace("urn:m").is_some());
        assert!(index.resolve_namespace("urn:ghost").is_none());
    }
}
