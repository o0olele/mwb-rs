//! Noise handles authentication, encryption, directional keys and nonce management.
//! A fresh handshake is performed on every connection; pairing secrets never travel on the wire.
use anyhow::{Context, Result, ensure};
use snow::{Builder, TransportState};
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use crate::protocol::Message;

const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"mwb-rust/v1";
pub const MAX_FRAME: usize = 65_535;

pub fn handshake(
    stream: &mut TcpStream,
    key: &[u8; 32],
    initiator: bool,
) -> Result<TransportState> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let builder = Builder::new(PATTERN.parse()?)
        .prologue(PROLOGUE)?
        .psk(0, key)?;
    let mut state = if initiator {
        builder.build_initiator()?
    } else {
        builder.build_responder()?
    };
    let mut buf = [0u8; MAX_FRAME];
    if initiator {
        let len = state.write_message(&[], &mut buf)?;
        write_frame(stream, &buf[..len])?;
        let frame = read_frame(stream)?;
        state
            .read_message(&frame, &mut buf)
            .context("配对失败，请检查两端密钥")?;
    } else {
        let frame = read_frame(stream)?;
        state
            .read_message(&frame, &mut buf)
            .context("配对失败，请检查两端密钥")?;
        let len = state.write_message(&[], &mut buf)?;
        write_frame(stream, &buf[..len])?;
    }
    Ok(state.into_transport_mode()?)
}

fn write_frame(stream: &mut impl Write, bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_FRAME,
        "数据帧大小无效"
    );
    stream.write_all(&(bytes.len() as u16).to_be_bytes())?;
    stream.write_all(bytes)?;
    Ok(())
}

fn read_frame(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut header = [0; 2];
    stream.read_exact(&mut header)?;
    let size = u16::from_be_bytes(header) as usize;
    ensure!(size > 0, "空数据帧");
    let mut data = vec![0; size];
    stream.read_exact(&mut data)?;
    Ok(data)
}

pub fn encode(state: &mut TransportState, msg: &Message) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(msg)?;
    ensure!(payload.len() + 16 <= MAX_FRAME, "消息过大");
    let mut encrypted = vec![0; payload.len() + 16];
    let len = state.write_message(&payload, &mut encrypted)?;
    encrypted.truncate(len);
    let mut frame = Vec::with_capacity(len + 2);
    write_frame(&mut frame, &encrypted)?;
    Ok(frame)
}

/// Preserves partial TCP reads, including a split length prefix.
pub fn decode_available(
    state: &mut TransportState,
    incoming: &mut Vec<u8>,
) -> Result<Vec<Message>> {
    let mut messages = Vec::new();
    let mut offset = 0;
    while incoming.len() - offset >= 2 {
        let size = u16::from_be_bytes([incoming[offset], incoming[offset + 1]]) as usize;
        ensure!(size >= 16, "无效的加密帧");
        if incoming.len() - offset < size + 2 {
            break;
        }
        let mut payload = vec![0; size];
        let len = state.read_message(&incoming[offset + 2..offset + 2 + size], &mut payload)?;
        messages.push(serde_json::from_slice(&payload[..len])?);
        offset += size + 2;
    }
    incoming.drain(..offset);
    ensure!(incoming.len() <= MAX_FRAME + 2, "接收缓冲区溢出");
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    fn pair() -> (TransportState, TransportState) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let other = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            handshake(&mut socket, &[42; 32], false).unwrap()
        });
        let mut socket = TcpStream::connect(addr).unwrap();
        let client = handshake(&mut socket, &[42; 32], true).unwrap();
        (client, other.join().unwrap())
    }

    #[test]
    fn encrypted_transport_handles_fragmentation_and_both_directions() {
        let (mut client, mut server) = pair();
        let expected = Message::Clipboard("中文剪贴板 🖱".into());
        let frame = encode(&mut client, &expected).unwrap();
        assert!(!String::from_utf8_lossy(&frame).contains("剪贴板"));
        let mut incoming = Vec::new();
        for byte in &frame[..frame.len() - 1] {
            incoming.push(*byte);
            assert!(
                decode_available(&mut server, &mut incoming)
                    .unwrap()
                    .is_empty()
            );
        }
        incoming.push(*frame.last().unwrap());
        assert_eq!(
            decode_available(&mut server, &mut incoming).unwrap(),
            vec![expected]
        );
        let mut reply = encode(&mut server, &Message::Ping).unwrap();
        assert_eq!(
            decode_available(&mut client, &mut reply).unwrap(),
            vec![Message::Ping]
        );
    }

    #[test]
    fn modified_ciphertext_is_rejected() {
        let (mut client, mut server) = pair();
        let mut frame = encode(&mut client, &Message::Ping).unwrap();
        *frame.last_mut().unwrap() ^= 1;
        assert!(decode_available(&mut server, &mut frame).is_err());
    }

    #[test]
    fn matching_short_passphrases_establish_an_encrypted_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let other = thread::spawn(move || {
            let key = crate::config::parse_pairing_key("1234").unwrap();
            let (mut socket, _) = listener.accept().unwrap();
            handshake(&mut socket, &key, false).unwrap()
        });
        let key = crate::config::parse_pairing_key("1234").unwrap();
        let mut socket = TcpStream::connect(addr).unwrap();
        let mut client = handshake(&mut socket, &key, true).unwrap();
        let mut server = other.join().unwrap();
        let expected = Message::Move { dx: 8, dy: -3 };
        let mut frame = encode(&mut client, &expected).unwrap();
        assert_eq!(
            decode_available(&mut server, &mut frame).unwrap(),
            vec![expected]
        );
    }

    #[test]
    fn different_pairing_keys_fail_authentication() {
        let server_key = crate::config::parse_pairing_key("1234").unwrap();
        let client_key = crate::config::parse_pairing_key("1235").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let other = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            handshake(&mut socket, &server_key, false).is_err()
        });
        let mut socket = TcpStream::connect(addr).unwrap();
        assert!(handshake(&mut socket, &client_key, true).is_err());
        assert!(other.join().unwrap());
    }
}
