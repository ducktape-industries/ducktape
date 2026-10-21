//! What two linked members say below the lanes, on one unidirectional stream each way per
//! connection: where each is reached directly, and the same passed on for a member they do not
//! share a link with. `reply` asks the one introduced to answer once with its own.

use std::io;
use std::net::SocketAddr;

use borsh::{BorshDeserialize, BorshSerialize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

pub(crate) const VERSION: u8 = 0;

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub(crate) enum Said {
    Mine {
        addrs: Vec<String>,
    },
    Introduce {
        to: Vec<u8>,
        addrs: Vec<String>,
        reply: bool,
    },
    Introduced {
        from: Vec<u8>,
        addrs: Vec<String>,
        reply: bool,
    },
}

pub(crate) fn read_addrs(written: &[String]) -> Vec<SocketAddr> {
    written
        .iter()
        .filter_map(|at| at.parse::<SocketAddr>().ok())
        .filter(|at| !at.ip().is_loopback() && !at.ip().is_unspecified())
        .collect()
}

pub(crate) fn written(addrs: &[SocketAddr]) -> Vec<String> {
    addrs.iter().map(ToString::to_string).collect()
}

pub(crate) async fn write<S: AsyncWrite + Unpin>(stream: &mut S, said: &Said) -> io::Result<()> {
    let body = borsh::to_vec(said)?;
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(&body).await
}

pub(crate) async fn read<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Option<Said>> {
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u64::from(u32::from_le_bytes(len));
    let mut body = Vec::new();
    (&mut *stream).take(len).read_to_end(&mut body).await?;
    let whole = body.len() as u64 == len;
    if !whole {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    borsh::from_slice(&body).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_message_reads_back_as_written_and_a_cut_one_is_refused() {
        let said = [
            Said::Mine {
                addrs: vec!["203.0.113.7:9100".into()],
            },
            Said::Introduce {
                to: vec![7; 32],
                addrs: vec!["198.51.100.2:4433".into()],
                reply: true,
            },
            Said::Introduced {
                from: vec![9; 32],
                addrs: Vec::new(),
                reply: false,
            },
        ];
        let mut wire = Vec::new();
        for one in &said {
            write(&mut wire, one).await.unwrap();
        }
        let mut reading = wire.as_slice();
        for one in &said {
            assert_eq!(read(&mut reading).await.unwrap().as_ref(), Some(one));
        }
        assert_eq!(read(&mut reading).await.unwrap(), None);
        let cut = &wire[..wire.len() - 1];
        let mut reading = cut;
        for _ in 0..said.len() - 1 {
            read(&mut reading).await.unwrap();
        }
        let refused = read(&mut reading).await.unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn addresses_read_back_as_written_and_a_wrong_loopback_or_unspecified_one_is_left_out() {
        let addrs: Vec<SocketAddr> = vec![
            "203.0.113.7:9100".parse().unwrap(),
            "[2001:db8::1]:9100".parse().unwrap(),
            "192.168.1.7:9100".parse().unwrap(),
        ];
        let mut text = written(&addrs);
        for left_out in [
            "not an address",
            "127.0.0.1:9100",
            "0.0.0.0:9100",
            "[::1]:9100",
            "[::]:9100",
        ] {
            text.push(left_out.into());
        }
        assert_eq!(read_addrs(&text), addrs);
    }
}
