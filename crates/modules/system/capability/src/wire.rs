//! the capability module's public wire surface — types only.
//!
//! the capability module is the network-wide registry of what each node's
//! host can execute ("codex", "claude", ...): node key -> announced tag set,
//! replicated in consensus state so every node holds an identical view of
//! who provides what. announcements are declarative and self-scoped: a node
//! states its OWN full set (identity comes from the verified submit origin,
//! never the payload), and an empty set removes the node from the registry.
//! tags are open-set strings so a new kind of executor is data, not code —
//! the impl crate validates shape (charset/length/count), not meaning.
//!
//! ## capability classes
//!
//! a CLASS is a namespace token (`agent`, `ai`, ...) that a MODULE claims:
//! the wire form of a classed capability is `<class>:<rest>`, and dispatch
//! addresses classed work as `<node_id>/<class>:<rest>` where `<node_id>` is
//! a concrete node key (lowercase hex) or `*` (any node). the registry is the
//! primary router for that address space: class -> claimant module. claims
//! are first-come-first-served ([`CapabilityMsg::ClaimClass`]), and there is
//! deliberately NO unclaim op — removing a claim would dangle every address
//! already routed through it, so claims are permanent. the address-parsing helpers below
//! ([`parse_classed_address`], [`class_of`]) are the single source of truth
//! for the separators, so later dispatch work cannot drift from the registry.

use sdk::ModuleId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// longest single capability tag, in bytes — a wire-format constant shared by
/// everything that mints or checks a tag (the registry's Announce validation,
/// host spec parsing, agent registration).
pub const MAX_TAG_LEN: usize = 64;

/// the ONE definition of a well-formed capability tag: non-empty, at most
/// [`MAX_TAG_LEN`] bytes, charset `[a-z0-9._-]`. every layer that accepts a
/// tag validates through this — a tag that passes here can never bounce off
/// another layer's copy of the rule, because there are no copies.
pub fn validate_tag(tag: &str) -> Result<(), String> {
    if tag.is_empty() {
        return Err("capability tag must be non-empty".into());
    }
    if tag.len() > MAX_TAG_LEN {
        return Err(format!(
            "capability tag exceeds {MAX_TAG_LEN} bytes: {} bytes",
            tag.len()
        ));
    }
    if !tag
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(format!(
            "capability tag has invalid characters (want [a-z0-9._-]): {tag:?}"
        ));
    }
    Ok(())
}

/// longest capability class name, in bytes — classes are routing namespace
/// tokens, deliberately short.
pub const MAX_CLASS_LEN: usize = 32;

/// the ONE definition of a well-formed capability class: non-empty, at most
/// [`MAX_CLASS_LEN`] bytes, charset `[a-z0-9-]`. STRICTER than a tag on
/// purpose: `:` and `/` are the classed-address separators and `.`/`_` are
/// excluded so a class can never be mistaken for a plain tag's dotted form.
/// every layer that accepts a class validates through this (the registry's
/// ClaimClass, the address parser) — no copies to drift.
pub fn validate_class(class: &str) -> Result<(), String> {
    if class.is_empty() {
        return Err("capability class must be non-empty".into());
    }
    if class.len() > MAX_CLASS_LEN {
        return Err(format!(
            "capability class exceeds {MAX_CLASS_LEN} bytes: {} bytes",
            class.len()
        ));
    }
    if !class
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!(
            "capability class has invalid characters (want [a-z0-9-]): {class:?}"
        ));
    }
    Ok(())
}

/// most dimensions one node may announce / one job may demand — a bound, not
/// a schema (dimensions are open-set data: "cores", "mem_gb", later "gpu").
pub const MAX_RESOURCE_DIMS: usize = 16;

/// the ONE rule for a resource map (announced capacity or demanded amounts):
/// bounded size, keys under the tag rule, values non-zero (zero means "don't
/// name the dimension").
pub fn validate_resources(resources: &BTreeMap<String, u64>) -> Result<(), String> {
    if resources.len() > MAX_RESOURCE_DIMS {
        return Err(format!(
            "too many resource dimensions: {} exceeds the {MAX_RESOURCE_DIMS} cap",
            resources.len()
        ));
    }
    for (key, value) in resources {
        validate_tag(key).map_err(|e| format!("resource dimension {key:?}: {e}"))?;
        if *value == 0 {
            return Err(format!(
                "resource dimension {key:?} is zero (omit it instead)"
            ));
        }
    }
    Ok(())
}

/// the node half of a classed address `<node_id>/<class>:<rest>`: a concrete
/// node key, or `*` — any node the router may pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeSelector {
    /// `*` — any node serving the class.
    Any,
    /// a concrete node key (the registry's announced-key bytes).
    Node(Vec<u8>),
}

/// parse a classed dispatch address `<node_id>/<class>:<rest>` into its three
/// parts. `<node_id>` is `*` or the lowercase-hex encoding of a node key; the
/// class must satisfy [`validate_class`]; `<rest>` is the free-form remainder
/// (non-empty — it may itself contain `:` or `/`, only the FIRST `/` and the
/// first `:` after it are separators). this parser is the single source of
/// truth for the address grammar: dispatch and the registry both route
/// through it, so the separators cannot drift.
pub fn parse_classed_address(addr: &str) -> Result<(NodeSelector, String, String), String> {
    let Some((node, capability)) = addr.split_once('/') else {
        return Err(format!(
            "classed address is missing the '/' node separator: {addr:?}"
        ));
    };
    let selector = if node == "*" {
        NodeSelector::Any
    } else {
        NodeSelector::Node(decode_node_hex(node)?)
    };
    let Some((class, rest)) = capability.split_once(':') else {
        return Err(format!(
            "classed address is missing the ':' class separator: {addr:?}"
        ));
    };
    validate_class(class)?;
    if rest.is_empty() {
        return Err(format!("classed address has an empty rest: {addr:?}"));
    }
    Ok((selector, class.to_string(), rest.to_string()))
}

/// the class of a classed capability `<class>:<rest>`, or `None` when the
/// string is not classed (no `:`, an ill-formed class before the first `:`,
/// or an empty rest). routers use this to decide plain-tag vs class routing,
/// so it must never claim a class the registry could not hold.
pub fn class_of(capability: &str) -> Option<&str> {
    let (class, rest) = capability.split_once(':')?;
    (validate_class(class).is_ok() && !rest.is_empty()).then_some(class)
}

/// decode a node key from its lowercase-hex address form. strict: non-empty,
/// even length, `[0-9a-f]` only — uppercase is rejected so every key has
/// exactly ONE address spelling.
fn decode_node_hex(s: &str) -> Result<Vec<u8>, String> {
    if s.is_empty() {
        return Err("classed address has an empty node id".into());
    }
    if !s.len().is_multiple_of(2) {
        return Err(format!("classed address node id has odd hex length: {s:?}"));
    }
    let nibble = |b: u8| -> Result<u8, String> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(format!(
                "classed address node id is not lowercase hex: {s:?}"
            )),
        }
    };
    s.as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityMsg {
    /// declaratively replace the submitter's announced capability set. the
    /// announced node is the SUBMIT ORIGIN's external key — a node can only
    /// speak for itself. announcing an empty set removes the node. re-sending
    /// the current set is an idempotent no-op state-wise, so hosts may
    /// re-derive and re-announce freely (restart, rediscovery).
    Announce {
        capabilities: Vec<String>,
        /// announced numeric capacity (e.g. "cores", "mem_gb"). OPTIONAL by
        /// design — a claim is a claim: a tags-only announce stays valid, it
        /// just never matches a demands-carrying job.
        resources: BTreeMap<String, u64>,
    },
    /// claim a capability CLASS for the submitting MODULE — the claimant is
    /// the verified `Origin::Module` id, never payload data ("a class is
    /// claimed by the module that serves it"), so External and System origins
    /// reject. first claim wins: a claim on a class already held by ANOTHER
    /// module is a deterministic rejection; a re-claim by the OWNING module
    /// is an idempotent no-op (root unchanged). there is no unclaim — see the
    /// module doc on dangling routes.
    ClaimClass { class: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityQuery {
    /// every node that announced `capability`, sorted by key.
    Providers { capability: String },
    /// the capability set a single node announced (empty if absent).
    Node { node: Vec<u8> },
    /// every node that announced `capability` AND whose announced resources
    /// cover `demands` per dimension (absent dimension ≠ infinite). empty
    /// demands degrade to `Providers`.
    CapableProviders {
        capability: String,
        demands: BTreeMap<String, u64>,
    },
    /// the resource map a single node announced (empty if absent) — the
    /// announcer's idempotence read, beside `Node`.
    Resources { node: Vec<u8> },
    /// the full registry, sorted by node key. no Rust constructor exists —
    /// this is the desktop app's registry-enumeration read (the executor
    /// picker and per-member capability display), submitted as wire JSON.
    All,
    /// the module that claimed `class`, if any — the router's primary read
    /// for a classed address.
    ResolveClass { class: String },
    /// every claimed class with its claimant module, sorted by class.
    Classes,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityReply {
    Providers(Vec<Vec<u8>>),
    Node(Vec<String>),
    Resources(BTreeMap<String, u64>),
    All(Vec<(Vec<u8>, Vec<String>)>),
    ClassOwner(Option<ModuleId>),
    Classes(Vec<(String, ModuleId)>),
}

pub fn encode_msg(m: &CapabilityMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<CapabilityMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &CapabilityQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<CapabilityQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &CapabilityReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<CapabilityReply, String> {
    sdk::wire::decode(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn res(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn validate_resources_accepts_sane_maps_and_rejects_bad_ones() {
        assert!(validate_resources(&res(&[])).is_ok());
        assert!(validate_resources(&res(&[("cores", 8), ("mem_gb", 32)])).is_ok());
        // keys obey the ONE tag rule (charset/length), values must be non-zero
        assert!(validate_resources(&res(&[("Cores", 8)])).is_err());
        assert!(validate_resources(&res(&[("cores", 0)])).is_err());
        let too_many: BTreeMap<String, u64> = (0..=MAX_RESOURCE_DIMS)
            .map(|i| (format!("d{i}"), 1))
            .collect();
        assert!(validate_resources(&too_many).is_err());
    }

    #[test]
    fn announce_requires_the_resources_field() {
        // `resources` is required on the wire (no serde default): a resources-less
        // announce is rejected. every producer emits it — a tags-only node sends
        // an explicit `"resources":{}`.
        let no_key = br#"{"announce":{"capabilities":["codex"]}}"#;
        assert!(
            decode_msg(no_key).is_err(),
            "an announce without a resources key must be rejected"
        );
        let empty = br#"{"announce":{"capabilities":["codex"],"resources":{}}}"#;
        let CapabilityMsg::Announce {
            capabilities,
            resources,
        } = decode_msg(empty).unwrap()
        else {
            panic!("expected an announce");
        };
        assert_eq!(capabilities, vec!["codex"]);
        assert!(resources.is_empty());
    }

    #[test]
    fn parses_concrete_node_addresses() {
        let (sel, class, rest) = parse_classed_address("ab12/agent:task").unwrap();
        assert_eq!(sel, NodeSelector::Node(vec![0xab, 0x12]));
        assert_eq!(class, "agent");
        assert_eq!(rest, "task");

        // a realistic 32-byte key round-trips through the hex form.
        let key: Vec<u8> = (0u8..32).collect();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let (sel, class, rest) = parse_classed_address(&format!("{hex}/a-1:x")).unwrap();
        assert_eq!(sel, NodeSelector::Node(key));
        assert_eq!((class.as_str(), rest.as_str()), ("a-1", "x"));
    }

    #[test]
    fn parses_the_any_selector() {
        let (sel, class, rest) = parse_classed_address("*/ai:summarize").unwrap();
        assert_eq!(sel, NodeSelector::Any);
        assert_eq!((class.as_str(), rest.as_str()), ("ai", "summarize"));
    }

    #[test]
    fn rest_is_free_form_only_the_first_separators_bind() {
        // only the FIRST '/' and the first ':' after it split — the rest may
        // itself carry both separators (structured sub-addresses).
        let (sel, class, rest) = parse_classed_address("*/agent:a:b/c").unwrap();
        assert_eq!(sel, NodeSelector::Any);
        assert_eq!((class.as_str(), rest.as_str()), ("agent", "a:b/c"));
    }

    #[test]
    fn bad_node_ids_are_rejected() {
        for (addr, why) in [
            ("zz/agent:x", "not lowercase hex"),
            ("AB12/agent:x", "not lowercase hex"),
            ("abc/agent:x", "odd hex length"),
            ("/agent:x", "empty node id"),
        ] {
            let err = parse_classed_address(addr).unwrap_err();
            assert!(err.contains(why), "{addr:?} -> {err}");
        }
    }

    #[test]
    fn missing_separators_are_rejected() {
        for (addr, why) in [
            ("agent:task", "'/' node separator"),
            ("*/agent", "':' class separator"),
            ("", "'/' node separator"),
        ] {
            let err = parse_classed_address(addr).unwrap_err();
            assert!(err.contains(why), "{addr:?} -> {err}");
        }
    }

    #[test]
    fn empty_parts_and_separator_injection_are_rejected() {
        let long = format!("*/{}:x", "c".repeat(MAX_CLASS_LEN + 1));
        for (addr, why) in [
            ("*/:x", "non-empty"),
            ("*/agent:", "empty rest"),
            // a '/' smuggled ahead of the class lands IN the class (only the
            // first '/' binds) and the class charset rejects it.
            ("*/def/agent:x", "invalid characters"),
            ("*/Agent:x", "invalid characters"),
            ("*/ag.ent:x", "invalid characters"),
            ("*/ag_ent:x", "invalid characters"),
            ("*/ag ent:x", "invalid characters"),
            (long.as_str(), "exceeds 32 bytes"),
        ] {
            let err = parse_classed_address(addr).unwrap_err();
            assert!(err.contains(why), "{addr:?} -> {err}");
        }
    }

    #[test]
    fn class_of_classifies_exactly_what_the_registry_could_hold() {
        assert_eq!(class_of("agent:task"), Some("agent"));
        assert_eq!(class_of("a-1:x:y"), Some("a-1"), "rest may carry ':'");
        assert_eq!(class_of("task"), None, "no separator: a plain tag");
        assert_eq!(class_of(":x"), None, "empty class");
        assert_eq!(class_of("agent:"), None, "empty rest");
        assert_eq!(class_of("Agent:x"), None, "ill-formed class");
        assert_eq!(class_of("ag.ent:x"), None, "tag charset is not a class");
    }
}
