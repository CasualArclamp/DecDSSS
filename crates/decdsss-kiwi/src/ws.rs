// made by CasualArclamp
//! The little of WebSocket (RFC 6455) a KiwiSDR needs, client side, over plain
//! TCP: the HTTP upgrade, masked text frames out, and frames in (binary, text,
//! ping, close; continuation frames joined). A Kiwi serves `ws://` on its own
//! port, so there is no TLS here.
//!
//! The server's `Sec-WebSocket-Accept` is not checked against the key (that
//! needs SHA-1 and guards only against a server that is not a WebSocket server
//! at all, which the protocol's first message shows anyway): a `101` answer is
//! taken as the upgrade.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A message off the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    /// The server closed the connection (its status code, if it gave one).
    Close(Option<u16>),
}

#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("the server answered the upgrade with: {0}")]
    Refused(String),
    /// A redirect (301, 307, 308) to this `host:port/path`.
    #[error("redirected to {0}")]
    Redirect(String),
    #[error("bad frame from the server: {0}")]
    Protocol(&'static str),
}

/// Standard base64 (RFC 4648) of `data`, with padding.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A cheap source of the bytes WebSocket wants random (the key and the masks):
/// they guard caches and proxies against confusion, not secrets, so a
/// SplitMix64 seeded from the clock does.
pub struct Mixer(u64);

impl Mixer {
    pub fn from_clock() -> Mixer {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Mixer(t ^ (u64::from(std::process::id()) << 32))
    }

    pub fn next_u64(&mut self) -> u64 {
        // SplitMix64 (Steele, Lea and Flood, 2014).
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A frame from client to server: FIN set, the payload masked with `mask`.
pub fn client_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 14);
    f.push(0x80 | (opcode & 0x0F));
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n <= 0xFFFF {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, &b)| b ^ mask[i % 4]));
    f
}

/// A frame as parsed: FIN, opcode, payload, and the bytes it took.
pub type Frame = (bool, u8, Vec<u8>, usize);

/// One frame parsed off the front of `buf`: (FIN, opcode, payload, bytes used),
/// or `None` if `buf` does not yet hold a whole frame. A server's frames are
/// not masked, but a masked one is unmasked all the same.
pub fn parse_frame(buf: &[u8]) -> Result<Option<Frame>, WsError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0F;
    let masked = buf[1] & 0x80 != 0;
    let (len, mut at) = match buf[1] & 0x7F {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u64::from(u16::from_be_bytes([buf[2], buf[3]])), 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[2..10]);
            (u64::from_be_bytes(b), 10)
        }
        n => (u64::from(n), 2),
    };
    if len > 64 << 20 {
        return Err(WsError::Protocol("a frame over 64 MB"));
    }
    let mask = if masked {
        if buf.len() < at + 4 {
            return Ok(None);
        }
        let m = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        at += 4;
        Some(m)
    } else {
        None
    };
    let end = at + len as usize;
    if buf.len() < end {
        return Ok(None);
    }
    let mut payload = buf[at..end].to_vec();
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Ok(Some((fin, opcode, payload, end)))
}

/// A client connection.
pub struct Socket {
    tcp: TcpStream,
    /// Bytes read but not yet made into frames.
    pending: Vec<u8>,
    /// A message arriving in pieces (continuation frames): its opcode and bytes.
    partial: Option<(u8, Vec<u8>)>,
    mixer: Mixer,
}

impl Socket {
    /// Opens `ws://host:port/path`. `timeout` bounds the connect and the
    /// upgrade; reads after it wait at most `read_timeout` (then
    /// [`Socket::read`] returns `None`, so a caller can do other work).
    pub fn connect(
        host: &str,
        port: u16,
        path: &str,
        timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Socket, WsError> {
        let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no address for {host}"))
        })?;
        let tcp = TcpStream::connect_timeout(&addr, timeout)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(timeout))?;
        let mut mixer = Mixer::from_clock();
        let mut key = [0u8; 16];
        key[..8].copy_from_slice(&mixer.next_u64().to_le_bytes());
        key[8..].copy_from_slice(&mixer.next_u64().to_le_bytes());
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            base64(&key)
        );
        let mut s = Socket {
            tcp,
            pending: Vec::new(),
            partial: None,
            mixer,
        };
        s.tcp.write_all(request.as_bytes())?;
        // The answer's head, up to the blank line; anything after it is frames.
        let mut head = Vec::new();
        let mut byte = [0u8; 1024];
        let split = loop {
            if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if head.len() > 16 * 1024 {
                return Err(WsError::Refused("an endless HTTP header".into()));
            }
            let n = s.tcp.read(&mut byte)?;
            if n == 0 {
                return Err(WsError::Refused("the connection closed".into()));
            }
            head.extend_from_slice(&byte[..n]);
        };
        s.pending = head[split..].to_vec();
        let text = String::from_utf8_lossy(&head[..split]).to_string();
        let status = text.lines().next().unwrap_or("").to_string();
        let code = status.split_whitespace().nth(1).unwrap_or("");
        match code {
            "101" => {}
            "301" | "302" | "307" | "308" => {
                let to = text
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("location")
                            .then(|| v.trim().to_string())
                    })
                    .unwrap_or_default();
                return Err(WsError::Redirect(to));
            }
            _ => return Err(WsError::Refused(status)),
        }
        s.tcp.set_read_timeout(Some(read_timeout))?;
        Ok(s)
    }

    /// Sends a text message.
    pub fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        let mask = (self.mixer.next_u64() as u32).to_le_bytes();
        self.tcp
            .write_all(&client_frame(0x1, text.as_bytes(), mask))?;
        Ok(())
    }

    /// The next whole message, or `None` if none came within the read timeout.
    /// Pings are answered here.
    pub fn read(&mut self) -> Result<Option<Message>, WsError> {
        loop {
            while let Some((fin, opcode, payload, used)) = parse_frame(&self.pending)? {
                self.pending.drain(..used);
                match opcode {
                    0x8 => {
                        let code = (payload.len() >= 2)
                            .then(|| u16::from_be_bytes([payload[0], payload[1]]));
                        return Ok(Some(Message::Close(code)));
                    }
                    0x9 => {
                        let mask = (self.mixer.next_u64() as u32).to_le_bytes();
                        self.tcp.write_all(&client_frame(0xA, &payload, mask))?;
                    }
                    0xA => {}
                    0x0 => {
                        let Some((op, mut acc)) = self.partial.take() else {
                            return Err(WsError::Protocol(
                                "a continuation with nothing to continue",
                            ));
                        };
                        acc.extend_from_slice(&payload);
                        if fin {
                            return Ok(Some(whole(op, acc)));
                        }
                        self.partial = Some((op, acc));
                    }
                    0x1 | 0x2 => {
                        if fin {
                            return Ok(Some(whole(opcode, payload)));
                        }
                        self.partial = Some((opcode, payload));
                    }
                    _ => return Err(WsError::Protocol("an unknown opcode")),
                }
            }
            let mut chunk = [0u8; 16 * 1024];
            match self.tcp.read(&mut chunk) {
                Ok(0) => return Ok(Some(Message::Close(None))),
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Says goodbye (a close frame), best effort.
    pub fn close(&mut self) {
        let mask = (self.mixer.next_u64() as u32).to_le_bytes();
        let _ = self
            .tcp
            .write_all(&client_frame(0x8, &1000u16.to_be_bytes(), mask));
        let _ = self.tcp.shutdown(std::net::Shutdown::Both);
    }
}

fn whole(opcode: u8, payload: Vec<u8>) -> Message {
    if opcode == 0x1 {
        Message::Text(payload)
    } else {
        Message::Binary(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_examples() {
        // RFC 4648 section 10.
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
        // RFC 6455's example key is 16 bytes: 24 characters.
        assert_eq!(base64(b"the sample nonce"), "dGhlIHNhbXBsZSBub25jZQ==");
    }

    #[test]
    fn a_client_frame_parses_back_at_every_length_form() {
        for n in [0usize, 5, 125, 126, 300, 65_535, 65_536, 70_000] {
            let payload: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
            let f = client_frame(0x2, &payload, [1, 2, 3, 4]);
            let (fin, op, back, used) = parse_frame(&f).unwrap().unwrap();
            assert!(fin);
            assert_eq!((op, used), (0x2, f.len()));
            assert_eq!(back, payload, "length {n}");
            // A frame cut short is not a frame yet.
            assert!(parse_frame(&f[..f.len() - 1]).unwrap().is_none());
        }
    }
}
