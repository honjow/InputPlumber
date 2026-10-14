//! Timing for profile-mapped touchscreen gestures only. Physical input and the
//! target implementations' own macro/chord schedules keep their original timing.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use crate::input::{
    capability::{Capability, Gamepad, Mouse, Touch},
    event::native::NativeEvent,
};

/// Give consumers sampling HID state enough time to observe a synthetic press.
const PRESS_TIME: Duration = Duration::from_millis(80);
/// Repeated gestures need an observable release as well as an observable press.
const RELEASE_TIME: Duration = Duration::from_millis(80);

#[derive(Debug, Default)]
struct Pulse {
    pending: VecDeque<NativeEvent>,
    ready_at: Option<Instant>,
    pressed: bool,
}

impl Pulse {
    fn next(&mut self, now: Instant) -> Option<NativeEvent> {
        if self.ready_at.is_some_and(|ready_at| now < ready_at) {
            return None;
        }
        let event = self.pending.pop_front()?;
        self.pressed = event.pressed();
        // Start each interval when the transition is emitted, not when it was
        // queued: a busy target must not collapse an entire pulse into one poll.
        self.ready_at = Some(
            now + if self.pressed {
                PRESS_TIME
            } else {
                RELEASE_TIME
            },
        );
        Some(event)
    }
}

#[derive(Debug, Default)]
pub(super) struct GesturePulses {
    pulses: HashMap<Capability, Pulse>,
    physical_pressed: HashSet<Capability>,
}

impl GesturePulses {
    /// Accept a command event and return an event to emit immediately, if any.
    pub fn submit(&mut self, event: NativeEvent, now: Instant) -> Option<NativeEvent> {
        let cap = event.as_capability();
        if !matches!(
            cap,
            Capability::Gamepad(Gamepad::Button(_))
                | Capability::Keyboard(_)
                | Capability::Mouse(Mouse::Button(_))
        ) {
            return Some(event);
        }

        let is_gesture = matches!(
            event.get_source_capability(),
            Some(Capability::Touchscreen(Touch::Gesture(_)))
        );
        if !is_gesture {
            // A normal input takes ownership immediately. Discard the complete
            // old pulse, including its queued up, so it cannot release a newer
            // physical press. Never delay normal physical button transitions.
            self.pulses.remove(&cap);
            if event.pressed() {
                self.physical_pressed.insert(cap);
            } else {
                self.physical_pressed.remove(&cap);
            }
            return Some(event);
        }
        if self.physical_pressed.contains(&cap) {
            return None;
        }

        let pulse = self.pulses.entry(cap).or_default();
        pulse.pending.push_back(event);
        pulse.next(now)
    }

    pub fn poll(&mut self, now: Instant) -> Vec<NativeEvent> {
        let mut events = Vec::new();
        self.pulses.retain(|_, pulse| {
            if let Some(event) = pulse.next(now) {
                events.push(event);
            }
            pulse.pressed
                || !pulse.pending.is_empty()
                || pulse.ready_at.is_some_and(|ready_at| now < ready_at)
        });
        events
    }

    pub fn clear(&mut self) {
        self.pulses.clear();
        self.physical_pressed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{
        capability::{GamepadButton, GestureType},
        event::value::InputValue,
    };

    fn button(pressed: bool) -> NativeEvent {
        NativeEvent::new(
            Capability::Gamepad(Gamepad::Button(GamepadButton::South)),
            InputValue::Bool(pressed),
        )
    }

    fn gesture(pressed: bool) -> NativeEvent {
        let mut event = button(pressed);
        event.set_source_capability(Capability::Touchscreen(Touch::Gesture(GestureType::Up)));
        event
    }

    #[test]
    fn gesture_press_and_release_are_observable() {
        let mut pulses = GesturePulses::default();
        let now = Instant::now();
        assert!(pulses.submit(gesture(true), now).unwrap().pressed());
        assert!(pulses.submit(gesture(false), now).is_none());
        assert!(pulses.poll(now + PRESS_TIME / 2).is_empty());
        let events = pulses.poll(now + PRESS_TIME);
        assert_eq!(events.len(), 1);
        assert!(!events[0].pressed());
    }

    #[test]
    fn rapid_repeated_gestures_keep_distinct_pulses() {
        let mut pulses = GesturePulses::default();
        let now = Instant::now();
        assert!(pulses.submit(gesture(true), now).unwrap().pressed());
        for pressed in [false, true, false] {
            assert!(pulses.submit(gesture(pressed), now).is_none());
        }
        for (offset, pressed) in [
            (PRESS_TIME, false),
            (PRESS_TIME + RELEASE_TIME, true),
            (PRESS_TIME * 2 + RELEASE_TIME, false),
        ] {
            let events = pulses.poll(now + offset);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].pressed(), pressed);
            assert!(pulses.poll(now + offset).is_empty());
        }
    }

    #[test]
    fn late_poll_does_not_collapse_repeated_pulses() {
        let mut pulses = GesturePulses::default();
        let now = Instant::now();
        pulses.submit(gesture(true), now);
        for pressed in [false, true, false] {
            pulses.submit(gesture(pressed), now);
        }
        let late = now + Duration::from_secs(1);
        let events = pulses.poll(late);
        assert_eq!(events.len(), 1);
        assert!(!events[0].pressed());
        assert!(pulses.poll(late).is_empty());
        assert!(pulses.poll(late + RELEASE_TIME)[0].pressed());
    }

    #[test]
    fn physical_input_is_immediate_and_cancels_stale_gesture_release() {
        let mut pulses = GesturePulses::default();
        let now = Instant::now();
        pulses.submit(gesture(true), now);
        pulses.submit(gesture(false), now);
        assert!(pulses.submit(button(true), now).unwrap().pressed());
        assert!(pulses.poll(now + PRESS_TIME).is_empty());
        assert!(pulses.submit(gesture(true), now).is_none());
        assert!(pulses.submit(gesture(false), now).is_none());
        assert!(!pulses.submit(button(false), now).unwrap().pressed());
        assert!(pulses.poll(now + PRESS_TIME * 2).is_empty());
    }

    #[test]
    fn clear_state_discards_pending_pulses() {
        let mut pulses = GesturePulses::default();
        let now = Instant::now();
        for pressed in [true, false, true, false] {
            pulses.submit(gesture(pressed), now);
        }
        pulses.clear();
        assert!(pulses.poll(now + Duration::from_secs(1)).is_empty());
        assert!(pulses.submit(gesture(true), now).unwrap().pressed());
    }

    #[test]
    fn ordinary_button_pairs_are_not_debounced() {
        let mut pulses = GesturePulses::default();
        let now = Instant::now();
        for pressed in [true, false, true, false] {
            assert_eq!(
                pulses.submit(button(pressed), now).unwrap().pressed(),
                pressed
            );
        }
        assert!(pulses.poll(now + PRESS_TIME).is_empty());
    }
}
