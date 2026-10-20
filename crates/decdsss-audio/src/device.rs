// made by CasualArclamp
// Copied from COFDMtv's `cofdmtv-io` (same author), itself from DecDRM's `decdrm-io`.
//! Sound-card enumeration and stream-configuration selection (cpal).

use std::sync::Mutex;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{SampleFormat, SupportedStreamConfig, SupportedStreamConfigRange};

use crate::{AudioFormat, Direction, Error, Result};

/// One supported configuration range of a device, as reported by the driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigRange {
    /// Channel count.
    pub channels: usize,
    /// Lowest supported sample rate in Hz.
    pub min_sample_rate: u32,
    /// Highest supported sample rate in Hz.
    pub max_sample_rate: u32,
    /// Native sample type, e.g. `"f32"`, `"i16"`, `"i24"`. Streams always hand you `f32`.
    pub sample_format: String,
}

/// Description of a sound-card device.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// Human-readable name, e.g. `"CABLE Output (VB-Audio Virtual Cable)"`. Pass it (or any
    /// unique part of it) to [`InputOptions::device`](crate::InputOptions::device) /
    /// [`OutputOptions::device`](crate::OutputOptions::device).
    pub name: String,
    /// Stable backend identifier (survives reboots and renames; good for config files), if
    /// the backend provides one. Also accepted wherever a device name is.
    pub id: Option<String>,
    /// Audio API in use, e.g. `"WASAPI"`, `"PipeWire"` or `"ALSA"`.
    pub host: String,
    /// Whether this is an input or output device.
    pub direction: Direction,
    /// `true` for the system's default device of this direction.
    pub is_default: bool,
    /// Format the device prefers (shared-mode mix format on Windows), if it reports one.
    pub default_format: Option<AudioFormat>,
    /// All configuration ranges the device reports.
    pub configs: Vec<ConfigRange>,
}

impl DeviceInfo {
    /// Distinct sample rates covered by [`configs`](Self::configs), ascending. Continuous
    /// ranges contribute their end points plus any common rate inside them.
    pub fn sample_rates(&self) -> Vec<u32> {
        const COMMON: [u32; 8] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100];
        const COMMON_HI: [u32; 4] = [48000, 88200, 96000, 192000];
        let mut rates: Vec<u32> = Vec::new();
        for c in &self.configs {
            rates.push(c.min_sample_rate);
            rates.push(c.max_sample_rate);
            rates.extend(
                COMMON
                    .iter()
                    .chain(COMMON_HI.iter())
                    .copied()
                    .filter(|r| (c.min_sample_rate..=c.max_sample_rate).contains(r)),
            );
        }
        rates.sort_unstable();
        rates.dedup();
        rates
    }

    /// Distinct channel counts offered, ascending.
    pub fn channel_counts(&self) -> Vec<usize> {
        let mut ch: Vec<usize> = self.configs.iter().map(|c| c.channels).collect();
        ch.sort_unstable();
        ch.dedup();
        ch
    }
}

/// Lists the input (capture) devices of the default audio host.
pub fn list_input_devices() -> Result<Vec<DeviceInfo>> {
    list_devices(Direction::Input)
}

/// Lists the output (playback) devices of the default audio host.
pub fn list_output_devices() -> Result<Vec<DeviceInfo>> {
    list_devices(Direction::Output)
}

/// Environment variable that names the audio host to use instead of the default, e.g.
/// `DECDSSS_AUDIO_HOST=wasapi` on Windows or `=alsa` to bypass PipeWire on Linux.
/// Case-insensitive; see [`available_hosts`]. [`set_host`] takes precedence.
pub const HOST_ENV: &str = "DECDSSS_AUDIO_HOST";

/// The host chosen by [`set_host`], if any (it wins over [`HOST_ENV`]).
static HOST_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

/// Chooses the audio host for everything opened or listed from now on (`None`: back to
/// [`HOST_ENV`] and the default). Streams already running keep the host they were opened on.
/// An unknown name makes the listing and open functions fail with the available hosts named.
pub fn set_host(name: Option<&str>) {
    let name = name.map(str::trim).filter(|n| !n.is_empty());
    // A poisoned lock only means another thread panicked holding it; the value is plain data.
    *HOST_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = name.map(str::to_string);
}

/// The host asked for, by [`set_host`] or [`HOST_ENV`].
fn wanted_host() -> Option<String> {
    if let Some(name) = HOST_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return Some(name);
    }
    std::env::var(HOST_ENV)
        .ok()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// Whether `name` is MME (WinMM), which only exists on Windows.
fn is_mme(name: &str) -> bool {
    cfg!(windows) && (name.eq_ignore_ascii_case("mme") || name.eq_ignore_ascii_case("winmm"))
}

/// Whether streams and listings go through our own MME backend rather than cpal. On
/// Windows that is the default; WASAPI is one [`set_host`] or [`HOST_ENV`] away.
pub(crate) fn use_mme() -> bool {
    cfg!(windows) && wanted_host().is_none_or(|w| is_mme(&w))
}

/// Name of the audio host in use: the one asked for with [`set_host`] or [`HOST_ENV`], else
/// the platform default (MME on Windows; on Linux PipeWire when its daemon is running,
/// otherwise ALSA).
pub fn host_name() -> Result<String> {
    if use_mme() {
        return Ok("MME".to_string());
    }
    Ok(audio_host()?.id().name().to_string())
}

/// Names of the audio hosts this build can reach right now, in the order a picker should
/// list them: `["MME", "WASAPI"]` on Windows, e.g. `["PipeWire", "ALSA"]` on Linux.
pub fn available_hosts() -> Vec<String> {
    let mut hosts = Vec::new();
    if cfg!(windows) {
        hosts.push("MME".to_string());
    }
    hosts.extend(
        cpal::available_hosts()
            .into_iter()
            .map(|id| id.name().to_string()),
    );
    hosts
}

/// The cpal audio host every cpal path here goes through: the one asked for, or cpal's
/// default for the platform. (MME is not a cpal host; see [`use_mme`].)
fn audio_host() -> Result<cpal::Host> {
    quiet_alsa();
    match wanted_host() {
        Some(want) if !is_mme(&want) => host_by_name(&want),
        _ => Ok(cpal::default_host()),
    }
}

/// The available audio host called `want` (any case), or an error that lists the ones there are.
fn host_by_name(want: &str) -> Result<cpal::Host> {
    let hosts = cpal::available_hosts();
    // `HostId` is a small `Copy` enum, so `.copied()` turns the `&HostId` items into values.
    let Some(id) = hosts
        .iter()
        .copied()
        .find(|id| id.name().eq_ignore_ascii_case(want))
    else {
        let names: Vec<&str> = hosts.iter().map(|id| id.name()).collect();
        return Err(cpal::Error::with_message(
            cpal::ErrorKind::HostUnavailable,
            format!(
                "no audio host {want:?} ({HOST_ENV}); available: {}",
                names.join(", ")
            ),
        )
        .into());
    };
    Ok(cpal::host_from_id(id)?)
}

/// Lists the devices of one direction on the audio host in use (see [`host_name`]). A machine
/// without sound cards yields an empty list, not an error.
pub fn list_devices(direction: Direction) -> Result<Vec<DeviceInfo>> {
    #[cfg(windows)]
    if use_mme() {
        return Ok(crate::mme::list(direction));
    }
    let host = audio_host()?;
    let host_name = host.id().name().to_string();
    // On WASAPI the default device is a special handle that follows the system default (and
    // is not `==` to the enumerated endpoint), so identify it by its resolved id, or name.
    let default = default_device(&host, direction);
    let default_id = default.as_ref().and_then(|d| d.id().ok());
    let default_name = default.as_ref().map(device_name);
    let devices = match direction {
        Direction::Input => host.input_devices()?.collect::<Vec<_>>(),
        Direction::Output => host.output_devices()?.collect::<Vec<_>>(),
    };
    Ok(devices
        .into_iter()
        .map(|dev| {
            let configs = supported_ranges(&dev, direction)
                .unwrap_or_default()
                .into_iter()
                .map(|r| ConfigRange {
                    channels: usize::from(r.channels()),
                    min_sample_rate: r.min_sample_rate(),
                    max_sample_rate: r.max_sample_rate(),
                    sample_format: r.sample_format().to_string(),
                })
                .collect();
            let id = dev.id().ok();
            let name = device_name(&dev);
            let is_default = match (&id, &default_id) {
                (Some(a), Some(b)) => a == b,
                _ => default_name.as_deref() == Some(name.as_str()),
            };
            DeviceInfo {
                name,
                id: id.map(|id| id.to_string()),
                host: host_name.clone(),
                direction,
                is_default,
                default_format: default_config(&dev, direction)
                    .ok()
                    .map(|c| AudioFormat::new(c.sample_rate(), usize::from(c.channels()))),
                configs,
            }
        })
        .collect())
}

/// Name of the system default device of `direction`, if there is one.
pub fn default_device_name(direction: Direction) -> Option<String> {
    #[cfg(windows)]
    if use_mme() {
        return crate::mme::default_device_name(direction);
    }
    let host = audio_host().ok()?;
    default_device(&host, direction).map(|d| device_name(&d))
}

/// alsa-lib writes to stderr whenever cpal, listing or opening sound cards, comes to one
/// it cannot open: the OSS emulation's `/dev/dsp`, a JACK server that is not running, and
/// the like. That is noise (cpal reports what fails), so on Linux this thread's alsa-lib
/// messages go into a buffer instead, a fresh one each time.
#[cfg(target_os = "linux")]
fn quiet_alsa() {
    let _ = alsa::Output::local_error_handler();
}

#[cfg(not(target_os = "linux"))]
fn quiet_alsa() {}

fn default_device(host: &cpal::Host, direction: Direction) -> Option<cpal::Device> {
    match direction {
        Direction::Input => host.default_input_device(),
        Direction::Output => host.default_output_device(),
    }
}

pub(crate) fn device_name(dev: &cpal::Device) -> String {
    match dev.description() {
        Ok(desc) => desc.name().to_string(),
        Err(_) => dev
            .id()
            .map(|id| id.to_string())
            .unwrap_or_else(|_| "<unnamed device>".to_string()),
    }
}

fn supported_ranges(
    dev: &cpal::Device,
    direction: Direction,
) -> Result<Vec<SupportedStreamConfigRange>> {
    Ok(match direction {
        Direction::Input => dev.supported_input_configs()?.collect(),
        Direction::Output => dev.supported_output_configs()?.collect(),
    })
}

fn default_config(dev: &cpal::Device, direction: Direction) -> Result<SupportedStreamConfig> {
    Ok(match direction {
        Direction::Input => dev.default_input_config()?,
        Direction::Output => dev.default_output_config()?,
    })
}

/// Finds a device by name: `None` selects the default device; otherwise an exact name or id
/// match wins, then a unique case-insensitive substring of the name.
pub(crate) fn find_device(direction: Direction, name: Option<&str>) -> Result<cpal::Device> {
    // The host is dropped when this returns, with the device still to be opened. That is fine:
    // a cpal device is plain data, and each PipeWire stream takes its own library-init guard.
    let host = audio_host()?;
    let Some(wanted) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return default_device(&host, direction).ok_or(Error::NoDefaultDevice(direction));
    };
    let devices: Vec<cpal::Device> = match direction {
        Direction::Input => host.input_devices()?.collect(),
        Direction::Output => host.output_devices()?.collect(),
    };
    let items = devices
        .into_iter()
        .map(|d| (device_name(&d), d.id().ok().map(|id| id.to_string()), d))
        .collect();
    pick_by_name(direction, items, wanted)
}

/// Picks the entry of `items` (name, backend id, device) that `wanted` names: an exact name
/// or id match wins, then a unique case-insensitive substring of the name.
pub(crate) fn pick_by_name<T>(
    direction: Direction,
    mut items: Vec<(String, Option<String>, T)>,
    wanted: &str,
) -> Result<T> {
    if let Some(i) = items
        .iter()
        .position(|(n, id, _)| n == wanted || id.as_deref() == Some(wanted))
    {
        return Ok(items.swap_remove(i).2);
    }
    let lower = wanted.to_lowercase();
    let hits: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, (n, _, _))| n.to_lowercase().contains(&lower))
        .map(|(i, _)| i)
        .collect();
    let list = |idx: &mut dyn Iterator<Item = usize>| {
        idx.map(|i| items[i].0.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match hits.as_slice() {
        [i] => {
            let i = *i;
            Ok(items.swap_remove(i).2)
        }
        [] => Err(Error::DeviceNotFound {
            direction,
            name: wanted.to_string(),
            available: list(&mut (0..items.len())),
        }),
        many => Err(Error::AmbiguousDevice {
            direction,
            name: wanted.to_string(),
            matches: list(&mut many.iter().copied()),
        }),
    }
}

/// The stream configuration picked for a device.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChosenConfig {
    pub config: cpal::StreamConfig,
    pub sample_format: SampleFormat,
}

impl ChosenConfig {
    pub fn format(&self) -> AudioFormat {
        AudioFormat::new(self.config.sample_rate, usize::from(self.config.channels))
    }
}

/// Sample types the stream callbacks can convert to/from `f32`.
pub(crate) fn is_convertible(fmt: SampleFormat) -> bool {
    matches!(
        fmt,
        SampleFormat::I8
            | SampleFormat::I16
            | SampleFormat::I24
            | SampleFormat::I32
            | SampleFormat::U8
            | SampleFormat::U16
            | SampleFormat::U32
            | SampleFormat::F32
            | SampleFormat::F64
    )
}

/// Preference among sample types (lower is better): float first, then widest integer.
fn format_rank(fmt: SampleFormat) -> u32 {
    match fmt {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        SampleFormat::I24 => 2,
        SampleFormat::F64 => 3,
        SampleFormat::I16 => 4,
        SampleFormat::U32 => 5,
        SampleFormat::U16 => 6,
        SampleFormat::I8 => 7,
        _ => 8,
    }
}

/// Chooses a stream configuration as close as possible to the preferred rate and channel
/// count. Channel count matters most (an I/Q input must have two channels), then the rate
/// (exact > higher > lower, because a lower rate loses bandwidth), then the sample type.
pub(crate) fn choose_config(
    dev: &cpal::Device,
    direction: Direction,
    want_rate: Option<u32>,
    want_channels: Option<usize>,
) -> Result<ChosenConfig> {
    let name = device_name(dev);
    let default = default_config(dev, direction).ok();
    let ranges: Vec<SupportedStreamConfigRange> = supported_ranges(dev, direction)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| is_convertible(r.sample_format()) && r.channels() > 0)
        .collect();

    let want_rate = want_rate
        .or(default.as_ref().map(|d| d.sample_rate()))
        .unwrap_or(crate::WORKING_RATE);
    let want_ch = want_channels
        .or(default.as_ref().map(|d| usize::from(d.channels())))
        .unwrap_or(2);

    let best = ranges
        .iter()
        .map(|r| {
            let rate = pick_rate(r.min_sample_rate(), r.max_sample_rate(), want_rate);
            let score = (
                channel_cost(usize::from(r.channels()), want_ch),
                rate_cost(rate, want_rate),
                format_rank(r.sample_format()),
            );
            (score, rate, r)
        })
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    match (best, default) {
        (Some((_, rate, r)), _) => Ok(ChosenConfig {
            config: cpal::StreamConfig {
                channels: r.channels(),
                sample_rate: rate,
                buffer_size: cpal::BufferSize::Default,
            },
            sample_format: r.sample_format(),
        }),
        // Some drivers list nothing but still report a usable default.
        (None, Some(d)) if is_convertible(d.sample_format()) => Ok(ChosenConfig {
            config: d.config(),
            sample_format: d.sample_format(),
        }),
        _ => Err(Error::NoUsableConfig(name)),
    }
}

/// The rate inside `[min, max]` closest to `want`, preferring standard rates.
fn pick_rate(min: u32, max: u32, want: u32) -> u32 {
    if (min..=max).contains(&want) {
        return want;
    }
    const STANDARD: [u32; 10] = [
        8000, 11025, 16000, 22050, 24000, 32000, 44100, 48000, 96000, 192000,
    ];
    let mut candidates: Vec<u32> = STANDARD
        .iter()
        .copied()
        .filter(|r| (min..=max).contains(r))
        .collect();
    candidates.push(min);
    candidates.push(max);
    candidates
        .into_iter()
        .min_by(|&a, &b| {
            rate_cost(a, want)
                .partial_cmp(&rate_cost(b, want))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(max)
}

/// Exact channel count is free; extra channels cost one each (the caller downmixes); too few
/// cost far more (an I/Q input must have two).
pub(crate) fn channel_cost(have: usize, want: usize) -> usize {
    if have == want {
        0
    } else if have > want {
        have - want
    } else {
        1000 + (want - have)
    }
}

/// Exact match is free; a higher rate costs a little (resampling, but no bandwidth lost); a
/// lower rate costs more (bandwidth lost).
pub(crate) fn rate_cost(rate: u32, want: u32) -> f64 {
    let (r, w) = (f64::from(rate), f64::from(want.max(1)));
    if rate == want {
        0.0
    } else if rate > want {
        1.0 + (r / w).ln()
    } else {
        10.0 + (w / r.max(1.0)).ln()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_found_by_name_in_any_case() {
        let default = cpal::default_host().id().name();
        assert!(host_by_name(&default.to_lowercase()).is_ok());
        assert!(host_by_name(&default.to_uppercase()).is_ok());
        let err = match host_by_name("no-such-host") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an unknown host was accepted"),
        };
        assert!(
            err.contains("no-such-host") && err.contains(default),
            "{err}"
        );
        assert!(available_hosts().iter().any(|h| h == default));
    }

    #[test]
    fn names_pick_exact_then_unique_part() {
        let items = || {
            vec![
                (
                    "CABLE-A Input (VB-Audio Cable A".to_string(),
                    Some("id-a".to_string()),
                    1,
                ),
                ("CABLE-B Input (VB-Audio Cable B".to_string(), None, 2),
                ("Speakers".to_string(), None, 3),
                ("Speakers 2".to_string(), None, 4),
            ]
        };
        let out = Direction::Output;
        assert_eq!(pick_by_name(out, items(), "cable-b input").unwrap(), 2);
        assert_eq!(pick_by_name(out, items(), "id-a").unwrap(), 1);
        // An exact name wins even when it is also part of another name.
        assert_eq!(pick_by_name(out, items(), "Speakers").unwrap(), 3);
        assert!(matches!(
            pick_by_name(out, items(), "Input"),
            Err(Error::AmbiguousDevice { .. })
        ));
        assert!(matches!(
            pick_by_name(out, items(), "nothing"),
            Err(Error::DeviceNotFound { .. })
        ));
    }

    #[test]
    fn mme_is_only_a_host_on_windows() {
        assert_eq!(is_mme("MME"), cfg!(windows));
        assert_eq!(is_mme("winmm"), cfg!(windows));
        assert!(!is_mme("wasapi"));
        assert_eq!(
            available_hosts().first().map(String::as_str) == Some("MME"),
            cfg!(windows),
            "MME is listed first"
        );
    }

    #[test]
    fn rate_preferences() {
        assert_eq!(pick_rate(8000, 192_000, 48_000), 48_000);
        assert_eq!(pick_rate(44_100, 44_100, 48_000), 44_100);
        assert_eq!(pick_rate(96_000, 96_000, 48_000), 96_000);
        // Prefer going up (96k) over losing bandwidth (44.1k) when both are offered.
        assert!(rate_cost(96_000, 48_000) < rate_cost(44_100, 48_000));
        assert_eq!(pick_rate(50_000, 60_000, 48_000), 50_000);
    }
}
