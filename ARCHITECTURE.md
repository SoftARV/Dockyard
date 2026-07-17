# Dockyard — how it works

A native GNOME app to manage Docker containers on one Linux laptop.

This document is the map: what the pieces are, why they're shaped that way, and
what's built versus what isn't. `CLAUDE.md` is the *rulebook* (what we may and
may not do); this is the *explanation*.

It assumes you know TypeScript/React well and Rust not at all, so it leans on
that comparison where it genuinely helps and says so when the analogy breaks.

---

## Running it

```bash
cargo run                     # dev build, launches the window
RUST_LOG=debug cargo run      # same, with our tracing logs on stdout
cargo clippy -- -D warnings   # the bar that must pass before any commit
cargo fmt                     # format
cargo build --release         # optimised binary at target/release/dockyard
```

**Only one copy runs at a time.** GTK apps register their app ID on D-Bus, so a
second `cargo run` while the first is open silently hands off to the running
window and exits 0 — no error, no second window. If a run seems to do nothing,
that's why. Close the first window, or:

```bash
pkill -f target/debug/dockyard
```

Requirements: Rust ≥ 1.93, gtk4 ≥ 4.10, libadwaita ≥ 1.4, and a reachable Docker
daemon. On Arch/CachyOS:

```bash
sudo pacman -S --needed base-devel pkgconf gtk4 libadwaita librsvg
sudo systemctl enable --now docker.socket
```

---

## The big picture: Redux, with a compiler

relm4 *is* the Elm architecture, which is where Redux came from. The mapping is
close to exact:

| Redux / React        | relm4                        | Here                      |
| -------------------- | ---------------------------- | ------------------------- |
| store state          | the model struct             | `AppModel` (`src/app.rs`) |
| action               | the `Input` enum             | `AppMsg`                  |
| reducer              | `update()`                   | `AppModel::update`        |
| `useSelector` + JSX  | the `view!` macro            | `AppModel`'s `view!`      |
| thunk / saga         | a `Command`                  | `sender.oneshot_command`  |
| child `onChange`     | the `Output` enum            | `ContainerRowOutput`      |

Where the analogy breaks, and it matters:

- **The view is not re-run.** React re-renders a whole tree and diffs it. relm4
  builds real GTK widgets once, then mutates the specific ones you marked
  `#[watch]`. There's no virtual DOM and no reconciliation. That's why
  `#[watch] set_sensitive: model.docker.is_some()` exists — it says "re-run just
  this one setter when the model changes."
- **`update()` must not await.** Reducers are synchronous *and* run on the GTK
  main thread, which is also the thread painting your window. Blocking it for
  3 seconds freezes the UI (see "Never block the main thread" below).
- **Messages are typed and exhaustive.** A `match` on `AppMsg` that misses a
  variant won't compile. Redux's `default:` case doesn't exist here.

---

## Module map

```
src/
  main.rs                     bootstrap: tracing, RelmApp, app ID
  app.rs                      root Component — the store + reducer + view
  docker/
    client.rs                 socket discovery, connect, ping, API wrappers
    types.rs                  our Container/ContainerState/Port
  components/
    container_row.rs          FactoryComponent -> adw::ActionRow
```

The dependency direction is strictly one-way:

```
main.rs  ->  app.rs  ->  components/container_row.rs
                 \
                  ->  docker/client.rs  ->  docker/types.rs  ->  [bollard]
```

`components/` never imports `bollard`. That's the point of `docker/types.rs`.

---

## How a click becomes a Docker call

Worth tracing once, because every feature follows this path.

1. You click **Stop** on a row. The GTK button emits `clicked`.
2. `container_row.rs` turns it into an intent and sends it *up*:
   `sender.output(ContainerRowOutput::Stop(id))`. The row does **not** call
   Docker. It doesn't know Docker exists.
3. `app.rs`'s `.forward(...)` maps that output into the parent's action:
   `ContainerRowOutput::Stop(id) => AppMsg::Stop(id)`.
4. `AppModel::update` matches `AppMsg::Stop(id)` and calls `dispatch(...)`,
   which spawns a **command** — an async task on a worker thread — and returns
   immediately. The reducer is done in microseconds.
5. The task runs `client::stop_container(...).await` off the main thread, then
   sends back a `CommandMsg::ActionDone(result)`.
6. relm4 delivers that to `update_cmd` **on the main thread**, so it's safe to
   touch the model and widgets there. Failure becomes a toast.
7. The 2s poll picks up the container's new state and the row redraws.

The important shape: **rows emit intent, the root decides.** All Docker I/O
lives in one reducer, so there's exactly one place where actions are ordered and
errors are handled — the same reason you keep `fetch` out of React components.

---

## Rust things you'll hit in this codebase

### `.clone()` on the Docker handle is not a copy

In `dispatch` and `AppMsg::Refresh`:

```rust
let Some(docker) = self.docker.clone() else { return };
sender.oneshot_command(async move { ... });
```

An async task must be `'static` — it may outlive the function that spawned it,
so it cannot borrow `&self.docker`. It also must be `Send`, to move to another
thread. Cloning satisfies both.

This clone is cheap and intended: bollard's `Docker` is an `Arc`-backed handle
(atomically reference-counted pointer), so cloning bumps a counter rather than
copying a connection. Think of it as copying a reference to a shared object, not
`structuredClone`. When CLAUDE.md says "don't sprinkle `.clone()` to quiet the
borrow checker", this is the legitimate case: the ownership problem is real
(the task outlives the caller) and the clone is the correct fix, not a dodge.

### `Option` and `Result` are the same idea, made explicit

`Option<T>` is "T or nothing" (TS's `T | undefined`). `Result<T, E>` is "T or an
error" (a `Promise` that settles, but you must handle the rejection). Rust has
no `null` and no exceptions, so both are ordinary values you destructure.

`?` is the useful bit: it means "unwrap this, or return the error from this
function". `docker.list_containers(...).await?` is roughly `await`-plus-rethrow
in one character.

`let Some(x) = expr else { return };` is the early-exit form — "if this is
nothing, bail". You'll see it in `dispatch`: no daemon, nothing to do.

**`.unwrap()` is the one to avoid.** It means "give me the value, and if there
isn't one, crash the process". CLAUDE.md bans it outside `main.rs` because
Docker calls fail routinely — a container can be removed between the poll that
drew the row and your click on it. Those become toasts, not panics.

### The type boundary in `docker/types.rs`

bollard's generated `ContainerSummary` has every field optional, because the
Docker API says so:

```rust
pub struct ContainerSummary {
    pub id: Option<String>,
    pub names: Option<Vec<String>>,
    pub state: Option<ContainerSummaryStateEnum>,
    ...
}
```

Writing UI against that is miserable, so `Container::from_summary` resolves all
of it once — defaults applied, names de-slashed, ports deduped — and everything
downstream gets plain owned data. This is the "parse, don't validate" idea: do
the messy narrowing at the edge, and the inside of the app stops being defensive.

It returns `Option<Container>` because a summary with no id is unusable — every
action keys off the id — so those are dropped with `filter_map`.

---

## Linux and GTK things that aren't obvious

### The Docker socket

Docker isn't a library; it's a daemon you talk to over a unix socket with an
HTTP API. So "connect to Docker" means "find that socket".

There are two common installs:

- **rootful** — daemon runs as root, socket at `/var/run/docker.sock`, owned
  `root:docker` with mode `srw-rw----`. You reach it by being in the `docker`
  group. *This is your machine.*
- **rootless** — daemon runs as you, socket at `$XDG_RUNTIME_DIR/docker.sock`
  (i.e. `/run/user/1000/docker.sock`). No group needed.

`DOCKER_HOST` overrides both, and is **not a path** — it's a URL that may be
`unix://`, `tcp://` or `ssh://`. We hand it to bollard rather than parsing it,
because bollard already routes on the scheme. `client.rs::resolve_endpoint`
implements the whole order.

### Being in the `docker` group is root, effectively

Anyone in the `docker` group can start a container that mounts `/` and edit any
file on the machine. That's not a Dockyard thing — it's true of the `docker` CLI
too — but it's worth knowing that this app has that power because you do.

### Why we `ping()`

bollard's `connect_with_*` constructors are **lazy**: they build a client
without touching the socket. They return `Ok` for a TCP address with nothing
listening. Verified live — pointing `DOCKER_HOST` at a dead port still produced
a working `Docker` value, and only `ping()` caught it.

Without the ping, the app would render "Ready" with an empty list and look like
you own zero containers, which is a much worse lie than an error.

### Errors have to name the fix

The most likely failure on someone else's machine isn't a missing socket — it's
a socket that's *right there* and returns `EACCES` because they're not in the
`docker` group. Discovery succeeds; the connection doesn't.

So `client.rs::diagnose` probes the socket on the error path to read the real
errno and says `sudo usermod -aG docker $USER` instead of "Docker isn't
reachable". Same for a stopped daemon (`ECONNREFUSED`).

### Never block the main thread

GTK is single-threaded: one thread owns every widget and paints every frame. If
`update()` blocks for 3 seconds, the window doesn't redraw or drag for 3
seconds. Docker calls take unbounded time, so they *all* go through commands,
which run on tokio worker threads and post results back. That's what CLAUDE.md
rule 4 protects.

### libadwaita versions are a build-time floor

`adw::ToolbarView` and `adw::NavigationView` arrived in libadwaita **1.4**.
relm4 gates them behind its `gnome_45` feature; the default `gnome_42` doesn't
have them and the code won't compile. We pin `gnome_45` in `Cargo.toml` — the
minimum for the widgets CLAUDE.md mandates, which keeps the app buildable on
distros back to GNOME 45 (Sept 2023). Raising it raises what users need
installed.

Relatedly: **relm4 0.11's docs.rs build is broken**, so the web only documents
0.10 and some of it is wrong for us. The reliable reference is the vendored
source:

```bash
ls ~/.cargo/registry/src/*/relm4-0.11.0/src/
```

That's how we established that `RelmApp::new` already calls `adw::init()` (so
`main.rs` deliberately doesn't) and that `adw::PreferencesGroup` implements
`FactoryView` (so rows can live in one directly).

---

## Decisions worth remembering

| Decision | Why |
| --- | --- |
| App ID `dev.miguelrincon.Dockyard` | Chosen when the repo had no remote. `io.github.SoftARV.Dockyard` is now also defensible; changing it means updating `main.rs`, the `.desktop` name and the GResource prefix together. |
| Poll every 2s, don't use events | CLAUDE.md phase 1. Boring and correct. `docker.events()` comes only once polling works end to end. |
| Update rows in place; rebuild only when membership changes | The first cut rebuilt every row on every poll. That destroys widgets 30 times a minute, and an open popover — parented to a row's menu button — died with it. Cheapness was never the issue; rebuilding throws away interaction state. |
| `remove_container` isn't forced | Removing a running container should fail loudly rather than silently kill it. |
| Sort by name | Docker returns newest-first; a list that reorders under your cursor every 2s is worse than a stable one. |
| `tracing-subscriber` added | Not in CLAUDE.md's stack table, but `main.rs`'s job of "tracing init" is impossible without it. `env-filter` gives `RUST_LOG`. |

---

## Status and timeline

### Done — 17 Jul 2026

- **Scaffold.** Cargo project, pinned stack, `gnome_45`, `.gitignore`.
  Required updating rustup's `stable` from 1.91 → 1.97; relm4 0.11 needs ≥1.93.
- **Socket discovery** (`docker/client.rs`). All three branches exercised:
  `DOCKER_HOST`, rootless, rootful. Plus `ping()` verification and actionable
  errors for permission-denied / dead-daemon / no-socket. None panic.
- **Type boundary** (`docker/types.rs`). bollard stops here.
- **Container list.** `FactoryVecDeque<ContainerRow>` into an
  `adw::PreferencesGroup`, 2s poll, status icon, ports, start/stop button and a
  restart/remove menu.
- **Lifecycle actions.** start / stop / restart / remove, all off-thread, all
  failures becoming toasts.

`cargo clippy -- -D warnings` clean.

### Next

1. **Logs page** — the last v1 feature. `docker.logs()` with `follow: true`,
   tail 200, pushed onto an `adw::NavigationView`. This is the first thing
   needing `command` (a *stream*) rather than `oneshot_command` (one result),
   so it's the natural place to learn that half of the API.
2. **`.desktop` file** — `data/dev.miguelrincon.Dockyard.desktop.in` plus an
   icon, so it launches from the app grid rather than a terminal.
3. **Empty state** — an `adw::StatusPage` for "no containers", which currently
   renders as a blank group.

### Later, deliberately

4. **Events instead of polling** — `docker.events()` via a `command`. Only after
   polling is proven.

### Known rough edges

- `ContainerState::is_running()` counts `Restarting` as running, so the button
  offers "stop" mid-restart. Defensible, not thought through.
- `Remove` has no confirmation dialog. It's destructive and one click away.

### A trap worth knowing: `#[watch]` and widget lifetime

Rows persist across polls now, and two bugs were hiding behind the old rebuild:

- **Closures capture once.** `connect_clicked[running = ...]` freezes `running`
  at widget-build time. While rows were rebuilt every 2s that was invisible;
  the moment they persist, the button offers the wrong action forever. Route
  the click through an `Input` and read the model at click time instead.
- **`add_css_class` appends, `set_css_classes` replaces.** Under `#[watch]` the
  appending form accumulates, so a container that ran and then exited ends up
  styled `success` *and* `dim-label`.

The general lesson: rebuilding widgets hides staleness bugs. Stop rebuilding
and they all surface at once.
- The rootless socket path is only reachable on a rootless install; on this
  machine it's tested by faking `XDG_RUNTIME_DIR`.

### Out of scope — don't build these

Image builds, `docker compose`, volumes, networks, registries, `exec`, resource
graphs, multi-host. If a change starts growing toward Docker Desktop, stop.
