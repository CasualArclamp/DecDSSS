// made by CasualArclamp
//! CRCs for headers (CRC-16) and payload frames (CRC-32).

/// CRC-16/CCITT-FALSE: polynomial 0x1021, init 0xFFFF, no reflection, no
/// final XOR. Check value of "123456789" is 0x29B1.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// CRC-32 (IEEE 802.3, reflected 0xEDB88320, init and final XOR 0xFFFFFFFF).
/// Check value of "123456789" is 0xCBF43926.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Bytes to bits, most significant first.
pub fn bits_of(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1))
        .collect()
}

/// Bits (most significant first) to bytes; a short last byte is padded with 0.
pub fn bytes_of(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |a, (i, &b)| a | (b << (7 - i)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_values() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn bits_round_trip() {
        let b = [0xA5u8, 0x01, 0xFF];
        assert_eq!(bytes_of(&bits_of(&b)), b);
        assert_eq!(&bits_of(&[0x80])[..2], &[1, 0]);
    }
}
