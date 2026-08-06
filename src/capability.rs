//! Typed capability model for server-scoped authorization.
//!
//! Replaces the previous free-form permission strings (`"power"`, `"files"`,
//! `"control.start"`, …) that were compared with ad-hoc prefix matching. A
//! capability is a closed enum, so an unknown or misspelled grant can no longer
//! silently widen access, and the full grantable surface is discoverable at
//! compile time via [`Capability::ALL`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

/// A single grantable action on a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "control.start")]
    ControlStart,
    #[serde(rename = "control.stop")]
    ControlStop,
    #[serde(rename = "control.restart")]
    ControlRestart,
    #[serde(rename = "control.kill")]
    ControlKill,
    #[serde(rename = "console.read")]
    ConsoleRead,
    #[serde(rename = "console.write")]
    ConsoleWrite,
    #[serde(rename = "files.read")]
    FilesRead,
    #[serde(rename = "files.write")]
    FilesWrite,
    #[serde(rename = "backups.read")]
    BackupsRead,
    #[serde(rename = "backups.write")]
    BackupsWrite,
    #[serde(rename = "schedule.read")]
    ScheduleRead,
    #[serde(rename = "schedule.write")]
    ScheduleWrite,
    #[serde(rename = "database.read")]
    DatabaseRead,
    #[serde(rename = "database.write")]
    DatabaseWrite,
    #[serde(rename = "startup.update")]
    StartupUpdate,
    #[serde(rename = "startup.install")]
    StartupInstall,
    #[serde(rename = "startup.secrets")]
    StartupSecrets,
    #[serde(rename = "subusers.read")]
    SubusersRead,
    #[serde(rename = "subusers.write")]
    SubusersWrite,
}

impl Capability {
    /// Every capability, in stable presentation order.
    pub const ALL: [Capability; 19] = [
        Capability::ControlStart,
        Capability::ControlStop,
        Capability::ControlRestart,
        Capability::ControlKill,
        Capability::ConsoleRead,
        Capability::ConsoleWrite,
        Capability::FilesRead,
        Capability::FilesWrite,
        Capability::BackupsRead,
        Capability::BackupsWrite,
        Capability::ScheduleRead,
        Capability::ScheduleWrite,
        Capability::DatabaseRead,
        Capability::DatabaseWrite,
        Capability::StartupUpdate,
        Capability::StartupInstall,
        Capability::StartupSecrets,
        Capability::SubusersRead,
        Capability::SubusersWrite,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Capability::ControlStart => "control.start",
            Capability::ControlStop => "control.stop",
            Capability::ControlRestart => "control.restart",
            Capability::ControlKill => "control.kill",
            Capability::ConsoleRead => "console.read",
            Capability::ConsoleWrite => "console.write",
            Capability::FilesRead => "files.read",
            Capability::FilesWrite => "files.write",
            Capability::BackupsRead => "backups.read",
            Capability::BackupsWrite => "backups.write",
            Capability::ScheduleRead => "schedule.read",
            Capability::ScheduleWrite => "schedule.write",
            Capability::DatabaseRead => "database.read",
            Capability::DatabaseWrite => "database.write",
            Capability::StartupUpdate => "startup.update",
            Capability::StartupInstall => "startup.install",
            Capability::StartupSecrets => "startup.secrets",
            Capability::SubusersRead => "subusers.read",
            Capability::SubusersWrite => "subusers.write",
        }
    }

    /// Grouping used by the UI (`control`, `files`, …).
    pub const fn category(self) -> &'static str {
        match self {
            Capability::ControlStart
            | Capability::ControlStop
            | Capability::ControlRestart
            | Capability::ControlKill => "control",
            Capability::ConsoleRead | Capability::ConsoleWrite => "console",
            Capability::FilesRead | Capability::FilesWrite => "files",
            Capability::BackupsRead | Capability::BackupsWrite => "backups",
            Capability::ScheduleRead | Capability::ScheduleWrite => "schedule",
            Capability::DatabaseRead | Capability::DatabaseWrite => "database",
            Capability::StartupUpdate
            | Capability::StartupInstall
            | Capability::StartupSecrets => "startup",
            Capability::SubusersRead | Capability::SubusersWrite => "subusers",
        }
    }

    /// Short human description surfaced by `GET /api/capabilities`.
    pub const fn describe(self) -> &'static str {
        match self {
            Capability::ControlStart => "Start the workload",
            Capability::ControlStop => "Gracefully stop the workload",
            Capability::ControlRestart => "Restart the workload",
            Capability::ControlKill => "Force-kill the workload",
            Capability::ConsoleRead => "Read console output and logs",
            Capability::ConsoleWrite => "Send commands to the console",
            Capability::FilesRead => "Browse and download files",
            Capability::FilesWrite => "Create, edit and delete files",
            Capability::BackupsRead => "List and download backups",
            Capability::BackupsWrite => "Create, restore and delete backups",
            Capability::ScheduleRead => "View schedules",
            Capability::ScheduleWrite => "Create and modify schedules",
            Capability::DatabaseRead => "Read embedded databases",
            Capability::DatabaseWrite => "Modify embedded databases",
            Capability::StartupUpdate => "Change launch inputs",
            Capability::StartupInstall => "Run the blueprint install step",
            Capability::StartupSecrets => "Reveal hidden launch inputs",
            Capability::SubusersRead => "View team members",
            Capability::SubusersWrite => "Manage team members",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Capability {
    type Err = UnknownCapability;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Capability::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| UnknownCapability(s.to_string()))
    }
}

/// Error returned when a caller supplies a capability name outside the enum.
#[derive(Debug, Clone)]
pub struct UnknownCapability(pub String);

impl fmt::Display for UnknownCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown capability: {}", self.0)
    }
}

impl std::error::Error for UnknownCapability {}

/// Named bundle of capabilities assigned to a team member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read-only visibility.
    Viewer,
    /// Day-to-day power control and console interaction.
    Operator,
    /// Operator plus content and configuration changes.
    Developer,
    /// Everything except transferring ownership.
    Manager,
    /// Explicit capability list, not derived from a preset.
    Custom,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Viewer, Role::Operator, Role::Developer, Role::Manager];

    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Developer => "developer",
            Role::Manager => "manager",
            Role::Custom => "custom",
        }
    }

    /// Capabilities implied by the preset. `Custom` implies nothing.
    pub fn capabilities(self) -> BTreeSet<Capability> {
        use Capability as C;
        let mut set = BTreeSet::new();
        if self == Role::Custom {
            return set;
        }
        set.extend([
            C::ConsoleRead,
            C::FilesRead,
            C::BackupsRead,
            C::ScheduleRead,
            C::DatabaseRead,
        ]);
        if matches!(self, Role::Operator | Role::Developer | Role::Manager) {
            set.extend([
                C::ControlStart,
                C::ControlStop,
                C::ControlRestart,
                C::ConsoleWrite,
            ]);
        }
        if matches!(self, Role::Developer | Role::Manager) {
            set.extend([
                C::FilesWrite,
                C::DatabaseWrite,
                C::ScheduleWrite,
                C::StartupUpdate,
                C::SubusersRead,
            ]);
        }
        if self == Role::Manager {
            set.extend([
                C::ControlKill,
                C::BackupsWrite,
                C::StartupInstall,
                C::StartupSecrets,
                C::SubusersWrite,
            ]);
        }
        set
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = UnknownCapability;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "viewer" => Ok(Role::Viewer),
            "operator" => Ok(Role::Operator),
            "developer" => Ok(Role::Developer),
            "manager" => Ok(Role::Manager),
            "custom" => Ok(Role::Custom),
            other => Err(UnknownCapability(other.to_string())),
        }
    }
}

/// An effective grant: a role preset plus any extra explicit capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub role: Role,
    caps: BTreeSet<Capability>,
}

impl Grant {
    /// Build from a role, unioning the preset with `extra`.
    pub fn new(role: Role, extra: impl IntoIterator<Item = Capability>) -> Self {
        let mut caps = role.capabilities();
        caps.extend(extra);
        Self { role, caps }
    }

    pub fn custom(caps: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            role: Role::Custom,
            caps: caps.into_iter().collect(),
        }
    }

    /// Full authority — used for server owners and root admins.
    pub fn owner() -> Self {
        Self {
            role: Role::Manager,
            caps: Capability::ALL.into_iter().collect(),
        }
    }

    pub fn contains(&self, cap: Capability) -> bool {
        self.caps.contains(&cap)
    }

    pub fn capabilities(&self) -> impl Iterator<Item = Capability> + '_ {
        self.caps.iter().copied()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.caps.iter().map(|c| c.as_str()).collect()
    }

    /// Serialize the capability set as the JSON array stored in `subusers.permissions`.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.names()).unwrap_or_else(|_| "[]".into())
    }

    /// Parse a stored capability array, tolerating pre-v6 legacy tokens.
    pub fn from_stored(role: &str, raw: &str) -> Self {
        let tokens: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
        let caps = expand_legacy(&tokens);
        let role = Role::from_str(role).unwrap_or(Role::Custom);
        Self { role, caps }
    }
}

/// Translate historical permission tokens into typed capabilities.
///
/// Pre-v6 rows stored wildcards (`*`), bare categories (`files`), and legacy
/// power verbs (`start`). Unknown tokens are dropped rather than granted.
pub fn expand_legacy(tokens: &[String]) -> BTreeSet<Capability> {
    use Capability as C;
    let mut out = BTreeSet::new();
    for token in tokens {
        let token = token.trim();
        if token == "*" {
            return Capability::ALL.into_iter().collect();
        }
        if let Ok(cap) = Capability::from_str(token) {
            out.insert(cap);
            continue;
        }
        match token {
            "power" | "control" => out.extend([
                C::ControlStart,
                C::ControlStop,
                C::ControlRestart,
                C::ControlKill,
            ]),
            "start" => {
                out.insert(C::ControlStart);
            }
            "stop" => {
                out.insert(C::ControlStop);
            }
            "restart" => {
                out.insert(C::ControlRestart);
            }
            "kill" => {
                out.insert(C::ControlKill);
            }
            "console" => out.extend([C::ConsoleRead, C::ConsoleWrite]),
            "files" => out.extend([C::FilesRead, C::FilesWrite]),
            "backups" => out.extend([C::BackupsRead, C::BackupsWrite]),
            "schedule" | "schedules" => out.extend([C::ScheduleRead, C::ScheduleWrite]),
            "database" | "databases" => out.extend([C::DatabaseRead, C::DatabaseWrite]),
            "startup" => out.extend([C::StartupUpdate, C::StartupInstall, C::StartupSecrets]),
            "subusers" => out.extend([C::SubusersRead, C::SubusersWrite]),
            _ => {}
        }
    }
    out
}

/// Capability required to perform a power action.
pub fn power_capability(action: crate::node_protocol::PowerAction) -> Capability {
    use crate::node_protocol::PowerAction as P;
    match action {
        P::Start => Capability::ControlStart,
        P::Stop => Capability::ControlStop,
        P::Restart => Capability::ControlRestart,
        P::Kill => Capability::ControlKill,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_expands_to_every_capability() {
        let caps = expand_legacy(&["*".to_string()]);
        assert_eq!(caps.len(), Capability::ALL.len());
    }

    #[test]
    fn legacy_tokens_map_to_typed_capabilities() {
        let caps = expand_legacy(&["power".into(), "files".into(), "start".into()]);
        assert!(caps.contains(&Capability::ControlKill));
        assert!(caps.contains(&Capability::FilesWrite));
        assert!(!caps.contains(&Capability::BackupsWrite));
    }

    #[test]
    fn unknown_tokens_are_dropped_not_granted() {
        let caps = expand_legacy(&["files.destroy".into(), "admin".into()]);
        assert!(caps.is_empty());
    }

    #[test]
    fn roles_are_strictly_nested() {
        let viewer = Role::Viewer.capabilities();
        let operator = Role::Operator.capabilities();
        let developer = Role::Developer.capabilities();
        let manager = Role::Manager.capabilities();
        assert!(viewer.is_subset(&operator));
        assert!(operator.is_subset(&developer));
        assert!(developer.is_subset(&manager));
        assert_eq!(manager.len(), Capability::ALL.len());
    }

    #[test]
    fn viewer_holds_no_mutating_capability() {
        let grant = Grant::new(Role::Viewer, []);
        for cap in Capability::ALL {
            let mutating = cap.as_str().ends_with(".write")
                || cap.category() == "control"
                || matches!(cap, Capability::StartupUpdate | Capability::StartupInstall);
            if mutating {
                assert!(!grant.contains(cap), "viewer must not hold {cap}");
            }
        }
    }

    #[test]
    fn grant_roundtrips_through_storage() {
        let grant = Grant::new(Role::Developer, [Capability::BackupsWrite]);
        let restored = Grant::from_stored(grant.role.as_str(), &grant.to_json());
        assert_eq!(grant, restored);
        assert!(restored.contains(Capability::BackupsWrite));
    }

    #[test]
    fn owner_grant_covers_every_capability() {
        let owner = Grant::owner();
        assert!(Capability::ALL.into_iter().all(|c| owner.contains(c)));
    }

    #[test]
    fn each_power_action_maps_to_its_own_capability() {
        use crate::node_protocol::PowerAction as P;
        assert_eq!(power_capability(P::Start), Capability::ControlStart);
        assert_eq!(power_capability(P::Stop), Capability::ControlStop);
        assert_eq!(power_capability(P::Restart), Capability::ControlRestart);
        assert_eq!(power_capability(P::Kill), Capability::ControlKill);
    }

    #[test]
    fn unknown_power_action_fails_to_deserialize() {
        use crate::node_protocol::PowerAction;
        assert!(serde_json::from_str::<PowerAction>("\"start\"").is_ok());
        assert!(serde_json::from_str::<PowerAction>("\"obliterate\"").is_err());
    }
}
