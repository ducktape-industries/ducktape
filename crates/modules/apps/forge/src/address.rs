//! forge's half of a `duck://` address.
//!
//! The shared parser ([`duck_address::Address`]) reads
//! `duck://<chain>/<module>/<module-path…>` and stops at the module segment:
//! what the tail MEANS is the module's question, so forge's answer lives here,
//! beside the wire surface everything else links, and not in the grammar.

use duck_address::{Address, Refused};
use sdk::refusal::INVALID_INPUT;

/// forge's path: `duck://<chain>/forge/<owner>/<repo>`.
///
/// NOTE the namespace this names does not exist in the forge module yet — its
/// repo namespace per network is flat today (one `repo` slug, no `/`). The
/// address carries `<owner>/<repo>` because that is the grammar the owner
/// fixed (ducktape#2616 §3, design note v3); forge learns the namespace in the
/// same wave as the git helper, and until it does the module refuses what this
/// parses. Nothing maps silently in between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeRepoAddress {
    pub owner: String,
    pub repo: String,
}

/// MIRRORED from the forge module's `MAX_REPO_NAME_LEN`
/// (`crates/modules/apps/forge/src/lib.rs` in the ducktape repository, the home
/// of `norm_repo` — the validator the module and noded's git smart-HTTP layer
/// share). Mirrored and not linked: the module lives behind the chain line in
/// another repository, and this wire surface is what a CLI, a helper and a view
/// link instead of it. A name this admits and the module refuses is a bug in
/// one of the two numbers.
const MAX_REPO_NAME_LEN: usize = 64;

impl TryFrom<&Address> for ForgeRepoAddress {
    type Error = Refused;

    fn try_from(address: &Address) -> Result<Self, Refused> {
        if address.module != "forge" {
            return Err(Refused::new(
                INVALID_INPUT,
                format!(
                    "A forge address is `duck://<chain>/forge/<owner>/<repo>`, but this one names the module `{}`.",
                    address.module
                ),
            ));
        }
        let [owner, repo] = address.path.as_slice() else {
            return Err(Refused::new(
                INVALID_INPUT,
                format!(
                    "A forge address is `duck://<chain>/forge/<owner>/<repo>` — two segments after `forge` — and this one carries {}.",
                    address.path.len()
                ),
            ));
        };
        if repo.ends_with(".git") {
            return Err(Refused::new(
                INVALID_INPUT,
                format!(
                    "Drop the `.git` from `{repo}`: an address names the forge repository, not a directory, and git is happy without it."
                ),
            ));
        }
        Ok(ForgeRepoAddress {
            owner: name("owner", owner)?,
            repo: name("repository", repo)?,
        })
    }
}

/// forge's name rule, as the module admits it: `[a-z0-9._-]`, never starting
/// with `.` (that collides with `.`/`..` as a path segment and with forge's own
/// dot-prefixed state files), 1..=[`MAX_REPO_NAME_LEN`] bytes. One token for
/// every way a name can fail it, because a caller does the same thing about all
/// of them: fix the name.
fn name(part: &str, value: &str) -> Result<String, Refused> {
    let refuse = |why: &str| {
        Err(Refused::new(
            INVALID_INPUT,
            format!("A forge {part} name {why}, and `{value}` does not."),
        ))
    };
    if value.is_empty() || value.len() > MAX_REPO_NAME_LEN {
        return refuse(&format!("is 1 to {MAX_REPO_NAME_LEN} bytes"));
    }
    if value.starts_with('.') {
        return refuse("never starts with `.`");
    }
    if !value
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return refuse("carries only [a-z0-9._-]");
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> Address {
        Address::parse(text).expect("the shared grammar parses this")
    }

    /// every forge refusal is one class (fix the address), so the sentence
    /// is what tells the rules apart.
    fn refused(text: &str) -> String {
        sentence(ForgeRepoAddress::try_from(&address(text)))
    }

    fn sentence(parsed: Result<ForgeRepoAddress, Refused>) -> String {
        let refused = parsed.expect_err("refused");
        assert_eq!(refused.reason, INVALID_INPUT);
        refused.sentence
    }

    #[test]
    fn reads_owner_and_repo() {
        let parsed =
            ForgeRepoAddress::try_from(&address("duck://dognet-b5b6ea90/forge/alice/ducktape-sdk"))
                .expect("accepted");
        assert_eq!(parsed.owner, "alice");
        assert_eq!(parsed.repo, "ducktape-sdk");
    }

    #[test]
    fn the_path_is_exactly_two_segments() {
        for text in [
            "duck://dognet-b5b6ea90/forge",
            "duck://dognet-b5b6ea90/forge/alice",
            "duck://dognet-b5b6ea90/forge/alice/my-crate/src",
        ] {
            assert!(
                refused(text).contains("two segments after `forge`"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_dot_git_suffix_is_refused() {
        assert!(
            refused("duck://dognet-b5b6ea90/forge/alice/my-crate.git")
                .starts_with("Drop the `.git`")
        );
    }

    #[test]
    fn a_name_forge_would_refuse_is_refused_here() {
        assert!(
            refused("duck://dognet-b5b6ea90/forge/alice/.tracker.bin")
                .starts_with("A forge repository name never starts with `.`")
        );
        assert!(
            refused("duck://dognet-b5b6ea90/forge/.alice/repo")
                .starts_with("A forge owner name never starts with `.`")
        );
        let long = "a".repeat(MAX_REPO_NAME_LEN + 1);
        assert!(
            refused(&format!("duck://dognet-b5b6ea90/forge/alice/{long}"))
                .starts_with("A forge repository name is 1 to")
        );
        assert!(
            ForgeRepoAddress::try_from(&address(&format!(
                "duck://dognet-b5b6ea90/forge/alice/{}",
                "a".repeat(MAX_REPO_NAME_LEN)
            )))
            .is_ok()
        );
    }

    /// the shared grammar's segment rule and forge's name rule agree today, so
    /// a bad character cannot arrive through `Address::parse`. It can arrive
    /// through a hand-built `Address`, and this check is forge's own — the
    /// module's validator is the one that matters, and this mirrors it whole
    /// rather than leaning on the parser upstream staying this strict.
    #[test]
    fn the_name_rule_does_not_lean_on_the_parser() {
        let built = Address {
            chain: "dognet-b5b6ea90".parse().expect("a chain id parses"),
            module: "forge".to_string(),
            path: vec!["alice".to_string(), "my~crate".to_string()],
        };
        assert!(
            sentence(ForgeRepoAddress::try_from(&built))
                .starts_with("A forge repository name carries only [a-z0-9._-]")
        );
    }

    #[test]
    fn another_module_is_not_a_forge_repo() {
        let built = Address {
            chain: "dognet-b5b6ea90".parse().expect("a chain id parses"),
            module: "gateway".to_string(),
            path: vec!["alice".to_string(), "page".to_string()],
        };
        assert!(
            sentence(ForgeRepoAddress::try_from(&built)).contains("names the module `gateway`")
        );
    }
}
