//! the durable outbox: what this daemon took ownership of, and how far each
//! item got — written down BEFORE the thing it describes happens.
//!
//! ## the ordering, which is the whole point
//!
//! What matters is not the file format — an atomically rewritten state file
//! holding the same three-valued enum would do as well — but that one
//! particular write is DURABLE BEFORE an irreversible act, and that the check
//! and the claim are one critical section. An append-only journal is simply
//! the smallest thing that gives both.
//!
//! | last record for an item | what the provider may have seen | recovered as |
//! | --- | --- | --- |
//! | [`Record::Queued`] | nothing — we had not offered it yet | `Queued`, safe to attempt |
//! | [`Record::Attempting`] | possibly the whole message | **`DeliveryUnknown`** |
//! | [`Record::Settled`] | whatever the state says | that state |
//!
//! A crash between `Attempting` and `Settled` is exactly the spec's "crashes
//! after provider input but before recording acceptance". The instruction may
//! already have been read and acted on, so this daemon MUST NOT replay it on
//! its own initiative: it keeps `DeliveryUnknown` and waits for someone with
//! authority to decide. An explicit retry reuses the same message id, so the
//! receiver can tell it may have seen it before.
//!
//! ## what is NOT in here
//!
//! No body is durable beyond the delivery attempt it belongs to, no provider
//! token, no socket path, no session id. A journal line names the message and
//! its state; recovering one is enough to REPORT, never enough to re-send.
//! (That is deliberate, and it is what makes automatic replay impossible by
//! construction rather than by discipline: after a restart this daemon no
//! longer holds the bytes it would have to replay.)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;

use crate::wire::State;

/// one item's identity in the journal. The conversation's committed event
/// sequence is unique within it, so this is the receipt key the collaboration
/// module acknowledges on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub conversation: String,
    pub seq: u64,
}

/// what the journal knows about one item after a fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub participant: String,
    pub binding_generation: u64,
    pub sender: String,
    pub message_id: String,
    pub expires_at: u64,
    /// the canonical request digest, hex. What makes a retry telling apart
    /// from a forgery possible: the SAME id with the SAME bytes is the same
    /// message and gets the state it already has, while the same id with
    /// different bytes is refused rather than delivered.
    pub digest: String,
    pub state: State,
    pub reason: Option<String>,
    /// whether a live delivery attempt currently owns this item.
    ///
    /// In memory only, and deliberately: after a restart nothing owns
    /// anything, so it starts `false` for every recovered entry — which is
    /// what lets a `Queued` item that a crash stranded be picked up again.
    /// While it is `true`, a concurrent re-drive of the same bytes is a
    /// duplicate rather than a second owner.
    pub claimed: bool,
}

/// what admitting an item decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// not seen before. The caller now owns it, durably.
    Fresh,
    /// seen before, byte for byte, and still only `Queued` — so the journal
    /// says no provider has ever been offered it. The caller owns it again and
    /// may offer it, exactly once.
    ///
    /// This is the ONLY way an item is ever re-offered, and it exists because
    /// without it a crash between taking ownership and reaching the lane
    /// strands a message forever: the record says `Queued`, so every re-drive
    /// would be answered "already queued" by a daemon that is not going to do
    /// anything about it.
    Reclaimed,
    /// seen before, byte for byte. The caller reports the state it already has
    /// and offers NOTHING to a provider — re-offering an accepted or unknown
    /// instruction is exactly the duplicate execution the retry contract
    /// exists to prevent.
    Duplicate {
        state: State,
        reason: Option<String>,
    },
    /// seen before under this id, with different bytes. Refused: one id names
    /// one message, and a second body under it is not a retry.
    Conflict,
}

/// one line of the journal. Appended and fsynced before the act it describes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "r", rename_all = "snake_case", deny_unknown_fields)]
enum Record {
    /// which NETWORK this journal belongs to. The first line of every journal,
    /// checked before a single item is folded.
    ///
    /// A [`Key`] is a conversation id and a sequence, and both are unique only
    /// WITHIN one network — so a storage directory that has been pointed at a
    /// second network holds a journal whose keys mean something else. Reading
    /// it would answer a fresh message "already delivered" from another
    /// network's record, which is a delivery that never happened. One header
    /// binds every key in the file, so the identity is stated once rather than
    /// repeated on every line, and a mismatch fails closed.
    ///
    /// It is the chain id — an immutable fact about the network — and never an
    /// endpoint, a URL or a listen address, all of which move while the
    /// network stays the same.
    Network { id: String },
    /// this daemon has taken durable ownership of the item. Written and synced
    /// BEFORE the node is told `Queued`, so queue ownership is never claimed
    /// on the strength of memory alone.
    Queued {
        #[serde(flatten)]
        key: Key,
        participant: String,
        binding_generation: u64,
        sender: String,
        message_id: String,
        expires_at: u64,
        digest: String,
    },
    /// the item is about to be offered to a provider. Written and synced
    /// BEFORE the adapter is called — the whole point being that a crash
    /// after this line and before the next is indistinguishable from success,
    /// and must therefore be reported as such.
    Attempting {
        #[serde(flatten)]
        key: Key,
    },
    /// the outcome, as the adapter reported it.
    Settled {
        #[serde(flatten)]
        key: Key,
        state: State,
        reason: Option<String>,
    },
}

/// the append-only delivery journal, and the state it folds to.
pub struct Outbox {
    path: PathBuf,
    /// the file AND the folded state under ONE lock, held across the whole
    /// append + fsync + publish.
    ///
    /// They are one lock because they are one fact. With the map behind its
    /// own lock, an admission could publish `Queued` the instant it decided to
    /// take an item and only then await the fsync — and a duplicate arriving
    /// in that window would be told, from memory, that this daemon durably
    /// owned something that was not on a disk yet and might never get there.
    /// Nothing is visible here until it is durable.
    journal: tokio::sync::Mutex<Journal>,
}

/// the journal's two halves, kept in step because nothing can see one without
/// the other.
struct Journal {
    file: tokio::fs::File,
    /// the folded state, live. Recovering it and then throwing it away would
    /// leave every duplicate to be re-executed: this IS the dedup map, and it
    /// is the same one the journal replays into at boot.
    state: BTreeMap<Key, Entry>,
    /// the network this journal belongs to, so a rewrite restates it.
    network: String,
    /// why this journal stopped being trustworthy, once it has.
    ///
    /// A write or sync that fails PART WAY leaves a file this process can no
    /// longer describe: the tail may be torn, or durable, or half of each. A
    /// later append onto it would join the tear or repeat an admission, and
    /// the fold that reads it back cannot tell which happened. So the first
    /// uncertain write is the last one — every operation after it refuses,
    /// which in particular means no provider is offered anything, and the
    /// file is left exactly as it is for a strict reopen to fold or reject.
    poisoned: Option<String>,
}

impl Journal {
    /// the check every operation makes before it decides anything.
    fn usable(&self) -> Result<(), String> {
        match &self.poisoned {
            Some(reason) => Err(format!("outbox journal is poisoned: {reason}")),
            None => Ok(()),
        }
    }

    /// append one line and make it durable. Writes nothing to `state` — the
    /// caller publishes, and only after this returns `Ok`.
    async fn append(&mut self, record: &Record) -> Result<(), String> {
        self.usable()?;
        let mut line = serde_json::to_string(record)
            .map_err(|error| format!("encode outbox record: {error}"))?;
        line.push('\n');
        // everything below this line is a write we cannot take back.
        if let Err(error) = self.file.write_all(line.as_bytes()).await {
            return Err(self.poison(format!("append outbox record: {error}")));
        }
        if let Err(error) = self.file.sync_data().await {
            return Err(self.poison(format!("sync outbox record: {error}")));
        }
        Ok(())
    }

    /// stop trusting this journal, and say why. Returns the reason so a caller
    /// can fail with it.
    fn poison(&mut self, reason: String) -> String {
        tracing::error!(
            target: "ducktape::collab",
            reason = "outbox_poisoned",
            detail = %reason,
            "the delivery journal is no longer describable; refusing every further write"
        );
        self.poisoned = Some(reason.clone());
        reason
    }
}

impl Outbox {
    /// open (creating) the journal at `dir/outbox.jsonl` and fold whatever is
    /// already there into the recovered state.
    ///
    /// Returns the recovered entries alongside the handle: a caller that skips
    /// them is a caller that lost the crash boundary, so they are not
    /// available any other way.
    pub async fn open(dir: &Path, network: &str) -> Result<(Self, BTreeMap<Key, Entry>), String> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|error| format!("create outbox dir: {error}"))?;
        let path = dir.join("outbox.jsonl");
        let existing = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("read outbox: {error}")),
        };
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|error| format!("open outbox: {error}"))?;

        // A crash mid-append leaves an unterminated last line. It is the ONE
        // tear a crash can produce, and it is REPAIRED HERE — by dropping it —
        // rather than tolerated for the rest of the file's life.
        //
        // Repairing by appending a newline instead would be worse than doing
        // nothing: it turns an incomplete tail, which recovery knows how to
        // forgive, into a COMPLETE line that will not parse — and the next
        // boot would then fail closed on a file that had just been recovered
        // from cleanly. Truncating to the last record boundary preserves every
        // complete record and leaves nothing to forgive.
        let complete = existing.rfind('\n').map_or(0, |end| end + 1);
        let torn_tail = complete < existing.len();
        if torn_tail {
            file.set_len(complete as u64)
                .await
                .map_err(|error| format!("truncate torn outbox tail: {error}"))?;
            file.sync_all()
                .await
                .map_err(|error| format!("sync truncated outbox: {error}"))?;
            tracing::warn!(
                target: "ducktape::collab",
                reason = "outbox_tail_torn",
                dropped_bytes = existing.len() - complete,
                "dropped an incomplete outbox record left by a crash"
            );
        }
        // a newly created file is not durable until its PARENT DIRECTORY is:
        // a crash can lose the directory entry while the contents survive,
        // and the whole claim this type makes is that queue ownership outlives
        // this process. One fsync per boot buys that.
        let directory = tokio::fs::File::open(dir)
            .await
            .map_err(|error| format!("open outbox dir: {error}"))?;
        directory
            .sync_all()
            .await
            .map_err(|error| format!("sync outbox dir: {error}"))?;

        // folded from the COMPLETE prefix only, so `fold` never has to guess
        // which failure it is looking at — and against THIS network, so a
        // directory that once belonged to another one is refused rather than
        // read as if its keys meant the same things.
        let recovered = fold(&existing[..complete], network)?;
        let mut journal = Journal {
            file,
            state: recovered.clone(),
            network: network.to_string(),
            poisoned: None,
        };
        // an empty journal is a new one, and its first line says whose it is.
        // Written and synced here, before any item can be admitted into it.
        let unclaimed = complete == 0;
        if unclaimed {
            journal
                .append(&Record::Network {
                    id: network.to_string(),
                })
                .await?;
        }
        Ok((
            Self {
                path,
                journal: tokio::sync::Mutex::new(journal),
            },
            recovered,
        ))
    }

    /// take durable ownership of one item, or say why not.
    ///
    /// The check and the claim are one critical section, so two deliveries of
    /// the same message — a node retrying, a duplicate frame — cannot both
    /// find it absent and both offer it to a provider.
    ///
    /// This is the seam that makes a retry safe: an identical one is answered
    /// from the record, and only a genuinely new item ever reaches a provider.
    pub async fn admit(&self, key: &Key, entry: &Entry) -> Result<Admission, String> {
        let mut journal = self.journal.lock().await;
        // checked here and not only inside `append`, because the three answers
        // that DO NOT append are the dangerous ones: `Reclaimed` licenses an
        // offer, and `Duplicate` and `Conflict` are answers read out of a map
        // that a poisoned journal can no longer vouch for.
        journal.usable()?;
        if let Some(existing) = journal.state.get_mut(key) {
            // one id, one message. Different bytes under it are not a retry of
            // anything, and delivering them would let a second instruction
            // inherit the first one's identity.
            if existing.digest != entry.digest {
                return Ok(Admission::Conflict);
            }
            // the item is durably ours and STILL only `Queued`: the journal
            // says no provider has been touched, so re-driving the same bytes
            // is safe and is the only thing that gets an item unstuck after a
            // crash between taking ownership and offering it.
            //
            // The claim flag is what makes it "exactly once": a second
            // re-drive, or one racing this, is a duplicate. And it is only
            // ever reached from `Queued` — an `AdapterAccepted` or a
            // `DeliveryUnknown` is never re-offered, because a model may
            // already have acted on it.
            let reclaimable = existing.state == State::Queued && !existing.claimed;
            if reclaimable {
                existing.claimed = true;
                return Ok(Admission::Reclaimed);
            }
            return Ok(Admission::Duplicate {
                state: existing.state,
                reason: existing.reason.clone(),
            });
        }
        // durable FIRST, published second, both inside this one critical
        // section. A concurrent duplicate either waits here and then sees a
        // record that is already on the disk, or arrives after a failure and
        // finds nothing — never a `Queued` that only memory believes in.
        journal
            .append(&Record::Queued {
                key: key.clone(),
                participant: entry.participant.clone(),
                binding_generation: entry.binding_generation,
                sender: entry.sender.clone(),
                message_id: entry.message_id.clone(),
                expires_at: entry.expires_at,
                digest: entry.digest.clone(),
            })
            .await?;
        journal.state.insert(
            key.clone(),
            Entry {
                claimed: true,
                ..entry.clone()
            },
        );
        Ok(Admission::Fresh)
    }

    /// what the journal currently says about one item.
    pub async fn state_of(&self, key: &Key) -> Option<State> {
        self.journal
            .lock()
            .await
            .state
            .get(key)
            .map(|entry| entry.state)
    }

    /// where the journal lives — for the operator-facing log line at boot, and
    /// for the tests that reopen it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every item this daemon still tracks for one binding, oldest first.
    ///
    /// A READ of durable state and nothing else — no journal write, no claim,
    /// no offer. It exists because a delivery state is a fact only this process
    /// holds: the node cannot re-derive one, so a receipt lost on the way there
    /// is lost for good unless the states on this disk can be re-reported.
    /// See [`crate::wire::Command::MsgReplay`].
    ///
    /// A poisoned journal answers `Err`: its map is no longer something to
    /// vouch for, and re-reporting out of it would state as observed what may
    /// not have been.
    pub async fn tracked(
        &self,
        conversation: &str,
        participant: &str,
    ) -> Result<Vec<(u64, Entry)>, String> {
        let journal = self.journal.lock().await;
        journal.usable()?;
        Ok(journal
            .state
            .iter()
            .filter(|(key, entry)| {
                key.conversation == conversation && entry.participant == participant
            })
            // BTreeMap over `Key`, whose Ord is (conversation, seq): already
            // ascending by sequence within one conversation, which is the order
            // the delivery diagram has to be replayed in.
            .map(|(key, entry)| (key.seq, entry.clone()))
            .collect())
    }

    /// drop every record the agreed clock has aged out, and rewrite the
    /// journal without them. Returns how many went.
    ///
    /// This is the ONLY thing that ever shrinks the dedup record, and the
    /// condition is deliberately not a local one. Age and count are heuristics
    /// — pruning on either would eventually forget an item while the network
    /// could still re-drive it, which is a licence to execute an instruction
    /// twice. `expires_at` is the message's own deadline on the AGREED clock:
    /// past it the network holds no delivery record either, so a replay cannot
    /// be admitted upstream and forgetting it locally costs nothing.
    ///
    /// Only TERMINAL, UNCLAIMED items go. An item still in flight, or one
    /// sitting at `DeliveryUnknown` waiting for someone with authority to
    /// decide, is kept whatever the clock says.
    pub async fn retire_expired(&self, now: u64) -> Result<usize, String> {
        let mut journal = self.journal.lock().await;
        journal.usable()?;
        let survivors: BTreeMap<Key, Entry> = journal
            .state
            .iter()
            .filter(|(_, entry)| {
                let aged_out = entry.expires_at <= now;
                let finished = entry.state.terminal() && !entry.claimed;
                !(aged_out && finished)
            })
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect();
        let pruned = journal.state.len() - survivors.len();
        // no write at all when nothing qualifies: a clock tick is frequent, and
        // rewriting the journal on every one would be an fsync storm for no
        // change.
        if pruned == 0 {
            return Ok(0);
        }
        self.rewrite(&mut journal, survivors).await?;
        Ok(pruned)
    }

    /// replace the journal with one holding exactly `survivors`.
    ///
    /// Written whole, synced, and renamed over the old one, so the file a
    /// crash leaves behind is either the complete old journal or the complete
    /// new one — never a partially pruned record of what this daemon owns.
    /// Nothing in memory moves until the new file is the one on the disk.
    async fn rewrite(
        &self,
        journal: &mut Journal,
        survivors: BTreeMap<Key, Entry>,
    ) -> Result<(), String> {
        // the header again, first: a compacted journal is still a journal, and
        // one that lost its network line would fail to recover at all.
        let mut text = serde_json::to_string(&Record::Network {
            id: journal.network.clone(),
        })
        .map_err(|error| format!("encode compacted network: {error}"))?;
        text.push('\n');
        for (key, entry) in &survivors {
            let admission = Record::Queued {
                key: key.clone(),
                participant: entry.participant.clone(),
                binding_generation: entry.binding_generation,
                sender: entry.sender.clone(),
                message_id: entry.message_id.clone(),
                expires_at: entry.expires_at,
                digest: entry.digest.clone(),
            };
            text.push_str(
                &serde_json::to_string(&admission)
                    .map_err(|error| format!("encode compacted admission: {error}"))?,
            );
            text.push('\n');
            // an item that never moved needs no second record; one that did
            // carries its CURRENT state, which folds back to exactly what the
            // longer history folded to.
            let moved = entry.state != State::Queued || entry.reason.is_some();
            if moved {
                let outcome = Record::Settled {
                    key: key.clone(),
                    state: entry.state,
                    reason: entry.reason.clone(),
                };
                text.push_str(
                    &serde_json::to_string(&outcome)
                        .map_err(|error| format!("encode compacted outcome: {error}"))?,
                );
                text.push('\n');
            }
        }

        let temp = self.path.with_extension("compacting");
        let mut replacement = tokio::fs::File::create(&temp)
            .await
            .map_err(|error| format!("create compacted outbox: {error}"))?;
        replacement
            .write_all(text.as_bytes())
            .await
            .map_err(|error| format!("write compacted outbox: {error}"))?;
        replacement
            .sync_all()
            .await
            .map_err(|error| format!("sync compacted outbox: {error}"))?;
        drop(replacement);
        // up to here the live journal is untouched, so every failure above
        // leaves the daemon exactly as it was.
        let directory = self
            .path
            .parent()
            .ok_or_else(|| "outbox has no parent directory".to_string())?
            .to_path_buf();
        tokio::fs::rename(&temp, &self.path)
            .await
            .map_err(|error| format!("install compacted outbox: {error}"))?;

        // EVERYTHING past the rename poisons on failure, not just the reopen.
        // The old inode is unlinked the moment the rename lands, so a handle
        // still pointing at it appends to a file no boot will ever read: the
        // records would be fsynced, acknowledged, and then simply gone. There
        // is no partial recovery from that, so the journal stops here instead.
        if let Err(error) = install(&directory).await {
            return Err(journal.poison(error));
        }
        // the file this handle pointed at is gone; failing to pick up the new
        // one leaves appends going nowhere visible, which is exactly the
        // state nothing may be offered from.
        let file = match tokio::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .await
        {
            Ok(file) => file,
            Err(error) => return Err(journal.poison(format!("reopen compacted outbox: {error}"))),
        };
        journal.file = file;
        journal.state = survivors;
        Ok(())
    }

    /// pretend a write failed part way, for the tests that assert what happens
    /// next.
    ///
    /// The real trigger is an I/O error inside [`Journal::append`], which no
    /// test can produce on a healthy filesystem without either a privileged
    /// device or an fd this type does not hand out. What the tests are for is
    /// the CONSEQUENCE — that nothing is offered to a provider afterwards —
    /// and that is reachable from the same flag the real failure sets.
    #[cfg(test)]
    pub(crate) async fn injure(&self, reason: &str) {
        self.journal.lock().await.poison(reason.to_string());
    }

    /// mark an item as about to reach a provider. Returns once durable; the
    /// caller must not touch the provider until it has.
    ///
    /// The in-memory state becomes `DeliveryUnknown` at the same moment,
    /// because from here until an outcome is recorded that IS what is true: if
    /// this process died right now, recovery would say exactly this.
    pub async fn attempting(&self, key: &Key) -> Result<(), String> {
        let mut journal = self.journal.lock().await;
        journal
            .append(&Record::Attempting { key: key.clone() })
            .await?;
        remember(
            &mut journal.state,
            key,
            State::DeliveryUnknown,
            Some("in_flight"),
        );
        Ok(())
    }

    /// record the outcome.
    ///
    /// Refuses to walk a state BACKWARDS. An item that reached a provider is
    /// never returned to `Queued` by a later report, because `Queued` is the
    /// one state that licenses offering it again — and the whole point of
    /// `DeliveryUnknown` is that offering it again may repeat something a
    /// model already did.
    pub async fn settled(
        &self,
        key: &Key,
        state: State,
        reason: Option<&str>,
    ) -> Result<(), String> {
        // the check and the write are one critical section, so the state this
        // decided against cannot have moved by the time the record lands.
        let mut journal = self.journal.lock().await;
        let downgrades = state == State::Queued
            && journal
                .state
                .get(key)
                .is_some_and(|held| held.state != State::Queued);
        if downgrades {
            tracing::warn!(
                target: "ducktape::collab",
                conversation = %key.conversation,
                seq = key.seq,
                reason = "would_downgrade_to_queued",
                "refused to move a delivery back to queued"
            );
            return Ok(());
        }
        journal
            .append(&Record::Settled {
                key: key.clone(),
                state,
                reason: reason.map(str::to_string),
            })
            .await?;
        remember(&mut journal.state, key, state, reason);
        if let Some(entry) = journal.state.get_mut(key) {
            entry.claimed = false;
        }
        Ok(())
    }

    /// record that the provider was NEVER OFFERED this item, and it is
    /// deliverable again.
    ///
    /// The one path back to `Queued`, and separate from [`Outbox::settled`] for
    /// exactly the reason that matters: those two look identical in the
    /// journal and mean opposite things.
    ///
    /// `attempting` is written before the adapter is called, so from the
    /// journal's point of view the item is already uncertain. When the adapter
    /// then reports that it never wrote anything — the session is closed, the
    /// CLI is absent, a turn is running and this input is not urgent — that
    /// uncertainty is RESOLVED, by the only thing in a position to resolve it.
    /// Going back to `Queued` on the strength of a guess would be a licence to
    /// replay; going back on the adapter's own statement is just the truth.
    pub async fn not_offered(&self, key: &Key, reason: &str) -> Result<(), String> {
        let mut journal = self.journal.lock().await;
        journal
            .append(&Record::Settled {
                key: key.clone(),
                state: State::Queued,
                reason: Some(reason.to_string()),
            })
            .await?;
        remember(&mut journal.state, key, State::Queued, Some(reason));
        // released, so the network re-driving it finds a deliverable item
        // rather than being told it is already queued by nobody.
        if let Some(entry) = journal.state.get_mut(key) {
            entry.claimed = false;
        }
        Ok(())
    }
}

/// make a rename durable by syncing the directory that now names the new file.
///
/// Split out so [`Outbox::rewrite`] has ONE post-rename failure path to poison
/// on: every step from here is a step past the point where the old journal
/// stopped existing.
async fn install(directory: &Path) -> Result<(), String> {
    let opened = tokio::fs::File::open(directory)
        .await
        .map_err(|error| format!("open outbox dir after compaction: {error}"))?;
    opened
        .sync_all()
        .await
        .map_err(|error| format!("sync outbox dir after compaction: {error}"))
}

/// publish a transition into the live map, after it is durable.
fn remember(state: &mut BTreeMap<Key, Entry>, key: &Key, to: State, reason: Option<&str>) {
    if let Some(entry) = state.get_mut(key) {
        entry.state = to;
        entry.reason = reason.map(str::to_string);
    }
}

/// replay the journal into per-item state.
///
/// Pure, so the crash boundaries are unit-testable without a disk: this is the
/// function the recovery contract actually lives in.
///
/// **Every line here is a COMPLETE record, and every one must read.** The
/// caller has already dropped an unterminated tail, so anything unreadable at
/// this point is corruption of a record that was finished — and this fails
/// closed on it rather than skipping.
///
/// Skipping is the tempting mistake and the dangerous one: the record it
/// cannot read might be the `Attempting` that is the only thing standing
/// between an already-executed instruction and an automatic replay, and
/// skipping it silently recovers that item as `Queued` — deliverable again.
fn fold(text: &str, network: &str) -> Result<BTreeMap<Key, Entry>, String> {
    let mut entries: BTreeMap<Key, Entry> = BTreeMap::new();
    let mut header_seen = false;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str::<Record>(line).map_err(|error| {
            format!(
                "outbox record {} is unreadable ({error}); refusing to recover past it",
                index + 1
            )
        })?;
        // whose journal this is comes FIRST, and once: an item folded before
        // the header would be an item admitted under an identity this daemon
        // never checked, and a second header is a spliced file.
        let names_the_network = matches!(record, Record::Network { .. });
        let second_header = names_the_network && header_seen;
        let item_before_the_header = !names_the_network && !header_seen;
        if second_header || item_before_the_header {
            return Err(format!(
                "outbox record {} is not where a journal states its network; refusing to recover",
                index + 1
            ));
        }
        match record {
            Record::Network { id } => {
                let another_network = id != network;
                if another_network {
                    return Err(
                        "outbox journal belongs to a different network; refusing to recover"
                            .to_string(),
                    );
                }
                header_seen = true;
            }
            Record::Queued {
                key,
                participant,
                binding_generation,
                sender,
                message_id,
                expires_at,
                digest,
            } => {
                // one item, one admission. `admit` writes this record only for
                // a key it did not find, so a second one is a duplicated or
                // torn write — and letting it through would REPLACE a settled
                // entry with a fresh `Queued`, which is a licence to re-offer
                // something a provider has already been given.
                let admitted_twice = entries.contains_key(&key);
                if admitted_twice {
                    return Err(format!(
                        "outbox record {} admits seq {} a second time; refusing to recover past it",
                        index + 1,
                        key.seq
                    ));
                }
                entries.insert(
                    key,
                    Entry {
                        participant,
                        binding_generation,
                        sender,
                        message_id,
                        expires_at,
                        digest,
                        state: State::Queued,
                        reason: None,
                        // nothing owns anything after a restart, which is
                        // what lets a stranded `Queued` be picked up again.
                        claimed: false,
                    },
                );
            }
            // the crash boundary. An item recorded as attempted and never
            // settled reaches here as `DeliveryUnknown` and stays there: the
            // provider may have read it, so nothing may claim it did not.
            Record::Attempting { key } => {
                if let Some(entry) = entries.get_mut(&key) {
                    entry.state = State::DeliveryUnknown;
                    entry.reason = Some("crashed_after_provider_input".to_string());
                }
            }
            Record::Settled { key, state, reason } => {
                if let Some(entry) = entries.get_mut(&key) {
                    entry.state = state;
                    entry.reason = reason;
                }
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// the network every test journal belongs to.
    const NETWORK: &str = "ducktape-test@aaaa";

    /// the first line of any journal these tests hand to [`fold`] directly.
    fn header() -> String {
        serde_json::to_string(&Record::Network {
            id: NETWORK.to_string(),
        })
        .unwrap()
    }

    fn key(seq: u64) -> Key {
        Key {
            conversation: "conv-1".to_string(),
            seq,
        }
    }

    fn entry() -> Entry {
        entry_with("digest-aaaa")
    }

    fn entry_with(digest: &str) -> Entry {
        Entry {
            participant: "p-recipient".to_string(),
            binding_generation: 3,
            sender: "p-sender".to_string(),
            message_id: "m-7".to_string(),
            expires_at: 1_200,
            digest: digest.to_string(),
            state: State::Queued,
            reason: None,
            claimed: false,
        }
    }

    async fn scratch(name: &str) -> (PathBuf, Outbox, BTreeMap<Key, Entry>) {
        let dir = std::env::temp_dir().join(format!(
            "ducktape-outbox-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let (outbox, recovered) = Outbox::open(&dir, NETWORK).await.expect("opens");
        (dir, outbox, recovered)
    }

    /// crash boundary 1: we took ownership but never reached the provider.
    /// Nothing was read, so the item is still deliverable.
    #[tokio::test]
    async fn a_crash_before_provider_input_recovers_as_queued() {
        let (dir, outbox, _) = scratch("before-input").await;
        outbox.admit(&key(7), &entry()).await.expect("admits");
        drop(outbox); // the crash

        let (_reopened, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens");
        assert_eq!(recovered[&key(7)].state, State::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// crash boundary 2: THE one that matters. We wrote the message into a
    /// provider and died before its outcome was recorded, so the model may
    /// already have read and acted on it. Anything other than
    /// `DeliveryUnknown` here is a fabricated status — and a `Queued` in
    /// particular would license an automatic replay of an instruction that has
    /// already been carried out.
    #[tokio::test]
    async fn a_crash_after_provider_input_recovers_as_delivery_unknown() {
        let (dir, outbox, _) = scratch("after-input").await;
        outbox.admit(&key(7), &entry()).await.expect("admits");
        outbox.attempting(&key(7)).await.expect("attempting");
        drop(outbox); // the crash, between the provider write and its receipt

        let (_reopened, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens");
        let entry = &recovered[&key(7)];
        assert_eq!(
            entry.state,
            State::DeliveryUnknown,
            "an attempt with no recorded outcome must never recover as anything else"
        );
        assert_eq!(
            entry.reason.as_deref(),
            Some("crashed_after_provider_input")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// crash boundary 3: the outcome was durable before we died, so it stands.
    #[tokio::test]
    async fn a_crash_after_the_receipt_recovers_that_receipt() {
        let (dir, outbox, _) = scratch("after-receipt").await;
        outbox.admit(&key(7), &entry()).await.expect("admits");
        outbox.attempting(&key(7)).await.expect("attempting");
        outbox
            .settled(&key(7), State::AdapterAccepted, None)
            .await
            .expect("settled");
        drop(outbox);

        let (_reopened, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens");
        assert_eq!(recovered[&key(7)].state, State::AdapterAccepted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// the identity that survives a crash is the SENDER's, not a fresh one: an
    /// explicit retry has to reuse it so the receiver can be told it may have
    /// seen this before.
    #[tokio::test]
    async fn recovery_preserves_the_senders_message_id() {
        let (dir, outbox, _) = scratch("identity").await;
        outbox.admit(&key(7), &entry()).await.expect("admits");
        outbox.attempting(&key(7)).await.expect("attempting");
        drop(outbox);

        let (_reopened, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens");
        let entry = &recovered[&key(7)];
        assert_eq!(entry.message_id, "m-7");
        assert_eq!(entry.sender, "p-sender");
        assert_eq!(entry.binding_generation, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn queued_line(seq: u64) -> String {
        serde_json::to_string(&Record::Queued {
            key: key(seq),
            participant: "p-recipient".to_string(),
            binding_generation: 3,
            sender: "p-sender".to_string(),
            message_id: format!("m-{seq}"),
            expires_at: 1_200,
            digest: "digest-aaaa".to_string(),
        })
        .unwrap()
    }

    /// The hole a "skip whatever will not parse" fold leaves: the damaged
    /// record can be the `Attempting` that is the ONLY thing standing between
    /// an already-executed instruction and an automatic replay. Skipping it
    /// recovers that item as `Queued` — deliverable again. A record that was
    /// complete and is now unreadable therefore fails closed.
    #[test]
    fn a_corrupt_record_is_never_skipped() {
        let corrupt_attempting = r#"{"r":"attem GARBAGE "seq":7}"#;
        let journal = format!(
            "{}\n{}\n{corrupt_attempting}\n{}\n",
            header(),
            queued_line(7),
            queued_line(8)
        );
        let error =
            fold(&journal, NETWORK).expect_err("a corrupt committed record must fail closed");
        assert!(error.contains("unreadable"), "{error}");
    }

    /// two items in one journal do not read each other's transitions.
    #[test]
    fn each_item_folds_independently() {
        let mut text = format!("{}\n", header());
        for seq in [7, 8] {
            text.push_str(&queued_line(seq));
            text.push('\n');
        }
        text.push_str(&serde_json::to_string(&Record::Attempting { key: key(7) }).unwrap());
        text.push('\n');
        text.push_str(
            &serde_json::to_string(&Record::Settled {
                key: key(7),
                state: State::Refused,
                reason: Some("auth_rejected".to_string()),
            })
            .unwrap(),
        );
        text.push('\n');
        text.push_str(&serde_json::to_string(&Record::Attempting { key: key(8) }).unwrap());

        let recovered = fold(&text, NETWORK).expect("folds");
        assert_eq!(recovered[&key(7)].state, State::Refused);
        assert_eq!(recovered[&key(7)].reason.as_deref(), Some("auth_rejected"));
        assert_eq!(recovered[&key(8)].state, State::DeliveryUnknown);
    }

    /// THE duplicate-execution guard. A node retrying, or a frame arriving
    /// twice, must not put the same instruction into a provider twice — the
    /// second admission answers from the record instead.
    #[tokio::test]
    async fn an_identical_retry_is_answered_from_the_record_not_re_offered() {
        let (dir, outbox, _) = scratch("dedup").await;
        assert_eq!(
            outbox.admit(&key(7), &entry()).await.expect("admits"),
            Admission::Fresh
        );
        outbox.attempting(&key(7)).await.expect("attempting");
        outbox
            .settled(&key(7), State::AdapterAccepted, Some("queued_by_cli"))
            .await
            .expect("settled");

        let again = outbox.admit(&key(7), &entry()).await.expect("admits");
        assert_eq!(
            again,
            Admission::Duplicate {
                state: State::AdapterAccepted,
                reason: Some("queued_by_cli".to_string()),
            },
            "an identical retry reports what already happened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One id names one message. A second body under it is not a retry of the
    /// first, and delivering it would let a different instruction inherit an
    /// identity the network already accounted for.
    #[tokio::test]
    async fn a_conflicting_body_under_the_same_id_is_refused() {
        let (dir, outbox, _) = scratch("conflict").await;
        assert_eq!(
            outbox
                .admit(&key(7), &entry_with("digest-aaaa"))
                .await
                .expect("admits"),
            Admission::Fresh
        );
        assert_eq!(
            outbox
                .admit(&key(7), &entry_with("digest-bbbb"))
                .await
                .expect("admits"),
            Admission::Conflict
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dedup must survive the restart, or a crash becomes a licence to replay.
    #[tokio::test]
    async fn dedup_survives_a_restart() {
        let (dir, outbox, _) = scratch("dedup-restart").await;
        outbox.admit(&key(7), &entry()).await.expect("admits");
        outbox.attempting(&key(7)).await.expect("attempting");
        drop(outbox);

        let (reopened, _) = Outbox::open(&dir, NETWORK).await.expect("reopens");
        let again = reopened.admit(&key(7), &entry()).await.expect("admits");
        assert_eq!(
            again,
            Admission::Duplicate {
                state: State::DeliveryUnknown,
                reason: Some("crashed_after_provider_input".to_string()),
            },
            "a message that may already have been read must not be re-offered after a restart"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Queued` is the one state that licenses offering an item to a provider.
    /// Nothing may move an item back into it once the provider has been
    /// touched — that would turn a "we do not know" into "deliver it again".
    #[tokio::test]
    async fn nothing_moves_a_delivery_back_to_queued() {
        let (dir, outbox, _) = scratch("no-downgrade").await;
        outbox.admit(&key(7), &entry()).await.expect("admits");
        outbox.attempting(&key(7)).await.expect("attempting");
        assert_eq!(outbox.state_of(&key(7)).await, Some(State::DeliveryUnknown));

        outbox
            .settled(&key(7), State::Queued, Some("turn_busy"))
            .await
            .expect("accepted but ignored");
        assert_eq!(
            outbox.state_of(&key(7)).await,
            Some(State::DeliveryUnknown),
            "an in-flight delivery must not be walked back to queued"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A conversation id and a sequence mean something only WITHIN one
    /// network. A storage directory that has been pointed at a second one
    /// holds a journal whose keys are another network's, and reading it would
    /// answer a fresh message "already delivered" from a record of a delivery
    /// that never happened here.
    #[tokio::test]
    async fn a_journal_from_another_network_is_refused() {
        let (dir, outbox, _) = scratch("foreign-network").await;
        outbox.admit(&key(7), &entry()).await.expect("admits");
        drop(outbox);

        let refused = Outbox::open(&dir, "some-other-network@bbbb").await;
        let error = refused
            .err()
            .expect("another network's journal is not ours");
        assert!(error.contains("different network"), "{error}");

        // and our own still opens, with its record intact.
        let (_ours, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens");
        assert_eq!(recovered[&key(7)].state, State::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The header is the FIRST line or the journal is not one. An item folded
    /// before it would be an item admitted under an identity nothing checked.
    #[test]
    fn a_journal_that_does_not_name_its_network_first_is_refused() {
        for spliced in [
            format!("{}\n", queued_line(7)),
            format!("{}\n{}\n{}\n", header(), queued_line(7), header()),
        ] {
            let error = fold(&spliced, NETWORK)
                .expect_err("a journal must name its network first and once");
            assert!(error.contains("states its network"), "{error}");
        }
    }

    /// A crash mid-append leaves a half-written record. Recovery must survive
    /// it, keep every complete record before it, and — the part a
    /// newline-repair gets wrong — still be recoverable the NEXT time.
    ///
    /// Appending a newline behind the tear would make it a complete line that
    /// does not parse, so this same file would fail closed on the second boot.
    /// Reopening twice is the assertion that says so.
    #[tokio::test]
    async fn a_torn_tail_is_dropped_and_the_file_reopens_cleanly_twice() {
        let dir = std::env::temp_dir().join(format!(
            "ducktape-outbox-torn-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let attempting = serde_json::to_string(&Record::Attempting { key: key(7) }).unwrap();
        std::fs::write(
            dir.join("outbox.jsonl"),
            // a header and two complete records, then a record cut off
            // mid-write.
            format!(
                "{}\n{}\n{attempting}\n{{\"r\":\"settl",
                header(),
                queued_line(7)
            ),
        )
        .expect("a journal with a half-written tail");

        // first boot: the complete prefix stands, so the item is still the
        // cautious `DeliveryUnknown` its `Attempting` record says it is.
        let (outbox, recovered) = Outbox::open(&dir, NETWORK).await.expect("recovers");
        assert_eq!(recovered[&key(7)].state, State::DeliveryUnknown);
        outbox
            .settled(&key(7), State::Refused, Some("auth_rejected"))
            .await
            .expect("settles");
        drop(outbox);

        // second boot: still readable. This is what fails if the tear was
        // "repaired" into a complete unparseable line instead of dropped.
        let (outbox, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens once");
        assert_eq!(recovered[&key(7)].state, State::Refused);
        drop(outbox);

        // third, for the same reason: repair must be idempotent, not a state
        // the file passes through on its way to being unreadable.
        let (_again, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens twice");
        assert_eq!(recovered[&key(7)].state, State::Refused);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Retirement follows the message's own DEADLINE on the agreed clock — and
    /// it keeps what is not finished with. An item at `DeliveryUnknown` is
    /// waiting for someone with authority to decide, and one a live attempt
    /// owns is in flight, so neither is disposable whatever the clock says.
    #[tokio::test]
    async fn the_clock_retires_only_expired_finished_deliveries() {
        let (dir, outbox, _) = scratch("retire").await;
        // 7: settled and past its deadline — the only one that may go.
        outbox.admit(&key(7), &entry()).await.expect("admits");
        outbox.attempting(&key(7)).await.expect("attempting");
        outbox
            .settled(&key(7), State::AdapterAccepted, Some("queued_by_cli"))
            .await
            .expect("settled");
        // 8: past its deadline, but nobody knows whether a model read it.
        outbox
            .admit(&key(8), &entry_with("digest-bbbb"))
            .await
            .expect("admits");
        outbox.attempting(&key(8)).await.expect("attempting");
        // 9: settled, but its deadline has not passed.
        let mut later = entry_with("digest-cccc");
        later.expires_at = 5_000;
        outbox.admit(&key(9), &later).await.expect("admits");
        outbox
            .settled(&key(9), State::Refused, Some("queue_refused"))
            .await
            .expect("settled");
        // 10: finished and past its deadline, but a live attempt owns it.
        let mut claimed = entry_with("digest-dddd");
        claimed.state = State::Refused;
        outbox.admit(&key(10), &claimed).await.expect("admits");

        // nothing has aged out yet, so nothing is written either: a clock tick
        // is frequent, and rewriting on every one would be an fsync storm.
        let untouched = std::fs::read_to_string(dir.join("outbox.jsonl")).expect("the journal");
        assert_eq!(outbox.retire_expired(1_199).await.expect("decides"), 0);
        assert_eq!(
            std::fs::read_to_string(dir.join("outbox.jsonl")).expect("the journal"),
            untouched,
            "a tick that retires nothing must not rewrite the journal"
        );

        let pruned = outbox.retire_expired(1_200).await.expect("compacts");
        assert_eq!(pruned, 1, "only the finished item past its deadline may go");
        assert_eq!(outbox.state_of(&key(7)).await, None);
        assert_eq!(
            outbox.state_of(&key(8)).await,
            Some(State::DeliveryUnknown),
            "an undecided delivery is not disposable"
        );
        assert_eq!(outbox.state_of(&key(9)).await, Some(State::Refused));
        assert_eq!(
            outbox.state_of(&key(10)).await,
            Some(State::Refused),
            "an item a live attempt owns is not pruned under it"
        );

        // and the compacted file folds back to exactly that.
        drop(outbox);
        let (_reopened, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens");
        assert!(!recovered.contains_key(&key(7)));
        assert_eq!(recovered[&key(8)].state, State::DeliveryUnknown);
        assert_eq!(recovered[&key(9)].state, State::Refused);
        assert_eq!(recovered[&key(9)].reason.as_deref(), Some("queue_refused"));
        assert_eq!(recovered[&key(9)].digest, "digest-cccc");
        assert_eq!(recovered[&key(10)].state, State::Refused);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A compaction that cannot even start must leave every record exactly
    /// where it was: nothing in memory moves until the new file is the one on
    /// the disk.
    #[tokio::test]
    async fn a_compaction_that_cannot_start_leaves_every_record_standing() {
        let (dir, outbox, _) = scratch("retire-rollback").await;
        outbox.admit(&key(7), &entry()).await.expect("admits");
        outbox
            .settled(&key(7), State::Refused, Some("queue_refused"))
            .await
            .expect("settled");

        // a directory where the replacement file goes: the compaction fails
        // BEFORE the rename, deterministically and without a privileged fd.
        std::fs::create_dir_all(dir.join("outbox.compacting")).expect("blocks the temp path");
        let refused = outbox.retire_expired(1_200).await;
        assert!(refused.is_err(), "the compaction must fail: {refused:?}");
        assert_eq!(outbox.state_of(&key(7)).await, Some(State::Refused));

        drop(outbox);
        let (_reopened, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens");
        assert_eq!(recovered[&key(7)].state, State::Refused);
        assert_eq!(recovered[&key(7)].reason.as_deref(), Some("queue_refused"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A write that failed part way leaves a file this process cannot
    /// describe. Every operation after it must refuse — and `attempting` in
    /// particular, because that is the one a delivery has to get past before
    /// it may touch a provider.
    #[tokio::test]
    async fn an_uncertain_write_stops_the_journal_for_good() {
        let (dir, outbox, _) = scratch("poisoned").await;
        outbox.admit(&key(7), &entry()).await.expect("admits");
        outbox.injure("sync outbox record: simulated").await;

        for refused in [
            outbox.admit(&key(8), &entry()).await.err(),
            outbox.attempting(&key(7)).await.err(),
            outbox
                .settled(&key(7), State::AdapterAccepted, None)
                .await
                .err(),
            outbox.not_offered(&key(7), "turn_busy").await.err(),
        ] {
            let error = refused.expect("a poisoned journal refuses every write");
            assert!(error.contains("poisoned"), "{error}");
        }
        // and a retry of the item it already owns is refused rather than
        // answered from a map the journal can no longer vouch for.
        let retried = outbox.admit(&key(7), &entry()).await;
        assert!(retried.is_err(), "{retried:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A duplicated or torn admission must not resurrect a settled item: the
    /// second `Queued` would overwrite the outcome with a state that licenses
    /// offering the message again.
    #[test]
    fn a_second_admission_of_one_item_fails_closed() {
        let journal = format!(
            "{}\n{}\n{}\n{}\n",
            header(),
            queued_line(7),
            serde_json::to_string(&Record::Settled {
                key: key(7),
                state: State::AdapterAccepted,
                reason: None,
            })
            .unwrap(),
            queued_line(7),
        );
        let error = fold(&journal, NETWORK).expect_err("a repeated admission must fail closed");
        assert!(error.contains("a second time"), "{error}");
    }

    /// Ownership becomes visible only once it is on the disk. With the map
    /// behind its own lock, an admission could publish `Queued` and only then
    /// await the fsync — and a duplicate arriving in that window would be told
    /// this daemon durably owned something that was not written yet.
    #[tokio::test]
    async fn a_duplicate_never_sees_ownership_that_is_not_yet_durable() {
        let (dir, outbox, _) = scratch("durable-before-visible").await;
        let outbox = Arc::new(outbox);

        // two admissions of the same item, racing.
        let first = {
            let outbox = outbox.clone();
            tokio::spawn(async move { outbox.admit(&key(7), &entry()).await })
        };
        let second = {
            let outbox = outbox.clone();
            tokio::spawn(async move { outbox.admit(&key(7), &entry()).await })
        };
        let outcomes = [
            first.await.expect("joins").expect("admits"),
            second.await.expect("joins").expect("admits"),
        ];

        // exactly one takes it; the other is told what the record says.
        let fresh = outcomes
            .iter()
            .filter(|admission| **admission == Admission::Fresh)
            .count();
        assert_eq!(
            fresh, 1,
            "exactly one admission may own the item: {outcomes:?}"
        );

        // and whatever the duplicate was told is what is actually on the disk.
        drop(outbox);
        let (_reopened, recovered) = Outbox::open(&dir, NETWORK).await.expect("reopens");
        assert_eq!(recovered[&key(7)].state, State::Queued);
        let journal = std::fs::read_to_string(dir.join("outbox.jsonl")).expect("the journal");
        assert_eq!(
            journal.matches(r#""r":"queued""#).count(),
            1,
            "one admission, one record: {journal}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
