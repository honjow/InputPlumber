// References:
// - https://github.com/zezba9000/MSI-Claw-Gamepad-Mode/blob/main/main.c
// - https://github.com/NeroReflex/hid-msi-claw-dkms/blob/main/hid-msi-claw.c
use std::{error::Error, ffi::CString, time::Duration};

use hidapi::HidDevice;
use packed_struct::PackedStruct;
use udev::Device;

use crate::{
    drivers::msi_claw::hid_report::Command,
    udev::device::{AttributeGetter, AttributeSetter, UdevDevice},
};

use super::hid_report::{GamepadMode, MkeysFunction, PackedCommandReport};

// Hardware ID's
pub const VID: u16 = 0x0db0;
pub const PID: u16 = 0x1901;

pub struct Driver {
    device: Option<HidDevice>,
}

impl Driver {
    pub fn new(udevice: UdevDevice) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let vid = udevice.id_vendor();
        let pid = udevice.id_product();
        if VID != vid || PID != pid {
            return Err(format!("'{}' is not a MSI Claw controller", udevice.devnode()).into());
        }

        let device = udevice.get_device()?;
        let if_num = device.get_attribute_from_tree("bInterfaceNumber");

        match if_num.as_str() {
            "01" => {
                // Interface 01: Configure M1/M2 buttons via sysfs
                log::debug!("Configuring M1/M2 buttons on interface 01");
                configure_m_remap(device);
                Ok(Self { device: None })
            }
            "02" => {
                // Interface 02: Open hidraw for mode switching
                let path = udevice.devnode();
                let path = CString::new(path)?;
                let api = hidapi::HidApi::new()?;
                let hid_device = api.open_path(&path)?;
                hid_device.set_blocking_mode(false)?;
                Ok(Self { device: Some(hid_device) })
            }
            _ => Err(format!("Invalid interface number {if_num}").into()),
        }
    }

    pub fn poll(&self) -> Result<Option<PackedCommandReport>, Box<dyn Error + Send + Sync>> {
        let Some(ref device) = self.device else {
            return Ok(None);
        };

        let mut buf = [0; 8];
        let bytes_read = device.read(&mut buf[..])?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let slice = &buf[..bytes_read];

        log::debug!("Got response bytes: {slice:?}");
        let report = PackedCommandReport::unpack(&buf)?;
        log::debug!("Response: {report}");

        if report.command == Command::GamepadModeAck {
            let mode: GamepadMode = report.arg1.into();
            log::debug!("Current gamepad mode: {mode:?}");
        }

        Ok(Some(report))
    }

    // Configure the device to be in the given mode
    // TODO: Update to use sysfs interface when kernel support is upstreamed
    pub fn set_mode(
        &self,
        mode: GamepadMode,
        mkeys: MkeysFunction,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let Some(ref device) = self.device else {
            return Ok(());
        };

        let report = PackedCommandReport::switch_mode(mode, mkeys);
        let data = report.pack()?;

        // The Claw appears to use a ring buffer of 64 bytes, so keep writing
        // the command until the ring buffer is full and an ACK response is
        // received. Attempts (buffer_size / report_size) number of times (8).
        for _ in 0..8 {
            // Write the SetMode command
            device.write(&data)?;
            std::thread::sleep(Duration::from_millis(50));

            // Poll the device for an acknowlgement response
            let Some(report) = self.poll()? else {
                continue;
            };

            // TODO: Validate that the device switched gamepad modes
            match report.command {
                Command::Ack | Command::GamepadModeAck => break,
                _ => break,
            }
        }

        Ok(())
    }

    /// Send a get mode request to the device
    pub fn get_mode(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let Some(ref device) = self.device else {
            return Ok(());
        };

        let report = PackedCommandReport::read_mode();
        let data = report.pack()?;
        device.write(&data)?;

        Ok(())
    }
}

/// Configure M1/M2 button remapping via sysfs (requires hid-msi-claw kernel driver)
fn configure_m_remap(mut device: Device) {
    // M1 -> KEY_INSERT, M2 -> KEY_DELETE
    // These will be captured by InputPlumber and mapped via capability_maps
    set_attribute(&mut device, "m1_remap", "KEY_INSERT");
    set_attribute(&mut device, "m2_remap", "KEY_DELETE");
}

fn set_attribute(device: &mut Device, attribute: &str, value: &str) {
    match device.set_attribute_on_tree(attribute, value) {
        Ok(_) => log::debug!("Set {attribute} to {value}"),
        Err(e) => log::debug!("Could not set {attribute} to {value}: {e:?}"),
    }
}
