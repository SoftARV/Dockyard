// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Socket discovery and thin async wrappers around bollard.
//!
//! Everything that knows a socket path lives here (CLAUDE.md rule 2).
//!
//! Two runtimes are supported — Docker and Podman — but only ever **one at a
//! time**. Podman exposes a Docker-compatible API, so once connected every call
//! below is identical; all the runtime-awareness is in discovery, in the
//! `Connection` we hand back, and in the remedy text when something's wrong.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::errors::Error as BollardError;
use bollard::models::SystemVersion;
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptions, RestartContainerOptions, StartContainerOptions, StatsOptionsBuilder,
    StopContainerOptions,
};
use futures_util::{Stream, StreamExt};
use tracing::{debug, info, warn};

use super::types::{Container, ContainerDetail, Stats};

const DOCKER_ROOTFUL_SOCKET: &str = "/var/run/docker.sock";
const PODMAN_ROOTFUL_SOCKET: &str = "/run/podman/podman.sock";

/// Which container runtime answered.
///
/// Determined by *asking the daemon*, never by the socket path — `podman-docker`
/// can put Podman behind `/var/run/docker.sock`, so the path proves nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Docker,
    Podman,
}

impl Runtime {
    /// The name to show a person.
    pub fn label(self) -> &'static str {
        match self {
            Runtime::Docker => "Docker",
            Runtime::Podman => "Podman",
        }
    }
}

/// Which runtime the user wants. `Auto` takes the first one that answers.
///
/// Persisted by `settings.rs`; the header's runtime menu sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimePreference {
    #[default]
    Auto,
    Docker,
    Podman,
}

impl RuntimePreference {
    /// The stable string written to the config file — and the menu action's
    /// target value, so the two can't drift apart.
    pub fn as_key(self) -> &'static str {
        match self {
            RuntimePreference::Auto => "auto",
            RuntimePreference::Docker => "docker",
            RuntimePreference::Podman => "podman",
        }
    }

    /// Anything unrecognised falls back to `Auto`, like the other settings: a
    /// hand-edited config file shouldn't be able to wedge the app.
    pub fn from_key(key: &str) -> Self {
        match key {
            "docker" => RuntimePreference::Docker,
            "podman" => RuntimePreference::Podman,
            _ => RuntimePreference::Auto,
        }
    }

    /// The menu label.
    pub fn label(self) -> &'static str {
        match self {
            RuntimePreference::Auto => "Automatic",
            RuntimePreference::Docker => "Docker",
            RuntimePreference::Podman => "Podman",
        }
    }

    /// Whether this preference will consider `runtime` at all.
    fn admits(self, runtime: Runtime) -> bool {
        match self {
            RuntimePreference::Auto => true,
            RuntimePreference::Docker => runtime == Runtime::Docker,
            RuntimePreference::Podman => runtime == Runtime::Podman,
        }
    }
}

/// A live connection, plus what's on the other end of it.
///
/// Replaces the bare `Docker` handle the app used to hold: the runtime and
/// version are established once, at connect time, rather than re-derived
/// wherever they're needed.
#[derive(Debug, Clone)]
pub struct Connection {
    pub docker: Docker,
    pub runtime: Runtime,
    /// As the daemon reports it, e.g. "6.0.1".
    pub version: String,
}

impl Connection {
    /// "Podman 6.0.1" — for the runtime chip's tooltip and the About dialog.
    pub fn describe(&self) -> String {
        format!("{} {}", self.runtime.label(), self.version)
    }
}

/// A socket we know how to look for.
///
/// `runtime` here is only a *guess* from the path — good enough to pick a
/// remedy to suggest, not good enough to label the UI with. `identify` settles
/// that after connecting.
#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    runtime: Runtime,
    /// Rootless sockets are per-user (`systemctl --user`); rootful ones need
    /// `sudo`. Only affects which command we suggest.
    rootless: bool,
}

/// Every socket we look for, in priority order.
///
/// **Runtime-major, Docker first** — not scope-major (all rootless, then all
/// rootful). On a machine with rootful Docker and rootless Podman, which is a
/// common setup, a scope-major order would match `podman.sock` *before*
/// `/var/run/docker.sock` and silently switch runtimes: you'd open the app and
/// your Docker containers would be gone. This order preserves the behaviour
/// Dockyard had when it only knew about Docker.
fn candidates() -> Vec<Candidate> {
    candidates_in(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
}

/// The list-building half of [`candidates`], with the environment passed in so
/// the ordering can be tested without mutating process-global state.
fn candidates_in(runtime_dir: Option<PathBuf>) -> Vec<Candidate> {
    let mut candidates = Vec::with_capacity(4);

    if let Some(dir) = &runtime_dir {
        candidates.push(Candidate {
            path: dir.join("docker.sock"),
            runtime: Runtime::Docker,
            rootless: true,
        });
    }
    candidates.push(Candidate {
        path: PathBuf::from(DOCKER_ROOTFUL_SOCKET),
        runtime: Runtime::Docker,
        rootless: false,
    });
    if let Some(dir) = &runtime_dir {
        candidates.push(Candidate {
            path: dir.join("podman").join("podman.sock"),
            runtime: Runtime::Podman,
            rootless: true,
        });
    }
    candidates.push(Candidate {
        path: PathBuf::from(PODMAN_ROOTFUL_SOCKET),
        runtime: Runtime::Podman,
        rootless: false,
    });

    candidates
}

/// `DOCKER_HOST`, if it's set to something non-empty.
fn docker_host() -> Option<String> {
    std::env::var("DOCKER_HOST")
        .ok()
        .filter(|host| !host.is_empty())
}

/// Which runtimes this machine could offer: the sockets that exist, plus Docker
/// whenever `DOCKER_HOST` is set (we can't know what's behind it without
/// connecting, and it is Docker's own variable).
///
/// Drives whether the header shows a runtime switcher at all — with one runtime
/// there's no choice to make, so the header stays exactly as it was.
pub fn available_runtimes() -> Vec<Runtime> {
    let mut found: Vec<Runtime> = Vec::with_capacity(2);

    if docker_host().is_some() {
        found.push(Runtime::Docker);
    }
    for candidate in candidates() {
        if !found.contains(&candidate.runtime) && candidate.path.exists() {
            found.push(candidate.runtime);
        }
    }

    debug!(?found, "available runtimes");
    found
}

/// Where we found (or failed to find) a daemon.
#[derive(Debug, Clone)]
enum Endpoint {
    /// `DOCKER_HOST` was set; bollard parses the scheme itself.
    DockerHost(String),
    /// A unix socket on this machine.
    Socket(Candidate),
}

/// Resolve the daemon endpoint for `pref`:
///
/// 1. `DOCKER_HOST`, if set
/// 2. the first existing socket in [`candidates`] that `pref` admits
///
/// `DOCKER_HOST` is skipped entirely when the user has explicitly asked for
/// Podman: choosing a runtime in the UI is a newer and more specific intent
/// than an environment variable named after the other one.
///
/// Note `DOCKER_HOST` is *not* a filesystem path — it's a URL that may be
/// `unix://`, `tcp://` or `ssh://`. We hand it to bollard rather than parsing it
/// ourselves, because bollard already routes on the scheme.
fn resolve_endpoint(pref: RuntimePreference) -> Result<Endpoint> {
    if pref.admits(Runtime::Docker)
        && let Some(host) = docker_host()
    {
        info!(%host, "using DOCKER_HOST");
        return Ok(Endpoint::DockerHost(host));
    }

    for candidate in candidates() {
        if !pref.admits(candidate.runtime) {
            continue;
        }
        if candidate.path.exists() {
            info!(
                path = %candidate.path.display(),
                runtime = candidate.runtime.label(),
                "found socket"
            );
            return Ok(Endpoint::Socket(candidate));
        }
        debug!(path = %candidate.path.display(), "no socket here");
    }

    anyhow::bail!(nothing_found(pref))
}

/// The "we looked everywhere" message, listing exactly where we looked and how
/// to fix it for whichever runtime was asked for.
fn nothing_found(pref: RuntimePreference) -> String {
    let checked: Vec<String> = candidates()
        .iter()
        .filter(|candidate| pref.admits(candidate.runtime))
        .map(|candidate| format!("    {}", candidate.path.display()))
        .collect();

    let (what, remedy) = match pref {
        RuntimePreference::Docker => ("Docker", "    sudo systemctl enable --now docker.socket"),
        RuntimePreference::Podman => ("Podman", "    systemctl --user enable --now podman.socket"),
        RuntimePreference::Auto => (
            "container runtime",
            "    sudo systemctl enable --now docker.socket\nor:\n    \
             systemctl --user enable --now podman.socket",
        ),
    };

    // DOCKER_HOST isn't consulted for an explicit Podman preference, so don't
    // claim we checked it.
    let env_note = if pref.admits(Runtime::Docker) {
        "$DOCKER_HOST and "
    } else {
        ""
    };

    format!(
        "No {what} socket found. Checked {env_note}:\n{}\n\nIs it installed and running? Try:\n{remedy}",
        checked.join("\n")
    )
}

/// Turn a failed connection into something the user can act on.
///
/// Called only on the error path. The overwhelmingly common cause is that the
/// socket is `srw-rw---- root docker` and the user isn't in the `docker` group:
/// discovery succeeds, then the connect returns `EACCES`. Saying "Docker isn't
/// reachable" there would be useless, so we probe the socket directly to read
/// the real errno.
fn diagnose(candidate: &Candidate) -> String {
    let path = candidate.path.display();

    match UnixStream::connect(&candidate.path) {
        // We can open the socket, so the daemon itself is the problem.
        Ok(_) => format!(
            "The socket at {path} accepted a connection, but the daemon didn't respond.\n\n\
             Try:\n{}",
            status_hint(candidate)
        ),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => permission_hint(candidate),
        Err(err) if err.kind() == ErrorKind::ConnectionRefused => format!(
            "Nothing is listening on {path}.\n\nThe {} daemon looks stopped. Try:\n{}",
            candidate.runtime.label(),
            start_hint(candidate)
        ),
        Err(err) => format!("Can't connect to {path}: {err}"),
    }
}

/// How to ask this runtime what it thinks it's doing.
fn status_hint(candidate: &Candidate) -> &'static str {
    match (candidate.runtime, candidate.rootless) {
        (Runtime::Docker, true) => "    systemctl --user status docker",
        (Runtime::Docker, false) => "    systemctl status docker",
        (Runtime::Podman, true) => "    systemctl --user status podman.socket",
        (Runtime::Podman, false) => "    sudo systemctl status podman.socket",
    }
}

/// How to start this runtime's socket. Podman's is famously *not* enabled by
/// default, which makes this the likeliest first-run failure on a Podman box.
fn start_hint(candidate: &Candidate) -> &'static str {
    match (candidate.runtime, candidate.rootless) {
        (Runtime::Docker, true) => "    systemctl --user start docker.socket",
        (Runtime::Docker, false) => "    sudo systemctl start docker.socket",
        (Runtime::Podman, true) => "    systemctl --user enable --now podman.socket",
        (Runtime::Podman, false) => "    sudo systemctl enable --now podman.socket",
    }
}

/// `EACCES` means something different for each runtime, so the remedy does too.
fn permission_hint(candidate: &Candidate) -> String {
    let path = candidate.path.display();

    match candidate.runtime {
        Runtime::Docker => format!(
            "No permission to access {path}.\n\nYou're probably not in the `docker` group. \
             Add yourself, then log out and back in:\n    \
             sudo usermod -aG docker $USER"
        ),
        // Podman's rootless socket is owned by you, so a denial almost always
        // means we reached the *system* socket instead. Point at the rootless
        // one rather than suggesting sudo — rootless is the point of Podman.
        Runtime::Podman => format!(
            "No permission to access {path}.\n\nThat's Podman's system socket, which belongs \
             to root. Rootless Podman is the usual setup:\n    \
             systemctl --user enable --now podman.socket"
        ),
    }
}

/// Which runtime is behind a connection, and what version it reports.
///
/// Asked rather than inferred: Podman's `/version` lists a component named
/// "Podman Engine" (Docker's says just "Engine"), and that holds even when
/// `podman-docker` has put Podman behind Docker's socket path. Verified against
/// the real payloads of Docker 29.6.2 and Podman 6.0.1.
fn identify(version: &SystemVersion) -> (Runtime, String) {
    let podman_engine = version
        .components
        .iter()
        .flatten()
        .find(|component| component.name.contains("Podman"));

    let runtime = if podman_engine.is_some() {
        Runtime::Podman
    } else {
        Runtime::Docker
    };

    // Both runtimes fill the top-level `Version`; the component is a fallback
    // for a daemon that reports one but not the other.
    let reported = version
        .version
        .clone()
        .or_else(|| podman_engine.map(|component| component.version.clone()))
        .unwrap_or_else(|| "unknown".to_owned());

    (runtime, reported)
}

/// Connect to the runtime `pref` asks for, and verify the daemon actually
/// answers.
///
/// bollard's `connect_with_*` constructors are lazy — they build a client
/// without touching the socket, so a dead daemon or a permission problem would
/// otherwise stay invisible until the first real request. `ping()` forces the
/// round trip, which is what lets the UI tell "connected" apart from "holding a
/// handle to nothing".
///
/// No API-version negotiation: bollard defaults to v1.53 and Podman advertises
/// v1.44, but Podman accepts the newer path prefix regardless (checked against
/// 6.0.1). If a call ever fails on version grounds, `Docker::negotiate_version`
/// is the lever.
pub async fn connect(pref: RuntimePreference) -> Result<Connection> {
    let endpoint = resolve_endpoint(pref)?;

    let docker = match &endpoint {
        Endpoint::DockerHost(host) => Docker::connect_with_defaults()
            .with_context(|| format!("DOCKER_HOST is set to `{host}`, but that isn't usable"))?,
        Endpoint::Socket(candidate) => {
            let path = candidate
                .path
                .to_str()
                .context("socket path isn't valid UTF-8")?;
            Docker::connect_with_socket(path, 120, bollard::API_DEFAULT_VERSION)
                .with_context(|| format!("failed to build a client for {path}"))?
        }
    };

    if let Err(err) = docker.ping().await {
        warn!(?err, "ping failed");
        // For a local socket we can read the real errno; for DOCKER_HOST we only
        // have bollard's error to go on.
        return Err(match &endpoint {
            Endpoint::Socket(candidate) => anyhow::anyhow!(diagnose(candidate)),
            Endpoint::DockerHost(host) => {
                anyhow::anyhow!("Couldn't reach a container runtime at `{host}`: {err}")
            }
        });
    }

    // `ping` only proves *something* answered. Ask what it is, so the UI can
    // say "Podman 6.0.1" honestly instead of guessing from the path.
    let (runtime, version) = match docker.version().await {
        Ok(reported) => identify(&reported),
        Err(err) => {
            // It answered `ping` a moment ago, so refusing to connect over a
            // missing version string would be the worse failure. Fall back to
            // the socket's guess and say so in the log.
            warn!(
                ?err,
                "version() failed; falling back to the socket's runtime"
            );
            let guess = match &endpoint {
                Endpoint::Socket(candidate) => candidate.runtime,
                Endpoint::DockerHost(_) => Runtime::Docker,
            };
            (guess, "unknown".to_owned())
        }
    };

    info!(runtime = runtime.label(), %version, "connected");
    Ok(Connection {
        docker,
        runtime,
        version,
    })
}

/// List all containers, including stopped ones.
pub async fn list_containers(docker: &Docker) -> Result<Vec<Container>> {
    let options = ListContainersOptionsBuilder::default().all(true).build();

    let summaries = docker
        .list_containers(Some(options))
        .await
        .map_err(rejected)?;

    let mut containers: Vec<Container> = summaries
        .into_iter()
        .filter_map(Container::from_summary)
        .collect();

    // Docker returns newest-first; a list that reorders under the cursor on every
    // 2s poll is worse than one that's stably alphabetical.
    containers.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(containers)
}

pub async fn start_container(docker: &Docker, id: &str) -> Result<()> {
    docker
        .start_container(id, None::<StartContainerOptions>)
        .await
        .map_err(rejected)
}

pub async fn stop_container(docker: &Docker, id: &str) -> Result<()> {
    docker
        .stop_container(id, None::<StopContainerOptions>)
        .await
        .map_err(rejected)
}

pub async fn restart_container(docker: &Docker, id: &str) -> Result<()> {
    docker
        .restart_container(id, None::<RestartContainerOptions>)
        .await
        .map_err(rejected)
}

/// Remove a container. Not forced: removing a running container should fail
/// loudly rather than silently killing it.
pub async fn remove_container(docker: &Docker, id: &str) -> Result<()> {
    docker
        .remove_container(id, None::<RemoveContainerOptions>)
        .await
        .map_err(rejected)
}

/// Fetch the extra detail the list doesn't carry (start time, command, ...).
pub async fn inspect(docker: &Docker, id: &str) -> Result<ContainerDetail> {
    let resp = docker
        .inspect_container(id, None::<InspectContainerOptions>)
        .await
        .map_err(rejected)?;
    Ok(ContainerDetail::from_inspect(resp))
}

/// Stream live resource samples (CPU %, memory) for a running container.
///
/// Like `logs`, this borrows `docker` and `id`; the consuming block owns them.
/// Incomplete frames — the first one, before Docker has two readings to diff —
/// are filtered out (`Stats::from_response` returns `None`), so the graphs only
/// ever see real samples.
pub fn stats<'a>(
    docker: &'a Docker,
    id: &'a str,
) -> impl Stream<Item = Result<Stats, String>> + Send + 'a {
    let options = StatsOptionsBuilder::default().stream(true).build();

    docker
        .stats(id, Some(options))
        .filter_map(|item| async move {
            match item {
                Ok(resp) => Stats::from_response(resp).map(Ok),
                Err(err) => Some(Err(short_reason(&err))),
            }
        })
}

/// Stream a container's logs: the last 200 lines, then live output as it
/// arrives (`follow`).
///
/// The bollard `LogOutput`/`Error` types are mapped to our own `String`/`String`
/// *here*, so the UI never touches a bollard type (rule 3). Each `Ok(String)`
/// is a decoded chunk — Docker frames these however it likes, so a chunk isn't
/// necessarily a whole line; the view just appends them in order.
///
/// The returned stream borrows `docker` and `id`, so the caller keeps both
/// alive for as long as it polls — which is exactly what happens when the
/// consuming `async` block owns them (see `components/logs_view.rs`). It can't
/// return an owned `'static` stream without either a self-referential struct or
/// an extra dependency, and borrowing costs neither.
pub fn logs<'a>(
    docker: &'a Docker,
    id: &'a str,
) -> impl Stream<Item = Result<String, String>> + Send + 'a {
    // Always ask Docker to prepend its own RFC3339 timestamp. The view parses
    // it off and shows it only when asked — so the timestamp toggle is pure
    // presentation and never has to restart the stream.
    let options = LogsOptionsBuilder::default()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .timestamps(true)
        .tail("200")
        .build();

    docker.logs(id, Some(options)).map(|item| {
        item.map(|output| String::from_utf8_lossy(&output.into_bytes()).into_owned())
            .map_err(|err| short_reason(&err))
    })
}

/// Log the whole failure, hand back only the part worth showing.
fn rejected(err: BollardError) -> anyhow::Error {
    warn!(?err, "docker rejected the request");
    anyhow::anyhow!(short_reason(&err))
}

/// Reduce a Docker failure to something that fits on a toast.
///
/// Three layers conspire to make these unreadable. Docker writes for a
/// terminal, quoting the full 64-character id back at you and spelling out the
/// remedy; bollard prefixes `Docker responded with status code 409:`; and we
/// used to add our own context on top. That's ~230 characters, and an
/// `adw::Toast` is a single truncating line.
///
/// The caller already knows which container and which action — all that's
/// missing is *why*. The full error still goes to the log via `rejected`.
fn short_reason(err: &BollardError) -> String {
    match err {
        // The container was removed between the poll that drew the row and the
        // click on it. Routine rather than exceptional (CLAUDE.md rule 5), and
        // Docker's own wording here is a wall of id.
        BollardError::DockerResponseServerError {
            status_code: 404, ..
        } => "it no longer exists".to_owned(),

        BollardError::DockerResponseServerError { message, .. } => reason_clause(message),

        // Transport-level: the daemon died, the socket went away. Nothing in
        // bollard's text is useful to a person.
        _ => "Docker isn't responding".to_owned(),
    }
}

/// Pull the reason out of Docker's `<action> "<id>": <reason>: <remedy>`.
///
/// Falls back to the whole message when it isn't shaped that way — a long toast
/// beats a wrong one.
fn reason_clause(message: &str) -> String {
    let after_id = message.split_once("\": ").map_or(message, |(_, rest)| rest);
    after_id
        .split(':')
        .next()
        .unwrap_or(after_id)
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::SystemVersionComponents;

    /// A `/version` payload with the fields `identify` actually reads.
    fn version_payload(top_level: Option<&str>, components: &[(&str, &str)]) -> SystemVersion {
        SystemVersion {
            version: top_level.map(str::to_owned),
            components: Some(
                components
                    .iter()
                    .map(|(name, version)| SystemVersionComponents {
                        name: (*name).to_owned(),
                        version: (*version).to_owned(),
                        details: None,
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn identifies_podman_by_its_engine_component() {
        // Captured from the real rootless Podman 6.0.1 socket.
        let reported = version_payload(
            Some("6.0.1"),
            &[
                ("Podman Engine", "6.0.1"),
                ("Conmon", "conmon version 2.2.1"),
                ("OCI Runtime (runc)", "runc version 1.5.1"),
            ],
        );
        assert_eq!(identify(&reported), (Runtime::Podman, "6.0.1".to_owned()));
    }

    #[test]
    fn identifies_docker_by_the_absence_of_podman() {
        // Captured from the real Docker 29.6.2 socket. Its engine component is
        // named just "Engine", so "no Podman Engine" is the whole test.
        let reported = version_payload(
            Some("29.6.2"),
            &[
                ("Engine", "29.6.2"),
                ("containerd", "2.2.1"),
                ("runc", "1.5.1"),
            ],
        );
        assert_eq!(identify(&reported), (Runtime::Docker, "29.6.2".to_owned()));
    }

    #[test]
    fn falls_back_to_the_component_version() {
        // A daemon that fills Components but not the top-level Version.
        let reported = version_payload(None, &[("Podman Engine", "5.2.0")]);
        assert_eq!(identify(&reported), (Runtime::Podman, "5.2.0".to_owned()));
    }

    #[test]
    fn an_empty_version_payload_is_not_fatal() {
        // Whatever this is, it answered `ping`. Assume Docker (the compatible
        // API) and say the version is unknown rather than refusing to connect.
        let reported = SystemVersion::default();
        assert_eq!(identify(&reported), (Runtime::Docker, "unknown".to_owned()));
    }

    #[test]
    fn discovery_is_runtime_major_not_scope_major() {
        // The regression this ordering exists to prevent: on a machine with
        // rootful Docker and rootless Podman, a scope-major order would find
        // podman.sock first and silently swap the user's containers out.
        let paths: Vec<String> = candidates_in(Some(PathBuf::from("/run/user/1000")))
            .iter()
            .map(|candidate| candidate.path.display().to_string())
            .collect();

        assert_eq!(
            paths,
            vec![
                "/run/user/1000/docker.sock",
                "/var/run/docker.sock",
                "/run/user/1000/podman/podman.sock",
                "/run/podman/podman.sock",
            ]
        );
    }

    #[test]
    fn without_xdg_runtime_dir_only_the_system_sockets_remain() {
        let candidates = candidates_in(None);
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| !candidate.rootless));
    }

    #[test]
    fn preference_keys_round_trip() {
        for pref in [
            RuntimePreference::Auto,
            RuntimePreference::Docker,
            RuntimePreference::Podman,
        ] {
            assert_eq!(RuntimePreference::from_key(pref.as_key()), pref);
        }
    }

    #[test]
    fn an_unreadable_preference_falls_back_to_auto() {
        // A hand-edited config file shouldn't be able to wedge the app.
        assert_eq!(RuntimePreference::from_key("crio"), RuntimePreference::Auto);
        assert_eq!(RuntimePreference::from_key(""), RuntimePreference::Auto);
    }

    #[test]
    fn an_explicit_preference_admits_only_its_own_runtime() {
        assert!(RuntimePreference::Auto.admits(Runtime::Docker));
        assert!(RuntimePreference::Auto.admits(Runtime::Podman));
        assert!(RuntimePreference::Docker.admits(Runtime::Docker));
        assert!(!RuntimePreference::Docker.admits(Runtime::Podman));
        assert!(RuntimePreference::Podman.admits(Runtime::Podman));
        assert!(!RuntimePreference::Podman.admits(Runtime::Docker));
    }

    #[test]
    fn the_not_found_message_names_the_right_remedy() {
        let podman = nothing_found(RuntimePreference::Podman);
        assert!(podman.contains("podman.socket"), "{podman}");
        assert!(!podman.contains("docker"), "{podman}");
        // DOCKER_HOST isn't consulted for an explicit Podman preference, so the
        // message mustn't claim we looked at it.
        assert!(!podman.contains("DOCKER_HOST"), "{podman}");

        let docker = nothing_found(RuntimePreference::Docker);
        assert!(docker.contains("docker.socket"), "{docker}");
        assert!(docker.contains("DOCKER_HOST"), "{docker}");
        assert!(!docker.contains("podman"), "{docker}");
    }

    #[test]
    fn the_remedy_differs_per_runtime_and_scope() {
        let rootless_podman = Candidate {
            path: PathBuf::from("/run/user/1000/podman/podman.sock"),
            runtime: Runtime::Podman,
            rootless: true,
        };
        // Podman's socket isn't enabled by default, so "enable --now" (not
        // "start") is the remedy that actually sticks.
        assert_eq!(
            start_hint(&rootless_podman),
            "    systemctl --user enable --now podman.socket"
        );
        // And it's the user's own socket — never suggest sudo for it.
        assert!(!permission_hint(&rootless_podman).contains("sudo"));

        let rootful_docker = Candidate {
            path: PathBuf::from(DOCKER_ROOTFUL_SOCKET),
            runtime: Runtime::Docker,
            rootless: false,
        };
        assert!(permission_hint(&rootful_docker).contains("usermod -aG docker"));
    }

    fn server_error(status_code: u16, message: &str) -> BollardError {
        BollardError::DockerResponseServerError {
            status_code,
            message: message.to_owned(),
        }
    }

    #[test]
    fn shortens_the_real_remove_refusal() {
        // Captured verbatim from the daemon. Note it quotes the full 64-char id
        // back, which is most of why the untrimmed toast ran to 234 characters.
        let err = server_error(
            409,
            "cannot remove container \
             \"f1635166cbf3f8c5a8a8ac3e39ab838f11cd610383bf6e0b8e3aabe1de1b0646\": \
             container is running: stop the container before removing or force remove",
        );
        assert_eq!(short_reason(&err), "container is running");
    }

    #[test]
    fn a_missing_container_is_not_worth_quoting_docker_over() {
        let err = server_error(404, "No such container: dockyard-test");
        assert_eq!(short_reason(&err), "it no longer exists");
    }

    #[test]
    fn a_500_still_shows_the_daemon_message() {
        // A server error carries a message, so it goes through the clause path
        // rather than the catch-all — the daemon said something, show it.
        let err = server_error(500, "server error");
        assert_eq!(short_reason(&err), "server error");
    }

    #[test]
    fn transport_failures_dont_leak_bollard_internals() {
        // Not a DockerResponseServerError at all — the daemon never answered,
        // so there's no message worth a person's time.
        let err = BollardError::APIVersionParseError {};
        assert_eq!(short_reason(&err), "Docker isn't responding");
    }

    #[test]
    fn keeps_the_whole_message_when_it_isnt_the_expected_shape() {
        // A long toast beats a wrong one.
        let err = server_error(409, "something we have never seen before");
        assert_eq!(short_reason(&err), "something we have never seen before");
    }

    #[test]
    fn reason_clause_drops_the_id_and_the_remedy() {
        assert_eq!(
            reason_clause("cannot remove container \"abc\": container is running: stop it first"),
            "container is running"
        );
    }

    #[test]
    fn reason_clause_survives_a_message_with_no_id() {
        assert_eq!(
            reason_clause("driver failed programming"),
            "driver failed programming"
        );
    }

    #[test]
    fn the_result_actually_fits_a_toast() {
        let err = server_error(
            409,
            "cannot remove container \
             \"f1635166cbf3f8c5a8a8ac3e39ab838f11cd610383bf6e0b8e3aabe1de1b0646\": \
             container is running: stop the container before removing or force remove",
        );
        // "Couldn't remove inventory_pos_db: " is ~33 chars; an adw::Toast
        // truncates around 60-70 in a 540px window.
        assert!(
            short_reason(&err).len() <= 30,
            "reason too long for a toast: {:?}",
            short_reason(&err)
        );
    }
}
