// made by CasualArclamp
//! Quasi-cyclic IRA LDPC codes: a base matrix of Z×Z circulant shifts whose
//! parity part is a block "zig-zag" accumulator, so encoding is a running XOR.
//!
//! H = [ H_info | H_parity ], with block row i of H_parity holding identity
//! blocks at parity columns i−1 and i. Encoding then reads
//! p_0 = λ_0, p_i = p_{i−1} ⊕ λ_i where λ_i is block row i of H_info·u.
//!
//! The info part is designed here, not copied: column weights are chosen per
//! rate, rows are filled evenly, and shifts are drawn from a seeded PCG32 so
//! that the lifted graph has no 4-cycles (the standard circulant condition,
//! checked against every earlier column including the accumulator). The result
//! is deterministic; `tests::tables_are_pinned` guards it against drift.
//!
//! Sign convention: a positive LLR means the bit is 0.

use decdsss_core::CodeRate;
use decdsss_dsp::rng::Rng;
use std::sync::OnceLock;

/// Design seeds, picked by `tests::pick_seed` as the best of 40 on simulated
/// frame errors.
const PAYLOAD_SEEDS: [u64; 3] = [2321, 5393, 1041];
const HEADER_SEED: u64 = 1809;

/// A code: its base matrix (shift, or −1 for a null block) and the lifting.
#[derive(Debug)]
pub struct Code {
    pub name: &'static str,
    pub z: usize,
    pub mb: usize,
    pub nb: usize,
    pub base: Vec<Vec<i16>>,
    /// Non-null blocks of each block row: (block column, shift).
    rows: Vec<Vec<(usize, usize)>>,
}

impl Code {
    pub fn n(&self) -> usize {
        self.nb * self.z
    }
    pub fn k(&self) -> usize {
        (self.nb - self.mb) * self.z
    }
    pub fn rate(&self) -> f64 {
        self.k() as f64 / self.n() as f64
    }
    fn kb(&self) -> usize {
        self.nb - self.mb
    }

    fn from_base(name: &'static str, z: usize, base: Vec<Vec<i16>>) -> Code {
        let mb = base.len();
        let nb = base[0].len();
        let rows = base
            .iter()
            .map(|r| {
                r.iter()
                    .enumerate()
                    .filter(|&(_, &s)| s >= 0)
                    .map(|(j, &s)| (j, s as usize))
                    .collect()
            })
            .collect();
        Code {
            name,
            z,
            mb,
            nb,
            base,
            rows,
        }
    }

    /// The payload code for a rate: Z = 48, 24 block columns, so n = 1152 and
    /// k = 576 / 768 / 864 bits.
    pub fn payload(rate: CodeRate) -> &'static Code {
        static CODES: OnceLock<[Code; 3]> = OnceLock::new();
        let codes = CODES.get_or_init(|| {
            [
                design("payload 1/2", 48, 12, 24, PAYLOAD_SEEDS[0]),
                design("payload 2/3", 48, 8, 24, PAYLOAD_SEEDS[1]),
                design("payload 3/4", 48, 6, 24, PAYLOAD_SEEDS[2]),
            ]
        });
        match rate {
            CodeRate::R1_2 => &codes[0],
            CodeRate::R2_3 => &codes[1],
            CodeRate::R3_4 => &codes[2],
        }
    }

    /// The header code: Z = 16, 16 block columns, rate 1/2: n = 256, k = 128.
    pub fn header() -> &'static Code {
        static CODE: OnceLock<Code> = OnceLock::new();
        CODE.get_or_init(|| design_with("header", 16, 8, 16, HEADER_SEED, &header_weights(8, 8)))
    }

    /// Codeword (info followed by parity, one bit a byte) for `info`.
    pub fn encode(&self, info: &[u8]) -> Vec<u8> {
        let (z, kb) = (self.z, self.kb());
        assert_eq!(info.len(), kb * z, "{}: wrong info length", self.name);
        let mut cw = vec![0u8; self.n()];
        cw[..info.len()].copy_from_slice(info);
        let mut prev = vec![0u8; z];
        for (i, row) in self.rows.iter().enumerate() {
            let mut lam = vec![0u8; z];
            for &(j, s) in row.iter().filter(|&&(j, _)| j < kb) {
                for (t, l) in lam.iter_mut().enumerate() {
                    *l ^= info[j * z + (t + s) % z];
                }
            }
            for t in 0..z {
                prev[t] ^= lam[t];
                cw[(kb + i) * z + t] = prev[t];
            }
        }
        cw
    }

    /// Number of unsatisfied parity checks for hard bits.
    pub fn syndrome_weight(&self, bits: &[u8]) -> usize {
        let z = self.z;
        let mut bad = 0;
        for row in &self.rows {
            for t in 0..z {
                let mut x = 0;
                for &(j, s) in row {
                    x ^= bits[j * z + (t + s) % z];
                }
                bad += usize::from(x & 1);
            }
        }
        bad
    }

    /// Variable-node degree histogram of the base matrix: (degree, columns).
    pub fn column_degrees(&self) -> Vec<(usize, usize)> {
        let mut h = std::collections::BTreeMap::new();
        for j in 0..self.nb {
            let d = self.base.iter().filter(|r| r[j] >= 0).count();
            *h.entry(d).or_insert(0) += 1;
        }
        h.into_iter().collect()
    }

    /// FNV-1a over the shifts: pins a design against accidental change.
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for row in &self.base {
            for &s in row {
                h ^= (s + 1) as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        h
    }
}

/// Info-column weights for the payload codes: six columns of weight 5 (capped
/// at the number of block rows), the rest 3. Measured best of the six
/// patterns tried (see `tests::explore`), 0.3–0.4 dB better than weight 3
/// everywhere or a few very heavy columns.
fn weights(kb: usize, mb: usize) -> Vec<usize> {
    (0..kb).map(|j| if j < 6 { mb.min(5) } else { 3 }).collect()
}

/// The header code's weights: the first third of the columns at 6.
fn header_weights(kb: usize, mb: usize) -> Vec<usize> {
    (0..kb)
        .map(|j| if j < kb / 3 { mb.min(6) } else { 3 })
        .collect()
}

/// Designs a code: parity part fixed, info part by seeded search.
fn design(name: &'static str, z: usize, mb: usize, nb: usize, seed: u64) -> Code {
    let w = weights(nb - mb, mb);
    design_with(name, z, mb, nb, seed, &w)
}

fn design_with(name: &'static str, z: usize, mb: usize, nb: usize, seed: u64, w: &[usize]) -> Code {
    for attempt in 0..1000u64 {
        let mut rng = Rng::new(seed + attempt * 7919);
        if let Some(base) = try_design(z, mb, nb, w, &mut rng) {
            return Code::from_base(name, z, base);
        }
    }
    panic!("{name}: no 4-cycle-free design found");
}

fn try_design(z: usize, mb: usize, nb: usize, w: &[usize], rng: &mut Rng) -> Option<Vec<Vec<i16>>> {
    let kb = nb - mb;
    let mut base = vec![vec![-1i16; nb]; mb];
    for i in 0..mb {
        base[i][kb + i] = 0;
        if i > 0 {
            base[i][kb + i - 1] = 0;
        }
    }
    let mut row_weight: Vec<usize> = (0..mb).map(|i| if i > 0 { 2 } else { 1 }).collect();
    // Columns with the highest weights first: they are the hardest to place.
    let mut order: Vec<usize> = (0..kb).collect();
    order.sort_by_key(|&j| std::cmp::Reverse(w[j]));
    // Parity columns are complete from the start; info columns join as placed.
    let mut done: Vec<usize> = (kb..nb).collect();
    for &j in &order {
        // The w[j] lightest rows, ties broken at random.
        let mut rows: Vec<usize> = (0..mb).collect();
        for i in (1..rows.len()).rev() {
            rows.swap(i, rng.next_u32() as usize % (i + 1));
        }
        rows.sort_by_key(|&i| row_weight[i]);
        rows.truncate(w[j]);
        rows.sort_unstable();
        for &i in &rows {
            // Draw shifts until none closes a 4-cycle with a finished column.
            let mut placed = false;
            for _ in 0..4 * z {
                let s = (rng.next_u32() as usize % z) as i16;
                base[i][j] = s;
                if no_four_cycle(&base, z, j, &done) {
                    placed = true;
                    break;
                }
            }
            if !placed {
                return None;
            }
            row_weight[i] += 1;
        }
        done.push(j);
    }
    Some(base)
}

/// True if column `j` makes no 4-cycle with any column in `others`: for two
/// columns sharing block rows a, b the lifted graph has a 4-cycle exactly when
/// s[a][j] − s[a][c] − s[b][j] + s[b][c] ≡ 0 (mod Z).
fn no_four_cycle(base: &[Vec<i16>], z: usize, j: usize, others: &[usize]) -> bool {
    let z = z as i32;
    for &c in others {
        let shared: Vec<usize> = (0..base.len())
            .filter(|&i| base[i][j] >= 0 && base[i][c] >= 0)
            .collect();
        for (x, &a) in shared.iter().enumerate() {
            for &b in &shared[x + 1..] {
                let d = i32::from(base[a][j]) - i32::from(base[a][c]) - i32::from(base[b][j])
                    + i32::from(base[b][c]);
                if d.rem_euclid(z) == 0 {
                    return false;
                }
            }
        }
    }
    true
}

/// What a decode ended with.
#[derive(Clone, Copy, Debug)]
pub struct Outcome {
    pub converged: bool,
    pub iterations: usize,
    pub unsatisfied: usize,
}

/// A layered normalised min-sum decoder for one code. Reusable: `decode` can
/// be called any number of times.
pub struct Decoder<'a> {
    code: &'a Code,
    /// Normalisation of the check message (0.75–0.9 is typical).
    pub alpha: f32,
    post: Vec<f32>,
    msgs: Vec<f32>,
    /// Offset of each block row's messages in `msgs`.
    offs: Vec<usize>,
    hard: Vec<u8>,
}

impl<'a> Decoder<'a> {
    pub fn new(code: &'a Code) -> Decoder<'a> {
        let mut offs = Vec::new();
        let mut total = 0;
        for row in &code.rows {
            offs.push(total);
            total += row.len() * code.z;
        }
        Decoder {
            code,
            alpha: 0.8,
            post: vec![0.0; code.n()],
            msgs: vec![0.0; total],
            offs,
            hard: vec![0; code.n()],
        }
    }

    /// Hard decisions of the last decode (all n bits).
    pub fn bits(&self) -> &[u8] {
        &self.hard
    }

    /// Decodes channel LLRs (n of them) for at most `max_iter` iterations.
    pub fn decode(&mut self, llr: &[f32], max_iter: usize) -> Outcome {
        let code = self.code;
        assert_eq!(llr.len(), code.n());
        self.post.copy_from_slice(llr);
        self.msgs.fill(0.0);
        let z = code.z;
        let mut idx = [0usize; 64];
        let mut q = [0f32; 64];
        let mut unsatisfied = usize::MAX;
        for iter in 1..=max_iter {
            for (i, row) in code.rows.iter().enumerate() {
                let deg = row.len();
                for t in 0..z {
                    let base = self.offs[i] + t * deg;
                    let (mut min1, mut min2, mut arg) = (f32::MAX, f32::MAX, 0);
                    let mut sign = 1.0f32;
                    for (e, &(j, s)) in row.iter().enumerate() {
                        let v = j * z + (t + s) % z;
                        idx[e] = v;
                        q[e] = self.post[v] - self.msgs[base + e];
                        let a = q[e].abs();
                        if q[e] < 0.0 {
                            sign = -sign;
                        }
                        if a < min1 {
                            min2 = min1;
                            min1 = a;
                            arg = e;
                        } else if a < min2 {
                            min2 = a;
                        }
                    }
                    for e in 0..deg {
                        let m = if e == arg { min2 } else { min1 };
                        // Sign of the product of the *other* inputs.
                        let s = if q[e] < 0.0 { -sign } else { sign };
                        let r = self.alpha * m * s;
                        self.msgs[base + e] = r;
                        self.post[idx[e]] = q[e] + r;
                    }
                }
            }
            for (h, &p) in self.hard.iter_mut().zip(&self.post) {
                *h = u8::from(p < 0.0);
            }
            unsatisfied = code.syndrome_weight(&self.hard);
            if unsatisfied == 0 {
                return Outcome {
                    converged: true,
                    iterations: iter,
                    unsatisfied,
                };
            }
        }
        Outcome {
            converged: false,
            iterations: max_iter,
            unsatisfied,
        }
    }
}

/// Result of an AWGN simulation.
#[derive(Clone, Copy, Debug)]
pub struct SimResult {
    pub frames: usize,
    pub frame_errors: usize,
    pub avg_iterations: f64,
}

impl SimResult {
    pub fn fer(&self) -> f64 {
        self.frame_errors as f64 / self.frames as f64
    }
}

/// BPSK over AWGN at `ebn0_db` (Eb per information bit), `frames` random
/// codewords, true-LLR input.
pub fn simulate(code: &Code, ebn0_db: f64, frames: usize, seed: u64, max_iter: usize) -> SimResult {
    let mut rng = Rng::new(seed);
    let mut dec = Decoder::new(code);
    let ebn0 = 10f64.powf(ebn0_db / 10.0);
    let sigma2 = 1.0 / (2.0 * code.rate() * ebn0);
    let sigma = sigma2.sqrt();
    let (mut errors, mut iters) = (0, 0);
    for _ in 0..frames {
        let info: Vec<u8> = (0..code.k()).map(|_| rng.bit()).collect();
        let cw = code.encode(&info);
        let llr: Vec<f32> = cw
            .iter()
            .map(|&b| {
                let y = 1.0 - 2.0 * f64::from(b) + sigma * rng.gauss();
                (2.0 * y / sigma2) as f32
            })
            .collect();
        let o = dec.decode(&llr, max_iter);
        iters += o.iterations;
        if !o.converged || dec.bits()[..code.k()] != info[..] {
            errors += 1;
        }
    }
    SimResult {
        frames,
        frame_errors: errors,
        avg_iterations: iters as f64 / frames as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_codes() -> Vec<&'static Code> {
        let mut v: Vec<&Code> = CodeRate::ALL.iter().map(|&r| Code::payload(r)).collect();
        v.push(Code::header());
        v
    }

    #[test]
    fn tables_are_pinned() {
        // The designs are regenerated from seeds at start-up; a change to the
        // search, the RNG or the seeds would silently make every previously
        // transmitted signal undecodable, so the shifts are fingerprinted.
        let got: Vec<u64> = all_codes().iter().map(|c| c.fingerprint()).collect();
        assert_eq!(
            got,
            [
                0x1e4b_a430_c3ca_4d79,
                0x8ed1_cf08_fdef_3d97,
                0xb451_e744_d14e_dcb5,
                0x4036_4c91_a9a7_1eb5
            ]
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(
            (
                Code::payload(CodeRate::R1_2).n(),
                Code::payload(CodeRate::R1_2).k()
            ),
            (1152, 576)
        );
        assert_eq!(Code::payload(CodeRate::R2_3).k(), 768);
        assert_eq!(Code::payload(CodeRate::R3_4).k(), 864);
        assert_eq!((Code::header().n(), Code::header().k()), (256, 128));
    }

    #[test]
    fn encoder_output_satisfies_every_check() {
        let mut rng = Rng::new(9);
        for c in all_codes() {
            for _ in 0..5 {
                let info: Vec<u8> = (0..c.k()).map(|_| rng.bit()).collect();
                let cw = c.encode(&info);
                assert_eq!(&cw[..c.k()], &info[..]);
                assert_eq!(c.syndrome_weight(&cw), 0, "{}", c.name);
            }
        }
    }

    #[test]
    fn no_four_cycles_in_the_lifted_graph() {
        for c in all_codes() {
            // Brute force: two checks sharing two variables is a 4-cycle.
            let z = c.z;
            let mut adj: Vec<Vec<usize>> = Vec::new();
            for row in &c.rows {
                for t in 0..z {
                    let mut vs: Vec<usize> =
                        row.iter().map(|&(j, s)| j * z + (t + s) % z).collect();
                    vs.sort_unstable();
                    adj.push(vs);
                }
            }
            let mut seen = std::collections::HashSet::new();
            for vs in &adj {
                for (x, &a) in vs.iter().enumerate() {
                    for &b in &vs[x + 1..] {
                        assert!(seen.insert((a, b)), "{}: 4-cycle through {a},{b}", c.name);
                    }
                }
            }
        }
    }

    #[test]
    fn decoder_corrects_a_noisy_codeword() {
        // Well above threshold: every frame decodes.
        for (c, ebn0) in [
            (Code::payload(CodeRate::R1_2), 3.0),
            (Code::payload(CodeRate::R2_3), 3.5),
            (Code::payload(CodeRate::R3_4), 4.0),
            (Code::header(), 4.0),
        ] {
            let r = simulate(c, ebn0, 40, 5, 50);
            assert_eq!(r.frame_errors, 0, "{} at {ebn0} dB: {r:?}", c.name);
        }
    }

    #[test]
    fn corrects_a_burst_of_erasures() {
        // 6 % of the bits erased (LLR 0) in one run.
        let c = Code::payload(CodeRate::R1_2);
        let mut rng = Rng::new(11);
        let info: Vec<u8> = (0..c.k()).map(|_| rng.bit()).collect();
        let cw = c.encode(&info);
        let mut llr: Vec<f32> = cw
            .iter()
            .map(|&b| 8.0 * (1.0 - 2.0 * f32::from(b)))
            .collect();
        for l in &mut llr[300..370] {
            *l = 0.0;
        }
        let mut d = Decoder::new(c);
        assert!(d.decode(&llr, 50).converged);
        assert_eq!(&d.bits()[..c.k()], &info[..]);
    }

    /// Seed selection; run with
    /// `cargo test -p decdsss-fec --release -- --ignored --nocapture pick_seed`.
    /// Prints the five best of 40 design seeds per code.
    #[test]
    #[ignore]
    fn pick_seed() {
        struct Target {
            name: &'static str,
            z: usize,
            mb: usize,
            nb: usize,
            w: Vec<usize>,
            ebn0: f64,
            frames: usize,
        }
        let t = |name, z, mb, nb, w, ebn0, frames| Target {
            name,
            z,
            mb,
            nb,
            w,
            ebn0,
            frames,
        };
        let targets = [
            t("payload 1/2", 48, 12, 24, weights(12, 12), 2.0, 800),
            t("payload 2/3", 48, 8, 24, weights(16, 8), 2.6, 800),
            t("payload 3/4", 48, 6, 24, weights(18, 6), 3.2, 1500),
            t("header", 16, 8, 16, header_weights(8, 8), 2.6, 3000),
        ];
        for Target {
            name,
            z,
            mb,
            nb,
            w,
            ebn0,
            frames,
        } in targets
        {
            let mut res: Vec<(usize, u64)> = (0..40u64)
                .map(|i| {
                    let seed = 0x100 * (i + 1) + 0x11;
                    let c = design_with("x", z, mb, nb, seed, &w);
                    (simulate(&c, ebn0, frames, 1234, 50).frame_errors, seed)
                })
                .collect();
            res.sort();
            println!("{name} at {ebn0} dB, {frames} frames: best {:?}", &res[..5]);
        }
    }

    /// Prints FER at a few Eb/N0 points; run with
    /// `cargo test -p decdsss-fec --release -- --ignored --nocapture sweep`.
    #[test]
    #[ignore]
    fn sweep() {
        for c in all_codes() {
            println!(
                "{} n={} k={} degrees {:?} fp {:016x}",
                c.name,
                c.n(),
                c.k(),
                c.column_degrees(),
                c.fingerprint()
            );
            let base = match c.name {
                "payload 1/2" => 0.8,
                "payload 2/3" => 1.9,
                "payload 3/4" => 2.6,
                _ => 1.8,
            };
            for step in 0..5 {
                let e = base + 0.4 * step as f64;
                let r = simulate(c, e, 400, 21, 50);
                println!(
                    "  Eb/N0 {e:4.1} dB  FER {:.4}  iters {:.1}",
                    r.fer(),
                    r.avg_iterations
                );
            }
        }
    }
}
