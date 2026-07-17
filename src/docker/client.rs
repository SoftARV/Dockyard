//! Socket discovery and thin async wrappers around bollard.
//!
//! Everything that knows a socket path lives here (CLAUDE.md rule 2).

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bollard::Docker;
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
        .context("couldn't list containers")?;

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
        .context("couldn't start the container")
}

pub async fn stop_container(docker: &Docker, id: &str) -> Result<()> {
    docker
        .stop_container(id, None::<StopContainerOptions>)
        .await
        .context("couldn't stop the container")
}

pub async fn restart_container(docker: &Docker, id: &str) -> Result<()> {
    docker
        .restart_container(id, None::<RestartContainerOptions>)
        .await
        .context("couldn't restart the container")
}

/// Remove a container. Not forced: removing a running container should fail
/// loudly rather than silently killing it.
pub async fn remove_container(docker: &Docker, id: &str) -> Result<()> {
    docker
        .remove_container(id, None::<RemoveContainerOptions>)
        .await
        .context("couldn't remove the container")
}
