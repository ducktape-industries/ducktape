//! Programs queue immutable forge deployments; module policy drives node work.
use super::*;
use crate::action_requests::Prepared;
use node_work::relative_path;
use crate::catalog::Operation;
use crate::facets::{RunnerResult, WireSink};

const HEAD: &str = "module_updates/head";
const TAIL: &str = "module_updates/tail";

fn record_key(sequence: u64) -> String {
    format!("module_updates/record/{sequence}")
}

fn status_key(sequence: u64) -> String {
    format!("module_updates/status/{sequence}")
}

fn request_key(request_id: &str) -> String {
    format!("module_updates/request/{request_id}")
}


impl RunsModule {
    async fn update_cursor(&self, key: &str) -> Result<u64, Error> {
        self.receipts
            .get(key)
            .await?
            .map(|bytes| sdk::wire::decode(&bytes).map_err(Error::Module))
            .transpose()
            .map(|value| value.unwrap_or(0))
    }

    pub(super) async fn module_update(
        &self,
        sequence: u64,
    ) -> Result<Option<ModuleUpdateView>, Error> {
        let Some(bytes) = self.receipts.get(&record_key(sequence)).await? else {
            return Ok(None);
        };
        let request = sdk::wire::decode(&bytes).map_err(Error::Module)?;
        let status = self
            .receipts
            .get(&status_key(sequence))
            .await?
            .ok_or_else(|| Error::Module("module update status is missing".into()))?;
        let status = sdk::wire::decode(&status).map_err(Error::Module)?;
        Ok(Some(ModuleUpdateView { request, status }))
    }

    pub(super) async fn next_module_update(&self) -> Result<Option<ModuleUpdateView>, Error> {
        self.module_update(self.update_cursor(HEAD).await?).await
    }

    pub(super) fn emit_module_updates(
        &self,
        ctx: &mut dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        result: &RunnerResult,
        operations: &[Operation],
    ) {
        for (index, operation) in operations.iter().enumerate() {
            let Operation::ModulesUpdate(update) = operation else {
                continue;
            };
            let WireSink::Pr {
                repo,
                source_branch,
                ..
            } = &entry.sink
            else {
                self.note(
                    ctx,
                    format!("run {run_id}: module update requires a forge output"),
                );
                continue;
            };
            let receipt = &result.workspace_receipt;
            let committed_output = result.sink.same_commitment(&entry.sink)
                && receipt.branch.as_ref() == Some(source_branch)
                && receipt.commit_error.is_none();
            let Some(commit) = receipt.output_commit.as_ref().filter(|_| committed_output) else {
                self.note(
                    ctx,
                    format!("run {run_id}: module update has no committed forge output"),
                );
                continue;
            };
            let prepared = Prepared::new(
                Msg {
                    target: self.id.clone(),
                    payload: encode_msg(&RunsMsg::RequestModuleUpdate {
                        request_id: format!("{}/{index}", dispatch_id_for(run_id)),
                        run_id: run_id.into(),
                        source: ModuleUpdateSource {
                            repo: repo.clone(),
                            branch: source_branch.clone(),
                            commit: commit.clone(),
                        },
                        update: update.clone(),
                    }),
                },
                operation.name(),
                serde_json::json!({
                    "module_id": update.module_id,
                    "code_hash": update.code_hash,
                }),
            );
            self.emit_prepared(ctx, prepared);
        }
    }

    pub(super) async fn request_module_update(
        &mut self,
        ctx: &mut dyn Ctx,
        request_id: String,
        run_id: String,
        source: ModuleUpdateSource,
        update: ModuleUpdateSpec,
    ) -> Result<(), Error> {
        let Origin::Program(account) = ctx.env().origin else {
            return Err(Error::Module(
                "module updates are requested by a program account".into(),
            ));
        };
        self.active_generation(ctx, account).await?;
        update.validate().map_err(Error::Module)?;
        let valid_source = relative_path(&source.repo)
            && !source.branch.is_empty()
            && source.commit.len() == 40
            && source.commit.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !valid_source {
            return Err(Error::Module(
                "module update requires an exact forge commit".into(),
            ));
        }
        if let Some(bytes) = self.receipts.get(&request_key(&request_id)).await? {
            let sequence = sdk::wire::decode(&bytes).map_err(Error::Module)?;
            let previous = self
                .module_update(sequence)
                .await?
                .ok_or_else(|| Error::Module("module update receipt is missing".into()))?;
            let exact = previous.request.account == account
                && previous.request.run_id == run_id
                && previous.request.source == source
                && previous.request.update == update;
            if !exact {
                return Err(Error::Module(
                    "module update id already names different work".into(),
                ));
            }
            ctx.set_output(sdk::wire::encode(&previous));
            return Ok(());
        }
        let sequence = self.update_cursor(TAIL).await?;
        let next = sequence
            .checked_add(1)
            .ok_or_else(|| Error::Module("module update sequence exhausted".into()))?;
        let view = ModuleUpdateView {
            request: ModuleUpdateRequest {
                sequence,
                request_id,
                account,
                run_id,
                source,
                update,
            },
            status: ModuleUpdateStatus::Requested,
        };
        self.receipts
            .stage(record_key(sequence), sdk::wire::encode(&view.request))?;
        self.receipts
            .stage(status_key(sequence), sdk::wire::encode(&view.status))?;
        self.receipts.stage(
            request_key(&view.request.request_id),
            sdk::wire::encode(&sequence),
        )?;
        self.receipts.stage(TAIL.into(), sdk::wire::encode(&next))?;
        ctx.set_output(sdk::wire::encode(&view));
        Ok(())
    }

    async fn update_proposal(
        &self,
        ctx: &dyn Ctx,
        sequence: u64,
    ) -> Result<Option<governance::ProposalView>, Error> {
        let bytes = ctx
            .query(
                "governance",
                &governance::encode_query(&governance::GovQuery::Proposal {
                    proposal_id: module_update_proposal_id(sequence),
                }),
            )
            .await?;
        let governance::GovReply::Proposal(proposal) =
            governance::decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module(
                "unexpected module update proposal reply".into(),
            ));
        };
        Ok(proposal)
    }

    fn finish_module_update(&mut self, view: &ModuleUpdateView) -> Result<(), Error> {
        self.receipts.stage(
            status_key(view.request.sequence),
            sdk::wire::encode(&view.status),
        )?;
        self.receipts
            .stage(HEAD.into(), sdk::wire::encode(&(view.request.sequence + 1)))
    }

    pub(super) async fn reconcile_module_update(
        &mut self,
        ctx: &mut dyn Ctx,
        sequence: u64,
    ) -> Result<(), Error> {
        let Some(mut view) = self.next_module_update().await? else {
            return Ok(());
        };
        if view.request.sequence != sequence {
            return Ok(());
        }
        let bytes = ctx
            .query(
                "modules",
                &modules::encode_query(&modules::ModulesQuery::ModuleStatus),
            )
            .await?;
        let modules::ModulesReply::ModuleStatus { modules } =
            modules::decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("unexpected module registry reply".into()));
        };
        let digest = view.request.update.digest().map_err(Error::Module)?;
        let activated = modules.iter().any(|module| {
            module.module_id == view.request.update.module_id
                && module.active_code_hash == digest
                && module.pending.is_none()
        });
        if activated {
            view.status = ModuleUpdateStatus::Activated {
                height: ctx.env().height,
            };
            return self.finish_module_update(&view);
        }
        let Some(proposal) = self.update_proposal(ctx, sequence).await? else {
            return Ok(());
        };
        let expected = governance::GovAction::UpdateModule {
            name: module_update_proposal_id(sequence),
            module_id: view.request.update.module_id.clone(),
            activation_lead: view.request.update.after,
            code_hash: digest.to_vec(),
        };
        if proposal.action != expected {
            view.status = ModuleUpdateStatus::Rejected {
                reason: "deployment proposal names different work".into(),
            };
            return self.finish_module_update(&view);
        }
        if proposal.status == governance::ProposalStatus::Rejected {
            view.status = ModuleUpdateStatus::Rejected {
                reason: "governance rejected the module update".into(),
            };
            return self.finish_module_update(&view);
        }
        let registry_did_not_schedule = proposal.status == governance::ProposalStatus::Passed
            && !modules.iter().any(|module| {
                module.module_id == view.request.update.module_id
                    && module
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.code_hash == digest)
            });
        if registry_did_not_schedule {
            view.status = ModuleUpdateStatus::Rejected {
                reason: "the registry did not schedule the deployment".into(),
            };
            return self.finish_module_update(&view);
        }
        let expired = modules.iter().any(|module| {
            module.module_id == view.request.update.module_id
                && module.pending.as_ref().is_some_and(|pending| {
                    pending.code_hash == digest && pending.stale_at(ctx.env().height)
                })
        });
        if expired {
            view.status = ModuleUpdateStatus::Rejected {
                reason: "deployment expired before every validator was ready".into(),
            };
            return self.finish_module_update(&view);
        }
        Ok(())
    }

    pub(super) async fn refuse_module_update(
        &mut self,
        ctx: &mut dyn Ctx,
        sequence: u64,
        reason: String,
    ) -> Result<(), Error> {
        let Origin::External(key) = &ctx.env().origin else {
            return Err(Error::Module(
                "only a validator may report a deployment failure".into(),
            ));
        };
        let members = valset::members(ctx, "valset").await?;
        if !members.contains(key) {
            return Err(Error::Module(
                "deployment failure reporter is not a validator".into(),
            ));
        }
        let Some(mut view) = self.next_module_update().await? else {
            return Ok(());
        };
        if view.request.sequence != sequence {
            return Ok(());
        }
        if self.update_proposal(ctx, sequence).await?.is_some() {
            return Err(Error::Module(
                "a proposed deployment settles through governance".into(),
            ));
        }
        view.status = ModuleUpdateStatus::Rejected { reason };
        self.finish_module_update(&view)
    }
}
