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
        capability::{Capability, Gamepad},
        event::{native::NativeEvent, value::InputValue},
        source::{InputError, SourceInputDevice, SourceOutputDevice},
    },
    udev::device::UdevDevice,
};

const RESUME_RECOVER_DELAY: Duration = Duration::from_secs(3);

pub struct BmiImu {
    driver: Option<Driver>,
    device_id: String,
    device_name: String,
    mount_matrix: Option<MountMatrix>,
    sample_rate: Option<f64>,
    event_filter: HashSet<Capability>,
}

impl BmiImu {
    /// Create a new BMI IMU source device with the given udev
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
        let driver = Driver::new(id.clone(), name.clone(), mount_matrix.clone(), sample_rate)?;

        Ok(Self {
            driver: Some(driver),
            device_id: id,
            device_name: name,
            mount_matrix,
            sample_rate,
            event_filter: HashSet::new(),
        })
    }
}

impl SourceInputDevice for BmiImu {
    fn poll(&mut self) -> Result<Vec<NativeEvent>, InputError> {
        let Some(ref mut driver) = self.driver else {
            return Ok(vec![]);
        };
        let events = driver.poll()?;
        Ok(translate_events(events))
    }

    fn get_capabilities(&self) -> Result<Vec<Capability>, InputError> {
        Ok(CAPABILITIES.into())
    }

    fn on_suspend(&mut self) {
        let name = &self.device_name;
        log::info!("Tearing down IIO driver for {name} before suspend");
        self.driver = None;
    }

    fn on_resume(&mut self) {
        let name = &self.device_name;
        if self.driver.is_some() {
            return;
        }

        log::info!("Recreating IIO driver for {name} after resume");
        std::thread::sleep(RESUME_RECOVER_DELAY);

        match Driver::new(
            self.device_id.clone(),
            self.device_name.clone(),
            self.mount_matrix.clone(),
            self.sample_rate,
        ) {
            Ok(mut new_driver) => {
                new_driver.update_filtered_events(self.event_filter.clone());
                log::info!("IIO driver recreated for {name} after resume");
                self.driver = Some(new_driver);
            }
            Err(e) => {
                log::error!("Failed to recreate IIO driver for {name}: {e}");
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

impl SourceOutputDevice for BmiImu {}

impl Debug for BmiImu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BmiImu").finish()
    }
}

// NOTE: Mark this struct as thread-safe as it will only ever be called from
// a single thread.
unsafe impl Send for BmiImu {}

fn translate_events(events: Vec<iio_imu::event::Event>) -> Vec<NativeEvent> {
    events.into_iter().map(translate_event).collect()
}

fn translate_event(event: iio_imu::event::Event) -> NativeEvent {
    match event {
        iio_imu::event::Event::Accelerometer(data) => {
            let cap = Capability::Gamepad(Gamepad::Accelerometer);
            let value = InputValue::Vector3 {
                x: Some(data.roll),
                y: Some(data.pitch),
                z: Some(data.yaw),
            };
            NativeEvent::new(cap, value)
        }
        iio_imu::event::Event::Gyro(data) => {
            let cap = Capability::Gamepad(Gamepad::Gyro);
            let value = InputValue::Vector3 {
                x: Some(data.roll * (180.0 / PI) * 12.0),
                y: Some(data.pitch * (180.0 / PI) * 12.0),
                z: Some(data.yaw * (180.0 / PI) * 12.0),
            };
            NativeEvent::new(cap, value)
        }
    }
}

pub const CAPABILITIES: &[Capability] = &[
    Capability::Gamepad(Gamepad::Accelerometer),
    Capability::Gamepad(Gamepad::Gyro),
];
