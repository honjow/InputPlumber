use crate::{
    config::capability_map::CapabilityMapConfigV2,
    input::{
        capability::Capability,
        event::{native::NativeEvent, value::InputValue},
    },
};

#[derive(Debug, Clone)]
struct HidrawButtonMapping {
    report_id: Option<u8>,
    byte_index: usize,
    detection: DetectionMode,
    capability: Capability,
}

#[derive(Debug, Clone)]
enum DetectionMode {
    NonZero,
    Value(u8),
    /// Bit position (LSB=0)
    Bit(u8),
}

/// Translates raw HID reports into [NativeEvent]s using a capability map.
#[derive(Debug)]
pub struct HidrawEventTranslator {
    source_events: Vec<HidrawButtonMapping>,
    state: Vec<bool>,
}

impl HidrawEventTranslator {
    /// Create a new translator from a V2 capability map.
    pub fn new(capability_map: &CapabilityMapConfigV2) -> Self {
        let mut source_events = Vec::new();

        for mapping in capability_map.mapping.iter() {
            for source in mapping.source_events.iter() {
                let Some(hidraw) = source.hidraw.as_ref() else {
                    continue;
                };

                if hidraw.input_type != "button" {
                    log::warn!(
                        "Unsupported hidraw input_type '{}' in mapping '{}', skipping",
                        hidraw.input_type,
                        mapping.name,
                    );
                    continue;
                }

                let cap: Capability = mapping.target_event.clone().into();
                if cap == Capability::NotImplemented {
                    log::warn!(
                        "Unresolved target capability in mapping '{}', skipping",
                        mapping.name,
                    );
                    continue;
                }

                if hidraw.bit_offset.is_some_and(|bit| bit > 7) {
                    log::warn!(
                        "Invalid HID bit offset in mapping '{}', skipping",
                        mapping.name
                    );
                    continue;
                }

                let detection = if let Some(value) = hidraw.value {
                    DetectionMode::Value(value)
                } else if let Some(bit) = hidraw.bit_offset {
                    DetectionMode::Bit(bit)
                } else {
                    DetectionMode::NonZero
                };

                source_events.push(HidrawButtonMapping {
                    report_id: hidraw.report_id,
                    byte_index: hidraw.byte_start as usize,
                    detection,
                    capability: cap,
                });
            }
        }

        let state = vec![false; source_events.len()];
        Self {
            source_events,
            state,
        }
    }

    pub fn has_hid_translation(&self) -> bool {
        !self.source_events.is_empty()
    }

    pub fn capabilities(&self) -> Vec<Capability> {
        self.source_events
            .iter()
            .map(|m| m.capability.clone())
            .collect()
    }

    /// Translate a raw HID report into [NativeEvent]s. Only emits events on
    /// state changes.
    pub fn translate(&mut self, report: &[u8]) -> Vec<NativeEvent> {
        let mut events = Vec::new();

        for (idx, mapping) in self.source_events.iter().enumerate() {
            if let Some(expected_id) = mapping.report_id {
                if report.first().copied() != Some(expected_id) {
                    continue;
                }
            }

            if mapping.byte_index >= report.len() {
                log::warn!(
                    "HID report too short for mapping at byte {}: got {} bytes",
                    mapping.byte_index,
                    report.len(),
                );
                continue;
            }

            let byte_val = report[mapping.byte_index];
            let pressed = match mapping.detection {
                DetectionMode::NonZero => byte_val != 0,
                DetectionMode::Value(expected) => byte_val == expected,
                DetectionMode::Bit(bit) => (byte_val & (1 << bit)) != 0,
            };

            if pressed != self.state[idx] {
                self.state[idx] = pressed;
                events.push(NativeEvent::new(
                    mapping.capability.clone(),
                    InputValue::Bool(pressed),
                ));
            }
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::capability::{Gamepad, GamepadButton};

    fn map(yaml: &str) -> HidrawEventTranslator {
        HidrawEventTranslator::new(&serde_yaml::from_str(yaml).unwrap())
    }

    #[test]
    fn win5_raw_report_replay_press_repeat_release() {
        let mut translator = map(include_str!(
            "../../../../rootfs/usr/share/inputplumber/capability_maps/gpd_v2_hid1.yaml"
        ));
        let idle = [1, 0xa5, 0, 0x5a, 0xff, 0, 1, 9, 0, 0, 0, 0];
        assert!(translator.translate(&idle).is_empty());
        let mut pressed = idle;
        pressed[8..11].copy_from_slice(&[0x68, 0x69, 0x6a]);
        let events = translator.translate(&pressed);
        assert_eq!(events.len(), 3);
        for (event, button) in events.iter().zip([
            GamepadButton::QuickAccess,
            GamepadButton::LeftPaddle1,
            GamepadButton::RightPaddle1,
        ]) {
            assert_eq!(
                event.as_capability(),
                Capability::Gamepad(Gamepad::Button(button))
            );
            assert!(event.pressed());
        }
        assert!(translator.translate(&pressed).is_empty());
        let releases = translator.translate(&idle);
        assert_eq!(releases.len(), 3);
        assert!(releases.iter().all(|event| !event.pressed()));
        assert!(translator.translate(&idle).is_empty());
    }

    #[test]
    fn first_pressed_report_and_short_packets() {
        let mut translator = map(include_str!(
            "../../../../rootfs/usr/share/inputplumber/capability_maps/tf_hid1.yaml"
        ));
        assert!(translator.translate(&[]).is_empty());
        assert!(translator.translate(&[0; 9]).is_empty());
        let mut report = [0; 10];
        report[9] = 1;
        assert!(translator.translate(&report)[0].pressed());
        assert!(translator.translate(&[0; 9]).is_empty());
        report[9] = 0;
        assert!(!translator.translate(&report)[0].pressed());
    }

    #[test]
    fn report_id_bit_and_value_filters() {
        let mut translator = map("version: 2\nkind: CapabilityMap\nname: test\nid: test\nmapping:\n  - name: bit\n    source_events:\n      - hidraw: {input_type: button, report_id: 7, byte_start: 1, bit_offset: 7}\n    target_event: {gamepad: {button: QuickAccess}}\n  - name: value\n    source_events:\n      - hidraw: {input_type: button, report_id: 7, byte_start: 2, value: 105}\n    target_event: {gamepad: {button: LeftPaddle1}}\n");
        assert!(translator.translate(&[6, 0x80, 105]).is_empty());
        assert_eq!(translator.translate(&[7, 0x80, 105]).len(), 2);
        assert!(translator.translate(&[6, 0, 0]).is_empty());
        let released = translator.translate(&[7, 0x01, 104]);
        assert_eq!(released.len(), 2);
        assert!(released.iter().all(|event| !event.pressed()));
    }

    #[test]
    fn invalid_bit_offset_is_rejected_without_panicking() {
        let mut translator = map("version: 2\nkind: CapabilityMap\nname: invalid\nid: invalid\nmapping:\n  - name: invalid\n    source_events:\n      - hidraw: {input_type: button, byte_start: 0, bit_offset: 8}\n    target_event: {gamepad: {button: QuickAccess}}\n");
        assert!(!translator.has_hid_translation());
        assert!(translator.translate(&[0xff]).is_empty());
    }
}
