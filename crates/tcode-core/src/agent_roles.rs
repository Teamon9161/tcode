/// Configurable model roles that are not defined by an agent Markdown file.
/// Their metadata belongs in core because config, model resolution, and the
/// `/agents` picker all need the same default semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Auto,
    Compact,
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
}

impl AgentRole {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Compact,
        Self::Suggest,
        Self::Vision,
        Self::Fetch,
    ];

    pub const fn meta(self) -> AgentRoleMeta {
        match self {
            Self::Auto => AgentRoleMeta {
                key: "auto",
                label: "auto",
                default: RoleDefault::InheritMain,
            },
            Self::Compact => AgentRoleMeta {
                key: "compact",
                label: "compact",
                default: RoleDefault::InheritMain,
            },
            Self::Suggest => AgentRoleMeta {
                key: "suggest",
                label: "suggest",
                default: RoleDefault::InheritMain,
            },
            Self::Vision => AgentRoleMeta {
                key: "vision",
                label: "vision",
                default: RoleDefault::InheritMain,
            },
            Self::Fetch => AgentRoleMeta {
                key: "fetch",
                label: "web-fetch",
                default: RoleDefault::Off,
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

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.key() == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_metadata_keeps_compact_inheriting_and_fetch_off_by_default() {
        assert_eq!(AgentRole::Compact.key(), "compact");
        assert_eq!(AgentRole::Compact.default(), RoleDefault::InheritMain);
        assert_eq!(AgentRole::Fetch.label(), "web-fetch");
        assert!(AgentRole::Fetch.allows_off());
        assert!(AgentRole::ALL
            .into_iter()
            .filter(|role| role.allows_off())
            .eq([AgentRole::Fetch]));
    }
}
