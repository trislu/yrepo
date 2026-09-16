//! `Library`: the resolved semantic database ([D1]) + `Outcome`.
//!
//! Cheap to clone/snapshot (`Arc`). All lookups are read-only.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::diag::Diagnostic;
use crate::schema::{
    BUILTIN_TYPES, ExtensionDef, FeatureDef, Grouping, Identity, IdentityRef, IdentityResolution,
    ModuleRecord, SchemaNode, SubmoduleRecord, TypeCandidate, TypeCandidateKind, TypeResolution,
    TypeStep, Typedef,
};
use crate::value::{Accum, ValueType, classify};

/// The result of `Repository::compile()`: diagnostics plus, when at least one
/// module compiled, a snapshot `Library`. Content problems are *never* errors
/// here ([D3]).
pub struct Outcome {
    pub library: Option<Arc<Library>>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Result of an `identityref` value check ([`Library::check_identityref`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityStatus {
    /// The value names an existing identity that is the `base` or derived from it.
    Ok,
    /// The value's module qualifier (or the identity) could not be resolved.
    UnknownIdentity,
    /// The identity exists but is neither the `base` nor derived from it.
    NotDerived,
}

/// The resolved, queryable database for a workspace.
pub struct Library {
    modules: Vec<ModuleRecord>,
    /// module name -> index of the "latest revision" module (name-only lookup).
    latest: HashMap<String, usize>,
    /// (name, revision) -> index.
    by_rev: HashMap<(String, String), usize>,
    submodules: Vec<SubmoduleRecord>,
    sub_by_name: HashMap<String, Vec<usize>>,
}

impl Library {
    pub(crate) fn from_parts(
        modules: Vec<ModuleRecord>,
        submodules: Vec<SubmoduleRecord>,
    ) -> Library {
        let mut latest: HashMap<String, usize> = HashMap::new();
        let mut by_rev: HashMap<(String, String), usize> = HashMap::new();
        for (i, m) in modules.iter().enumerate() {
            let rev = m.revision.clone().unwrap_or_default();
            by_rev.insert((m.name.clone(), rev), i);
            let replace = match latest.get(&m.name) {
                None => true,
                Some(&cur) => {
                    let cur_rev = modules[cur].revision.as_deref().unwrap_or("");
                    let new_rev = m.revision.as_deref().unwrap_or("");
                    new_rev > cur_rev
                }
            };
            if replace {
                latest.insert(m.name.clone(), i);
            }
        }

        let mut sub_by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, s) in submodules.iter().enumerate() {
            sub_by_name.entry(s.name.clone()).or_default().push(i);
        }

        Library {
            modules,
            latest,
            by_rev,
            submodules,
            sub_by_name,
        }
    }

    // ---- modules --------------------------------------------------------

    /// All compiled modules (submodules folded into their parents).
    pub fn modules(&self) -> &[ModuleRecord] {
        &self.modules
    }

    /// Look up a module by name, resolving to its **latest** revision.
    ///
    /// Rustdoc (name-only variant): a module with no `revision` is valid and
    /// its revision is treated as empty. When several revisions of one name are
    /// loaded the most recent is returned; use [`Library::module_rev`] to pick
    /// an exact one.
    pub fn module(&self, name: &str) -> Option<&ModuleRecord> {
        self.latest.get(name).map(|&i| &self.modules[i])
    }

    /// Look up a module by its **exact** `(name, revision)`.
    ///
    /// Rustdoc (rev-required variant): `revision` is the value of the module's
    /// `revision` statement (e.g. `"2026-01-31"`). A module written without a
    /// `revision` is registered under the empty revision. Returns `None` when
    /// that exact revision is not loaded.
    pub fn module_rev(&self, name: &str, revision: &str) -> Option<&ModuleRecord> {
        self.by_rev
            .get(&(name.to_string(), revision.to_string()))
            .map(|&i| &self.modules[i])
    }

    // ---- modules by namespace -------------------------------------------

    /// Every module declaring `ns` as its `namespace`. Usually zero or one;
    /// several modules/revisions can share a namespace, so callers must
    /// disambiguate (e.g. by local-name uniqueness).
    pub fn modules_by_namespace(&self, ns: &str) -> Vec<&ModuleRecord> {
        self.modules
            .iter()
            .filter(|m| m.namespace() == Some(ns))
            .collect()
    }

    // ---- submodules -----------------------------------------------------

    pub fn submodules(&self) -> &[SubmoduleRecord] {
        &self.submodules
    }

    /// Look up a submodule document by name. Useful for `goto` on `include`
    /// arguments ([D6]).
    pub fn submodule(&self, name: &str) -> Option<&SubmoduleRecord> {
        self.sub_by_name
            .get(name)
            .and_then(|v| v.first())
            .map(|&i| &self.submodules[i])
    }

    // ---- symbols --------------------------------------------------------

    pub fn search_type(&self, module: &str, type_name: &str) -> Option<&Typedef> {
        self.module(module)?
            .typedefs
            .iter()
            .find(|t| t.name == type_name)
    }

    pub fn search_grouping(&self, module: &str, grouping: &str) -> Option<&Grouping> {
        self.module(module)?
            .groupings
            .iter()
            .find(|g| g.name == grouping)
    }

    pub fn search_identity(&self, module: &str, identity: &str) -> Option<&Identity> {
        self.module(module)?
            .identities
            .iter()
            .find(|i| i.name == identity)
    }

    pub fn search_extension(&self, module: &str, extension: &str) -> Option<&ExtensionDef> {
        self.module(module)?
            .extensions
            .iter()
            .find(|e| e.name == extension)
    }

    pub fn search_feature(&self, module: &str, feature: &str) -> Option<&FeatureDef> {
        self.module(module)?
            .features
            .iter()
            .find(|f| f.name == feature)
    }

    // ---- prefixes -------------------------------------------------------

    /// Resolve a prefix to a module name, in the scope of `module`.
    ///
    /// Covers the module's own prefix, its imports, and (for folded submodule
    /// content) `belongs-to` prefixes.
    pub fn prefix_to_module(&self, module: &str, prefix: &str) -> Option<&str> {
        let m = self.module(module)?;
        m.prefix_map.get(prefix).map(|s| s.as_ref())
    }

    // ---- schema-nodeid resolution --------------------------------------

    /// Resolve an absolute (or descendant) schema-nodeid to an effective node.
    ///
    /// Backs goto/hover on `augment`/`refine`/`deviation` arguments and type
    /// references (D9). `path` may be prefix-qualified (`/if:x/if:y`) or
    /// bare (`/x/y`) — the first segment resolves against `module`'s scope.
    pub fn resolve_abs_schema_node_id(&self, module: &str, path: &str) -> Option<&SchemaNode> {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return None;
        }
        // Predicates (`[…]`) select list instances, never schema nodes: drop
        // them segment-wise so `/a:b/c:d[k='v']/e:f` walks a:b -> c:d -> e:f.
        fn strip_preds(seg: &str) -> &str {
            match seg.find('[') {
                Some(i) => &seg[..i],
                None => seg,
            }
        }
        let (target_name, first_local) = match segments[0].split_once(':') {
            Some((prefix, local)) => {
                let t = self.prefix_to_module(module, prefix)?;
                (t.to_string(), strip_preds(local))
            }
            None => (module.to_string(), strip_preds(segments[0])),
        };
        let target = self.module(&target_name)?;
        let mut current = target
            .top_nodes()
            .iter()
            .find(|&&id| {
                target
                    .node(id)
                    .map(|n| n.name() == first_local)
                    .unwrap_or(false)
            })
            .copied()?;
        for seg in &segments[1..] {
            let local = strip_preds(seg)
                .rsplit(':')
                .next()
                .unwrap_or(strip_preds(seg));
            let node = target.node(current)?;
            current = node
                .children()
                .iter()
                .find(|&&id| target.node(id).map(|n| n.name() == local).unwrap_or(false))
                .copied()?;
        }
        target.node(current)
    }

    /// Find the instance-visible child of `id` (in `module`'s arena) whose
    /// local name is `name` **and** whose instance module (the module owning
    /// its namespace in instance data, RFC 7950 §7.13) declares the namespace
    /// `ns`.
    ///
    /// Contrast [`ModuleRecord::data_child`], which matches the name only:
    /// this returns `None` on a namespace mismatch even when the name exists
    /// (→ "wrong namespace"), while the name-only call distinguishes "unknown
    /// node". Augmented and grouping-born children live in the target module's
    /// arena but keep their own instance module, so this searches through
    /// `choice`/`case` wrappers exactly like the data tree does.
    pub fn data_child(
        &self,
        module: &str,
        id: crate::schema::NodeId,
        ns: &str,
        name: &str,
    ) -> Option<crate::schema::NodeId> {
        let rec = self.module(module)?;
        for c in rec.data_children(id) {
            let Some(node) = rec.node(c) else {
                continue;
            };
            if node.name() != name {
                continue;
            }
            let owner_ns = self
                .module(node.instance_module())
                .and_then(|m| m.namespace());
            if owner_ns == Some(ns) {
                return Some(c);
            }
        }
        None
    }

    /// Render the canonical absolute-schema-nodeid of an effective node — the
    /// path from its module's top **including** `choice`/`case`/`input`/
    /// `output` wrappers — prefixing each segment by the **instance module**
    /// that owns the segment's namespace (RFC 7950 §7.13; a grouping-born
    /// node is addressed with the *using* module's prefix, not the grouping
    /// module's), falling back to the module name when it has no prefix.
    ///
    /// This is the *schema* path: instance documents express the shorter
    /// *data* path that skips the wrapper nodes, so a data path cannot be fed
    /// back to schema-path-based resolution verbatim.
    pub fn schema_nodeid(&self, module: &str, id: crate::schema::NodeId) -> Option<String> {
        let rec = self.module(module)?;
        let mut chain = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            chain.push(c);
            cur = rec.node(c).and_then(|n| n.parent());
        }
        chain.reverse();
        let mut segs = Vec::with_capacity(chain.len());
        for c in chain {
            let node = rec.node(c)?;
            let prefix = self
                .module(node.instance_module())
                .and_then(|m| m.prefix().map(str::to_owned))
                .unwrap_or_else(|| node.instance_module().to_string());
            segs.push(format!("{prefix}:{}", node.name()));
        }
        Some(format!("/{}", segs.join("/")))
    }

    // ---- type / identity resolution (existence + chain, D13) ------------

    /// Resolve `[prefix:]name` against `module`'s scope into `(module, local)`.
    /// `None` when the prefix is unmapped in `module`.
    fn qualify(&self, module: &str, text: &str) -> Option<(String, String)> {
        match text.split_once(':') {
            None => Some((module.to_string(), text.to_string())),
            Some((prefix, local)) => self
                .prefix_to_module(module, prefix)
                .map(|m| (m.to_string(), local.to_string())),
        }
    }

    /// Resolve a type reference — following the typedef chain, possibly across
    /// modules — down to a builtin type when possible.
    ///
    /// `None` when `type_name` is neither a builtin nor a typedef in scope.
    /// When the chain is resolvable but open (a base or typedef is missing),
    /// the returned [`TypeResolution`] has `complete == false`.
    ///
    /// ```text
    /// resolve_type("m", "service-port") ⇒ typedefs: [service-port, port], builtin: Some("uint16")
    /// ```
    pub fn resolve_type(&self, module: &str, type_name: &str) -> Option<TypeResolution> {
        if crate::schema::is_builtin_type(type_name) {
            return Some(TypeResolution {
                builtin: Some(type_name.to_string()),
                typedefs: Vec::new(),
                complete: true,
            });
        }
        let (m0, l0) = self.qualify(module, type_name)?;
        let first = self
            .module(&m0)?
            .typedefs
            .iter()
            .find(|t| t.name == l0)?
            .clone();

        let mut steps = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut cur_mod = m0;
        let mut cur = first;
        loop {
            let key = (cur_mod.clone(), cur.name.clone());
            if !seen.insert(key) {
                return Some(TypeResolution {
                    builtin: None,
                    typedefs: steps,
                    complete: false,
                });
            }
            steps.push(TypeStep {
                module: Arc::from(cur_mod.as_str()),
                name: cur.name.clone(),
                defining: cur.defining.clone(),
                base: cur.base.clone(),
            });
            let Some(base) = cur.base.as_deref() else {
                return Some(TypeResolution {
                    builtin: None,
                    typedefs: steps,
                    complete: false,
                });
            };
            if crate::schema::is_builtin_type(base) {
                return Some(TypeResolution {
                    builtin: Some(base.to_string()),
                    typedefs: steps,
                    complete: true,
                });
            }
            let (m1, l1) = match self.qualify(&cur_mod, base) {
                Some(x) => x,
                None => {
                    return Some(TypeResolution {
                        builtin: None,
                        typedefs: steps,
                        complete: false,
                    });
                }
            };
            let next = match self.module(&m1) {
                Some(r) => match r.typedefs.iter().find(|t| t.name == l1) {
                    Some(t) => t.clone(),
                    None => {
                        return Some(TypeResolution {
                            builtin: None,
                            typedefs: steps,
                            complete: false,
                        });
                    }
                },
                None => {
                    return Some(TypeResolution {
                        builtin: None,
                        typedefs: steps,
                        complete: false,
                    });
                }
            };
            cur_mod = m1;
            cur = next;
        }
    }

    /// Resolve a leaf/leaf-list's **value type** (D31/M5): reduce its `type`
    /// reference through the typedef chain to a builtin and classify it,
    /// accumulating the facets written along the way (`length`/`pattern`/
    /// `range`, `enum`/`bit` members, `leafref` `path`).
    ///
    /// `None` when `id` is not a typed node (no `type` statement). A chain
    /// that cannot be resolved, or a builtin we do not classify, yields
    /// [`ValueType::Unknown`]; `union` yields [`ValueType::Union`] (never
    /// checked, D31).
    pub fn value_type(&self, module: &str, id: crate::schema::NodeId) -> Option<ValueType> {
        let rec = self.module(module)?;
        let node = rec.node(id)?;
        let type_name = node.type_name()?;
        let mut acc = Accum::default();
        acc.fold(node.type_facets());
        let mut base: Option<&str> = None;
        if crate::schema::is_builtin_type(type_name) {
            base = Some(type_name);
        } else if let Some((m0, l0)) = self.qualify(module, type_name) {
            let mut cur_mod = m0;
            let mut local = l0;
            let mut steps = 0usize;
            while steps < 64 && base.is_none() {
                steps += 1;
                let Some(r) = self.module(&cur_mod) else {
                    break;
                };
                let Some(td) = r.typedefs.iter().find(|t| t.name == local) else {
                    break;
                };
                acc.fold(&td.facets);
                let Some(b) = td.base.as_deref() else {
                    break;
                };
                if crate::schema::is_builtin_type(b) {
                    base = Some(b);
                } else if let Some((m1, l1)) = self.qualify(&cur_mod, b) {
                    cur_mod = m1;
                    local = l1;
                } else {
                    break;
                }
            }
        }
        Some(match base {
            Some(b) => classify(b, &acc),
            None => ValueType::Unknown,
        })
    }

    /// Resolve an identity and the chain of its bases (its ancestry).
    pub fn resolve_identity(&self, module: &str, name: &str) -> Option<IdentityResolution> {
        let (m0, l0) = self.qualify(module, name)?;
        let rec = self.module(&m0)?;
        let root = rec.identities.iter().find(|i| i.name == l0)?.clone();
        let root_ref = IdentityRef {
            module: Arc::from(m0.as_str()),
            name: root.name.clone(),
            defining: root.defining.clone(),
            base: root.base.clone(),
        };
        let mut bases = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut cur_mod = m0;
        let mut cur = root;
        loop {
            if !seen.insert((cur_mod.clone(), cur.name.clone())) {
                break;
            }
            let Some(base) = cur.base.clone() else {
                break;
            };
            let Some((m1, l1)) = self.qualify(&cur_mod, &base) else {
                break;
            };
            let Some(next) = self
                .module(&m1)
                .and_then(|r| r.identities.iter().find(|i| i.name == l1))
                .cloned()
            else {
                break;
            };
            bases.push(IdentityRef {
                module: Arc::from(m1.as_str()),
                name: next.name.clone(),
                defining: next.defining.clone(),
                base: next.base.clone(),
            });
            cur_mod = m1;
            cur = next;
        }
        Some(IdentityResolution {
            root: root_ref,
            bases,
        })
    }

    /// Every identity that is `base` or is (transitively) derived from it —
    /// the value set an `identityref { base … }` accepts.
    pub fn derived_identities(&self, module: &str, base: &str) -> Vec<IdentityRef> {
        let Some((bm, bl)) = self.qualify(module, base) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for rec in &self.modules {
            for id in &rec.identities {
                if self.identity_reaches(&rec.name, id, &bm, &bl) {
                    out.push(IdentityRef {
                        module: Arc::from(rec.name.as_str()),
                        name: id.name.clone(),
                        defining: id.defining.clone(),
                        base: id.base.clone(),
                    });
                }
            }
        }
        out.sort_by(|a, b| (&*a.module, &a.name).cmp(&(&*b.module, &b.name)));
        out
    }

    fn identity_reaches(
        &self,
        module: &str,
        id: &Identity,
        target: &str,
        target_local: &str,
    ) -> bool {
        let mut cur_mod = module.to_string();
        let mut cur = id.clone();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        loop {
            if !seen.insert((cur_mod.clone(), cur.name.clone())) {
                return false;
            }
            if cur_mod == target && cur.name == target_local {
                return true;
            }
            let Some(base) = cur.base.clone() else {
                return false;
            };
            let Some((m1, l1)) = self.qualify(&cur_mod, &base) else {
                return false;
            };
            let Some(next) = self
                .module(&m1)
                .and_then(|r| r.identities.iter().find(|i| i.name == l1))
                .cloned()
            else {
                return false;
            };
            cur_mod = m1;
            cur = next;
        }
    }

    /// Resolve an identity QName in `module`'s scope: a qualifier that names a
    /// compiled module wins; otherwise it is treated as an import prefix.
    fn qresolve(&self, module: &str, q: &str) -> Option<(String, String)> {
        match q.split_once(':') {
            None => Some((module.to_string(), q.to_string())),
            Some((p, local)) => {
                if self.module(p).is_some() {
                    Some((p.to_string(), local.to_string()))
                } else {
                    self.prefix_to_module(module, p)
                        .map(|m| (m.to_string(), local.to_string()))
                }
            }
        }
    }

    /// Semantic check for an `identityref` leaf value (D31 / M5): `value` is a
    /// QName (`module:name`, or `prefix:name` in `module`'s scope). With `base`
    /// (the identityref's `base`, resolved in `module`'s scope) the identity
    /// must be the `base` or derived from it; with no `base`, any existing
    /// identity is accepted.
    pub fn check_identityref(
        &self,
        module: &str,
        base: Option<&str>,
        value: &str,
    ) -> IdentityStatus {
        let Some((vm, vl)) = self.qresolve(module, value) else {
            return IdentityStatus::UnknownIdentity;
        };
        self.check_identityref_in(module, base, &vm, &vl)
    }

    /// Semantic check for an `identityref` value whose **module the caller has
    /// already resolved**, so no YANG-prefix/name guessing is involved.
    /// Instance encodings resolve the module themselves: XML from the value's
    /// XML namespace prefix or in-scope default namespace (RFC 7950 §9.10.3),
    /// JSON from its `module:name` qualifier (RFC 7951 §6.8).
    ///
    /// `leaf_module` is the module the leaf is defined in and `base` its
    /// identityref `base`, resolved in `leaf_module`'s YANG scope. The identity
    /// `(value_module, value_local)` must exist and be `base` or derived from
    /// it; with no `base`, any existing identity is accepted. A `value_module`
    /// that is not compiled is [`IdentityStatus::UnknownIdentity`] — never an
    /// accidental match against a same-named prefix.
    pub fn check_identityref_in(
        &self,
        leaf_module: &str,
        base: Option<&str>,
        value_module: &str,
        value_local: &str,
    ) -> IdentityStatus {
        let Some(rec) = self.module(value_module) else {
            return IdentityStatus::UnknownIdentity;
        };
        let Some(id) = rec.identities.iter().find(|i| i.name == value_local) else {
            return IdentityStatus::UnknownIdentity;
        };
        let Some(base) = base else {
            return IdentityStatus::Ok;
        };
        // An unresolvable base is a schema problem; stay silent on values.
        let Some((bm, bl)) = self.qresolve(leaf_module, base) else {
            return IdentityStatus::Ok;
        };
        if self.identity_reaches(value_module, id, &bm, &bl) {
            IdentityStatus::Ok
        } else {
            IdentityStatus::NotDerived
        }
    }

    /// Completion candidates for a `type` argument: builtins, this module's
    /// typedefs, and imported typedefs as `prefix:name`.
    pub fn type_candidates(&self, module: &str) -> Vec<TypeCandidate> {
        let mut out: Vec<TypeCandidate> = BUILTIN_TYPES
            .iter()
            .map(|b| TypeCandidate {
                name: (*b).to_string(),
                kind: TypeCandidateKind::Builtin,
                module: None,
            })
            .collect();
        let Some(rec) = self.module(module) else {
            return out;
        };
        for t in &rec.typedefs {
            out.push(TypeCandidate {
                name: t.name.clone(),
                kind: TypeCandidateKind::Typedef,
                module: Some(module.to_string()),
            });
        }
        for imp in &rec.imports {
            if let Some(trec) = self.module(&imp.module) {
                for t in &trec.typedefs {
                    out.push(TypeCandidate {
                        name: format!("{}:{}", imp.prefix, t.name),
                        kind: TypeCandidateKind::Typedef,
                        module: Some(imp.module.clone()),
                    });
                }
            }
        }
        out
    }

    /// Completion candidates for an identity `base` argument: this module's
    /// identities plus imported ones as `prefix:name`.
    pub fn identity_candidates(&self, module: &str) -> Vec<String> {
        let mut out = Vec::new();
        let Some(rec) = self.module(module) else {
            return out;
        };
        for i in &rec.identities {
            out.push(i.name.clone());
        }
        for imp in &rec.imports {
            if let Some(irec) = self.module(&imp.module) {
                for i in &irec.identities {
                    out.push(format!("{}:{}", imp.prefix, i.name));
                }
            }
        }
        out
    }

    /// `(prefix, module name)` pairs for every import of `module` (in source
    /// order), for prefix completion of cross-module paths and references.
    pub fn import_prefixes(&self, module: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(rec) = self.module(module) {
            for imp in &rec.imports {
                out.push((imp.prefix.clone(), imp.module.clone()));
            }
        }
        out
    }

    /// Grouping names available for a `uses` argument: the module's own
    /// groupings (bare) plus imported modules' groupings as `prefix:name`.
    pub fn grouping_candidates(&self, module: &str) -> Vec<String> {
        let mut out = Vec::new();
        let Some(rec) = self.module(module) else {
            return out;
        };
        for g in rec.groupings() {
            out.push(g.name.clone());
        }
        for imp in &rec.imports {
            if let Some(irec) = self.module(&imp.module) {
                for g in irec.groupings() {
                    out.push(format!("{}:{}", imp.prefix, g.name));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::Repository;
    use crate::schema::NodeKind;

    use super::Library;

    /// Compile a set of `(url, source)` YANG documents into a `Library`.
    fn compile(src: &[(&str, &str)]) -> Arc<Library> {
        let mut repo = Repository::new();
        for (url, text) in src {
            repo.upsert(*url, *text);
        }
        repo.compile()
            .library
            .expect("module set should compile to a library")
    }

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn names(lib: &Library, module: &str, ids: &[usize]) -> Vec<String> {
        let rec = lib.module(module).unwrap();
        let mut out: Vec<String> = ids
            .iter()
            .map(|&i| rec.node(i).expect("node id").name().to_string())
            .collect();
        out.sort();
        out
    }

    /// Top-level node id by name (independent of the new data helpers).
    fn top_id(lib: &Library, module: &str, name: &str) -> usize {
        let rec = lib.module(module).unwrap();
        rec.top_nodes()
            .iter()
            .copied()
            .find(|&i| rec.node(i).unwrap().name() == name)
            .unwrap_or_else(|| panic!("no top node '{name}'"))
    }

    /// Descend through **direct** children by name, so test setup does not
    /// depend on the code under test.
    fn child_id(lib: &Library, module: &str, root: usize, path: &[&str]) -> usize {
        let rec = lib.module(module).unwrap();
        let mut cur = root;
        for name in path {
            cur = rec
                .node(cur)
                .unwrap()
                .children()
                .iter()
                .copied()
                .find(|&c| rec.node(c).unwrap().name() == *name)
                .unwrap_or_else(|| panic!("child '{name}' not found under {cur}"));
        }
        cur
    }

    /// Module A: container with a direct leaf + a `choice` (explicit case and
    /// a shorthand-case leaf) + an rpc with `input`/`output`.
    const MOD_A: &str = r#"module a {
  yang-version 1.1;
  namespace "urn:a";
  prefix a;
  revision 2026-01-01;
  container c {
    leaf direct { type string; }
    choice ch {
      case c1 { leaf l1 { type string; } }
      leaf short { type string; }
    }
  }
  rpc op {
    input { leaf arg1 { type string; } }
    output { leaf result { type string; } }
  }
}"#;

    /// Module B: augments a new leaf `x` into module A's `c`.
    const MOD_B: &str = r#"module b {
  yang-version 1.1;
  namespace "urn:b";
  prefix b;
  revision 2026-01-01;
  import a { prefix a; }
  augment "/a:c" { leaf x { type string; } }
}"#;

    #[test]
    fn node_kind_data_wrapper_classifiers() {
        assert!(NodeKind::Container.is_data());
        assert!(NodeKind::Leaf.is_data());
        assert!(NodeKind::LeafList.is_data());
        assert!(NodeKind::List.is_data());
        assert!(NodeKind::Anyxml.is_data());
        assert!(NodeKind::Anydata.is_data());
        assert!(!NodeKind::Rpc.is_data());
        assert!(!NodeKind::Notification.is_data());

        assert!(NodeKind::Choice.is_wrapper());
        assert!(NodeKind::Case.is_wrapper());
        assert!(NodeKind::Input.is_wrapper());
        assert!(NodeKind::Output.is_wrapper());
        assert!(!NodeKind::Leaf.is_wrapper());
        assert!(!NodeKind::Container.is_wrapper());
        assert!(!NodeKind::Rpc.is_wrapper());
    }

    #[test]
    fn data_children_skip_choice_case_wrappers() {
        let lib = compile(&[("/a.yang", MOD_A)]);
        let rec = lib.module("a").unwrap();

        // Container `c`: the choice's cases collapse to their data children;
        // `choice ch` / `case c1` / the shorthand `case short` never appear.
        let c = top_id(&lib, "a", "c");
        assert_eq!(
            names(&lib, "a", &rec.data_children(c)),
            v(&["direct", "l1", "short"])
        );

        // Choice and case are transparent to data navigation too.
        let ch = child_id(&lib, "a", c, &["ch"]);
        assert_eq!(
            names(&lib, "a", &rec.data_children(ch)),
            v(&["l1", "short"])
        );
        let c1 = child_id(&lib, "a", ch, &["c1"]);
        assert_eq!(names(&lib, "a", &rec.data_children(c1)), v(&["l1"]));

        // Name lookup through the wrappers, and unknown-name detection.
        let l1 = rec.data_child(c, "l1").expect("l1 is visible under c");
        assert_eq!(rec.node(l1).unwrap().kind(), NodeKind::Leaf);
        assert!(rec.data_child(c, "nope").is_none());
        assert!(
            rec.data_child(c, "ch").is_none(),
            "choice is not a data child"
        );
        assert!(
            rec.data_child(c, "c1").is_none(),
            "case is not a data child"
        );
    }

    #[test]
    fn rpc_input_output_data_direction() {
        let lib = compile(&[("/a.yang", MOD_A)]);
        let rec = lib.module("a").unwrap();

        let op = top_id(&lib, "a", "op");
        // The rpc's own instance-visible children exclude input/output.
        assert!(rec.data_children(op).is_empty());

        let input = rec.rpc_input(op).expect("synthesized input present");
        let output = rec.rpc_output(op).expect("synthesized output present");
        assert_eq!(rec.node(input).unwrap().kind(), NodeKind::Input);
        assert_eq!(rec.node(output).unwrap().kind(), NodeKind::Output);
        assert_eq!(names(&lib, "a", &rec.data_children(input)), v(&["arg1"]));
        assert_eq!(names(&lib, "a", &rec.data_children(output)), v(&["result"]));
    }

    #[test]
    fn schema_nodeid_includes_wrappers() {
        let lib = compile(&[("/a.yang", MOD_A)]);
        let c = top_id(&lib, "a", "c");
        let l1 = child_id(&lib, "a", c, &["ch", "c1", "l1"]);

        assert_eq!(lib.schema_nodeid("a", c).as_deref(), Some("/a:c"));
        // The data path is "/a:c/a:l1" — the schema path re-inserts
        // choice/case wrappers.
        assert_eq!(
            lib.schema_nodeid("a", l1).as_deref(),
            Some("/a:c/a:ch/a:c1/a:l1")
        );
    }

    #[test]
    fn augmented_children_and_namespace_lookup() {
        let lib = compile(&[("/a.yang", MOD_A), ("/b.yang", MOD_B)]);
        let rec_a = lib.module("a").unwrap();
        let c = top_id(&lib, "a", "c");

        // Augmented leaf `x` (module b) is a data child of a:c.
        assert_eq!(
            names(&lib, "a", &rec_a.data_children(c)),
            v(&["direct", "l1", "short", "x"])
        );
        let x = rec_a.data_child(c, "x").expect("x visible under a:c");
        assert_eq!(rec_a.node(x).unwrap().origin_module(), "b");

        // Namespace-aware lookup: x is urn:b; a bare-name match with the
        // wrong namespace is None (distinguishing "wrong ns" from "unknown").
        assert_eq!(lib.data_child("a", c, "urn:b", "x"), Some(x));
        assert_eq!(lib.data_child("a", c, "urn:a", "x"), None);
        assert_eq!(lib.data_child("a", c, "urn:b", "nope"), None);
        let direct = rec_a.data_child(c, "direct").unwrap();
        assert_eq!(lib.data_child("a", c, "urn:a", "direct"), Some(direct));

        // Augmented nodes render under their own module's prefix.
        assert_eq!(lib.schema_nodeid("a", x).as_deref(), Some("/a:c/b:x"));

        // Namespace index.
        assert_eq!(
            lib.modules_by_namespace("urn:b")
                .iter()
                .map(|m| m.name())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
        assert!(lib.modules_by_namespace("urn:missing").is_empty());
    }

    /// Module G: defines a grouping with a nested container + leaf.
    const MOD_G: &str = r#"module g {
  yang-version 1.1;
  namespace "urn:g";
  prefix g;
  revision 2026-01-01;
  grouping gg {
    container gc {
      leaf x { type string; }
    }
  }
}"#;

    /// Module U: uses module G's grouping inside its own container.
    const MOD_U: &str = r#"module u {
  yang-version 1.1;
  namespace "urn:u";
  prefix u;
  revision 2026-01-01;
  import g { prefix g; }
  container uc {
    uses g:gg;
  }
}"#;

    #[test]
    fn uses_born_nodes_take_the_using_modules_namespace() {
        let lib = compile(&[("/g.yang", MOD_G), ("/u.yang", MOD_U)]);
        let rec = lib.module("u").unwrap();
        let uc = top_id(&lib, "u", "uc");

        // grouping-born container gc is a data-visible child of u:uc
        let gc = rec.data_child(uc, "gc").expect("gc visible under uc");
        // definition module is g (goto/defining), instance owner is u
        assert_eq!(rec.node(gc).unwrap().origin_module(), "g");
        assert_eq!(rec.node(gc).unwrap().instance_module(), "u");
        // nested grouping content also keeps the using module
        let x = rec.data_child(gc, "x").expect("x visible under gc");
        assert_eq!(rec.node(x).unwrap().instance_module(), "u");

        // Namespace-aware lookup keys on the instance owner: matches urn:u,
        // not urn:g (even though the grouping is defined in g).
        assert_eq!(lib.data_child("u", uc, "urn:u", "gc"), Some(gc));
        assert_eq!(lib.data_child("u", uc, "urn:g", "gc"), None);

        // schema-nodeid prefixes by the instance module (u), not origin (g).
        assert_eq!(lib.schema_nodeid("u", gc).as_deref(), Some("/u:uc/u:gc"));
        assert_eq!(lib.schema_nodeid("u", x).as_deref(), Some("/u:uc/u:gc/u:x"));
    }
}
