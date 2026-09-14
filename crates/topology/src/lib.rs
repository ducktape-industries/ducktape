//! Build presets for the founding sets staged beside the binaries.
//!
//! [`PRODUCTION`] names the default `modules/` directory; [`SIM_BASE`] and
//! [`SIM_VALSET`] name the simulator selections in `sim-modules/`. This catalog
//! decides which files a build stages. It does not restrict the module ids or
//! optional mappers an operator's genesis or a live registry admission may use.
//! Every runtime module, including `modules` and `valset`, is Wasm and declares
//! its own backing, configuration keys, and query mode through `shape`.

/// One module in the build catalog.
pub struct ModuleSpec {
    /// The consensus-visible module id (the key in the host registry / root-hash).
    pub id: &'static str,
    /// Whether this module's crate carries an index guest (`src/index_guest.rs`,
    /// staged by `crates/noded/build.rs` as `<id>.index.wasm`). This is a build
    /// consistency check; operator-supplied directories discover their own files.
    pub has_index_guest: bool,
}

/// The module composition topology: the id universe and the three named
/// genesis selections drawn from it.
pub struct ModuleTopology {
    /// Every module that appears in ANY selection.
    pub modules: &'static [ModuleSpec],
    /// node's production genesis set (wasm backend).
    pub production: &'static [&'static str],
    /// simnode's + the noded daemon's default set.
    pub sim_base: &'static [&'static str],
    /// the system modules simnode's `--with-valset` appends AFTER `sim_base`.
    pub sim_valset: &'static [&'static str],
}

impl ModuleTopology {
    /// The spec for `id`, if it is in the universe.
    pub fn spec(&self, id: &str) -> Option<&ModuleSpec> {
        self.modules.iter().find(|m| m.id == id)
    }

    /// Component ids in a build preset, in the preset's order.
    pub fn wasm_ids(&self, selection: &[&'static str]) -> Vec<&'static str> {
        selection.to_vec()
    }

    /// Mapper ids in a build preset. Arbitrary founding directories discover
    /// their own mappers independently of this catalog.
    pub fn index_guest_ids(&self, selection: &[&'static str]) -> Vec<&'static str> {
        selection
            .iter()
            .copied()
            .filter(|id| self.spec(id).is_some_and(|m| m.has_index_guest))
            .collect()
    }
}

const fn wasm(id: &'static str) -> ModuleSpec {
    ModuleSpec {
        id,
        has_index_guest: false,
    }
}

const fn wasm_indexed(id: &'static str) -> ModuleSpec {
    ModuleSpec {
        id,
        has_index_guest: true,
    }
}

/// The module id universe. Alphabetical by id (order here is documentation
/// only — selections carry the composer orders).
const MODULES: &[ModuleSpec] = &[
    wasm("acl"),
    wasm("agent"),
    wasm("attribution"),
    wasm("automations"),
    wasm("capability"),
    wasm_indexed("chat"),
    wasm("collaboration"),
    wasm("dispatch"),
    wasm("files"),
    wasm("forge"),
    wasm("gateway"),
    wasm("governance"),
    wasm("identity"),
    wasm_indexed("inbox"),
    wasm("kv"),
    wasm("modules"),
    wasm_indexed("pages"),
    wasm_indexed("runs"),
    wasm_indexed("saga"),
    wasm_indexed("tasks"),
    wasm("valset"),
];

/// Default founding set (20). An operator may compose a different set with
/// `node init --modules`; each network pins the resulting deployments.
pub const PRODUCTION: &[&str] = &[
    "pages",
    "chat",
    "forge",
    "valset",
    "acl",
    "governance",
    "modules",
    "saga",
    "capability",
    "dispatch",
    "attribution",
    "tasks",
    "identity",
    // the MERGED gateway owns the whole `.duck` name -> AccountId -> route
    // pipeline: the route plane PLUS the human-name handle plane the retired
    // `duckdns` module used to own separately.
    "gateway",
    "inbox",
    "automations",
    "files",
    "agent",
    "runs",
    // the agent-to-agent messaging plane runs' `collaboration.*` operations
    // target. It follows `runs` because a run's prepared send is the only thing
    // in this set that names it.
    "collaboration",
];

/// The founding VIEW-ONLY entries: registry entries of `Kind::View` with no
/// consensus code, staged beside the production set as `<id>.view.wasm` +
/// `<id>.assets` out of `target/views` (built by `make views` from
/// `crates/views/<id>`), and composed by `node init` into the same genesis.
/// A view here draws a tab in the app off the registry alone; it must not
/// also be a module id.
pub const VIEWS: &[&str] = &["home"];

/// the DEFAULT set (16) simnode and the noded daemon compose at genesis —
/// `bin/noded/tests/daemon_e2e.rs` pins the same `sim_base` against noded.
/// Changing it means changing the daemon.
pub const SIM_BASE: &[&str] = &[
    "chat",
    "saga",
    // saga's guest is wired to `capability` (Strict lease over ANNOUNCED providers);
    // without it every tagged saga degrades to accept-any and the sim cannot
    // reproduce production's provider gate.
    "capability",
    "dispatch",
    "attribution",
    "tasks",
    "inbox",
    "automations",
    "agent",
    "runs",
    "collaboration",
    "pages",
    "forge",
    "files",
    "identity",
    // the MERGED gateway owns both the `.duck` handle plane and the route plane.
    "gateway",
];

/// the five system modules simnode's opt-in `--with-valset` genesis appends
/// AFTER `sim_base`: the KV store, the membership registry
/// seeded with the genesis validators, the acl policy table (empty =
/// allow-all), governance (the sole authorized author of valset and acl
/// change), and the modules registry — whose mere registration makes the
/// host-injected once-per-block boundary `Advance` ride every block.
pub const SIM_VALSET: &[&str] = &["kv", "valset", "acl", "governance", "modules"];

/// The catalog the build staging code reads.
pub const TOPOLOGY: ModuleTopology = ModuleTopology {
    modules: MODULES,
    production: PRODUCTION,
    sim_base: SIM_BASE,
    sim_valset: SIM_VALSET,
};

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn has_dups(sel: &[&str]) -> bool {
        sel.len() != sel.iter().collect::<BTreeSet<_>>().len()
    }

    fn sorted(s: &[&'static str]) -> Vec<&'static str> {
        let mut v = s.to_vec();
        v.sort_unstable();
        v
    }

    /// The named selections pin to today's sets — the derivation guard: a change
    /// to a composer's genesis set that forgets this file (or vice versa) fails
    /// here. Counts AND membership, so neither a stray add nor a silent drop slips.
    #[test]
    fn selections_pin_to_todays_sets() {
        assert_eq!(PRODUCTION.len(), 20, "production is the 20-module set");
        assert_eq!(SIM_BASE.len(), 16, "sim_base is the default 16-module set");
        assert_eq!(SIM_VALSET.len(), 5, "sim_valset appends 5 system modules");

        // exact membership (sorted — registration order is not consensus-relevant)
        assert_eq!(
            sorted(PRODUCTION),
            sorted(&[
                "acl",
                "agent",
                "automations",
                "capability",
                "chat",
                "collaboration",
                "dispatch",
                "files",
                "forge",
                "gateway",
                "governance",
                "identity",
                "inbox",
                "modules",
                "pages",
                "runs",
                "saga",
                "attribution",
                "tasks",
                "valset",
            ])
        );
        assert_eq!(
            sorted(SIM_BASE),
            sorted(&[
                "agent",
                "automations",
                "capability",
                "chat",
                "collaboration",
                "dispatch",
                "files",
                "forge",
                "gateway",
                "identity",
                "inbox",
                "pages",
                "runs",
                "saga",
                "attribution",
                "tasks",
            ])
        );
        assert_eq!(
            sorted(SIM_VALSET),
            sorted(&["acl", "governance", "kv", "modules", "valset"])
        );
    }

    #[test]
    fn selections_have_no_duplicates() {
        for (name, sel) in [
            ("production", PRODUCTION),
            ("sim_base", SIM_BASE),
            ("sim_valset", SIM_VALSET),
        ] {
            assert!(!has_dups(sel), "{name} has a duplicate id");
        }
    }

    /// `sim_valset` is APPENDED to `sim_base` under `--with-valset`, so the two
    /// must be disjoint or the concatenation would double-register a module.
    #[test]
    fn sim_base_and_valset_are_disjoint() {
        let base: BTreeSet<&str> = SIM_BASE.iter().copied().collect();
        for id in SIM_VALSET {
            assert!(
                !base.contains(id),
                "sim_valset id {id} is already in sim_base"
            );
        }
    }

    /// Every selection id has a spec, and every spec is used by some selection —
    /// so the universe and the selections cannot drift apart.
    #[test]
    fn universe_and_selections_cover_each_other() {
        let universe: BTreeSet<&str> = MODULES.iter().map(|m| m.id).collect();
        assert_eq!(
            universe.len(),
            MODULES.len(),
            "the module universe has a duplicate id"
        );

        let used: BTreeSet<&str> = PRODUCTION
            .iter()
            .chain(SIM_BASE)
            .chain(SIM_VALSET)
            .copied()
            .collect();
        assert_eq!(
            universe, used,
            "every spec must be composed by some selection and every composed id must have a spec"
        );
    }

    /// a founding view is a registry entry beside the modules, under one id
    /// space: a view id that is also a module id would collide at genesis.
    #[test]
    fn founding_views_are_not_module_ids() {
        assert_eq!(VIEWS, &["home"], "the founding view set");
        assert!(!has_dups(VIEWS), "views has a duplicate id");
        for id in VIEWS {
            assert!(TOPOLOGY.spec(id).is_none(), "view {id} is also a module");
        }
    }

    #[test]
    fn accessors_resolve_specs() {
        assert!(TOPOLOGY.spec("chat").is_some());
        assert!(TOPOLOGY.spec("not-a-module").is_none());
    }

    #[test]
    fn wasm_ids_preserves_the_preset_order() {
        assert_eq!(TOPOLOGY.wasm_ids(SIM_VALSET), SIM_VALSET);
    }

    /// pins today's index-guest-shipping set — the same 5 crates that carry
    /// `src/index_guest.rs` and that `crates/noded/build.rs` cross-checks this
    /// flag against at every build.
    #[test]
    fn index_guest_ids_selects_only_the_declared_shippers() {
        let ids = TOPOLOGY.index_guest_ids(PRODUCTION);
        assert_eq!(
            sorted(&ids),
            sorted(&["chat", "inbox", "pages", "runs", "saga", "tasks"])
        );
    }
}
