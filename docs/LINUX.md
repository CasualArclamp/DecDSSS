<!-- made by CasualArclamp -->
# Installing DecDSSS on Linux

There are no prebuilt Linux binaries yet: you build from source, which takes a
Rust toolchain, a few development packages and two commands. This page is the
whole install; the [README](../README.md) says what the program does.

**What has been checked.** CI builds and tests the workspace on Ubuntu (the
latest runner image), both with PipeWire and as the ALSA-only build below, and
runs `decdsss modes`. Nobody has yet opened a sound-card stream, run the window
or keyed a rig on a real Linux machine, so expect rough edges and say what you
find.

## 1. What you need

- **Rust 1.95 or newer**, from [rustup.rs](https://rustup.rs) (the workspace's
  `rust-version`; distro packages are usually older).
- **A C compiler.** Codec2 is a vendored C core, built by `cc` during the build.
- **Development packages**, for the sound card and the window.

Debian and Ubuntu, the list CI installs (plus `build-essential` for the compiler):

```bash
sudo apt-get install -y build-essential pkg-config \
  libasound2-dev libpipewire-0.3-dev libclang-dev \
  libgl1-mesa-dev libxkbcommon-dev libx11-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

| Package | Why |
|---|---|
| `libasound2-dev` | ALSA, through cpal. |
| `libpipewire-0.3-dev` | PipeWire, through cpal. |
| `libclang-dev` | The PipeWire bindings are generated at build time (bindgen). |
| the `libgl…`, `libx…`, `libxcb…` lines | The window (eframe/egui). |

On Fedora the audio packages are `alsa-lib-devel`, `pipewire-devel` and
`clang-devel`. That is from the packages' usual names, not something we have
built on Fedora; for the window install the equivalents of the other lines.

## 2. Build and install

```bash
git clone https://github.com/CasualArclamp/DecDSSS.git
cd DecDSSS
cargo build --release -p decdsss-gui -p decdsss-cli

install -Dm755 target/release/decdsss-gui ~/.local/bin/decdsss-gui
install -Dm755 target/release/decdsss     ~/.local/bin/decdsss
```

`decdsss-gui` is the window (receive and transmit) and `decdsss` the command
line. `~/.local/bin` is on the `PATH` of most distributions; if `decdsss` is not
found, add it to yours. Without the `install` lines the programs are still in
`target/release/`.

To update, `git pull`, run the `cargo build` line again and repeat the
`install` lines.

### ALSA only

PipeWire is on by default. If your system has no PipeWire, or you do not want
the library, build without it:

```bash
cargo build --release --no-default-features -p decdsss-gui -p decdsss-cli
```

That build needs neither `libpipewire-0.3-dev` nor `libclang-dev`, and the
programs it makes do not link `libpipewire-0.3.so`. A default build does link it,
so on a machine without PipeWire's library it fails to start with a message about
`libpipewire-0.3.so.0`: install PipeWire's runtime library or use this build.

## 3. First run

```bash
decdsss modes            # the mode table; needs no sound card
decdsss devices          # the sound card in use and its inputs and outputs
decdsss-gui --demo       # the window on a built-in demo signal; needs no radio
```

`decdsss devices` starts with the audio host in use, for example `Audio host:
PipeWire (also ALSA; pick one with DECDSSS_AUDIO_HOST)`. The star marks the
system default device.

## 4. Sound: PipeWire and ALSA

- DecDSSS uses **PipeWire** when its socket (`pipewire-0`) is found in
  `$PIPEWIRE_RUNTIME_DIR` or `$XDG_RUNTIME_DIR`, and **ALSA** otherwise.
- To force one: `DECDSSS_AUDIO_HOST=alsa decdsss-gui`, or `=pipewire`. A name
  that is not available gives an error listing the ones that are. ALSA is the way
  to reach a raw `hw:` device while PipeWire is running.
- A device is chosen by name, or any unique part of its name, as `decdsss
  devices` prints it: `decdsss-gui --device "USB Audio"`. The window keeps your
  choice between runs.
- The two hosts name devices differently (ALSA `hw:CARD=…`, PipeWire by the
  node's description), so after switching host pick the device again.

On Windows the project is tested through a silent VB-Audio virtual cable. The
Linux equivalent has not been tried.

## 5. A rig on a serial port

DecDSSS never keys a transmitter on its own; CAT and PTT only act when you ask
the window to. To see the serial ports and check that a rig answers (this only
asks the rig what it is):

```bash
decdsss rig
decdsss rig --model ft710 --port /dev/ttyUSB0
```

Ports are listed by scanning `/sys/class/tty`; DecDSSS is built without libudev,
so it needs no extra library. Opening `/dev/ttyUSB0` or `/dev/ttyACM0` normally
needs your user to be in the group that owns it, `dialout` on Debian, Ubuntu
and Fedora (it differs on other distributions: `ls -l /dev/ttyUSB0`):

```bash
sudo usermod -aG dialout "$USER"     # then log out and in
```

That is the usual Linux arrangement, not something tested here. A Hamlib
`rigctld` over TCP is the other route and needs nothing from the system.

## 6. Where things go

| What | Where |
|---|---|
| Received files, pictures, speech and `messages.log` | `~/Documents/DecDSSS` (change it in the window, or `--out-dir DIR`) |
| The window's settings | `$XDG_CONFIG_HOME/DecDSSS/prefs.txt`, or `~/.config/DecDSSS/prefs.txt` |

To uninstall, delete the two programs from `~/.local/bin` and, if you want the
settings gone, `~/.config/DecDSSS`.

## 7. If something goes wrong

| What you see | Try |
|---|---|
| The build stops in `pipewire-sys` or `libspa-sys` about `libclang` | Install `libclang-dev`, or build ALSA-only (above). |
| The build stops looking for `libpipewire-0.3` or `alsa` | Install `libpipewire-0.3-dev` and `pkg-config`, or `libasound2-dev`. |
| `error while loading shared libraries: libpipewire-0.3.so.0` | This machine lacks PipeWire's library: install it, or use the ALSA-only build. |
| `decdsss devices` says ALSA while you run PipeWire | The socket was not found. Check `ls "$XDG_RUNTIME_DIR"/pipewire-0`; under `sudo` or in a container `XDG_RUNTIME_DIR` is often unset. |
| The device you chose last time is gone | You changed host (see section 4); choose it again. |
| `decdsss rig` shows no port, or opening it is refused | `ls -l /dev/ttyUSB*`, then the `dialout` step in section 5. |
