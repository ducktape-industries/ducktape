// One RPC request's context, built by the server before routing: the newest view, taken once, and
// the handles a handler may use. Handlers see the node only through it.

use std::sync::Arc;

use abi::role::crosspond::{Acked, settled_key};
use abi::{NetworkId, ProgramId, Refusal, reason, role};
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use host::Layer;
use node::{Preconfirm, Source, Taken};

use crate::changes::Subscription;
use crate::crosspond::{Asked, Unbuilt};
use crate::view::{Reading, View};
use crate::wire::{
    AcksAsk, BlockAsk, Blocks, FactsAsk, FactsPage, Finalized, Located, Network, Page, TxAsk,
    TxPage, TxRange, activity,
};
use crate::{Context, Daemon, Error, Result};

pub(crate) struct Ctx<E: Context> {
    view: Arc<View<E>>,
    daemon: Arc<Daemon<E>>,
}

impl<E: Context> Clone for Ctx<E> {
    fn clone(&self) -> Self {
        Ctx {
            view: self.view.clone(),
            daemon: self.daemon.clone(),
        }
    }
}

pub(crate) struct Stopped(Error);

pub(crate) enum EgressSigner {
    Node,
    Granted,
    Stranger,
}

impl IntoResponse for Stopped {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

impl<E: Context> FromRequestParts<Arc<Daemon<E>>> for Ctx<E> {
    type Rejection = Stopped;

    async fn from_request_parts(
        _: &mut Parts,
        daemon: &Arc<Daemon<E>>,
    ) -> std::result::Result<Self, Stopped> {
        Ctx::of(daemon).map_err(Stopped)
    }
}

impl<E: Context> Ctx<E> {
    pub(crate) fn of(daemon: &Arc<Daemon<E>>) -> Result<Ctx<E>> {
        let view = daemon.view()?;
        Ok(Ctx {
            view,
            daemon: daemon.clone(),
        })
    }

    pub(crate) fn view(&self) -> &Arc<View<E>> {
        &self.view
    }

    pub(crate) fn reading(&self, layer: Layer) -> Result<Reading<E>> {
        match layer {
            Layer::Confirmed => Ok(Reading::Confirmed(self.view.clone())),
            Layer::Preconfirmed => self.daemon.current.reading(layer),
        }
    }

    pub(crate) fn network_id(&self) -> Vec<u8> {
        self.daemon.descriptor.id()
    }

    pub(crate) fn identity(&self) -> &[u8] {
        &self.daemon.identity
    }

    pub(crate) fn now(&self) -> u64 {
        self.daemon.now()
    }

    pub(crate) async fn submit(
        &self,
        frame: Vec<u8>,
        preconfirm: Preconfirm,
    ) -> Result<std::result::Result<Taken, Refusal>> {
        let checked = match self.daemon.admit(frame.clone()).await? {
            Ok(checked) => checked,
            Err(refusal) => return Ok(Err(refusal)),
        };
        self.daemon
            .current
            .for_anchor(
                self.tips(),
                &self.daemon.finalized,
                checked.submission().anchor.height,
            )
            .await?;
        let taken = self.daemon.take(checked, Source::Client, preconfirm).await;
        if let Ok(Ok(Taken::New(_))) = &taken {
            self.daemon.relay.send([frame.as_slice()]);
        }
        taken
    }

    pub(crate) async fn query(
        &self,
        query: crate::wire::Query,
    ) -> Result<std::result::Result<(ProgramId, host::Queried), Refusal>> {
        crate::query::answer(
            &self.daemon.current,
            self.tips(),
            &self.daemon.finalized,
            query,
            self.network_id(),
            || self.now(),
        )
        .await
    }

    pub(crate) async fn play(
        &self,
        body: node::Body,
        preconfirm: Preconfirm,
    ) -> Result<std::result::Result<Taken, Refusal>> {
        self.daemon.play(body, preconfirm).await
    }

    pub(crate) async fn advance(&self, height: u64) -> Result<u64> {
        self.daemon.advance(height).await
    }

    pub(crate) async fn sync(&self, request: statesync::Request) -> Result<statesync::Response> {
        self.daemon.sync(request).await
    }

    pub(crate) async fn held_head(&self) -> Result<(statesync::Response, Option<crate::HeldHead>)> {
        self.daemon.held_head().await
    }

    pub(crate) fn tips(&self) -> tokio::sync::watch::Receiver<u64> {
        self.daemon.tips.subscribe()
    }

    pub(crate) async fn network(&self) -> Result<Network> {
        self.daemon.network().await
    }

    pub(crate) async fn blocks(&self, page: Blocks) -> Result<Page> {
        self.daemon.blocks(page).await
    }

    pub(crate) async fn block(&self, ask: BlockAsk) -> Result<Option<Finalized>> {
        self.daemon.block(ask).await
    }

    pub(crate) async fn block_facts(&self, ask: FactsAsk) -> Result<FactsPage> {
        self.daemon.block_facts(ask).await
    }

    pub(crate) async fn txs(&self, range: TxRange) -> Result<TxPage> {
        self.daemon.txs(range).await
    }

    pub(crate) async fn crosspond_acks(
        &self,
        ask: AcksAsk,
    ) -> std::result::Result<std::result::Result<Acked, Refusal>, String> {
        let Some(program) = self.view.source().roles().get(role::CROSSPOND).cloned() else {
            return Ok(Err(Refusal::new(
                reason::NOT_FOUND,
                format!(
                    "this network binds no program to its {} role",
                    role::CROSSPOND
                ),
            )));
        };
        let view = self.view.clone();
        let read = program.clone();
        let AcksAsk { from, seqs, held } = ask;
        let rows = crate::offload(move || settled_rows(&view, &read, &from, &seqs))
            .await
            .map_err(|error| error.to_string())??;
        let sync = self.view.sync().clone();
        let serving = |program: &str| sync.serving(program).cloned();
        let asked = Asked {
            program: &program,
            held,
            rows,
        };
        match crate::crosspond::acked(&self.daemon.crosspond, &serving, asked).await {
            Ok(acked) => Ok(Ok(acked)),
            Err(Unbuilt::Unopened) => Ok(Err(Refusal::new(
                reason::NOT_YET,
                Unbuilt::Unopened.to_string(),
            ))),
            Err(unbuilt) => Err(unbuilt.to_string()),
        }
    }

    pub(crate) async fn tx(&self, ask: TxAsk) -> Result<Option<Located>> {
        self.daemon.tx(ask).await
    }

    pub(crate) async fn activity(
        &self,
        query: activity::Query,
    ) -> Result<std::result::Result<activity::Reply, Refusal>> {
        let reads = self.daemon.activity.reads();
        crate::offload(move || reads.read(query)).await?
    }

    pub(crate) fn blobs(&self) -> &blob::service::Service {
        &self.daemon.blobs
    }

    pub(crate) fn logs(&self) -> Vec<String> {
        self.daemon.logs.lines()
    }

    pub(crate) async fn health(
        &self,
        since: Option<u64>,
    ) -> Result<std::result::Result<crate::health::Health, String>> {
        self.daemon.health(since).await
    }

    pub(crate) fn retune(&self, directives: &str) -> std::result::Result<(), String> {
        self.daemon.logs.retune(directives)
    }

    pub(crate) fn shut_down(&self) {
        self.daemon.shutdown.send_replace(true);
    }

    pub(crate) fn reload_egress(&self) -> std::result::Result<(), crate::egress::Refused> {
        self.daemon.egress.reload()
    }

    pub(crate) fn egress_signer(&self, signer: &[u8]) -> std::io::Result<EgressSigner> {
        let own = signer == self.daemon.identity.as_slice();
        if own {
            return Ok(EgressSigner::Node);
        }
        let grants = self.daemon.egress.grants()?;
        match grants.iter().any(|key| key.as_slice() == signer) {
            true => Ok(EgressSigner::Granted),
            false => Ok(EgressSigner::Stranger),
        }
    }

    pub(crate) fn egress_wait(&self) -> std::time::Duration {
        let block = self.daemon.network.cadence.block_time_ms();
        std::time::Duration::from_millis(abi::role::egress::DEFAULT_WAIT * block)
    }

    pub(crate) fn plan_egress(
        &self,
        text: String,
    ) -> std::result::Result<crate::egress::Planned, crate::egress::Refused> {
        self.daemon.egress.plan(text)
    }

    pub(crate) fn put_egress(
        &self,
        planned: crate::egress::Planned,
    ) -> std::result::Result<crate::egress::Tally, crate::egress::Refused> {
        self.daemon.egress.put(planned)
    }

    pub(crate) async fn metrics(self) -> Result<String> {
        let daemon = self.daemon;
        crate::offload(move || daemon.context.encode()).await
    }

    pub(crate) fn subscribe(self, program: ProgramId, keys: bool) -> Subscription {
        self.daemon.subscribe(program, keys)
    }

    pub(crate) fn watch(self, program: ProgramId, keys: bool) -> crate::standing::Watching<E> {
        crate::standing::Watching {
            pull: self.daemon.changes.pull(program, keys),
            daemon: self.daemon,
        }
    }

    pub(crate) fn subscribe_heads(self) -> Subscription {
        self.daemon.subscribe_heads()
    }

    pub(crate) fn mesh(self) -> std::net::SocketAddr {
        self.daemon.mesh
    }

    pub(crate) fn live(self) -> Arc<crate::live::Relay<E>> {
        self.daemon.live.clone()
    }

    pub(crate) fn halo(self) -> Arc<halo::Halo> {
        self.daemon.halo.clone()
    }

    pub(crate) fn forwarding(self) -> Arc<Daemon<E>> {
        self.daemon
    }

    pub(crate) fn answered(&self, answered: crate::server::Answered) {
        let _ = self.daemon.answered.send(answered);
    }

    pub(crate) fn queried(&self, queried: crate::census::Queried) {
        let _ = self.daemon.queried.send(queried);
    }

    pub(crate) fn holds_ended(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.daemon.holds_ended.clone()
    }

    pub(crate) fn snapshot(&self) -> Option<Arc<crate::snapshot::Kept>> {
        self.daemon.snapshot.load_full()
    }

    pub(crate) fn live_port(&self) -> Option<u16> {
        self.daemon.live.port()
    }

    pub(crate) fn halo_self(&self) -> crate::wire::HaloSelf {
        self.daemon.halo_self.borrow().clone()
    }
}

type Rows = Vec<(Vec<u8>, Vec<u8>)>;

fn settled_rows<E: Context>(
    view: &View<E>,
    program: &str,
    from: &NetworkId,
    seqs: &[u64],
) -> std::result::Result<Rows, String> {
    let source = view.source();
    let state = source.view(Layer::Confirmed);
    let mut rows = Vec::new();
    for seq in seqs {
        let key = settled_key(from, *seq);
        let value = state
            .get(program, &key)
            .map_err(|error| error.to_string())?;
        if let Some(value) = value {
            rows.push((key, value));
        }
    }
    Ok(rows)
}
