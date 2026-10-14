use std::{
    collections::HashSet,
    error::Error,
    fmt, fs,
    os::fd::RawFd,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use crate::input::capability::{Capability, Source};

use super::{
    event::{AxisData, Event},
    info::{
        cleanup_buffer, compute_layout, configure_buffer, configure_sysfs_rate, decode_group,
        discover_group, discover_sysfs_group, discover_timestamp, group_scale,
        is_retryable_read_error, read_mount_matrix, read_sysfs_group, BufferGuard, MountMatrix,
        NonblockingFdSource, RecordLayout, RecordSource, SysfsChannel, TriggerStrategy,
    },
};

const POLL_SLOW_THRESHOLD: Duration = Duration::from_secs(1);
const POLL_BACKOFF_SLEEP: Duration = Duration::from_secs(3);

#[derive(Debug)]
enum ReadMode {
    Buffer {
        source: Box<dyn RecordSource>,
        layout: Box<RecordLayout>,
        record: Vec<u8>,
        accel_scale: f64,
        gyro_scale: f64,
        // Includes hardware-trigger buffers; Drop also rolls back failed setup.
        _guard: BufferGuard,
    },
    Sysfs {
        accel: Vec<SysfsChannel>,
        gyro: Vec<SysfsChannel>,
    },
}

pub struct Driver {
    mount_matrix: MountMatrix,
    mode: ReadMode,
    filtered_events: HashSet<Capability>,
    poll_interval: Duration,
    accel_state: Option<AxisData>,
    gyro_state: Option<AxisData>,
}

impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver").field("mode", &self.mode).finish()
    }
}

impl Driver {
    /// Create an IIO driver with raw-buffer decoding and direct sysfs fallback.
    pub fn new(
        name: String,
        matrix: Option<MountMatrix>,
        use_buffer: Option<bool>,
        sample_rate: Option<f64>,
        trigger: TriggerStrategy,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        Self::new_at(
            &PathBuf::from(format!("/sys/bus/iio/devices/{name}")),
            &PathBuf::from(format!("/dev/{name}")),
            matrix,
            use_buffer,
            sample_rate,
            trigger,
        )
    }

    fn new_at(
        base: &Path,
        devnode: &Path,
        matrix: Option<MountMatrix>,
        use_buffer: Option<bool>,
        sample_rate: Option<f64>,
        trigger: TriggerStrategy,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if !base.is_dir() {
            return Err(format!("IIO device path not found: {}", base.display()).into());
        }
        let mount_matrix = if let Some(matrix) = matrix {
            matrix
        } else if let Some(matrix) = read_mount_matrix(base) {
            MountMatrix::new(matrix)?
        } else {
            MountMatrix::default()
        };

        let buffered = if use_buffer != Some(false) {
            match Self::buffer_mode(base, devnode, sample_rate, trigger) {
                Ok(buffered) => Some(buffered),
                Err(error) => {
                    log::warn!(
                        "IIO buffer unavailable for {}: {error}; using sysfs",
                        base.display()
                    );
                    None
                }
            }
        } else {
            None
        };
        let (mode, rate) = if let Some(buffered) = buffered {
            buffered
        } else {
            cleanup_buffer(base);
            let accel = discover_sysfs_group(base, "accel");
            let gyro = discover_sysfs_group(base, "anglvel");
            if accel.is_empty() && gyro.is_empty() {
                return Err(format!("No readable IMU channels in {}", base.display()).into());
            }
            let rate = configure_sysfs_rate(base, &accel, &gyro, sample_rate);
            (ReadMode::Sysfs { accel, gyro }, rate)
        };
        // Invalid device-reported rates must not panic, overflow or busy-loop.
        let poll_interval = Duration::try_from_secs_f64(1.0 / rate)
            .ok()
            .filter(|interval| !interval.is_zero())
            .unwrap_or(Duration::from_millis(5));

        Ok(Self {
            mount_matrix,
            mode,
            filtered_events: Default::default(),
            poll_interval,
            accel_state: None,
            gyro_state: None,
        })
    }

    fn buffer_mode(
        base: &Path,
        devnode: &Path,
        sample_rate: Option<f64>,
        trigger: TriggerStrategy,
    ) -> Result<(ReadMode, f64), Box<dyn Error + Send + Sync>> {
        let accel = discover_group(base, "accel")?;
        let gyro = discover_group(base, "anglvel")?;
        if accel.is_empty() && gyro.is_empty() {
            return Err("No IIO scan channels available".into());
        }
        let timestamp = discover_timestamp(base)?;
        // HID sensor hubs may require a direct read to request the next sample.
        let kick_path = match &trigger {
            TriggerStrategy::FindExisting => accel
                .first()
                .or(gyro.first())
                .map(|channel| base.join(format!("{}_raw", channel.id))),
            TriggerStrategy::Hrtimer(_) => None,
        };
        let config = configure_buffer(base, &accel, &gyro, sample_rate, trigger)?;
        let layout = compute_layout(accel, gyro, timestamp);
        let accel_scale = group_scale(&layout.accel, "accel");
        let gyro_scale = group_scale(&layout.gyro, "anglvel");
        let source = NonblockingFdSource::open(devnode, kick_path)?;
        let record = vec![0; layout.record_len];
        Ok((
            ReadMode::Buffer {
                source: Box::new(source),
                layout: Box::new(layout),
                record,
                accel_scale,
                gyro_scale,
                _guard: config.guard,
            },
            config.rate,
        ))
    }

    pub fn update_filtered_events(&mut self, events: HashSet<Capability>) {
        self.filtered_events = events;
    }

    pub fn has_accel(&self) -> bool {
        match &self.mode {
            ReadMode::Buffer { layout, .. } => !layout.accel.is_empty(),
            ReadMode::Sysfs { accel, .. } => !accel.is_empty(),
        }
    }

    pub fn has_gyro(&self) -> bool {
        match &self.mode {
            ReadMode::Buffer { layout, .. } => !layout.gyro.is_empty(),
            ReadMode::Sysfs { gyro, .. } => !gyro.is_empty(),
        }
    }

    pub fn poll_fd(&self) -> Option<RawFd> {
        match &self.mode {
            ReadMode::Buffer { source, .. } => Some(source.poll_fd()),
            ReadMode::Sysfs { .. } => None,
        }
    }

    pub fn get_default_event_filter(
        &self,
    ) -> Result<HashSet<Capability>, Box<dyn Error + Send + Sync>> {
        match fs::read_to_string("/proc/modules") {
            Ok(modules)
                if modules
                    .lines()
                    .any(|line| line.split_whitespace().next() == Some("hid_lenovo_go")) =>
            {
                Ok(HashSet::from([
                    Capability::Accelerometer(Source::Center),
                    Capability::Gyroscope(Source::Center),
                ]))
            }
            Ok(_) => Ok(HashSet::new()),
            Err(error) => Err(format!("Failed to read '/proc/modules': {error}").into()),
        }
    }

    /// Return changed SI samples. Raw fds never wait here; the owning source loop
    /// handles bounded fd waits so suspend, resume, and stop remain responsive.
    pub fn poll(&mut self) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        let start = Instant::now();
        let accel_enabled = !self
            .filtered_events
            .contains(&Capability::Accelerometer(Source::Center));
        let gyro_enabled = !self
            .filtered_events
            .contains(&Capability::Gyroscope(Source::Center));
        let (accel, gyro) = match &mut self.mode {
            ReadMode::Buffer {
                source,
                layout,
                record,
                accel_scale,
                gyro_scale,
                ..
            } => {
                if let Err(error) = source.read_one(record) {
                    if is_retryable_read_error(&error) {
                        return Ok(vec![]);
                    }
                    return Err(error.into());
                }
                (
                    (accel_enabled && !layout.accel.is_empty())
                        .then(|| decode_group(&layout.accel, *accel_scale, record)),
                    (gyro_enabled && !layout.gyro.is_empty())
                        .then(|| decode_group(&layout.gyro, *gyro_scale, record)),
                )
            }
            ReadMode::Sysfs { accel, gyro } => (
                if accel_enabled {
                    read_sysfs_group(accel)?
                } else {
                    None
                },
                if gyro_enabled {
                    read_sysfs_group(gyro)?
                } else {
                    None
                },
            ),
        };
        let mut events = Vec::new();
        if let Some(mut data) = accel {
            self.rotate_value(&mut data);
            if self.accel_state.as_ref() != Some(&data) {
                self.accel_state = Some(data.clone());
                events.push(Event::Accelerometer(data));
            }
        }
        if let Some(mut data) = gyro {
            self.rotate_value(&mut data);
            if self.gyro_state.as_ref() != Some(&data) {
                self.gyro_state = Some(data.clone());
                events.push(Event::Gyroscope(data));
            }
        }
        if matches!(self.mode, ReadMode::Sysfs { .. }) {
            if start.elapsed() > POLL_SLOW_THRESHOLD {
                log::warn!(
                    "IIO sysfs poll took {:.1}s; backing off while sensor hub recovers",
                    start.elapsed().as_secs_f64()
                );
                thread::sleep(POLL_BACKOFF_SLEEP);
            } else if let Some(remaining) = self.poll_interval.checked_sub(start.elapsed()) {
                thread::sleep(remaining);
            }
        }
        Ok(events)
    }

    fn rotate_value(&self, value: &mut AxisData) {
        let (x, y, z) = (value.roll, value.pitch, value.yaw);
        let m = &self.mount_matrix;
        value.roll = m.x.0 * x + m.x.1 * y + m.x.2 * z;
        value.pitch = m.y.0 * x + m.y.1 * y + m.y.2 * z;
        value.yaw = m.z.0 * x + m.z.1 * y + m.z.2 * z;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::iio_imu::test_support::TestDir;

    #[test]
    fn use_buffer_false_bypasses_scan_layout_trigger_and_devnode() {
        let dir = TestDir::new();
        dir.write("in_accel_x_raw", "10");
        dir.write("in_accel_scale", "0.5");
        // Deliberately malformed scan metadata must never be read in sysfs-only mode.
        dir.write("scan_elements/in_accel_x_index", "invalid");
        let mut driver = Driver::new_at(
            dir.path(),
            &dir.path().join("missing-devnode"),
            None,
            Some(false),
            Some(1000.0),
            TriggerStrategy::Hrtimer("never-created".into()),
        )
        .unwrap();
        assert!(driver.has_accel());
        assert!(!driver.has_gyro());
        assert_eq!(driver.poll_fd(), None);
        let events = driver.poll().unwrap();
        assert!(
            matches!(&events[..], [Event::Accelerometer(data)] if data.roll * data.scale == 5.0)
        );
        assert!(driver.poll().unwrap().is_empty()); // upstream unchanged-sample suppression
        driver.update_filtered_events(HashSet::from([Capability::Accelerometer(Source::Center)]));
        dir.write("in_accel_x_raw", "12");
        assert!(driver.poll().unwrap().is_empty());
        driver.update_filtered_events(HashSet::new());
        assert!(
            matches!(&driver.poll().unwrap()[..], [Event::Accelerometer(data)] if data.roll * data.scale == 6.0)
        );
    }

    #[test]
    fn unavailable_raw_buffer_falls_back_to_sysfs_with_mount_matrix() {
        let dir = TestDir::new();
        dir.write("in_anglvel_x_raw", "2");
        dir.write("in_anglvel_y_raw", "3");
        dir.write("in_anglvel_scale", "0.25");
        dir.write("in_mount_matrix", "0, 1, 0; -1, 0, 0; 0, 0, 1");
        let mut driver = Driver::new_at(
            dir.path(),
            &dir.path().join("absent"),
            None,
            None,
            Some(1000.0),
            TriggerStrategy::FindExisting,
        )
        .unwrap();
        assert!(!driver.has_accel());
        assert!(driver.has_gyro());
        assert!(
            matches!(&driver.poll().unwrap()[..], [Event::Gyroscope(data)] if data.roll == 0.75 && data.pitch == -0.5 && data.scale == 1.0)
        );
    }

    #[test]
    fn sysfs_poll_propagates_terminal_device_failures() {
        let dir = TestDir::new();
        dir.write("in_accel_x_raw", "1");
        let mut driver = Driver::new_at(
            dir.path(),
            &dir.path().join("absent"),
            None,
            Some(false),
            None,
            TriggerStrategy::FindExisting,
        )
        .unwrap();
        fs::remove_file(dir.path().join("in_accel_x_raw")).unwrap();
        assert!(driver.poll().is_err());
    }
}
