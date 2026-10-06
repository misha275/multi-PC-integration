//! Encrypted, authenticated message stream between two peers.
//!
//! All machines of one group share a random 32-byte key (the "pairing key").
//! Sessions use the Noise `NNpsk0` handshake: fresh ephemeral keys per connection
//! (forward secrecy) and the pairing key mixed in, so a machine without the key
//! can neither connect nor read anything.
//!
//! Wire format after the handshake: frames of `u16 length (BE) || ciphertext`.
//! Each frame's plaintext is `flag || data`, where flag 1 marks the last frame
//! of a message, so messages larger than one Noise frame are split.

use anyhow::{bail, ensure, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use snow::StatelessTransportState;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const MAX_FRAME: usize = 65535;
const TAG_LEN: usize = 16;
const CHUNK: usize = MAX_FRAME - TAG_LEN - 1;
/// Upper bound for one message (clipboard images are the largest).
pub const MAX_MESSAGE: usize = 128 * 1024 * 1024;

pub type Psk = [u8; 32];

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    ensure!(data.len() <= MAX_FRAME, "frame too large");
    w.write_all(&(data.len() as u16).to_be_bytes()).await?;
    w.write_all(data).await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> Result<()> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    buf.resize(u16::from_be_bytes(len) as usize, 0);
    r.read_exact(buf).await?;
    Ok(())
}

/// What a connection is for. The first four bytes on the wire tell the listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A machine of our group, authenticated by the shared key.
    Group,
    /// A machine asking to join; encrypted but not yet trusted. Both sides show a
    /// code derived from the handshake, and the user compares them.
    Pairing,
}

const GROUP_MAGIC: &[u8; 4] = b"MPC2";
const PAIR_MAGIC: &[u8; 4] = b"MPP1";
const NOISE_PAIR: &str = "Noise_NN_25519_ChaChaPoly_BLAKE2s";

/// Listener side: read which kind of connection this is.
pub async fn read_kind<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Kind> {
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic).await?;
    match &magic {
        GROUP_MAGIC => Ok(Kind::Group),
        PAIR_MAGIC => Ok(Kind::Pairing),
        _ => bail!("not a MultiPC peer (or a different version)"),
    }
}

pub struct Session<S> {
    pub reader: SecureReader<S>,
    pub writer: SecureWriter<S>,
    /// Six-digit code both sides can compare; only meaningful for pairing.
    pub code: String,
}

/// Open an encrypted session as the connecting side.
pub async fn connect<S>(mut stream: S, kind: Kind, psk: &Psk) -> Result<Session<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(if kind == Kind::Group { GROUP_MAGIC } else { PAIR_MAGIC }).await?;
    handshake(stream, kind, psk, true).await
}

/// Open an encrypted session as the listening side, after [`read_kind`].
pub async fn accept<S>(stream: S, kind: Kind, psk: &Psk) -> Result<Session<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handshake(stream, kind, psk, false).await
}

async fn handshake<S>(stream: S, kind: Kind, psk: &Psk, initiator: bool) -> Result<Session<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut r, mut w) = tokio::io::split(stream);
    let builder = match kind {
        Kind::Group => snow::Builder::new(NOISE_PARAMS.parse()?).psk(0, psk)?,
        Kind::Pairing => snow::Builder::new(NOISE_PAIR.parse()?),
    };
    let mut buf = vec![0u8; MAX_FRAME];
    let mut frame = Vec::new();

    let (state, hash) = if initiator {
        let mut hs = builder.build_initiator()?;
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&mut w, &buf[..n]).await?;
        w.flush().await?;
        read_frame(&mut r, &mut frame).await?;
        hs.read_message(&frame, &mut buf).context("handshake failed: different pairing key?")?;
        let hash = hs.get_handshake_hash().to_vec();
        (hs.into_stateless_transport_mode()?, hash)
    } else {
        let mut hs = builder.build_responder()?;
        read_frame(&mut r, &mut frame).await?;
        hs.read_message(&frame, &mut buf).context("handshake failed: different pairing key?")?;
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&mut w, &buf[..n]).await?;
        w.flush().await?;
        let hash = hs.get_handshake_hash().to_vec();
        (hs.into_stateless_transport_mode()?, hash)
    };
    let n = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]) % 1_000_000;
    let code = format!("{:03} {:03}", n / 1000, n % 1000);

    let state = Arc::new(state);
    Ok(Session {
        code,
        reader: SecureReader { inner: r, state: state.clone(), nonce: 0, frame: Vec::new(), plain: vec![0; MAX_FRAME] },
        writer: SecureWriter { inner: w, state, nonce: 0, plain: Vec::with_capacity(MAX_FRAME), cipher: vec![0; MAX_FRAME] },
    })
}

pub struct SecureWriter<S> {
    inner: tokio::io::WriteHalf<S>,
    state: Arc<StatelessTransportState>,
    nonce: u64,
    plain: Vec<u8>,
    cipher: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> SecureWriter<S> {
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let bytes = postcard::to_stdvec(msg)?;
        ensure!(bytes.len() <= MAX_MESSAGE, "message too large");
        let mut chunks = bytes.chunks(CHUNK).peekable();
        if chunks.peek().is_none() {
            self.send_frame(&[], true).await?;
        }
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            self.send_frame(chunk, last).await?;
        }
        self.inner.flush().await?;
        Ok(())
    }

    async fn send_frame(&mut self, data: &[u8], last: bool) -> Result<()> {
        self.plain.clear();
        self.plain.push(last as u8);
        self.plain.extend_from_slice(data);
        let n = self.state.write_message(self.nonce, &self.plain, &mut self.cipher)?;
        self.nonce += 1;
        write_frame(&mut self.inner, &self.cipher[..n]).await
    }
}

pub struct SecureReader<S> {
    inner: tokio::io::ReadHalf<S>,
    state: Arc<StatelessTransportState>,
    nonce: u64,
    frame: Vec<u8>,
    plain: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> SecureReader<S> {
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        let mut msg = Vec::new();
        loop {
            read_frame(&mut self.inner, &mut self.frame).await?;
            let n = self.state.read_message(self.nonce, &self.frame, &mut self.plain).context("corrupted or forged frame")?;
            self.nonce += 1;
            ensure!(n >= 1, "empty frame");
            msg.extend_from_slice(&self.plain[1..n]);
            ensure!(msg.len() <= MAX_MESSAGE, "message too large");
            if self.plain[0] == 1 {
                break;
            }
        }
        Ok(postcard::from_bytes(&msg)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClipboardData, InputEvent, Message};

    async fn pair_up(
        kind: Kind,
        psk_a: Psk,
        psk_b: Psk,
    ) -> (Result<Session<tokio::io::DuplexStream>>, Result<Session<tokio::io::DuplexStream>>) {
        let (a, mut b) = tokio::io::duplex(1 << 16);
        let server = async move {
            let k = read_kind(&mut b).await?;
            assert_eq!(k, kind);
            accept(b, k, &psk_b).await
        };
        tokio::join!(connect(a, kind, &psk_a), server)
    }

    #[tokio::test]
    async fn roundtrip_small_and_large() {
        let (sa, sb) = pair_up(Kind::Group, [7u8; 32], [7u8; 32]).await;
        let Session { reader: mut ra, writer: mut wa, .. } = sa.unwrap();
        let Session { reader: mut rb, writer: mut wb, .. } = sb.unwrap();

        let small = Message::Input(InputEvent::MouseMove { dx: -3, dy: 7 });
        let big = Message::Clipboard(ClipboardData::Text("ж".repeat(200_000)));
        let (big2, small2) = (big.clone(), small.clone());
        let sender = tokio::spawn(async move {
            wa.send(&small2).await.unwrap();
            wa.send(&big2).await.unwrap();
            wa
        });
        assert_eq!(rb.recv::<Message>().await.unwrap(), small);
        assert_eq!(rb.recv::<Message>().await.unwrap(), big);
        sender.await.unwrap();

        // And the other direction.
        wb.send(&Message::Ping(42)).await.unwrap();
        assert_eq!(ra.recv::<Message>().await.unwrap(), Message::Ping(42));
    }

    #[tokio::test]
    async fn wrong_key_is_rejected() {
        let (sa, sb) = pair_up(Kind::Group, [1u8; 32], [2u8; 32]).await;
        assert!(sa.is_err() || sb.is_err());
    }

    #[tokio::test]
    async fn pairing_needs_no_key_and_both_sides_see_the_same_code() {
        let (sa, sb) = pair_up(Kind::Pairing, [1u8; 32], [2u8; 32]).await;
        let (mut a, mut b) = (sa.unwrap(), sb.unwrap());
        assert_eq!(a.code, b.code);
        assert_eq!(a.code.len(), 7);
        a.writer.send(&"привет".to_string()).await.unwrap();
        assert_eq!(b.reader.recv::<String>().await.unwrap(), "привет");
        // A fresh session gets a different code.
        let (sc, _) = pair_up(Kind::Pairing, [0; 32], [0; 32]).await;
        assert_ne!(sc.unwrap().code, a.code);
        let _ = &mut b;
    }
}
