use std::{
    collections::HashMap,
    error::Error,
    fmt, fs,
    io::{self, Read},
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use nix::libc;

use crate::drivers::iio_imu::event::AxisData;

const DEFAULT_SAMPLE_RATE: f64 = 200.0;

/// The [MountMatrix] is used to define how sensors are oriented inside a device
/// https://github.com/torvalds/linux/blob/master/Documentation/devicetree/bindings/iio/mount-matrix.txt
#[derive(Clone, Debug)]
pub struct MountMatrix {
    pub x: (f64, f64, f64),
    pub y: (f64, f64, f64),
    pub z: (f64, f64, f64),
}

impl MountMatrix {
    /// Create a new [MountMatrix] from the given mount matrix string
    /// Example:
    /// "1, 0, 0; 0, 1, 0; 0, 0, 1"
    pub fn new(matrix_str: String) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let parts: Vec<&str> = matrix_str.split(';').map(|part| part.trim()).collect();
        if parts.len() != 3 {
            return Err("Not enough parts found in the matrix".into());
        }

        let mut matrix = MountMatrix::default();
        for (i, part) in parts.into_iter().enumerate() {
            let sub_parts: Vec<&str> = part.split(',').map(|part| part.trim()).collect();
            if sub_parts.len() != 3 {
                return Err("Not enough subparts".into());
            }
            let x = sub_parts.first().unwrap().parse::<f64>()?;
            let y = sub_parts.get(1).unwrap().parse::<f64>()?;
            let z = sub_parts.get(2).unwrap().parse::<f64>()?;

            match i {
                0 => matrix.x = (x, y, z),
                1 => matrix.y = (x, y, z),
                2 => matrix.z = (x, y, z),
                _ => (),
            }
        }

        Ok(matrix)
    }
}

impl Default for MountMatrix {
    fn default() -> Self {
        MountMatrix {
            x: (1.0, 0.0, 0.0),
            y: (0.0, 1.0, 0.0),
            z: (0.0, 0.0, 1.0),
        }
    }
}

impl fmt::Display for MountMatrix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "--- Mount Matrix ---\nx: {}, {}, {}\ny: {}, {}, {}\nz: {}, {}, {}",
            self.x.0,
            self.x.1,
            self.x.2,
            self.y.0,
            self.y.1,
            self.y.2,
            self.z.0,
            self.z.1,
            self.z.2,
        )
    }
}

/// A complete raw IIO record, or a retryable error when no complete record is
/// ready. Descriptors are owned by the source and refreshed by the source loop
/// after every lifecycle transition; this layer must never wait for samples.
pub trait RecordSource: fmt::Debug + Send {
    fn read_one(&mut self, buf: &mut [u8]) -> io::Result<()>;
    fn poll_fd(&self) -> RawFd;
}

#[derive(Debug)]
pub struct NonblockingFdSource {
    file: fs::File,
    kick_path: Option<PathBuf>,
    pending: Vec<u8>,
    filled: usize,
}

impl NonblockingFdSource {
    pub fn open(devnode: &Path, kick_path: Option<PathBuf>) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(devnode)?;
        Ok(Self {
            file,
            kick_path,
            pending: Vec::new(),
            filled: 0,
        })
    }
}

impl RecordSource for NonblockingFdSource {
    fn read_one(&mut self, buf: &mut [u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty IIO record",
            ));
        }
        if self.pending.len() != buf.len() {
            if self.filled != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "IIO record size changed",
                ));
            }
            self.pending.resize(buf.len(), 0);
        }
        if self.filled == 0 {
            if let Some(path) = &self.kick_path {
                // HID sensor hubs may need a raw read to request their next sample.
                // Like the direct sysfs fallback, this kernel operation can be slow
                // during resume; the raw-buffer fd itself always stays nonblocking.
                let _ = fs::read_to_string(path);
            }
        }
        // Read at most once per poll: partial records survive EAGAIN/EINTR without
        // being decoded, and the source loop remains responsive to lifecycle commands.
        let read = self.file.read(&mut self.pending[self.filled..])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "IIO buffer closed",
            ));
        }
        self.filled += read;
        if self.filled < self.pending.len() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        buf.copy_from_slice(&self.pending);
        self.filled = 0;
        Ok(())
    }

    fn poll_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

pub fn is_retryable_read_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

/// Parsed form of a `scan_elements/in_*_type` value, e.g. "le:s16/16>>0".
#[derive(Debug, Clone, Copy)]
pub struct ScanFormat {
    little_endian: bool,
    signed: bool,
    /// Bits actually allotted in the record (padded to a byte boundary).
    storage_bits: u8,
    /// Bits of real, significant data once shifted into place.
    real_bits: u8,
    shift: u8,
}

impl ScanFormat {
    fn parse(s: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let (endian, rest) = s
            .split_once(':')
            .ok_or_else(|| format!("malformed scan type '{s}' (missing endianness)"))?;
        let little_endian = match endian {
            "le" => true,
            "be" => false,
            other => return Err(format!("unknown endianness '{other}' in scan type '{s}'").into()),
        };

        let (sign, rest) = rest
            .split_at_checked(1)
            .ok_or_else(|| format!("malformed scan type '{s}' (missing sign)"))?;
        let signed = match sign {
            "s" => true,
            "u" => false,
            other => return Err(format!("unknown sign flag '{other}' in scan type '{s}'").into()),
        };

        let (bits_str, rest) = rest
            .split_once('/')
            .ok_or_else(|| format!("malformed scan type '{s}' (missing real bits)"))?;
        let real_bits: u8 = bits_str.parse()?;

        let (storage_str, shift_str) = rest
            .split_once(">>")
            .ok_or_else(|| format!("malformed scan type '{s}' (missing shift)"))?;
        let storage_bits: u8 = storage_str.parse()?;
        let shift: u8 = shift_str.parse()?;
        if !matches!(storage_bits, 8 | 16 | 32 | 64)
            || real_bits == 0
            || u16::from(real_bits) + u16::from(shift) > u16::from(storage_bits)
        {
            return Err(format!("unsupported scan type '{s}'").into());
        }

        Ok(Self {
            little_endian,
            signed,
            storage_bits,
            real_bits,
            shift,
        })
    }

    fn storage_bytes(&self) -> usize {
        (self.storage_bits as usize).div_ceil(8)
    }

    /// Decode this channel's raw bytes into a signed integer.
    fn decode(&self, raw: &[u8]) -> i64 {
        let n = self.storage_bytes().min(8);
        let mut buf = [0u8; 8];
        if self.little_endian {
            buf[..n].copy_from_slice(&raw[..n]);
        } else {
            for i in 0..n {
                buf[n - 1 - i] = raw[i];
            }
        }
        let mut value = u64::from_le_bytes(buf);
        value >>= self.shift;
        let mask = if self.real_bits >= 64 {
            u64::MAX
        } else {
            (1u64 << self.real_bits) - 1
        };
        value &= mask;

        if self.signed && self.real_bits < 64 {
            let sign_bit = 1u64 << (self.real_bits - 1);
            if value & sign_bit != 0 {
                return (value as i64) - (1i64 << self.real_bits);
            }
        }
        value as i64
    }
}

/// A single enabled scan channel discovered from sysfs, before the
/// buffer's byte layout has been computed.
#[derive(Debug, Clone)]
pub struct DiscoveredChannel {
    /// e.g. "in_accel_x"
    pub id: String,
    pub axis: char,
    pub scan_index: i32,
    pub format: ScanFormat,
    pub offset: i64,
    pub scale: f64,
}

/// One channel's final position within a fixed-size buffer record, plus
/// its calibration data.
#[derive(Debug, Clone)]
pub struct ChannelLayout {
    byte_offset: usize,
    format: ScanFormat,
    offset: i64,
    scale: f64,
}

/// Fixed layout of one complete buffer record, computed once at startup.
#[derive(Debug, Clone, Default)]
pub struct RecordLayout {
    pub record_len: usize,
    pub accel: HashMap<char, ChannelLayout>,
    pub gyro: HashMap<char, ChannelLayout>,
}

pub fn read_trim(path: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

/// Reads the device's mount matrix: a shared attribute first, then per-group.
pub fn read_mount_matrix(base: &Path) -> Option<String> {
    read_trim(&base.join("in_mount_matrix"))
        .ok()
        .or_else(|| read_trim(&base.join("in_accel_mount_matrix")).ok())
        .or_else(|| read_trim(&base.join("in_anglvel_mount_matrix")).ok())
}

/// Discover the x/y/z scan channels for one group from `scan_elements/in_<group>_<axis>_*`.
pub fn discover_group(
    base: &Path,
    prefix: &str,
) -> Result<Vec<DiscoveredChannel>, Box<dyn Error + Send + Sync>> {
    let scan_dir = base.join("scan_elements");
    let mut out = Vec::new();

    for axis in ['x', 'y', 'z'] {
        let id = format!("in_{prefix}_{axis}");
        let index_path = scan_dir.join(format!("{id}_index"));
        if !index_path.exists() {
            continue;
        }

        let scan_index: i32 = read_trim(&index_path)?.trim().parse()?;
        let type_str = read_trim(&scan_dir.join(format!("{id}_type")))?;
        let format = ScanFormat::parse(&type_str)?;

        let offset = read_trim(&base.join(format!("{id}_offset")))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let scale = read_trim(&base.join(format!("{id}_scale")))
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                read_trim(&base.join(format!("in_{prefix}_scale")))
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(1.0);

        log::debug!(
            "Found channel {id} (scan_index={scan_index}, offset={offset}, scale={scale}, type={type_str})"
        );

        out.push(DiscoveredChannel {
            id,
            axis,
            scan_index,
            format,
            offset,
            scale,
        });
    }

    Ok(out)
}

/// Look up the `in_timestamp` scan channel, if the device exposes one.
pub fn discover_timestamp(
    base: &Path,
) -> Result<Option<(i32, ScanFormat)>, Box<dyn Error + Send + Sync>> {
    let scan_dir = base.join("scan_elements");
    let index_path = scan_dir.join("in_timestamp_index");
    if !index_path.exists() {
        return Ok(None);
    }
    let scan_index: i32 = read_trim(&index_path)?.trim().parse()?;
    let format = ScanFormat::parse(&read_trim(&scan_dir.join("in_timestamp_type"))?)?;
    Ok(Some((scan_index, format)))
}

/// Direct sysfs channels do not require a character device or scan_elements.
/// This is essential for AMD SFH devices configured with use_buffer: false.
#[derive(Debug, Clone)]
pub struct SysfsChannel {
    path: PathBuf,
    axis: char,
    offset: f64,
    scale: f64,
}

pub fn discover_sysfs_group(base: &Path, prefix: &str) -> Vec<SysfsChannel> {
    ['x', 'y', 'z']
        .into_iter()
        .filter_map(|axis| {
            let id = format!("in_{prefix}_{axis}");
            let path = base.join(format!("{id}_raw"));
            if !path.exists() {
                return None;
            }
            let attribute = |name: &str, default| {
                read_trim(&base.join(format!("{id}_{name}")))
                    .ok()
                    .or_else(|| read_trim(&base.join(format!("in_{prefix}_{name}"))).ok())
                    .and_then(|value| value.parse::<f64>().ok())
                    .unwrap_or(default)
            };
            Some(SysfsChannel {
                path,
                axis,
                offset: attribute("offset", 0.0),
                scale: attribute("scale", 1.0),
            })
        })
        .collect()
}

pub fn read_sysfs_group(channels: &[SysfsChannel]) -> io::Result<Option<AxisData>> {
    if channels.is_empty() {
        return Ok(None);
    }
    // Sysfs values are converted per-axis before rotation, retaining fractional
    // offsets and non-uniform scales. Translation still applies SI corrections once.
    let mut data = AxisData {
        scale: 1.0,
        ..Default::default()
    };
    for channel in channels {
        let raw: f64 = read_trim(&channel.path)?
            .parse()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let value = (raw + channel.offset) * channel.scale;
        match channel.axis {
            'x' => data.roll = value,
            'y' => data.pitch = value,
            'z' => data.yaw = value,
            _ => unreachable!(),
        }
    }
    Ok(Some(data))
}

pub fn configure_sysfs_rate(
    base: &Path,
    accel: &[SysfsChannel],
    gyro: &[SysfsChannel],
    sample_rate: Option<f64>,
) -> f64 {
    // Reuse rate negotiation without requiring scan metadata.
    let channels = |group: &[SysfsChannel]| {
        group
            .iter()
            .map(|channel| DiscoveredChannel {
                id: channel
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .trim_end_matches("_raw")
                    .to_string(),
                axis: channel.axis,
                scan_index: 0,
                format: ScanFormat {
                    little_endian: true,
                    signed: true,
                    storage_bits: 64,
                    real_bits: 64,
                    shift: 0,
                },
                offset: 0,
                scale: channel.scale,
            })
            .collect::<Vec<_>>()
    };
    let accel_rate = negotiate_sample_rate(base, &channels(accel), "accel", sample_rate);
    let gyro_rate = negotiate_sample_rate(base, &channels(gyro), "anglvel", sample_rate);
    if !accel.is_empty() {
        accel_rate
    } else {
        gyro_rate
    }
}

enum ScanEntry {
    Accel(DiscoveredChannel),
    Gyro(DiscoveredChannel),
    Timestamp(ScanFormat),
}

/// Computes each channel's byte offset within one buffer record, packing
/// them in ascending scan-index order with natural alignment and padding
/// the whole record to the widest element.
pub fn compute_layout(
    accel: Vec<DiscoveredChannel>,
    gyro: Vec<DiscoveredChannel>,
    timestamp: Option<(i32, ScanFormat)>,
) -> RecordLayout {
    let mut all: Vec<(i32, ScanEntry)> = accel
        .into_iter()
        .map(|c| (c.scan_index, ScanEntry::Accel(c)))
        .chain(gyro.into_iter().map(|c| (c.scan_index, ScanEntry::Gyro(c))))
        .collect();
    if let Some((scan_index, format)) = timestamp {
        all.push((scan_index, ScanEntry::Timestamp(format)));
    }
    all.sort_by_key(|(scan_index, _)| *scan_index);

    let mut offset = 0usize;
    let mut max_align = 1usize;
    let mut accel_layout = HashMap::new();
    let mut gyro_layout = HashMap::new();

    for (_, entry) in all {
        let format = match &entry {
            ScanEntry::Accel(c) | ScanEntry::Gyro(c) => c.format,
            ScanEntry::Timestamp(f) => *f,
        };
        let size = format.storage_bytes().max(1);
        max_align = max_align.max(size);
        if !offset.is_multiple_of(size) {
            offset += size - (offset % size);
        }

        match entry {
            ScanEntry::Accel(c) => {
                accel_layout.insert(
                    c.axis,
                    ChannelLayout {
                        byte_offset: offset,
                        format: c.format,
                        offset: c.offset,
                        scale: c.scale,
                    },
                );
            }
            ScanEntry::Gyro(c) => {
                gyro_layout.insert(
                    c.axis,
                    ChannelLayout {
                        byte_offset: offset,
                        format: c.format,
                        offset: c.offset,
                        scale: c.scale,
                    },
                );
            }
            ScanEntry::Timestamp(_) => {}
        }
        offset += size;
    }

    if max_align > 0 && !offset.is_multiple_of(max_align) {
        offset += max_align - (offset % max_align);
    }

    RecordLayout {
        record_len: offset,
        accel: accel_layout,
        gyro: gyro_layout,
    }
}

/// The scale to apply to a group's axes.
pub fn group_scale(group: &HashMap<char, ChannelLayout>, group_name: &str) -> f64 {
    let scale = ['x', 'y', 'z']
        .into_iter()
        .find_map(|axis| group.get(&axis).map(|c| c.scale))
        .unwrap_or(1.0);
    if group.values().any(|c| c.scale != scale) {
        log::warn!("IIO {group_name} channels report non-uniform scales; using {scale}");
    }
    scale
}

/// Decode one sample group from a raw IIO record using the pre-computed
/// channel layout.
pub fn decode_group(layout: &HashMap<char, ChannelLayout>, scale: f64, record: &[u8]) -> AxisData {
    let mut out = AxisData {
        scale,
        ..Default::default()
    };
    for (&axis, chan) in layout.iter() {
        let end = chan.byte_offset + chan.format.storage_bytes();
        let raw = chan.format.decode(&record[chan.byte_offset..end]);
        let value = raw as f64 + chan.offset as f64;
        match axis {
            'x' => out.roll = value,
            'y' => out.pitch = value,
            'z' => out.yaw = value,
            _ => unreachable!("discover_group only ever produces x/y/z"),
        }
    }
    out
}

fn write_bool_verified(path: &Path, value: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
    let s = if value { "1" } else { "0" };
    fs::write(path, s)?;
    let actual = read_trim(path)?;
    if actual != s {
        return Err(format!(
            "write to {} did not take effect: wrote '{s}', read back '{actual}'",
            path.display()
        )
        .into());
    }
    Ok(())
}

fn write_str_verified(path: &Path, value: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    fs::write(path, value)?;
    let actual = read_trim(path)?;
    if actual != value {
        return Err(format!(
            "write to {} did not take effect: wrote '{value}', read back '{actual}'",
            path.display()
        )
        .into());
    }
    Ok(())
}

const HRTIMER_TRIGGER_ROOT: &str = "/sys/kernel/config/iio/triggers/hrtimer";

/// How to obtain the trigger bound to a buffer during setup.
#[derive(Clone, Debug)]
pub enum TriggerStrategy {
    /// Search for an existing IIO trigger that already references this device.
    FindExisting,
    /// Always create (or reuse) and bind a dedicated hrtimer trigger.
    Hrtimer(String),
}

/// Unbinds and removes a dedicated hrtimer trigger when the owning
/// driver is dropped, so it doesn't accumulate in configfs across
/// restarts or unplug/replug cycles.
#[derive(Debug)]
pub struct HrtimerTriggerGuard {
    base: PathBuf,
    trigger_dir: PathBuf,
}

impl Drop for HrtimerTriggerGuard {
    fn drop(&mut self) {
        let buffer_dir = if self.base.join("buffer0").is_dir() {
            self.base.join("buffer0")
        } else {
            self.base.join("buffer")
        };
        let _ = fs::write(buffer_dir.join("enable"), "0");
        let _ = fs::write(self.base.join("trigger/current_trigger"), "");
        if let Err(e) = fs::remove_dir(&self.trigger_dir) {
            log::warn!(
                "Failed to remove hrtimer trigger {}: {e}",
                self.trigger_dir.display()
            );
        }
    }
}

fn bind_trigger(
    base: &Path,
    strategy: &TriggerStrategy,
    rate: f64,
    hrtimer_root: &Path,
) -> Result<Option<HrtimerTriggerGuard>, Box<dyn Error + Send + Sync>> {
    match strategy {
        TriggerStrategy::FindExisting => {
            if let Some(trigger) = find_trigger_for(base)? {
                write_str_verified(&base.join("trigger/current_trigger"), &trigger)
                    .map_err(|e| format!("failed to bind trigger '{trigger}': {e}"))?;
            } else {
                log::debug!(
                    "No matching trigger found for {}; assuming none required",
                    base.display()
                );
            }
            Ok(None)
        }
        TriggerStrategy::Hrtimer(name) => {
            if !hrtimer_root.is_dir() {
                return Err(
                    "hrtimer trigger support not available - is iio_trig_hrtimer loaded?".into(),
                );
            }
            let trigger_dir = hrtimer_root.join(name);
            if !trigger_dir.is_dir() {
                fs::create_dir(&trigger_dir).map_err(|e| {
                    format!(
                        "failed to create hrtimer trigger {}: {e}",
                        trigger_dir.display()
                    )
                })?;
            }

            // Own cleanup before any following operation can fail.
            let guard = HrtimerTriggerGuard {
                base: base.to_path_buf(),
                trigger_dir,
            };

            // The hrtimer trigger only latches sampling_frequency at
            // bind/enable time, so this must happen before it's bound.
            let devices_root = base.parent().ok_or("IIO device has no parent directory")?;
            match find_trigger_device(name, devices_root) {
                Some(trig_dev) => {
                    let path = trig_dev.join("sampling_frequency");
                    match fs::write(&path, rate.to_string()) {
                        Ok(_) => {
                            if let Ok(actual) = read_trim(&path) {
                                log::info!("Set trigger sampling_frequency to {actual} Hz");
                            }
                        }
                        Err(e) => log::warn!("Failed to set trigger sampling_frequency: {e:?}"),
                    }
                }
                None => log::warn!(
                    "Could not locate trigger device for '{name}'; its firing rate will stay at kernel default"
                ),
            }

            write_str_verified(&base.join("trigger/current_trigger"), name)
                .map_err(|e| format!("failed to bind hrtimer trigger '{name}': {e}"))?;

            Ok(Some(guard))
        }
    }
}

fn bind_trigger_with_fallback(
    base: &Path,
    strategy: &TriggerStrategy,
    rate: f64,
    hrtimer_root: &Path,
) -> Result<Option<HrtimerTriggerGuard>, Box<dyn Error + Send + Sync>> {
    match bind_trigger(base, strategy, rate, hrtimer_root) {
        Ok(guard) => Ok(guard),
        Err(error) if matches!(strategy, TriggerStrategy::Hrtimer(_)) => {
            // A missing hrtimer module/configfs must not discard a functioning
            // hardware trigger. If neither is usable, the caller uses sysfs.
            log::warn!("Dedicated hrtimer unavailable: {error}; trying matched hardware trigger");
            let Some(existing) = find_trigger_for(base)? else {
                return Err(error);
            };
            write_str_verified(&base.join("trigger/current_trigger"), &existing)?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Finds the `/sys/bus/iio/devices/triggerN` entry matching `name` exactly.
/// Doesn't gate on `trigger_now` since hrtimer triggers don't expose it.
fn find_trigger_device(name: &str, devices_root: &Path) -> Option<PathBuf> {
    fs::read_dir(devices_root)
        .ok()?
        .filter_map(|e| e.ok())
        .find_map(|entry| {
            if !entry.file_name().to_string_lossy().starts_with("trigger") {
                return None;
            }
            let path = entry.path();
            (read_trim(&path.join("name")).ok()? == name).then_some(path)
        })
}

/// Finds a trigger device for the given sensor from its parent device, or
/// None if no trigger is present on the system.
fn find_trigger_for(base: &Path) -> Result<Option<String>, Box<dyn Error + Send + Sync>> {
    let own_name = read_trim(&base.join("name")).unwrap_or_default();
    if own_name.is_empty() {
        return Ok(None);
    }

    let own_parent = fs::canonicalize(base)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let mut fallback: Option<String> = None;

    let devices_root = base.parent().ok_or("IIO device has no parent directory")?;
    for entry in fs::read_dir(devices_root)? {
        let entry = entry?;
        let path = entry.path();
        // Hardware data-ready triggers do not expose trigger_now; use the IIO
        // trigger device name rather than that software-trigger-only attribute.
        if !entry.file_name().to_string_lossy().starts_with("trigger") {
            continue;
        }
        let Ok(name) = read_trim(&path.join("name")) else {
            continue;
        };
        let is_sibling = own_parent.as_ref().and_then(|parent| {
            fs::canonicalize(&path)
                .ok()
                .and_then(|p| p.parent().map(|p| p == parent.as_path()))
        }) == Some(true);
        if is_sibling {
            return Ok(Some(name));
        }
        if fallback.is_none() && name.starts_with(&format!("{own_name}-")) {
            fallback = Some(name);
        }
    }
    Ok(fallback)
}

fn read_available_rates(base: &Path, channels: &[DiscoveredChannel], prefix: &str) -> Vec<f64> {
    for chan in channels {
        if let Ok(v) = read_trim(&base.join(format!("{}_sampling_frequency_available", chan.id))) {
            let rates: Vec<f64> = v
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if !rates.is_empty() {
                return rates;
            }
        }
    }
    read_trim(&base.join(format!("in_{prefix}_sampling_frequency_available")))
        .map(|v| {
            v.split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Write a sampling rate to the device. Tries per-channel first, then falls
/// back to the device-level attribute.
fn write_sample_rate(base: &Path, channels: &[DiscoveredChannel], prefix: &str, rate: f64) {
    for chan in channels {
        let path = base.join(format!("{}_sampling_frequency", chan.id));
        if path.exists() {
            match fs::write(&path, rate.to_string()) {
                Ok(_) => {
                    if let Ok(actual) = read_trim(&path) {
                        log::info!("Set {} sampling_frequency to {actual} Hz", chan.id);
                    }
                    return;
                }
                Err(e) => log::warn!(
                    "Per-channel sampling_frequency write failed for {}: {e:?}",
                    chan.id
                ),
            }
        }
    }

    let attr = format!("in_{prefix}_sampling_frequency");
    let path = base.join(&attr);
    match fs::write(&path, rate.to_string()) {
        Ok(_) => {
            if let Ok(actual) = read_trim(&path) {
                log::info!("Set device-level {attr} to {actual} Hz");
            }
        }
        Err(e) => log::warn!("Failed to set {attr}: {e:?}"),
    }
}

fn negotiate_sample_rate(
    base: &Path,
    channels: &[DiscoveredChannel],
    prefix: &str,
    target: Option<f64>,
) -> f64 {
    if channels.is_empty() {
        return DEFAULT_SAMPLE_RATE;
    }
    let rate = target
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .unwrap_or(DEFAULT_SAMPLE_RATE);
    let avail: Vec<f64> = read_available_rates(base, channels, prefix)
        .into_iter()
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .collect();

    let chosen = if avail.is_empty() {
        log::warn!("No available sample rates reported for {prefix}; requesting {rate} Hz anyway");
        rate
    } else if avail.contains(&rate) {
        rate
    } else {
        let max = avail.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        log::warn!(
            "Requested {rate} Hz not in available rates {avail:?} for {prefix}; using max {max} Hz"
        );
        max
    };

    write_sample_rate(base, channels, prefix, chosen);
    chosen
}

/// Clear any leftover enabled/triggered state from a previous crashed
/// instance before reconfiguring.
fn warm_reset(base: &Path, buffer_dir: &Path) {
    if fs::write(buffer_dir.join("enable"), "0").is_err() {
        thread::sleep(Duration::from_millis(100));
        let _ = fs::write(buffer_dir.join("enable"), "0");
    }
    let _ = fs::write(base.join("trigger/current_trigger"), "");
    thread::sleep(Duration::from_millis(50));
}

/// Own every buffer, including hardware-triggered ones. Dropping a driver on
/// suspend, stop, or failed setup must leave direct-mode sysfs usable again.
#[derive(Debug)]
pub struct BufferGuard {
    base: PathBuf,
    buffer_dir: PathBuf,
    _trigger_guard: Option<HrtimerTriggerGuard>,
}

impl Drop for BufferGuard {
    fn drop(&mut self) {
        disable_buffer(&self.base, &self.buffer_dir);
    }
}

fn disable_scan_elements(base: &Path) {
    if let Ok(entries) = fs::read_dir(base.join("scan_elements")) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with("_en") {
                let _ = fs::write(entry.path(), "0");
            }
        }
    }
}

fn disable_buffer(base: &Path, buffer_dir: &Path) {
    let _ = fs::write(buffer_dir.join("enable"), "0");
    let _ = fs::write(base.join("trigger/current_trigger"), "");
    disable_scan_elements(base);
}

/// Release residual buffer ownership before direct sysfs reads. Missing buffer
/// or trigger paths are expected on sysfs-only devices such as AMD SFH.
pub fn cleanup_buffer(base: &Path) {
    let buffer_dir = if base.join("buffer0").is_dir() {
        base.join("buffer0")
    } else {
        base.join("buffer")
    };
    disable_buffer(base, &buffer_dir);
}

/// Result of `configure_buffer`. The guard must outlive all buffer reads.
pub struct BufferConfig {
    pub rate: f64,
    pub guard: BufferGuard,
}

/// Run the setup sequence
pub fn configure_buffer(
    base: &Path,
    accel: &[DiscoveredChannel],
    gyro: &[DiscoveredChannel],
    sample_rate: Option<f64>,
    trigger: TriggerStrategy,
) -> Result<BufferConfig, Box<dyn Error + Send + Sync>> {
    let buffer_dir = if base.join("buffer0").is_dir() {
        base.join("buffer0")
    } else {
        base.join("buffer")
    };
    let scan_dir = base.join("scan_elements");

    warm_reset(base, &buffer_dir);
    let mut guard = BufferGuard {
        base: base.to_path_buf(),
        buffer_dir: buffer_dir.clone(),
        _trigger_guard: None,
    };

    write_bool_verified(&buffer_dir.join("enable"), false)
        .map_err(|e| format!("failed to disable buffer before reconfiguring: {e}"))?;

    let accel_rate = negotiate_sample_rate(base, accel, "accel", sample_rate);
    let gyro_rate = negotiate_sample_rate(base, gyro, "anglvel", sample_rate);
    let effective_rate = if !accel.is_empty() {
        accel_rate
    } else {
        gyro_rate
    };

    guard._trigger_guard = bind_trigger_with_fallback(
        base,
        &trigger,
        effective_rate,
        Path::new(HRTIMER_TRIGGER_ROOT),
    )?;

    // Remove stale enabled channels before computing the exact selected layout.
    for entry in fs::read_dir(&scan_dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().ends_with("_en") {
            write_bool_verified(&entry.path(), false)?;
        }
    }
    for chan in accel.iter().chain(gyro.iter()) {
        write_bool_verified(&scan_dir.join(format!("{}_en", chan.id)), true)
            .map_err(|e| format!("failed to enable scan element {}: {e}", chan.id))?;
    }

    // Timestamp channel isn't decoded, but leaving it disabled would
    // change bytes_per_datum out from under compute_layout's count.
    let timestamp_en = scan_dir.join("in_timestamp_en");
    if timestamp_en.exists() {
        write_bool_verified(&timestamp_en, true)
            .map_err(|e| format!("failed to enable in_timestamp: {e}"))?;
    }

    // Lowest-latency watermark: wake as soon as a single sample is ready,
    // and never let the kernel accumulate more than one unread record.
    let watermark_path = buffer_dir.join("watermark");
    if watermark_path.exists() {
        write_str_verified(&watermark_path, "1")
            .map_err(|e| format!("failed to set watermark to 1: {e}"))?;
    }

    // TODO: Holding this buffer open blocks direct-mode sysfs reads for the
    // life of the process, breaking other consumers like iio-dsu-bridge.
    // Add DSU target device to feed other consumers.
    write_bool_verified(&buffer_dir.join("enable"), true)
        .map_err(|e| format!("failed to arm buffer: {e}"))?;

    Ok(BufferConfig {
        rate: effective_rate,
        guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::iio_imu::test_support::TestDir;
    use std::{
        io::Write,
        os::{fd::OwnedFd, unix::net::UnixStream},
    };

    #[test]
    fn raw_fd_open_is_nonblocking() {
        let source = NonblockingFdSource::open(Path::new("/dev/null"), None).unwrap();
        let flags = nix::fcntl::fcntl(&source.file, nix::fcntl::FcntlArg::F_GETFL).unwrap();
        assert_ne!(flags & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn partial_record_survives_would_block_without_exposing_stale_data() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let fd: OwnedFd = reader.into();
        let mut source = NonblockingFdSource {
            file: fd.into(),
            kick_path: None,
            pending: vec![],
            filled: 0,
        };
        let mut record = [99; 4];
        assert_eq!(
            source.read_one(&mut record).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        writer.write_all(&[1, 2]).unwrap();
        assert_eq!(
            source.read_one(&mut record).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(record, [99; 4]);
        assert_eq!(
            source.read_one(&mut record).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        writer.write_all(&[3, 4]).unwrap();
        source.read_one(&mut record).unwrap();
        assert_eq!(record, [1, 2, 3, 4]);
        writer.write_all(&[5, 6, 7, 8]).unwrap();
        source.read_one(&mut record).unwrap();
        assert_eq!(record, [5, 6, 7, 8]);
        drop(writer);
        assert_eq!(
            source.read_one(&mut record).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn only_transient_raw_read_errors_are_retried() {
        for errno in [libc::EAGAIN, libc::EINTR] {
            assert!(is_retryable_read_error(&io::Error::from_raw_os_error(
                errno
            )));
        }
        for errno in [libc::ENODEV, libc::EIO, libc::EBADF] {
            assert!(!is_retryable_read_error(&io::Error::from_raw_os_error(
                errno
            )));
        }
        assert!(!is_retryable_read_error(
            &io::ErrorKind::UnexpectedEof.into()
        ));
    }

    #[test]
    fn scan_decoder_handles_signed_shifted_big_endian_and_mixed_width_records() {
        let channel = |axis, scan_index, ty: &str, offset| DiscoveredChannel {
            id: format!("in_accel_{axis}"),
            axis,
            scan_index,
            format: ScanFormat::parse(ty).unwrap(),
            offset,
            scale: 0.5,
        };
        let layout = compute_layout(
            vec![
                channel('x', 0, "le:s12/16>>4", 2),
                channel('y', 1, "be:u16/16>>0", 0),
            ],
            vec![channel('z', 2, "le:s32/32>>0", 0)],
            Some((3, ScanFormat::parse("le:s64/64>>0").unwrap())),
        );
        assert_eq!(layout.record_len, 16);
        let mut record = [0; 16];
        record[..2].copy_from_slice(&(-3i16 << 4).to_le_bytes());
        record[2..4].copy_from_slice(&1000u16.to_be_bytes());
        record[4..8].copy_from_slice(&(-123456i32).to_le_bytes());
        let accel = decode_group(&layout.accel, 0.5, &record);
        assert_eq!(accel.roll, -1.0); // Linux IIO calibration is raw + offset.
        assert_eq!(accel.pitch, 1000.0);
        assert_eq!(accel.scale, 0.5);
        let gyro = decode_group(&layout.gyro, 0.5, &record);
        assert_eq!(gyro.yaw, -123456.0);
    }

    #[test]
    fn malformed_or_unsupported_scan_formats_fail_without_panicking() {
        for format in [
            "",
            "le:",
            "le:s0/16>>0",
            "le:s16/0>>0",
            "le:s16/128>>0",
            "le:s16/16>>1",
            "le:s12/24>>0",
            "le:s8/8X3>>0",
        ] {
            assert!(ScanFormat::parse(format).is_err(), "accepted {format}");
        }
    }

    #[test]
    fn sysfs_only_channels_do_not_require_scan_elements_and_keep_si_scaling() {
        let dir = TestDir::new();
        dir.write("in_accel_x_raw", "10");
        dir.write("in_accel_y_raw", "20");
        dir.write("in_accel_offset", "0.5");
        dir.write("in_accel_scale", "0.25");
        dir.write("in_accel_y_scale", "0.5");
        let channels = discover_sysfs_group(dir.path(), "accel");
        let data = read_sysfs_group(&channels).unwrap().unwrap();
        assert_eq!(data.roll, 2.625);
        assert_eq!(data.pitch, 10.25);
        assert_eq!(data.scale, 1.0);
        assert!(discover_sysfs_group(dir.path(), "anglvel").is_empty());
    }

    #[test]
    fn hardware_trigger_discovery_does_not_require_trigger_now() {
        let dir = TestDir::new();
        dir.write("iio:device0/name", "bmi260");
        dir.write("trigger0/name", "bmi260-dev0");
        assert_eq!(
            find_trigger_for(&dir.path().join("iio:device0")).unwrap(),
            Some("bmi260-dev0".into())
        );
    }

    #[test]
    fn successful_buffer_drop_disables_hardware_buffer_and_stale_scan_channels() {
        let dir = TestDir::new();
        dir.write("buffer/enable", "0");
        dir.write("buffer/watermark", "4");
        dir.write("trigger/current_trigger", "old-trigger");
        dir.write("scan_elements/in_accel_x_en", "0");
        dir.write("scan_elements/in_temperature_en", "1");
        let channel = DiscoveredChannel {
            id: "in_accel_x".into(),
            axis: 'x',
            scan_index: 0,
            format: ScanFormat::parse("le:s16/16>>0").unwrap(),
            offset: 0,
            scale: 1.0,
        };
        let config = configure_buffer(
            dir.path(),
            &[channel],
            &[],
            Some(100.0),
            TriggerStrategy::FindExisting,
        )
        .unwrap();
        assert_eq!(read_trim(&dir.path().join("buffer/enable")).unwrap(), "1");
        assert_eq!(
            read_trim(&dir.path().join("scan_elements/in_temperature_en")).unwrap(),
            "0"
        );
        assert_eq!(
            read_trim(&dir.path().join("scan_elements/in_accel_x_en")).unwrap(),
            "1"
        );
        assert_eq!(config.rate, 100.0);
        drop(config);
        assert_eq!(read_trim(&dir.path().join("buffer/enable")).unwrap(), "0");
        assert_eq!(
            read_trim(&dir.path().join("scan_elements/in_accel_x_en")).unwrap(),
            "0"
        );
        assert_eq!(
            read_trim(&dir.path().join("trigger/current_trigger")).unwrap(),
            ""
        );
    }

    #[test]
    fn failed_buffer_setup_rolls_back_enabled_channels() {
        let dir = TestDir::new();
        dir.write("buffer0/enable", "0");
        dir.write("trigger/current_trigger", "old-trigger");
        dir.write("scan_elements/in_accel_x_en", "1");
        // A directory instead of an enable attribute deterministically fails even as root.
        fs::create_dir(dir.path().join("scan_elements/in_accel_y_en")).unwrap();
        let channels: Vec<_> = ['x', 'y']
            .into_iter()
            .enumerate()
            .map(|(index, axis)| DiscoveredChannel {
                id: format!("in_accel_{axis}"),
                axis,
                scan_index: index as i32,
                format: ScanFormat::parse("le:s16/16>>0").unwrap(),
                offset: 0,
                scale: 1.0,
            })
            .collect();
        assert!(configure_buffer(
            dir.path(),
            &channels,
            &[],
            None,
            TriggerStrategy::FindExisting
        )
        .is_err());
        assert_eq!(read_trim(&dir.path().join("buffer0/enable")).unwrap(), "0");
        assert_eq!(
            read_trim(&dir.path().join("scan_elements/in_accel_x_en")).unwrap(),
            "0"
        );
    }

    #[test]
    fn invalid_requested_and_advertised_rates_cannot_create_invalid_intervals() {
        let dir = TestDir::new();
        dir.write("in_accel_x_raw", "1");
        dir.write(
            "in_accel_sampling_frequency_available",
            "NaN -10 0 inf 100 200",
        );
        let channels = discover_sysfs_group(dir.path(), "accel");
        for rate in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            assert_eq!(
                configure_sysfs_rate(dir.path(), &channels, &[], Some(rate)),
                200.0
            );
        }
    }
    #[test]
    fn dedicated_hrtimer_binds_rate_and_cleans_up_without_real_configfs() {
        let dir = TestDir::new();
        dir.write("devices/iio:device0/buffer/enable", "1");
        dir.write("devices/iio:device0/trigger/current_trigger", "old");
        dir.write("devices/trigger0/name", "inputplumber-test");
        dir.write("devices/trigger0/sampling_frequency", "10");
        let configfs = dir.path().join("fake-configfs");
        fs::create_dir(&configfs).unwrap();
        let base = dir.path().join("devices/iio:device0");
        let guard = bind_trigger_with_fallback(
            &base,
            &TriggerStrategy::Hrtimer("inputplumber-test".into()),
            100.0,
            &configfs,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            read_trim(&base.join("trigger/current_trigger")).unwrap(),
            "inputplumber-test"
        );
        assert_eq!(
            read_trim(&dir.path().join("devices/trigger0/sampling_frequency")).unwrap(),
            "100"
        );
        assert!(configfs.join("inputplumber-test").is_dir());
        drop(guard);
        assert!(!configfs.join("inputplumber-test").exists());
        assert_eq!(read_trim(&base.join("buffer/enable")).unwrap(), "0");
        assert_eq!(
            read_trim(&base.join("trigger/current_trigger")).unwrap(),
            ""
        );
    }

    #[test]
    fn unavailable_hrtimer_uses_matching_hardware_or_returns_error() {
        let dir = TestDir::new();
        dir.write("devices/iio:device0/name", "bmi260");
        dir.write("devices/iio:device0/trigger/current_trigger", "");
        dir.write("devices/trigger0/name", "bmi260-dev0");
        let base = dir.path().join("devices/iio:device0");
        let unavailable = dir.path().join("no-configfs");
        let strategy = TriggerStrategy::Hrtimer("inputplumber-test".into());
        assert!(
            bind_trigger_with_fallback(&base, &strategy, 100.0, &unavailable)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            read_trim(&base.join("trigger/current_trigger")).unwrap(),
            "bmi260-dev0"
        );
        fs::remove_dir_all(dir.path().join("devices/trigger0")).unwrap();
        assert!(bind_trigger_with_fallback(&base, &strategy, 100.0, &unavailable).is_err());
    }
}
