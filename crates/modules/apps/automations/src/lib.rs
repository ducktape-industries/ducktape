//! Standing rules over authenticated chat events. Any identity account
//! registers a rule and any account administers one; a rule observes every
//! channel chat hooks this module into and posts wherever it names. Actions
//! publish source-owned attribution, posts or tasks in the originating atomic
//! unit. Structurally refused actions remain visible in run history.
mod interface;
pub use interface::*;

use borsh::{BorshDeserialize, BorshSerialize};
use chat::{
    Block, ChatEvent, ChatMsg, ChatQuery, ChatReply, Party, decode_event as chat_decode_event,
    decode_reply as chat_decode_reply, encode_msg as chat_encode_msg,
    encode_query as chat_encode_query,
};
use sdk::{
    AccountNumber, Ctx, Error, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget,
    StagedStore, StateRoot, StateSyncHandle, require_non_empty,
};
use tasks::{
    TaskMsg, TaskQuery, TaskReply, decode_task_reply as tasks_decode_reply,
    encode_task_msg as tasks_encode_msg, encode_task_query as tasks_encode_query,
};

/// max rules retained NETWORK-WIDE. registering beyond this is rejected at
/// execute. [`MAX_RULES_PER_OWNER`] is the per-account bound that keeps any
/// ONE account from being the reason this global roster ever fills.
pub const MAX_RULES: usize = 1024;
/// max rules one creator may hold at once, the tasks board's
/// [`tasks::MAX_OPEN_TASKS_PER_OWNER`] shape applied to the rule roster: no
/// single account can fill [`MAX_RULES`] and permanently deny the feature to
/// everyone else. an [`AutomationsMsg::DeleteRule`], from any account, frees
/// a slot.
pub const MAX_RULES_PER_OWNER: usize = 32;
/// `rule_id` byte bound (also the `channel_id`/`task_id_prefix` bound).
pub const MAX_ID_BYTES: usize = 256;
/// trigger filter (`mention`, `text_contains`) byte bound.
pub const MAX_FILTER_BYTES: usize = 256;
/// The notification kind budget, retained by source-owned reports.
pub const MAX_REPORT_KIND_BYTES: usize = 64;
/// action template byte bound.
pub const MAX_TEMPLATE_BYTES: usize = 4096;
/// byte bound on a SUBSTITUTED template — the same budget as the template it
/// was rendered from. capping the template alone does not bound the render:
/// substitution AMPLIFIES, and 17 `{text}` tokens on a 64 KiB chat message
/// produce a ~1.09 MiB string, past both tasks' record cap and chat's
/// message-head cap. a follow-up must never be able to fail the op that
/// triggered it, so an over-cap render is TRUNCATED rather than refused:
/// refusing is safe for the block (a `build_and_emit` error is RECORDED, not
/// emitted) but silently stops the rule from firing on every large message
/// forever, and a clipped title or post body is the honest lesser harm.
pub const MAX_SUBSTITUTED_BYTES: usize = MAX_TEMPLATE_BYTES;
/// run-history ring capacity; the oldest record is dropped past this.
pub const MAX_RUN_HISTORY: usize = 1024;
/// actions emitted per incoming event; matching rules past this are recorded as
/// skipped (`action_ok = false`, `detail = "action budget exceeded"`).
pub const MAX_ACTIONS_PER_EVENT: usize = 8;
/// serialized roster-record byte bound, enforced at create. the per-field caps
/// do not bound the roster's SERIALIZED form tightly enough on their own:
/// [`MAX_RULES`] ids of [`MAX_ID_BYTES`] control characters JSON-escape past
/// the qmdb wire codec's decode ceiling — a committed over-cap value would
/// wedge every syncing peer (the poison-value lesson), so the create op
/// refuses loudly instead.
pub const MAX_ROSTER_RECORD_BYTES: usize = 512 * 1024;
async fn identity_account(
    ctx: &dyn Ctx,
    identity: &str,
    query: identity::IdentityQuery,
) -> Result<identity::AccountView, Error> {
    let bytes = ctx.query(identity, &identity::encode_query(&query)).await?;
    let reply = identity::decode_reply(&bytes).map_err(Error::Module)?;
    let identity::IdentityReply::Account(Some(account)) = reply else {
        return Err(Error::Module(
            "automation rules require an identity account".into(),
        ));
    };
    Ok(account)
}

/// the account an admin op is submitted under: the creator a new rule
/// records and the key of the per-account rule quota. any account
/// administers any rule.
async fn submitting_account(ctx: &dyn Ctx, identity: &str) -> Result<AccountNumber, Error> {
    let query = match &ctx.env().origin {
        Origin::External(key) => {
            if key.is_empty() {
                return Err(Error::Module(
                    "external origin must carry a non-empty submitter id".into(),
                ));
            }
            identity::IdentityQuery::OfKey { key: key.clone() }
        }
        Origin::Program(number) => identity::IdentityQuery::Get { number: *number },
        Origin::Module(_) | Origin::System => {
            return Err(Error::Module(
                "automation rules require an account origin".into(),
            ));
        }
    };
    let account = identity_account(ctx, identity, query).await?;
    let names_non_program = matches!(ctx.env().origin, Origin::Program(_))
        && !matches!(account.control, identity::Control::Program { .. });
    if names_non_program {
        return Err(Error::Module(
            "program origin requires a program account".into(),
        ));
    }
    Ok(account.number)
}

/// per-rule record key: prefix + 0 + id (the single-component shape chat
/// uses). safe because every key literal below is fixed and none is another
/// followed by a 0 byte.
fn rule_key(rule_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + 1 + rule_id.len());
    key.extend_from_slice(b"rule");
    key.push(0);
    key.extend_from_slice(rule_id.as_bytes());
    key
}

/// per-run-record key: prefix + 0 + seq (big-endian).
fn run_key(seq: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + 1 + 8);
    key.extend_from_slice(b"run");
    key.push(0);
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn owner_rule_count_key(owner: &AccountNumber) -> Vec<u8> {
    let mut key = Vec::with_capacity(7 + 1 + 8);
    key.extend_from_slice(b"rulecnt");
    key.push(0);
    key.extend_from_slice(&owner.to_be_bytes());
    key
}

/// the roster record's whole key. collides with no `rule\0...`/`run\0...`/
/// `rulecnt\0...` key.
const ROSTER_KEY: &[u8] = b"roster";

/// the run-history cursor record's whole key.
const RUN_CURSOR_KEY: &[u8] = b"runcursor";

/// the run-history ring bounds, maintained by the write path: `head` is the
/// oldest live record's seq, `next` the seq the next append takes (empty ring
/// when equal). consensus consumes it on every append — placing the new
/// record and trimming the ring are decided from this record, never from a
/// store scan — so it stays canonical.
#[derive(Default, BorshSerialize, BorshDeserialize)]
struct RunCursor {
    head: u64,
    next: u64,
}

/// what one chat event has already prepared, carried across the rules it
/// fires. every probe in [`Automations::build_and_emit`] reads
/// staged-or-committed state, and a follow-up this dispatch emitted is in
/// NEITHER until the unit applies — so two rules on one post would each see
/// a free id, or an owner one task under cap, both emit, and the second
/// follow-up's failure at its target would unwind the post that triggered
/// it. A rule that loses a race to an earlier one records a refusal instead,
/// which is the same answer it would have got had the earlier action already
/// landed. Not persisted: it lives exactly as long as the event.
#[derive(Default)]
struct Prepared {
    message_ids: std::collections::BTreeSet<String>,
    task_ids: std::collections::BTreeSet<String>,
    tasks_per_owner: std::collections::BTreeMap<AccountNumber, u64>,
}

pub struct Automations {
    id: ModuleId,
    /// the chat module id — both the trusted hook origin and the `PostMessage`
    /// follow-up target.
    chat: ModuleId,
    /// the tasks module id — the `CreateTask` follow-up target.
    tasks: ModuleId,
    identity: ModuleId,
    attribution: ModuleId,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`). store key
    /// is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl Automations {
    /// wrap the host-constructed store under module identity `id`.
    pub fn new(
        id: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
        chat: impl Into<ModuleId>,
        tasks: impl Into<ModuleId>,
        identity: impl Into<ModuleId>,
        attribution: impl Into<ModuleId>,
    ) -> Self {
        Self {
            id: id.into(),
            chat: chat.into(),
            tasks: tasks.into(),
            identity: identity.into(),
            attribution: attribution.into(),
            staged: StagedStore::new(store),
        }
    }

    // ---- staged-over-committed reads ----------------------------------------

    async fn load<T>(&self, key: &[u8]) -> Result<Option<T>, Error>
    where
        T: BorshDeserialize,
    {
        match self.staged.get(key).await? {
            Some(bytes) => Ok(Some(
                borsh::from_slice(&bytes).map_err(|e| Error::Module(e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// stage a value whose serialized size the execute-time field caps already
    /// bound (rule records, run records, the cursor) — see the module doc's
    /// poison-value paragraph. the roster goes through [`Self::store_bounded`].
    fn store<T>(&mut self, key: Vec<u8>, value: &T)
    where
        T: BorshSerialize,
    {
        self.staged.stage(
            key,
            borsh::to_vec(value).expect("automations value is serializable"),
        );
    }

    /// stage a value only if its serialized size fits `cap` — the write-time
    /// guard against poison values (the qmdb codec cap is decode-only).
    fn store_bounded<T>(
        &mut self,
        key: Vec<u8>,
        value: &T,
        cap: usize,
        what: &str,
    ) -> Result<(), Error>
    where
        T: BorshSerialize,
    {
        let bytes = borsh::to_vec(value).expect("automations value is serializable");
        if bytes.len() > cap {
            return Err(Error::Module(format!(
                "{what} record too large: {} > {cap} bytes",
                bytes.len()
            )));
        }
        self.staged.stage(key, bytes);
        Ok(())
    }

    async fn rule(&self, rule_id: &str) -> Result<Option<Rule>, Error> {
        self.load(&rule_key(rule_id)).await
    }

    /// the rule roster — every registered id, sorted. record and roster are
    /// staged (and commit or abort) together, so membership in one is
    /// membership in both; the roster is the ONE existence authority at create.
    async fn roster(&self) -> Result<Vec<String>, Error> {
        Ok(self.load(ROSTER_KEY).await?.unwrap_or_default())
    }

    /// one owner's live rule count, read through the staged overlay — what
    /// [`MAX_RULES_PER_OWNER`] is checked against. absent reads as zero, the
    /// tasks board's `owner_count` shape.
    async fn owner_rule_count(&self, owner: &AccountNumber) -> Result<u64, Error> {
        let Some(bytes) = self.staged.get(&owner_rule_count_key(owner)).await? else {
            return Ok(0);
        };
        let raw: [u8; 8] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::Module("owner rule census record is not a u64".into()))?;
        Ok(u64::from_le_bytes(raw))
    }

    /// stage an owner's rule census. a zero count DELETES the key, so an
    /// owner with no rules left hashes the same as one who never created any
    /// (the tasks board's `stage_owner_count` rule).
    fn stage_owner_rule_count(&mut self, owner: &AccountNumber, count: u64) {
        let key = owner_rule_count_key(owner);
        if count == 0 {
            self.staged.delete(key);
            return;
        }
        self.staged.stage(key, count.to_le_bytes().to_vec());
    }

    /// every rule, in roster (rule-id) order — the ONE enumeration read:
    /// consensus itself consumes it (each hook event evaluates each rule), so
    /// it stays canonical, bounded by [`MAX_RULES`]. a rostered id without a
    /// record is a store bug — loud, never skipped.
    async fn all_rules(&self) -> Result<Vec<Rule>, Error> {
        let mut rules = Vec::new();
        for rule_id in self.roster().await? {
            let Some(rule) = self.rule(&rule_id).await? else {
                return Err(Error::Module(format!("missing rule record: {rule_id}")));
            };
            rules.push(rule);
        }
        Ok(rules)
    }

    // ---- validation ---------------------------------------------------------

    fn validate_len(field: &str, value: &str, max: usize) -> Result<(), Error> {
        if value.len() > max {
            return Err(Error::Module(format!(
                "{field} exceeds {max} bytes ({} given)",
                value.len()
            )));
        }
        Ok(())
    }

    fn validate_trigger(trigger: &Trigger) -> Result<(), Error> {
        if let Some(channel_id) = &trigger.channel_id {
            require_non_empty("trigger channel_id", channel_id)?;
            Self::validate_len("trigger channel_id", channel_id, MAX_ID_BYTES)?;
        }
        if let Some(mention) = &trigger.mention {
            Self::validate_len("trigger mention", mention, MAX_FILTER_BYTES)?;
        }
        if let Some(text_contains) = &trigger.text_contains {
            Self::validate_len("trigger text_contains", text_contains, MAX_FILTER_BYTES)?;
        }
        Ok(())
    }

    fn validate_action(action: &Action) -> Result<(), Error> {
        match action {
            Action::PostMessage {
                channel_id,
                template,
            } => {
                require_non_empty("action channel_id", channel_id)?;
                Self::validate_len("action channel_id", channel_id, MAX_ID_BYTES)?;
                Self::validate_len("action template", template, MAX_TEMPLATE_BYTES)?;
            }
            Action::CreateTask {
                task_id_prefix,
                title_template,
            } => {
                require_non_empty("action task_id_prefix", task_id_prefix)?;
                Self::validate_len("action task_id_prefix", task_id_prefix, MAX_ID_BYTES)?;
                Self::validate_len("action title_template", title_template, MAX_TEMPLATE_BYTES)?;
            }
            Action::Report {
                recipient: _,
                kind,
                body_template,
            } => {
                require_non_empty("action kind", kind)?;
                Self::validate_len("action kind", kind, MAX_REPORT_KIND_BYTES)?;
                Self::validate_len("action body_template", body_template, MAX_TEMPLATE_BYTES)?;
            }
        }
        Ok(())
    }

    // ---- admin ops ----------------------------------------------------------

    async fn stage_create_rule(
        &mut self,
        owner: AccountNumber,
        rule_id: String,
        trigger: Trigger,
        action: Action,
        consensus_time: u64,
    ) -> Result<(), Error> {
        require_non_empty("rule_id", &rule_id)?;
        Self::validate_len("rule_id", &rule_id, MAX_ID_BYTES)?;
        Self::validate_trigger(&trigger)?;
        Self::validate_action(&action)?;
        let mut roster = self.roster().await?;
        let position = match roster.binary_search(&rule_id) {
            Ok(_) => {
                return Err(Error::Module(format!("rule already exists: {rule_id}")));
            }
            Err(position) => position,
        };
        if roster.len() >= MAX_RULES {
            return Err(Error::Module(format!("rule cap reached ({MAX_RULES})")));
        }
        let owner_rules = self.owner_rule_count(&owner).await?;
        if owner_rules >= MAX_RULES_PER_OWNER as u64 {
            return Err(Error::Module(format!(
                "rule owner at cap: {MAX_RULES_PER_OWNER} rules"
            )));
        }
        roster.insert(position, rule_id.clone());
        // the roster's byte gate first: a refusal must stage NOTHING.
        self.store_bounded(
            ROSTER_KEY.to_vec(),
            &roster,
            MAX_ROSTER_RECORD_BYTES,
            "roster",
        )?;
        self.store(
            rule_key(&rule_id),
            &Rule {
                rule_id: rule_id.clone(),
                owner,
                enabled: true,
                trigger,
                action,
                created_at: consensus_time,
                fire_count: 0,
            },
        );
        self.stage_owner_rule_count(&owner, owner_rules + 1);
        Ok(())
    }

    async fn stage_set_enabled(&mut self, rule_id: String, enabled: bool) -> Result<(), Error> {
        require_non_empty("rule_id", &rule_id)?;
        let Some(mut rule) = self.rule(&rule_id).await? else {
            return Err(Error::Module(format!("unknown rule: {rule_id}")));
        };
        let unchanged = rule.enabled == enabled;
        if unchanged {
            // idempotent: staging nothing keeps the op log — and the root —
            // byte-identical to no write at all.
            return Ok(());
        }
        rule.enabled = enabled;
        self.store(rule_key(&rule_id), &rule);
        Ok(())
    }

    async fn stage_delete_rule(&mut self, rule_id: String) -> Result<(), Error> {
        require_non_empty("rule_id", &rule_id)?;
        let mut roster = self.roster().await?;
        let Ok(position) = roster.binary_search(&rule_id) else {
            return Err(Error::Module(format!("unknown rule: {rule_id}")));
        };
        // the roster is the existence authority; the RECORD carries the
        // creator whose quota the delete frees. a rostered id without a record
        // is a store bug — loud, as everywhere.
        let Some(rule) = self.rule(&rule_id).await? else {
            return Err(Error::Module(format!("missing rule record: {rule_id}")));
        };
        let owner_rules = self.owner_rule_count(&rule.owner).await?;
        roster.remove(position);
        self.staged.delete(rule_key(&rule_id));
        // shrinking keeps the roster under its create-time byte gate.
        self.store(ROSTER_KEY.to_vec(), &roster);
        self.stage_owner_rule_count(&rule.owner, owner_rules.saturating_sub(1));
        Ok(())
    }

    // ---- the chat hook intake (NO-FAIL arm) ---------------------------------

    async fn on_chat_event(&mut self, ctx: &mut dyn Ctx, payload: &[u8]) -> Result<(), Error> {
        let Ok(event) = chat_decode_event(payload) else {
            // an undecodable event must not abort the posting block.
            return Ok(());
        };
        let ChatEvent::MessagePosted {
            channel_id,
            seq,
            thread_root: _,
            author,
            mentions,
        } = event;

        // Automatic posts from this rules module do not recursively fire its
        // standing rules. Other accounts and modules remain eligible.
        if author == Party::Module(self.id.clone()) {
            return Ok(());
        }
        let height = ctx.env().height;
        let rules = self.all_rules().await?;

        // a rule observes every channel chat hooks this module into: a
        // wildcard trigger is "every hooked channel", and only the trigger's
        // own filters decide a match.
        let candidates: Vec<&Rule> = rules
            .iter()
            .filter(|rule| {
                rule.enabled && Self::matches_channel_and_mention(rule, &channel_id, &mentions)
            })
            .collect();

        // fetch the post's text ONCE, and only if some matching rule needs it
        // (a `text_contains` filter, or a `{text}` placeholder). `None` = the
        // fetch FAILED (query error / message absent), which is distinct from a
        // legitimately empty body (`Some("")`): rules that need text record a
        // failure on `None` instead of silently matching against emptiness.
        let needs_text = candidates.iter().copied().any(Self::rule_wants_text);
        let text: Option<String> = if needs_text {
            self.fetch_text(&*ctx, &channel_id, seq).await
        } else {
            Some(String::new())
        };
        let author_actor = actor_of(&author);
        let mention_actor = mentions
            .first()
            .map(|account| format!("acct:{account}"))
            .unwrap_or_default();

        // evaluate in deterministic rule_id order (roster order).
        let cursor: RunCursor = self.load(RUN_CURSOR_KEY).await?.unwrap_or_default();
        cursor
            .next
            .checked_add(candidates.len() as u64)
            .ok_or_else(|| Error::Module("run history sequence exhausted".into()))?;
        let mut budget = 0usize;
        let mut fired: Vec<Rule> = Vec::new();
        let mut records: Vec<RunRecord> = Vec::new();
        let mut prepared = Prepared::default();
        for rule in candidates {
            let record = |action_ok: bool, detail: String| RunRecord {
                rule_id: rule.rule_id.clone(),
                channel_id: channel_id.clone(),
                seq,
                height,
                action_ok,
                detail,
            };
            // a rule that needs text cannot be evaluated (or substituted) when
            // the fetch failed: a recorded failure, never empty-text guessing.
            let text = match (&text, Self::rule_wants_text(rule)) {
                (Some(text), _) => text.as_str(),
                (None, false) => "",
                (None, true) => {
                    records.push(record(false, "text fetch failed".into()));
                    continue;
                }
            };
            if let Some(want) = &rule.trigger.text_contains
                && !text.contains(want.as_str())
            {
                continue;
            }
            if budget >= MAX_ACTIONS_PER_EVENT {
                records.push(record(false, "action budget exceeded".into()));
                continue;
            }
            let vars = TemplateVars {
                channel: &channel_id,
                seq: Some(seq),
                author: &author_actor,
                text,
                mention: &mention_actor,
            };
            match self
                .build_and_emit(
                    ctx,
                    rule,
                    cursor.next + records.len() as u64,
                    &vars,
                    &mut prepared,
                )
                .await
            {
                Ok(detail) => {
                    budget += 1;
                    let mut updated = rule.clone();
                    updated.fire_count = updated.fire_count.saturating_add(1);
                    fired.push(updated);
                    records.push(record(true, detail));
                }
                Err(detail) => {
                    // a structurally impossible or probe-rejected action is
                    // recorded, not emitted, and never bumps fire_count or
                    // consumes the action budget.
                    records.push(record(false, detail));
                }
            }
        }
        for updated in fired {
            self.store(rule_key(&updated.rule_id), &updated);
        }
        self.append_history(cursor, records);
        Ok(())
    }

    /// append this event's run records and trim the ring to
    /// [`MAX_RUN_HISTORY`]: place each record at `next`, then point-delete
    /// from `head` — every decision reads the cursor, never a store scan.
    fn append_history(&mut self, mut cursor: RunCursor, records: Vec<RunRecord>) {
        if records.is_empty() {
            return;
        }
        for record in records {
            self.store(run_key(cursor.next), &record);
            cursor.next += 1;
        }
        while cursor.next - cursor.head > MAX_RUN_HISTORY as u64 {
            self.staged.delete(run_key(cursor.head));
            cursor.head += 1;
        }
        self.store(RUN_CURSOR_KEY.to_vec(), &cursor);
    }

    /// build the action for a firing rule, PROBE its target, and emit it as a
    /// follow-up. returns the success `detail` on emit, or an error `detail`
    /// when the action is structurally impossible or a probe rejects it
    /// (recorded, not a block failure).
    ///
    /// `prepared` is what THIS event has already emitted. The probes read
    /// staged-or-committed state, which a follow-up emitted earlier in this
    /// same dispatch has not reached yet — so without it a second rule sees
    /// an id that is about to be taken, or an owner that is about to be at
    /// cap, emits anyway, and its failure at the target unwinds the post
    /// that triggered it.
    ///
    /// the probe layer (the no-fail-arm pattern applied to follow-ups):
    /// every structurally-KNOWABLE follow-up failure is checked here via
    /// host-routed queries against the target's staged-or-committed state —
    /// deterministic on every validator — so a missing channel, a squatted
    /// deterministic id, or a task-id collision downgrades to a RunRecord
    /// instead of aborting the posting user's block. what this event has
    /// already emitted is checked against [`Prepared`], since the target's
    /// state does not carry it yet; a post-probe follow-up failure from any
    /// other cause still aborts the block by P2 design.
    async fn build_and_emit(
        &self,
        ctx: &mut dyn Ctx,
        rule: &Rule,
        run_seq: u64,
        vars: &TemplateVars<'_>,
        prepared: &mut Prepared,
    ) -> Result<String, String> {
        let event_channel = vars.channel;
        let Some(seq) = vars.seq else {
            return Err("chat event has no sequence".into());
        };
        match &rule.action {
            Action::PostMessage {
                channel_id,
                template,
            } => {
                let body = substitute_bounded(template, vars);
                if body.is_empty() {
                    return Err("post template produced an empty message".into());
                }
                // deterministic, collision-free per (rule, message).
                let message_id = format!("auto-{}-{}-{}", rule.rule_id, event_channel, seq);
                // composed-id guard BEFORE the probes: event channel ids are
                // unbounded, so the composition can exceed this module's id cap.
                if message_id.len() > MAX_ID_BYTES {
                    return Err("composed id exceeds cap".into());
                }
                // probe 1: the target channel must exist — chat would reject
                // the post and abort the block otherwise.
                let req = chat_encode_query(&ChatQuery::Channel {
                    channel_id: channel_id.clone(),
                });
                match ctx.query(&self.chat, &req).await {
                    Err(e) => return Err(format!("chat probe failed: {e}")),
                    Ok(bytes) => match chat_decode_reply(&bytes) {
                        Ok(ChatReply::Channel(Some(_))) => {}
                        Ok(ChatReply::Channel(None)) => {
                            return Err(format!("target channel does not exist: {channel_id}"));
                        }
                        _ => return Err("chat probe returned an unexpected reply".into()),
                    },
                }
                // probe 2: the deterministic message id must be unused — ids
                // are caller-supplied at chat, so a user could pre-post the
                // composed id to wedge this rule's next fire (id squatting).
                let req = chat_encode_query(&ChatQuery::Message {
                    message_id: message_id.clone(),
                });
                match ctx.query(&self.chat, &req).await {
                    Err(e) => return Err(format!("chat probe failed: {e}")),
                    Ok(bytes) => match chat_decode_reply(&bytes) {
                        Ok(ChatReply::Message(None)) => {}
                        Ok(ChatReply::Message(Some(_))) => {
                            return Err(format!("message id already taken: {message_id}"));
                        }
                        _ => return Err("chat probe returned an unexpected reply".into()),
                    },
                }
                // probe 3: no rule earlier in THIS event already composed the
                // same id — chat has not seen that post yet, so probe 2 cannot.
                if !prepared.message_ids.insert(message_id.clone()) {
                    return Err(format!("message id already prepared: {message_id}"));
                }
                ctx.emit_msg(Msg {
                    target: self.chat.clone(),
                    payload: chat_encode_msg(&ChatMsg::PostMessage {
                        channel_id: channel_id.clone(),
                        message_id,
                        blocks: vec![Block::paragraph(body)],
                        thread: None,
                    }),
                });
                Ok(format!("posted to {channel_id}"))
            }
            Action::CreateTask {
                task_id_prefix,
                title_template,
            } => {
                let title = substitute_bounded(title_template, vars);
                if title.is_empty() {
                    return Err("task template produced an empty title".into());
                }
                // deterministic, collision-free per (rule, message) — the rule
                // id is IN the composition for the same reason a posted
                // message's is: two rules are free to carry the same prefix,
                // and an id that left them out would have them both compose
                // one task, the second create failing at tasks and unwinding
                // the triggering post.
                let task_id = format!("{task_id_prefix}-{}-{event_channel}-{seq}", rule.rule_id);
                // composed-id guard BEFORE the probe (see PostMessage), held
                // against TASKS' rule rather than this module's: an id tasks
                // rejects at apply (over MAX_TASK_ID, or carrying the reserved
                // KEY_SEP a rule author is free to put in the prefix) unwinds
                // the triggering post. MAX_ID_BYTES happens to equal
                // MAX_TASK_ID today; that coincidence is not the constraint.
                sdk::validate_id("task_id", &task_id, tasks::MAX_TASK_ID)
                    .map_err(|e| e.to_string())?;
                // probe: the composed task id must be unused — tasks rejects
                // duplicates, which would abort the block. ONE by-id read: the
                // board walk this replaced cost a store read per task, so a
                // matching chat post on a large board blew the wasm host's
                // per-dispatch read budget and failed every rule from then on.
                // the read is overlay-aware on the tasks side, so a task staged
                // earlier in THIS block is seen (that visibility is why the
                // probe works at all).
                let req = tasks_encode_query(&TaskQuery::Get {
                    task_id: task_id.clone(),
                });
                match ctx.query(&self.tasks, &req).await {
                    Err(e) => return Err(format!("tasks probe failed: {e}")),
                    Ok(bytes) => match tasks_decode_reply(&bytes) {
                        Ok(TaskReply::Task(Some(_))) => {
                            return Err(format!("task id already exists: {task_id}"));
                        }
                        Ok(TaskReply::Task(None)) => {}
                        Ok(TaskReply::Tasks(_) | TaskReply::OwnerOpenCount(_)) => {
                            return Err("tasks answered a page, not a task".into());
                        }
                        Err(_) => return Err("tasks probe returned an unexpected reply".into()),
                    },
                }
                // probe: no rule earlier in THIS event already composed the
                // same id — tasks has not applied that create yet, so the
                // by-id read above cannot see it.
                if !prepared.task_ids.insert(task_id.clone()) {
                    return Err(format!("task id already prepared: {task_id}"));
                }
                // probe: the RULE OWNER's own open-task census must be under
                // cap — the task is created under the owner, not this
                // module's identity (see the created task's `owner` below),
                // so a full owner refuses the RULE's action here, never the
                // triggering post's block.
                let owner_actor = tasks::Party::Account(rule.owner);
                let req = tasks_encode_query(&TaskQuery::OwnerOpenCount {
                    owner: owner_actor.clone(),
                });
                match ctx.query(&self.tasks, &req).await {
                    Err(e) => return Err(format!("tasks probe failed: {e}")),
                    Ok(bytes) => match tasks_decode_reply(&bytes) {
                        Ok(TaskReply::OwnerOpenCount(count)) => {
                            // the census is staged-or-committed state, which
                            // this event's own creates have not reached: an
                            // owner one task under cap would otherwise admit
                            // every rule that fires on this post.
                            let reserved =
                                prepared.tasks_per_owner.get(&rule.owner).copied().unwrap_or(0);
                            if count + reserved >= tasks::MAX_OPEN_TASKS_PER_OWNER as u64 {
                                return Err(format!(
                                    "rule owner at task cap: {} open tasks",
                                    tasks::MAX_OPEN_TASKS_PER_OWNER
                                ));
                            }
                        }
                        _ => return Err("tasks probe returned an unexpected reply".into()),
                    },
                }
                *prepared.tasks_per_owner.entry(rule.owner).or_insert(0) += 1;
                ctx.emit_msg(Msg {
                    target: self.tasks.clone(),
                    payload: tasks_encode_msg(&TaskMsg::CreateTask {
                        task_id: task_id.clone(),
                        title,
                        owner: Some(rule.owner),
                    }),
                });
                Ok(format!("created task {task_id}"))
            }
            Action::Report {
                recipient,
                kind,
                body_template,
            } => {
                let body = substitute_bounded(body_template, vars);
                let detail = sdk::wire::encode(
                    &serde_json::json!({ "rule_id": rule.rule_id, "channel_id": event_channel, "seq": seq, "kind": kind, "body": body }),
                );
                ctx.emit_msg(Msg {
                    target: self.attribution.clone(),
                    payload: attribution::encode_msg(&attribution::AttributionMsg::Attribute {
                        object: attribution::ObjectRef {
                            kind: "report".into(),
                            object: run_seq.to_string(),
                        },
                        revision: 1,
                        actor: attribution::Actor::Account(rule.owner),
                        relations: vec![attribution::Relation {
                            recipient: *recipient,
                            reason: attribution::Reason::Report,
                            detail,
                        }],
                        transfers: Vec::new(),
                    }),
                });
                Ok(format!("reported {kind} to account {recipient}"))
            }
        }
    }

    /// single-message fetch by sequence. `Some(text)` is the message's
    /// concatenated text blocks (possibly legitimately empty); `None` means the
    /// FETCH failed — a query error, an undecodable reply, or an absent
    /// message — which callers record instead of treating as empty text.
    async fn fetch_text(&self, ctx: &dyn Ctx, channel_id: &str, seq: u64) -> Option<String> {
        let req = chat_encode_query(&ChatQuery::MessagesRange {
            channel_id: channel_id.to_string(),
            from_seq: seq,
            limit: 1,
        });
        let bytes = ctx.query(&self.chat, &req).await.ok()?;
        let Ok(ChatReply::Messages(views)) = chat_decode_reply(&bytes) else {
            return None;
        };
        views
            .into_iter()
            .find(|view| view.seq == seq)
            .map(|view| blocks_text(&view.head.blocks))
    }

    fn matches_channel_and_mention(
        rule: &Rule,
        channel_id: &str,
        mentions: &[AccountNumber],
    ) -> bool {
        if let Some(want) = &rule.trigger.channel_id
            && want != channel_id
        {
            return false;
        }
        if let Some(want) = &rule.trigger.mention
            && !mentions
                .iter()
                .any(|account| format!("acct:{account}").contains(want.as_str()))
        {
            return false;
        }
        true
    }

    fn rule_wants_text(rule: &Rule) -> bool {
        if rule.trigger.text_contains.is_some() {
            return true;
        }
        match &rule.action {
            Action::PostMessage { template, .. } => template.contains("{text}"),
            Action::CreateTask { title_template, .. } => title_template.contains("{text}"),
            Action::Report { body_template, .. } => body_template.contains("{text}"),
        }
    }
}

// ---- the one author rendering: the ACTOR-STRING domain -----------------------

struct TemplateVars<'a> {
    channel: &'a str,
    seq: Option<u64>,
    author: &'a str,
    text: &'a str,
    mention: &'a str,
}

fn actor_of(author: &Party) -> String {
    match author {
        Party::Account(account) => format!("acct:{account}"),
        Party::Key(key) => Origin::External(key.clone()).actor_string(),
        Party::Module(module) => Origin::Module(module.clone()).actor_string(),
        Party::System => Origin::System.actor_string(),
    }
}

/// concatenate a message's text blocks: spans within paragraph/quote blocks are
/// joined directly, code blocks contribute their text, dividers nothing, and
/// blocks are joined by newlines. deterministic.
fn blocks_text(blocks: &[Block]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            Block::Paragraph(spans) | Block::Quote(spans) => {
                parts.push(spans.iter().map(|span| span.text.as_str()).collect());
            }
            Block::Code { text, .. } => parts.push(text.clone()),
            Block::Divider => {}
        }
    }
    parts.join("\n")
}

/// single-pass placeholder substitution (deterministic, no regex). the known
/// placeholders are replaced from the triggering event; any other `{...}` token
/// is left literal. a single pass means substituted values are never re-scanned
/// for placeholders.
#[cfg(test)]
fn substitute(template: &str, channel: &str, seq: u64, author: &str, text: &str) -> String {
    let vars = TemplateVars {
        channel,
        seq: Some(seq),
        author,
        text,
        mention: "",
    };
    substitute_vars(template, &vars)
}

fn substitute_bounded(template: &str, vars: &TemplateVars<'_>) -> String {
    let mut rendered = substitute_vars(template, vars);
    if rendered.len() <= MAX_SUBSTITUTED_BYTES {
        return rendered;
    }
    let mut keep = MAX_SUBSTITUTED_BYTES;
    while !rendered.is_char_boundary(keep) {
        keep -= 1;
    }
    rendered.truncate(keep);
    rendered
}

fn substitute_vars(template: &str, vars: &TemplateVars<'_>) -> String {
    let seq_str;
    let seq = match vars.seq {
        Some(seq) => {
            seq_str = seq.to_string();
            seq_str.as_str()
        }
        None => "",
    };
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let Some(close) = rest.find('}') else {
            // no closing brace: the remainder is literal.
            break;
        };
        let replacement = match &rest[..=close] {
            "{channel}" => vars.channel,
            "{seq}" => seq,
            "{author}" => vars.author,
            "{text}" => vars.text,
            "{mention}" => vars.mention,
            _ => {
                // unknown token: emit the '{' literally, rescan after it.
                out.push('{');
                rest = &rest[1..];
                continue;
            }
        };
        out.push_str(replacement);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

#[async_trait::async_trait(?Send)]
impl Module for Automations {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the store's merkle root over all committed records, verbatim — the
    /// staged overlay is invisible here until `commit_block`.
    fn root(&self) -> StateRoot {
        self.staged.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }

    /// the network state-sync serve lane: answers the shared qmdb wire requests
    /// (historical proof-carrying op ranges) from committed state. read-only;
    /// the joiner's sync engine merkle-verifies every batch.
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        // route by the HOST-ASSIGNED origin (spoof-proof): only chat's own
        // follow-ups reach the hook arm; everything else is an admin op. the
        // hook lane returns HERE, before the submitter is resolved — a rule
        // RUNS under a module origin and is CREATED under a submitter's.
        let origin = ctx.env().origin.clone();
        let is_chat_hook = origin == Origin::Module(self.chat.clone());
        if is_chat_hook {
            return self.on_chat_event(ctx, &msg.payload).await;
        }
        // every admin op is submitted under an identity account, resolved ONCE
        // before the payload is decoded: a create records it as the rule's
        // creator and keys the per-account quota on it.
        let submitter = submitting_account(ctx, &self.identity).await?;
        match decode_msg(&msg.payload).map_err(Error::Module)? {
            AutomationsMsg::CreateRule {
                rule_id,
                trigger,
                action,
            } => {
                let consensus_time = ctx.env().consensus_time;
                self.stage_create_rule(submitter, rule_id, trigger, action, consensus_time)
                    .await
            }
            AutomationsMsg::SetEnabled { rule_id, enabled } => {
                self.stage_set_enabled(rule_id, enabled).await
            }
            AutomationsMsg::DeleteRule { rule_id } => self.stage_delete_rule(rule_id).await,
            AutomationsMsg::HookEvent(_) => Err(Error::Module(
                "hook events must originate from the chat module".into(),
            )),
        }
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(Error::Module)? {
            AutomationsQuery::ListRules => Ok(encode_reply(&AutomationsReply::Rules(
                self.all_rules().await?,
            ))),
            AutomationsQuery::GetRule { rule_id } => Ok(encode_reply(&AutomationsReply::Rule(
                self.rule(&rule_id).await?,
            ))),
            AutomationsQuery::RunHistory { rule_id, limit } => {
                let limit = usize::try_from(limit)
                    .unwrap_or(usize::MAX)
                    .min(MAX_RUN_HISTORY);
                // walk the ring by derived key between the cursor's bounds
                // (≤ MAX_RUN_HISTORY point reads). a seq inside the bounds
                // without a record is a store bug — loud, never skipped.
                let cursor: RunCursor = self.load(RUN_CURSOR_KEY).await?.unwrap_or_default();
                let mut matched: Vec<RunRecord> = Vec::new();
                for seq in cursor.head..cursor.next {
                    let Some(record) = self.load::<RunRecord>(&run_key(seq)).await? else {
                        return Err(Error::Module(format!("missing run record: {seq}")));
                    };
                    if record.rule_id == rule_id {
                        matched.push(record);
                    }
                }
                if matched.len() > limit {
                    matched = matched.split_off(matched.len() - limit);
                }
                Ok(encode_reply(&AutomationsReply::History(matched)))
            }
        }
    }

    /// publish the block's staged writes in ONE store batch. no-op (and no
    /// root movement) if nothing was staged.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests;

// the wasm-guest port: the store-backed dispatch shell that adapts this module
// to the ducktape:module world. compiled only by the guest-builder's
// synthesized wasm32 cdylib workspace (feature `guest`), never by the native
// build.
#[cfg(feature = "guest")]
mod guest;
