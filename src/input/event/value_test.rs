use crate::config::capability_map::CapabilityConfig;
use crate::input::capability::{
    Capability, Gamepad, GamepadButton, Keyboard, Mouse, MouseButton, Touch, TouchButton, Touchpad,
};
use crate::input::event::value::InputValue;

#[test]
fn test_touchpad_button_translations() {
    let source_cap = Capability::Touchpad(Touchpad::RightPad(Touch::Button(TouchButton::Press)));
    let source_config = CapabilityConfig::default();
    let target_config = CapabilityConfig::default();

    let input_val = InputValue::Bool(true);

    let target_cap = Capability::Mouse(Mouse::Button(MouseButton::Left));
    let res = input_val
        .translate(&source_cap, &source_config, &target_cap, &target_config)
        .expect("should translate touchpad button to mouse button");
    assert!(res.pressed());

    let target_cap = Capability::Gamepad(Gamepad::Button(GamepadButton::South));
    let res = input_val
        .translate(&source_cap, &source_config, &target_cap, &target_config)
        .expect("should translate touchpad button to gamepad button");
    assert!(res.pressed());

    let target_cap = Capability::Keyboard(Keyboard::KeyA);
    let res = input_val
        .translate(&source_cap, &source_config, &target_cap, &target_config)
        .expect("should translate touchpad button to keyboard key");
    assert!(res.pressed());

    let target_cap = Capability::Touchpad(Touchpad::CenterPad(Touch::Button(TouchButton::Touch)));
    let res = input_val
        .translate(&source_cap, &source_config, &target_cap, &target_config)
        .expect("should translate touchpad button to touchpad button");
    assert!(res.pressed());

    let source_cap = Capability::Touchscreen(Touch::Button(TouchButton::Press));
    let target_cap = Capability::Mouse(Mouse::Button(MouseButton::Right));
    let res = input_val
        .translate(&source_cap, &source_config, &target_cap, &target_config)
        .expect("should translate touchscreen button to mouse button");
    assert!(res.pressed());
}

#[test]
fn touchscreen_gesture_button_translations_preserve_press_and_release() {
    use crate::input::capability::GestureType;
    let source = Capability::Touchscreen(Touch::Gesture(GestureType::Up));
    let config = CapabilityConfig::default();
    for target in [
        Capability::Gamepad(Gamepad::Button(GamepadButton::Guide)),
        Capability::Keyboard(Keyboard::KeyA),
        Capability::Mouse(Mouse::Button(MouseButton::Left)),
    ] {
        for pressed in [true, false] {
            let value = InputValue::Bool(pressed)
                .translate(&source, &config, &target, &config)
                .unwrap();
            assert_eq!(value.pressed(), pressed);
        }
    }
}

#[test]
fn ordinary_touch_buttons_keep_translation_without_synthesizing_gestures() {
    use crate::input::{capability::GestureType, event::value::TranslationError};
    let config = CapabilityConfig::default();
    for source in [
        Capability::Touchpad(Touchpad::LeftPad(Touch::Button(TouchButton::Press))),
        Capability::Touchpad(Touchpad::RightPad(Touch::Button(TouchButton::Press))),
        Capability::Touchpad(Touchpad::CenterPad(Touch::Button(TouchButton::Press))),
        Capability::Touchscreen(Touch::Button(TouchButton::Press)),
    ] {
        for touch in [
            Touch::Button(TouchButton::Touch),
            Touch::Gesture(GestureType::Up),
        ] {
            for target in [
                Capability::Touchpad(Touchpad::LeftPad(touch.clone())),
                Capability::Touchpad(Touchpad::RightPad(touch.clone())),
                Capability::Touchpad(Touchpad::CenterPad(touch.clone())),
                Capability::Touchscreen(touch.clone()),
            ] {
                for pressed in [true, false] {
                    let result =
                        InputValue::Bool(pressed).translate(&source, &config, &target, &config);
                    if matches!(touch, Touch::Button(_)) {
                        assert_eq!(result.unwrap().pressed(), pressed);
                    } else {
                        assert!(matches!(result, Err(TranslationError::NotImplemented)));
                    }
                }
            }
        }
    }
}
