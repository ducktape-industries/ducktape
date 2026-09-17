//! the task board (assigned-list kind): an ordered, deterministic task list
//! over the module's qmdb store.
//!
//! each task is its OWN record (`t/{task_id}`), so a create or a status change
//! touches one key. the store hashes its keys and cannot enumerate, so the
//! ascending id order [`TaskQuery::List`] answers in lives in ONE enumeration
//! index record (`t#`) -- the same shape `pages` uses for its page index.
//!
//! writes stage during `execute` and publish only at `commit_block`; reads go
//! through the staged overlay, so a later op in the same block sees an earlier
//! one's write. BOTH queries read through it -- DELIBERATELY, and unlike the job
//! board's committed-only `Get`: `automations` probes this board mid-block for a
//! duplicate id before emitting its create, so a task staged earlier in the
//! SAME block must be visible or the probe passes and the create aborts the
//! block. do not "harmonize" the two boards' read visibility.
//!
//! every read is BOUNDED: `Get` is one store read, `List` at most
//! [`MAX_LIST_LIMIT`] plus the index. that bound is the whole point -- the wasm
//! host allows 4096 store reads per dispatch, so an unpaged board walk on a
//! consensus caller's execute path (the agent settle in `runs`, the duplicate
//! probe in `automations`) would wedge those callers PERMANENTLY once the
//! board outgrew the budget. [`TaskMsg::DeleteTask`] is how an owner recedes
//! from that limit, and [`MAX_OPEN_TASKS_PER_OWNER`] is what keeps any ONE
//! account from being the reason the board fills at all -- the job board's
//! `submitter`/`Cancel`/`Prune` shape, over [`Task::owner`].

use std::collections::BTreeSet;
use std::ops::Bound;

use sdk::{Error, StagedStore, require_non_empty};

use crate::{Party, Task, TaskMsg, TaskQuery, TaskReply, TaskStatus, check_record, stage_record};

/// max bytes of a `task_id`, matching the job board's [`crate::MAX_JOB_ID`].
///
/// this cap is load-bearing, not cosmetic: every id shares the ONE `t#` index
/// record bounded by [`crate::MAX_RECORD_BYTES`], so an UNCAPPED id let two or
/// three ops (an op frame carries up to `node::MAX_FRAME_BYTES`, 1 MiB + 16 KiB)
/// pack that record to just under the cap and refuse EVERY later create, for
/// every user, forever -- there is no delete op to recover with.
pub const MAX_TASK_ID: usize = 256;

/// max distinct tasks on the board, the peer of the job board's
/// [`crate::MAX_JOBS`]. [`TaskMsg::DeleteTask`] is the only way this ceiling
/// recedes -- which is exactly why it must refuse by name: past it, a create
/// says "task board full" instead of failing whatever opaque byte/budget
/// limit it would otherwise hit first.
pub const MAX_TASKS: usize = 4096;

/// max tasks one owner may hold open at once, well under [`MAX_TASKS`]: no
/// single account can fill a shared board -- 33 accounts already exhaust it
/// at this cap, and [`TaskMsg::DeleteTask`] is what lets an owner recede from
/// it instead of burning a permanent slot.
pub const MAX_OPEN_TASKS_PER_OWNER: usize = 128;

pub use tasks_wire::MAX_LIST_LIMIT;

/// one task record per id.
const RECORD_PREFIX: &[u8] = b"t/";
/// the per-owner live-task census (u64 LE), one record per owner -- what
/// [`MAX_OPEN_TASKS_PER_OWNER`] is checked against. an EMPTY (zero) count
/// drops the key entirely, the job board's `stage_count` rule, so an owner
/// with no tasks left hashes the same as one who never created any.
const OWNER_COUNT_PREFIX: &[u8] = b"t@";
/// the enumeration index: every live task id, ascending.
///
// ponytail: ONE index record holds every task id, so a create is O(all ids) in
// bytes. that is bounded on both axes -- MAX_TASKS ids, each at most
// MAX_TASK_ID bytes -- and whichever of the two guards trips first (the count
// cap here, or MAX_RECORD_BYTES on the index record at long ids) refuses the
// create loudly. a human task list never gets there; if it must, shard the
// index by id prefix BEFORE the board is used at that scale.
const INDEX_KEY: &[u8] = b"t#";

fn record_key(task_id: &str) -> Vec<u8> {
    let mut key = RECORD_PREFIX.to_vec();
    key.extend_from_slice(task_id.as_bytes());
    key
}

fn owner_count_key(owner: &Party) -> Vec<u8> {
    let mut key = OWNER_COUNT_PREFIX.to_vec();
    key.extend_from_slice(&sdk::wire::encode(owner));
    key
}

/// count of an owner's live tasks, reading through the staged overlay.
async fn owner_count(staged: &StagedStore, owner: &Party) -> Result<u64, Error> {
    let Some(bytes) = staged.get(&owner_count_key(owner)).await? else {
        return Ok(0);
    };
    let raw: [u8; 8] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Module("owner task census record is not a u64".into()))?;
    Ok(u64::from_le_bytes(raw))
}

/// stage an owner's census (see [`OWNER_COUNT_PREFIX`] on the zero case).
fn stage_owner_count(staged: &mut StagedStore, owner: &Party, count: u64) {
    let key = owner_count_key(owner);
    if count == 0 {
        staged.delete(key);
        return;
    }
    staged.stage(key, count.to_le_bytes().to_vec());
}

/// read one task through the staged overlay (`None` == absent).
pub(crate) async fn load(staged: &StagedStore, task_id: &str) -> Result<Option<Task>, Error> {
    let Some(bytes) = staged.get(&record_key(task_id)).await? else {
        return Ok(None);
    };
    sdk::wire::decode(&bytes)
        .map(Some)
        .map_err(|e| Error::Module(format!("task record decode: {e}")))
}

/// read the enumeration index through the staged overlay. absent reads as the
/// empty set; `BTreeSet` serializes ASCENDING, so the bytes are canonical and
/// every validator commits the same index record.
async fn load_index(staged: &StagedStore) -> Result<BTreeSet<String>, Error> {
    let Some(bytes) = staged.get(INDEX_KEY).await? else {
        return Ok(BTreeSet::new());
    };
    sdk::wire::decode(&bytes).map_err(|e| Error::Module(format!("task index decode: {e}")))
}

/// the task's owner: the named account when one is given, else the actor.
/// ownership is attribution and the per-owner census key, never consent,
/// so any actor may name any existing account.
fn resolve_owner(actor: &Party, override_owner: Option<u64>) -> Result<Party, Error> {
    let Some(account) = override_owner else {
        return Ok(actor.clone());
    };
    if account == 0 {
        return Err(Error::Module("task owner account must be nonzero".into()));
    }
    Ok(Party::Account(account))
}

async fn create(
    staged: &mut StagedStore,
    task_id: String,
    title: String,
    owner_override: Option<u64>,
    actor: &Party,
    consensus_time: u64,
) -> Result<(), Error> {
    sdk::validate_id("task_id", &task_id, MAX_TASK_ID)?;
    require_non_empty("title", &title)?;
    if load(staged, &task_id).await?.is_some() {
        return Err(Error::Module(format!("task already exists: {task_id}")));
    }

    let mut index = load_index(staged).await?;
    if index.len() >= MAX_TASKS {
        return Err(Error::Module(format!(
            "task board full: {MAX_TASKS} live tasks"
        )));
    }
    let owner = resolve_owner(actor, owner_override)?;
    let owner_live = owner_count(staged, &owner).await?;
    if owner_live >= MAX_OPEN_TASKS_PER_OWNER as u64 {
        return Err(Error::Module(format!(
            "task owner at cap: {MAX_OPEN_TASKS_PER_OWNER} open tasks"
        )));
    }

    let task = Task {
        id: task_id.clone(),
        title,
        status: TaskStatus::Open,
        owner: owner.clone(),
        created_at: consensus_time,
        updated_at: consensus_time,
    };
    index.insert(task_id.clone());

    // a create writes TWO records; check both BEFORE staging either, so a
    // refusal leaves the overlay untouched (never an index entry naming a task
    // whose record was refused).
    let record = sdk::wire::encode(&task);
    let index_record = sdk::wire::encode(&index);
    check_record(&record, "task record")?;
    check_record(&index_record, "task index")?;
    staged.stage(record_key(&task_id), record);
    staged.stage(INDEX_KEY.to_vec(), index_record);
    stage_owner_count(staged, &owner, owner_live + 1);
    Ok(())
}

/// move a task's status. any member's op lands: the owner is attribution,
/// not a consent the update needs.
async fn update_status(
    staged: &mut StagedStore,
    task_id: String,
    status: TaskStatus,
    consensus_time: u64,
) -> Result<(), Error> {
    sdk::validate_id("task_id", &task_id, MAX_TASK_ID)?;
    let mut task = load(staged, &task_id)
        .await?
        .ok_or_else(|| Error::Module(format!("task not found: {task_id}")))?;
    // an accepted no-op: it stages NOTHING, so the block's root holds.
    if task.status == status {
        return Ok(());
    }

    task.status = status;
    task.updated_at = consensus_time;
    stage_record(
        staged,
        record_key(&task_id),
        sdk::wire::encode(&task),
        "task record",
    )
}

/// remove a task's record and free its board slot -- the only way the index
/// (and [`MAX_TASKS`]) ever recedes. any member's op lands, the job board's
/// `prune` shape; the slot freed is the record owner's, whoever deletes.
async fn delete(staged: &mut StagedStore, task_id: String) -> Result<(), Error> {
    sdk::validate_id("task_id", &task_id, MAX_TASK_ID)?;
    let task = load(staged, &task_id)
        .await?
        .ok_or_else(|| Error::Module(format!("task not found: {task_id}")))?;

    let owner_live = owner_count(staged, &task.owner).await?;
    let remaining = owner_live
        .checked_sub(1)
        .ok_or_else(|| Error::Module("task census underflow".into()))?;
    let mut index = load_index(staged).await?;
    index.remove(&task_id);
    if index.is_empty() {
        staged.delete(INDEX_KEY.to_vec());
    } else {
        let index_record = sdk::wire::encode(&index);
        check_record(&index_record, "task index")?;
        staged.stage(INDEX_KEY.to_vec(), index_record);
    }
    staged.delete(record_key(&task_id));

    stage_owner_count(staged, &task.owner, remaining);
    Ok(())
}

pub(crate) async fn execute(
    staged: &mut StagedStore,
    actor: &Party,
    msg: TaskMsg,
    consensus_time: u64,
) -> Result<(), Error> {
    match msg {
        TaskMsg::CreateTask {
            task_id,
            title,
            owner,
        } => create(staged, task_id, title, owner, actor, consensus_time).await,
        TaskMsg::UpdateStatus { task_id, status } => {
            update_status(staged, task_id, status, consensus_time).await
        }
        TaskMsg::DeleteTask { task_id } => delete(staged, task_id).await,
    }
}

/// the board's two reads, both through the staged overlay (this block's staged
/// creates and status changes are visible -- see the module docblock).
pub(crate) async fn query(staged: &StagedStore, q: TaskQuery) -> Result<TaskReply, Error> {
    match q {
        TaskQuery::Get { task_id } => get(staged, task_id).await,
        TaskQuery::List { limit, after } => list(staged, limit, after).await,
        TaskQuery::OwnerOpenCount { owner } => Ok(TaskReply::OwnerOpenCount(
            owner_count(staged, &owner).await?,
        )),
    }
}

/// ONE task by id — the point read a consensus caller's execute path uses. an
/// absent id is `None`, never an error: "does this id exist" is the question.
async fn get(staged: &StagedStore, task_id: String) -> Result<TaskReply, Error> {
    Ok(TaskReply::Task(load(staged, &task_id).await?))
}

/// ONE page of the board in ascending id order: at most `limit` tasks (clamped
/// into `1..=MAX_LIST_LIMIT`) whose ids sort strictly after `after`. page by
/// handing the last returned id back as the next `after`; a short page ends the
/// board.
async fn list(staged: &StagedStore, limit: u64, after: Option<String>) -> Result<TaskReply, Error> {
    let limit = limit.clamp(1, MAX_LIST_LIMIT) as usize;
    let index = load_index(staged).await?;
    // the cursor is EXCLUSIVE, so a page that ends on an id resumes past it.
    let start = match after.as_deref() {
        Some(cursor) => Bound::Excluded(cursor),
        None => Bound::Unbounded,
    };
    let mut tasks = Vec::with_capacity(limit);
    for task_id in index.range::<str, _>((start, Bound::Unbounded)).take(limit) {
        let task = load(staged, task_id)
            .await?
            .ok_or_else(|| Error::Module(format!("task index names a missing task: {task_id}")))?;
        tasks.push(task);
    }
    Ok(TaskReply::Tasks(tasks))
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use sdk_testkit::MemStore;

    use super::*;

    fn staged() -> StagedStore {
        StagedStore::new(Box::new(MemStore::new()))
    }

    fn alice() -> Party {
        Party::Account(1)
    }

    fn mallory() -> Party {
        Party::Account(2)
    }

    async fn create_as(
        staged: &mut StagedStore,
        actor: &Party,
        task_id: &str,
        title: &str,
    ) -> Result<(), Error> {
        create(staged, task_id.into(), title.into(), None, actor, 1).await
    }

    /// a board AT the cap refuses the next create BY NAME. the full index is
    /// staged directly rather than built by [`MAX_TASKS`] creates: the guard
    /// reads the index, so seeding it is the same input at one encode's cost.
    #[test]
    fn create_refuses_a_full_board() {
        block_on(async {
            let mut staged = staged();
            let index: BTreeSet<String> = (0..MAX_TASKS).map(|n| format!("t{n:06}")).collect();
            staged.stage(INDEX_KEY.to_vec(), sdk::wire::encode(&index));

            let refused = create_as(&mut staged, &alice(), "one-more", "over")
                .await
                .expect_err("a full board refuses");
            assert!(
                refused.to_string().contains("task board full"),
                "the refusal names the cap: {refused}"
            );
            // the refusal staged NOTHING: the would-be record is absent.
            assert!(load(&staged, "one-more").await.unwrap().is_none());
        });
    }

    /// the page walks the whole board in ascending id order, `limit` at a time,
    /// resuming EXCLUSIVELY past the id the caller last saw.
    #[test]
    fn list_pages_the_board_by_cursor() {
        block_on(async {
            let mut staged = staged();
            for id in ["a", "b", "c"] {
                create_as(&mut staged, &alice(), id, &format!("title {id}"))
                    .await
                    .expect("create");
            }

            let TaskReply::Tasks(first) = list(&staged, 2, None).await.unwrap() else {
                panic!("a list answers a page");
            };
            assert_eq!(
                first.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
                ["a", "b"]
            );

            let cursor = first.last().map(|t| t.id.clone());
            let TaskReply::Tasks(second) = list(&staged, 2, cursor).await.unwrap() else {
                panic!("a list answers a page");
            };
            assert_eq!(
                second.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
                ["c"]
            );

            // a limit over the clamp is clamped, never refused.
            let TaskReply::Tasks(all) = list(&staged, u64::MAX, None).await.unwrap() else {
                panic!("a list answers a page");
            };
            assert_eq!(all.len(), 3);
        });
    }

    /// a create records the actor from the origin as the task's owner.
    #[test]
    fn create_records_the_owner_from_origin() {
        block_on(async {
            let mut staged = staged();
            create_as(&mut staged, &alice(), "t1", "ship it")
                .await
                .expect("create");
            let task = load(&staged, "t1").await.unwrap().expect("task exists");
            assert_eq!(task.owner, alice());
        });
    }

    /// any member's `UpdateStatus` lands on any task, and the owner stays
    /// the creator: ownership is attribution, not a consent the update needs.
    #[test]
    fn any_member_updates_any_tasks_status() {
        block_on(async {
            let mut staged = staged();
            create_as(&mut staged, &alice(), "t1", "ship it")
                .await
                .expect("create");

            update_status(&mut staged, "t1".into(), TaskStatus::Done, 2)
                .await
                .expect("a stranger's update lands on another owner's task");
            let task = load(&staged, "t1").await.unwrap().unwrap();
            assert_eq!(task.status, TaskStatus::Done);
            assert_eq!(task.owner, alice(), "the owner is attribution and stays");
            assert_eq!(task.updated_at, 2);
        });
    }

    /// any member's delete frees the index slot AND decrements the RECORD
    /// owner's live count, so a board at the per-owner cap admits the owner's
    /// create right after a stranger's delete.
    #[test]
    fn any_member_deletes_any_task_and_frees_its_owners_slot() {
        block_on(async {
            let mut staged = staged();
            create_as(&mut staged, &alice(), "t1", "ship it")
                .await
                .expect("create");

            delete(&mut staged, "t1".into())
                .await
                .expect("a stranger's delete lands on another owner's task");
            assert!(load(&staged, "t1").await.unwrap().is_none());
            assert!(
                load_index(&staged).await.unwrap().is_empty(),
                "the index slot is freed"
            );
            assert_eq!(
                owner_count(&staged, &alice()).await.unwrap(),
                0,
                "the owner census recedes"
            );

            // the freed slot is usable again by the same owner.
            create_as(&mut staged, &alice(), "t1", "ship it again")
                .await
                .expect("recreate after delete");
        });
    }

    /// a per-owner cap refuses the (N+1)th create while another owner is
    /// still admitted -- no single account can fill the shared board.
    #[test]
    fn per_owner_cap_refuses_the_next_create_for_that_owner_only() {
        block_on(async {
            let mut staged = staged();
            for n in 0..MAX_OPEN_TASKS_PER_OWNER {
                create_as(&mut staged, &alice(), &format!("a{n}"), "mine")
                    .await
                    .expect("under the per-owner cap");
            }

            let refused = create_as(&mut staged, &alice(), "a-over", "one too many")
                .await
                .expect_err("alice is at her per-owner cap");
            assert!(
                refused.to_string().contains("task owner at cap"),
                "unexpected error: {refused}"
            );

            // a different owner is unaffected.
            create_as(&mut staged, &mallory(), "m1", "not alice's problem")
                .await
                .expect("another owner is still admitted");
        });
    }

    #[test]
    fn module_origin_may_override_the_owner() {
        block_on(async {
            let mut staged = staged();
            create(
                &mut staged,
                "t1".into(),
                "ship it".into(),
                Some(1),
                &Party::Module("automations".into()),
                1,
            )
            .await
            .expect("a module may vouch for another owner");
            let task = load(&staged, "t1").await.unwrap().expect("task exists");
            assert_eq!(task.owner, alice(), "the override wins");
        });
    }

    /// any origin may name any account as the owner: the override is
    /// attribution and the census key, not a consent the named account gives.
    #[test]
    fn any_origin_may_name_another_account_as_owner() {
        block_on(async {
            let mut staged = staged();
            create(
                &mut staged,
                "t1".into(),
                "ship it".into(),
                Some(1),
                &mallory(),
                1,
            )
            .await
            .expect("mallory names alice as the owner");
            let task = load(&staged, "t1").await.unwrap().expect("task exists");
            assert_eq!(task.owner, alice(), "the named account owns the task");
            assert_eq!(
                owner_count(&staged, &alice()).await.unwrap(),
                1,
                "the census counts the named owner, not the actor"
            );
            assert_eq!(owner_count(&staged, &mallory()).await.unwrap(), 0);

            // naming ITSELF is a no-op, accepted the same as `None`.
            create(
                &mut staged,
                "t2".into(),
                "ship it too".into(),
                Some(2),
                &mallory(),
                1,
            )
            .await
            .expect("naming yourself is always allowed");
            let task = load(&staged, "t2").await.unwrap().expect("task exists");
            assert_eq!(task.owner, mallory());
        });
    }
    #[test]
    fn corrupt_owner_census_refuses_delete_before_any_staging() {
        block_on(async {
            let mut staged = staged();
            create_as(&mut staged, &alice(), "task", "Work")
                .await
                .unwrap();
            staged.stage(owner_count_key(&alice()), vec![0]);
            staged.commit().await.unwrap();
            let before = staged.root();
            assert!(delete(&mut staged, "task".into()).await.is_err());
            assert!(staged.is_empty());
            staged.commit().await.unwrap();
            assert_eq!(staged.root(), before);
            assert!(load(&staged, "task").await.unwrap().is_some());
        });
    }
}
