//! Socket discovery and thin async wrappers around bollard.
//!
//! Everything that knows a socket path lives here (CLAUDE.md rule 2).

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::errors::Error as BollardError;
use bollard::query_parameters::{
    ListContainersOptionsBuilder, RemoveContainerOptions, RestartContainerOptions,
    StartContainerOptions, StopContainerOptions,
};
use tracing::{debug, info, warn};

use super::types::Container;

const ROOTFUL_SOCKET: &str = "/var/run/docker.sock";

/// Where we found (or failed to find) the daemon.
#[derive(Debug, Clone)]
enum Endpoint {
    /// `DOCKER_HOST` was set; bollard parses the scheme itself.
    DockerHost(String),
    /// A unix socket on this machine.
    Socket(PathBuf),
}

/// Resolve the daemon endpoint, in the order specified by CLAUDE.md rule 2:
///
/// 1. `DOCKER_HOST`, if set
/// 2. `$XDG_RUNTIME_DIR/docker.sock` (rootless), if it exists
/// 3. `/var/run/docker.sock` (rootful)
///
/// Note that `DOCKER_HOST` is *not* a filesystem path — it's a URL that may be
/// `unix://`, `tcp://` or `ssh://`. We hand it to bollard rather than parsing it
/// ourselves, because bollard already routes on the scheme.
fn resolve_endpoint() -> Result<Endpoint> {
    if let Ok(host) = std::env::var("DOCKER_HOST")
        && !host.is_empty()
    {
        info!(%host, "using DOCKER_HOST");
        return Ok(Endpoint::DockerHost(host));
    }

    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let rootless = Path::new(&runtime_dir).join("docker.sock");
        if rootless.exists() {
            info!(path = %rootless.display(), "found rootless socket");
            return Ok(Endpoint::Socket(rootless));
        }
        debug!(path = %rootless.display(), "no rootless socket, trying rootful");
    }

    let rootful = PathBuf::from(ROOTFUL_SOCKET);
    if rootful.exists() {
        info!(path = %rootful.display(), "found rootful socket");
        return Ok(Endpoint::Socket(rootful));
    }

    anyhow::bail!(
        "No Docker socket found. Checked $DOCKER_HOST, $XDG_RUNTIME_DIR/docker.sock \
         and {ROOTFUL_SOCKET}.\n\nIs Docker installed and running? Try:\n    \
         sudo systemctl enable --now docker.socket"
    )
}

/// Turn a failed connection to `path` into something the user can act on.
///
/// Called only on the error path. The overwhelmingly common cause is that the
/// socket is `srw-rw---- root docker` and the user isn't in the `docker` group:
/// discovery succeeds, then the connect returns `EACCES`. Saying "Docker isn't
/// reachable" there would be useless, so we probe the socket directly to read
/// the real errno.
fn diagnose(path: &Path) -> String {
    match UnixStream::connect(path) {
        // We can open the socket, so the daemon itself is the problem.
        Ok(_) => format!(
            "Docker's socket at {} accepted a connection, but the daemon didn't respond.\n\n\
             Try:\n    systemctl status docker",
            path.display()
        ),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => format!(
            "No permission to access {}.\n\nYou're probably not in the `docker` group. \
             Add yourself, then log out and back in:\n    \
             sudo usermod -aG docker $USER",
            path.display()
        ),
        Err(err) if err.kind() == ErrorKind::ConnectionRefused => format!(
            "Nothing is listening on {}.\n\nThe Docker daemon looks stopped. Try:\n    \
             sudo systemctl start docker.socket",
            path.display()
        ),
        Err(err) => format!("Can't connect to {}: {err}", path.display()),
    }
}

/// Connect to Docker and verify the daemon actually answers.
///
/// bollard's `connect_with_*` constructors are lazy — they build a client
/// without touching the socket, so a dead daemon or a permission problem would
/// otherwise stay invisible until the first real request. `ping()` forces the
/// round trip, which is what lets the UI tell "connected" apart from "holding a
/// handle to nothing".
pub async fn connect() -> Result<Docker> {
    let endpoint = resolve_endpoint()?;

    let docker = match &endpoint {
        Endpoint::DockerHost(host) => Docker::connect_with_defaults()
            .with_context(|| format!("DOCKER_HOST is set to `{host}`, but that isn't usable"))?,
        Endpoint::Socket(path) => {
            let path_str = path.to_str().context("socket path isn't valid UTF-8")?;
            Docker::connect_with_socket(path_str, 120, bollard::API_DEFAULT_VERSION)
                .with_context(|| format!("failed to build a client for {}", path.display()))?
        }
    };

    if let Err(err) = docker.ping().await {
        warn!(?err, "ping failed");
        // For a local socket we can read the real errno; for DOCKER_HOST we only
        // have bollard's error to go on.
        return Err(match &endpoint {
            Endpoint::Socket(path) => anyhow::anyhow!(diagnose(path)),
            Endpoint::DockerHost(host) => {
                anyhow::anyhow!("Couldn't reach the Docker daemon at `{host}`: {err}")
            }
        });
    }

    info!("connected to Docker");
    Ok(docker)
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
