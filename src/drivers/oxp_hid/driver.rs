use std::{error::Error, ffi::CString};

use hidapi::HidDevice;

use super::event::{BinaryInput, ButtonEvent, Event};

pub const VID: u16 = 0x1a86;
pub const PID: u16 = 0xfe00;
pub const IID: i32 = 0x02;

const PACKET_SIZE: usize = 64;

// HID buffer read timeout
const HID_TIMEOUT: i32 = 10;

// HID command IDs
const CMD_BUTTON: u8 = 0xB2;

// Button codes for vendor HID report mode events
const BTN_GUIDE: u8 = 0x21;
const BTN_M1: u8 = 0x22;
const BTN_M2: u8 = 0x23;
const BTN_KEYBOARD: u8 = 0x24;

// B4 button mapping commands: configure M1/M2 as keyboard mode (key=0x00)
// to disable their default LT/RT mirroring on the Xbox gamepad.
// byte6 = 0x20 (protocol constant, confirmed by firmware readback on X1 Mini).
const INIT_CMD_1: [u8; PACKET_SIZE] = gen_cmd(
    0xB4,
    &[
        0x02, 0x38, 0x20, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x02, 0x01, 0x02, 0x00,
        0x00, 0x00, 0x03, 0x01, 0x03, 0x00, 0x00, 0x00, 0x04, 0x01, 0x04, 0x00, 0x00, 0x00, 0x05,
        0x01, 0x05, 0x00, 0x00, 0x00, 0x06, 0x01, 0x06, 0x00, 0x00, 0x00, 0x07, 0x01, 0x07, 0x00,
        0x00, 0x00, 0x08, 0x01, 0x08, 0x00, 0x00, 0x00, 0x09, 0x01, 0x09, 0x00, 0x00, 0x00,
    ],
);

const INIT_CMD_2: [u8; PACKET_SIZE] = gen_cmd(
    0xB4,
    &[
        0x02, 0x38, 0x20, 0x02, 0x01, 0x0a, 0x01, 0x0a, 0x00, 0x00, 0x00, 0x0b, 0x01, 0x0b, 0x00,
        0x00, 0x00, 0x0c, 0x01, 0x0c, 0x00, 0x00, 0x00, 0x0d, 0x01, 0x0d, 0x00, 0x00, 0x00, 0x0e,
        0x01, 0x0e, 0x00, 0x00, 0x00, 0x0f, 0x01, 0x0f, 0x00, 0x00, 0x00, 0x10, 0x01, 0x10, 0x00,
        0x00, 0x00, 0x22, 0x02, 0x00, 0x00, 0x00, 0x00, 0x23, 0x02, 0x00, 0x00, 0x00, 0x00,
    ],
);

/// Generate a command with 0x3F framing: [cid, 0x3F, 0x01, ...data, 0x3F, cid]
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

// B2 report mode activation: ENABLE then DISABLE cycle.
// Required on Apex; harmless on X1 Mini (tested: both phases produce events).
const B2_ENABLE: [u8; PACKET_SIZE] = gen_cmd(CMD_BUTTON, &[0x03, 0x01, 0x02]);
const B2_DISABLE: [u8; PACKET_SIZE] = gen_cmd(CMD_BUTTON, &[0x00, 0x01, 0x02]);

pub struct Driver {
    device: HidDevice,
    btn_state: [bool; 0x25],
    initialized: bool,
}

impl Driver {
    pub fn new(path: String) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let cs_path = CString::new(path.clone())?;
        let api = hidapi::HidApi::new()?;
        let device = api.open_path(&cs_path)?;
        let info = device.get_device_info()?;
        if info.vendor_id() != VID || info.product_id() != PID {
            return Err(format!("Device '{path}' is not an OXP HID controller").into());
        }

        Ok(Self {
            device,
            btn_state: [false; 0x25],
            initialized: false,
        })
    }

    /// Send initialization commands: B4 button mapping + B2 report mode activation.
    fn initialize(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        log::debug!("Sending OXP HID initialization commands");

        // Configure button mappings (M1/M2 → keyboard mode, disables Xbox LT/RT mirror)
        self.device.write(&INIT_CMD_1)?;
        self.device.write(&INIT_CMD_2)?;

        // Activate report mode via B2 ENABLE→DISABLE cycle.
        // This is required on Apex and harmless on X1 Mini.
        self.device.write(&B2_ENABLE)?;
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Drain any ACK responses from the enable command
        let mut drain_buf = [0u8; PACKET_SIZE];
        for _ in 0..10 {
            if self.device.read_timeout(&mut drain_buf, 50)? == 0 {
                break;
            }
        }

        self.device.write(&B2_DISABLE)?;
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Drain disable ACK
        for _ in 0..10 {
            if self.device.read_timeout(&mut drain_buf, 50)? == 0 {
                break;
            }
        }

        log::info!("OXP HID controller initialized (report mode active)");
        self.initialized = true;
        Ok(())
    }

    /// Poll the device and read input reports
    pub fn poll(&mut self) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        if !self.initialized {
            self.initialize()?;
        }

        let mut buf = [0u8; PACKET_SIZE];
        let bytes_read = self.device.read_timeout(&mut buf[..], HID_TIMEOUT)?;

        if bytes_read < PACKET_SIZE {
            return Ok(Vec::new());
        }

        let cid = buf[0];
        let valid = buf[1] == 0x3F && buf[PACKET_SIZE - 2] == 0x3F;

        if !valid || cid != CMD_BUTTON {
            return Ok(Vec::new());
        }

        let btn = buf[6];
        let pressed = buf[12] == 1;

        let event = match btn {
            BTN_M1 => ButtonEvent::M1(BinaryInput { pressed }),
            BTN_M2 => ButtonEvent::M2(BinaryInput { pressed }),
            BTN_KEYBOARD => ButtonEvent::Keyboard(BinaryInput { pressed }),
            BTN_GUIDE => ButtonEvent::Guide(BinaryInput { pressed }),
            0x00 => return Ok(Vec::new()),
            _ => {
                log::trace!("OXP HID: unknown button code: 0x{btn:02x}");
                return Ok(Vec::new());
            }
        };

        // Debounce
        if let Some(prev) = self.btn_state.get_mut(btn as usize) {
            if *prev == pressed {
                return Ok(Vec::new());
            }
            *prev = pressed;
        }

        Ok(vec![Event::Button(event)])
    }
}
