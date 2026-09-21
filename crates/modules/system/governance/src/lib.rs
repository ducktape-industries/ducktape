use abi::{Cause, Env, ItemRef, Outcome, Refusal, Scheme};
use guest::Program;
use modules::governance::{
    Action, Allocation, Ballot, Effect, Electorate, Grant, INVITE_NAMESPACE, Invite, Op, Proposal,
    Query, Redemption, Reply, Rule, Shares, Status, Voter,
};
use modules::program::{bytes_key, conflict, invalid, not_found, u64_key, unauthorized};
use modules::{AccountNumber, acl, identity, module_registry, valset};

const PROPOSAL: &str = "p/";
const REDEMPTION: &str = "r/";
const EFFECT: &str = "i/";
const SHARES: &[u8] = b"shares";

struct Governance;

fn proposal_key(id: &str) -> Vec<u8> {
    format!("{PROPOSAL}{id}").into_bytes()
}

fn redemption_key(nonce: &[u8]) -> Vec<u8> {
    bytes_key(REDEMPTION, nonce)
}

fn effect_key(item: u64) -> Vec<u8> {
    u64_key(EFFECT, item)
}

impl Program for Governance {
    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        match &env.cause {
            Cause::Direct | Cause::Delivery(_) => direct(&env, payload),
            Cause::Completion { item, outcome } => completed(item, outcome),
        }
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Proposal { id } => Reply::Proposal(guest::record(proposal_key(&id))?),
            Query::Proposals { page } => Reply::Proposals(
                guest::records::<Proposal>(page.scan(PROPOSAL.as_bytes()))?
                    .into_iter()
                    .map(|(_, proposal)| proposal)
                    .collect(),
            ),
            Query::Shares => Reply::Shares(shares()?),
            Query::Redemption { nonce } => {
                Reply::Redemption(guest::record(redemption_key(&nonce))?)
            }
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn direct(env: &Env, payload: &[u8]) -> Result<(), Refusal> {
    acl::admit(env)?;
    let signer = modules::program::external(env)?;
    match abi::decode(payload)? {
        Op::Propose {
            id,
            action,
            voting_blocks,
        } => propose(env, signer, id, action, voting_blocks),
        Op::Vote { id, approve } => vote(env, signer, &id, approve),
        Op::Execute { id } => execute(env, &id),
        Op::Redeem { invite, address } => redeem(env, signer, invite, address),
    }
}

fn shares() -> Result<Shares, Refusal> {
    Ok(guest::record(SHARES)?.unwrap_or(Shares {
        enabled: false,
        allocations: Vec::new(),
    }))
}

fn proposal(id: &str) -> Result<Proposal, Refusal> {
    guest::record(proposal_key(id))?.ok_or_else(|| not_found(format!("proposal {id}")))
}

fn store(proposal: &Proposal) {
    guest::put(proposal_key(&proposal.id), proposal);
}

fn validators() -> Result<Vec<Vec<u8>>, Refusal> {
    match guest::ask(valset::PROGRAM, &valset::Query::Validators)? {
        valset::Reply::Validators(validators) => Ok(validators),
        other => Err(Refusal::new(
            modules::reason::PROTOCOL,
            format!("valset answered Validators with {other:?}"),
        )),
    }
}

fn electorate(shares: &Shares) -> Result<(Electorate, Vec<Voter>), Refusal> {
    if shares.enabled {
        let voters = shares
            .allocations
            .iter()
            .map(|allocation| Voter {
                principal: identity::principal(allocation.account),
                power: allocation.shares,
            })
            .collect();
        return Ok((Electorate::Shareholders, voters));
    }
    let voters = validators()?
        .into_iter()
        .map(|principal| Voter {
            principal,
            power: 1,
        })
        .collect();
    Ok((Electorate::Validators, voters))
}

fn principal_of(electorate: Electorate, signer: &[u8]) -> Result<Vec<u8>, Refusal> {
    match electorate {
        Electorate::Validators => Ok(signer.to_vec()),
        Electorate::Shareholders => {
            let account = identity::account_of(signer)?
                .ok_or_else(|| unauthorized("this key holds no account"))?;
            Ok(identity::principal(account))
        }
    }
}

fn rule(electorate: Electorate, action: &Action, total: u64) -> Rule {
    let simple_majority = Rule::Threshold {
        required_yes: total / 2 + 1,
    };
    let two_thirds = Rule::Threshold {
        required_yes: total - total / 3,
    };
    let half_participate = Rule::ParticipatingMajority {
        quorum: total.div_ceil(2),
    };
    match (electorate, action) {
        (Electorate::Validators, _) => simple_majority,
        (Electorate::Shareholders, Action::Signal { .. }) => half_participate,
        (Electorate::Shareholders, _) => two_thirds,
    }
}

fn admissible(shares: &Shares, action: &Action) -> Result<(), Refusal> {
    match action {
        Action::SetMembership(membership) => {
            let promotes = membership.standing == valset::Standing::Validator;
            let met = valset::standing(&membership.key)?.is_some();
            if promotes && !met {
                return Err(conflict(
                    "a validator is promoted out of the resident tier; grant standing first",
                ));
            }
            Ok(())
        }
        Action::RemoveMember { .. } | Action::Signal { .. } | Action::CancelProgram { .. } => {
            Ok(())
        }
        Action::AdoptShares { allocations } => {
            let adopted = !shares.allocations.is_empty();
            if adopted {
                return Err(conflict("shares are adopted once"));
            }
            if allocations.is_empty() {
                return Err(invalid("an adoption names at least one shareholder"));
            }
            let mut seen = std::collections::BTreeSet::new();
            for allocation in allocations {
                let distinct = seen.insert(allocation.account);
                if !distinct {
                    return Err(invalid(format!(
                        "account {} is allocated twice",
                        allocation.account
                    )));
                }
                if allocation.shares == 0 {
                    return Err(invalid(format!(
                        "account {} holds no shares",
                        allocation.account
                    )));
                }
                if identity::account(allocation.account)?.is_none() {
                    return Err(not_found(format!("account {}", allocation.account)));
                }
            }
            Ok(())
        }
        Action::SetShares { account, .. } => {
            let adopted = !shares.allocations.is_empty();
            if !adopted {
                return Err(conflict("shares are not adopted"));
            }
            if identity::account(*account)?.is_none() {
                return Err(not_found(format!("account {account}")));
            }
            Ok(())
        }
        Action::SetShareMode { .. } => {
            let adopted = !shares.allocations.is_empty();
            if !adopted {
                return Err(conflict("shares are not adopted"));
            }
            Ok(())
        }
        Action::ScheduleProgram { lead, change } => {
            if *lead == 0 {
                return Err(invalid(
                    "a program change lands at least one block after it is scheduled",
                ));
            }
            if let module_registry::Change::Set(entry) = change {
                let published = guest::blob_stat(entry.code).is_some();
                if !published {
                    return Err(not_found(format!("code {:?} is not published", entry.code)));
                }
            }
            Ok(())
        }
        Action::SetPolicy { target, standing } => {
            let governs_governance = target == modules::governance::PROGRAM || target == acl::ANY;
            if !governs_governance {
                return Ok(());
            }
            let electorate_still_submits = match standing {
                None | Some(acl::Standing::Open) => true,
                Some(acl::Standing::User) => shares.enabled,
                Some(acl::Standing::Node) | Some(acl::Standing::Validator) => !shares.enabled,
            };
            if !electorate_still_submits {
                return Err(conflict(
                    "this policy would lock the electorate out of governance",
                ));
            }
            Ok(())
        }
    }
}

fn propose(
    env: &Env,
    signer: Vec<u8>,
    id: String,
    action: Action,
    voting_blocks: u64,
) -> Result<(), Refusal> {
    let taken = guest::get(proposal_key(&id)).is_some();
    if taken {
        return Err(conflict(format!("proposal {id} exists")));
    }
    let shares = shares()?;
    admissible(&shares, &action)?;
    let (electorate, voters) = electorate(&shares)?;
    let proposer = principal_of(electorate, &signer)?;
    let enfranchised = voters.iter().any(|voter| voter.principal == proposer);
    if !enfranchised {
        return Err(unauthorized("the proposer is not in the electorate"));
    }
    let total = voters.iter().map(|voter| voter.power).sum();
    store(&Proposal {
        rule: rule(electorate, &action, total),
        id,
        action,
        proposer,
        opened_at: env.height,
        deadline: env.height + voting_blocks,
        electorate,
        voters,
        ballots: Vec::new(),
        status: Status::Open,
    });
    Ok(())
}

fn vote(env: &Env, signer: Vec<u8>, id: &str, approve: bool) -> Result<(), Refusal> {
    let mut proposal = proposal(id)?;
    if proposal.status != Status::Open {
        return Err(Refusal::new(
            modules::reason::CLOSED,
            format!("proposal {id} is settled"),
        ));
    }
    let closed = env.height >= proposal.deadline;
    if closed {
        return Err(Refusal::new(
            modules::reason::CLOSED,
            format!("voting on {id} has closed"),
        ));
    }
    let principal = principal_of(proposal.electorate, &signer)?;
    let enfranchised = proposal
        .voters
        .iter()
        .any(|voter| voter.principal == principal);
    if !enfranchised {
        return Err(unauthorized("the voter is not in the electorate"));
    }
    proposal
        .ballots
        .retain(|ballot| ballot.principal != principal);
    proposal.ballots.push(Ballot { principal, approve });
    store(&proposal);
    Ok(())
}

struct Tally {
    yes: u64,
    no: u64,
    total: u64,
}

fn tally(proposal: &Proposal) -> Tally {
    let mut tally = Tally {
        yes: 0,
        no: 0,
        total: proposal.voters.iter().map(|voter| voter.power).sum(),
    };
    for ballot in &proposal.ballots {
        let power = proposal
            .voters
            .iter()
            .find(|voter| voter.principal == ballot.principal)
            .map_or(0, |voter| voter.power);
        match ballot.approve {
            true => tally.yes += power,
            false => tally.no += power,
        }
    }
    tally
}

enum Verdict {
    Passes,
    Fails,
    Undecided,
}

fn verdict(proposal: &Proposal, height: u64) -> Verdict {
    let tally = tally(proposal);
    let deadline_reached = height >= proposal.deadline;
    match proposal.rule {
        Rule::Threshold { required_yes } => {
            let passes = tally.yes >= required_yes;
            let cannot_pass = tally.total - tally.no < required_yes;
            if passes {
                return Verdict::Passes;
            }
            if cannot_pass || deadline_reached {
                return Verdict::Fails;
            }
            Verdict::Undecided
        }
        Rule::ParticipatingMajority { quorum } => {
            let participation = tally.yes + tally.no;
            let quorate = participation >= quorum;
            let irreversible = quorate && tally.yes > tally.total - tally.yes;
            if irreversible {
                return Verdict::Passes;
            }
            if !deadline_reached {
                return Verdict::Undecided;
            }
            match quorate && tally.yes > tally.no {
                true => Verdict::Passes,
                false => Verdict::Fails,
            }
        }
    }
}

fn execute(env: &Env, id: &str) -> Result<(), Refusal> {
    let mut proposal = proposal(id)?;
    if proposal.status != Status::Open {
        return Err(Refusal::new(
            modules::reason::CLOSED,
            format!("proposal {id} is settled"),
        ));
    }
    proposal.status = match verdict(&proposal, env.height) {
        Verdict::Undecided => {
            return Err(Refusal::new(
                modules::reason::CLOSED,
                format!(
                    "proposal {id} is not decidable before block {}",
                    proposal.deadline
                ),
            ));
        }
        Verdict::Fails => Status::Rejected,
        Verdict::Passes => Status::Passed {
            effect: Some(perform(env, &proposal.action)?),
        },
    };
    store(&proposal);
    if let Status::Passed {
        effect: Some(Effect::Pending(item)),
    } = &proposal.status
    {
        guest::put(effect_key(item.item), &proposal.id);
    }
    Ok(())
}

fn perform(env: &Env, action: &Action) -> Result<Effect, Refusal> {
    let effect = match action {
        Action::SetMembership(membership) => {
            ask_for(valset::PROGRAM, &valset::Op::Set(membership.clone()))
        }
        Action::RemoveMember { key } => {
            ask_for(valset::PROGRAM, &valset::Op::Remove { key: key.clone() })
        }
        Action::Signal { .. } => Effect::Applied,
        Action::AdoptShares { allocations } => {
            guest::put(
                SHARES,
                &Shares {
                    enabled: true,
                    allocations: allocations.clone(),
                },
            );
            Effect::Applied
        }
        Action::SetShares { account, shares } => {
            set_shares(*account, *shares)?;
            Effect::Applied
        }
        Action::SetShareMode { enabled } => {
            let mut shares = shares()?;
            shares.enabled = *enabled;
            guest::put(SHARES, &shares);
            Effect::Applied
        }
        Action::ScheduleProgram { lead, change } => ask_for(
            module_registry::PROGRAM,
            &module_registry::Op::Schedule(module_registry::Scheduled {
                height: env.height + 1 + lead,
                change: change.clone(),
            }),
        ),
        Action::CancelProgram { height, program } => ask_for(
            module_registry::PROGRAM,
            &module_registry::Op::Cancel {
                height: *height,
                program: program.clone(),
            },
        ),
        Action::SetPolicy { target, standing } => ask_for(
            acl::PROGRAM,
            &acl::Op::SetPolicy {
                target: target.clone(),
                standing: *standing,
            },
        ),
    };
    Ok(effect)
}

fn ask_for<T: borsh::BorshSerialize>(program: &str, op: &T) -> Effect {
    Effect::Pending(guest::call(program, abi::encode(op)))
}

fn set_shares(account: AccountNumber, held: u64) -> Result<(), Refusal> {
    let mut shares = shares()?;
    shares
        .allocations
        .retain(|allocation| allocation.account != account);
    if held > 0 {
        shares.allocations.push(Allocation {
            account,
            shares: held,
        });
        shares
            .allocations
            .sort_by_key(|allocation| allocation.account);
    }
    guest::put(SHARES, &shares);
    Ok(())
}

fn completed(item: &ItemRef, outcome: &Outcome) -> Result<(), Refusal> {
    let id: String = guest::record(effect_key(item.item))?
        .ok_or_else(|| not_found(format!("no proposal awaits item {}", item.item)))?;
    let mut proposal = proposal(&id)?;
    let Status::Passed { effect } = &mut proposal.status else {
        return Err(conflict(format!("proposal {id} did not pass")));
    };
    *effect = Some(match outcome {
        Outcome::Applied { .. } => Effect::Applied,
        Outcome::Rejected(refusal) => Effect::Refused(refusal.clone()),
    });
    store(&proposal);
    guest::delete(effect_key(item.item));
    Ok(())
}

fn redeem(env: &Env, joiner: Vec<u8>, invite: Invite, address: String) -> Result<(), Refusal> {
    let issuer_is_a_member = valset::standing(&invite.issuer)?.is_some();
    if !issuer_is_a_member {
        return Err(unauthorized("the issuer is not a member"));
    }
    let expired = env.time > invite.expires_at;
    if expired {
        return Err(unauthorized("the invite has expired"));
    }
    let spent = guest::get(redemption_key(&invite.nonce)).is_some();
    if spent {
        return Err(conflict("the invite was redeemed"));
    }
    let grant = Grant {
        network: env.network.clone(),
        nonce: invite.nonce.clone(),
        expires_at: invite.expires_at,
    };
    let granted = guest::verify(
        Scheme::Ed25519,
        invite.issuer.clone(),
        INVITE_NAMESPACE,
        grant.preimage(),
        invite.signature,
    )?;
    if !granted {
        return Err(unauthorized("the invite does not verify"));
    }
    guest::emit(
        valset::PROGRAM,
        abi::encode(&valset::Op::Set(valset::Membership {
            key: joiner.clone(),
            address,
            standing: valset::Standing::Resident,
        })),
    );
    guest::put(
        redemption_key(&invite.nonce),
        &Redemption {
            nonce: invite.nonce,
            issuer: invite.issuer,
            joiner,
            height: env.height,
        },
    );
    Ok(())
}

guest::program!(Governance);
