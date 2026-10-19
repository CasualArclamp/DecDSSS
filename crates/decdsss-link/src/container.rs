// made by CasualArclamp
//! The file container: `"DS" | name_len | name | length (u32, big endian) |
//! data | CRC-32 of data`. The frames carry this stream, zero-padded to a
//! whole number of frames; the length says where it ends.

use decdsss_fec::crc::crc32;

const MAGIC: &[u8; 2] = b"DS";

/// Longest file name stored.
pub const NAME_MAX: usize = 100;

/// Wraps a file. The name is cut to `NAME_MAX` bytes and stripped of any
/// directory part.
pub fn pack(name: &str, data: &[u8]) -> Vec<u8> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut name: Vec<u8> = base.bytes().take(NAME_MAX).collect();
    // Keep it valid UTF-8 if the cut fell inside a character.
    while std::str::from_utf8(&name).is_err() {
        name.pop();
    }
    let mut v = Vec::with_capacity(data.len() + name.len() + 11);
    v.extend_from_slice(MAGIC);
    v.push(name.len() as u8);
    v.extend_from_slice(&name);
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    v.extend_from_slice(&crc32(data).to_be_bytes());
    v
}

/// Total bytes of the container whose first bytes are `head`, if `head` holds
/// enough of the front to say.
pub fn total_len(head: &[u8]) -> Option<usize> {
    if head.len() < 3 || &head[..2] != MAGIC {
        return None;
    }
    let n = usize::from(head[2]);
    let len: [u8; 4] = head.get(3 + n..7 + n)?.try_into().ok()?;
    Some(7 + n + u32::from_be_bytes(len) as usize + 4)
}

/// A container's front: its name, the bytes of data in `head` so far (cut at
/// the declared length) and the declared length. `None` while the name and
/// length are not all in.
pub fn peek(head: &[u8]) -> Option<(String, &[u8], usize)> {
    if head.len() < 3 || &head[..2] != MAGIC {
        return None;
    }
    let n = usize::from(head[2]);
    let len: [u8; 4] = head.get(3 + n..7 + n)?.try_into().ok()?;
    let len = u32::from_be_bytes(len) as usize;
    let name = std::str::from_utf8(&head[3..3 + n]).ok()?.to_string();
    let have = &head[7 + n..];
    Some((name, &have[..have.len().min(len)], len))
}

/// Unwraps a container (trailing padding is ignored). `None` if it is cut
/// short, not a container, or fails its CRC.
pub fn unpack(bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    let total = total_len(bytes)?;
    let bytes = bytes.get(..total)?;
    let n = usize::from(bytes[2]);
    let name = std::str::from_utf8(&bytes[3..3 + n]).ok()?.to_string();
    let data = &bytes[7 + n..total - 4];
    let crc = u32::from_be_bytes(bytes[total - 4..].try_into().ok()?);
    (crc32(data) == crc).then(|| (name, data.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_padding() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let mut c = pack("pics/holiday.webp", &data);
        let len = c.len();
        c.extend_from_slice(&[0; 40]);
        assert_eq!(total_len(&c[..20]), Some(len));
        assert_eq!(unpack(&c), Some(("holiday.webp".to_string(), data)));
    }

    #[test]
    fn peek_gives_the_front_before_the_rest_is_in() {
        let data: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let c = pack("p.jpg", &data);
        assert_eq!(peek(&c[..9]), None, "name in, length not yet");
        let (name, have, len) = peek(&c[..50]).unwrap();
        assert_eq!((name.as_str(), len), ("p.jpg", 300));
        assert_eq!(have, &data[..have.len()]);
        assert_eq!(have.len(), 50 - 12);
        // The CRC and padding after the data are not data.
        let mut whole = c.clone();
        whole.extend_from_slice(&[0; 20]);
        assert_eq!(peek(&whole).unwrap().1, &data[..]);
    }

    #[test]
    fn rejects_damage_and_truncation() {
        let c = pack("a.txt", b"hello world");
        let mut bad = c.clone();
        bad[10] ^= 1;
        assert_eq!(unpack(&bad), None);
        assert_eq!(unpack(&c[..c.len() - 1]), None);
        assert_eq!(unpack(b"XX\x00"), None);
        assert_eq!(total_len(b"DS"), None);
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary() {
        let name = "é".repeat(80); // 160 bytes
        let (n, _) = unpack(&pack(&name, b"x")).unwrap();
        assert!(n.len() <= NAME_MAX && n.chars().all(|c| c == 'é'));
    }
}
