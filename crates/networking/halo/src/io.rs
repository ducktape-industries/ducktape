//! The node-key handshake over any byte stream: commonware's sink and stream over tokio
//! halves, and the handshake's encrypted frames carried back as a byte stream.

use commonware_runtime::{Error, IoBufs, Sink, Stream};
use commonware_stream::encrypted::{Receiver, Sender};
use tokio::io::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader, DuplexStream,
};

pub const FRAME: usize = 64 << 10;
const WINDOW: usize = 256 << 10;

pub struct Writer<W>(pub W);

impl<W: AsyncWrite + Unpin + Send + Sync + 'static> Sink for Writer<W> {
    async fn send(&mut self, bufs: impl Into<IoBufs> + Send) -> Result<(), Error> {
        let mut bufs = bufs.into();
        self.0
            .write_all_buf(&mut bufs)
            .await
            .map_err(|_| Error::SendFailed)?;
        self.0.flush().await.map_err(|_| Error::SendFailed)
    }
}

pub struct Reader<R>(BufReader<R>);

impl<R: AsyncRead> Reader<R> {
    pub fn new(inner: R) -> Reader<R> {
        Reader(BufReader::new(inner))
    }
}

impl<R: AsyncRead + Unpin + Send + Sync + 'static> Stream for Reader<R> {
    async fn recv(&mut self, len: usize) -> Result<IoBufs, Error> {
        let mut bytes = vec![0u8; len];
        self.0
            .read_exact(&mut bytes)
            .await
            .map_err(|_| Error::RecvFailed)?;
        Ok(IoBufs::from(bytes))
    }

    fn peek(&self, max_len: usize) -> &[u8] {
        let buffered = self.0.buffer();
        &buffered[..buffered.len().min(max_len)]
    }
}

pub fn carried<O: Sink, I: Stream>(
    mut sender: Sender<O>,
    mut receiver: Receiver<I>,
) -> DuplexStream {
    let (ours, theirs) = tokio::io::duplex(WINDOW);
    let (mut from_us, mut to_us) = tokio::io::split(theirs);
    tokio::spawn(async move {
        let mut buf = vec![0u8; FRAME];
        loop {
            let read = from_us.read(&mut buf).await.unwrap_or(0);
            let ended = read == 0;
            let sent = sender.send(buf[..read].to_vec()).await;
            if ended || sent.is_err() {
                return;
            }
        }
    });
    tokio::spawn(async move {
        while let Ok(frame) = receiver.recv().await {
            let frame = frame.coalesce();
            if frame.is_empty() {
                break;
            }
            if to_us.write_all(frame.as_ref()).await.is_err() {
                return;
            }
        }
        let _ = to_us.shutdown().await;
    });
    ours
}
