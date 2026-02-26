use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fs::{self, File},
    io::{self, BufRead, BufReader},
    path::Path,
    time::Instant,
};

use industrial_io::{buffer::Buffer, Channel, ChannelType, Device, Direction};

use crate::{
    drivers::iio_imu::info::MountMatrix,
    input::capability::{Capability, Source},
};

use super::{
    event::{AxisData, Event},
    info::AxisInfo,
};

/// Noise threshold for value change detection (in physical units after scale).
/// Values below this threshold are considered noise and are suppressed.
const GYRO_NOISE_THRESHOLD: f64 = 0.01;
const ACCEL_NOISE_THRESHOLD: f64 = 0.01;

/// Default sampling rate in Hz when using buffer mode
const DEFAULT_SAMPLE_RATE_HZ: f64 = 400.0;

/// Name prefix for hrtimer triggers created by InputPlumber
const HRTIMER_TRIGGER_NAME: &str = "inputplumber";

/// Driver for reading IIO IMU data.
/// Supports two modes:
///   - **Buffer mode**: uses IIO kernel buffers + trigger for atomic, efficient reads
///   - **Sysfs mode** (fallback): reads channel attributes directly
pub struct Driver {
    mount_matrix: MountMatrix,
    accel: HashMap<String, Channel>,
    accel_info: HashMap<String, AxisInfo>,
    gyro: HashMap<String, Channel>,
    gyro_info: HashMap<String, AxisInfo>,
    filtered_events: HashSet<Capability>,
    prev_accel: AxisData,
    prev_gyro: AxisData,
    start_time: Instant,
    /// IIO buffer for atomic data reading (None = sysfs fallback mode)
    buffer: Option<Buffer>,
    /// Hardware timestamp channel if available
    timestamp_channel: Option<Channel>,
    /// Whether this driver created the hrtimer trigger (and should clean it up)
    owns_hrtimer_trigger: bool,
}

impl Driver {
    pub fn new(
        id: String,
        name: String,
        matrix: Option<MountMatrix>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        log::debug!("Creating IIO IMU driver instance for {name}");

        let ctx = industrial_io::context::Context::new()?;
        log::debug!("IIO context version: {}", ctx.version());

        let Some(device) = ctx.find_device(id.as_str()) else {
            return Err("Failed to find device".into());
        };

        let mount_matrix = if let Some(matrix) = matrix {
            matrix
        } else if let Some(mount) = device.find_channel("mount", Direction::Input) {
            let matrix_str = mount.attr_read_str("matrix")?;
            log::debug!("Found mount matrix: {matrix_str}");
            let matrix = MountMatrix::new(matrix_str)?;
            log::debug!("Decoded mount matrix: {matrix}");
            matrix
        } else {
            MountMatrix::default()
        };

        let (accel, accel_info) = get_channels_with_type(&device, ChannelType::Accel);
        for attr in &accel_info {
            log::debug!("Found accel_info: {:?}", attr);
        }
        let (gyro, gyro_info) = get_channels_with_type(&device, ChannelType::AnglVel);
        for attr in &gyro_info {
            log::debug!("Found gyro_info: {:?}", attr);
        }

        for attr in device.attributes() {
            log::trace!("Found device attribute: {:?}", attr)
        }
        for channel in device.channels() {
            log::trace!("Found channel: {:?} {:?}", channel.id(), channel.name());
            log::trace!("  Is output: {}", channel.is_output());
            log::trace!("  Is scan element: {}", channel.is_scan_element());
            for attr in channel.attrs() {
                log::trace!("  Found attribute: {:?}", attr);
            }
        }

        // Try to set up buffer mode with trigger
        let (buffer, timestamp_channel, owns_hrtimer) =
            try_setup_buffer_mode(&device, &accel, &gyro, &ctx);

        Ok(Self {
            mount_matrix,
            accel,
            accel_info,
            gyro,
            gyro_info,
            filtered_events: Default::default(),
            prev_accel: AxisData::default(),
            prev_gyro: AxisData::default(),
            start_time: Instant::now(),
            buffer,
            timestamp_channel,
            owns_hrtimer_trigger: owns_hrtimer,
        })
    }

    pub fn update_filtered_events(&mut self, events: HashSet<Capability>) {
        self.filtered_events = events;
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

    /// Returns true if the driver is in buffer mode
    pub fn is_buffer_mode(&self) -> bool {
        self.buffer.is_some()
    }

    /// Poll the device for data
    pub fn poll(&mut self) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        if self.buffer.is_some() {
            self.poll_buffer()
        } else {
            self.poll_sysfs()
        }
    }

    /// Poll using IIO buffer mode — blocks until trigger fires
    fn poll_buffer(&mut self) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        let read_accel = !self
            .filtered_events
            .contains(&Capability::Accelerometer(Source::Center));
        let read_gyro = !self
            .filtered_events
            .contains(&Capability::Gyroscope(Source::Center));

        // All buffer reads happen in this block so the mutable borrow is released
        // before we call self.rotate_value().
        let (ts, accel_input, gyro_input) = {
            let buffer = self.buffer.as_mut().unwrap();
            buffer.refill()?;

            let ts = if let Some(ref ts_chan) = self.timestamp_channel {
                match ts_chan.read::<i64>(buffer) {
                    Ok(vals) if !vals.is_empty() => (vals[0] / 1000) as u64,
                    _ => self.start_time.elapsed().as_micros() as u64,
                }
            } else {
                self.start_time.elapsed().as_micros() as u64
            };

            let accel_input = if read_accel {
                let mut data = AxisData::default();
                for (id, channel) in self.accel.iter() {
                    let Some(info) = self.accel_info.get(id) else {
                        continue;
                    };
                    let raw_vals = channel.read::<i32>(buffer)?;
                    if raw_vals.is_empty() {
                        continue;
                    }
                    let value = (raw_vals[0] as i64 + info.offset) as f64 * info.scale;
                    if id.ends_with('x') {
                        data.roll = value;
                    }
                    if id.ends_with('y') {
                        data.pitch = value;
                    }
                    if id.ends_with('z') {
                        data.yaw = value;
                    }
                }
                Some(data)
            } else {
                None
            };

            let gyro_input = if read_gyro {
                let mut data = AxisData::default();
                for (id, channel) in self.gyro.iter() {
                    let Some(info) = self.gyro_info.get(id) else {
                        continue;
                    };
                    let raw_vals = channel.read::<i32>(buffer)?;
                    if raw_vals.is_empty() {
                        continue;
                    }
                    let value = (raw_vals[0] as i64 + info.offset) as f64 * info.scale;
                    if id.ends_with('x') {
                        data.roll = value;
                    }
                    if id.ends_with('y') {
                        data.pitch = value;
                    }
                    if id.ends_with('z') {
                        data.yaw = value;
                    }
                }
                Some(data)
            } else {
                None
            };

            (ts, accel_input, gyro_input)
        };

        let mut events = vec![];

        if let Some(mut accel) = accel_input {
            self.rotate_value(&mut accel);
            if axis_changed(&accel, &self.prev_accel, ACCEL_NOISE_THRESHOLD) {
                self.prev_accel = accel.clone();
                events.push(Event::Accelerometer(accel, ts));
            }
        }

        if let Some(mut gyro) = gyro_input {
            self.rotate_value(&mut gyro);
            if axis_changed(&gyro, &self.prev_gyro, GYRO_NOISE_THRESHOLD) {
                self.prev_gyro = gyro.clone();
                events.push(Event::Gyro(gyro, ts));
            }
        }

        Ok(events)
    }

    /// Poll using sysfs attribute reads (fallback mode)
    fn poll_sysfs(&mut self) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        let mut events = vec![];

        if !self
            .filtered_events
            .contains(&Capability::Accelerometer(Source::Center))
        {
            if let Some(event) = self.poll_accel_sysfs()? {
                events.push(event);
            }
        }

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

    fn poll_accel_sysfs(&mut self) -> Result<Option<Event>, Box<dyn Error + Send + Sync>> {
        let mut accel_input = AxisData::default();
        for (id, channel) in self.accel.iter() {
            let Some(info) = self.accel_info.get(id) else {
                continue;
            };
            let data = channel.attr_read_int("raw")?;
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

        if !axis_changed(&accel_input, &self.prev_accel, ACCEL_NOISE_THRESHOLD) {
            return Ok(None);
        }
        self.prev_accel = accel_input.clone();
        let ts = self.start_time.elapsed().as_micros() as u64;
        Ok(Some(Event::Accelerometer(accel_input, ts)))
    }

    fn poll_gyro_sysfs(&mut self) -> Result<Option<Event>, Box<dyn Error + Send + Sync>> {
        let mut gyro_input = AxisData::default();
        for (id, channel) in self.gyro.iter() {
            let Some(info) = self.gyro_info.get(id) else {
                continue;
            };
            let data = channel.attr_read_int("raw")?;
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

        if !axis_changed(&gyro_input, &self.prev_gyro, GYRO_NOISE_THRESHOLD) {
            return Ok(None);
        }
        self.prev_gyro = gyro_input.clone();
        let ts = self.start_time.elapsed().as_micros() as u64;
        Ok(Some(Event::Gyro(gyro_input, ts)))
    }

    /// Rotate the given axis data according to the mount matrix.
    // Values are intended to be multiplied as:
    //   x' = mxx * x + myx * y + mzx * z
    //   y' = mxy * x + myy * y + mzy * z
    //   z' = mxz * x + myz * y + mzz * z
    fn rotate_value(&self, value: &mut AxisData) {
        let x = value.roll;
        let y = value.pitch;
        let z = value.yaw;
        let mxx = self.mount_matrix.x.0;
        let myx = self.mount_matrix.x.1;
        let mzx = self.mount_matrix.x.2;
        let mxy = self.mount_matrix.y.0;
        let myy = self.mount_matrix.y.1;
        let mzy = self.mount_matrix.y.2;
        let mxz = self.mount_matrix.z.0;
        let myz = self.mount_matrix.z.1;
        let mzz = self.mount_matrix.z.2;
        value.roll = mxx * x + myx * y + mzx * z;
        value.pitch = mxy * x + myy * y + mzy * z;
        value.yaw = mxz * x + myz * y + mzz * z;
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // Clean up hrtimer trigger if we created it
        if self.owns_hrtimer_trigger {
            let trigger_path = format!(
                "/sys/kernel/config/iio/triggers/hrtimer/{}",
                HRTIMER_TRIGGER_NAME
            );
            if Path::new(&trigger_path).exists() {
                log::debug!("Removing hrtimer trigger: {trigger_path}");
                if let Err(e) = fs::remove_dir(&trigger_path) {
                    log::warn!("Failed to remove hrtimer trigger: {e:?}");
                }
            }
        }
    }
}

/// Try to set up IIO buffer mode with a suitable trigger.
/// Returns (buffer, timestamp_channel, owns_hrtimer_trigger).
/// If setup fails at any step, returns (None, None, false) for sysfs fallback.
fn try_setup_buffer_mode(
    device: &Device,
    accel: &HashMap<String, Channel>,
    gyro: &HashMap<String, Channel>,
    ctx: &industrial_io::context::Context,
) -> (Option<Buffer>, Option<Channel>, bool) {
    // Check if device supports buffered I/O
    if !device.is_buffer_capable() {
        log::info!("IIO device is not buffer-capable, using sysfs mode");
        return (None, None, false);
    }

    // Enable scan element channels for accel and gyro
    for channel in accel.values() {
        if channel.is_scan_element() {
            channel.enable();
        }
    }
    for channel in gyro.values() {
        if channel.is_scan_element() {
            channel.enable();
        }
    }

    // Try to find and enable the timestamp channel
    let timestamp_channel = device
        .find_channel("timestamp", Direction::Input)
        .filter(|ch| ch.is_scan_element());
    if let Some(ref ts_ch) = timestamp_channel {
        ts_ch.enable();
        log::debug!("Enabled hardware timestamp channel");
    }

    // Try to set up a trigger
    let owns_hrtimer = try_setup_trigger(device, ctx);

    // Create the buffer (sample_count=1 for single-sample reads)
    match device.create_buffer(1, false) {
        Ok(buffer) => {
            if let Err(e) = buffer.set_blocking_mode(true) {
                log::warn!("Failed to set blocking mode on buffer: {e:?}");
            }
            log::info!("IIO buffer mode enabled successfully");
            (Some(buffer), timestamp_channel, owns_hrtimer)
        }
        Err(e) => {
            log::warn!("Failed to create IIO buffer: {e:?}, falling back to sysfs mode");
            // Disable channels since we're not using buffer mode
            for channel in accel.values() {
                channel.disable();
            }
            for channel in gyro.values() {
                channel.disable();
            }
            if let Some(ref ts_ch) = timestamp_channel {
                ts_ch.disable();
            }
            (None, None, false)
        }
    }
}

/// Try to set up a trigger for the device. Returns true if we created an hrtimer trigger.
fn try_setup_trigger(device: &Device, ctx: &industrial_io::context::Context) -> bool {
    // Priority 1: Find device's own trigger
    for dev in ctx.devices() {
        if dev.is_trigger() {
            if let Some(trigger_name) = dev.name() {
                if let Some(device_name) = device.name() {
                    // Match trigger to device (e.g., bmi323-dev0 trigger for bmi323)
                    if trigger_name.starts_with(&device_name) {
                        log::debug!("Found matching device trigger: {trigger_name}");
                        if let Err(e) = device.set_trigger(&dev) {
                            log::warn!("Failed to set device trigger: {e:?}");
                        } else {
                            log::info!("Using device trigger: {trigger_name}");
                            // Try setting the sampling frequency
                            try_set_sampling_frequency(device, DEFAULT_SAMPLE_RATE_HZ);
                            return false;
                        }
                    }
                }
            }
        }
    }

    // Priority 2: Create an hrtimer trigger
    let trigger_path = format!(
        "/sys/kernel/config/iio/triggers/hrtimer/{}",
        HRTIMER_TRIGGER_NAME
    );
    if !Path::new(&trigger_path).exists() {
        if let Err(e) = fs::create_dir_all(&trigger_path) {
            log::warn!("Failed to create hrtimer trigger at {trigger_path}: {e:?}");
            return false;
        }
        log::debug!("Created hrtimer trigger at {trigger_path}");
    }

    // Find the hrtimer trigger device
    // Re-scan the context to pick up the newly created trigger
    let new_ctx = match industrial_io::context::Context::new() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Failed to create new IIO context for trigger: {e:?}");
            return true;
        }
    };

    for dev in new_ctx.devices() {
        if dev.is_trigger() {
            if let Some(trigger_name) = dev.name() {
                if trigger_name == HRTIMER_TRIGGER_NAME {
                    log::debug!("Found hrtimer trigger device: {trigger_name}");
                    // Set the sampling frequency on the trigger
                    if let Err(e) = dev.attr_write_float("sampling_frequency", DEFAULT_SAMPLE_RATE_HZ)
                    {
                        log::warn!("Failed to set hrtimer sampling frequency: {e:?}");
                    }
                    // Associate the trigger with the device
                    if let Err(e) = device.set_trigger(&dev) {
                        log::warn!("Failed to set hrtimer trigger: {e:?}");
                    } else {
                        log::info!(
                            "Using hrtimer trigger at {DEFAULT_SAMPLE_RATE_HZ} Hz"
                        );
                    }
                    return true;
                }
            }
        }
    }

    log::warn!("Failed to find hrtimer trigger device after creation");
    true
}

/// Try to set the sampling frequency on the device
fn try_set_sampling_frequency(device: &Device, freq: f64) {
    if let Err(e) = device.attr_write_float("sampling_frequency", freq) {
        log::debug!("Failed to set device sampling frequency to {freq}: {e:?}");
    } else {
        log::info!("Set device sampling frequency to {freq} Hz");
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

            let offset = match channel.attr_read_int("offset") {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("Unable to read offset for channel {id}: {:?}", e);
                    0
                }
            };

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

/// Returns true if any axis changed by more than the given threshold.
fn axis_changed(current: &AxisData, prev: &AxisData, threshold: f64) -> bool {
    (current.roll - prev.roll).abs() > threshold
        || (current.pitch - prev.pitch).abs() > threshold
        || (current.yaw - prev.yaw).abs() > threshold
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
