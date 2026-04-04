use std::{
    collections::HashSet,
    error::Error,
    f64::consts::PI,
    fmt::Debug,
    time::Duration,
};

use crate::{
    config,
    drivers::iio_imu::{self, driver::Driver, info::MountMatrix},
    input::{
        capability::{Capability, Source},
        event::{native::NativeEvent, value::InputValue},
        source::{InputError, SourceInputDevice, SourceOutputDevice},
    },
    udev::device::UdevDevice,
};

const RESUME_RECOVER_DELAY: Duration = Duration::from_secs(3);

pub struct AccelGyro3dImu {
    driver: Option<Driver>,
    capabilities: Vec<Capability>,
    device_id: String,
    device_name: String,
    mount_matrix: Option<MountMatrix>,
    sample_rate: Option<f64>,
    event_filter: HashSet<Capability>,
}

impl AccelGyro3dImu {
    /// Create a new Accel Gyro 3D source device with the given udev
    /// device information
    pub fn new(
        device_info: UdevDevice,
        config: Option<config::IIO>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        // Override the mount matrix if one is defined in the config
        let mount_matrix = if let Some(config) = config.as_ref() {
            #[allow(deprecated)]
            if let Some(matrix_config) = config.mount_matrix.as_ref() {
                let matrix = MountMatrix {
                    x: (matrix_config.x[0], matrix_config.x[1], matrix_config.x[2]),
                    y: (matrix_config.y[0], matrix_config.y[1], matrix_config.y[2]),
                    z: (matrix_config.z[0], matrix_config.z[1], matrix_config.z[2]),
                };
                Some(matrix)
            } else {
                None
            }
        } else {
            None
        };

        let sample_rate = config.as_ref().and_then(|c| c.sample_rate);

        let id = device_info.sysname();
        let name = device_info.name();
        let driver = Driver::new(
            id.clone(),
            name.clone(),
            mount_matrix.clone(),
            sample_rate,
        )?;

        // accel and gyro may be separate IIO devices on HID Sensor Hub
        let mut capabilities = vec![];
        if driver.has_accel() {
            capabilities.push(Capability::Accelerometer(Source::Center));
        }
        if driver.has_gyro() {
            capabilities.push(Capability::Gyroscope(Source::Center));
        }
        log::debug!("AccelGyro3dImu capabilities: {capabilities:?}");

        Ok(Self {
            driver: Some(driver),
            capabilities,
            device_id: id,
            device_name: name,
            mount_matrix,
            sample_rate,
            event_filter: HashSet::new(),
        })
    }
}

impl SourceInputDevice for AccelGyro3dImu {
    fn poll(&mut self) -> Result<Vec<NativeEvent>, InputError> {
        let Some(ref mut driver) = self.driver else {
            return Ok(vec![]);
        };
        let events = driver.poll()?;
        Ok(translate_events(events))
    }

    fn get_capabilities(&self) -> Result<Vec<Capability>, InputError> {
        Ok(self.capabilities.clone())
    }

    fn on_suspend(&mut self) {
        log::info!("Tearing down IIO driver for {} before suspend", self.device_name);
        self.driver = None;
    }

    fn on_resume(&mut self) {
        if self.driver.is_some() {
            return;
        }

        log::info!("Recreating IIO driver for {} after resume", self.device_name);
        std::thread::sleep(RESUME_RECOVER_DELAY);

        match Driver::new(
            self.device_id.clone(),
            self.device_name.clone(),
            self.mount_matrix.clone(),
            self.sample_rate,
        ) {
            Ok(mut new_driver) => {
                new_driver.update_filtered_events(self.event_filter.clone());
                log::info!("IIO driver recreated for {} after resume", self.device_name);
                self.driver = Some(new_driver);
            }
            Err(e) => {
                log::error!(
                    "Failed to recreate IIO driver for {}: {e}",
                    self.device_name
                );
            }
        }
    }

    fn update_event_filter(&mut self, events: HashSet<Capability>) -> Result<(), InputError> {
        self.event_filter = events.clone();
        if let Some(ref mut driver) = self.driver {
            driver.update_filtered_events(events);
        }
        Ok(())
    }

    fn get_default_event_filter(&self) -> Result<HashSet<Capability>, InputError> {
        let Some(ref driver) = self.driver else {
            return Ok(HashSet::new());
        };
        driver
            .get_default_event_filter()
            .map_err(|e| format!("Failed to get default event filter: {:?}", e).into())
    }
}

impl SourceOutputDevice for AccelGyro3dImu {}

impl Debug for AccelGyro3dImu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccelGyro3dImu").finish()
    }
}

// NOTE: Mark this struct as thread-safe as it will only ever be called from
// a single thread.
unsafe impl Send for AccelGyro3dImu {}

fn translate_events(events: Vec<iio_imu::event::Event>) -> Vec<NativeEvent> {
    events.into_iter().map(translate_event).collect()
}

fn translate_event(event: iio_imu::event::Event) -> NativeEvent {
    match event {
        iio_imu::event::Event::Accelerometer(data) => {
            let factor = 1.0 / 0.0006125; // m/s² → UHID LSB
            let cap = Capability::Accelerometer(Source::Center);
            let value = InputValue::Vector3 {
                x: Some(data.roll * factor),
                y: Some(data.pitch * factor),
                z: Some(data.yaw * factor),
            };
            NativeEvent::new(cap, value)
        }
        iio_imu::event::Event::Gyro(data) => {
            let factor = (180.0 / PI) / 0.0625; // rad/s → UHID LSB
            let cap = Capability::Gyroscope(Source::Center);
            let value = InputValue::Vector3 {
                x: Some(data.roll * factor),
                y: Some(data.pitch * factor),
                z: Some(data.yaw * factor),
            };
            NativeEvent::new(cap, value)
        }
    }
}
