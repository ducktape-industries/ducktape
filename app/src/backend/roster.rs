use super::*;
use identity::{AccountNumber, AccountView, IdentityQuery, IdentityReply};

/// The network's name directory as this process last read it: the account
/// name bound to every user key. Every surface that names a key reads it, and
/// every read of the identity roster ([`read_accounts`]) rewrites it whole —
/// on each chat load, before a row renders, and on every identity op the live
/// stream delivers.
static NAME_DIRECTORY: std::sync::RwLock<Names> = std::sync::RwLock::new(Names {
    generation: 0,
    directory: NameDirectory::empty(),
});

/// The directory with the generation of the read that seated it, so a
/// holder of a snapshot can tell when the directory has moved on without
/// cloning it again.
struct Names {
    generation: u64,
    directory: NameDirectory,
}

fn read_names() -> std::sync::RwLockReadGuard<'static, Names> {
    NAME_DIRECTORY
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Seats a freshly read directory as the one every surface reads.
fn seat_names(directory: NameDirectory) {
    let mut names = NAME_DIRECTORY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    names.generation += 1;
    names.directory = directory;
}

/// The directory as last read — a snapshot the caller owns, so a loader can
/// lend it across its awaits and the update thread can read it without one.
pub(crate) fn names() -> NameDirectory {
    read_names().directory.clone()
}

/// Every identity account, paged the way the module serves them: numbered
/// from 1 with no gaps, at most `MAX_QUERY_LIMIT` per page. THE ONE read of
/// the identity roster; the name directory is rewritten from what it returns.
pub(crate) async fn read_accounts(client: &RpcClient) -> Result<Vec<AccountView>, String> {
    let page_limit =
        usize::try_from(identity::MAX_QUERY_LIMIT).expect("the identity page cap fits a usize");
    let mut accounts: Vec<AccountView> = Vec::new();
    let mut from: AccountNumber = 0;
    loop {
        let reply: IdentityReply = client
            .query(
                "identity",
                &IdentityQuery::All {
                    from,
                    limit: identity::MAX_QUERY_LIMIT,
                },
            )
            .await?;
        let IdentityReply::Accounts(page) = reply else {
            return Err("the identity module returned the wrong reply".to_string());
        };
        let page_is_last = page.len() < page_limit;
        let Some(last) = page.last().map(|account| account.number) else {
            break;
        };
        accounts.extend(page);
        if page_is_last {
            break;
        }
        from = last + 1;
    }
    seat_names(directory_of(&accounts));
    Ok(accounts)
}

/// The directory an account list binds: every key of an account resolves to
/// that account — its number and its name.
pub(crate) fn directory_of(accounts: &[AccountView]) -> NameDirectory {
    NameDirectory::from_accounts(accounts)
}

/// A test's directory, seated the way a roster read seats it, for as long as
/// the guard lives. The directory is one per process, so tests that seat one
/// take turns on it, and a guard dropped leaves it empty for the next.
#[cfg(test)]
pub(crate) fn seed_names(directory: NameDirectory) -> SeededNames {
    static TURN: Mutex<()> = Mutex::new(());
    let turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let seeded = SeededNames { _turn: turn };
    seeded.seat(directory);
    seeded
}

/// A test's turn on the process-wide directory.
#[cfg(test)]
pub(crate) struct SeededNames {
    _turn: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl SeededNames {
    /// Replace the seated directory, the turn kept.
    pub(crate) fn seat(&self, directory: NameDirectory) {
        seat_names(directory);
    }
}

#[cfg(test)]
impl Drop for SeededNames {
    fn drop(&mut self) {
        self.seat(NameDirectory::empty());
    }
}

/// Refresh the directory and nothing else — what a chat load does before it
/// renders a row.
pub(crate) async fn refresh_names(client: &RpcClient) -> Result<(), String> {
    read_accounts(client).await.map(|_accounts| ())
}

/// What every chat renderer is handed: this device's key (the `by me` facts)
/// and the directory (every label), owned so a loader can lend a
/// [`ChatReader`] across its awaits.
pub(crate) struct ReaderFacts {
    key: Option<Vec<u8>>,
    names: NameDirectory,
}

impl ReaderFacts {
    pub(crate) async fn current() -> Self {
        Self {
            key: local_user_key().await,
            names: names(),
        }
    }

    pub(crate) fn reader(&self) -> ChatReader<'_> {
        ChatReader::new(self.key.as_deref(), &self.names)
    }

    pub(crate) fn names(&self) -> &NameDirectory {
        &self.names
    }
}
