<!-- made by CasualArclamp -->
# DecDSSS — design

A narrowband spread-spectrum modem for amateur radio: direct-sequence and
code-shift-keyed spreading inside an SSB voice channel (500 Hz, 2.4 kHz or
3 kHz), carrying digital voice, pictures and files, with a receiver that shows
everything it is doing — acquisition surface, chip-phase correlation, rake
fingers, code tracking, constellations — in the style of DecSAT's DVB-CID card.

This file holds the agreed scope, the air interface, the crate layout and the
milestones. Update it when a decision changes.

## Decisions (Rory, 2026-10-19)

| Question | Answer |
|---|---|
| Payloads | Digital voice (Codec2), SSTV-style pictures, files. No text/chat mode. |
| Spreading | Both families as modes: DS-PSK with a spreading-factor ladder, and M-ary code-shift keying (CSK). |
| Channels | HF SSB skywave; sound card / virtual cable first. |
| Bandwidths | 2.4 kHz (300–2700 Hz) and 3 kHz (300–3300 Hz), plus a 500 Hz weak-signal profile. |
| Station ID | Callsign in the header only (no CW ID). |
| Code use | CDMA: a common *call code* for preambles and headers, then data on a code derived from the station's callsign; the receiver despreads every station it finds at once. |
| Radio I/O | Sound card with VOX/manual PTT; CAT keying for IC-7300 / FT-710 (from voicemodem); WAV and IQ files in and out. |
| Receiver panels | DVB-CID's set, plus acquisition heatmap, rake/multipath profile, waterfall + PSD against the noise floor, chip scope. |
| Link | Both: one-way broadcast with FEC, and an ARQ connected mode for files. |
| FEC | LDPC with a time interleaver. |
| First | The modem and its panels, proven on file transfer through WAV and VB-Audio cables; then pictures; then voice. |
| GitHub | `CasualArclamp/DecDSSS`, private for now. |
| Stack | Rust workspace like DecSAT: egui GUI + CLI, portable exes, CI on Actions, GPL-3.0-or-later. |

**Licence conditions are Rory's call.** Some administrations restrict spread
spectrum below VHF (US Part 97 allows SS emissions only on 222 MHz and up,
§97.305(c)). DecDSSS never keys a transmitter on its own.

## Air interface

### Profiles

Chips are root-raised-cosine shaped with roll-off α = 0.25, so a profile
occupies exactly `chip rate × 1.25`. The chip rates divide 48 kHz, so a
transmitter at 48 kHz runs an integer number of samples per chip.

| Profile | Chip rate | Centre | Occupies | Samples/chip @ 48 kHz | Chip length |
|---|---|---|---|---|---|
| `500` | 400 chip/s | 1500 Hz | 1250–1750 Hz | 120 | 2.5 ms |
| `2k4` | 1920 chip/s | 1500 Hz | 300–2700 Hz | 25 | 0.52 ms |
| `3k` | 2400 chip/s | 1800 Hz | 300–3300 Hz | 20 | 0.42 ms |

On `2k4` and `3k`, a chip is short against HF delay spreads (0.5–3 ms), so
multipath resolves into separate paths a rake receiver can combine. On `500`,
it mostly does not (flat fading).

### Chips and codes

The transmitted chip stream is complex: every chip is the modulation symbol
times a QPSK scrambling chip `(±1 ± j)/√2` drawn from two Gold sequences, one
for I and one for Q. A Gold family of degree 15 gives 32 769 sequences.
Index 0 is the **call code**, shared by everyone. A station's **own code** is
picked by a hash of its callsign. The scrambler restarts at each frame
boundary, so a receiver that has the frame timing also has the code phase.

Over the scrambler, the two families modulate differently:

- **DS-PSK** — a QPSK data symbol held for SF chips, with SF in {4, 8, 16, 32,
  64}. Coherent detection with time-multiplexed pilots: one known symbol in
  every 8 (`PILOT_INTERVAL`). The rake estimates each path's gain from the pilots
  and combines them by maximal ratio.
- **CSK** — M-ary orthogonal code keying, as on the IS-95 uplink: a symbol
  sends one of the L Walsh–Hadamard codes of length L, so it carries log₂L
  bits. L is in {32, 64, 128, 256}. Detection is noncoherent: one fast Walsh
  transform of the descrambled chips gives all L correlations, the symbol is
  the largest, bit LLRs come by max-log from the correlation powers, and the
  rake adds the powers across paths, each descrambled at its own delay. No
  pilots are needed, and the phase only has to hold for one symbol, which suits
  HF fading at the robust end.

Walsh codes are exactly orthogonal, binary, and cost only adds to detect. Two
other choices were rejected:
- Cyclic shifts of one base sequence (JTIDS-style CCSK) were tried first. An
  m-sequence extended to 2^k chips measured a peak periodic sidelobe of 8/32
  (−12 dB) at L = 32.
- Zadoff–Chu has perfect cyclic autocorrelation, but its delay–Doppler ridge
  turns a frequency error into a wrong symbol.

The scrambler on top keeps the spectrum white whatever code is sent.

### Rates

`decdsss modes` prints the full table. On `2k4`, with rate-½ LDPC:

| Mode | Symbols/s | Coded bit/s | Info bit/s @ r=½ |
|---|---|---|---|
| DS4 | 480 | 840 | 420 |
| DS8 | 240 | 420 | 210 |
| DS16 | 120 | 210 | 105 |
| DS32 | 60 | 105 | 52.5 |
| DS64 | 30 | 52.5 | 26.3 |
| CSK32 | 60 | 300 | 150 |
| CSK64 | 30 | 180 | 90 |
| CSK128 | 15 | 105 | 52.5 |
| CSK256 | 7.5 | 60 | 30 |

`3k` is 1.25× these rates and `500` is 0.208×. FEC rates are ½, ⅔ and ¾. The
in-band SNR a mode needs is `Eb/N0 + 10·log₁₀(Rb / B)`, where B is the profile's
occupied width. The CLI prints that offset per mode. The Eb/N0 thresholds
themselves are measured in M1 on the channel simulator, not assumed.

Voice uses the fast end, and that is the price of spreading in 2.4 kHz:

- **Voice on `2k4`:** DS4 ⅔ gives 560 bit/s, enough for Codec2 450.
- **Voice on `3k`:** DS4 ¾ gives 787 bit/s, enough for Codec2 700C.
- **`500`:** no voice.

Processing gain against narrowband interference is then about 6 dB. Pictures
and files can use the deep end.

### Over structure

A transmission is one or more *segments*; a segment is:

1. **Preamble** — twelve periods of the profile's m-sequence A (511 chips on
   `2k4`/`3k`, 127 on `500`), then two periods of a second m-sequence B of the
   same length. Real BPSK chips, unscrambled, so every station's preamble is
   the same and any receiver finds it by FFT circular correlation. A body is
   found by A; the two B periods mark exactly where the header starts. (A
   sign-flip pattern was considered and dropped: it breaks the one-period
   windows the search relies on.) 4.3 s on `2k4`, 3.4 s on `3k`, 5.1 s on `500`.
2. **Header** — 128 bits (version, profile, mode, depth, payload type,
   callsign, session, first frame number, blocks in this segment, frames in the
   whole transmission, CRC-16) in a rate-¼ n = 512 LDPC codeword, interleaved,
   as 64 CSK256 symbols on the call code. Always CSK256 whatever the payload,
   so it is decodable wherever any payload mode is. 8.5 s on `2k4`, 6.8 s on
   `3k`, 41 s on `500`. (A rate-½ n = 256 header was built first; it needed
   about 3.4 dB more signal, which made it the limit of the whole system.)
3. **Payload blocks** — `nblocks` blocks on the station's own code, each
   `depth` LDPC codewords interleaved together. DS-PSK blocks carry a known
   pilot every eighth symbol and a closing pilot, so each block decodes alone.
   Each frame is data bytes plus a CRC-32.
4. **Resync** — a long transmission is cut into segments, each a fresh preamble
   and header with its first frame number, so late joiners can start and a
   receiver that lost the signal can find its place. How often is the sender's
   choice (≈ 60 s keeps the overhead near 15 %).

Several stations may transmit at once: their preambles overlap only by
coincidence, their payloads sit on different codes, and the receiver opens one
decoding channel per detected header. Near–far is the limit: a station far
stronger than the processing gain masks a weak one.

### Receiver chain

Audio at the device rate goes through a resampler to complex baseband at 4
samples/chip around the profile centre. Every profile is searched at once.

1. **Acquisition** — FFT circular correlation of the call-code preambles across
   frequency bins (±100 Hz on `2k4`/`3k`, ±25 Hz on `500`), noncoherent
   accumulation over periods, and a CFAR threshold.
2. **Tracking** — a code DLL (early/prompt/late), carrier frequency tracking from
   pilots (DS) or from successive CSK peaks, and rake fingers on the paths the
   channel profile shows.
3. **Demodulation and decoding** — header decode, then per-channel payload
   demodulation, deinterleaving, LDPC decoding (normalised min-sum) and the
   payload layer.

### FEC

DecDSSS's own quasi-cyclic LDPC codes: IRA (a block zig-zag accumulator as the
parity part), so encoding is a running XOR. The payload codes are n = 1152
(Z = 48, 24 block columns) at rates ½, ⅔ and ¾ (k = 576 / 768 / 864); the header
code is rate ¼, n = 512, k = 128 (Z = 32). The payload info part is designed here:
six columns of weight 5 and the rest weight 3 (the best of six weight patterns
tried; the header's four columns all have weight 8, best of 3–12), rows
filled evenly, and circulant shifts drawn from a seeded PCG32 so the lifted
graph has no 4-cycle (tested by brute force). The design seed is the best of 40
on simulated frame errors. The designs are regenerated from the seeds at start-
up and pinned by a fingerprint test, so a change that would make old
transmissions undecodable fails CI.

The decoder is layered normalised min-sum on float LLRs, stopping as soon as
the syndrome is zero. The normalisation α was tuned per code on frame errors
at threshold and 1.2 dB above it: α = 0.9 for the payload codes, 0.85 for the
header (0.8, the first guess, left some frames stuck on a single wrong degree-1
parity bit and plain min-sum lost 2 dB on the header).

Measured on BPSK/AWGN, 400 frames a point, true-LLR input, noise the seed
selection never saw (`cargo test -p decdsss-fec --release -- --ignored
--nocapture sweep`):

| Code | Eb/N0 for FER 10⁻² | Normal-approximation limit | Gap |
|---|---|---|---|
| payload ½ (n = 1152) | ≈ 1.85 dB | 0.85 dB | ≈ 1.0 dB |
| payload ⅔ | ≈ 2.5 dB | 1.65 dB | ≈ 0.85 dB |
| payload ¾ | ≈ 2.9 dB | 2.19 dB | ≈ 0.7 dB |
| header ¼ (n = 512) | ≈ 2.3 dB | 0.52 dB | ≈ 1.8 dB |

The limit is the normal approximation to the finite-length capacity of the
BI-AWGN channel (Polyanskiy–Poor–Verdú: R ≈ C − √(V/n)·Q⁻¹(ε) + log₂n / 2n),
computed by numerical integration for this code size, rate and FER. It is an
approximation, not a bound that a real code cannot beat. The header has the
most room left.

The time interleaver is a golden-ratio stride permutation over a block of
codewords: a run of B consecutive faded bits comes from B coding positions about
N/B apart and from every codeword in equal share (tested). Frame CRCs are
CRC-16 (headers) and CRC-32 (payload frames).

## Payloads and link layer

- **Files** (M2): one-way, split into frames. Extra frames are erasure-coded
  (Cauchy Reed–Solomon across frames, as in COFDMtv), so any large enough
  subset rebuilds the file.
- **Pictures** (M3): COFDMtv's pipeline. WebP by default, progressive JPEG as an
  option, sized by air time, systematic frames first so the picture draws as it
  arrives.
- **Voice** (M4): Codec2 (450 on `2k4`, 700C on `3k`), streamed in PTT overs with
  a short interleaver. Codecs run off the real-time loop (voicemodem's lesson).
  **Open:** the pure-Rust `codec2` crate voicemodem uses (0.3.1) has no 700C or
  450 modes; its slowest is 1200 bit/s. The options are to build the C
  libcodec2 (LGPL-2.1, CMake, as a submodule like DecSAT's libopus), to port
  700C/450 to Rust, or to run Codec2 1200 at SF 2 (≈3 dB processing gain).
  Rory's call before M4.
- **ARQ** (M5): a connected mode for files on both stations' own codes, with
  selective repeat and ACK bitmaps sent at a robust mode.

## Receiver panels (GUI)

DecSAT's instrument-panel look: dark faces, hairline frames, one muted hue per
quantity.

- **Band view** — waterfall and spectrum of the audio passband with the three
  profiles marked; PSD against the estimated noise floor, so you can see the
  spread signal sitting under the noise beside its despread SNR.
- **Per station** (one card per decoding channel):
  - acquisition heatmap (code phase × frequency, peak marked)
  - chip-phase correlation profile around the prompt
  - rake / multipath profile (path delays and strengths in chips, fingers marked)
  - E/P/L code tracking
  - despread constellation, and the symbol × previous symbol constellation
  - chip scope (raw chips against the local replica, chip eye)
  - despread SNR and frequency strips
  - soft bits, frame sync, and LDPC iterations / FER
- **Payload views** — picture as it arrives, file list, voice level.
- **Transmit** — profile, mode, payload, callsign, PTT (VOX/manual/CAT).

## Radio I/O

Sound card in and out (cpal), with VOX or manual PTT. CAT keying for the IC-7300
and FT-710 comes from voicemodem's `rig` crate. Files: WAV (mono audio, or
stereo as I/Q) and raw `cf32`. Live tests go through VB-Audio cable A, which is
silent.

## Crate layout

| Crate | Role |
|---|---|
| `decdsss-core` | Profiles, modes, rates — the fixed numbers. |
| `decdsss-dsp` | NCO, RRC, resamplers, FFT correlation, AGC (M1). |
| `decdsss-fec` | QC-LDPC, interleaver, CRC, erasure code (M1–M2). |
| `decdsss-phy` | Codes, TX modulator, acquisition, tracking, rake, demod, over framing (M1). |
| `decdsss-chan` | Channel simulator: AWGN, frequency offset and drift, Watterson HF fading (ITU-R F.1487 good/moderate/poor) (M1). |
| `decdsss-audio` | Sound card and WAV/IQ file I/O (M2). |
| `decdsss-link` | Files, pictures, voice, ARQ (M2–M5). |
| `decdsss-rig` | CAT keying (M4). |
| `apps/decdsss-cli` | `decdsss` CLI: modes, tx/rx to WAV, simulate, measure. |
| `apps/decdsss-gui` | egui GUI (M2). |

## Milestones

- **M0** — scaffold, this document, CI, private GitHub repo. `decdsss modes`.
- **M1** — modem core: codes, TX, channel simulator, acquisition, tracking, rake,
  DS-PSK and CSK demod, LDPC and interleaver, header; `decdsss tx/rx` through
  WAV; measured Eb/N0 thresholds per mode on AWGN and Watterson channels.
- **M2** — GUI with every receiver panel, live sound card RX/TX, one-way files.
  Checked through VB-Audio cable A.
- **M3** — pictures.
- **M4** — voice, VOX/manual PTT, CAT keying.
- **M5** — ARQ connected mode.
- **M6** — polish and the first release (portable exes), when Rory says so.

## Verification

The channel simulator is the reference: every mode's threshold is measured on
AWGN and on the three Watterson channels, with frequency offset and drift. The
results go in this file and in README. Heavy sweeps run on CI, not on Rory's
machine. Live radio tests are Rory's.
