// What a host's reads see (`Source`), over its live store or over a `Snapshot`: committed state
// pinned at one height with the programs and speculative layer of that moment. The node's RPC
// and its own readers read a snapshot while the host applies the next block.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::sync::Arc;

use abi::role::validators;
use abi::{BlobId, Origin, ProgramId, Receipt, Refusal, Roles, Root, Scan, reason};
use blob::{Blobs, Stage};
use commonware_runtime::Spawner;
use commonware_storage::Context;
use sha2::{Digest as _, Sha256};
use state::{Committed, Overlay, Pinned, View};

use crate::reads::{ReadSet, Recorder};
use crate::unit::{self, Loaded, Runs, World};
use crate::{
    Accounts, BLOBS, Error, Layer, NETWORK, Preconfirmed, RECENT, Result, Submission, Tip, bound,
    corrupt, fresh, member_cap_of, namespace, programs_of, runnable, sync_code,
};

pub struct Source<'a, E>
where
    E: Context + Spawner,
{
    pub(crate) committed: Committed<'a, E>,
    pub(crate) blobs: &'a Blobs,
    pub(crate) loaded: &'a Loaded,
    pub(crate) network: &'a [u8],
    pub(crate) roles: &'a Roles,
    pub(crate) preconfirmed: Option<&'a Preconfirmed>,
    pub(crate) rows_from: u64,
}

impl<'a, E> Source<'a, E>
where
    E: Context + Spawner,
{
    pub fn height(&self) -> Result<u64> {
        self.committed.reads().height()?.ok_or(Error::Unfounded)
    }

    pub fn next_height(&self) -> Result<u64> {
        Ok(self
            .committed
            .reads()
            .height()?
            .map_or(0, |height| height + 1))
    }

    pub fn tip(&self) -> Result<Tip> {
        let height = self.height()?;
        let bytes = self
            .committed
            .view(Vec::new())
            .get(RECENT, &namespace::height(height))?
            .ok_or_else(|| Error::Corrupt("the network records no tip".into()))?;
        let id = abi::decode(&bytes).map_err(corrupt)?;
        Ok(Tip { height, id })
    }

    pub fn epoch_length(&self) -> Result<u64> {
        let bytes = self
            .committed
            .view(Vec::new())
            .get(NETWORK, namespace::EPOCH_LENGTH)?
            .ok_or_else(|| Error::Corrupt("the network records no epoch length".into()))?;
        abi::decode(&bytes).map_err(corrupt)
    }

    pub fn member_cap(&self) -> Result<u32> {
        member_cap_of(&self.committed.view(Vec::new()))
    }

    pub fn epoch_members(&self, epoch: u64) -> Result<Option<Vec<validators::Member>>> {
        self.committed
            .view(Vec::new())
            .get(NETWORK, &namespace::epoch(epoch))?
            .map(|bytes| abi::decode(&bytes).map_err(corrupt))
            .transpose()
    }

    pub fn epoch_validators(&self, epoch: u64) -> Result<Option<Vec<Vec<u8>>>> {
        self.committed
            .view(Vec::new())
            .get(NETWORK, &namespace::epoch_validators(epoch))?
            .map(|bytes| abi::decode(&bytes).map_err(corrupt))
            .transpose()
    }

    pub fn roles(&self) -> &'a Roles {
        self.roles
    }

    pub fn role(&self, role: &str) -> Result<&'a ProgramId> {
        bound(self.roles, role)
    }

    pub fn root(&self) -> Result<Root> {
        let roots = self.committed.roots()?;
        Ok(Root(Sha256::digest(abi::encode(&roots)).into()))
    }

    pub fn view(&self, layer: Layer) -> View<'a> {
        self.committed.view(self.layers(layer))
    }

    pub fn fresh(&self, submission: &Submission) -> Result<std::result::Result<(), Refusal>> {
        fresh(
            &self.view(Layer::Confirmed),
            submission,
            &submission.id(),
            self.next_height()?,
        )
    }

    fn layers(&self, layer: Layer) -> Vec<&'a Overlay> {
        match layer {
            Layer::Confirmed => Vec::new(),
            Layer::Preconfirmed => self
                .preconfirmed
                .map(|preconfirmed| &preconfirmed.overlay)
                .into_iter()
                .collect(),
        }
    }

    fn height_of(&self, layer: Layer) -> Result<u64> {
        match layer {
            Layer::Confirmed => self.height(),
            Layer::Preconfirmed => self.next_height(),
        }
    }

    pub fn programs(&self) -> Result<BTreeMap<ProgramId, BlobId>> {
        programs_of(&self.committed.view(Vec::new()))
    }

    pub fn missing_blobs(&self) -> Result<Vec<BlobId>> {
        let view = self.committed.view(Vec::new());
        let mut named = BTreeSet::new();
        for entry in view.scan(BLOBS, &Scan::prefix(b""))? {
            if namespace::is_index(&entry.key) {
                continue;
            }
            named.insert(abi::decode::<BlobId>(&entry.key).map_err(corrupt)?);
        }
        named.extend(programs_of(&view)?.into_values());
        Ok(named.into_iter().filter(|id| !self.blobs.has(id)).collect())
    }

    pub async fn readable(&self, id: &BlobId, time: u64) -> Result<bool> {
        let height = self.height()?;
        self.readable_in(id, height, time, &Overlay::default(), &Stage::default())
            .await
    }

    pub async fn unnamed(&self, ids: &BTreeSet<BlobId>, time: u64) -> Result<Vec<BlobId>> {
        let running: BTreeSet<BlobId> = self.programs()?.into_values().collect();
        let mut unnamed = Vec::new();
        for id in ids {
            let named = running.contains(id) || self.readable(id, time).await?;
            if !named {
                unnamed.push(*id);
            }
        }
        Ok(unnamed)
    }

    pub(crate) async fn readable_in(
        &self,
        id: &BlobId,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<bool> {
        let rostered = self
            .committed
            .view(vec![overlay])
            .get(BLOBS, &namespace::blob(id))?
            .is_some();
        if rostered {
            return Ok(true);
        }
        unit::held(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            id,
            &mut self.loaded.limits().fuel,
        )
        .await
    }

    pub async fn blob(&self, id: &BlobId, time: u64) -> Result<Option<Vec<u8>>> {
        if !self.readable(id, time).await? {
            return Ok(None);
        }
        self.blobs
            .framed(id)?
            .map(Some)
            .ok_or(Error::BlobUnavailable(*id))
    }

    pub(crate) fn world(&self, height: u64, time: u64) -> World<'a, E> {
        World {
            store: self.committed,
            blobs: self.blobs,
            loaded: self.loaded,
            network: self.network,
            roles: self.roles,
            height,
            time,
            recorder: None,
            runs: Runs::Chain,
        }
    }

    pub async fn watch(
        &self,
        time: u64,
        origin: Origin,
        program: &str,
        request: Vec<u8>,
    ) -> Result<(std::result::Result<Vec<u8>, Refusal>, ReadSet)> {
        let unmetered = self.query_fuel().is_none();
        if unmetered {
            let refused = Refusal::new(
                reason::UNSUPPORTED,
                "this node caps no query's fuel (`load.query_fuel`), so a standing query could run forever",
            );
            return Ok((Err(refused), ReadSet::default()));
        }
        if let Some(code) = self.loaded.awaiting(program) {
            return Err(Error::BlobUnavailable(code));
        }
        let recorder = Recorder::default();
        let world = World {
            recorder: Some(&recorder),
            runs: Runs::Query,
            ..self.world(self.height()?, time)
        };
        let unstaged = Stage::default();
        let answer = unit::query(
            world,
            Vec::new(),
            &unstaged,
            &[],
            origin,
            program.to_owned(),
            request,
            &mut self.query_fuel(),
        )
        .await?;
        Ok((answer, recorder.into_reads()))
    }

    fn query_fuel(&self) -> Option<u64> {
        self.loaded.query_fuel()
    }

    pub async fn query(
        &self,
        layer: Layer,
        time: u64,
        origin: Origin,
        program: &str,
        request: Vec<u8>,
    ) -> Result<std::result::Result<Vec<u8>, Refusal>> {
        Ok(self
            .queried(layer, time, origin, program, request)
            .await?
            .answer)
    }

    pub async fn queried(
        &self,
        layer: Layer,
        time: u64,
        origin: Origin,
        program: &str,
        request: Vec<u8>,
    ) -> Result<Queried> {
        if let Some(code) = self.loaded.awaiting(program) {
            return Err(Error::BlobUnavailable(code));
        }
        let height = self.height_of(layer)?;
        let unstaged = Stage::default();
        let stage = match (layer, self.preconfirmed) {
            (Layer::Preconfirmed, Some(preconfirmed)) => &preconfirmed.stage,
            (Layer::Preconfirmed, None) | (Layer::Confirmed, _) => &unstaged,
        };
        let budget = self.loaded.query_budget();
        let mut fuel = Some(budget);
        let world = World {
            runs: Runs::Query,
            ..self.world(height, time)
        };
        let answer = unit::query(
            world,
            self.layers(layer),
            stage,
            &[],
            origin,
            program.to_owned(),
            request,
            &mut fuel,
        )
        .await?;
        let ran_out = matches!(&answer, Err(refusal) if refusal.reason == abi::reason::TRAP && refusal.sentence == runtime::OUT_OF_FUEL);
        Ok(Queried {
            answer,
            fuel: fuel.map_or(0, |left| budget.saturating_sub(left)),
            ran_out,
        })
    }
}

pub struct Queried {
    pub answer: std::result::Result<Vec<u8>, Refusal>,
    pub fuel: u64,
    pub ran_out: bool,
}

pub struct Snapshot<E> {
    pub(crate) pinned: Pinned,
    pub(crate) blobs: Blobs,
    pub(crate) loaded: Loaded,
    pub(crate) network: Vec<u8>,
    pub(crate) roles: Roles,
    pub(crate) preconfirmed: Arc<Preconfirmed>,
    pub(crate) rows_from: u64,
    pub(crate) context: PhantomData<fn() -> E>,
}

impl<E> Clone for Snapshot<E> {
    fn clone(&self) -> Snapshot<E> {
        Snapshot {
            pinned: self.pinned.clone(),
            blobs: self.blobs.clone(),
            loaded: self.loaded.clone(),
            network: self.network.clone(),
            roles: self.roles.clone(),
            preconfirmed: self.preconfirmed.clone(),
            rows_from: self.rows_from,
            context: PhantomData,
        }
    }
}

impl<E> Snapshot<E>
where
    E: Context + Spawner,
{
    pub fn source(&self) -> Source<'_, E> {
        Source {
            committed: Committed::Pinned(&self.pinned),
            blobs: &self.blobs,
            loaded: &self.loaded,
            network: &self.network,
            roles: &self.roles,
            preconfirmed: Some(&self.preconfirmed),
            rows_from: self.rows_from,
        }
    }

    pub async fn preconfirm(
        &mut self,
        time: u64,
        submissions: Vec<Submission>,
    ) -> Result<Vec<Receipt>> {
        self.ready()?;
        let height = self.source().next_height()?;
        let mut preconfirmed = std::mem::take(&mut self.preconfirmed);
        let working = Arc::make_mut(&mut preconfirmed);
        let mark = working.mark();
        let predicted = self
            .source()
            .predict(submissions, height, time, working)
            .await;
        if predicted.is_err() {
            working.rewind(mark);
        }
        self.preconfirmed = preconfirmed;
        predicted
    }

    fn ready(&mut self) -> Result<()> {
        let view = Committed::<E>::Pinned(&self.pinned).view(Vec::new());
        sync_code(&mut self.loaded, &view, &self.blobs)?;
        runnable(&self.loaded)
    }
}

impl<'a, E> Source<'a, E>
where
    E: Context + Spawner,
{
    async fn predict(
        &self,
        submissions: Vec<Submission>,
        height: u64,
        time: u64,
        preconfirmed: &mut Preconfirmed,
    ) -> Result<Vec<Receipt>> {
        let mut receipts = Vec::new();
        let mut accounts = Accounts::default();
        for submission in submissions {
            let submitted = self
                .submit(
                    submission,
                    height,
                    time,
                    &mut preconfirmed.overlay,
                    &mut preconfirmed.stage,
                    &mut accounts,
                )
                .await?;
            receipts.push(submitted.receipt());
        }
        Ok(receipts)
    }
}
