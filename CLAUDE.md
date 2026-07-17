# CLAUDE.md

Project instructions for Claude Code. Read this fully before writing code.

## What this is

A small, native GNOME desktop app to manage Docker containers on a personal Linux
laptop. Not a product, not multi-user, not cross-platform. One user, one machine.

The app should be indistinguishable from a first-party GNOME application. If a
design decision would make it look like an Electron app or a generic Qt tool, it
is the wrong decision.

## Author context — read this, it changes how you should respond

The author is a senior frontend engineer (~10 years: TypeScript, React, React
Native, Node) who is **new to Rust**. Consequences:

- When you introduce ownership, borrowing, lifetimes, `Rc`/`Arc`/`RefCell`, or
  `async` pinning, **briefly explain why** in a comment or in your reply. Do not
  silently sprinkle `.clone()` to make the borrow checker quiet — say what the
  ownership problem was and why the clone is the right or pragmatic fix.
- Analogies to React/Redux are welcome and land well. relm4 *is* the Elm
  architecture; say so.
- Do not dumb down the Rust. Idiomatic code with explanation, not beginner code.
- Prefer clarity over cleverness. No macro tricks, no premature generics.

## Stack (pinned — do not swap these out)

| Layer          | Crate                    | Version |
| -------------- | ------------------------ | ------- |
| UI framework   | `relm4`                  | 0.11    |
| Widgets        | `gtk4`, `libadwaita`     | 0.11 / 0.9 (transitively via relm4) |
| Docker client  | `bollard`                | 0.21    |
| Async runtime  | `tokio`                  | 1       |
| Streams        | `futures-util`           | 0.3     |
| Logging        | `tracing`                | 0.1     |

Rust edition 2024.

Enable relm4's `libadwaita` feature. Do **not** add `gtk4` or `libadwaita` as
direct dependencies with independent versions — take them through relm4 so the
versions can't drift apart.

**relm4, not raw gtk4-rs.** Every component is a relm4 `Component` or
`FactoryComponent`. If you find yourself reaching for `Rc<RefCell<>>` to share
widget state, stop — that's a sign the state belongs in a model and the change
belongs in an `update()`.

## Hard rules

### 1. Never trust your training data for the bollard API

bollard's options API changed in **0.19**. The old form:

```rust
// WRONG — deprecated since 0.19, will be removed
use bollard::container::ListContainersOptions;
let opts = Some(ListContainersOptions { all: true, ..Default::default() });
```

The current form is OpenAPI-generated builders under `bollard::query_parameters`:

```rust
// RIGHT
use bollard::query_parameters::ListContainersOptionsBuilder;
let opts = ListContainersOptionsBuilder::default().all(true).build();
docker.list_containers(Some(opts)).await?;
```

Response/body types live in `bollard::models` (e.g. `ContainerSummary`,
`ContainerCreateBody`). Do not depend on `bollard-stubs` directly.

**Before writing any bollard call you haven't already written in this repo,
check https://docs.rs/bollard/latest/ for the real signature.** Most bollard
examples on the internet predate 0.19. If you emit a deprecated form and
`cargo build` warns, fix it rather than leaving the warning.

### 2. Socket discovery — never hardcode `/var/run/docker.sock`

The target machine may run rootless Docker, where the socket is at
`$XDG_RUNTIME_DIR/docker.sock`. Resolution order:

1. `DOCKER_HOST` env var, if set
2. `$XDG_RUNTIME_DIR/docker.sock`, if it exists
3. `/var/run/docker.sock`

Put this in one function in `docker/client.rs`. If none resolve, the app must
show an `adw::StatusPage` explaining that Docker isn't reachable — never panic,
never `.unwrap()` on the connection.

### 3. bollard types must not leak into the UI

Map `bollard::models::ContainerSummary` into our own `Container` struct in
`docker/types.rs` at the boundary. Reasons: bollard's generated types are a
swamp of `Option<Vec<Option<String>>>`, and the `view!` macro is miserable to
write against them. The UI layer should only ever see our types.

### 4. Never block the GTK main thread

All Docker I/O goes through relm4 `Command`s (`oneshot_command` for one-shot
calls, `command` for streams). The `update()` function stays synchronous and
fast. If a Docker call takes 3 seconds, the window must still drag smoothly.

### 5. No `.unwrap()` / `.expect()` outside `main.rs` and tests

Docker calls fail routinely: daemon down, container already stopped, container
removed between poll and click. Every failure becomes an `adw::Toast`, not a
crash. Use `anyhow::Result` internally.

## Architecture

```
src/
  main.rs              # RelmApp bootstrap, adw init, tracing init
  app.rs               # root Component: AppModel, AppMsg, update, view
  docker/
    mod.rs
    client.rs          # socket discovery, Docker handle, thin async wrappers
    types.rs           # our Container / ContainerState / Port structs
  components/
    container_list.rs  # FactoryVecDeque<ContainerRow>
    container_row.rs   # FactoryComponent -> adw::ActionRow
    logs_page.rs       # streaming log view
data/
  dev.miguelrincon.Dockyard.desktop.in
  icons/
```

The root model is roughly:

```rust
struct AppModel {
    docker: Option<Docker>,          // None = not connected
    containers: FactoryVecDeque<ContainerRow>,
    state: ViewState,                // Loading | Ready | Disconnected(String)
}

enum AppMsg {
    Refresh,
    ContainersLoaded(Vec<Container>),
    Start(String),                   // container id
    Stop(String),
    Restart(String),
    Remove(String),
    ShowLogs(String),
    Error(String),
}
```

This is Redux with a compiler. Actions in, single reducer, view derived from state.

## UI shape

- `adw::ApplicationWindow` > `adw::ToolbarView` > `adw::HeaderBar`
- Main content: `adw::NavigationView`. Root page = container list, push a detail
  page for logs.
- Each container is an `adw::ActionRow`: title = name, subtitle = image + status,
  a status dot prefix, a start/stop button suffix, and a menu button for
  restart/remove.
- Empty state and disconnected state: `adw::StatusPage`.
- Errors: `adw::ToastOverlay` wrapping the content.
- **Use libadwaita widgets, not raw GTK equivalents.** `adw::ActionRow` over a
  hand-built `gtk::Box`; `adw::PreferencesGroup` over a labelled frame. That's
  where the native feel comes from — accent colour, dark mode, and adaptive
  layout are then free.
- No custom CSS unless there is no libadwaita widget for the job. If you think
  you need custom CSS, say why first.

## Refresh strategy

**Phase 1: poll.** `glib::timeout` every 2s → `AppMsg::Refresh` → list containers.
Boring, correct, ~20 lines.

**Phase 2 (later): events.** `docker.events()` returns a stream; subscribe via a
relm4 `command` and push messages on container start/stop/die. Do not build this
until polling works end to end.

## Scope

In scope for v1:

- List containers (name, image, status, ports), including stopped ones
- Start / stop / restart / remove
- View logs (`docker.logs()` with `follow: true`, tail 200)

**Explicitly out of scope** — do not build these, do not scaffold for them:
image builds, `docker compose`, volumes, networks, registries, `exec` into a
container, resource graphs, multi-host.

If a change looks like it's growing toward Docker Desktop, push back.

## Commands

```bash
cargo run                  # dev
cargo build --release
cargo clippy -- -D warnings   # must pass clean before any commit
cargo fmt
```

System deps (CachyOS / Arch):

```bash
sudo pacman -S --needed base-devel pkgconf rust gtk4 libadwaita librsvg
```

The Docker daemon is socket-activated: `sudo systemctl enable --now docker.socket`.
For rootless: `systemctl --user enable --now docker`.

## Conventions

- `cargo clippy -- -D warnings` is the bar, not `cargo build`.
- Commits: conventional commits (`feat:`, `fix:`, `refactor:`).
- App ID: `dev.miguelrincon.Dockyard`. It must match the `.desktop` file name,
  the GResource prefix (`/dev/miguelrincon/Dockyard/`), and `RelmApp::new()`.
  The app is called **Dockyard** — use that in the window title and `.desktop`
  `Name=`, not "Docker Manager".
- No Flatpak packaging for now. A Flatpak sandbox can't see the Docker socket
  without a `--filesystem` hole that defeats the point of the sandbox. Plain
  `cargo build --release` plus a `.desktop` file is the target.

## When you're unsure

Ask before: adding a dependency, introducing a new module, or deviating from the
relm4 component model. Don't ask before: fixing a clippy lint, adding a doc
comment, or checking docs.rs.
