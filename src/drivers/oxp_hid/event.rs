/// Events that can be emitted by the OXP HID driver
#[derive(Clone, Debug)]
pub enum Event {
    Button(ButtonEvent),
    Axis(AxisEvent),
    Trigger(TriggerEvent),
}

/// Binary input contains either pressed or unpressed
#[derive(Clone, Debug)]
pub struct BinaryInput {
    pub pressed: bool,
}

/// Axis input contains signed 16-bit (x, y) values for joysticks
#[derive(Clone, Debug)]
pub struct AxisInput {
    pub x: i16,
    pub y: i16,
}

/// Trigger input contains an unsigned 8-bit value (0-255)
#[derive(Clone, Debug)]
pub struct TriggerInput {
    pub value: u8,
}

/// GamepadButton events represent binary button presses.
/// In non-intercept mode, only M1/M2/Keyboard/Guide are reported.
/// In intercept mode, all gamepad buttons come through vendor HID.
#[derive(Clone, Debug)]
pub enum ButtonEvent {
    /// A / South
    A(BinaryInput),
    /// B / East
    B(BinaryInput),
    /// X / West
    X(BinaryInput),
    /// Y / North
    Y(BinaryInput),
    /// Left bumper
    LB(BinaryInput),
    /// Right bumper
    RB(BinaryInput),
    /// Start / Menu
    Start(BinaryInput),
    /// Select / View
    Select(BinaryInput),
    /// Left stick click
    LSClick(BinaryInput),
    /// Right stick click
    RSClick(BinaryInput),
    /// D-pad up
    DPadUp(BinaryInput),
    /// D-pad down
    DPadDown(BinaryInput),
    /// D-pad left
    DPadLeft(BinaryInput),
    /// D-pad right
    DPadRight(BinaryInput),
    /// M1 back paddle (left on X1 Mini, right on Apex in intercept mode)
    M1(BinaryInput),
    /// M2 back paddle (right on X1 Mini, left on Apex in intercept mode)
    M2(BinaryInput),
    /// Keyboard button
    Keyboard(BinaryInput),
    /// Guide/Home button
    Guide(BinaryInput),
}

/// Axis events for joystick analog input (intercept mode only)
#[derive(Clone, Debug)]
pub enum AxisEvent {
    LStick(AxisInput),
    RStick(AxisInput),
}

/// Trigger events for analog trigger input (intercept mode only)
#[derive(Clone, Debug)]
pub enum TriggerEvent {
    LTrigger(TriggerInput),
    RTrigger(TriggerInput),
}
