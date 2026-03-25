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

/// Correct stick axis overflow for OXP devices.
///
/// Some OXP devices (e.g. Apex) have analog sticks whose physical range
/// exceeds signed 16-bit, causing the raw value to wrap around at full
/// deflection. When this happens, the raw value lands on an exact i16
/// boundary (32767 or -32768) with the opposite sign of the previous frame.
///
/// Detection requires both conditions to be true simultaneously:
///   1. The raw value is at an exact i16 boundary (i16::MAX or i16::MIN)
///   2. The sign is opposite to the previous frame's value
///
/// This is safe for devices without overflow (e.g. X1 Mini) because normal
/// stick movement reaching the boundary will have the same sign as the
/// previous frame.
fn correct_axis_overflow(raw: i16, prev: &mut Option<f64>, axis_name: &str) -> i16 {
    let normalized = raw as f64 / 32768.0;

    if let Some(prev_val) = *prev {
        let at_boundary = raw == i16::MIN || raw == i16::MAX;
        let sign_flipped = (normalized > 0.0 && prev_val < 0.0)
            || (normalized < 0.0 && prev_val > 0.0);

        if at_boundary && sign_flipped {
            let corrected = if prev_val > 0.0 { 32767i16 } else { -32767i16 };
            log::info!(
                "OXP stick overflow: {axis_name} raw={raw}, prev={prev_val:.3}, \
                 correcting to {corrected}"
            );
            *prev = Some(corrected as f64 / 32768.0);
            return corrected;
        }
    }

    *prev = Some(normalized);
    raw.max(-32767)
}

pub struct Driver {
    device: HidDevice,
    intercept: bool,
    // Button state for debouncing
    btn_state: [bool; 0x25],
    initialized: bool,
    // Previous normalized axis values for overflow detection.
    // Some OXP devices (e.g. Apex) have analog sticks whose physical range
    // exceeds signed 16-bit, causing the raw value to wrap around at full
    // deflection. We detect this by checking if the normalized value jumped
    // by more than 1.5 between frames (impossible for real stick movement),
    // and clamp to the previous direction's extreme when it happens.
    prev_axes: [Option<f64>; 4], // [LX, LY, RX, RY]
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
            prev_axes: [None; 4],
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

                let lx_raw = i16::from_le_bytes([buf[17], buf[18]]);
                let ly_raw = i16::from_le_bytes([buf[19], buf[20]]);
                let rx_raw = i16::from_le_bytes([buf[21], buf[22]]);
                let ry_raw = i16::from_le_bytes([buf[23], buf[24]]);

                log::trace!(
                    "OXP stick raw: LX={lx_raw} LY={ly_raw} RX={rx_raw} RY={ry_raw}"
                );

                let lx = correct_axis_overflow(lx_raw, &mut self.prev_axes[0], "LX");
                let ly = correct_axis_overflow(ly_raw, &mut self.prev_axes[1], "LY");
                let rx = correct_axis_overflow(rx_raw, &mut self.prev_axes[2], "RX");
                let ry = correct_axis_overflow(ry_raw, &mut self.prev_axes[3], "RY");

                events.push(Event::Axis(AxisEvent::LStick(AxisInput { x: lx, y: ly })));
                events.push(Event::Axis(AxisEvent::RStick(AxisInput { x: rx, y: ry })));
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
