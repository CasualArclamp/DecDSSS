<!-- made by CasualArclamp -->
# DecDSSS

A narrowband spread-spectrum modem for amateur radio. It uses direct-sequence
(DS-PSK) and cyclic code-shift-keyed (CSK) spreading inside an SSB voice
channel of 500 Hz, 2.4 kHz or 3 kHz, and carries digital voice, pictures and
files.

The receiver shows what it is doing:

- the acquisition surface (code phase × frequency)
- chip-phase correlation and the rake fingers on HF multipath
- early/prompt/late code tracking
- despread constellations and a chip scope
- the spread signal on the waterfall, sitting under the noise floor

Status: **M0** — the design and the skeleton. See [`docs/DESIGN.md`](docs/DESIGN.md)
for the air interface and the milestone plan.

```bash
cargo run -p decdsss-cli -- modes --profile 2k4
```

Spread spectrum below VHF is restricted in some countries (for example, US
Part 97 allows SS only on 222 MHz and up). Check your own licence conditions.

Licence: GPL-3.0-or-later.
