use std::collections::HashMap;

use crate::input::capability::Capability;

/// Tokens remain unique across release/repress cycles, so an old timer cannot
/// release a newly pressed key. Only explicitly opted-in capabilities use this.
#[derive(Debug, Default)]
pub(super) struct TranslatableWatchdog {
    generation: u64,
    active: HashMap<Capability, u64>,
}

impl TranslatableWatchdog {
    pub fn refresh(&mut self, cap: Capability) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.active.insert(cap, self.generation);
        self.generation
    }

    pub fn cancel(&mut self, cap: &Capability) {
        self.active.remove(cap);
    }

    pub fn is_current(&self, cap: &Capability, generation: u64) -> bool {
        self.active.get(cap).copied() == Some(generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::capability::Keyboard;

    #[test]
    fn old_timeout_cannot_release_repressed_key() {
        let cap = Capability::Keyboard(Keyboard::KeyDelete);
        let mut watchdog = TranslatableWatchdog::default();
        let old = watchdog.refresh(cap.clone());
        watchdog.cancel(&cap);
        assert!(!watchdog.is_current(&cap, old));
        let new = watchdog.refresh(cap.clone());
        assert!(!watchdog.is_current(&cap, old));
        assert!(watchdog.is_current(&cap, new));
    }

    #[test]
    fn repeats_refresh_only_their_own_timeout() {
        let delete = Capability::Keyboard(Keyboard::KeyDelete);
        let tab = Capability::Keyboard(Keyboard::KeyTab);
        let mut watchdog = TranslatableWatchdog::default();
        let first = watchdog.refresh(delete.clone());
        let other = watchdog.refresh(tab.clone());
        let repeat = watchdog.refresh(delete.clone());
        assert!(!watchdog.is_current(&delete, first));
        assert!(watchdog.is_current(&delete, repeat));
        assert!(watchdog.is_current(&tab, other));
        watchdog.cancel(&delete);
        assert!(!watchdog.is_current(&delete, repeat));
    }
}
