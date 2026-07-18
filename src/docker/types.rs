// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Our own container types.
//!
//! bollard's generated models stop here and never reach the UI (CLAUDE.md
//! rule 3). Everything below is plain owned data with no `Option<Vec<Option<_>>>`
//! in sight, so the `view!` macro stays readable.

use bollard::models::{
    ContainerInspectResponse, ContainerStateStatusEnum, ContainerStatsResponse, ContainerSummary,
    ContainerSummaryStateEnum, PortSummary, PortSummaryTypeEnum,
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

// The list and inspect use different-but-identical state enums.
impl From<ContainerStateStatusEnum> for ContainerState {
    fn from(state: ContainerStateStatusEnum) -> Self {
        use ContainerStateStatusEnum as S;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerDetail {
    /// Current state, so a periodic re-inspect can keep the chip/button live.
    pub state: ContainerState,
    /// Published ports, so a re-inspect picks up bindings a container gains when
    /// it starts (a stopped container has none).
    pub ports: Vec<Port>,
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

        let container_state = state
            .and_then(|s| s.status)
            .map(ContainerState::from)
            .unwrap_or(ContainerState::Unknown);

        let ports = ports_from_inspect(&resp);

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
            state: container_state,
            ports,
            started_at,
            created: resp.created,
            command,
        }
    }
}

/// Map inspect's `NetworkSettings.ports` to our published-port list.
///
/// The map is keyed `"5432/tcp"` -> host bindings. We keep only ports actually
/// published to the host (a binding with a `host_port`), matching what the list
/// summary shows and dropping merely-exposed ports.
fn ports_from_inspect(resp: &ContainerInspectResponse) -> Vec<Port> {
    let mut ports: Vec<Port> = resp
        .network_settings
        .as_ref()
        .and_then(|net| net.ports.as_ref())
        .map(|map| {
            map.iter()
                .filter_map(|(key, bindings)| {
                    let (private, protocol) = parse_port_key(key)?;
                    // First binding that names a host port.
                    let public = bindings
                        .as_ref()?
                        .iter()
                        .find_map(|b| b.host_port.as_ref()?.parse::<u16>().ok())?;
                    Some(Port {
                        private,
                        public,
                        protocol,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // One mapping per interface (v4/v6) would otherwise duplicate.
    ports.sort_unstable_by_key(|port| (port.public, port.private));
    ports.dedup();
    ports
}

/// `"5432/tcp"` -> `(5432, Tcp)`.
fn parse_port_key(key: &str) -> Option<(u16, Protocol)> {
    let (port, proto) = key.split_once('/')?;
    let private = port.parse().ok()?;
    let protocol = match proto {
        "udp" => Protocol::Udp,
        "sctp" => Protocol::Sctp,
        _ => Protocol::Tcp,
    };
    Some((private, protocol))
}

/// One resource sample from the stats stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    /// CPU as a percentage. Can exceed 100 on multiple cores (like `docker
    /// stats`), which is why it's not clamped.
    pub cpu_percent: f64,
    /// Memory the container is using, in bytes.
    pub mem_used: u64,
    /// The memory limit, in bytes. 0 if unset.
    pub mem_limit: u64,
}

impl Stats {
    /// Compute a sample from one stats frame. Docker includes the previous
    /// reading (`precpu_stats`) in each frame, so CPU% comes from the delta
    /// between the two. Returns `None` for an incomplete frame (the first one,
    /// or a stopped container) rather than a bogus zero.
    pub fn from_response(resp: ContainerStatsResponse) -> Option<Stats> {
        let cpu = resp.cpu_stats?;
        let pre = resp.precpu_stats?;

        let cpu_total = cpu.cpu_usage?.total_usage?;
        let pre_total = pre.cpu_usage.and_then(|u| u.total_usage).unwrap_or(0);
        let system = cpu.system_cpu_usage?;
        let pre_system = pre.system_cpu_usage.unwrap_or(0);

        let cpu_delta = cpu_total.saturating_sub(pre_total) as f64;
        let system_delta = system.saturating_sub(pre_system) as f64;
        // online_cpus is missing on some daemons; fall back to the per-CPU list.
        let cpus = cpu.online_cpus.map(|n| n as f64).unwrap_or(1.0).max(1.0);

        let cpu_percent = if system_delta > 0.0 {
            cpu_delta / system_delta * cpus * 100.0
        } else {
            0.0
        };

        let mem = resp.memory_stats?;
        let usage = mem.usage?;
        // Docker's own calc subtracts the reclaimable page cache (`inactive_file`
        // on cgroup v2) so the figure matches `docker stats`.
        let inactive = mem
            .stats
            .as_ref()
            .and_then(|s| s.get("inactive_file").copied())
            .unwrap_or(0);

        Some(Stats {
            cpu_percent,
            mem_used: usage.saturating_sub(inactive),
            mem_limit: mem.limit.unwrap_or(0),
        })
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
    fn computes_cpu_percent_from_the_delta() {
        use bollard::models::{ContainerCpuStats, ContainerCpuUsage, ContainerMemoryStats};

        let cpu = |total, system| ContainerCpuStats {
            cpu_usage: Some(ContainerCpuUsage {
                total_usage: Some(total),
                ..Default::default()
            }),
            system_cpu_usage: Some(system),
            online_cpus: Some(2),
            ..Default::default()
        };
        let resp = ContainerStatsResponse {
            cpu_stats: Some(cpu(200, 2000)),
            precpu_stats: Some(cpu(100, 1000)),
            memory_stats: Some(ContainerMemoryStats {
                usage: Some(1000),
                limit: Some(4000),
                stats: Some([("inactive_file".to_owned(), 200)].into_iter().collect()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let stats = Stats::from_response(resp).expect("complete frame");
        // cpu_delta 100 / system_delta 1000 * 2 cpus * 100 = 20%
        assert!((stats.cpu_percent - 20.0).abs() < 1e-9);
        // 1000 usage minus 200 reclaimable cache
        assert_eq!(stats.mem_used, 800);
        assert_eq!(stats.mem_limit, 4000);
    }

    #[test]
    fn maps_published_ports_from_inspect() {
        use bollard::models::{NetworkSettings, PortBinding};

        let resp = ContainerInspectResponse {
            network_settings: Some(NetworkSettings {
                ports: Some(
                    [
                        // Published: two interface bindings dedupe to one.
                        (
                            "5432/tcp".to_owned(),
                            Some(vec![
                                PortBinding {
                                    host_ip: Some("0.0.0.0".to_owned()),
                                    host_port: Some("5432".to_owned()),
                                },
                                PortBinding {
                                    host_ip: Some("::".to_owned()),
                                    host_port: Some("5432".to_owned()),
                                },
                            ]),
                        ),
                        // Exposed but not published -> dropped.
                        ("9000/tcp".to_owned(), None),
                    ]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        };

        let ports = ContainerDetail::from_inspect(resp).ports;
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].public, 5432);
        assert_eq!(ports[0].private, 5432);
        assert_eq!(ports[0].protocol, Protocol::Tcp);
    }

    #[test]
    fn incomplete_stats_frame_is_none() {
        // No cpu_stats -> the first frame / a stopped container. Skip, don't zero.
        assert!(Stats::from_response(ContainerStatsResponse::default()).is_none());
    }

    #[test]
    fn zero_system_delta_doesnt_divide_by_zero() {
        use bollard::models::{ContainerCpuStats, ContainerCpuUsage, ContainerMemoryStats};
        let cpu = ContainerCpuStats {
            cpu_usage: Some(ContainerCpuUsage {
                total_usage: Some(100),
                ..Default::default()
            }),
            system_cpu_usage: Some(1000),
            online_cpus: Some(1),
            ..Default::default()
        };
        let resp = ContainerStatsResponse {
            cpu_stats: Some(cpu.clone()),
            precpu_stats: Some(cpu), // same values -> zero deltas
            memory_stats: Some(ContainerMemoryStats {
                usage: Some(500),
                limit: Some(1000),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(Stats::from_response(resp).unwrap().cpu_percent, 0.0);
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
