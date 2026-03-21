use std::{error::Error, ffi::CString};

use hidapi::HidDevice;

use super::event::{
    AxisEvent, AxisInput, BinaryInput, ButtonEvent, Event, TriggerEvent, TriggerInput,
};

pub const VID: u16 = 0x1a86;
pub const PID: u16 = 0xfe00;
pub const IID: i32 = 0x02;

const PACKET_SIZE: usize = 64;

// HID buffer read timeout
const HID_TIMEOUT: i32 = 10;

// HID command IDs
const CMD_BUTTON: u8 = 0xB2;

// Button codes (shared between non-intercept and intercept modes)
const BTN_A: u8 = 0x01;
const BTN_B: u8 = 0x02;
// OXP firmware reports X/Y swapped: physical X sends 0x04, physical Y sends 0x03
const BTN_X: u8 = 0x04;
const BTN_Y: u8 = 0x03;
const BTN_LB: u8 = 0x05;
const BTN_RB: u8 = 0x06;
// 0x07 = LT digital, 0x08 = RT digital (ignored in intercept, use analog instead)
const BTN_START: u8 = 0x09;
const BTN_SELECT: u8 = 0x0A;
const BTN_LS: u8 = 0x0B;
const BTN_RS: u8 = 0x0C;
const BTN_DPAD_UP: u8 = 0x0D;
const BTN_DPAD_DOWN: u8 = 0x0E;
const BTN_DPAD_LEFT: u8 = 0x0F;
const BTN_DPAD_RIGHT: u8 = 0x10;
const BTN_GUIDE: u8 = 0x21;
const BTN_M1: u8 = 0x22;
const BTN_M2: u8 = 0x23;
const BTN_KEYBOARD: u8 = 0x24;

// Intercept mode packet types (byte[3] when cmd[0] == 0xB2)
const PKT_BUTTON: u8 = 0x01;
const PKT_GAMEPAD_STATE: u8 = 0x02;
// const PKT_ACK: u8 = 0x03;

// Non-intercept mode: initialization commands to configure button mappings
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

const fn gen_intercept_enable() -> [u8; PACKET_SIZE] {
    gen_cmd(CMD_BUTTON, &[0x03, 0x01, 0x02])
}

const fn gen_intercept_disable() -> [u8; PACKET_SIZE] {
    gen_cmd(CMD_BUTTON, &[0x00, 0x01, 0x02])
}

const INTERCEPT_ENABLE: [u8; PACKET_SIZE] = gen_intercept_enable();
const INTERCEPT_DISABLE: [u8; PACKET_SIZE] = gen_intercept_disable();

/// Build a vibration command (0xB3) with the given strength (0-255).
fn gen_vibration_cmd(strength: u8) -> [u8; PACKET_SIZE] {
    let mode: u8 = if strength == 0 { 0x02 } else { 0x01 };
    let mut data = [0u8; 59];
    // Header + preamble
    let header: [u8; 10] = [0x02, 0x38, 0x02, 0xE3, 0x39, 0xE3, 0x39, 0xE3, 0x39, mode];
    data[..10].copy_from_slice(&header);
    // Strength for both motors
    data[10] = strength;
    data[11] = strength;
    // Continuation bytes
    data[12] = 0xE3;
    data[13] = 0x39;
    data[14] = 0xE3;
    // bytes 15..50 are zero (already initialized)
    // Trailer
    let trailer: [u8; 9] = [0x39, 0xE3, 0x39, 0xE3, 0xE3, 0x02, 0x04, 0x39, 0x39];
    data[50..59].copy_from_slice(&trailer);
    gen_cmd(0xB3, &data)
}

/// Clamp -32768 to -32767 so that normalizing by /32767 stays within [-1.0, 1.0].
fn clamp_axis_raw(raw: i16) -> i16 {
    raw.max(-32767)
}

pub struct Driver {
    device: HidDevice,
    intercept: bool,
    // Button state for debouncing
    btn_state: [bool; 0x25],
    initialized: bool,
}

impl Driver {
    pub fn new(
        path: String,
        intercept: bool,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let cs_path = CString::new(path.clone())?;
        let api = hidapi::HidApi::new()?;
        let device = api.open_path(&cs_path)?;
        let info = device.get_device_info()?;
        if info.vendor_id() != VID || info.product_id() != PID {
            return Err(format!("Device '{path}' is not an OXP HID controller").into());
        }

        Ok(Self {
            device,
            intercept,
            btn_state: [false; 0x25],
            initialized: false,
        })
    }

    /// Send initialization commands to the device
    fn initialize(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        if self.intercept {
            log::debug!("Sending OXP HID intercept enable command");
            self.device.write(&INTERCEPT_ENABLE)?;
            log::info!("OXP HID controller initialized in intercept mode");
        } else {
            log::debug!("Sending OXP HID initialization commands (non-intercept)");
            self.device.write(&INIT_CMD_1)?;
            self.device.write(&INIT_CMD_2)?;
            self.device.write(&INTERCEPT_DISABLE)?;
            log::info!("OXP HID controller initialized in non-intercept mode");
        }
        self.initialized = true;
        Ok(())
    }

    /// Send intercept disable command on shutdown to restore Xbox gamepad
    pub fn disable_intercept(&self) {
        if self.intercept {
            if let Err(e) = self.device.write(&INTERCEPT_DISABLE) {
                log::error!("Failed to send intercept disable on shutdown: {e}");
            } else {
                log::info!("OXP HID intercept disabled, Xbox gamepad restored");
            }
        }
    }

    /// Set vibration strength via vendor HID 0xB3 command.
    /// Strength is 0-255 (0 = off). Both motors are set to the same value.
    pub fn set_vibration(&self, strength: u8) -> Result<(), Box<dyn Error + Send + Sync>> {
        let cmd = gen_vibration_cmd(strength);
        self.device.write(&cmd)?;
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

        if self.intercept {
            self.poll_intercept(&buf, bytes_read)
        } else {
            self.poll_non_intercept(&buf, bytes_read)
        }
    }

    /// Parse packets in non-intercept mode (0x3F framed, only extra buttons)
    fn poll_non_intercept(
        &mut self,
        buf: &[u8; PACKET_SIZE],
        bytes_read: usize,
    ) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        if bytes_read < PACKET_SIZE {
            return Ok(Vec::new());
        }

        let cid = buf[0];
        let valid = buf[1] == 0x3F && buf[PACKET_SIZE - 2] == 0x3F;

        if !valid {
            log::trace!("OXP HID: invalid packet framing");
            return Ok(Vec::new());
        }

        if cid != CMD_BUTTON {
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

    /// Parse packets in intercept mode (no 0x3F framing, full gamepad data)
    fn poll_intercept(
        &mut self,
        buf: &[u8; PACKET_SIZE],
        bytes_read: usize,
    ) -> Result<Vec<Event>, Box<dyn Error + Send + Sync>> {
        if bytes_read < 4 {
            return Ok(Vec::new());
        }

        if buf[0] != CMD_BUTTON {
            return Ok(Vec::new());
        }

        let pkt_type = buf[3];
        let mut events = Vec::new();

        match pkt_type {
            PKT_BUTTON if bytes_read >= 13 => {
                let btn_code = buf[6];
                let pressed = buf[12] == 0x01;

                let event = match btn_code {
                    BTN_A => ButtonEvent::A(BinaryInput { pressed }),
                    BTN_B => ButtonEvent::B(BinaryInput { pressed }),
                    BTN_X => ButtonEvent::X(BinaryInput { pressed }),
                    BTN_Y => ButtonEvent::Y(BinaryInput { pressed }),
                    BTN_LB => ButtonEvent::LB(BinaryInput { pressed }),
                    BTN_RB => ButtonEvent::RB(BinaryInput { pressed }),
                    BTN_START => ButtonEvent::Start(BinaryInput { pressed }),
                    BTN_SELECT => ButtonEvent::Select(BinaryInput { pressed }),
                    BTN_LS => ButtonEvent::LSClick(BinaryInput { pressed }),
                    BTN_RS => ButtonEvent::RSClick(BinaryInput { pressed }),
                    BTN_DPAD_UP => ButtonEvent::DPadUp(BinaryInput { pressed }),
                    BTN_DPAD_DOWN => ButtonEvent::DPadDown(BinaryInput { pressed }),
                    BTN_DPAD_LEFT => ButtonEvent::DPadLeft(BinaryInput { pressed }),
                    BTN_DPAD_RIGHT => ButtonEvent::DPadRight(BinaryInput { pressed }),
                    BTN_GUIDE => ButtonEvent::Guide(BinaryInput { pressed }),
                    BTN_M1 => ButtonEvent::M1(BinaryInput { pressed }),
                    BTN_M2 => ButtonEvent::M2(BinaryInput { pressed }),
                    BTN_KEYBOARD => ButtonEvent::Keyboard(BinaryInput { pressed }),
                    // 0x07/0x08 = LT/RT digital click, ignore (use analog from state packets)
                    0x07 | 0x08 => return Ok(Vec::new()),
                    0x00 => return Ok(Vec::new()),
                    _ => {
                        log::trace!("OXP HID intercept: unknown button code: 0x{btn_code:02x}");
                        return Ok(Vec::new());
                    }
                };

                // Debounce
                if let Some(prev) = self.btn_state.get_mut(btn_code as usize) {
                    if *prev == pressed {
                        return Ok(Vec::new());
                    }
                    *prev = pressed;
                }

                events.push(Event::Button(event));
            }

            PKT_GAMEPAD_STATE if bytes_read >= 25 => {
                // Analog sticks and triggers
                let lt_raw = buf[15];
                let rt_raw = buf[16];

                // bytes[17:19] = LX (signed 16-bit LE)
                let lx = i16::from_le_bytes([buf[17], buf[18]]);
                // bytes[19:21] = LY (signed 16-bit LE, needs inversion)
                let ly = i16::from_le_bytes([buf[19], buf[20]]);
                // bytes[21:23] = RX (signed 16-bit LE)
                let rx = i16::from_le_bytes([buf[21], buf[22]]);
                // bytes[23:25] = RY (signed 16-bit LE, needs inversion)
                let ry = i16::from_le_bytes([buf[23], buf[24]]);

                events.push(Event::Axis(AxisEvent::LStick(AxisInput {
                    x: clamp_axis_raw(lx),
                    y: clamp_axis_raw(ly),
                })));
                events.push(Event::Axis(AxisEvent::RStick(AxisInput {
                    x: clamp_axis_raw(rx),
                    y: clamp_axis_raw(ry),
                })));
                events.push(Event::Trigger(TriggerEvent::LTrigger(TriggerInput {
                    value: lt_raw,
                })));
                events.push(Event::Trigger(TriggerEvent::RTrigger(TriggerInput {
                    value: rt_raw,
                })));
            }

            // type 0x03 = ACK, silently ignore; unknown types also ignored
            _ => {}
        }

        Ok(events)
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.disable_intercept();
    }
}
