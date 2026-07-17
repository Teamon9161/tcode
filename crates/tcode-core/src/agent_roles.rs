/// Configurable model roles shared by the consumers that run auxiliary work
/// and the `/agents` frontend. Role metadata belongs here because config,
/// model resolution, and the picker all need the same default semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Explore,
    Plan,
    General,
    Auto,
    Suggest,
    Vision,
    Fetch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleDefault {
    InheritMain,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentRoleMeta {
    pub key: &'static str,
    pub label: &'static str,
    pub default: RoleDefault,
    pub task_kind: bool,
}

impl AgentRole {
    pub const ALL: [Self; 7] = [
        Self::Explore,
        Self::Plan,
        Self::General,
        Self::Auto,
        Self::Suggest,
        Self::Vision,
        Self::Fetch,
    ];
    pub const TASK_KINDS: [Self; 3] = [Self::Explore, Self::Plan, Self::General];
    pub const TASK_KEYS: [&'static str; 3] = ["explore", "plan", "general"];

    pub const fn meta(self) -> AgentRoleMeta {
        match self {
            Self::Explore => AgentRoleMeta {
                key: "explore",
                label: "explore",
                default: RoleDefault::InheritMain,
                task_kind: true,
            },
            Self::Plan => AgentRoleMeta {
                key: "plan",
                label: "plan",
                default: RoleDefault::InheritMain,
                task_kind: true,
            },
            Self::General => AgentRoleMeta {
                key: "general",
                label: "general",
                default: RoleDefault::InheritMain,
                task_kind: true,
            },
            Self::Auto => AgentRoleMeta {
                key: "auto",
                label: "auto",
                default: RoleDefault::InheritMain,
                task_kind: false,
            },
            Self::Suggest => AgentRoleMeta {
                key: "suggest",
                label: "suggest",
                default: RoleDefault::InheritMain,
                task_kind: false,
            },
            Self::Vision => AgentRoleMeta {
                key: "vision",
                label: "vision",
                default: RoleDefault::InheritMain,
                task_kind: false,
            },
            Self::Fetch => AgentRoleMeta {
                key: "fetch",
                label: "web-fetch",
                default: RoleDefault::Off,
                task_kind: false,
            },
        }
    }

    pub const fn key(self) -> &'static str {
        self.meta().key
    }
    pub const fn label(self) -> &'static str {
        self.meta().label
    }
    pub const fn default(self) -> RoleDefault {
        self.meta().default
    }
    pub const fn allows_off(self) -> bool {
        matches!(self.default(), RoleDefault::Off)
    }
    pub const fn is_task_kind(self) -> bool {
        self.meta().task_kind
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.key() == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_metadata_keeps_task_and_fetch_semantics_distinct() {
        assert_eq!(AgentRole::Fetch.label(), "web-fetch");
        assert!(AgentRole::Fetch.allows_off());
        assert_eq!(
            AgentRole::TASK_KINDS,
            [AgentRole::Explore, AgentRole::Plan, AgentRole::General]
        );
        assert!(AgentRole::ALL
            .into_iter()
            .filter(|role| role.allows_off())
            .eq([AgentRole::Fetch]));
    }
}
