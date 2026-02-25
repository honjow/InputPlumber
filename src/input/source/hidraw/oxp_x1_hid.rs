use std::{error::Error, fmt::Debug};

use crate::{
    drivers::oxp_hid::{driver::Driver, event},
    input::{
        capability::{Capability, Gamepad, GamepadButton},
        event::{native::NativeEvent, value::InputValue},
        source::{InputError, SourceInputDevice, SourceOutputDevice},
    },
    udev::device::UdevDevice,
};

/// OXP X1 HID source device implementation for back buttons
pub struct OxpX1Hid {
    driver: Driver,
}

impl OxpX1Hid {
    pub fn new(device_info: UdevDevice) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let driver = Driver::new(device_info)?;
        Ok(Self { driver })
    }
}

impl SourceInputDevice for OxpX1Hid {
    fn poll(&mut self) -> Result<Vec<NativeEvent>, InputError> {
        let events = self.driver.poll()?;
        let native_events = translate_events(events);
        Ok(native_events)
    }

    fn get_capabilities(&self) -> Result<Vec<Capability>, InputError> {
        Ok(CAPABILITIES.into())
    }
}

impl SourceOutputDevice for OxpX1Hid {}

impl Debug for OxpX1Hid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OxpX1Hid").finish()
    }
}

fn translate_events(events: Vec<event::Event>) -> Vec<NativeEvent> {
    events.into_iter().map(translate_event).collect()
}

fn translate_event(event: event::Event) -> NativeEvent {
    match event {
        event::Event::GamepadButton(button) => match button {
            event::GamepadButtonEvent::M1(value) => NativeEvent::new(
                Capability::Gamepad(Gamepad::Button(GamepadButton::LeftPaddle1)),
                InputValue::Bool(value.pressed),
            ),
            event::GamepadButtonEvent::M2(value) => NativeEvent::new(
                Capability::Gamepad(Gamepad::Button(GamepadButton::RightPaddle1)),
                InputValue::Bool(value.pressed),
            ),
            event::GamepadButtonEvent::Keyboard(value) => NativeEvent::new(
                Capability::Gamepad(Gamepad::Button(GamepadButton::Keyboard)),
                InputValue::Bool(value.pressed),
            ),
            event::GamepadButtonEvent::Guide(value) => NativeEvent::new(
                Capability::Gamepad(Gamepad::Button(GamepadButton::Guide)),
                InputValue::Bool(value.pressed),
            ),
        },
    }
}

/// List of all input capabilities that the OXP X1 HID driver implements
pub const CAPABILITIES: &[Capability] = &[
    Capability::Gamepad(Gamepad::Button(GamepadButton::LeftPaddle1)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::RightPaddle1)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::Keyboard)),
    Capability::Gamepad(Gamepad::Button(GamepadButton::Guide)),
];
