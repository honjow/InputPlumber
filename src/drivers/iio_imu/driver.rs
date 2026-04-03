use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fs::File,
    io::{self, BufRead, BufReader},
    os::fd::RawFd,
};

use industrial_io::{Channel, ChannelType, Context, Device, Direction};

use crate::{
    drivers::iio_imu::info::MountMatrix,
    input::capability::{Capability, Source},
};

use super::{
    event::{AxisData, Event},
    info::AxisInfo,
    trigger,
};

const DEFAULT_SAMPLE_RATE: f64 = 200.0;
const DEFAULT_BUFFER_SAMPLES: usize = 1;

enum ReadMode {
    Sysfs,
    Buffer {
        buffer: industrial_io::Buffer,
        poll_fd: RawFd,
        storage_bits: u32,
    },
}

/// Driver for reading IIO IMU data
pub struct Driver {
    _device: Device, // must outlive Channel raw pointers
    mount_matrix: MountMatrix,
    accel: HashMap<String, Channel>,
    accel_info: HashMap<String, AxisInfo>,
    gyro: HashMap<String, Channel>,
    gyro_info: HashMap<String, AxisInfo>,
    /// List of events that should not be generated
    filtered_events: HashSet<Capability>,
    read_mode: ReadMode,
}

impl Driver {
    pub fn new(
        id: String,
        name: String,
        matrix: Option<MountMatrix>,
        use_buffer: Option<bool>,
        sample_rate: Option<f64>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        log::debug!("Creating IIO IMU driver instance for {name}");

        // Create an IIO local context used to query for devices
        let ctx = Context::new()?;
        log::debug!("IIO context version: {}", ctx.version());

        // Find the IMU device
        let Some(device) = ctx.find_device(id.as_str()) else {
            return Err("Failed to find device".into());
        };

        // Try finding the mount matrix to determine how sensors were mounted inside
        // the device.
        // https://github.com/torvalds/linux/blob/master/Documentation/devicetree/bindings/iio/mount-matrix.txt
        let mount_matrix = if let Some(matrix) = matrix {
            // Use the provided mount matrix if it is defined
            matrix
        } else if let Some(mount) = device.find_channel("mount", Direction::Input) {
            // Read from the matrix
            let matrix_str = mount.attr_read_str("matrix")?;
            log::debug!("Found mount matrix: {matrix_str}");
            let matrix = MountMatrix::new(matrix_str)?;
            log::debug!("Decoded mount matrix: {matrix}");
            matrix
        } else {
            MountMatrix::default()
        };

        // Find all accelerometer and gyro channels and insert them into a hashmap
        let (accel, accel_info) = get_channels_with_type(&device, ChannelType::Accel);
        for attr in &accel_info {
            log::debug!("Found accel_info: {:?}", attr);
        }
        let (gyro, gyro_info) = get_channels_with_type(&device, ChannelType::AnglVel);
        for attr in &gyro_info {
            log::debug!("Found gyro_info: {:?}", attr);
        }

        // Log device attributes
        for attr in device.attributes() {
            log::trace!("Found device attribute: {:?}", attr)
        }

        // Log all found channels
        for channel in device.channels() {
            log::trace!("Found channel: {:?} {:?}", channel.id(), channel.name());
            log::trace!("  Is output: {}", channel.is_output());
            log::trace!("  Is scan element: {}", channel.is_scan_element());
            for attr in channel.attrs() {
                log::trace!("  Found attribute: {:?}", attr);
            }
        }

        if !accel.is_empty() {
            set_sample_rate(&device, &accel, ChannelType::Accel, sample_rate);
        }
        if !gyro.is_empty() {
            set_sample_rate(&device, &gyro, ChannelType::AnglVel, sample_rate);
        }

        let should_try_buffer = use_buffer.unwrap_or(true);
        let rate = sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE);

        let read_mode = if should_try_buffer {
            match try_buffer_mode(&ctx, &device, &name, &accel, &gyro, rate) {
                Ok(mode) => {
                    log::info!("IIO buffer mode enabled for {name}");
                    mode
                }
                Err(e) => {
                    log::warn!(
                        "Buffer mode unavailable for {name}, using sysfs fallback: {e}"
                    );
                    ReadMode::Sysfs
                }
            }
        } else {
            log::info!("Buffer mode disabled by config for {name}, using sysfs");
            ReadMode::Sysfs
        };

        Ok(Self {
            _device: device,
            mount_matrix,
            accel,
            accel_info,
            gyro,
            gyro_info,
            filtered_events: Default::default(),
            read_mode,
        })
    }

    //TODO: Using InputPlumber Capability enum prevents this driver from having the ability to be
    //a standalone crate. When this driver is eventually separated, refactor the Event type to
    //follow the pattern DeviceEvent(Event, Value) and create a match table for
    //Capability->Event/Event->Capability in the SourceDriver implementation.
    pub fn has_accel(&self) -> bool {
        !self.accel.is_empty()
    }

    pub fn has_gyro(&self) -> bool {
        !self.gyro.is_empty()
    }

    pub fn update_filtered_events(&mut self, events: HashSet<Capability>) {
        self.filtered_events = events;
    }

    pub fn poll_fd(&self) -> Option<RawFd> {
        match &self.read_mode {
            ReadMode::Buffer { poll_fd, .. } => Some(*poll_fd),
            ReadMode::Sysfs => None,
        }
    }

    pub fn get_default_event_filter(
        &self,
    ) -> Result<HashSet<Capability>, Box<dyn Error + Send + Sync>> {
        let filtered_events = match is_driver_loaded("hid_lenovo_go") {
            Ok(true) => {
                log::debug!("Found hid-lenovo-go driver. Disabling internal gyroscope.");
                HashSet::from([
                    Capability::Accelerometer(Source::Center),
                    Capability::Gyroscope(Source::Center),
                ])
            }
            Ok(false) => {
                log::debug!("Did not find hid-lenovo-go driver. Enabling internal gyroscope.");
                HashSet::new()
            }
            Err(e) => {
                return Err(format!("Failed to read '/proc/modules': {e:?}").into());
            }
        };
        Ok(filtered_events)
    }

    /// Poll the device for data
    pub fn poll(&mut self) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        if matches!(self.read_mode, ReadMode::Buffer { .. }) {
            self.poll_buffer()
        } else {
            self.poll_sysfs()
        }
    }

    fn poll_sysfs(&self) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        let mut events = vec![];

        // Read from the accelerometer
        if !self
            .filtered_events
            .contains(&Capability::Accelerometer(Source::Center))
        {
            if let Some(event) = self.poll_accel_sysfs()? {
                events.push(event);
            }
        }

        // Read from the gyro
        if !self
            .filtered_events
            .contains(&Capability::Gyroscope(Source::Center))
        {
            if let Some(event) = self.poll_gyro_sysfs()? {
                events.push(event);
            }
        }

        Ok(events)
    }

    fn poll_buffer(&mut self) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        let ReadMode::Buffer {
            ref mut buffer,
            storage_bits,
            ..
        } = self.read_mode
        else {
            unreachable!();
        };

        if let Err(e) = buffer.refill() {
            log::trace!("Buffer refill: {e}");
            return Ok(vec![]);
        }

        let mut events = vec![];

        if !self
            .filtered_events
            .contains(&Capability::Accelerometer(Source::Center))
            && !self.accel.is_empty()
        {
            let mut data = AxisData::default();
            for (id, channel) in &self.accel {
                if let Some(info) = self.accel_info.get(id) {
                    if let Some(raw) = read_buffer_sample(buffer, channel, storage_bits) {
                        let value = (raw + info.offset) as f64 * info.scale;
                        match id.chars().last() {
                            Some('x') => data.roll = value,
                            Some('y') => data.pitch = value,
                            Some('z') => data.yaw = value,
                            _ => {}
                        }
                    }
                }
            }
            rotate_value(&self.mount_matrix, &mut data);
            events.push(Event::Accelerometer(data));
        }

        if !self
            .filtered_events
            .contains(&Capability::Gyroscope(Source::Center))
            && !self.gyro.is_empty()
        {
            let mut data = AxisData::default();
            for (id, channel) in &self.gyro {
                if let Some(info) = self.gyro_info.get(id) {
                    if let Some(raw) = read_buffer_sample(buffer, channel, storage_bits) {
                        let value = (raw + info.offset) as f64 * info.scale;
                        match id.chars().last() {
                            Some('x') => data.roll = value,
                            Some('y') => data.pitch = value,
                            Some('z') => data.yaw = value,
                            _ => {}
                        }
                    }
                }
            }
            rotate_value(&self.mount_matrix, &mut data);
            events.push(Event::Gyro(data));
        }

        Ok(events)
    }

    /// Polls all the channels from the accelerometer
    fn poll_accel_sysfs(&self) -> Result<Option<Event>, Box<dyn Error + Send + Sync>> {
        if self.accel.is_empty() {
            return Ok(None);
        }

        // Read from each accel channel
        let mut accel_input = AxisData::default();
        for (id, channel) in self.accel.iter() {
            // Get the info for the axis and read the data
            let Some(info) = self.accel_info.get(id) else {
                continue;
            };
            let data = channel.attr_read_int("raw")?;

            // processed_value = (raw + offset) * scale
            let value = (data + info.offset) as f64 * info.scale;
            if id.ends_with('x') {
                accel_input.roll = value;
            }
            if id.ends_with('y') {
                accel_input.pitch = value;
            }
            if id.ends_with('z') {
                accel_input.yaw = value;
            }
        }
        self.rotate_value(&mut accel_input);

        Ok(Some(Event::Accelerometer(accel_input)))
    }

    /// Polls all the channels from the gyro
    fn poll_gyro_sysfs(&self) -> Result<Option<Event>, Box<dyn Error + Send + Sync>> {
        if self.gyro.is_empty() {
            return Ok(None);
        }

        let mut gyro_input = AxisData::default();
        for (id, channel) in self.gyro.iter() {
            // Get the info for the axis and read the data
            let Some(info) = self.gyro_info.get(id) else {
                continue;
            };
            let data = channel.attr_read_int("raw")?;

            // processed_value = (raw + offset) * scale
            let value = (data + info.offset) as f64 * info.scale;

            if id.ends_with('x') {
                gyro_input.roll = value;
            }
            if id.ends_with('y') {
                gyro_input.pitch = value;
            }
            if id.ends_with('z') {
                gyro_input.yaw = value;
            }
        }
        self.rotate_value(&mut gyro_input);

        Ok(Some(Event::Gyro(gyro_input)))
    }

    /// Rotate the given axis data according to the mount matrix. This is used
    /// to calculate the final value according to the sensor oritentation.
    // Values are intended to be multiplied as:
    //   x' = mxx * x + myx * y + mzx * z
    //   y' = mxy * x + myy * y + mzy * z
    //   z' = mxz * x + myz * y + mzz * z
    fn rotate_value(&self, value: &mut AxisData) {
        rotate_value(&self.mount_matrix, value);
    }
}

fn rotate_value(m: &MountMatrix, value: &mut AxisData) {
    let x = value.roll;
    let y = value.pitch;
    let z = value.yaw;
    value.roll = m.x.0 * x + m.x.1 * y + m.x.2 * z;
    value.pitch = m.y.0 * x + m.y.1 * y + m.y.2 * z;
    value.yaw = m.z.0 * x + m.z.1 * y + m.z.2 * z;
}

fn try_buffer_mode(
    ctx: &Context,
    device: &Device,
    name: &str,
    accel: &HashMap<String, Channel>,
    gyro: &HashMap<String, Channel>,
    sample_rate: f64,
) -> Result<ReadMode, Box<dyn Error + Send + Sync>> {
    let device_id = device.id().unwrap_or_default();

    cleanup_iio_buffer_state(&device_id);

    let trigger = trigger::find_trigger(ctx, name, sample_rate)
        .ok_or("No suitable IIO trigger found")?;

    let trigger_name = trigger.name().unwrap_or_default();
    log::debug!("Binding trigger {trigger_name} to {name}");
    device.set_trigger(&trigger)?;

    let mut enabled = 0u32;
    for channel in accel.values().chain(gyro.values()) {
        if channel.is_scan_element() {
            channel.enable();
            enabled += 1;
        }
    }
    if enabled == 0 {
        return Err("No scan elements available for buffer mode".into());
    }

    let storage_bits = detect_storage_bits(&device_id);
    log::info!("Detected scan element storage bits: {storage_bits}");

    let buffer = device.create_buffer(DEFAULT_BUFFER_SAMPLES, false)?;
    buffer.set_blocking_mode(false)?;

    let fd = buffer.poll_fd()? as RawFd;
    log::info!("IIO buffer created: {enabled} channels, poll_fd={fd}");

    Ok(ReadMode::Buffer {
        buffer,
        poll_fd: fd,
        storage_bits,
    })
}

fn read_buffer_sample(
    buffer: &industrial_io::Buffer,
    channel: &Channel,
    storage_bits: u32,
) -> Option<i64> {
    match storage_bits {
        16 => buffer.channel_iter::<i16>(channel).last().map(|&v| v as i64),
        32 => buffer.channel_iter::<i32>(channel).last().map(|&v| v as i64),
        64 => buffer.channel_iter::<i64>(channel).last().copied(),
        _ => {
            log::warn!("Unsupported scan element storage bits: {storage_bits}");
            None
        }
    }
}

// Parse storage bits from sysfs type string, e.g. "le:s16/16>>0" → 16
fn detect_storage_bits(device_id: &str) -> u32 {
    let base = format!("/sys/bus/iio/devices/{device_id}/scan_elements");
    for name in ["in_accel_x_type", "in_anglvel_x_type", "in_accel_y_type", "in_anglvel_y_type"] {
        let path = format!("{base}/{name}");
        if let Ok(type_str) = std::fs::read_to_string(&path) {
            if let Some(slash) = type_str.find('/') {
                if let Some(shift) = type_str.find(">>") {
                    if let Ok(bits) = type_str[slash + 1..shift].parse::<u32>() {
                        return bits;
                    }
                }
            }
        }
    }
    log::warn!("Could not detect storage bits from sysfs, defaulting to 32");
    32
}

fn cleanup_iio_buffer_state(device_id: &str) {
    let base = format!("/sys/bus/iio/devices/{device_id}");

    let buf_enable = format!("{base}/buffer/enable");
    if let Ok(val) = std::fs::read_to_string(&buf_enable) {
        if val.trim() == "1" {
            log::info!("Cleaning up residual IIO buffer state for {device_id}");
            if let Err(e) = std::fs::write(&buf_enable, "0") {
                log::warn!("Failed to disable residual buffer: {e}");
                return;
            }
        }
    }

    let trigger_path = format!("{base}/trigger/current_trigger");
    if let Ok(val) = std::fs::read_to_string(&trigger_path) {
        if !val.trim().is_empty() {
            if let Err(e) = std::fs::write(&trigger_path, "") {
                log::debug!("Failed to unbind residual trigger: {e}");
            }
        }
    }

    let scan_dir = format!("{base}/scan_elements");
    if let Ok(entries) = std::fs::read_dir(&scan_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with("_en") {
                if let Ok(val) = std::fs::read_to_string(entry.path()) {
                    if val.trim() == "1" {
                        if let Err(e) = std::fs::write(entry.path(), "0") {
                            log::debug!("Failed to disable scan element {name}: {e}");
                        }
                    }
                }
            }
        }
    }
}

/// Returns all channels and channel information from the given device matching
/// the given channel type.
fn get_channels_with_type(
    device: &Device,
    channel_type: ChannelType,
) -> (HashMap<String, Channel>, HashMap<String, AxisInfo>) {
    let mut channels = HashMap::new();
    let mut channel_info = HashMap::new();
    device
        .channels()
        .filter(|channel| channel.channel_type() == channel_type)
        .for_each(|channel| {
            let Some(id) = channel.id() else {
                log::warn!("Unable to get channel id for channel: {:?}", channel);
                return;
            };
            log::debug!("Found channel: {id}");

            // Get the offset of the axis
            let offset = match channel.attr_read_int("offset") {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("Unable to read offset for channel {id}: {:?}", e);
                    0
                }
            };

            // Get the sample rate of the axis
            let sample_rate = match channel.attr_read_float("sampling_frequency") {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("Unable to read sample rate for channel {id}: {:?}", e);
                    4.0
                }
            };

            let sample_rates_avail = match channel.attr_read_str("sampling_frequency_available") {
                Ok(v) => {
                    let mut all_scales = Vec::new();
                    for val in v.split_whitespace() {
                        // convert the string into f64
                        all_scales.push(val.parse::<f64>().unwrap());
                    }
                    all_scales
                }
                Err(e) => {
                    log::warn!(
                        "Unable to read available sample rates for channel {id}: {:?}",
                        e
                    );
                    vec![4.0]
                }
            };

            // Get the scale of the axis to normalize values to meters per second or rads per
            // second
            let scale = match channel.attr_read_float("scale") {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("Unable to read scale for channel {id}: {:?}", e);
                    1.0
                }
            };

            let scales_avail = match channel.attr_read_str("scale_available") {
                Ok(v) => {
                    let mut all_scales = Vec::new();
                    for val in v.split_whitespace() {
                        // convert the string into f64
                        all_scales.push(val.parse::<f64>().unwrap());
                    }
                    all_scales
                }
                Err(e) => {
                    log::warn!("Unable to read available scales for channel {id}: {:?}", e);
                    vec![1.0]
                }
            };

            let info = AxisInfo {
                offset,
                sample_rate,
                sample_rates_avail,
                scale,
                scales_avail,
            };
            channel_info.insert(id.clone(), info);
            channels.insert(id, channel);
        });

    (channels, channel_info)
}

fn is_driver_loaded(driver_name: &str) -> io::Result<bool> {
    let file = File::open("/proc/modules")?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        if line.starts_with(driver_name) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn set_sample_rate(
    device: &Device,
    channels: &HashMap<String, Channel>,
    channel_type: ChannelType,
    target_rate: Option<f64>,
) {
    let rate = if let Some(r) = target_rate {
        log::debug!("Using configured sample rate: {r} Hz");
        r
    } else {
        let avail = read_sample_rates_available(device, channels, &channel_type);
        if !avail.is_empty() {
            let max = avail.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            log::debug!("Using max available sample rate: {max} Hz");
            max
        } else {
            log::debug!("No available rates found, using default: {DEFAULT_SAMPLE_RATE} Hz");
            DEFAULT_SAMPLE_RATE
        }
    };

    // per-channel attribute
    for (id, channel) in channels.iter() {
        match channel.attr_write_float("sampling_frequency", rate) {
            Ok(_) => {
                let actual = channel
                    .attr_read_float("sampling_frequency")
                    .unwrap_or(rate);
                log::debug!("Set sampling_frequency to {actual} Hz via channel {id}");
                return;
            }
            Err(e) => {
                log::debug!(
                    "Per-channel sampling_frequency write failed for {id}: {e:?}, trying device-level"
                );
            }
        }
    }

    // device-level fallback
    let attr = match channel_type {
        ChannelType::Accel => "in_accel_sampling_frequency",
        ChannelType::AnglVel => "in_anglvel_sampling_frequency",
        _ => return,
    };

    match device.attr_write_float(attr, rate) {
        Ok(_) => {
            let actual = device.attr_read_float(attr).unwrap_or(rate);
            log::info!("Set device-level {attr} to {actual} Hz");
        }
        Err(e) => {
            log::warn!("Failed to set {attr}: {e:?}");
        }
    }
}

fn read_sample_rates_available(
    device: &Device,
    channels: &HashMap<String, Channel>,
    channel_type: &ChannelType,
) -> Vec<f64> {
    for (_, channel) in channels.iter() {
        if let Ok(v) = channel.attr_read_str("sampling_frequency_available") {
            let rates: Vec<f64> = v
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if !rates.is_empty() {
                return rates;
            }
        }
    }

    let attr = match channel_type {
        ChannelType::Accel => "in_accel_sampling_frequency_available",
        ChannelType::AnglVel => "in_anglvel_sampling_frequency_available",
        _ => return vec![],
    };

    if let Ok(v) = device.attr_read_str(attr) {
        let rates: Vec<f64> = v
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if !rates.is_empty() {
            return rates;
        }
    }

    vec![]
}
