//! Our own container types.
//!
//! bollard's generated models stop here and never reach the UI (CLAUDE.md
//! rule 3). Everything below is plain owned data with no `Option<Vec<Option<_>>>`
//! in sight, so the `view!` macro stays readable.

use bollard::models::{
    ContainerInspectResponse, ContainerSummary, ContainerSummaryStateEnum, PortSummary,
    PortSummaryTypeEnum,
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

/// The extra fields `inspect` gives beyond the list summary. Kept separate from
/// `Container` because the detail page fetches them lazily, one container at a
/// time — the list never needs them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerDetail {
    /// Start time as an RFC3339 string, for computing live uptime. `None` when
    /// the container isn't running (a stopped container has no meaningful
    /// uptime).
    pub started_at: Option<String>,
    /// Creation time, RFC3339.
    pub created: Option<String>,
    /// The full command line the container runs.
    pub command: Option<String>,
}

impl ContainerDetail {
    pub fn from_inspect(resp: ContainerInspectResponse) -> Self {
        let state = resp.state.as_ref();
        let running = state.and_then(|s| s.running).unwrap_or(false);

        let started_at = running
            .then(|| state.and_then(|s| s.started_at.clone()))
            .flatten();

        // Docker splits the command across entrypoint + cmd; join what's there
        // into one line, matching how `docker ps` shows it.
        let command = resp.config.as_ref().and_then(|config| {
            let parts: Vec<&str> = config
                .entrypoint
                .iter()
                .flatten()
                .chain(config.cmd.iter().flatten())
                .map(String::as_str)
                .collect();
            (!parts.is_empty()).then(|| parts.join(" "))
        });

        Self {
            started_at,
            created: resp.created,
            command,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary with only an id — every other field `None`, which is exactly
    /// the shape the Docker API is allowed to hand us.
    fn bare(id: &str) -> ContainerSummary {
        ContainerSummary {
            id: Some(id.to_owned()),
            ..Default::default()
        }
    }

    fn port(private: u16, public: Option<u16>, ip: &str) -> PortSummary {
        PortSummary {
            ip: Some(ip.to_owned()),
            private_port: private,
            public_port: public,
            typ: Some(PortSummaryTypeEnum::TCP),
        }
    }

    #[test]
    fn drops_a_summary_with_no_id() {
        // Every action keys off the id, so a row for this could never work.
        let summary = ContainerSummary {
            names: Some(vec!["/orphan".to_owned()]),
            ..Default::default()
        };
        assert!(Container::from_summary(summary).is_none());
    }

    #[test]
    fn strips_the_leading_slash_docker_puts_on_names() {
        let summary = ContainerSummary {
            names: Some(vec!["/inventory_pos_db".to_owned()]),
            ..bare("abc123")
        };
        let container = Container::from_summary(summary).expect("has an id");
        assert_eq!(container.name, "inventory_pos_db");
    }

    #[test]
    fn takes_the_first_name_when_a_container_has_several() {
        let summary = ContainerSummary {
            names: Some(vec!["/first".to_owned(), "/second".to_owned()]),
            ..bare("abc123")
        };
        let container = Container::from_summary(summary).expect("has an id");
        assert_eq!(container.name, "first");
    }

    #[test]
    fn falls_back_to_a_short_id_when_unnamed() {
        let container = Container::from_summary(bare("0123456789abcdef0123")).expect("has an id");
        assert_eq!(container.name, "0123456789ab", "should be 12 chars of id");
    }

    #[test]
    fn dedupes_the_same_mapping_on_ipv4_and_ipv6() {
        // Docker reports one mapping per host interface. Rendering both would
        // put a duplicate "8080:80" in the row's subtitle.
        let summary = ContainerSummary {
            ports: Some(vec![
                port(80, Some(8080), "0.0.0.0"),
                port(80, Some(8080), "::"),
            ]),
            ..bare("abc123")
        };
        let container = Container::from_summary(summary).expect("has an id");
        assert_eq!(container.ports.len(), 1);
        assert_eq!(container.ports[0].public, 8080);
        assert_eq!(container.ports[0].private, 80);
    }

    #[test]
    fn drops_unpublished_ports() {
        // No public port means it isn't reachable from the host, so there's
        // nothing worth showing.
        let summary = ContainerSummary {
            ports: Some(vec![port(5432, None, "0.0.0.0")]),
            ..bare("abc123")
        };
        let container = Container::from_summary(summary).expect("has an id");
        assert!(container.ports.is_empty());
    }

    #[test]
    fn keeps_distinct_ports_and_sorts_them() {
        let summary = ContainerSummary {
            ports: Some(vec![
                port(443, Some(8443), "0.0.0.0"),
                port(80, Some(8080), "0.0.0.0"),
            ]),
            ..bare("abc123")
        };
        let container = Container::from_summary(summary).expect("has an id");
        assert_eq!(container.ports.len(), 2);
        assert_eq!(container.ports[0].public, 8080, "sorted by public port");
        assert_eq!(container.ports[1].public, 8443);
    }

    #[test]
    fn missing_state_is_unknown_not_a_guess() {
        let container = Container::from_summary(bare("abc123")).expect("has an id");
        assert_eq!(container.state, ContainerState::Unknown);
        assert!(!container.state.is_running());
    }

    #[test]
    fn maps_docker_states_we_act_on() {
        use ContainerSummaryStateEnum as S;
        let cases = [
            (S::RUNNING, ContainerState::Running, true),
            (S::EXITED, ContainerState::Exited, false),
            (S::PAUSED, ContainerState::Paused, false),
            (S::DEAD, ContainerState::Dead, false),
            // Restarting counts as running, so the button offers "stop".
            (S::RESTARTING, ContainerState::Restarting, true),
            // Docker's empty string means "no state", not a state called "".
            (S::EMPTY, ContainerState::Unknown, false),
        ];

        for (docker_state, expected, running) in cases {
            let summary = ContainerSummary {
                state: Some(docker_state),
                ..bare("abc123")
            };
            let container = Container::from_summary(summary).expect("has an id");
            assert_eq!(container.state, expected, "mapping {docker_state:?}");
            assert_eq!(
                container.state.is_running(),
                running,
                "for {docker_state:?}"
            );
        }
    }

    #[test]
    fn missing_image_does_not_render_an_empty_subtitle() {
        let container = Container::from_summary(bare("abc123")).expect("has an id");
        assert_eq!(container.image, "<unknown>");
    }
}
