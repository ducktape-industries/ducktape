use abi::ProgramId;
use commonware_codec::{Decode as _, Encode as _};
use commonware_storage::qmdb::sync::{FeedbackTx, Source};
use node::Digest;

use crate::wire::{Request, Response};
use crate::{Error, Exchange, SyncRequest, SyncResponse, ask};

pub struct Remote<X> {
    pub exchange: X,
    pub program: ProgramId,
}

impl<X: Exchange> Source for Remote<X> {
    type Family = state::Family;
    type Digest = Digest;
    type Op = state::Op;
    type Error = Error;

    async fn serve(&self, request: SyncRequest) -> Result<(SyncResponse, FeedbackTx), Error> {
        let max_ops = request.max_ops().get() as usize;
        let request = Request::Sync {
            program: self.program.clone(),
            request: request.encode().to_vec(),
        };
        let bytes = match ask(&self.exchange, request).await? {
            Response::Sync(bytes) => bytes,
            Response::Refused(refusal) => return Err(Error::Refused(refusal)),
            other => return Err(Error::Answer(Box::new(other))),
        };
        let response =
            SyncResponse::decode_cfg(bytes.as_slice(), &(max_ops, state::codec_config()))?;
        Ok((response, None))
    }
}
