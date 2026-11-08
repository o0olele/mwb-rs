//! Physical modifier state and permission for an individual captured movement.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum EdgeSwitchCondition {
    #[default]
    Always = 0,
    Ctrl = 1,
    Shift = 2,
}

const CTRL: u64 = 0b000011;
const ALT: u64 = 0b001100;
const SHIFT: u64 = 0b110000;
const CONDITION: u64 = 0b11000000;
const PAYLOAD: u64 = 0xff;

fn modifier_bit(vk: u16) -> u64 {
    match vk {
        0x11 | 0xA2 => 1,
        0xA3 => 2,
        0x12 | 0xA4 => 4,
        0xA5 => 8,
        0x10 | 0xA0 => 16,
        0xA1 => 32,
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EdgePermit(u64);

pub struct SwitchGate {
    // One atomic snapshot includes both settings and physical keys. A generation
    // invalidates queued moves and relative cursor feedback after release/repress.
    state: AtomicU64,
}

impl SwitchGate {
    pub fn new(condition: EdgeSwitchCondition) -> Self {
        Self {
            state: AtomicU64::new((condition as u64) << 6),
        }
    }

    fn update(&self, change: impl Fn(u64) -> u64) {
        let _ = self
            .state
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                let payload = change(old & PAYLOAD);
                (payload != (old & PAYLOAD))
                    .then(|| (old & !PAYLOAD).wrapping_add(PAYLOAD + 1) | payload)
            });
    }

    pub fn set_condition(&self, condition: EdgeSwitchCondition) {
        self.update(|state| (state & !CONDITION) | ((condition as u64) << 6));
    }

    /// Only physical events enter here, including when sharing/keyboard is off.
    /// Returns whether this is a modifier whose release must also reach Windows.
    pub(crate) fn observe_key(&self, vk: u16, down: bool) -> bool {
        let bit = modifier_bit(vk);
        self.update(|state| if down { state | bit } else { state & !bit });
        bit != 0
    }

    pub(crate) fn clear_keys(&self) {
        self.update(|state| state & CONDITION);
    }

    pub(crate) fn ctrl_alt_down(&self) -> (bool, bool) {
        let state = self.state.load(Ordering::SeqCst);
        (state & CTRL != 0, state & ALT != 0)
    }

    pub(crate) fn permit(&self) -> Option<EdgePermit> {
        let state = self.state.load(Ordering::SeqCst);
        let allowed = match (state & CONDITION) >> 6 {
            0 => true,
            1 => state & CTRL != 0,
            2 => state & SHIFT != 0,
            _ => false,
        };
        allowed.then_some(EdgePermit(state))
    }

    pub(crate) fn allows(&self, permit: Option<EdgePermit>) -> bool {
        permit.is_some_and(|p| self.state.load(Ordering::SeqCst) == p.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_side_can_hold_the_condition_without_unrelated_keys_clearing_it() {
        for (condition, left, right, wrong) in [
            (EdgeSwitchCondition::Ctrl, 0xA2, 0xA3, 0xA0),
            (EdgeSwitchCondition::Shift, 0xA0, 0xA1, 0xA2),
        ] {
            let gate = SwitchGate::new(condition);
            assert!(gate.permit().is_none());
            gate.observe_key(wrong, true);
            assert!(gate.permit().is_none());
            gate.observe_key(left, true);
            let first = gate.permit();
            gate.observe_key(65, true);
            gate.observe_key(65, false);
            gate.observe_key(left, true); // auto-repeat does not invalidate motion
            assert!(gate.allows(first));
            gate.observe_key(right, true);
            gate.observe_key(left, false);
            assert!(gate.permit().is_some());
            gate.observe_key(right, false);
            assert!(gate.permit().is_none());
        }
    }

    #[test]
    fn delayed_motion_is_invalid_after_release_repress_or_preference_change() {
        let gate = SwitchGate::new(EdgeSwitchCondition::Ctrl);
        gate.observe_key(0xA3, true);
        let captured = gate.permit();
        gate.observe_key(0xA3, false);
        assert!(!gate.allows(captured));
        gate.observe_key(0xA3, true);
        assert!(!gate.allows(captured));
        let captured = gate.permit();
        gate.set_condition(EdgeSwitchCondition::Always);
        gate.set_condition(EdgeSwitchCondition::Ctrl);
        assert!(!gate.allows(captured));
        gate.clear_keys();
        assert!(gate.permit().is_none());
    }

    #[test]
    fn emergency_chord_and_modifier_releases_work_independently_of_condition() {
        let gate = SwitchGate::new(EdgeSwitchCondition::Shift);
        assert!(gate.observe_key(0xA3, true));
        assert!(gate.observe_key(0xA5, true));
        assert_eq!(gate.ctrl_alt_down(), (true, true));
        assert!(gate.permit().is_none());
        assert!(gate.observe_key(0xA3, false));
        assert_eq!(gate.ctrl_alt_down(), (false, true));
        assert!(!gate.observe_key(0x24, false));
    }
}
