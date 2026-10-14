use std::{collections::HashSet, error::Error, fmt::Debug, os::fd::RawFd, time::Duration};

use crate::{
    config,
    drivers::iio_imu::{
        driver::Driver,
        info::{MountMatrix, TriggerStrategy},
    },
    input::{
        capability::{Capability, Source},
        event::native::NativeEvent,
        source::{InputError, SourceInputDevice, SourceOutputDevice},
    },
    udev::device::UdevDevice,
};

use super::common::{
    corrections_from_config, mount_matrix_from_config, sample_rate_from_config, translate_events,
    use_buffer_from_config,
};

const RESUME_RECOVER_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct AccelGyro3dImu {
    driver: Option<Driver>,
    capabilities: Vec<Capability>,
    device_id: String,
    device_name: String,
    mount_matrix: Option<MountMatrix>,
    use_buffer: Option<bool>,
    sample_rate: Option<f64>,
    accel_correction: f64,
    gyro_correction: f64,
    event_filter: HashSet<Capability>,
}

impl AccelGyro3dImu {
    /// Create a HID Sensor Hub source, retaining configuration for resume.
    pub fn new(
        device_info: UdevDevice,
        config: Option<config::SourceDevice>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let mount_matrix = mount_matrix_from_config(config.as_ref());
        let sample_rate = sample_rate_from_config(config.as_ref());
        let use_buffer = use_buffer_from_config(config.as_ref());
        let (accel_correction, gyro_correction) = corrections_from_config(config.as_ref());
        let id = device_info.sysname();
        let name = device_info.name();
        let driver = Driver::new(
            id.clone(),
            mount_matrix.clone(),
            use_buffer,
            sample_rate,
            TriggerStrategy::FindExisting,
        )?;
        // Sensor hubs may expose accelerometer and gyroscope as separate devices.
        let mut capabilities = vec![];
        if driver.has_accel() {
            capabilities.push(Capability::Accelerometer(Source::Center));
        }
        if driver.has_gyro() {
            capabilities.push(Capability::Gyroscope(Source::Center));
        }
        Ok(Self {
            driver: Some(driver),
            capabilities,
            device_id: id,
            device_name: name,
            mount_matrix,
            use_buffer,
            sample_rate,
            accel_correction,
            gyro_correction,
            event_filter: HashSet::new(),
        })
    }
}

impl SourceInputDevice for AccelGyro3dImu {
    fn poll(&mut self) -> Result<Vec<NativeEvent>, InputError> {
        let Some(driver) = self.driver.as_mut() else {
            return Ok(vec![]);
        };
        Ok(translate_events(
            driver.poll()?,
            self.accel_correction,
            self.gyro_correction,
        ))
    }

    fn get_capabilities(&self) -> Result<Vec<Capability>, InputError> {
        Ok(self.capabilities.clone())
    }

    fn get_poll_fds(&self) -> Vec<RawFd> {
        self.driver
            .as_ref()
            .and_then(Driver::poll_fd)
            .into_iter()
            .collect()
    }

    fn on_suspend(&mut self) {
        log::info!(
            "Tearing down IIO driver for {} before suspend",
            self.device_name
        );
        // Drops the raw fd, disables its buffer and unbinds/removes owned triggers.
        self.driver = None;
    }

    fn on_resume(&mut self) {
        if self.driver.is_some() {
            return;
        }
        log::info!(
            "Recreating IIO driver for {} after resume",
            self.device_name
        );
        std::thread::sleep(RESUME_RECOVER_DELAY);
        match Driver::new(
            self.device_id.clone(),
            self.mount_matrix.clone(),
            self.use_buffer,
            self.sample_rate,
            TriggerStrategy::FindExisting,
        ) {
            Ok(mut driver) => {
                driver.update_filtered_events(self.event_filter.clone());
                self.driver = Some(driver);
                log::info!("IIO driver recreated for {} after resume", self.device_name);
            }
            Err(error) => log::error!(
                "Failed to recreate IIO driver for {}: {error}",
                self.device_name
            ),
        }
    }

    fn update_event_filter(&mut self, events: HashSet<Capability>) -> Result<(), InputError> {
        self.event_filter = events.clone();
        if let Some(driver) = self.driver.as_mut() {
            driver.update_filtered_events(events);
        }
        Ok(())
    }

    fn get_default_event_filter(&self) -> Result<HashSet<Capability>, InputError> {
        match self.driver.as_ref() {
            Some(driver) => Ok(driver.get_default_event_filter()?),
            None => Ok(self.event_filter.clone()),
        }
    }
}

impl SourceOutputDevice for AccelGyro3dImu {}
