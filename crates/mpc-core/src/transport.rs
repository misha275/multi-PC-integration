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

use crate::protocol::Message;
use anyhow::{bail, ensure, Context, Result};
use snow::StatelessTransportState;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const MAGIC: &[u8; 4] = b"MPC1";
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

/// Run the handshake on `stream` and split it into an encrypted reader and writer.
pub async fn handshake<S>(stream: S, psk: &Psk, initiator: bool) -> Result<(SecureReader<S>, SecureWriter<S>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut r, mut w) = tokio::io::split(stream);
    let builder = snow::Builder::new(NOISE_PARAMS.parse()?).psk(0, psk)?;
    let mut buf = vec![0u8; MAX_FRAME];
    let mut frame = Vec::new();

    w.write_all(MAGIC).await?;
    w.flush().await?;
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).await?;
    if &magic != MAGIC {
        bail!("not a MultiPC peer");
    }

    let state = if initiator {
        let mut hs = builder.build_initiator()?;
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&mut w, &buf[..n]).await?;
        w.flush().await?;
        read_frame(&mut r, &mut frame).await?;
        hs.read_message(&frame, &mut buf).context("handshake failed: different pairing key?")?;
        hs.into_stateless_transport_mode()?
    } else {
        let mut hs = builder.build_responder()?;
        read_frame(&mut r, &mut frame).await?;
        hs.read_message(&frame, &mut buf).context("handshake failed: different pairing key?")?;
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&mut w, &buf[..n]).await?;
        w.flush().await?;
        hs.into_stateless_transport_mode()?
    };

    let state = Arc::new(state);
    Ok((
        SecureReader { inner: r, state: state.clone(), nonce: 0, frame: Vec::new(), plain: vec![0; MAX_FRAME] },
        SecureWriter { inner: w, state, nonce: 0, plain: Vec::with_capacity(MAX_FRAME), cipher: vec![0; MAX_FRAME] },
    ))
}

pub struct SecureWriter<S> {
    inner: tokio::io::WriteHalf<S>,
    state: Arc<StatelessTransportState>,
    nonce: u64,
    plain: Vec<u8>,
    cipher: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> SecureWriter<S> {
    pub async fn send(&mut self, msg: &Message) -> Result<()> {
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
    pub async fn recv(&mut self) -> Result<Message> {
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
    use crate::protocol::{ClipboardData, InputEvent};

    #[tokio::test]
    async fn roundtrip_small_and_large() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let psk = [7u8; 32];
        let (ra, rb) = tokio::join!(handshake(a, &psk, true), handshake(b, &psk, false));
        let (mut ra, mut wa) = ra.unwrap();
        let (mut rb, mut wb) = rb.unwrap();

        let small = Message::Input(InputEvent::MouseMove { dx: -3, dy: 7 });
        let big = Message::Clipboard(ClipboardData::Text("ж".repeat(200_000)));
        let (big2, small2) = (big.clone(), small.clone());
        let sender = tokio::spawn(async move {
            wa.send(&small2).await.unwrap();
            wa.send(&big2).await.unwrap();
            wa
        });
        assert_eq!(rb.recv().await.unwrap(), small);
        assert_eq!(rb.recv().await.unwrap(), big);
        sender.await.unwrap();

        // And the other direction.
        wb.send(&Message::Ping(42)).await.unwrap();
        assert_eq!(ra.recv().await.unwrap(), Message::Ping(42));
    }

    #[tokio::test]
    async fn wrong_key_is_rejected() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let (ra, rb) = tokio::join!(handshake(a, &[1u8; 32], true), handshake(b, &[2u8; 32], false));
        assert!(ra.is_err() || rb.is_err());
    }
}
