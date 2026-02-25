/// Events that can be emitted by the OXP HID driver
#[derive(Clone, Debug)]
pub enum Event {
    GamepadButton(GamepadButtonEvent),
}

/// Binary input contain either pressed or unpressed
#[derive(Clone, Debug)]
pub struct BinaryInput {
    pub pressed: bool,
}

/// GamepadButton events represent binary button presses
#[derive(Clone, Debug)]
pub enum GamepadButtonEvent {
    /// M1 back button on the left
    M1(BinaryInput),
    /// M2 back button on the right
    M2(BinaryInput),
    /// Keyboard button
    Keyboard(BinaryInput),
    /// Guide/Home button
    Guide(BinaryInput),
}
