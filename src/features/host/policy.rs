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
    fn admits(self, default_allowed: bool) -> bool {
        match self {
            Self::Follow => default_allowed,
            Self::Allow => true,
            Self::Deny => false,
        }
    }
}

/// 主控设备 ID → 策略。只存偏离「跟随全局」的选择，所以缺省即 Follow。
pub(crate) type DevicePolicies = BTreeMap<String, DevicePolicy>;

/// `default_allowed` 是「允许被控」总开关，它只是一个默认值：关掉之后
/// 新设备一律被拒，但被单独设为「允许」的设备仍然连得进来。
pub(crate) fn admits(policies: &DevicePolicies, controller: &str, default_allowed: bool) -> bool {
    policies
        .get(controller)
        .copied()
        .unwrap_or_default()
        .admits(default_allowed)
}

/// 有没有设备被单独放行。总开关关着时，本机靠它决定还要不要保持在线可被控，
/// 否则那些设备在主控那边只会看到「未开放连接」。
pub(crate) fn any_allowed(policies: &DevicePolicies) -> bool {
    policies.values().any(|p| *p == DevicePolicy::Allow)
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
    fn follow_takes_the_switch() {
        let policies = DevicePolicies::new();
        assert!(admits(&policies, "a", true));
        assert!(!admits(&policies, "a", false));
    }

    #[test]
    fn per_device_choice_wins_over_the_switch() {
        let mut policies = DevicePolicies::new();
        insert(&mut policies, "a", DevicePolicy::Allow);
        insert(&mut policies, "b", DevicePolicy::Deny);
        // 总开关关着时，被单独放行的设备仍然连得进来，其余的一律不行。
        assert!(admits(&policies, "a", false));
        assert!(!admits(&policies, "b", true));
        assert!(admits(&policies, "c", true));
        assert!(!admits(&policies, "c", false));
    }

    #[test]
    fn an_allowed_device_keeps_the_machine_online() {
        let mut policies = DevicePolicies::new();
        assert!(!any_allowed(&policies));
        insert(&mut policies, "a", DevicePolicy::Deny);
        assert!(!any_allowed(&policies));
        insert(&mut policies, "b", DevicePolicy::Allow);
        assert!(any_allowed(&policies));
        insert(&mut policies, "b", DevicePolicy::Follow);
        assert!(!any_allowed(&policies));
    }

    #[test]
    fn follow_is_not_stored() {
        let mut policies = DevicePolicies::new();
        insert(&mut policies, "a", DevicePolicy::Deny);
        insert(&mut policies, "a", DevicePolicy::Follow);
        assert!(policies.is_empty());
    }
}
