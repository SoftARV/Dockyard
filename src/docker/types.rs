//! Our own container types.
//!
//! bollard's generated models stop here and never reach the UI (CLAUDE.md
//! rule 3). Everything below is plain owned data with no `Option<Vec<Option<_>>>`
//! in sight, so the `view!` macro stays readable.

use bollard::models::{
    ContainerSummary, ContainerSummaryStateEnum, PortSummary, PortSummaryTypeEnum,
};

/// Lifecycle state of a container.
///
/// Mirrors Docker's own state machine rather than collapsing to a bool, because
/// the status dot wants to distinguish "exited" from "dead" from "restarting".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Paused,
    Restarting,
    Stopping,
    Exited,
    Removing,
    Dead,
    /// Docker sent a state we don't model, or none at all.
    Unknown,
}

impl ContainerState {
    /// Whether the primary button should offer "stop" rather than "start".
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Restarting)
    }
}

impl From<ContainerSummaryStateEnum> for ContainerState {
    fn from(state: ContainerSummaryStateEnum) -> Self {
        use ContainerSummaryStateEnum as S;
        match state {
            S::CREATED => Self::Created,
            S::RUNNING => Self::Running,
            S::PAUSED => Self::Paused,
            S::RESTARTING => Self::Restarting,
            S::STOPPING => Self::Stopping,
            S::EXITED => Self::Exited,
            S::REMOVING => Self::Removing,
            S::DEAD => Self::Dead,
            S::EMPTY => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
    Sctp,
}

impl From<PortSummaryTypeEnum> for Protocol {
    fn from(proto: PortSummaryTypeEnum) -> Self {
        use PortSummaryTypeEnum as P;
        match proto {
            P::UDP => Self::Udp,
            P::SCTP => Self::Sctp,
            // Docker omits the type for plain TCP, so EMPTY means TCP.
            P::TCP | P::EMPTY => Self::Tcp,
        }
    }
}

/// A published port mapping. Unpublished ports are dropped at the boundary —
/// they're not actionable from the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub private: u16,
    pub public: u16,
    pub protocol: Protocol,
}

impl Port {
    fn from_summary(summary: &PortSummary) -> Option<Self> {
        Some(Self {
            private: summary.private_port,
            // No public port means the port isn't published to the host.
            public: summary.public_port?,
            protocol: summary.typ.map(Protocol::from).unwrap_or(Protocol::Tcp),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: ContainerState,
    /// Docker's human-readable status, e.g. "Up 39 minutes (healthy)".
    pub status: String,
    pub ports: Vec<Port>,
}

impl Container {
    /// Map a bollard summary into our type.
    ///
    /// Returns `None` when the summary has no id: every action we expose keys
    /// off the id, so a container without one is not something we can render a
    /// working row for. Callers use `filter_map` to drop these.
    pub fn from_summary(summary: ContainerSummary) -> Option<Self> {
        let id = summary.id?;

        // Docker returns names with a leading slash ("/inventory_pos_db") and a
        // container can technically have several; the first is the canonical one.
        let name = summary
            .names
            .and_then(|names| names.into_iter().next())
            .map(|name| name.trim_start_matches('/').to_owned())
            .unwrap_or_else(|| id.chars().take(12).collect());

        let mut ports: Vec<Port> = summary
            .ports
            .unwrap_or_default()
            .iter()
            .filter_map(Port::from_summary)
            .collect();
        // Docker lists a mapping per host interface (IPv4 and IPv6), which would
        // render as a duplicate "8080:80" in the subtitle.
        ports.sort_unstable_by_key(|port| (port.public, port.private));
        ports.dedup();

        Some(Self {
            id,
            name,
            image: summary.image.unwrap_or_else(|| "<unknown>".to_owned()),
            state: summary
                .state
                .map(ContainerState::from)
                .unwrap_or(ContainerState::Unknown),
            status: summary.status.unwrap_or_default(),
            ports,
        })
    }
}
