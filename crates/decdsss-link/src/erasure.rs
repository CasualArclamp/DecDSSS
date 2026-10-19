// made by CasualArclamp
// Ported from COFDMtv's `cofdmtv-core/src/coding/crs.rs` (same author), itself a port of
// aicodix/code `cauchy_reed_solomon_erasure_coding.hh` and `galois_field.hh` (0BSD).
//! Cauchy Reed–Solomon erasure coding over GF(2^16).
//!
//! A message of `count` data blocks is spread over any number of coded blocks, each
//! identified by an `ident` ≥ `count`; any `count` distinct coded blocks give back the
//! data. Coded block `x` is Σ_j d_j / (x + j) (a Cauchy matrix; `+` is XOR in the
//! field). Decoding multiplies with the inverse of the Cauchy submatrix of the received
//! idents, which has a closed form. Symbols are 16-bit little-endian words of the blocks'
//! bytes, so blocks are an even number of bytes.
//!
//! The data blocks themselves go along as blocks `0…count−1` (a systematic code): every
//! square submatrix of a Cauchy matrix is regular, so any `count` blocks, data or
//! coded, give back the data. [`Decoder`] takes the data blocks in hand out of the coded
//! ones and solves for the rest, so the cost follows the number of blocks *missing*.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// x^16 + x^12 + x^3 + x + 1.
const POLY: u32 = 0b1_0001_0000_0000_1011;
const Q: usize = 1 << 16;
/// The multiplicative group's order, also the "log of zero" sentinel.
const N: u32 = (Q - 1) as u32;

/// The most data blocks one message may have (coded idents must stay below 2^16).
pub const MAX_BLOCKS: usize = 30_000;

struct Tables {
    log: Vec<u16>,
    exp: Vec<u16>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut log = vec![0u16; Q];
        let mut exp = vec![0u16; Q];
        exp[N as usize] = 0;
        log[0] = N as u16;
        let mut a: u32 = 1;
        for i in 0..N {
            exp[i as usize] = a as u16;
            log[a as usize] = i as u16;
            a <<= 1;
            if a & (Q as u32) != 0 {
                a ^= POLY;
            }
        }
        debug_assert_eq!(a, 1);
        Tables { log, exp }
    })
}

/// An element in log form (`i` stands for α^i).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Index(u32);

impl Index {
    const ONE: Index = Index(0);

    fn of(v: u16) -> Index {
        debug_assert!(v != 0);
        Index(u32::from(tables().log[v as usize]))
    }

    fn mul(self, b: Index) -> Index {
        let s = self.0 + b.0;
        Index(if s >= N { s - N } else { s })
    }

    fn div(self, b: Index) -> Index {
        Index(if self.0 < b.0 {
            self.0 + N - b.0
        } else {
            self.0 - b.0
        })
    }

    fn rcp(self) -> Index {
        Index(if self.0 == 0 { 0 } else { N - self.0 })
    }

    /// `self · v` for a value `v`.
    #[inline]
    fn times(self, v: u16, t: &Tables) -> u16 {
        if v == 0 {
            return 0;
        }
        let s = self.0 + u32::from(t.log[v as usize]);
        t.exp[(if s >= N { s - N } else { s }) as usize]
    }
}

/// c (+)= b · a over 16-bit little-endian symbols.
fn multiply_accumulate(c: &mut [u8], a: &[u8], b: Index, init: bool) {
    let t = tables();
    let (cs, _) = c.as_chunks_mut::<2>();
    let (a_s, _) = a.as_chunks::<2>();
    for (cc, aa) in cs.iter_mut().zip(a_s) {
        let p = b.times(u16::from_le_bytes(*aa), t);
        let v = if init { p } else { p ^ u16::from_le_bytes(*cc) };
        *cc = v.to_le_bytes();
    }
}

/// a_ij = 1 / (x_i + y_j) with row `i` (an ident) and column `j` (a data block).
fn cauchy(i: u16, j: u16) -> Index {
    Index::of(i ^ j).rcp()
}

/// Encodes coded block `ident` (≥ the number of data blocks) of the data blocks in
/// `data` (`count` blocks of `block_bytes` each, `block_bytes` even).
pub fn encode(data: &[u8], ident: u16, block_bytes: usize) -> Vec<u8> {
    let count = data.len() / block_bytes;
    assert!(
        block_bytes.is_multiple_of(2) && count >= 1 && usize::from(ident) >= count,
        "bad erasure-coding arguments"
    );
    let mut block = vec![0u8; block_bytes];
    for (k, d) in data.chunks_exact(block_bytes).enumerate() {
        multiply_accumulate(&mut block, d, cauchy(ident, k as u16), k == 0);
    }
    block
}

/// Adds data block `column`'s share to coded block `ident`: `block += data / (ident +
/// column)`. Adding is subtracting, so this also takes a data block in hand out of a
/// coded one.
fn accumulate(block: &mut [u8], data: &[u8], ident: u16, column: u16) {
    multiply_accumulate(block, data, cauchy(ident, column), false);
}

/// Recovers the data blocks `columns` from as many coded blocks that hold only their
/// shares: `blocks` one after the other, `idents` their identifiers. Writes the blocks
/// of `columns`, in that order, to `data`.
fn decode_columns(
    data: &mut [u8],
    blocks: &[u8],
    idents: &[u16],
    columns: &[u16],
    block_bytes: usize,
) {
    let n = idents.len();
    for i in 0..n {
        // The row of the inverse Cauchy matrix for data block `columns[i]` (closed form,
        // as in the original's `inverse_cauchy_matrix`, there with columns 0…n−1).
        let col_i = columns[i];
        let (mut row_num, mut row_den) = (Index::ONE, Index::ONE);
        for k in 0..n {
            row_num = row_num.mul(Index::of(idents[k] ^ col_i));
            if k != i {
                row_den = row_den.mul(Index::of(col_i ^ columns[k]));
            }
        }
        let out = &mut data[i * block_bytes..(i + 1) * block_bytes];
        for j in 0..n {
            let (mut num, mut den) = (row_num, row_den);
            for k in 0..n {
                num = num.mul(Index::of(idents[j] ^ columns[k]));
                if k != j {
                    den = den.mul(Index::of(idents[j] ^ idents[k]));
                }
            }
            let b = num.div(Index::of(idents[j] ^ col_i).mul(den));
            multiply_accumulate(
                out,
                &blocks[j * block_bytes..(j + 1) * block_bytes],
                b,
                j == 0,
            );
        }
    }
}

/// Collects blocks of one message (data blocks `0…count−1` and coded ones from
/// `count` up) and recovers the data once `count` distinct blocks are in.
pub struct Decoder {
    count: usize,
    block_bytes: usize,
    blocks: BTreeMap<u16, Vec<u8>>,
}

impl Decoder {
    pub fn new(count: usize, block_bytes: usize) -> Decoder {
        assert!(count >= 1 && block_bytes.is_multiple_of(2));
        Decoder {
            count,
            block_bytes,
            blocks: BTreeMap::new(),
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn block_bytes(&self) -> usize {
        self.block_bytes
    }

    /// Takes a block; false if it was already held (or is the wrong size).
    pub fn push(&mut self, ident: u16, block: &[u8]) -> bool {
        if block.len() != self.block_bytes || self.blocks.contains_key(&ident) {
            return false;
        }
        self.blocks.insert(ident, block.to_vec());
        true
    }

    /// Distinct blocks held (any `count` of them suffice).
    pub fn have(&self) -> usize {
        self.blocks.len()
    }

    /// Which of the data blocks are held as themselves.
    pub fn data_held(&self) -> Vec<bool> {
        (0..self.count as u16)
            .map(|i| self.blocks.contains_key(&i))
            .collect()
    }

    /// The data (`count` blocks one after the other) if enough blocks are in.
    pub fn solve(&self) -> Option<Vec<u8>> {
        if self.blocks.len() < self.count {
            return None;
        }
        let bb = self.block_bytes;
        let mut out = vec![0u8; self.count * bb];
        let missing: Vec<u16> = (0..self.count as u16)
            .filter(|i| !self.blocks.contains_key(i))
            .collect();
        for (i, b) in self.blocks.range(..self.count as u16) {
            out[usize::from(*i) * bb..][..bb].copy_from_slice(b);
        }
        if missing.is_empty() {
            return Some(out);
        }
        // Take the data in hand out of as many coded blocks as data is missing.
        let coded: Vec<u16> = self
            .blocks
            .range(self.count as u16..)
            .map(|(i, _)| *i)
            .take(missing.len())
            .collect();
        debug_assert_eq!(coded.len(), missing.len());
        let mut rows = Vec::with_capacity(missing.len() * bb);
        for &id in &coded {
            let mut b = self.blocks[&id].clone();
            for (&j, held) in self.blocks.range(..self.count as u16) {
                accumulate(&mut b, held, id, j);
            }
            rows.extend(b);
        }
        let mut solved = vec![0u8; missing.len() * bb];
        decode_columns(&mut solved, &rows, &coded, &missing, bb);
        for (i, &j) in missing.iter().enumerate() {
            out[usize::from(j) * bb..][..bb].copy_from_slice(&solved[i * bb..][..bb]);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(count: usize, bytes: usize) -> Vec<u8> {
        let mut x = 0x1234_5678u32;
        (0..count * bytes)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 8) as u8
            })
            .collect()
    }

    fn block_of(d: &[u8], bytes: usize, count: usize, id: u16) -> Vec<u8> {
        if usize::from(id) < count {
            d[usize::from(id) * bytes..][..bytes].to_vec()
        } else {
            encode(d, id, bytes)
        }
    }

    #[test]
    fn any_count_blocks_recover_the_data() {
        let (count, bytes) = (6usize, 64usize);
        let d = data(count, bytes);
        for ids in [
            vec![0u16, 1, 2, 3, 4, 5],
            vec![6, 7, 8, 9, 10, 11],
            vec![0, 7, 2, 9, 4, 11],
            vec![50, 1, 33, 4, 20_000, 5],
        ] {
            let mut dec = Decoder::new(count, bytes);
            for &id in &ids {
                assert!(dec.push(id, &block_of(&d, bytes, count, id)));
            }
            assert_eq!(dec.solve().as_deref(), Some(&d[..]), "{ids:?}");
        }
    }

    #[test]
    fn fewer_than_count_is_not_enough_and_duplicates_do_not_count() {
        let (count, bytes) = (4usize, 32usize);
        let d = data(count, bytes);
        let mut dec = Decoder::new(count, bytes);
        for id in [0u16, 5, 6] {
            dec.push(id, &block_of(&d, bytes, count, id));
        }
        assert!(!dec.push(5, &block_of(&d, bytes, count, 5)));
        assert_eq!(dec.have(), 3);
        assert!(dec.solve().is_none());
        dec.push(9, &block_of(&d, bytes, count, 9));
        assert_eq!(dec.solve().as_deref(), Some(&d[..]));
    }

    #[test]
    fn a_big_message_with_few_missing_decodes() {
        // 300 blocks, 7 of them lost, replaced by 7 coded: the cost follows the 7.
        let (count, bytes) = (300usize, 64usize);
        let d = data(count, bytes);
        let lost = [3u16, 40, 41, 99, 150, 200, 299];
        let mut dec = Decoder::new(count, bytes);
        for id in 0..count as u16 {
            if !lost.contains(&id) {
                dec.push(id, &block_of(&d, bytes, count, id));
            }
        }
        for k in 0..7u16 {
            dec.push(
                count as u16 + 11 * k,
                &block_of(&d, bytes, count, count as u16 + 11 * k),
            );
        }
        assert_eq!(dec.solve().as_deref(), Some(&d[..]));
        assert_eq!(dec.data_held().iter().filter(|b| !**b).count(), 7);
    }

    #[test]
    fn the_field_tables_are_consistent() {
        let t = tables();
        for v in [1u16, 2, 3, 255, 4096, 65535] {
            assert_eq!(t.exp[usize::from(t.log[usize::from(v)])], v);
        }
    }
}
