//! 同账号设备的被控许可：总开关是默认值，单台设备可以覆盖它。
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DevicePolicy {
    /// 跟随「允许被控」总开关。
    #[default]
    Follow,
    Allow,
    Deny,
}

impl DevicePolicy {
    pub(crate) const ALL: [Self; 3] = [Self::Follow, Self::Allow, Self::Deny];
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Follow => "跟随全局",
            Self::Allow => "允许",
            Self::Deny => "禁止",
        }
    }
}

/// 主控设备 ID → 策略。只存偏离「跟随全局」的选择，所以缺省即 Follow。
pub(crate) type DevicePolicies = BTreeMap<String, DevicePolicy>;

/// `enabled` 是「允许被控」总开关，它是硬开关：关着时这张表完全不参与判定。
/// 开着时只有「禁止」会挡下一台设备，「允许」与「跟随全局」等价——留着它是
/// 为了把「这台设备可以」写死，将来默认值收紧时不必再回来改。
pub(crate) fn admits(policies: &DevicePolicies, controller: &str, enabled: bool) -> bool {
    enabled && !matches!(policies.get(controller), Some(DevicePolicy::Deny))
}

/// 只留下非 Follow 的条目，避免这份表跟着设备列表一起长胖。
pub(crate) fn insert(policies: &mut DevicePolicies, device: &str, policy: DevicePolicy) {
    if policy == DevicePolicy::Follow {
        policies.remove(device);
    } else {
        policies.insert(device.to_owned(), policy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_gates_every_device() {
        let mut policies = DevicePolicies::new();
        insert(&mut policies, "a", DevicePolicy::Allow);
        insert(&mut policies, "b", DevicePolicy::Deny);
        assert!(admits(&policies, "a", true));
        assert!(admits(&policies, "c", true));
        assert!(!admits(&policies, "b", true));
        // 总开关关着时，连显式放行的设备也一起挡住。
        assert!(!admits(&policies, "a", false));
        assert!(!admits(&policies, "c", false));
    }

    #[test]
    fn follow_is_not_stored() {
        let mut policies = DevicePolicies::new();
        insert(&mut policies, "a", DevicePolicy::Deny);
        insert(&mut policies, "a", DevicePolicy::Follow);
        assert!(policies.is_empty());
    }
}
