//! Existing node work-admission configuration shared with installed services.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// the policy file, beside `node.toml` in the workspace. Deliberately its own
/// file rather than a `node.toml` table: `write_node_toml` REWRITES the whole
/// config on every `init`/`join` merge, so a list living there would need a
/// `Plumbing` field to survive — five touch points for one list. Absent = the
/// default, exactly as an absent `services.toml` means "no grants".
pub const FILE_NAME: &str = "work-admit.toml";

/// the one `admit` entry that is not an account number — and the one word the
/// CLI takes for it, deliberately the SAME token in both places rather than a
/// config spelling and a CLI spelling that must be kept in sync. It is a
/// statement, not an entry, so it may not be mixed with account numbers.
pub const ANYONE: &str = "anyone";

// ============================================================================
// the policy
// ============================================================================

/// Whose work this node will run. ONE discriminant; the file's `admit` list
/// decodes into exactly one arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkAdmission {
    /// exactly these accounts (plus this node's own submissions, always). The
    /// default is the EMPTY set: a node runs nobody's work but its own until
    /// its operator admits an account — the same shape as the record it
    /// protects, `gateway::credential_use_allowed`, which admits explicit
    /// grantees and nobody else.
    Accounts(BTreeSet<u64>),
    /// any node the mesh admitted. Opt-in, and it re-opens what this module
    /// exists to close — the CLI says so on the way in.
    Anyone,
}

impl Default for WorkAdmission {
    fn default() -> Self {
        WorkAdmission::Accounts(BTreeSet::new())
    }
}

impl WorkAdmission {
    /// the file's `admit` entries, in the canonical order the file is written
    /// in. The default is the empty list.
    pub fn entries(&self) -> Vec<String> {
        match self {
            WorkAdmission::Anyone => vec![ANYONE.to_string()],
            WorkAdmission::Accounts(accounts) => {
                accounts.iter().map(|number| number.to_string()).collect()
            }
        }
    }

    /// admit one more account — or widen to [`Self::Anyone`]. `Anyone` absorbs.
    pub fn with(self, target: AdmitTarget) -> Self {
        match target {
            AdmitTarget::Anyone => WorkAdmission::Anyone,
            AdmitTarget::Account(number) => match self {
                WorkAdmission::Anyone => WorkAdmission::Anyone,
                WorkAdmission::Accounts(mut accounts) => {
                    accounts.insert(number);
                    WorkAdmission::Accounts(accounts)
                }
            },
        }
    }

    /// stop admitting one account — or narrow back from [`Self::Anyone`].
    pub fn without(self, target: AdmitTarget) -> Self {
        match target {
            AdmitTarget::Anyone => WorkAdmission::default(),
            AdmitTarget::Account(number) => match self {
                // revoking one account from a wildcard is meaningless and
                // silently doing nothing would read as success: the CLI refuses
                // it before this is reached.
                WorkAdmission::Anyone => WorkAdmission::Anyone,
                WorkAdmission::Accounts(mut accounts) => {
                    accounts.remove(&number);
                    WorkAdmission::Accounts(accounts)
                }
            },
        }
    }
}

/// what one `node work admit|revoke` names. ONE discriminant — the literal
/// `anyone`, or an account number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitTarget {
    Anyone,
    Account(u64),
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmitFile {
    admit: Vec<String>,
}

/// Read the workspace's policy. A MISSING file is the default, not an error —
/// the same convention `services.toml` uses for "no grants".
pub fn load(workspace: &Path) -> Result<WorkAdmission, String> {
    let path = policy_path(workspace);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkAdmission::default());
        }
        Err(error) => return Err(format!("read {path:?}: {error}")),
    };
    let file: AdmitFile = toml::from_str(&text).map_err(|error| format!("{path:?}: {error}"))?;
    parse(&file.admit)
}

/// Write the workspace's policy. The default REMOVES the file: an empty policy
/// and a missing one mean the same thing, and leaving the husk behind invites
/// a stale read (`services::save`'s rule, for the same reason).
pub fn save(workspace: &Path, policy: &WorkAdmission) -> Result<(), String> {
    let path = policy_path(workspace);
    let entries = policy.entries();
    if entries.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove {path:?}: {error}")),
        };
    }
    let listed: String = entries
        .iter()
        .map(|entry| format!("  \"{entry}\",\n"))
        .collect();
    let text = format!(
        "# whose work this node will execute — the account numbers this node admits,\n\
         # on top of its own submissions (always admitted).\n\
         # managed by `ducktape node work admit|revoke`; re-read on every decision.\n\
         # [\"{ANYONE}\"] admits any network member: this node then runs a stranger's\n\
         # workload AND lets it draw on every credential this node is granted.\n\
         admit = [\n{listed}]\n"
    );
    let temporary = workspace.join(format!(".{FILE_NAME}.tmp"));
    std::fs::write(&temporary, text).map_err(|error| format!("write {temporary:?}: {error}"))?;
    if let Err(error) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("replace {path:?}: {error}"));
    }
    Ok(())
}

pub fn policy_path(workspace: &Path) -> PathBuf {
    workspace.join(FILE_NAME)
}

/// decode the `admit` list into exactly one policy.
pub fn parse(entries: &[String]) -> Result<WorkAdmission, String> {
    let wildcards = entries.iter().filter(|entry| *entry == ANYONE).count();
    let mixed = wildcards > 0 && wildcards != entries.len();
    if mixed {
        return Err(format!(
            "admit lists {ANYONE:?} alongside account numbers: a wildcard is a statement, not \
             an entry — use either {ANYONE:?} alone or only account numbers"
        ));
    }
    if wildcards > 0 {
        return Ok(WorkAdmission::Anyone);
    }
    let mut accounts = BTreeSet::new();
    for entry in entries {
        let number: u64 = entry
            .parse()
            .map_err(|_| format!("admit entry {entry:?} is not an account number"))?;
        if number == 0 {
            return Err("admit carries account number 0, which is no account".into());
        }
        accounts.insert(number);
    }
    Ok(WorkAdmission::Accounts(accounts))
}
