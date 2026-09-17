//! Model configuration belongs to runs; identity remains the account authority.
//! Any submitter may configure any model: the record confers no authority, so
//! there is nothing a configuration could escalate.
use super::*;
use capability::validate_tag;

/// a skill's `source_prefix` must be a SCOPED duckfs subtree, never a
/// namespace root: `resolve_skills` copies it verbatim into a run's
/// dispatch payload, and the provisioner's checkout runs one full checkout
/// per mount, so an unscoped prefix ("/", "/shared", "/home/<x>") makes every
/// run of the agent check out the whole namespace once per curated skill.
///
/// this is a local, minimal stand-in for `files::paths::canonical` (absolute,
/// `/`-separated, no empty/dot segments, at least 3 segments deep — the same
/// depth `library_skills` requires for `/shared/skills/<name>`): the agent
/// module is self-contained by design (see the crate doc), so it does not
/// depend on another module's crate for this shape check.
fn is_scoped_duckfs_prefix(prefix: &str) -> bool {
    if !prefix.starts_with('/') || prefix.contains('\0') {
        return false;
    }
    let segments: Vec<&str> = prefix.trim_start_matches('/').split('/').collect();
    let all_named = segments
        .iter()
        .all(|s| !s.is_empty() && *s != "." && *s != "..");
    all_named && segments.len() >= 3
}

// ---- the module -----------------------------------------------------------

impl RunsModule {
    /// a recipe hash is empty (unset) or exactly [`RECIPE_HASH_LEN`] bytes.
    fn validate_recipe_hash(recipe_hash: &[u8]) -> Result<(), Error> {
        if !recipe_hash.is_empty() && recipe_hash.len() != RECIPE_HASH_LEN {
            return Err(Error::Module(format!(
                "recipe_hash must be empty or {RECIPE_HASH_LEN} bytes, got {}",
                recipe_hash.len()
            )));
        }
        Ok(())
    }

    /// a skill ref must carry a name that is [`is_skill_mount_name`] (the
    /// SAME predicate the noded provisioner's `mount_dir_name` calls — one
    /// rule, not two that could drift), unique within the record, and a
    /// source_prefix that is a scoped duckfs subtree ([`is_scoped_duckfs_prefix`]),
    /// also unique within the record. a pinned snapshot, when present, must be
    /// non-empty. order is preserved verbatim (skills are an ordered override
    /// list).
    ///
    /// the COUNT is capped ([`MAX_SKILLS_PER_AGENT`]) for the same reason the
    /// record's bytes are: the list is replicated state, and it is also the run's
    /// context budget. curation is the whole point of the tier design — a
    /// 500-skill list is a library, and the library lives in duckfs, not in the
    /// record.
    fn validate_skills(skills: &[SkillRef]) -> Result<(), Error> {
        if skills.len() > MAX_SKILLS_PER_AGENT {
            return Err(Error::Module(format!(
                "an agent may curate at most {MAX_SKILLS_PER_AGENT} skills, got {}; leave the \
                 rest in the shared skill library",
                skills.len()
            )));
        }
        let mut names = BTreeSet::new();
        let mut prefixes = BTreeSet::new();
        for skill in skills {
            if !is_skill_mount_name(&skill.name) {
                return Err(Error::Module(format!(
                    "skill name {:?} is not a safe mount directory name (want \
                     [a-zA-Z0-9._-]+, at most {MAX_SKILL_NAME_BYTES} bytes, not \".\" or \"..\")",
                    skill.name
                )));
            }
            if !names.insert(skill.name.as_str()) {
                return Err(Error::Module(format!(
                    "duplicate skill name {:?}",
                    skill.name
                )));
            }
            if !is_scoped_duckfs_prefix(&skill.source_prefix) {
                return Err(Error::Module(format!(
                    "skill source_prefix {:?} is not a scoped duckfs subtree \
                     (want an absolute path at least 3 segments deep, e.g. \
                     /shared/skills/<name>)",
                    skill.source_prefix
                )));
            }
            if !prefixes.insert(skill.source_prefix.as_str()) {
                return Err(Error::Module(format!(
                    "duplicate skill source_prefix {:?}",
                    skill.source_prefix
                )));
            }
            if let Some(snapshot) = &skill.source_snapshot
                && snapshot.is_empty()
            {
                return Err(Error::Module(
                    "skill source_snapshot must not be empty when set".into(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn model(&self, id: &str) -> Option<&ModelRecord> {
        match self.pending_models.get(id) {
            Some(record) => record.as_ref(),
            None => self.models.get(id),
        }
    }

    pub(super) fn model_records(&self) -> Vec<ModelRecord> {
        Self::visible_ids(&self.models, &self.pending_models)
            .into_iter()
            .filter_map(|id| self.model(&id).cloned())
            .collect()
    }

    pub(super) async fn account_control(
        &self,
        ctx: &dyn Ctx,
        account: sdk::AccountNumber,
    ) -> Result<identity::Control, Error> {
        let bytes = ctx
            .query(
                "identity",
                &identity::encode_query(&identity::IdentityQuery::Get { number: account }),
            )
            .await?;
        let identity::IdentityReply::Account(Some(view)) =
            identity::decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("model account does not exist".into()));
        };
        Ok(view.control)
    }

    pub(super) async fn active_generation(
        &self,
        ctx: &dyn Ctx,
        account: u64,
    ) -> Result<u64, Error> {
        let identity::Control::Program {
            executor,
            generation,
            standing: identity::ProgramStanding::Active,
            ..
        } = self.account_control(ctx, account).await?
        else {
            return Err(Error::Module("program authority is not active".into()));
        };
        if executor != self.agent {
            return Err(Error::Module("program executor does not match".into()));
        }
        Ok(generation)
    }

    /// a model serves a live program account executed by this module's agent
    /// module; any other account has no program a run could act through.
    pub(super) async fn program_model(
        &self,
        ctx: &dyn Ctx,
        account: sdk::AccountNumber,
    ) -> Result<(), Error> {
        let identity::Control::Program { executor, .. } =
            self.account_control(ctx, account).await?
        else {
            return Err(Error::Module(
                "model requires a live program account".into(),
            ));
        };
        if executor != self.agent {
            return Err(Error::Module("program executor does not match".into()));
        }
        Ok(())
    }

    fn stage_model(&mut self, record: ModelRecord) -> Result<(), Error> {
        let bytes = sdk::wire::encode(&record);
        if bytes.len() > MAX_AGENT_RECORD_BYTES {
            return Err(Error::Module(format!(
                "model record exceeds {MAX_AGENT_RECORD_BYTES} bytes"
            )));
        }
        self.pending_models
            .insert(record.agent_id.clone(), Some(record));
        Ok(())
    }

    fn registered_model(&self, id: &str) -> Result<ModelRecord, Error> {
        self.model(id)
            .cloned()
            .ok_or_else(|| Error::Module(format!("unknown model: {id}")))
    }

    pub(super) async fn configure_model(
        &mut self,
        ctx: &mut dyn Ctx,
        operation: ModelMsg,
    ) -> Result<(), Error> {
        match operation {
            ModelMsg::RegisterModel {
                account,
                agent_id,
                display_name,
                capability,
                recipe_hash,
                skills,
            } => {
                self.program_model(ctx, account).await?;
                validate_agent_id(&agent_id).map_err(Error::Module)?;
                Self::validate_non_empty("display_name", &display_name)?;
                validate_tag(&capability).map_err(Error::Module)?;
                if self.model(&agent_id).is_some() {
                    return Err(Error::Module(format!("model already exists: {agent_id}")));
                }
                let records = self.model_records();
                if records.len() >= MAX_REGISTERED_AGENTS {
                    return Err(Error::Module("model registry is full".into()));
                }
                let owner = canonical_origin(&ctx.env().origin)?;
                let owned = records
                    .iter()
                    .filter(|record| record.owner == owner)
                    .count();
                if owned >= MAX_AGENTS_PER_OWNER {
                    return Err(Error::Module("model owner allocation is full".into()));
                }
                let recipe_hash = recipe_hash.unwrap_or_default();
                Self::validate_recipe_hash(&recipe_hash)?;
                let skills = skills.unwrap_or_default();
                Self::validate_skills(&skills)?;
                let record = ModelRecord {
                    account,
                    agent_id: agent_id.clone(),
                    owner,
                    display_name,
                    capability: capability.clone(),
                    status: ModelStatus::Active,
                    role: ModelRole::default(),
                    created_at: ctx.env().consensus_time,
                    updated_at: ctx.env().consensus_time,
                    recipe_hash,
                    skills,
                };
                self.stage_model(record)?;
                self.apply_model_change(
                    ctx,
                    ModelChange::Registered {
                        agent_id,
                        capability,
                    },
                )
            }
            ModelMsg::UpdateModel {
                agent_id,
                display_name,
                capability,
                recipe_hash,
                skills,
            } => {
                let mut record = self.registered_model(&agent_id)?;
                if let Some(name) = display_name {
                    Self::validate_non_empty("display_name", &name)?;
                    record.display_name = name;
                }
                if let Some(capability) = capability {
                    validate_tag(&capability).map_err(Error::Module)?;
                    if capability != record.capability {
                        self.apply_model_change(
                            ctx,
                            ModelChange::CapabilityChanged {
                                agent_id: agent_id.clone(),
                                capability: capability.clone(),
                            },
                        )?;
                    }
                    record.capability = capability;
                }
                if let Some(hash) = recipe_hash {
                    Self::validate_recipe_hash(&hash)?;
                    record.recipe_hash = hash;
                }
                if let Some(skills) = skills {
                    Self::validate_skills(&skills)?;
                    record.skills = skills;
                }
                record.updated_at = ctx.env().consensus_time;
                self.stage_model(record)
            }
            ModelMsg::PauseModel { agent_id } => {
                self.set_model_status(ctx, agent_id, ModelStatus::Paused)
                    .await
            }
            ModelMsg::ResumeModel { agent_id } => {
                self.set_model_status(ctx, agent_id, ModelStatus::Active)
                    .await
            }
            ModelMsg::DeregisterModel { agent_id } => {
                self.registered_model(&agent_id)?;
                self.pending_models.insert(agent_id.clone(), None);
                self.apply_model_change(ctx, ModelChange::Deregistered { agent_id })
            }
        }
    }

    async fn set_model_status(
        &mut self,
        ctx: &dyn Ctx,
        id: String,
        status: ModelStatus,
    ) -> Result<(), Error> {
        let mut record = self.registered_model(&id)?;
        record.status = status;
        record.updated_at = ctx.env().consensus_time;
        self.stage_model(record)
    }
}
