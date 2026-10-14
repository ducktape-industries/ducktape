//! The price feed on a bench host: a refresh parks on its call to provider, and resumes with
//! the answer or the refusal that resolves the call.
use std::collections::BTreeMap;

use abi::{
    Cause, Env, HostOp, HostReply, Invocation, Origin, Principal, ProgramCall, Refusal, Roles,
    reason,
};
use fixture_feed::{Ask, Op, PRICES, PROVIDER, Quote, quote_key};
use runtime::{Host, Image, Limits, Ran, Runtime};

const FEED: &[u8] = include_bytes!("../../fixtures/wasm/fixture_feed.wasm");
const LIMITS: Limits = Limits {
    fuel: Some(10_000_000),
    memory_bytes: Some(16 << 20),
};

#[derive(Default)]
struct Bench {
    calls: Vec<(String, Vec<u8>)>,
    state: BTreeMap<Vec<u8>, Vec<u8>>,
}

#[async_trait::async_trait]
impl Host for Bench {
    async fn call(&mut self, op: HostOp, _fuel: &mut Option<u64>) -> HostReply {
        match op {
            HostOp::Await { target, payload } => {
                self.calls.push((target, payload));
                HostReply::Ticket(self.calls.len() as u64)
            }
            HostOp::Set { key, value } => {
                self.state.insert(key, value);
                HostReply::Done
            }
            other => HostReply::Refused(Refusal::new(
                reason::UNSUPPORTED,
                format!("the bench does not serve {other:?}"),
            )),
        }
    }
}

fn at(height: u64, call: ProgramCall) -> Invocation {
    Invocation {
        env: Env {
            network: b"net".to_vec(),
            height,
            time: height * 10,
            me: "feed".into(),
            origin: Origin::External(vec![7; 32]),
            sender: Some(Principal::Account(1)),
            roles: Roles {
                programs: "programs".into(),
                validators: "validators".into(),
                identity: "identity".into(),
                blobs: "blobs".into(),
            },
            cause: Cause::Direct,
        },
        call,
    }
}

async fn refresh(bench: &mut Bench, pair: &str) -> Image {
    let runtime = Runtime::new(LIMITS);
    let code = runtime.load(FEED).unwrap();
    let op = Op::Refresh { pair: pair.into() };
    let call = ProgramCall::Execute(abi::encode(&op));
    let ran = runtime
        .start(&code, at(10, call), bench, &mut LIMITS.fuel.clone())
        .await
        .unwrap();
    let Ran::Parked(image) = ran else {
        panic!("a refresh parks on its call to provider, not {ran:?}");
    };
    image
}

async fn resolve(bench: &mut Bench, image: &Image, answer: Result<Vec<u8>, Refusal>) -> Ran {
    let runtime = Runtime::new(LIMITS);
    let code = runtime.load(FEED).unwrap();
    let call = ProgramCall::Resume { ticket: 1, answer };
    runtime
        .resume(&code, image, at(13, call), bench, &mut LIMITS.fuel.clone())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_refresh_parks_on_provider_and_keeps_the_quote_its_answer_brings() {
    let mut bench = Bench::default();
    let image = refresh(&mut bench, "BTC/USD").await;
    let ask = Ask {
        service: PRICES.into(),
        input: b"BTC/USD".to_vec(),
    };
    assert_eq!(bench.calls, [(PROVIDER.to_owned(), abi::encode(&ask))]);
    assert!(
        bench.state.is_empty(),
        "nothing is written before the answer"
    );

    let ran = resolve(&mut bench, &image, Ok(abi::encode(&64_000u64))).await;
    assert_eq!(ran, Ran::Done(Ok(())));
    let quote: Quote = abi::decode(&bench.state[&quote_key("BTC/USD")]).unwrap();
    assert_eq!(
        quote,
        Quote {
            price: 64_000,
            asked_at: 10,
            answered_at: 13,
        }
    );
}

#[tokio::test]
async fn an_expired_call_refuses_the_refresh_with_the_expiry() {
    let mut bench = Bench::default();
    let image = refresh(&mut bench, "BTC/USD").await;
    let expired = Refusal::new(reason::WRONG_STATE, "the call expired at height 74");
    let ran = resolve(&mut bench, &image, Err(expired.clone())).await;
    assert_eq!(ran, Ran::Done(Err(expired)));
    assert!(bench.state.is_empty());
}
