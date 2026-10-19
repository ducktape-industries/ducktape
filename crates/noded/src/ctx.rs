// One RPC request's context, built by the server before routing: the newest view, taken once, and
// the handles a handler may use. Handlers see the node only through it.

use std::sync::Arc;

use abi::{BlobId, ProgramId, Refusal};
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use host::Layer;
use node::{Preconfirm, Source, Taken};

use crate::changes::Subscription;
use crate::view::{Reading, View};
use crate::wire::{BlockAsk, Blocks, Finalized, Located, Network, Page, TxAsk, activity};
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
        let view = daemon.view().map_err(Stopped)?;
        Ok(Ctx {
            view,
            daemon: daemon.clone(),
        })
    }
}

impl<E: Context> Ctx<E> {
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
        let taken = self
            .daemon
            .submit(frame.clone(), Source::Client, preconfirm)
            .await;
        if let Ok(Ok(Taken::New(_))) = &taken {
            self.daemon.relay.send([frame.as_slice()]);
        }
        taken
    }

    pub(crate) async fn sync(&self, request: statesync::Request) -> Result<statesync::Response> {
        self.daemon.sync(request).await
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

    pub(crate) async fn fetch(&self, id: BlobId) {
        self.daemon.blobs.fetch(id, halo::Class::Client).await;
    }

    pub(crate) fn logs(&self) -> Vec<String> {
        self.daemon.logs.lines()
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

    pub(crate) fn mesh(self) -> std::net::SocketAddr {
        self.daemon.mesh
    }

    pub(crate) fn live(self) -> Arc<crate::live::Relay<E>> {
        self.daemon.live.clone()
    }

    pub(crate) fn daemon(self) -> Arc<Daemon<E>> {
        self.daemon
    }

    pub(crate) fn live_port(&self) -> Option<u16> {
        self.daemon.live.port()
    }
}
