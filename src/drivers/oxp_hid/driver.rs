use std::{error::Error, ffi::CString};

use hidapi::HidDevice;

use crate::udev::device::UdevDevice;

use super::event::{BinaryInput, Event, GamepadButtonEvent};

pub const VID: u16 = 0x1a86;
pub const PID: u16 = 0xfe00;

const PACKET_SIZE: usize = 64;

// HID buffer read timeout
const HID_TIMEOUT: i32 = 10;

// HID command IDs
const CMD_BUTTON: u8 = 0xB2;

// Button codes
const BTN_M1: u8 = 0x22;
const BTN_M2: u8 = 0x23;
const BTN_KEYBOARD: u8 = 0x24;
const BTN_GUIDE: u8 = 0x21;

// Initialization commands to configure button mappings on the controller
const INIT_CMD_1: [u8; PACKET_SIZE] = gen_cmd(
    0xB4,
    &[
        0x02, 0x38, 0x02, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x02, 0x01, 0x02, 0x00,
        0x00, 0x00, 0x03, 0x01, 0x03, 0x00, 0x00, 0x00, 0x04, 0x01, 0x04, 0x00, 0x00, 0x00, 0x05,
        0x01, 0x05, 0x00, 0x00, 0x00, 0x06, 0x01, 0x06, 0x00, 0x00, 0x00, 0x07, 0x01, 0x07, 0x00,
        0x00, 0x00, 0x08, 0x01, 0x08, 0x00, 0x00, 0x00, 0x09, 0x01, 0x09, 0x00, 0x00, 0x00,
    ],
);

const INIT_CMD_2: [u8; PACKET_SIZE] = gen_cmd(
    0xB4,
    &[
        0x02, 0x38, 0x02, 0x02, 0x01, 0x0a, 0x01, 0x0a, 0x00, 0x00, 0x00, 0x0b, 0x01, 0x0b, 0x00,
        0x00, 0x00, 0x0c, 0x01, 0x0c, 0x00, 0x00, 0x00, 0x0d, 0x01, 0x0d, 0x00, 0x00, 0x00, 0x0e,
        0x01, 0x0e, 0x00, 0x00, 0x00, 0x0f, 0x01, 0x0f, 0x00, 0x00, 0x00, 0x10, 0x01, 0x10, 0x00,
        0x00, 0x00, 0x22, 0x02, 0x00, 0x00, 0x00, 0x00, 0x23, 0x02, 0x00, 0x00, 0x00, 0x00,
    ],
);

/// Generate an initialization command with format: [cid, 0x3F, 0x01, ...data, 0x3F, cid]
const fn gen_cmd(cid: u8, data: &[u8]) -> [u8; PACKET_SIZE] {
    let mut buf = [0u8; PACKET_SIZE];
    buf[0] = cid;
    buf[1] = 0x3F;
    buf[2] = 0x01;

    let mut i = 0;
    while i < data.len() && (i + 3) < PACKET_SIZE - 2 {
        buf[i + 3] = data[i];
        i += 1;
    }

    buf[PACKET_SIZE - 2] = 0x3F;
    buf[PACKET_SIZE - 1] = cid;
    buf
}

/// Generate the intercept disable command
const fn gen_intercept_disable() -> [u8; PACKET_SIZE] {
    gen_cmd(CMD_BUTTON, &[0x00, 0x01, 0x02])
}

const INIT_INTERCEPT_DISABLE: [u8; PACKET_SIZE] = gen_intercept_disable();

pub struct Driver {
    device: HidDevice,
    m1_pressed: bool,
    m2_pressed: bool,
    keyboard_pressed: bool,
    guide_pressed: bool,
    initialized: bool,
}

impl Driver {
    pub fn new(udevice: UdevDevice) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let path = udevice.devnode();
        let cs_path = CString::new(path.clone())?;
        let api = hidapi::HidApi::new()?;
        let device = api.open_path(&cs_path)?;
        let info = device.get_device_info()?;
        if info.vendor_id() != VID || info.product_id() != PID {
            return Err(format!("Device '{path}' is not an OXP X1 HID controller").into());
        }

        Ok(Self {
            device,
            m1_pressed: false,
            m2_pressed: false,
            keyboard_pressed: false,
            guide_pressed: false,
            initialized: false,
        })
    }

    /// Send initialization commands to the device
    fn initialize(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        log::debug!("Sending OXP HID initialization commands");
        self.device.write(&INIT_CMD_1)?;
        self.device.write(&INIT_CMD_2)?;
        self.device.write(&INIT_INTERCEPT_DISABLE)?;
        self.initialized = true;
        log::info!("OXP HID controller initialized");
        Ok(())
    }

    /// Poll the device and read input reports
    pub fn poll(&mut self) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        if !self.initialized {
            self.initialize()?;
        }

        let mut buf = [0u8; PACKET_SIZE];
        let bytes_read = self.device.read_timeout(&mut buf[..], HID_TIMEOUT)?;

        if bytes_read == 0 {
            return Ok(Vec::new());
        }

        if bytes_read < PACKET_SIZE {
            return Ok(Vec::new());
        }

        let cid = buf[0];
        let valid = buf[1] == 0x3F && buf[PACKET_SIZE - 2] == 0x3F;

        if !valid {
            log::trace!("OXP HID: invalid packet framing");
            return Ok(Vec::new());
        }

        // Skip non-button command responses
        if cid != CMD_BUTTON {
            return Ok(Vec::new());
        }

        let btn = buf[6];
        let pressed = buf[12] == 1;

        let (prev, event_fn): (&mut bool, fn(BinaryInput) -> GamepadButtonEvent) = match btn {
            BTN_M1 => (&mut self.m1_pressed, GamepadButtonEvent::M1),
            BTN_M2 => (&mut self.m2_pressed, GamepadButtonEvent::M2),
            BTN_KEYBOARD => (&mut self.keyboard_pressed, GamepadButtonEvent::Keyboard),
            BTN_GUIDE => (&mut self.guide_pressed, GamepadButtonEvent::Guide),
            0x00 => return Ok(Vec::new()),
            _ => {
                log::trace!("OXP HID: unknown button code: 0x{btn:02x}");
                return Ok(Vec::new());
            }
        };

        // Debounce: skip if state unchanged
        if *prev == pressed {
            return Ok(Vec::new());
        }
        *prev = pressed;

        Ok(vec![Event::GamepadButton(event_fn(BinaryInput { pressed }))])
    }
}
