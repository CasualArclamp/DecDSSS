# decdsss-codec2

Codec2 (David Rowe, <https://github.com/drowe67/codec2>) for DecDSSS voice, with
a small safe Rust wrapper (`Codec2`, `Mode`, `pack_frames`/`unpack_frames`).

The C core in `c/` is **Codec2 at commit `9f5e2deebcdf702ae9fd71785879891bc38d2f4e`
(2023-07-16)**, the last before the 450 and 450PWB modes were removed upstream.
450 bit/s is the only mode that fits the 2.4 kHz channel with room to spare
(700C needs the 3 kHz profile). Licence: **LGPL-2.1** (`c/COPYING`); the sources keep
their copyright headers. DecSAT-style GPL-3.0-or-later code may link LGPL-2.1
libraries; relinking is possible because this crate is built from source.

## Changes to the upstream sources

Only what building with MSVC required, done by `vendor_codec2.py` (run it with a
checkout of that commit and a built `generate_codebook`):

- C99 variable-length arrays, which MSVC does not have, are declared with
  `VLA(type, name, n)` (`c/decdsss_vla.h`): the real thing on gcc/clang, `_alloca`
  on MSVC. The one two-dimensional case (`levinson_durbin`) is flattened, and `M`
  is an enum where the code always sets it to 4.
- The codebooks (`c/codebook*.c`) are what upstream's `generate_codebook` writes
  from `src/codebook/*.txt`, generated once.
- Only the files the speech modes need are included (no OFDM/FreeDV/LDPC).

## Things to know

- The decoder draws random phases for noise-like speech from a process-wide
  generator (`codec2_rand`), so two decodes of the same bits are the same speech
  but not sample for sample. Do not decode from several threads at once.
- Voice uses 8 kHz, 16-bit mono speech.
