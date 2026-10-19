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

One receiver per profile, all on the same audio. The front end mixes the audio
down by the profile's centre, resamples to four samples a chip (a polyphase
sinc, 70 dB down at the mixing image) and applies the root-raised-cosine matched
filter, normalised so a chip comes out at its own amplitude.

1. **Search** (never stops, whatever else is open) — FFT circular correlation of
   the preamble's sequence A across frequency (half-bin steps, ±100 Hz, the
   half-bin ones from a second FFT of the window times e^{-jπn/N}), each window
   scaled by its own median, the last three to eight summed, a gamma-tail
   threshold from a false-alarm rate of 10⁻⁴ a window, and two more windows
   allowed to settle after the first crossing. Normalising each window matters:
   a fading signal changes the level between windows and, unnormalised,
   fattened the sum's tail to six false alarms a minute on payload alone.
2. **Candidates** — up to four preamble candidates in flight at once; a false
   alarm dies on its own and cannot block a real preamble. Each period of the
   preamble is correlated coherently with A and B at early, prompt and late
   timing; a loop follows the boundary, periods with no signal in them are
   ignored (the search window starts early), the pattern A…A B B is found from a
   per-period score that needs no noise estimate, and a line through the period
   boundaries gives the clock and the header start.
3. **Header** — 64 CSK256 symbols on the call code, the rake fingers from the
   preamble's delay profile, a log-Bessel noncoherent metric, de-interleave, LDPC.
   Once decoded, its chips are known exactly, and they are used to refine timing,
   clock and carrier (a quarter second at a time over a fine lag grid, following
   the timing from segment to segment) and to place the fingers from the profile
   of the header's tail, on the same reference as the end position.
4. **Payload** — one channel per decoded header, on that station's own code,
   several at once. Each block starts with a pre-pass: the delay profile from
   the first pilots (DS-PSK) or from the Walsh peaks of the first symbols
   (CSK), fingers placed on its peaks by deconvolution (CLEAN, with the
   raised-cosine pulse response), reference moved onto the strongest path.
   Then symbol by symbol as samples arrive: rake despreading; an early/late
   timing loop on the sum over all fingers; a carrier loop from pilot gains or
   successive CSK peaks, updated only when the phase steps agree and limited in
   step and excursion; and, every six seconds, a conservative searcher that moves
   the reference only to a path a third of a chip away and 1.8 dB stronger.
   At the end of the block: DS-PSK combines with smoothed, interpolated pilot
   gains shrunk by power subtraction where a finger is not clearly above its
   estimation noise; CSK sums a log-Bessel metric over fingers; then
   de-interleave and LDPC. A block whose codewords all decode is re-modulated and
   the refinement above runs on it, so the clock and carrier are re-learned
   every block.

Why not one early/late loop on one finger: with two paths a chip or so apart the
late arm picks up the next path's correlation tail, and the loop settles
between the paths or walks off them. This was found, not guessed: a stretch of
blocks lost at high SNR on a two-path channel traced to it.

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

Built (egui, glow renderer), in DecSAT's instrument-panel look: dark faces,
hairline frames, one muted hue per quantity.

- **Band view** — the audio spectrum 0–4 kHz with the three profile bands
  shaded, the noise floor (15th percentile of 300–3300 Hz), and a label at each
  locked carrier; a waterfall below it (levels from the floor to the strongest
  recent signal).
- **Per profile** — the acquisition search surface as a heatmap (frequency ×
  code phase, scaled to the noise, the peak ringed) and one row per candidate
  preamble with its A/B period bars.
- **Per station** (one card per decoding channel; dimmed between segments):
  - DS-PSK: despread constellation (Es/N0 from the error), symbol × previous
    symbol, early/prompt/late timing; CSK: the Walsh correlator bank (a waterfall
    of the correlators whose bright trace is the decided codes), the last symbol's
    bars, timing
  - the delay profile with the rake fingers marked (multipath, in chips)
  - the chip scope (the last symbol's chips as received, and despread)
  - symbol SNR and carrier strips
  - the last codeword's soft bits, and a frame map of the transmission
- **Files** — the received list (click to open), progress bars, the log.
- **Transmit** — file, callsign, profile, mode (with its rate and the SNR it
  needs), depth, preamble interval, repeat, level, output device; play to the
  sound card (at that device's own rate) or save a WAV.

Sources: a sound card or virtual cable (stereo is mixed to mono), a WAV file at
any speed, or the built-in demo (two stations, one DS8 and one CSK64, on one
2.4 kHz channel through moderate fading). `--screenshot OUT.png --after SECS`
takes the picture from inside the window, for unattended checks.

## Radio I/O

Sound card in and out (cpal), with VOX or manual PTT. CAT keying for the IC-7300
and FT-710 comes from voicemodem's `rig` crate. Files: WAV (mono audio, or
stereo as I/Q) and raw `cf32`. Live tests go through VB-Audio cable A, which is
silent.

## Crate layout

| Crate | Role |
|---|---|
| `decdsss-core` | Profiles, modes, rates — the fixed numbers. |
| `decdsss-dsp` | RRC, pulse shaper, resampler, matched filter, interpolation, Hilbert, a seeded RNG. |
| `decdsss-fec` | QC-LDPC (design, encoder, layered min-sum), the interleaver, CRCs. |
| `decdsss-phy` | Codes, framing, the transmitter, the receiver (search, preamble, header, payload, refinement), the end-to-end simulator. |
| `decdsss-chan` | Channel simulator: AWGN, carrier offset and drift, clock error, Watterson fading with the CCIR presets. |
| `decdsss-link` | File container, sender planning (segments, repeat), receive-side assembly. |
| `decdsss-audio` | Sound-card capture and playback (cpal) and WAV files; copied from COFDMtv's `cofdmtv-io`. |
| `decdsss-rig` | CAT keying (M4). |
| `apps/decdsss-cli` | `decdsss`: modes, devices, tx, rx, channel, sweep. |
| `apps/decdsss-gui` | `decdsss-gui`: the window above. |

## Milestones

- **M0** — scaffold, this document, CI, private GitHub repo. `decdsss modes`. *Done.*
- **M1** — modem core: codes, TX, channel simulator, acquisition, tracking, rake,
  DS-PSK and CSK demod, LDPC and interleaver, header; `decdsss tx/rx/channel/sweep`
  through WAV; thresholds measured on AWGN and Watterson channels. *Done* (see
  Verification).
- **M2** — GUI with every receiver panel, live sound card RX/TX, one-way files.
  Checked through VB-Audio cable A (a file sent by the CLI into CABLE-A Input
  and received in the GUI and the CLI from CABLE-A Output, byte-identical).
  *Done.* Left for later: erasure-coded extra frames (the picture and file
  modes will want them), a CI smoke test of the window.
- **M3** — pictures.
- **M4** — voice, VOX/manual PTT, CAT keying.
- **M5** — ARQ connected mode.
- **M6** — polish and the first release (portable exes), when Rory says so.

## Verification

The channel simulator is the reference. `decdsss sweep` runs whole transmissions
(preamble, header, payload blocks) through it and the real receiver, so a
threshold here includes acquisition, the preamble, the header and the payload
together, with the carrier offset (±6 Hz) and the clock error (±30 ppm) spread
over the trials. Eb/N0 is per information bit of a codeword (its CRC counted),
with the energy of the pilots and padding counted as air time. Heavy sweeps run
on CI or by hand, not as default tests; live radio tests are Rory's.

### AWGN, full receiver (2026-10-19)

The lowest Eb/N0, in 0.5 dB steps, at which every one of at least 56 frames
(14 transmissions) arrived. The in-band SNR is in the profile's occupied width.
LDPC alone (BPSK, same codewords) needs 1.85 dB for 1 % at rate ½, so the whole
modem is about 2 dB above its code.

| Profile | Mode | Eb/N0 for no loss | In-band SNR |
|---|---|---|---|
| 2.4 kHz | DS4 ½ | 4.0 dB | −3.6 dB |
| | DS8 ½ | 4.5 dB | −6.1 dB |
| | DS16 ½ | 4.0 dB | −9.6 dB |
| | DS32 ½ | 4.5 dB | −12.1 dB |
| | DS64 ½ | 4.0 dB | −15.6 dB |
| | DS4 ¾ | 5.0 dB | −0.8 dB |
| | DS16 ¾ | 4.5 dB | −7.3 dB |
| | CSK32 ½ | 4.0 dB | −8.0 dB |
| | CSK64 ½ | 4.0 dB | −10.3 dB |
| | CSK128 ½ | 4.0 dB | −12.6 dB |
| | CSK256 ½ | 4.0 dB | −15.0 dB |
| | CSK32 ¾ | 4.0 dB | −6.3 dB |
| | CSK64 ¾ | 3.5 dB | −9.0 dB |
| 3 kHz | DS8 ½ | 4.0 dB | −6.6 dB |
| | CSK64 ½ | 4.0 dB | −10.3 dB |
| | CSK256 ½ | 4.0 dB | −15.0 dB |
| 500 Hz | DS8 ½ | 4.0 dB | −6.6 dB |
| | CSK64 ½ | 6.0 dB (one frame of 14 lost at 4–5 dB) | −8.3 dB |

The header, a rate-¼ CSK256 codeword, decodes 12 of 12 times from −15 dB to
+5 dB in-band on AWGN and 9 of 12 at −17 dB (the preamble search finds the
preamble at −17 dB with a 50 ppm clock error and 55 Hz offset, and makes no false
detection in a minute of noise or of strong payload).

### Watterson fading (2.4 kHz, ½ rate, depth 4, two blocks, 24 transmissions a point)

Frame loss against Eb/N0. The paths are equal-power Rayleigh with Gaussian
Doppler spectra: *good* 0.5 ms and 0.1 Hz spread, *moderate* 1 ms and 0.5 Hz,
*poor* 2 ms and 1 Hz.

| Channel | Mode | 9 dB | 13 dB | 17 dB | 21 dB |
|---|---|---|---|---|---|
| good | DS8 | 21 % | 7.8 % | 1.0 % | 0 % |
| | CSK64 | 37 % | 6.3 % | 2.1 % | 0 % |
| moderate | DS8 | 2.1 % | 0 | 0 | 0 |
| | CSK64 | 18.8 % | 0 | 0 | 0 |
| poor | DS8 | 0 | 0 | 0 | 0 |
| | CSK64 | 6.3 % | 0 | 0 | 0 |

The slow *good* channel is the hard one: its fades last seconds, so a block (11 s
for DS8, 25 s for CSK64 at depth 4) can lose a codeword to one. Deeper
interleaving, repeating the transmission (`decdsss tx --repeat`) and, later,
erasure-coded extra frames and ARQ are the remedies; a repeated pass fills a file's gaps
(tested end to end).

### Not yet measured

The 3 kHz and 500 Hz profiles on fading; flutter fading; voice modes (no
Codec2 yet); the effect of in-band interference; two stations at once beyond
the functional test in `rx::tests` (equal levels, different codes, 12 s
apart).
