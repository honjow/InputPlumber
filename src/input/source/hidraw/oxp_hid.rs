use std::{collections::HashMap, error::Error, fmt::Debug};

use evdev::{FFEffectData, FFEffectKind};
use packed_struct::types::SizedInteger;

use crate::{
    drivers::oxp_hid::{driver::Driver, event},
    input::{
        capability::{Capability, Gamepad, GamepadAxis, GamepadButton, GamepadTrigger},
        event::{
            native::NativeEvent,
            value::InputValue,
            value::{normalize_signed_value, normalize_unsigned_value},
        },
        output_capability::OutputCapability,
        output_event::OutputEvent,
        source::{InputError, OutputError, SourceInputDevice, SourceOutputDevice},
    },
    udev::device::UdevDevice,
};

const AXIS_MIN: f64 = -32767.0;
const AXIS_MAX: f64 = 32767.0;
const TRIGGER_MAX: f64 = 255.0;
const MAX_FF_EFFECTS: i16 = 16;

/// OXP HID source device implementation.
/// Supports both non-intercept mode (extra buttons only) and full intercept
/// mode (all gamepad input through vendor HID).
/// Vibration output is handled via vendor HID 0xB3 command in both modes.
pub struct OxpHid {
    driver: Driver,
    intercept: bool,
    vendor_rumble: bool,
    ff_evdev_effects: HashMap<i16, FFEffectData>,
}

impl OxpHid {
    pub fn new(
        device_info: UdevDevice,
        intercept: bool,
        vendor_rumble: bool,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let driver = Driver::new(device_info.devnode(), intercept)?;
        Ok(Self {
            driver,
            intercept,
            vendor_rumble,
            ff_evdev_effects: HashMap::new(),
        })
    }

    fn next_ff_effect_id(&self) -> i16 {
        for i in 0..MAX_FF_EFFECTS {
            if !self.ff_evdev_effects.contains_key(&i) {
                return i;
            }
        }
        -1
    }

    /// Convert left/right motor speeds (0-255 each) to a single 0xB3 strength
    fn set_rumble(&mut self, left: u8, right: u8) -> Result<(), Box<dyn Error + Send + Sync>> {
        let strength = left.max(right);
        self.driver.set_vibration(strength)
    }

    fn process_evdev_ff(
        &mut self,
        input_event: evdev::InputEvent,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let (code, value) =
            if let evdev::EventSummary::ForceFeedback(_, code, value) = input_event.destructure() {
                (code, value)
            } else {
                return Ok(());
            };

        let effect_id = code.0 as i16;
        let Some(effect_data) = self.ff_evdev_effects.get(&effect_id) else {
            log::warn!("OXP HID: no FF effect id found: {}", code.0);
            return Ok(());
        };

        if value == 0 {
            self.set_rumble(0, 0)?;
            return Ok(());
        }

        if let FFEffectKind::Rumble {
            strong_magnitude,
            weak_magnitude,
        } = effect_data.kind
        {
            let left = (strong_magnitude / 256) as u8;
            let right = (weak_magnitude / 256) as u8;
            self.set_rumble(left, right)?;
        }

        Ok(())
    }
}

impl SourceInputDevice for OxpHid {
    /// Poll the device for input events
    fn poll(&mut self) -> Result<Vec<NativeEvent>, InputError> {
        let events = self.driver.poll()?;
        let native_events = translate_events(events);
        Ok(native_events)
    }

    /// Returns the input capabilities of this device
    fn get_capabilities(&self) -> Result<Vec<Capability>, InputError> {
        if self.intercept {
            Ok(CAPABILITIES_INTERCEPT.into())
        } else {
            Ok(CAPABILITIES_NON_INTERCEPT.into())
        }
    }
}

impl SourceOutputDevice for OxpHid {
    /// Returns the output capabilities of this device
    fn get_output_capabilities(&self) -> Result<Vec<OutputCapability>, OutputError> {
        if self.vendor_rumble {
            Ok(vec![OutputCapability::ForceFeedback])
        } else {
            Ok(vec![])
        }
    }

    /// Write the given output event to the source device. Output events are
    /// events that flow from an application (like a game) to the physical
    /// input device, such as force feedback events.
    fn write_event(&mut self, event: OutputEvent) -> Result<(), OutputError> {
        if !self.vendor_rumble {
            return Ok(());
        }
        match event {
            OutputEvent::Evdev(input_event) => {
                self.process_evdev_ff(input_event)?;
            }
            OutputEvent::DualSense(report) => {
                if report.use_rumble_not_haptics || report.enable_improved_rumble_emulation {
                    self.set_rumble(
                        report.rumble_emulation_left,
                        report.rumble_emulation_right,
                    )?;
                }
            }
            OutputEvent::SteamDeckRumble(report) => {
                let left = (report.left_speed.to_primitive() / 256) as u8;
                let right = (report.right_speed.to_primitive() / 256) as u8;
                self.set_rumble(left, right)?;
            }
            OutputEvent::Rumble {
                weak_magnitude,
                strong_magnitude,
            } => {
                let left = (strong_magnitude / 256) as u8;
                let right = (weak_magnitude / 256) as u8;
                self.set_rumble(left, right)?;
            }
            OutputEvent::Uinput(_) | OutputEvent::SteamDeckHaptics(_) => (),
        }
        Ok(())
    }

    /// Upload the given force feedback effect data to the source device.
    /// Returns a device-specific id of the uploaded effect if it is successful.
    fn upload_effect(&mut self, effect: FFEffectData) -> Result<i16, OutputError> {
        let id = self.next_ff_effect_id();
        if id == -1 {
            return Err("Maximum FF effects uploaded".into());
        }
        self.ff_evdev_effects.insert(id, effect);
        Ok(id)
    }

    /// Update an existing force feedback effect with new data.
    fn update_effect(&mut self, effect_id: i16, effect: FFEffectData) -> Result<(), OutputError> {
        if self.ff_evdev_effects.contains_key(&effect_id) {
            self.ff_evdev_effects.insert(effect_id, effect);
        }
        Ok(())
    }

    /// Erase the force feedback effect with the given id.
    fn erase_effect(&mut self, effect_id: i16) -> Result<(), OutputError> {
        self.ff_evdev_effects.remove(&effect_id);
        Ok(())
    }
}

impl Debug for OxpHid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OxpHid")
            .field("intercept", &self.intercept)
            .field("vendor_rumble", &self.vendor_rumble)
            .finish()
    }
}

fn translate_events(events: Vec<event::Event>) -> Vec<NativeEvent> {
    events.into_iter().map(translate_event).collect()
}

fn translate_event(event: event::Event) -> NativeEvent {
    match event {
        event::Event::Button(button) => translate_button(button),
        event::Event::Axis(axis) => translate_axis(axis),
        event::Event::Trigger(trigger) => translate_trigger(trigger),
    }
}

fn translate_button(button: event::ButtonEvent) -> NativeEvent {
    let (capability, pressed) = match button {
        event::ButtonEvent::A(v) => (GamepadButton::South, v.pressed),
        event::ButtonEvent::B(v) => (GamepadButton::East, v.pressed),
        event::ButtonEvent::X(v) => (GamepadButton::West, v.pressed),
        event::ButtonEvent::Y(v) => (GamepadButton::North, v.pressed),
        event::ButtonEvent::LB(v) => (GamepadButton::LeftBumper, v.pressed),
        event::ButtonEvent::RB(v) => (GamepadButton::RightBumper, v.pressed),
        event::ButtonEvent::Start(v) => (GamepadButton::Start, v.pressed),
        event::ButtonEvent::Select(v) => (GamepadButton::Select, v.pressed),
        event::ButtonEvent::LSClick(v) => (GamepadButton::LeftStick, v.pressed),
        event::ButtonEvent::RSClick(v) => (GamepadButton::RightStick, v.pressed),
        event::ButtonEvent::DPadUp(v) => (GamepadButton::DPadUp, v.pressed),
        event::ButtonEvent::DPadDown(v) => (GamepadButton::DPadDown, v.pressed),
        event::ButtonEvent::DPadLeft(v) => (GamepadButton::DPadLeft, v.pressed),
        event::ButtonEvent::DPadRight(v) => (GamepadButton::DPadRight, v.pressed),
        event::ButtonEvent::M1(v) => (GamepadButton::LeftPaddle1, v.pressed),
        event::ButtonEvent::M2(v) => (GamepadButton::RightPaddle1, v.pressed),
        event::ButtonEvent::Keyboard(v) => (GamepadButton::Keyboard, v.pressed),
        event::ButtonEvent::Guide(v) => (GamepadButton::Guide, v.pressed),
    };
    NativeEvent::new(
        Capability::Gamepad(Gamepad::Button(capability)),
        InputValue::Bool(pressed),
    )
}

fn translate_axis(axis: event::AxisEvent) -> NativeEvent {
    match axis {
        event::AxisEvent::LStick(value) => {
            let x = normalize_signed_value(value.x as f64, AXIS_MIN, AXIS_MAX);
            let y = -normalize_signed_value(value.y as f64, AXIS_MIN, AXIS_MAX);
            NativeEvent::new(
                Capability::Gamepad(Gamepad::Axis(GamepadAxis::LeftStick)),
                InputValue::Vector2 {
                    x: Some(x),
                    y: Some(y),
                },
            )
        }
        event::AxisEvent::RStick(value) => {
            let x = normalize_signed_value(value.x as f64, AXIS_MIN, AXIS_MAX);
            let y = -normalize_signed_value(value.y as f64, AXIS_MIN, AXIS_MAX);
            NativeEvent::new(
                Capability::Gamepad(Gamepad::Axis(GamepadAxis::RightStick)),
                InputValue::Vector2 {
                    x: Some(x),
                    y: Some(y),
                },
            )
        }
    }
}

fn translate_trigger(trigger: event::TriggerEvent) -> NativeEvent {
    match trigger {
        event::TriggerEvent::LTrigger(value) => NativeEvent::new(
            Capability::Gamepad(Gamepad::Trigger(GamepadTrigger::LeftTrigger)),
            InputValue::Float(normalize_unsigned_value(value.value as f64, TRIGGER_MAX)),
        ),
        event::TriggerEvent::RTrigger(value) => NativeEvent::new(
            Capability::Gamepad(Gamepad::Trigger(GamepadTrigger::RightTrigger)),
            InputValue::Float(normalize_unsigned_value(value.value as f64, TRIGGER_MAX)),
        ),
    }
}

/// Capabilities in non-intercept mode (extra buttons only)
pub const CAPABILITIES_NON_INTERCEPT: &[Capability] = &[
    Capability::Gamepad(Gamepad::Button(GamepadButton::LeftPaddle1)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::RightPaddle1)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::Keyboard)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::Guide)),
];

/// Capabilities in intercept mode (full gamepad)
pub const CAPABILITIES_INTERCEPT: &[Capability] = &[
    Capability::Gamepad(Gamepad::Button(GamepadButton::South)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::East)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::West)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::North)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::LeftBumper)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::RightBumper)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::Start)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::Select)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::LeftStick)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::RightStick)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::DPadUp)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::DPadDown)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::DPadLeft)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::DPadRight)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::LeftPaddle1)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::RightPaddle1)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::Keyboard)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::Guide)),
    Capability::Gamepad(Gamepad::Axis(GamepadAxis::LeftStick)),
    Capability::Gamepad(Gamepad::Axis(GamepadAxis::RightStick)),
    Capability::Gamepad(Gamepad::Trigger(GamepadTrigger::LeftTrigger)),
    Capability::Gamepad(Gamepad::Trigger(GamepadTrigger::RightTrigger)),
];
