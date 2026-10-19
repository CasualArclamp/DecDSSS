<!-- made by CasualArclamp -->
# DecDSSS — working notes

Read [`docs/DESIGN.md`](docs/DESIGN.md) first: it holds the agreed scope, the
air interface, the crate layout and the milestone plan. Update it when a
decision changes.

## Commands

```bash
cargo check --workspace --all-targets   # fast feedback
cargo test --workspace                  # unit tests
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p decdsss-cli -- modes       # the mode ladder and its rates
cargo run --release -p decdsss-cli -- tx photo.png --call VK2ABC --picture --air 120 -o pic.wav   # a picture sized to the air time
cargo run --release -p decdsss-cli -- tx speech.wav --call VK2ABC --voice -o voice.wav   # Codec2 speech
cargo run --release -p decdsss-cli -- tx file.bin --call VK2ABC --profile 500 --slot +500 -o f.wav   # a 500 Hz slot (-500, 0 or +500)
cargo run --release -p decdsss-cli -- sweep --mode ds8-1/2 --ebn0 2 6 1   # frame loss vs Eb/N0, whole receiver
powershell -ExecutionPolicy Bypass -File scripts\build-portable.ps1   # portable exes into exe\ (git-ignored)
cargo run --release -p decdsss-gui -- --demo   # the window; --screenshot OUT.png --after 20 for pictures
```

## House rules

- **Licence:** GPL-3.0-or-later. Code copied from DecSAT, voicemodem or COFDMtv
  (same author) gets a comment naming where it came from; anything borrowed
  from third-party code credits its source at the function or module.
- **Rebuild the exes after every change** (Rory runs them from `exe\`): `powershell
  -ExecutionPolicy Bypass -File scripts\build-portable.ps1`, then say so. Build from a
  tree without half-done experiments; `exe\` is git-ignored and is not a release.
- **Never commit** recordings (`*.wav`, `*.cf32`), reference clones
  (`reference/`) or built exes.
- **Commit identity:** `Arclamp <45412977+CasualArclamp@users.noreply.github.com>`
  (set in this repo's local git config).
- Rory has a strong DSP background and is newer to Rust: explain Rust idioms
  in comments, not DSP basics. He checks DSP claims, so a number in a comment,
  README or DESIGN.md is either derived or measured, and says which.
- DSP is unusably slow unoptimised, so `[profile.dev]` runs at `opt-level = 1`
  with dependencies at 3. Prefer `--release` for anything timing-sensitive.
- Heavy test sweeps (threshold measurements, long fading runs) belong on CI or
  behind `#[ignore]`, not on Rory's machine by default.
- Live tests go through VB-Audio cable A (silent). Live radio tests are Rory's;
  never key a transmitter.
