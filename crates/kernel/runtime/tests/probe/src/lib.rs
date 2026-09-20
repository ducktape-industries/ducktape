use borsh::{BorshDeserialize, BorshSerialize};
use guest::{HostOp, HostReply, Program, Refusal, abi};

#[derive(BorshSerialize, BorshDeserialize)]
pub enum Step {
    Op(HostOp),
    Spin,
    Grow(u32),
    Fail(String),
}

struct Probe;

impl Program for Probe {
    fn init(params: &[u8]) -> Result<(), Refusal> {
        Self::execute(params)
    }

    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let replies = run(payload)?;
        guest::output(abi::encode(&replies));
        Ok(())
    }

    fn query(request: &[u8]) -> Result<Vec<u8>, Refusal> {
        run(request).map(|replies| abi::encode(&replies))
    }
}

fn run(script: &[u8]) -> Result<Vec<HostReply>, Refusal> {
    let steps: Vec<Step> = abi::decode(script)?;
    let mut replies = Vec::new();
    for step in steps {
        match step {
            Step::Op(op) => replies.push(guest::host(&op)),
            Step::Spin => loop {
                core::hint::black_box(());
            },
            Step::Grow(pages) => {
                let bytes = vec![1u8; pages as usize * 65536];
                core::hint::black_box(&bytes);
            }
            Step::Fail(sentence) => return Err(Refusal::new("probe", sentence)),
        }
    }
    Ok(replies)
}

guest::program!(Probe);
