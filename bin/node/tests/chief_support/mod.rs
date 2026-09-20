#![allow(dead_code)]
//! The real-host bed the chief installer runs on: signed people reach a keyless
//! model program, and the ops a plan emits cross a genesis whose app modules are
//! the COMMITTED guests this repo ships
//! (`crates/modules/apps/<id>/component.wasm`). Their source lives in
//! ducktape-modules, and the sibling wiring a native constructor used to take as
//! arguments is compiled into each guest; the system modules are core-resident
//! and still native.

use host::{BlockContext, Host};
use noded::testkit::committed_module;
use sdk::{Msg, Origin};
use sdk_testkit::MemStore;

/// this composition's identity chain id — and the value the `runs` guest reads
/// out of its genesis `__config` record.
const CHAIN_ID: &str = "runs-test";

pub fn store() -> Box<dyn sdk::MerkleStore> {
    Box::new(MemStore::new())
}
pub fn member() -> Origin {
    Origin::External(vec![1; 32])
}
pub fn provider() -> Origin {
    Origin::External(vec![8; 32])
}
pub fn session() -> Origin {
    Origin::External(vec![9; 32])
}
fn context(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}
pub fn msg<T: serde::Serialize>(target: &str, payload: &T) -> Msg {
    Msg {
        target: target.into(),
        payload: sdk::wire::encode(payload),
    }
}

pub struct Network {
    pub host: Host,
    pub height: u64,
    pub events: Vec<sdk::Event>,
}
impl Network {
    pub async fn new() -> Self {
        let mut valset = valset::Valset::new("valset", store(), "governance");
        valset.seed(vec![8; 32]).await.unwrap();
        valset.seed(vec![7; 32]).await.unwrap();
        valset.finish_seed().await.unwrap();
        let host = Host::genesis(vec![
            Box::new(identity::Identity::new(
                "identity",
                store(),
                CHAIN_ID.into(),
            )),
            Box::new(
                attribution::AttributionModule::new("attribution", store())
                    .with_subscribers(["agent"]),
            ),
            Box::new(committed_module("agent", store(), CHAIN_ID).await),
            Box::new(committed_module("chat", store(), CHAIN_ID).await),
            Box::new(committed_module("pages", store(), CHAIN_ID).await),
            Box::new(valset),
            Box::new(capability::CapabilityRegistry::new(
                "capability",
                store(),
                Some("valset".into()),
            )),
            Box::new(saga::SagaModule::with_assignment(
                "saga",
                store(),
                "valset",
                "capability",
                saga::LeasePolicy::Open,
            )),
            Box::new(dispatch::DispatchModule::new(
                "dispatch",
                "saga",
                "identity",
                store(),
            )),
            Box::new(committed_module("tasks", store(), CHAIN_ID).await),
            Box::new(files::Files::in_mem()),
            Box::new(committed_module("runs", store(), CHAIN_ID).await),
        ])
        .unwrap();
        Self {
            host,
            height: 0,
            events: Vec::new(),
        }
    }
    pub async fn submit(&mut self, origin: Origin, message: Msg) {
        self.height += 1;
        let outcome = self
            .host
            .submit_at(context(self.height, origin), message)
            .await
            .unwrap();
        self.events.extend(outcome.events);
    }
    pub async fn step(&mut self) {
        self.height += 1;
        let outcome = self
            .host
            .submit_block(context(self.height, Origin::System), Vec::new())
            .await
            .unwrap();
        self.events.extend(outcome.events);
    }
    pub async fn drain(&mut self) {
        // Every step executes the host's next committed queue batch. There is
        // no clock or external worker to poll in this deterministic drain.
        while self.host.has_pending_work().await.unwrap() {
            self.step().await;
        }
    }
}
