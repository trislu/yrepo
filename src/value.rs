//! Leaf **value** typing (D31 / M5).
//!
//! Two pieces:
//!
//! - [`TypeFacets`] — the restrictions written on **one** `type` statement
//!   (on a leaf/leaf-list or on a typedef): string `length`/`pattern`, numeric
//!   `range`, `enum`/`bit` members, `leafref` `path`/`require-instance`.
//! - [`ValueType`] — the **resolved** value type of a leaf: the reduced
//!   builtin plus the restrictions accumulated along the typedef chain
//!   ([`Library::value_type`]).
//!
//! The scope is D31: the LS validates a leaf value **only** when the type
//! reduces to a scalar — [`ValueType::is_checked`]. `union` is deliberately
//! **not** checked: RFC 7950 §9.12 tries the members in order and a bare value
//! carries no type tag, so `123` fits both `string` and `uint16`; there is no
//! single validator. `leafref`/`identityref`/`instance-identifier` are
//! resolved kinds (coarse hints), not checked scalars.

use crate::syntax::{Statement, StatementKind};

/// Restrictions captured on one `type` statement. Argument text is stored
/// dequoted and trimmed, exactly as written (multiple intervals may appear in
/// a `length`/`range` string, separated by `|`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypeFacets {
    /// The `length` argument (string base), when written here.
    pub length: Option<String>,
    /// The `range` argument (numeric base), when written here.
    pub range: Option<String>,
    /// `pattern` bodies (string base), in order.
    pub patterns: Vec<String>,
    /// `enum` member names (enumeration base), in order.
    pub enum_members: Vec<String>,
    /// `bit` member names (bits base), in order.
    pub bit_members: Vec<String>,
    /// The `path` argument (leafref base), when written here.
    pub path: Option<String>,
    /// The `require-instance` argument (leafref base), when written here.
    pub require_instance: Option<bool>,
    /// The `base` argument (identityref base), when written here.
    pub base: Option<String>,
}

impl TypeFacets {
    /// Capture the facets written as substatements of `stmt` (a `type`
    /// statement). Substatements that carry *other* kinds (e.g. a `union`'s
    /// member `type`s, or an `enum`'s `value`) are not modeled.
    pub(crate) fn from_type_stmt(stmt: &Statement) -> TypeFacets {
        let mut f = TypeFacets::default();
        for c in &stmt.children {
            match c.kind {
                StatementKind::Length => {
                    f.length = c.arg.as_ref().map(|a| a.logical.trim().to_string());
                }
                StatementKind::Range => {
                    f.range = c.arg.as_ref().map(|a| a.logical.trim().to_string());
                }
                StatementKind::Pattern => {
                    if let Some(a) = c.arg.as_ref() {
                        f.patterns.push(a.logical.trim().to_string());
                    }
                }
                StatementKind::Enum => {
                    if let Some(a) = c.arg.as_ref() {
                        f.enum_members.push(a.name().to_string());
                    }
                }
                StatementKind::Bit => {
                    if let Some(a) = c.arg.as_ref() {
                        f.bit_members.push(a.name().to_string());
                    }
                }
                StatementKind::Path => {
                    f.path = c.arg.as_ref().map(|a| a.logical.trim().to_string());
                }
                StatementKind::RequireInstance => {
                    f.require_instance = c.arg.as_ref().map(|a| a.name() == "true");
                }
                StatementKind::Base => {
                    f.base = c.arg.as_ref().map(|a| a.logical.trim().to_string());
                }
                _ => {}
            }
        }
        f
    }
}

/// The resolved value type of a leaf/leaf-list (D31).
///
/// `String`/`Integer`/`Decimal64` carry the restrictions from *every* level of
/// the typedef chain (all apply). `Enumeration`/`Bits` carry the member list
/// of the **most-derived** statement that lists one (a derived type restricts
/// its base to a subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    /// `string` (possibly restricted). `lengths`/`patterns` accumulate along
    /// the typedef chain; a value must satisfy every one.
    String {
        lengths: Vec<String>,
        patterns: Vec<String>,
    },
    /// `int8`…`int64` / `uint8`…`uint64`. `ranges` accumulate (each applies).
    Integer {
        signed: bool,
        bits: u8,
        ranges: Vec<String>,
    },
    /// `decimal64`. `ranges` accumulate.
    Decimal64 {
        ranges: Vec<String>,
    },
    Boolean,
    Empty,
    Binary,
    /// `enumeration` — value must be one of `members`.
    Enumeration {
        members: Vec<String>,
    },
    /// `bits` — value is whitespace-separated subset of `members`.
    Bits {
        members: Vec<String>,
    },
    /// `leafref` — chase to the target leaf's value type when possible.
    Leafref {
        path: Option<String>,
        require_instance: bool,
    },
    /// `identityref` — the value must name `base` or a derived identity (or
    /// any identity when no `base` is declared).
    Identityref {
        base: Option<String>,
    },
    InstanceIdentifier,
    /// `union` — deliberately not checked (first-match ambiguity).
    Union,
    /// A type we do not classify, or a typedef chain that did not resolve.
    Unknown,
}

impl ValueType {
    /// Whether the LS should run a value check for this type (D31: reducible
    /// scalars only). `union`, references, and unresolved chains are excluded.
    pub fn is_checked(&self) -> bool {
        matches!(
            self,
            ValueType::String { .. }
                | ValueType::Integer { .. }
                | ValueType::Decimal64 { .. }
                | ValueType::Boolean
                | ValueType::Empty
                | ValueType::Binary
                | ValueType::Enumeration { .. }
                | ValueType::Bits { .. }
        )
    }

    /// Whether this is a plain `union` (the LS stays silent).
    pub fn is_union(&self) -> bool {
        matches!(self, ValueType::Union)
    }
}

/// Accumulated restrictions while walking a type reference down to a builtin.
#[derive(Debug, Default)]
pub(crate) struct Accum {
    pub lengths: Vec<String>,
    pub ranges: Vec<String>,
    pub patterns: Vec<String>,
    /// Members from the *first* level that lists them (most-derived wins).
    pub members: Vec<String>,
    pub path: Option<String>,
    pub require_instance: Option<bool>,
    pub base: Option<String>,
}

impl Accum {
    /// Fold one statement's facets into the accumulation.
    pub(crate) fn fold(&mut self, f: &TypeFacets) {
        if let Some(l) = &f.length {
            self.lengths.push(l.clone());
        }
        if let Some(r) = &f.range {
            self.ranges.push(r.clone());
        }
        self.patterns.extend(f.patterns.iter().cloned());
        if self.members.is_empty() && (!f.enum_members.is_empty() || !f.bit_members.is_empty()) {
            if !f.enum_members.is_empty() {
                self.members = f.enum_members.clone();
            } else {
                self.members = f.bit_members.clone();
            }
        }
        if self.path.is_none() {
            self.path = f.path.clone();
        }
        if self.require_instance.is_none() {
            self.require_instance = f.require_instance;
        }
        if self.base.is_none() {
            self.base = f.base.clone();
        }
    }
}

fn integer_spec(name: &str) -> Option<(bool, u8)> {
    let rest = name
        .strip_prefix("int")
        .or_else(|| name.strip_prefix("uint"))?;
    let bits: u8 = rest.parse().ok()?;
    let signed = name.starts_with("int");
    Some((signed, bits))
}

/// Classify a resolved builtin `base` using the accumulated facets.
pub(crate) fn classify(base: &str, acc: &Accum) -> ValueType {
    use ValueType as V;
    match base {
        "string" => V::String {
            lengths: acc.lengths.clone(),
            patterns: acc.patterns.clone(),
        },
        "decimal64" => V::Decimal64 {
            ranges: acc.ranges.clone(),
        },
        "boolean" => V::Boolean,
        "empty" => V::Empty,
        "binary" => V::Binary,
        "enumeration" => V::Enumeration {
            members: acc.members.clone(),
        },
        "bits" => V::Bits {
            members: acc.members.clone(),
        },
        "leafref" => V::Leafref {
            path: acc.path.clone(),
            require_instance: acc.require_instance.unwrap_or(true),
        },
        "identityref" => V::Identityref {
            base: acc.base.clone(),
        },
        "instance-identifier" => V::InstanceIdentifier,
        "union" => V::Union,
        other => match integer_spec(other) {
            Some((signed, bits)) => V::Integer {
                signed,
                bits,
                ranges: acc.ranges.clone(),
            },
            None => V::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{Library, Repository};

    use super::*;

    fn lib(src: &str) -> Arc<Library> {
        let mut repo = Repository::new();
        repo.upsert("/m.yang", src);
        repo.compile().library.expect("library")
    }

    fn vt(l: &Library, name: &str) -> ValueType {
        let id = l
            .module("m")
            .unwrap()
            .nodes()
            .iter()
            .position(|n| n.name() == name)
            .unwrap_or_else(|| panic!("no node {name}"));
        l.value_type("m", id)
            .unwrap_or_else(|| panic!("no type on {name}"))
    }

    const BUILTINS: &str = r#"module m {
  yang-version 1.1;
  namespace "urn:m";
  prefix m;
  revision 2026-01-01;
  container c {
    leaf a { type string; }
    leaf b { type uint16; }
    leaf i8 { type int8; }
    leaf d64 { type decimal64 { fraction-digits 2; } }
    leaf bo { type boolean; }
    leaf em { type empty; }
    leaf bin { type binary; }
    leaf en { type enumeration { enum red; enum green; } }
    leaf bi { type bits { bit a; bit b; } }
    leaf u { type union { type string; type uint16; } }
    leaf lr { type leafref { path "/m:c/m:a"; } }
    leaf idr { type identityref; }
  }
}"#;

    #[test]
    fn classifies_builtin_scalars() {
        let l = lib(BUILTINS);
        assert_eq!(
            vt(&l, "a"),
            ValueType::String {
                lengths: vec![],
                patterns: vec![]
            }
        );
        assert_eq!(
            vt(&l, "b"),
            ValueType::Integer {
                signed: false,
                bits: 16,
                ranges: vec![]
            }
        );
        assert_eq!(
            vt(&l, "i8"),
            ValueType::Integer {
                signed: true,
                bits: 8,
                ranges: vec![]
            }
        );
        assert_eq!(vt(&l, "d64"), ValueType::Decimal64 { ranges: vec![] });
        assert_eq!(vt(&l, "bo"), ValueType::Boolean);
        assert_eq!(vt(&l, "em"), ValueType::Empty);
        assert_eq!(vt(&l, "bin"), ValueType::Binary);
        assert_eq!(
            vt(&l, "en"),
            ValueType::Enumeration {
                members: vec!["red".into(), "green".into()]
            }
        );
        assert_eq!(
            vt(&l, "bi"),
            ValueType::Bits {
                members: vec!["a".into(), "b".into()]
            }
        );
        // union → never checked.
        assert_eq!(vt(&l, "u"), ValueType::Union);
        assert!(vt(&l, "u").is_union());
        // refs are not checked scalars.
        assert!(matches!(vt(&l, "lr"), ValueType::Leafref { .. }));
        assert_eq!(vt(&l, "idr"), ValueType::Identityref { base: None });
        for name in ["a", "b", "i8", "d64", "bo", "em", "bin", "en", "bi"] {
            assert!(vt(&l, name).is_checked(), "{name}");
        }
        for name in ["u", "lr", "idr"] {
            assert!(!vt(&l, name).is_checked(), "{name}");
        }
    }

    const TYPEDEFS: &str = r#"module m {
  yang-version 1.1;
  namespace "urn:m";
  prefix m;
  revision 2026-01-01;
  typedef port { type uint16 { range "1..65535"; } }
  typedef base16 { type uint16 { range "1..100"; } }
  typedef portx { type base16; }
  typedef label { type string { length "1..32"; pattern "[a-z]+"; } }
  typedef color { type enumeration { enum red; enum green; enum blue; } }
  typedef maybe { type union { type string; type uint16; } }
  container c {
    leaf p { type port; }
    leaf p2 { type portx; }
    leaf s2 { type string { length "2..4"; } }
    leaf n { type label; }
    leaf col { type color; }
    leaf maybe1 { type maybe; }
  }
}"#;

    #[test]
    fn reduces_typedef_chains_accumulating_facets() {
        let l = lib(TYPEDEFS);
        // Numeric typedef: range captured; derived typedef chain still reduces.
        assert_eq!(
            vt(&l, "p"),
            ValueType::Integer {
                signed: false,
                bits: 16,
                ranges: vec!["1..65535".into()]
            }
        );
        assert_eq!(
            vt(&l, "p2"),
            ValueType::Integer {
                signed: false,
                bits: 16,
                ranges: vec!["1..100".into()]
            }
        );
        // String: length+pattern from the typedef; a leaf-level length alone.
        assert_eq!(
            vt(&l, "n"),
            ValueType::String {
                lengths: vec!["1..32".into()],
                patterns: vec!["[a-z]+".into()]
            }
        );
        assert_eq!(
            vt(&l, "s2"),
            ValueType::String {
                lengths: vec!["2..4".into()],
                patterns: vec![]
            }
        );
        // Enumeration typedef members.
        assert_eq!(
            vt(&l, "col"),
            ValueType::Enumeration {
                members: vec!["red".into(), "green".into(), "blue".into()]
            }
        );
        // Union typedef → silent.
        assert_eq!(vt(&l, "maybe1"), ValueType::Union);
    }

    const IDENT: &str = r#"module m {
  yang-version 1.1;
  namespace "urn:im";
  prefix im;
  revision 2026-01-01;
  identity base;
  identity child { base base; }
  identity other;
  container c {
    leaf ref { type identityref { base base; } }
  }
}"#;

    #[test]
    fn identityref_captures_base_and_checks_semantically() {
        let l = lib(IDENT);
        let vt = vt(&l, "ref");
        assert!(matches!(vt, ValueType::Identityref { base: Some(_) }));
        // value_type resolves the raw `base` name.
        assert_eq!(
            vt,
            ValueType::Identityref {
                base: Some("base".to_owned())
            }
        );
        // Semantic membership: base itself and derived identities are fine.
        assert_eq!(
            l.check_identityref("m", Some("base"), "m:base"),
            crate::IdentityStatus::Ok
        );
        assert_eq!(
            l.check_identityref("m", Some("base"), "m:child"),
            crate::IdentityStatus::Ok
        );
        // Unknown identity vs. not derived vs. no base.
        assert_eq!(
            l.check_identityref("m", Some("base"), "m:other"),
            crate::IdentityStatus::NotDerived
        );
        assert_eq!(
            l.check_identityref("m", Some("base"), "m:nope"),
            crate::IdentityStatus::UnknownIdentity
        );
        assert_eq!(
            l.check_identityref("m", None, "m:other"),
            crate::IdentityStatus::Ok
        );
    }

    /// `check_identityref_in` skips prefix/module-name guessing: the caller
    /// supplies the value's module (an XML namespace resolved to a module, or
    /// an RFC 7951 `module:name`). Same derivation semantics as
    /// `check_identityref`, but an uncompiled value module is
    /// `UnknownIdentity` regardless of any same-named prefix.
    #[test]
    fn check_identityref_in_uses_the_callers_module() {
        let l = lib(IDENT);
        assert_eq!(
            l.check_identityref_in("m", Some("base"), "m", "child"),
            crate::IdentityStatus::Ok
        );
        assert_eq!(
            l.check_identityref_in("m", Some("base"), "m", "other"),
            crate::IdentityStatus::NotDerived
        );
        assert_eq!(
            l.check_identityref_in("m", Some("base"), "m", "nope"),
            crate::IdentityStatus::UnknownIdentity
        );
        assert_eq!(
            l.check_identityref_in("m", None, "m", "other"),
            crate::IdentityStatus::Ok
        );
        // `m` has no `ghost` import prefix, so a missing module stays unknown.
        assert_eq!(
            l.check_identityref_in("m", Some("base"), "ghost", "child"),
            crate::IdentityStatus::UnknownIdentity
        );
    }
}
