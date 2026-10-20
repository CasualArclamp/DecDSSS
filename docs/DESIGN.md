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
| Payloads | Digital voice (Codec2), SSTV-style pictures, files. (Text was left out at first; a text mode for chat and a header-only CQ packet were added on Rory's request later the same day.) |
| Spreading | Both families as modes: DS-PSK with a spreading-factor ladder, and M-ary code-shift keying (CSK). |
| Channels | HF SSB skywave; sound card / virtual cable first. |
| Bandwidths | 2.4 kHz (300–2700 Hz) and 3 kHz (300–3300 Hz), plus a 500 Hz weak-signal profile. |
| Station ID | Callsign in the header only (no CW ID). An optional *waterfall ID* (the callsign and mode written on the spectrum ahead of the signal, for anyone watching a waterfall; nothing decodes it) was added when Rory asked for it on 2026-10-20: *Radio I/O*. |
| Code use | CDMA: a common *call code* for preambles and headers, then data on a code derived from the station's callsign; the receiver despreads every station it finds at once. |
| Radio I/O | Sound card with VOX/manual PTT; CAT keying for IC-7300 / FT-710 (from voicemodem), any Hamlib rig through `rigctld`, and DTR/RTS keying; WAV and IQ files in and out. |
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

**500 Hz slots (FDMA).** The 500 Hz profile can sit in any of three slots, 500 Hz
apart: −500 Hz (audio centre 1000 Hz, band 750–1250), the middle (1500 Hz, 1250–1750)
and +500 Hz (2000 Hz, 1750–2250). The occupied width is 500 Hz, so neighbouring
slots touch with no guard band, and all three fit in a 2.4 kHz filter, so up to three
500 Hz stations can share one SSB channel, as narrow HF modems do. The slot is not
signalled: a transmitter just sends from its slot (`tx --profile 500 --slot +500`, the
Slot selector in the Transmit panel) and a receiver runs one 500 Hz receiver per
slot, each with its front end mixed down from that slot's centre (`rx::Receiver::at`,
`MultiReceiver::with_bands`; `core::Band` is a profile and a slot). Events say which
band heard them. *Measured* (`rx::tests::fdma_neighbour_curve`, ignored; a weak DS4
1/2 station in the middle slot at 8 dB in-band SNR, a DS4 1/2 neighbour in the slot
above or below, louder by the amount given, 16 frames a point, no fading): every
frame delivered with a neighbour up to 40 dB louder at any carrier offset tried (0 and
±25 Hz, the weak station also DS16 1/2); at 50 dB delivered with the neighbour on
frequency or moved away from the weak station, lost with it 25 Hz toward it; at 60 dB
lost, except a neighbour below and 25 Hz further down (delivered, lost at 70 dB). So
the isolation between slots is about 40 to 60 dB in this simulation (the shaping
filter at the transmitter and the matched filter at the receiver, not separated); a
real radio's filter skirts and a neighbour's splatter are not modelled. A station
far above the noise (over a cable, 58 dB symbol SNR) is also found, faintly, by the
other slots' preamble searches, which correlate at one sample a chip, so a signal
500 Hz away folds onto 100 Hz (500 mod the 400 chip/s) after the matched filter has
taken some off it (seen: the middle slot decoding the +500 Hz station at "carrier
+100.00 Hz", symbol SNR 1 dB against 57 dB in its own slot). The same header decoded
in two slots is one transmission, reported by the slot where it is stronger by 6 dB or
more (the first to hear it otherwise), and the other copy's events and channel are
dropped (`MultiReceiver`; `rx::tests::a_strong_station_is_reported_by_its_own_slot_only`).
Three stations at once, one per slot, are all received
(`rx::tests::three_500_hz_stations_share_the_passband`).

*Why a receiver per slot, not one wide receiver.* One front end could cover all
three slots (their 1500 Hz fits the 1600 Hz complex rate of a 500 Hz receiver), but
it would not be cheaper or simpler. *Measured* (release build, 47.9 s of audio through
`decdsss rx`): one 500 Hz receiver 0.65 s, the three slots 1.48 s (including decoding
one transmission), all five bands 2.82 s, so about 1.4 % of a core for the first and
3 % for the three. A wide search would cover ±600 Hz where three narrow ones cover
3 × ±100 Hz, so it would do more work, not less. The search correlates at one
sample a chip, where 100 Hz and 500 Hz are the same frequency (the alias above), so
a wide search would have to work at four samples a chip or in two stages. And each
signal found still needs its own mixer and matched filter, which is what a slot's
receiver already is. What a wide receiver would give is free tuning: a station
anywhere in the passband, not only within ±100 Hz of a slot centre (today a station
at +250 Hz is in no slot's search). That would be a channelizer (one wide search, then
a mixer and filter per candidate) and is the thing to build if the 500 Hz channels
should not sit on a fixed grid.

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

- **DS-PSK** — a QPSK data symbol held for SF chips, with SF in {1, 2, 4, 8, 16,
  32, 64}. Coherent detection with time-multiplexed pilots: one known symbol in
  every 8 (`PILOT_INTERVAL`). The rake estimates each path's gain from the pilots
  and combines them by maximal ratio. SF 2 and 1 (added 2026-10-20, see
  *DS2 and DS1* under Verification) have little or no processing gain left; they
  are modes for a clean, strong channel.
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
| DS1 | 1920 | 3360 | 1680 |
| DS2 | 960 | 1680 | 840 |
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
- **Faster codecs (2026-10-20):** Codec2 1200 on `2k4` rides DS1 ½ (1680 bit/s), 1600
  DS1 ⅔ (2240); on `3k`, 1200 rides DS2 ¾ (1575) and 1600 DS1 ½ (2100) (derived:
  `3k` is 1.25 times `2k4`'s rates). They need a clean, strong channel (DS2 and DS1,
  see *DS2 and DS1* under Verification).
- **`500`:** no voice.

Processing gain against narrowband interference is then about 6 dB. Pictures
and files can use the deep end, or the fast end: DS2 and DS1 double and quadruple
DS4's rates (derived: the symbol rate is the chip rate over SF, the pilots stay one
in eight), 1260 and 2520 info bit/s at rate ¾ on `2k4`, at the price of the
processing gain (3 dB and none) and of fading margin (Verification).

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
   as CSK symbols on the call code, in one of three alphabets (`HEADER_ALPHABETS`:
   64, 128 or 256 codes of as many chips, 6 to 8 bits a symbol, the same
   codeword). The weak payload modes (DS32, DS64 and every CSK mode) keep CSK256,
   decodable wherever any payload mode is: 8.5 s on `2k4`, 6.8 s on `3k`, 41 s on
   `500`. The fast modes send a shorter one so decoding starts sooner: DS4 (and
   DS2 and DS1) use CSK64 (86 symbols, 2.9 s on `2k4`) and DS8 and DS16, at every rate, CSK128
   (74 symbols, 4.9 s), where the header still decodes below the payload's own
   threshold. (The mode field's 3-bit spread code keeps the original ladder's
   codes, DS 4 to 64 as 0 to 4, and gives SF 2 and 1 the two that were free, 5
   and 6: a build from before DS2 and DS1 skips a header that names them, and
   every build reads the older ones. The payload type is 3 bits, codes 0 to 7; the
   types from code 8 up, Codec2 1600 speech the first, go in a version-1 header whose
   type field is one bit wider and whose session is one bit narrower (5 bits, 0 to
   31). Every type that fits in 3 bits is still sent in the version-0 header, as
   before, and a build that predates version 1 skips a version-1 header, which is
   all it could do with a type it does not know.) (16 and 32 were measured too and never chosen, see Verification, so
   the receiver does not try them.) The receiver tries the alphabets shortest
   first, each as soon as its chips are in, so it starts as early as the
   transmitter allows. A longer header seen in a shorter alphabet despreads to
   clean decisions (a Walsh code of 2L chips is a code of L chips twice, the
   second time negated when its top bit is set), so only the LDPC and the CRC
   turn it away; the display tells it by phase instead. The start-up
   (preamble and header) of a DS4 transmission is 6.6 s instead of 12.3 s. (A rate-½
   n = 256 header was built first; it needed about 3.4 dB more signal, which made
   it the limit of the whole system.)
3. **Payload blocks** — `nblocks` blocks on the station's own code, each
   `depth` LDPC codewords interleaved together. DS-PSK blocks carry a known
   pilot every eighth symbol and a closing pilot, so each block decodes alone.
   Each frame is data bytes plus a CRC-32.
4. **Resync** — a long transmission is cut into segments, each a fresh preamble
   and header with its first frame number, so late joiners can start and a
   receiver that lost the signal can find its place. How often is the sender's
   choice (≈ 60 s keeps the overhead near 15 %).
5. **Postamble** — after the last block of the last segment, and of a live stream
   when the talker lets go, one second (four coherent quarter-seconds: 1920 chips on
   `2k4`, 2400 on `3k`, 400 on `500`) of the station's own scrambler run on from the
   last block, times the preamble's B sequence. Real chips at full power, no data.
   A receiver already following the station has its timing, carrier and rake
   fingers; it correlates the chips that come next with this known sequence at the
   delay profile's lags (−6 to +20 chips), sums the four segments' powers, and calls
   the transmission over when the profile's peak is 7 times its median (a live stream
   is asked after every block; a finished segment for the second after it, and only
   a yes is reported). A miss changes nothing: a live stream still closes on its end
   frame or after six empty blocks. The B sequence on top of the scrambler is what
   keeps data from answering: data is the scrambler times a symbol held for SF
   chips, which the bare scrambler would sum up coherently into a peak at the main
   path, while B changes every chip and data sums like noise against it. The
   station's own scrambler keeps one station's postamble from ending another's
   channel. It costs one second of air time per transmission (counted in the
   picture budget). The threshold is derived from the tail of a sum of four
   exponentials: P(X > 7 × 3.67) = 1.5·10⁻⁶ a lag, about 26 independent lags in the
   window, so about 4·10⁻⁵ a test. *Measured* (`rx::tests::eot_curve`, ignored; 2.4
   kHz, genie timing, 24 trials a point, in-band SNR): heard in 23 of 24 trials at
   −18 dB and all from −15 dB without fading (DS4 3/4 and DS8 1/2 alike; the
   weakest payload mode, DS64 1/2, needs −15.6 dB); on moderate fading 9 of 24 at
   −18, 19 of 24 at −15, 23 of 24 at −12 and all from −6 dB. Mistaken for one: 0 in
   576 tests on data (the place of the next block mid-stream) and 0 in 576 on noise
   after a transmission without one, the highest ratio seen 5.5.

Several stations may transmit at once: their preambles overlap only by
coincidence, their payloads sit on different codes, and the receiver opens one
decoding channel per detected header. Near–far is the limit: a station far
stronger than the processing gain masks a weak one.

### Receiver chain

One receiver per profile, all on the same audio. The front end mixes the audio
down by the profile's centre, resamples to four samples a chip (a polyphase
sinc, 70 dB down at the mixing image) and applies the root-raised-cosine matched
filter, normalised so a chip comes out at its own amplitude.

A fast sound card is first brought down for all the bands at once: a whole-factor
decimator (Kaiser sinc, passing the highest band edge plus the 100 Hz offset margin,
70 dB down from the new Nyquist frequency) leaves at least three times that edge, so
44.1 and 48 kHz go ÷4 to 11 025 and 12 000 Hz with every band on (`rx::shared_decimator`).
The resampler's length grows with its input rate, so this is what made 44.1 kHz cost
what 12 kHz does. The bands then run side by side on rayon's thread pool. Measured with
`cargo run --release -p decdsss-phy --example realtime` (five bands, 20 ms pushes, the
11:00:19Z ironstonerange websdr recording at 44.1 kHz, 16-thread desktop): 18× real
time before, 51× after; each change alone gave about 2× (decimator) and 1.5× (pool).
At threshold the decimated chain decodes the same frames as the plain 48 kHz one to
within 1 % (3 kHz profile, 3 dB Eb/N0, 960 frames, 40 ppm clock), and clock offsets
of ±100 and ±300 ppm change nothing at 4 dB.

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
3. **Header** — CSK symbols on the call code (alphabets above), the rake fingers from the
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
   That wait (four seconds of pilots) is only taken when the fingers are stale: a
   block that follows one that decoded whole starts at once, with the fingers and
   reference its known-chip refinement left (below), and so does the first block,
   with the header's. Otherwise the instruments stand still for the first four
   seconds of every block and then catch up, which for a DS4 block of 4.4 s is most
   of it (`rx::tests::the_channel_keeps_up_with_the_signal_between_blocks`: the
   counter stood still 2.7 s, now under a second). Frame loss through the whole
   receiver, 32 to 48 transmissions a point, waiting against starting at once:
   DS4 2/3 depth 2 on moderate fading (14, 18, 22 dB) and on AWGN (2, 4, 6 dB),
   DS8 1/2 on moderate fading (6, 9, 12 dB): identical, to the frame. CSK64 1/2 on
   moderate fading at 6 dB 65.1 % to 60.9 % lost, at 9 dB 9.4 % to 8.3 %; DS16 3/4
   on good fading with 20 ppm at 9 dB 23.4 % to 21.1 %, the rest equal. The
   pre-pass was not doing work the refinement had not done.
   Then symbol by symbol as samples arrive: rake despreading; an early/late
   timing loop on the sum over all fingers; a carrier loop from pilot gains or
   successive CSK peaks, updated only when the phase steps agree and limited in
   step and excursion; and, every six seconds, a conservative searcher that moves
   the reference only to a path a third of a chip away and 1.8 dB stronger.
   At the end of the block: DS-PSK combines with smoothed, interpolated pilot
   gains (a triangle of the same length in time at every SF: [1 2 1]/4 at SF 4 and
   above, twice that length at SF 2 and 1; see *DS2 and DS1*) shrunk by power
   subtraction where a finger is not clearly above its estimation noise; CSK sums a log-Bessel metric over fingers; then
   de-interleave and LDPC. DS-PSK is combined twice: by a G-RAKE first (below),
   and by the rake for any codeword the G-RAKE's LLRs do not decode.

**G-RAKE** (`rx/grake.rs`; Bottomley, Ottosson and Wang, IEEE JSAC 2000, the
parametric form). The symbols are despread again on a half-chip grid reaching
three chips either side of each finger (at most 40 delays) and combined with
w = Q⁻¹p₀, where p₀ is the paths' composite response on the grid and Q the
neighbouring chips' share of it plus the matched filter's noise. Followed by
despreading this is the chip-level LMMSE equaliser. The paths come from the
smoothed pilot gains by deconvolving the pulse overlap between fingers, so the
weights follow a fade pilot by pilot as the rake's do; the noise is the main
finger's pilot-gain scatter less the model's own inter-chip share. The QPSK
LLRs are then exactly 2√2·L·Re(wᴴy). *Derived* (tests in `grake.rs`): with two
paths a whole chip apart the rake's chip SIR stops at
(|a₀|²+|a₁|²)²/(2|a₀|²|a₁|²), 3 dB for equal paths, at any SNR; the G-RAKE
reaches 10.7 dB there (equal paths leave a null in the band) and 27.9 dB with the
second path at half amplitude, at 30 dB. What it does on the simulator is under
Verification: nothing measurable at SF 4 and above, a good deal at SF 2 and 1
(*DS2 and DS1*). A block whose codewords all decode is re-modulated and
   the refinement above runs on it, so the clock and carrier are re-learned
   every block.

**Slips** (`rx/rewind.rs`, after BinModem's rewind). A sound card or a
network stream that drops or repeats a buffer moves the signal by that much: the
track is then chips off, which the early/late loop cannot pull and the block
pre-pass (−6 to +20 chips, on a few pilots) often cannot see. A watch on each
channel judges, every quarter second (at least 12 symbols), the signal the
fingers collect: the energy less the noise measured the same way where there is
no path (DS: eight chips past the last finger; CSK: the other codes' mean power
times H_L, the mean of the largest of L noise powers). A window 5 dB under the
settled level is a loss. Then a search on the samples kept: over the symbols
since the loss, the despread power at every lag within ±60 ms; a shift of the
fingers' whole pattern that collects twice what it collects where it is, and at
least 0.4 of the settled signal, is a slip if the energy moved in one clean step
(the old lag empty after it and the new lag empty before it, noise taken off; a
fade of one path onto another path fails this). The tracking state goes back
to the snapshot (taken every window) from before the step, the symbols are
despread again from there with the track moved from the step on, and the block's
known-chip refinement starts at the step. A fade is left alone: holding the loops
in it, as BinModem does, cost DS4 whole transmissions on moderate fading, because
the loops were held at the start before they had settled.

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

- **CQ packet** (done): a CQ is only a header. The header already carries the
  callsign (rate-¼ LDPC, CRC-16, CSK on the call code), so a CQ is a preamble and a
  header with `nblocks` 0 and `PayloadType::Cq`: no frames, no container, nothing
  after the header that has to decode too (`link::cq`; the receiver already treated a
  header announcing no blocks as complete). It always uses the long CSK256 header (the
  mode field is fixed at DS64 1/2, which is what picks it) whatever mode is set, and
  is sent in 2 copies by default (`--copies`, up to 8), each decodable alone, so a
  fade or a burst that takes one leaves the other; `frame0` is the copy number and
  `total_frames` the copies. Two copies on 2.4 kHz are 25.5 s of signal (the CLI's count; two
  preambles and headers and the 1 s postamble; `cq::tests` checks the estimate against
  the real chips). The receiver reports it on the header event (`CQ from CALL` in the
  CLI and the GUI's Messages panel, once however many copies arrive). The GUI's CQ
  button sets this up; `tx --cq` is the CLI. *Measured* (`tests/cq.rs::cq_curve`,
  ignored; 2.4 kHz, 24 trials a point, carrier offset and clock error, in-band SNR,
  number of trials in which the call was heard): on AWGN one copy was heard in 24 of
  24 down to −14 dB and in 24 of 24 for two copies down to −16 dB; on moderate fading
  one copy 21 of 24 at −12 dB, two copies 24 of 24 at −12, 19 at −14 and 5 at −16.
  The text column counts only the header of a text CQ (DS8 1/2) being decoded, which
  flatters it: its data frame has to decode as well. Even so it lost
  the header on AWGN at −16 dB (1 of 24) and on fading at −12 dB (13 of 24, against
  24 for the packet). Not done: soft-combining the copies' headers (each is decoded
  alone today, so two copies help by chance, not by adding up), and a CQ carrying
  anything beyond the callsign (grid, a reply request).
- **Text** (done): a CQ call or a few lines of chat, up to 1000 bytes of UTF-8, as
  a payload of its own (`PayloadType::Text`, `link::text`). It rides the file
  machinery (the container with no file name, erasure-coded frames, `--parity` and
  `--repeat`), so it has the same protection, and the receiver shows it as a message
  (the GUI's Messages panel, `TEXT from …` in the CLI, and a line in `messages.log`
  in the output folder) instead of saving a file. It always goes out at depth 1: a
  message is a frame or two, and a deeper block would pad it with empty frames and
  take that many times the air time. A CQ used to be sent as text ("CQ CQ CQ de CALL CALL CALL pse K");
  that is still `tx --text`, but the CQ button and `tx --cq` now send the CQ packet above. The
  start-up (preamble and header, 6.6 s or more) is most of the air time of a short
  message, so a robust mode costs little extra: a text CQ at DS8 1/2 on 2.4 kHz is 17 s of
  audio including the postamble. Any bandwidth and any 500 Hz slot (checked end to
  end through the simulator on 2.4 kHz and in the +500 Hz slot, `tests/text.rs`, and
  through the cables in the GUI).
- **Files** (done): one-way, erasure-coded. The file goes into a small container
  (name, length, CRC-32), is cut into `K` data shards, and the sender adds coded
  shards (Cauchy Reed–Solomon over GF(2^16), ported from COFDMtv, itself from
  aicodix): any `K` distinct frames rebuild the file, however they are lost. Each
  frame's data is `[ident u16][K u16][shard]`, so a receiver learns `K` from any
  frame it hears and needs no frame numbers. A pass sends the `K` data frames
  then some coded ones (`--parity`, 25 % by default); each further pass
  (`--repeat`) sends *new* coded frames, so every frame of the next pass helps
  whatever the first lost, where the old plain repeat only helped for the frames
  that happened to be the lost ones. Decoding cost follows the number of data
  frames missing, not `K`. A 2400-byte file through slow fading that lost 3–5
  frames of 38 a pass now arrives in one pass with 10 coded frames (4 of 5
  seeds; the fifth lost 13 of 48, more than the code covers).
- **Pictures** (M3, done): a progressive JPEG, pure Rust (`jpeg-encoder` to write
  it, `zune-jpeg` through `image` to read it; no C code, unlike COFDMtv's
  mozjpeg/libwebp route). The picture is scaled (Lanczos) to the largest size of a
  ladder (√2 steps in pixel count, 256 pixels to 1 M) whose JPEG fits the byte
  budget at a quality of at least `--min-quality` (default 30), found by
  bisection; the budget is what `--air` seconds of one pass carry
  (`max_stream_bytes`, exact: it is the same arithmetic as the sender's plan,
  tested against the real chip count). It is sent as an ordinary erasure-coded
  file with the header's payload type set to Picture. The data frames go first
  and in order, so the front of the file is readable while the rest is on the
  way, and the receiver draws it after every block (`Assembler::front` gives the
  bytes held from the start without a gap; `decode_partial` decodes them with the
  end-of-image marker put on the cut, or up to the last complete scan if the cut
  one will not decode). A JPEG with 6 scans per component (the DC of each
  component, then 5 equal bands of AC coefficients; the encoder's script, not
  tunable) shows the whole picture, blurred, from about a fifth of the file, and
  sharpens from there. Measured on one 192×144 photograph at quality 40
  (4198 bytes): PSNR against the original 18.4 dB with 20 % of the file, 19.7 at
  30 %, 21.3 at 40 %, 22.8 at 50 %, 25.1 at 60 %, 26.9 at 75 %, 28.6 whole; four
  scans per component would be 6 % smaller and 1.4 dB worse at half the file.
  Not done: a custom scan script (successive approximation of the DC and
  low AC first would sharpen the first scans further), and sending only the
  scans that fit with the rest on a later pass.
  Examples (a 797×1200 photograph, 120 s a pass, 25 % coded frames): DS4 ¾ on
  2.4 kHz gives 147×222 at quality 48 in 5319 bytes, DS8 ½ gives 62×93 at 47 in
  1576 bytes.
- **Voice** (M4, link layer and CLI done): Codec2 450 on `2k4`, 700C on `3k`, in
  the mode `Mode::voice` names (DS4 ⅔ and DS4 ¾), as store-and-forward "overs": a
  recording is compressed, sent, and the receiver rebuilds the speech. Codec2 1200
  and 1600 are offered too, on the fast modes (*Faster codecs* below).
  - *Codec.* The pure-Rust `codec2` crate (0.3.1) has no 700C or 450 and upstream
    removed 450 in July 2023, so `decdsss-codec2` vendors the C core of the last
    commit that had it (9f5e2de), compiled with `cc` (no CMake, MSVC and gcc),
    with variable-length arrays turned into a portable macro (MSVC has none) and
    the codebooks pre-generated (README in the crate, `vendor_codec2.py` to
    redo it). LGPL-2.1, linked into a GPL-3.0-or-later program; relinking is
    possible since it builds from source. Modes 3200–1200 are there too.
  - *Framing.* The payload type says the codec (`Voice` = 700C, `Voice450`,
    `Voice1200`, `Voice1600`). The
    codec's frames are packed back to back as bits, a whole number to a modem
    frame (92 bytes at DS4 ⅔ hold 40 frames of 450, 18 bits each; 104 bytes at
    DS4 ¾ hold 29 frames of 700C), so a modem frame that does not decode is a
    silent gap of known length (1.6 s at 450, 1.2 s at 700C) and the next one
    starts on a codec frame. The header's frame number puts a frame in time, so a
    listener who joins at a later segment hears the rest. No erasure coding or
    ARQ for voice: LDPC and the interleaver only.
  - *Rate.* A modem frame carries 720 bits of speech in 1.37 s of signal at DS4 ⅔
    on 2.4 kHz, 1.17 times the codec's 450 bit/s (1.05 at 700C on 3 kHz, 29 × 28
    bits per 1.10 s block of DS4 ¾). The payload therefore keeps up with the
    speech, but only just, and a 6.6 s preamble and header come first (12.3 s before the
    short headers): a live
    stream would need a listener's buffer of about a minute to ride out one
    resync. So this is store-and-forward: a 10 s recording is 23 s of signal
    (6.6 s of preamble and header, then the payload).
  - *Receive.* `Assembler` gathers voice by (call, session) like files; the speech
    so far can be rendered at any time (`voice_speech`: silence where frames are
    missing) and the whole is handed over when every frame is in (`take_voice`).
    The CLI writes `CALL-SESSION.wav` (8 kHz) and a `-partial.wav` for a
    transmission that did not finish.
  - *Keying.* `--vox` (CLI) and the VOX box (GUI) put 0.4 s of 1 kHz and a 0.3 s
    pause ahead of the signal so a radio on VOX is transmitting before the
    preamble; otherwise you key the radio, as for any transmission.
  - *Live.* `Voice: live` in the Transmit panel streams while the PTT is held
    (`link::live`, `phy::tx::LiveTx`, GUI `live.rs`): one very long segment (4095
    blocks, frame total 0) sent without a gap, each modem frame behind a number
    byte (speech 0..253, END, FILLER); a block takes whatever speech frames are
    ready and a filler for the rest, so the signal never stops; the receiver puts
    silence where numbers are missing, ignores fillers, plays what arrives through
    a jitter buffer (a second of speech before it starts, and again after running
    dry) and closes the channel after the end frame, the postamble (Over structure, 5) or six
    blocks with nothing decoded. The microphone is levelled (peak follower, at most +30 dB), brought
    to 8 kHz and encoded as it is spoken; the transmitter starts the preamble and
    header at once, so the speech said during the start-up (6.6 s at DS4) is sent
    afterwards, catching up at 1.17 times the codec's rate until it has caught up,
    then fillers. Simulated end to end (`tests/live.rs`: 12 dB AWGN, offset, clock
    error, 25 s talked): every speech frame arrives, the end is seen (the postamble
    within 3 s of the signal's end), the playout never runs dry, first speech heard 16 s after keying with the long header
    (about 10 s with CSK64). Through VB-Audio cables the GUI streamed from a cable
    "microphone" for 12 s, the CLI received header and five blocks and saved
    12.8 s of speech at the end frame; the speech player was checked by playing a
    tone through it into one cable and recording it from the other.
  - *Faster codecs (2026-10-20).* Codec2 1200 (48 bits per 40 ms) and 1600 (64 bits
    per 40 ms) for the fast modes, where a modem frame carries speech faster than
    it is spoken, so a live stream catches up with its backlog and the speech after
    the start-up waits less. The pairs, `link::voice::voice_mode`, with the payload's
    rate over the codec's (measured by the plan, whole codec frames to a modem
    frame, depth 1):

    | Codec | `2k4` | rate ratio | `3k` | rate ratio |
    |---|---|---|---|---|
    | 1200 | DS1 ½ (11 codec frames a modem frame) | 1.27 | DS2 ¾ (17) | 1.23 |
    | 1600 | DS1 ⅔ (11) | 1.27 | DS1 ½ (8) | 1.15 |

    The other pairs that look close do not keep up: DS2 ¾ on `2k4` carries 1200 at
    0.99 and DS1 ½ on `2k4` carries 1600 at 0.93 (whole codec frames; 1260 and 1680
    bit/s are not enough once the modem frame is cut into 6 and 8 bytes), so the
    link layer does not suggest them. `tx --voice --codec 1200` takes the pair's mode
    unless `--mode` says otherwise; the GUI's Codec2 choice does the same and its
    "Use it" button says which mode suits the codec. 1300, 1400, 2400 and 3200 are in
    the vendored core but not offered. A modem frame lost is a silent gap of 0.44 s
    (11 frames of 40 ms), 0.68 s or 0.32 s. Codec2 1600's type is the first over
    code 7, hence the version-1 header (Over structure, 2); its session is 0 to 31.
    Speech quality of these codecs against 450 and 700C was not measured here; it is
    the codec's, not the modem's.
  - *Keying:* see Radio I/O. *Not yet:* a short preamble.
- **ARQ** (M5): a connected mode for files on both stations' own codes, with
  selective repeat and ACK bitmaps sent at a robust mode.

## Receiver panels (GUI)

Built (egui, glow renderer), in DecSAT's instrument-panel look: dark faces,
hairline frames, one muted hue per quantity.

*Layout.* Tabs in the top bar: **RX**, **TX** and **RX + TX** (the transmit controls
in a left column beside the receiver, as in FreeDV; the last choice is
remembered). The side columns (transmit on the left, the stations' instruments on
the right) run the full height under the bar; the band view, the received
pictures and speech, the files and the log sit in the column between them, so
nothing cuts the instruments off. A recording or a picture being prepared keeps
going when the TX tab is not showing.

*Push to talk.* Voice in the Transmit panel is PTT style: hold the big button (or
the space bar) and speak, let go and what was said (at least half a second, at most
60 s) is sent as one over; "send when I let go" can be switched off to keep the
speech and send it with *Play to the sound card*. Received speech plays by
itself when a transmission is whole (*auto-play speech*). Everything you listen
to (the RF monitor, received speech, the speech previews) plays on the speakers
chosen in the bar, never on the output that feeds the radio. This is
store-and-forward, not live: a 6.6 s preamble and header come before the speech
(see Payloads, Voice); a live, streamed mode needs a shorter start-up first.
Checked through VB-Audio cables (`--ptt-for 6 --mic "CABLE-B Output" --tx-play
"CABLE-A Input"`: six seconds recorded from one cable, sent into the other,
received by the CLI as 6.4 s of speech).

*Waterfall.* A spectrum row is made 60 times a second (a 4096-point window every
800 samples) and written into the next row of a 900-row ring texture with a
one-row upload, so nothing is rebuilt per frame and 15 s of history scrolls at the
rows' own rate; the window is held to 60 frames a second (`DECDSSS_FPS=1` prints
the measured rate; a 165 Hz display would otherwise run the scopes at 165).
*RF monitor.* A button in the bar plays the audio the receiver is hearing on an
output device (volume, device), tapped before decoding; it never touches the
signal. Received speech and the speech previews are for the same speakers, never
the output that feeds the radio. *Stop.* A red **STOP TX** in the bar (and one in
the TX panel) cuts a transmission in progress at once. The release build is a
Windows GUI program (no console window); it attaches to the terminal it was
started from, if any, so `--version` and `--write-demo` still print.

- **Band view** — the audio spectrum 0–4 kHz with the three profile bands
  shaded, the noise floor (15th percentile of 300–3300 Hz), and a label at each
  locked carrier; a waterfall below it (levels from the floor to the strongest
  recent signal).
- **Per profile** — the acquisition search surface as a heatmap (frequency ×
  code phase, scaled to the noise, the peak ringed) and one row per candidate
  preamble with its A/B period bars.
- **Header** (a card per candidate once its preamble has ended, until the header
  decodes): the symbols of the alphabet being tried, despread as they arrive by
  the same per-symbol loop that decodes them (`rx::header::HeaderWatch`):
  - side by side, as on a station card: symbol × previous symbol (main finger,
    decided code), the carrier's mean turn a symbol taken off, and the Walsh
    correlator bank. CSK carries nothing in the phase, so the header being sent
    is one cluster at 1 and noise a ring; a longer header seen in a shorter
    alphabet is two clusters at ±1, about half the points each (derived, see
    Header above). Neighbouring symbols rather than a running phase reference:
    on the moderate channel a reference slipped 180° in a fade and painted the
    real header as the wrong one
  - in place of a progress bar: one bar a symbol across the whole header, its
    height the decision margin (decided code over the runner-up; noise alone is
    about 1 dB, derived, and drawn as a line), green in phase with the symbol
    before, red turned round, amber unclear; the slots still to come empty; the
    seconds in and needed, the alphabets that did not decode, and "a longer
    header?" when a fifth of the points sit at −1 with a median margin of 4 dB or
    more (display choices: `the_watch_tells_the_alphabet_sent_by_its_phase`
    measured 0–3 % for the alphabet sent and 34–41 % for a shorter one, AWGN at
    0 dB and moderate fading at +2 dB in-band)
  - the preamble's delay profile with the fingers the header uses, and
    early/prompt/late timing
- **Per station** (one card per decoding channel; dimmed between segments):
  - the mode and its link margin, laid out as DecSAT's DVB card: the margin is
    the Eb/N0 now less the Eb/N0 the mode needs, red below 0 dB, amber within
    1 dB; under it an Eb/N0 bar on a fixed 0–20 dB scale with a tick at the need,
    and the Eb/N0 and in-band SNR as numbers. The need is the AWGN no-loss figure
    under Verification where that mode was measured on that profile, else 4.0 dB
    (shown as ≈; most measured modes came out there). Eb/N0 is the symbol SNR over
    the last 1.5 s less 10·log10 of the information bits a symbol, pilots counted
    as air time (padding left out). CSK's SNR is the main path's alone, so with
    multipath its margin reads low.
  - symbol SNR, carrier and E−L as numbers
  - two plots side by side, each chosen by its own row of tabs (the choice is
    shared by every card and remembered): DS-PSK's despread constellation (Es/N0
    from the error) or symbol × previous symbol, CSK's Walsh correlator bank (a
    waterfall of the correlators whose bright trace is the decided codes) or the
    last symbol's bars; and for both early/prompt/late timing, the delay profile
    with the rake fingers (multipath, in chips), the chip scope (the last symbol's
    chips as received, and despread), the last codeword's soft bits, and the
    symbol SNR and carrier strips. A strip point is half a second: the signal
    powers summed over the noise powers summed. Left defaults to the
    constellation or the Walsh bank, right to the delay profile.
  - a frame map of the transmission
- **Files** — the received list (click to open), progress bars, the log.
- **Speech** — cards in the same strip: the waveform of the speech so far
  (frames not yet in are shaded red and silent), frames in against frames in
  all, and a *Play* button that plays it on the output device chosen in the
  Transmit window. A completed transmission is saved as `CALL-SESSION.wav`.
- **Pictures** — a strip under the band view with one card per picture (newest
  first, up to 8): the picture as far as it has arrived, upscaled, with bytes in
  against bytes in all and the scans decoded; it sharpens as blocks arrive.
- **Transmit** — *send as* file, picture or voice; callsign, profile, mode (with its rate and the SNR it
  needs), depth, preamble interval, coded frames, passes, level, output device;
  play to the sound card (at that device's own rate) or save a WAV. The spreading
  family is a pair of buttons (DS-PSK, CSK) above the mode. Beside the controls
  (under them in the narrow RX + TX column) a preview of what goes on air: the
  audio band with the signal's place (and for 500 Hz the other two slots
  outlined); the transmission to scale, VOX tone, each segment's preamble and
  header, data then coded frames, postamble, with its air time
  (`decdsss_link::timeline`: the sender's arithmetic without encoding a frame,
  tested equal to the planned segments and to `air_seconds`); and the mode's
  information rate, the SNR it needs, a frame's bytes and a block's length. Choosing an
  image switches on *As a picture*: air time a pass and the lowest quality
  accepted, and the window shows the picture exactly as it will arrive (decoded
  from the JPEG that will be sent), its size, quality, bytes and seconds of
  signal. Compression runs off the UI thread. Voice: record from any input
  device (up to 60 s) or choose a WAV, hear it as is or *as Codec2 will send it*
  (compressed and expanded again), pick 450 or 700C (default by profile), see the
  frames, seconds of signal and the payload's rate against the codec's; *Use it*
  switches to the profile's voice mode. Recording was checked through VB-Audio
  cable A (`--record-for 8 --mic "CABLE-A Output"` while the CLI played a
  transmission into the cable: a 7.4 s clip).

Sources: a sound card or virtual cable (stereo is mixed to mono), a WAV file at
any speed, or the built-in demo (two stations, one DS8 and one CSK64, on one
2.4 kHz channel through moderate fading). `--screenshot OUT.png --after SECS`
takes the picture from inside the window, for unattended checks.

## Radio I/O

Sound card in and out (cpal), with VOX or manual PTT, or the transmitter keyed by
the program (below). Files: WAV (mono audio, or stereo as I/Q) and raw `cf32`.
Live tests go through VB-Audio cable A, which is silent.

**Linux audio:** cpal reaches the sound card through WASAPI on Windows and, on
Linux, through PipeWire or ALSA. The `pipewire` feature of `decdsss-audio`
(default on in both apps) adds the PipeWire host, which cpal then prefers when a
PipeWire daemon is running; with no daemon, or built `--no-default-features`, it
is ALSA. `DECDSSS_AUDIO_HOST=alsa` (or `pipewire`) forces one, and `decdsss
devices` says which is in use. Choosing PipeWire costs a build-time dependency
(`libpipewire-0.3-dev`, plus `libclang-dev` for the bindings) and a run-time
link to `libpipewire-0.3.so`, so a system without PipeWire needs the
ALSA-only build. A saved device name from one host will not match the other's
(ALSA `hw:CARD=…` against PipeWire node names). Not yet run on a real Linux
machine: CI builds and tests it, nothing has opened a stream.

**Keying** (`decdsss-rig`, the GUI's *CAT / PTT…* button in the bar; a dialog laid
out as WSJT-X's "CAT and PTT Config"). The program only keys the transmitter: the
operator sets the frequency and the mode (USB, or data) on the radio, as with any
sound-card interface. What keys it (*PTT uses*):

- **VOX** (the default): nothing is keyed; the radio is on VOX or keyed by hand.
- **CAT**: a command to the rig, found on its port first. The IC-7300 by CI-V
  (address 94h as it comes) and the FT-710 by Yaesu's ASCII (`TX1;` / `TX0;`, the
  state read back with `TX;`) are driven directly; the code is voicemodem's `rig`
  crate (same author). Any other rig goes through Hamlib's `rigctld` over TCP
  (`T 1` / `T 0`; the model "Hamlib rigctld", address `host:port`, 4532 if none):
  Hamlib itself is C and is not linked in, so this is how the whole Hamlib list is
  reached without it. The CAT rate set is tried first and the rig's other rates
  after it.
- **DTR** or **RTS**: the line of a serial port goes high to transmit and low to
  receive, on the CAT port or on a port of its own (*PTT serial device*). Both
  lines are held low when a port is opened, so opening one never keys the rig.
  *Force RTS / DTR* hold a line of the CAT port high, for an interface that takes
  its power from it.

A transmission to the radio (files, pictures, text, voice overs, live voice) keys
the transmitter before its first sample and lets go after its last: the rig's
answer is waited for (CAT) and then a lead time (100 ms as it comes), and after the
last sample a tail time (100 ms) passes before the key is let go; both are set in
the dialog. A rig that will not key stops the transmission before any audio, with
the reason in the Transmit panel. STOP TX lets go at once. The transmitter is
never left keyed: the key is held by a guard that lets go however the transmission
ends, the rig's thread lets go when it is dropped, and the window lets go on exit.
Audio to the speakers (the RF monitor, received speech) never keys. *Test PTT* keys
with no audio until it is pressed again, and lets go by itself after 15 s. If the
rig was not found (switched off at start-up), the next transmission looks again.
The settings are remembered (`rig_*` in the preferences file); unattended runs
(`--screenshot`) neither read nor write them. `decdsss rig` lists the serial ports
and, with `--model` and `--port`, checks that a rig answers (it asks "ID;" or its
equivalent and never keys).

*Checked:* the CAT, line and `rigctld` code against stand-in rigs (`cargo test -p
decdsss-rig`, including `rigctld` over a real local TCP socket); the keying around
audio through VB-Audio cable A with a stand-in rig (`cargo test -p decdsss-gui --
--ignored keyed_around`); and, without keying, the real FT-710 on this PC answering
`ID;` with 0800 on both of its COM ports (the Standard one at 4800 baud, the
Enhanced one at 38400). *Not checked:* keying a real transmitter (Rory's to do),
the IC-7300 against a real one, CI-V and `rigctld` against real rigs, and DTR/RTS
on a real interface. `tx --play` keys the same way, with the same settings: it reads the `rig_*` lines of the GUI's
preferences file, and `--ptt vox|cat|dtr|rts`, `--rig`, `--rig-port`, `--rig-baud`
and `--ptt-port` change them for one run. **So once the dialog is set to key, a plain
`tx --play` keys the radio; a test into a virtual cable passes `--ptt vox`.** Ctrl-C
during playback lets go before the program ends; a settings error is refused before
anything is opened. Checked through cable A with a stand-in `rigctld` on a local
socket: it saw `T 1`, then `T 0` 17.5 s later, and the CLI printed "keyed", "playing"
and "let go" in that order (timing against the first sample was not measured here; the
GUI path's ordering is tested in `engine::tests`). Not done from the WSJT-X dialog: OmniRig, a separate PTT input
(CTS), the left-channel VOX tone (the Transmit panel's VOX box sends a tone ahead
of the signal) and the rest of Hamlib's models other than through `rigctld`.

**Waterfall ID** (`decdsss_phy::banner`; `tx --wfid`, the Transmit panel's *Waterfall ID*
box, `--tx-wfid` for the GUI's screenshot runs). Ahead of the signal, after the VOX
tone if there is one, a few seconds of audio whose spectrogram is the text
`VK2ABC DS8 1/2` (a CQ packet says `VK2ABC CQ`), the way fldigi's waterfall ID does
it. It is not part of the signal: no receiver decodes it and the preamble that
follows is found as before. A 5×7 dot-matrix font; each bitmap column is a tone and
each row 0.2 s (`ROW_S`) in which the tones of its lit pixels sound together, frequency
left to right and, because a waterfall puts the newest line at the top (this one does;
fldigi's and WSJT-X's do as far as is known, not checked here), the bottom row first. It stays inside the
band the signal will fill (tones 10 Hz inside its edges, so a banner in one 500 Hz
slot does not touch the next): 32 Hz between columns at most, and when the text
would need less than 16 Hz it goes one part to a line (the callsign over the mode) instead
of on one. Derived: on 2.4 kHz `VK2ABC DS8 1/2` is 83 columns at 29 Hz (7 rows, 1.4 s,
1.93 s with the ramps and the 0.5 s pause that follows); in a 500 Hz slot it is two
lines, 41 columns at 12 Hz (15 rows, 3.53 s), which a 4096-point spectrum at 48 kHz
(11.7 Hz a bin) only just resolves: it reads, if fuzzily, in this program's waterfall
and better in a display with finer bins. *Level:* every tone has the same
amplitude, so every lit pixel is as bright as the next, and the busiest row would
have the signal's RMS, but a comb of tones has the higher crest factor (measured:
9.8, 10.3 and 11.2 dB for the banner's peak over its busiest row's RMS, drawn at 2.4 kHz
and 500 Hz (`VK2ABC DS8 1/2`) and 3 kHz (`VK2ABC/QRP CSK256 3/4`), with Newman
phases; 18.8 dB with every tone at phase zero) than the signal (7.7-7.9 dB, peak 2.4-2.5
times RMS, 40 000 chips at each profile). A transmitter is set for the signal's
peaks, so the banner is scaled until its peak is no higher than the signal's
(`PEAK_PER_RMS` = 2.5 × the level): measured, its busiest row then comes out 1.8, 2.3
and 3.2 dB under the signal's RMS. Measured out of band: 37.3 dB (2.4 kHz) and 35.7 dB
(500 Hz slot) below the banner's own power. Tested (`phy::banner`,
`link/tests/banner.rs`, `gui tx::tests`): the spectrogram reads back, with a
Hann-windowed DFT of each row, as exactly the bitmap, newest line first; a CQ and a
text behind a banner are heard as without one (a CQ 8 dB under the noise in at least 3
of 4 runs, the bar the same test sets without a banner), only in the band they were sent in; and the banner alone is not
taken for a signal at 30 or 0 dB SNR. Looked at in the GUI's waterfall (a 2.4 kHz
CQ and a 500 Hz +500 slot CQ): both read. *Not done:* a banner at the end, a custom
text, a choice of size or speed (a slow waterfall, such as WSJT-X's, needs a row longer than
0.2 s to resolve the lines), the live-voice path checked on air (it is wired in
`live.rs` the same way; the sound-card start is not exercised by a test).

## Crate layout

| Crate | Role |
|---|---|
| `decdsss-core` | Profiles, modes, rates — the fixed numbers. |
| `decdsss-dsp` | RRC, pulse shaper, resampler, decimator, matched filter, interpolation, Hilbert, a seeded RNG. |
| `decdsss-fec` | QC-LDPC (design, encoder, layered min-sum), the interleaver, CRCs. |
| `decdsss-phy` | Codes, framing, the transmitter, the receiver (search, preamble, header, payload, refinement), the end-to-end simulator. |
| `decdsss-chan` | Channel simulator: AWGN, carrier offset and drift, clock error, Watterson fading with the CCIR presets. |
| `decdsss-link` | File container, sender planning (erasure-coded frames, segments, passes, air time and byte budget), voice framing, receive-side assembly (the readable front of a file still arriving, speech so far). |
| `decdsss-codec2` | Codec2 (vendored C core, LGPL-2.1) with a safe wrapper: modes 3200 … 450, bit-exact frame packing. |
| `decdsss-image` | Pictures: load, scale and compress to a byte budget as a progressive JPEG; decode one that is cut short. |
| `decdsss-audio` | Sound-card capture and playback (cpal: WASAPI, PipeWire, ALSA) and WAV files; copied from COFDMtv's `cofdmtv-io`. |
| `decdsss-rig` | Transmitter keying: CAT (IC-7300, FT-710, Hamlib `rigctld`) and DTR/RTS, on a thread that owns the rig. |
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
  Erasure-coded frames followed (any `K` distinct frames rebuild a file; see
  Payloads). *Done.* Left for later: a CI smoke test of the window.
- **M3** — pictures: progressive JPEG sized by air time, drawn as it arrives, in
  the CLI (`tx --picture --air`, a partial PNG saved by `rx` for a picture that
  did not finish) and the GUI. *Done* (see Verification).
- **M4** — voice, VOX/manual PTT, CAT keying. Link layer and CLI (`tx --voice`,
  `rx` writes the speech), GUI (record, send, receive cards, play) and VOX tone
  *done*; CAT/PTT keying *done* (Radio I/O; not yet tried on a real transmitter);
  the fast-mode fix (see Verification) to do.
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
| | DS2 ½ | 3.0 dB | −1.6 dB |
| | DS2 ⅔ | 3.5 dB | +0.2 dB |
| | DS2 ¾ | 4.0 dB | +1.2 dB |
| | DS1 ½ | 3.0 dB | +1.4 dB |
| | DS1 ⅔ | 3.5 dB | +3.2 dB |
| | DS1 ¾ | 4.0 dB | +4.2 dB |
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

The long header, a rate-¼ CSK256 codeword, decodes 12 of 12 times from −15 dB to
+5 dB in-band on AWGN and 9 of 12 at −17 dB (the preamble search finds the
preamble at −17 dB with a 50 ppm clock error and 55 Hz offset, and makes no false
detection in a minute of noise or of strong payload).

**Header alphabets** (`cargo test --release -p decdsss-phy header_alphabet_curve --
--ignored --nocapture`; 2.4 kHz, ten trials a point, with offsets and clock errors).
In-band SNR from which the header decoded: AWGN, 16 about −7 dB, 32 −7 dB (5 of 10
at −10), 64 −10 dB, 128 −13 dB, 256 −16 dB; moderate fading, 16 about +8 dB, 32 −1 dB,
64 about +5 dB (8 to 9 of 10 between −4 and +2), 128 −7 dB, 256 −4 dB (8 of 10 at
−13). The alphabets are not all as good for the payload as a long header: over 256
transmissions a point on the moderate channel, DS4 2/3 at 24 dB Eb/N0 lost 1.3 % of
its frames with CSK256, 0.9 % with 64, 1.2 % with 128 and 2.8 % with 32; DS8 1/2 at
10 dB 2.3 %, 3.2 %, 3.2 % with 256, 128, 64 (a short header gives the timing loop
less to start from). Hence 64 for DS4 and 128 for DS8 and DS16, and the receiver
tries only 64, 128 and 256. (These figures were taken with the receiver trying
every alphabet in turn, 16 first; the test now tries each alphabet alone, still
including 16 and 32, and has not been run since.)

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
interleaving, coded frames (`decdsss tx --parity`), further passes
(`--repeat`) and, later, ARQ are the remedies. With 10 coded frames on a
38-frame file (depth 4, DS8 1/2, 8 dB, drift 0.05 Hz) four of five fading
seeds delivered the file in one pass; the fifth lost 13 of 48 frames and was
left 3 short.

### Pictures

A 797×1200 photograph sent with `tx --picture --air 120` (DS8 ½, 2.4 kHz, depth 4,
25 % coded frames, two segments, 62×93 pixels, 1576 bytes), through moderate
fading with +8 Hz and 20 ppm: the file arrived byte for byte on all of five fading
seeds at both 9 dB and 12 dB in-band SNR. In `crates/decdsss-link/tests/picture.rs`
a 96×72 picture goes through the same chain in the test suite and is drawn after
every block on the way (the front and the scans grow monotonically).

**A limit found (see Voice, "The DS4 floor"):** the same photograph at DS4 3/4
(147×222, 5319 bytes, two segments) through moderate fading was delivered on four
of five seeds at 25 dB (seeds 1, 2, 4, 5); on the fifth the whole second segment
(28 of 68 frames) was lost at every SNR from 6 to 25 dB, so it is not noise. The
delay-spread explanation I first gave for it (self-interference at SF 4) did not
hold up; what was found, and the small change made, is under Voice. That
transmission was not re-run after the change.

### Voice

An 11.2 s clip of real speech (Codec2's `ve9qrp_10s` sample) sent with
`tx --voice` at DS4 ⅔ on 2.4 kHz, depth 2: 7 frames, 23 s of signal; received
through AWGN at 6 dB in-band with all frames, as speech of the same length. Both
voice modes (450 on 2.4 kHz, 700C on 3 kHz) run through the simulator with noise,
offset and clock error in `crates/decdsss-link/tests/voice.rs`.

On the Watterson channels (12 and 20 dB in-band SNR, six seeds each): *good*
12 of 12 transmissions complete; *moderate* 10 of 12 (seed 5 lost one block, two
frames, at both SNRs).

**The DS4 floor.** The loss at high SNR is not noise. Over 64 transmissions at
24 and at 30 dB Eb/N0 on the moderate channel (`decdsss sweep ... --trials 64`),
DS4 2/3 lost 0.39 % of its frames (one block, at either SNR), DS4 3/4 0.59 %, and
DS8 1/2 nothing. My first explanation, interference between two paths 2 chips
apart at spreading factor 4, did not survive a closer look: a hunt with a
genie start (true timing, carrier and paths given, so the preamble and header are
out of it; `cargo test --release -p decdsss-phy debug_genie_floor -- --ignored
--nocapture`, with `DEBUG_TRIAL`, `DEBUG_NONOISE`, `DEBUG_FADE`, `DEBUG_ERRS` and
`DEBUG_FREEZE=refresh|adopt|ghosts|all` to look into one) loses 2 frames in 960 at
30 dB, with the same block failing without any noise at all. The failing block
had lost its second finger (two paths, both faded for about a second, one block
after another), and freezing the post-block refresh (the refinement of timing,
clock and carrier from the decoded chips, and the re-pick of the fingers) makes the
loss go away, so the refresh and the fingers are implicated, not the SNR.
*Change made:* a path that drops out of the measured profile stays a finger for
three blocks (`set_fingers`, a ghost at 0.3 of the main path), as a path in a fade
comes back. Measured over the same 64 transmissions (moderate fading, offset 5 Hz,
10 ppm): DS4 2/3 at 30 dB 0.39 % to 0, at 14 dB Eb/N0 0.59 % to 0.20 %, at 10 dB
8.59 % to 8.20 %; DS8 1/2 at 10 dB 1.95 % to 0.78 %, at 6 dB 31.3 % to 30.1 %;
CSK64 and the high-SNR points unchanged at 0. The genie hunt still loses its two
frames, so the cause is not fully understood: the refined track after a fade is the
next thing to look at.

### G-RAKE against the rake (2026-10-19)

One build, the G-RAKE switched off by `DECDSSS_RAKE_ONLY` for the comparison,
`decdsss sweep --trials 128 --offset 5 --clock-ppm 10`, depth 4, 2.4 kHz, Eb/N0
6 to 30 dB in 4 dB steps: DS4 3/4, 2/3, 1/2 and DS8 1/2 on moderate and on good
fading, and DS4 3/4 and DS8 1/2 on AWGN. Frame loss was the same to the frame at
every point but three, where the G-RAKE lost one or two frames fewer (DS4 3/4:
good 18 dB 7.81 % to 7.32 %, moderate 6 dB 89.06 % to 88.96 %, AWGN 4 dB
34.38 % to 33.79 %). The receiver took 1.6 % longer.

Why so little: the Watterson paths are 0.96 and 1.92 chips apart, near whole
chips, and CLEAN puts the fingers on them, so the rake's decisions are already
nearly clean where the frames are lost (at 30 dB on moderate fading 0 to 5 of
2310 hard bits differ between the two). The loss at high SNR is whole
transmissions: 3.12 % of DS4 3/4 on moderate fading is four of 128 transmissions,
headers decoded. That is the receiver losing the signal, not combining badly.
The G-RAKE is kept on: it can only add frames, and it is what a faster mode (SF 2
or 1) would need, where the rake's 3 dB ceiling is the whole symbol SINR.

Off air (40 m, VK4SUS heard on the ironstonerange WebSDR, nine recordings), an
offline study against the known preamble (a least-squares channel fit in 30 ms
windows, held-out chips) put the in-band SNR at 1 to 5.5 dB: the WebSDR's AGC
and SSB filter make the band look 25 dB above a floor that is outside the
filter. There a chip-level LMMSE gained 0.7 dB of chip SINR over a rake on
average (0 to 1.2 dB a recording), and the receiver decoded the same frames with
and without the G-RAKE.

### Dropped and repeated audio (2026-10-19)

`rx::payload::tests::sync_slip_curve` (ignored): six blocks of depth 2, a stretch
of audio cut out (−) or silence put in (+) in the middle of block 2, four seeds,
offset 3 Hz, 20 ppm; AWGN at Eb/N0 10 dB, moderate fading at 20 dB. Frames
received of 48:

| Mode, channel | before | with the rewind |
|---|---|---|
| DS4 2/3 AWGN, ±1 to ±50 ms | 16 (every block after the cut lost) | 48 at every size |
| DS8 1/2 AWGN, ±1 to ±50 ms | 16 to 30 (16 from 5 ms) | 48 at every size |
| CSK64 1/2 AWGN, ±1 to ±50 ms | 40 at ±1 and +5 ms, 16 at −5 ms and from ±20 ms | 48 at every size |
| DS4 2/3 moderate, ±5 to ±50 ms | 14 | 32 to 40 |
| DS8 1/2 moderate, ±5 to ±50 ms | 16 to 22 | 44 to 46 |
| CSK64 1/2 moderate, ±5 to ±50 ms | 16 to 40 | 48 |
| no cut (AWGN; moderate) | 48; DS4 46, DS8 42, CSK64 48 | 48; DS4 46, DS8 46, CSK64 48 |

On moderate fading DS4 still often loses the block the slip is in (the search
needs the signal back from a fade to see where it went) but no longer the blocks
after. DS8 on moderate fading reports a few slips with no cut at all; they cost
nothing there (46 against 42 frames without the rewind). The on-air recordings in
`reference/` (27, from two WebSDRs and a KiwiSDR on 40 and 20 m) decode exactly
the same frames with and without it.

With no cut at all it is never worse and sometimes better: one build, the
rewind switched off by `DECDSSS_NO_REWIND` for the comparison, `decdsss sweep
--trials 128 --offset 5 --clock-ppm 10`, depth 4, Eb/N0 6 to 30 dB. Frame loss,
off → on, where it moved:

| Mode | Channel | Eb/N0 | off | on |
|---|---|---|---|---|
| CSK64 1/2 | good | 14 / 18 / 22 / 26 / 30 dB | 10.55 / 4.30 / 5.86 / 4.59 / 3.71 % | 4.79 / 0.78 / 1.56 / 0.78 / 0 % |
| CSK64 1/2 | moderate | 10 / 14-22 dB | 5.27 / 0.39 % | 3.52 / 0 % |
| DS4 3/4 | good | 14 to 30 dB | 14.06 to 5.18 % | 13.28 to 4.39 % (0.8 less at each) |
| DS4 2/3 | good | 14 to 30 dB | 10.35 to 4.00 % | 9.57 to 3.22 % |
| DS4 2/3 | moderate | 10 / 14 dB | 8.11 / 4.39 % | 7.52 / 3.22 % |
| DS8 1/2 | moderate, good | 10 dB | 2.54, 5.37 % | 1.17, 4.59 % |

Everything else (DS4 3/4 moderate, the rest of DS8, AWGN DS4 3/4 and CSK64 1/2
at their thresholds) the same to the frame. So part of the floor at high SNR on
fading was the track losing the signal for good after a deep fade, which the
search now finds again. What is left of it (DS4 about 3 % on moderate fading at
any SNR) is still to be found.

Not covered: a slip in the preamble or the header (the header is one codeword;
its CSK256 symbols are long), one larger than 60 ms, and one in the last
quarter second or so of a block, which costs that block (a block cannot be
rewound once decoded).

### DS2 and DS1 (2026-10-20)

SF 2 and 1 extend the DS-PSK ladder: on `2k4`, DS2 is 840 / 1120 / 1260 bit/s and
DS1 1680 / 2240 / 2520 bit/s at rates ½ / ⅔ / ¾ (derived: twice and four times
DS4's; `decdsss modes`). The air interface is otherwise as it was: the same header
(CSK64, which `header_alphabet_for` already gave every SF up to 4), the same codes,
the same G-RAKE. DS1 ¾ is 2520 bit/s in 2400 Hz, +0.2 dB between its in-band SNR and
its Eb/N0 (derived, `snr_offset_db`).

**AWGN** (table above; `decdsss sweep --mode ds1-3/4 --ebn0 1 8 0.5 --trials 24`,
192 frames a point, the carrier and the clock spread as for the other rows): both
decode with no change to the receiver, and need no more Eb/N0 than DS4 does (DS4 ½
re-measured with the same command: 4.0 dB, as before). On `3k` and `500`, DS2 ½ and
DS1 ½ delivered all 96 frames of 12 transmissions at 7 dB Eb/N0 — a check, not a
threshold.

**Moderate fading broke them, and the cause was the channel estimate.** First run
(moderate fading, 30 dB Eb/N0, the whole receiver, 32 transmissions, 8 frames each):
DS4 ¾ lost 0 %, DS2 ¾ 28 %, DS1 ¾ 89 %, with every header decoded (DS1 ½, 48
transmissions: 43 % with or without the rewind and the G-RAKE); a genie start (true
timing, carrier and paths, `debug_genie_floor`) lost 654 of 960 DS1 ¾ frames, so the
fault was in combining, not in acquisition. The pilot gains were smoothed over three
pilots ([1 2 1]/4) at every SF. A pilot gain is a sum over L chips, and the thermal
noise and the other paths' chips both average down as 1/L, so a raw gain is 1/L as
good (derived); pilots are one symbol in eight, so at SF 1 they come four times as
often as at SF 4 and three of them span 8 ms, not 33 ms: the estimate was 6 dB worse
than SF 4's (10·log₁₀ 4, derived) and, with the other path's unknown data chips in
it, sat at an error that more signal does not lessen. Now the triangle spans the
same time at every SF (SF 2: 7 pilots either side, SF 1: 15; SF 4 and above are as
they were), and the carrier is measured from pilots the same interval apart (SF 1:
four pilots, not one). DS4 was checked frame for frame against the old build
(moderate fading, 6 to 30 dB, 32 transmissions a point: identical). DS1 ¾ then lost
15 % at 30 dB, DS2 ¾ 12 %.

*A longer span helps the slow fades and hurts the fast ones.* Triangle span as a
multiple of SF 4's, 64 transmissions, DS1 ¾ at 30 dB on moderate fading and DS1 ½
and DS2 ½ at 26 dB on flutter fading (frames lost):

| Span | DS1 ¾ moderate | DS1 ½ flutter | DS2 ½ flutter |
|---|---|---|---|
| ×1 | 13.7 % | 35 % | 2.3 % |
| ×2 (kept) | 6.3 % | 45 % | 3.1 % |
| ×3 | 6.1 % | 77 % | 18 % |
| ×4 | 5.7 % | 96 % | 54 % |

×2 is the compromise (`SMOOTH_STRETCH_FAST`). An estimator that picked the span from
the pilots themselves (leave-one-out) would get the best of both; it was not built,
because it gains under a point on moderate fading over ×2.

**Moderate fading, frame loss (%)**, 2.4 kHz, depth 4, two blocks (8 frames), 128
transmissions a point (1024 frames), carrier offset 5 Hz and clock 10 ppm around
which the trials are spread (the settings of the G-RAKE study above); `decdsss sweep
--mode ds1-3/4 --ebn0 6 30 4 --trials 128 --fading moderate --offset 5 --clock-ppm
10`:

| Mode | 6 dB | 10 | 14 | 18 | 22 | 26 | 30 |
|---|---|---|---|---|---|---|---|
| DS4 ½ | 48.4 | 2.7 | 1.1 | 0 | 0 | 0.8 | 1.6 |
| DS2 ½ | 35.2 | 3.6 | 1.7 | 1.6 | 1.6 | 3.0 | 3.7 |
| DS1 ½ | 46.5 | 13.5 | 5.4 | 4.6 | 4.4 | 3.9 | 3.7 |
| DS4 ⅔ | 75.1 | 7.5 | 3.2 | 3.1 | 3.0 | 3.0 | 3.9 |
| DS2 ⅔ | 58.6 | 11.5 | 3.8 | 1.8 | 1.8 | 1.7 | 1.7 |
| DS1 ⅔ | 68.6 | 25.3 | 8.5 | 4.2 | 3.9 | 3.9 | 3.5 |
| DS4 ¾ | 89.0 | 16.2 | 3.1 | 3.1 | 3.1 | 3.1 | 2.3 |
| DS2 ¾ | 75.5 | 23.0 | 9.2 | 7.2 | 6.9 | 7.0 | 6.6 |
| DS1 ¾ | 81.2 | 40.7 | 14.6 | 6.9 | 5.7 | 5.5 | 5.1 |

DS4's floor (0 to 3.9 % from 18 dB up; the 3 % found earlier) is about the height
of the new modes' at ½ and ⅔: DS2 1.6 to 3.7 %, DS1 3.5 to 4.6 % from 18 dB up. At ¾
they are higher, DS2 6.6 to 7.2 % and DS1 5.1 to 5.7 % from 22 dB up, against DS4's
2.3 to 3.1 %. The loss does not come down with a deeper interleaver: at depth 8 (one block
is then 2.7 s at DS1, derived) DS2 ½ is 2.1 to 3.7 % from 14 dB up, DS1 ½ 3.7 to 4.7
%, DS2 ¾ 4.7 to 6.3 %, DS1 ¾ 8.1 to 8.3 % from 18 dB up (the same sweep with
`--depth 8`, 2048 frames a point), so it is not the outage of a short block but the
equaliser's limit: the symbol SINR at SF 1 is the chip SINR, and the channel
estimate is the pilots' (derived: no despreading gain to average the estimate's
error).

**The G-RAKE matters here.** The same sweeps with `DECDSSS_RAKE_ONLY=1` (the rake
alone, maximal-ratio combining), frame loss at 10 / 18 / 30 dB:

| Mode | rake, 10 dB | 18 | 30 | G-RAKE, 10 dB | 18 | 30 |
|---|---|---|---|---|---|---|
| DS4 ¾ | 16.2 % | 3.1 | 2.3 | 16.2 % | 3.1 | 2.3 |
| DS2 ½ | 4.3 % | 1.6 | 3.7 | 3.6 % | 1.6 | 3.7 |
| DS1 ½ | 24.0 % | 6.7 | 5.5 | 13.5 % | 4.6 | 3.7 |
| DS2 ¾ | 37.3 % | 7.9 | 6.9 | 23.0 % | 7.2 | 6.6 |
| DS1 ¾ | 83.4 % | 58.8 | 53.4 | 40.7 % | 6.9 | 5.1 |

DS4 does not care, as measured before. At SF 1 the rake's chip SIR stops at 3 dB for
two equal paths a chip apart (derived, `grake.rs`), which is the symbol SINR there,
and DS1 ¾ loses half its frames at any SNR; the G-RAKE removes the ceiling. This is
the fast mode the G-RAKE was kept for.

**Other channels** (½ rate, 26 dB Eb/N0, 64 transmissions, depth 4; frames lost):

| Channel | DS4 ½ | DS2 ½ | DS1 ½ |
|---|---|---|---|
| good | 0.6 % | 6.1 % | 3.5 % |
| moderate | 0 | 3.9 % | 3.3 % |
| poor | 3.1 % (2 headers missed) | 0 | 0 |
| flutter | 3.9 % (1 header missed) | 3.1 % | 44.5 % |

DS1 does not survive flutter fading (it lost 35 % even with SF 4's span, so the
longer span is not the whole of it; the cause was not looked into); DS2 does.
Neither is measured on `500` under fading; `3k` is below (*Faster voice*).

**Not covered: a slip in the audio.** `sync_slip_curve`
(`SLIP_MODES=ds2-1/2,ds1-1/2`) cuts or repeats 1 to 50 ms in the middle of block 2
on AWGN at 10 dB Eb/N0: DS4 and DS8 get every frame (Dropped and repeated audio,
above); DS2 ½ gets 16 to 32 of 48 frames at most sizes (46 at +5 ms): the blocks
before the cut and none after, except in about half the transmissions, where the
rewind followed it. DS1 ½ gets 16 of 48 at every size, every block after the cut
lost. (On moderate fading at 20 dB the same, except a 1 ms cut or repeat: DS2 ½ 48
and 41 of 48, DS1 ½ 40 and 27.) The rewind's watch judges the energy a symbol
collects, and at SF 1 that is |r|² whatever the scrambler's alignment (derived: the
scrambler chips have unit modulus), so a lost signal looks like a present one; at SF
2 the aligned sum is 3 dB over the misaligned one (derived) and the test caught the
slip in about half the transmissions (measured above). A coherent statistic across
pilots (the gain products) would see it; it was not built. Until then a glitch in
the audio costs a DS2 or DS1 transmission the rest of its segment, so shorten
`--resync` for them.

### Faster voice: Codec2 1200 and 1600 (2026-10-20)

**Speech through the whole chain** (`tests/voice.rs`, the CLI): 6 s (test) or 12 s
(CLI) of speech, 12 to 14 dB in-band SNR on AWGN, a carrier offset of 4 Hz and a
clock error of 10 ppm, depth 2. All four pairs arrive with no frame lost and the
speech the length that was sent (the CLI's 12 s of speech took 28, 28, 18 and 38
modem frames for `2k4` 1200, `2k4` 1600, `3k` 1200 and `3k` 1600; the plan's rate
ratios at depth 2 were 1.28, 1.28, 1.24 and 1.16, a little off the depth-1 ratios in
the table above).

**The modes' thresholds**, the same method as the AWGN table (24 transmissions a
point, 0.5 dB steps; no frame lost from): `2k4` DS1 ½ 3.0 dB Eb/N0, DS1 ⅔ 3.5 dB
(those two are in the table above); `3k` DS2 ¾ 4.5 dB (in-band +1.7 dB) and DS1 ½
3.0 dB (+1.4 dB). On moderate fading (128 transmissions a point, depth 4, 8 frames
each, offset 5 Hz, clock 10 ppm; frame loss % at 6 / 10 / 14 / 18 / 22 / 26 / 30 dB
Eb/N0):

| `3k` mode | 6 | 10 | 14 | 18 | 22 | 26 | 30 |
|---|---|---|---|---|---|---|---|
| DS4 ¾ (700C's mode) | 81.0 | 17.0 | 1.9 | 1.9 | 1.9 | 1.9 | 1.9 |
| DS2 ¾ (1200) | 61.8 | 16.9 | 4.6 | 4.0 | 2.7 | 2.4 | 2.4 |
| DS1 ½ (1600) | 39.5 | 12.1 | 2.7 | 2.7 | 2.7 | 2.7 | 2.7 |

(`2k4` DS1 ½ and ⅔ are in the DS2 and DS1 table: floors 3.5 to 4.6 %.) For speech a
lost frame is a gap, not a retransmission: from 18 dB Eb/N0 up, 2.4 to 4.6 % of the
modem frames silent on a moderate HF path for these four pairs, against 1.9 % for
700C on DS4 ¾.

**Live voice** (`tests/live.rs`, in simulated time, 12 dB AWGN, offset 5 Hz): all
the speech arrives, the end frame and the postamble are seen (within 3 s of the
signal's end), the playout never runs dry. `2k4` 1200 on DS1 ½, 25 s talked: first
speech heard 8.4 s into the signal (the lead is 6.6 s), 57 speech frames, 2 fillers
(the backlog of the start-up is worked off). `3k` 1600 on DS1 ½, 50 s talked: first
speech at 6.9 s (lead 5.3 s), 157 speech frames, 14 fillers; at 1.155 times the
codec's rate the 5.3 s backlog takes 5.3 / 0.155 = 34 s to work off (derived), so a
25 s talk would send none.

*A fault found on the way:* the postamble that ends a live stream is asked about a
second into each block (Over structure, 5), and a DS1 or DS2 block at depth 2 is
shorter than that (0.69 s at DS1 ½ on `2k4`), so the block ended before the samples
for the question were in and the postamble was never reported: the stream still
closed on its end frame, but not on the postamble. The question now stays open
across such a block (`eot_pending`) and is answered when the samples are in. A block
longer than the postamble and its lags never takes that path: the live streams that
existed, DS4 ⅔ and DS4 ¾ from depth 1 up, have blocks of 1.1 s or more.

### Not yet measured

The 3 kHz and 500 Hz profiles on fading (DS2 and DS1 included); flutter fading
beyond the one point above; voice modes (no
Codec2 yet); the effect of in-band interference; two stations at once beyond
the functional test in `rx::tests` (equal levels, different codes, 12 s
apart).
