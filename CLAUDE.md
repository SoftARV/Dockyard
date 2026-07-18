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

Rust edition 2024. Plus `anyhow` 1 (rule 5) and `tracing-subscriber` 0.3 with
`env-filter` — `main.rs` can't init tracing without it, and it gives `RUST_LOG`.

`futures-util` is used by the logs stream (`StreamExt`/`FutureExt` in
`client.rs` and `logs_view.rs`). `tokio` is pinned but **not** used directly by
our code — relm4 owns the tokio runtime — and may never be.

Enable relm4's `libadwaita` **and `gnome_46`** features. Do **not** add `gtk4`
or `libadwaita` as direct dependencies with independent versions — take them
through relm4 so the versions can't drift apart.

`gnome_46` is a floor on what users must have installed, not a free upgrade.
relm4's `gnome_*` features decide which libadwaita widgets exist at all:

| relm4 feature | libadwaita | Notable |
| --- | --- | --- |
| `gnome_42` (default) | 1.0 | `ActionRow`, `StatusPage`, `Toast` |
| `gnome_45` | 1.4 | `ToolbarView`, `NavigationView` |
| **`gnome_46`** ← us | **1.5** | **`AlertDialog`** |
| `gnome_47` | 1.6 | `MessageDialog` becomes deprecated |

Raise it only for a widget you actually need, and say so.

**relm4 0.11's docs.rs build is broken** — the site only documents 0.10, and
parts of it are wrong for us. Read the vendored source instead; it's the exact
version we compile against:

```bash
ls ~/.cargo/registry/src/*/relm4-0.11.0/src/
```

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

Two things learned building this, both now in `client.rs`:

- **`DOCKER_HOST` is a URL, not a path.** It may be `unix://`, `tcp://` or
  `ssh://`. Hand it to `Docker::connect_with_defaults()`, which routes on the
  scheme, rather than treating step 1 as a filesystem path.
- **`connect_with_*` is lazy and proves nothing.** It builds a client without
  touching the socket, and returns `Ok` for a TCP address with nothing
  listening. Always `ping()`. Without it the app renders "Ready" against a dead
  daemon and shows an empty list — a worse lie than an error.

And the error text has to name the fix. The likeliest failure on someone else's
machine isn't a missing socket; it's a socket that's right there and returns
`EACCES` because they aren't in the `docker` group. "Docker isn't reachable"
would be useless there.

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
  main.rs              # RelmApp bootstrap, tracing init; loads settings + applies theme;
                       #   calls each component's CSS install
  app.rs               # root Component: AppModel, AppMsg, update, view; search + the
                       #   Preferences dialog live here
  settings.rs          # persistent global settings via glib::KeyFile
                       #   (~/.config/dockyard/settings.ini) + the Theme enum
  docker/
    mod.rs
    client.rs          # socket discovery, Docker handle, thin async wrappers
    types.rs           # our Container / ContainerState / Port structs (+ tests)
  components/
    mod.rs
    container_row.rs      # FactoryComponent -> adw::ActionRow
    container_detail.rs   # Component -> responsive detail dashboard (status/uptime/CPU/mem
                          #   cards, details, ports) that embeds a LogsView
    logs_view.rs          # Component -> embeddable Box, streaming log view; fixed-dark
                          #   "terminal" look (owns its `.log-terminal` stylesheet)
    sparkline.rs          # Component -> one live cairo sparkline; CPU and memory each embed one
    status_chip.rs        # shared WidgetTemplate (the pill + dot) plus state -> label/variant;
                          #   owns the `.status-chip` stylesheet
data/
  dev.miguelrincon.Dockyard.desktop     # plain, not .in — see below
  icons/hicolor/{16x16,...,512x512,scalable}/apps/dev.miguelrincon.Dockyard.{png,svg}
Makefile             # make install -> ~/.local (no sudo); make uninstall; make check
```

The `.desktop` file is deliberately **not** the `.desktop.in` this tree
originally named. The `.in` suffix is a meson/autotools convention for
build-time `@VARIABLE@` substitution; with only `cargo` and a fixed `~/.local`
install there is nothing to substitute, so the template would be ceremony.

Two notes on what this tree does *not* say:

- **There is no `container_list.rs`.** The `FactoryVecDeque<ContainerRow>` lives
  directly on `AppModel`, as the model sketch below shows — a separate module
  would have been a wrapper around one field.
- **`main.rs` does not init adw.** `RelmApp::new` already calls `gtk::init()`
  and, with relm4's `libadwaita` feature, `adw::init()`, and builds an
  `adw::Application`. Doing it again would be redundant.

The root model is roughly:

```rust
struct AppModel {
    docker: Option<Docker>,          // None = not connected
    containers: FactoryVecDeque<ContainerRow>,  // the *visible* (search-filtered) rows
    all_containers: Vec<Container>,  // full set from the last poll; the filter reads this
    query: String,                   // current search text ("" = no filter)
    state: ViewState,                // Loading | Ready | Disconnected(String)
    pending: HashMap<String, Action>, // ids with an action in flight, and which one
    refreshing: bool,                // user-initiated refresh only
    poll: Option<glib::SourceId>,    // None while the window is hidden
    settings: Settings,              // persisted; the Preferences dialog edits it
    toast_overlay: adw::ToastOverlay,
    // also holds nav / detail / detail_id / refresh_action handles for imperative use
}

enum AppMsg {
    Refresh,                         // the 2s poll; silent
    ManualRefresh,                   // the menu's Refresh item / Ctrl+R; spins
    Start(String),                   // container id
    Stop(String),
    Restart(String),
    Remove(String),                  // asks for confirmation
    RemoveConfirmed(String),         // actually removes
    ShowDetails(String),             // push the detail page (logs live inside it)
    SearchChanged(String),           // live filter, one per keystroke
    ShowAbout,                       // open the About dialog (primary menu)
    ShowPreferences,                 // open the Preferences dialog (primary menu)
    SetLogsWrap(bool),               // Preferences edits: the two log defaults and
    SetLogsTimestamps(bool),         //   the theme. Each persists to settings.ini;
    SetTheme(Theme),                 //   SetTheme also applies the scheme immediately
    Error(String),
    SuspendedChanged(bool),          // window visible / not visible
}

// Results landing back from off-thread work. relm4 gives commands their own
// channel, so these are the `CommandOutput` type rather than `AppMsg`.
enum CommandMsg {
    Connected(Box<Result<Docker, String>>),
    ContainersLoaded(Vec<Container>),
    // `action` rides along so a failure can name the verb, and so an open detail
    // page can be told its start/stop settled (to clear its transitional chip).
    ActionDone { id: String, action: Action, result: Result<(), String> },
    ListFailed(String),
}
```

This is Redux with a compiler. Actions in, single reducer, view derived from state.

One divergence worth knowing: **`ContainersLoaded` is a `CommandMsg`, not an
`AppMsg`.** relm4 separates a component's `Input` from its `CommandOutput`, so
"the user or a timer asked for something" and "off-thread work came back" stay
distinct. Everything arriving in `update_cmd` came from a command.

## UI shape

- `adw::ApplicationWindow` (opens 900×720) > `adw::ToolbarView` > `adw::HeaderBar`
- The header bar's title is an `adw::WindowTitle` with a **live subtitle** — a
  running count, e.g. "5 containers · 3 running" (of the full set, not the
  filtered view). Its far left is a **search** toggle (`system-search-symbolic`);
  it and **Ctrl+F** reveal a `gtk::SearchBar` under the header that filters the
  list live by name or image (client-side, no extra Docker call). No matches
  shows a third `adw::StatusPage`, distinct from the empty state.
- The header bar's far right is the **primary menu** (the `open-menu-symbolic`
  hamburger): a real `gio::Menu` model built with relm4's `menu!` macro, whose
  items are `GAction`s in a "win" group — **Refresh** (Ctrl+R / F5),
  **Preferences** (Ctrl+,), **About** (an `adw::AboutDialog`), and **Quit**
  (Ctrl+Q). Refresh greys out until
  connected (its `GAction` is enabled in the `Connected` handler, since a menu
  item's enabled state can't be `#[watch]`ed). Menu items *must* invoke actions,
  so this is the one place we use relm4's **actions** module rather than the
  message reducer — each action is a thin bridge that posts an `AppMsg` (except
  Quit, which acts directly). Contrast the row's `⋮`, still a hand-built popover.
- Main content: `adw::NavigationView`. Root page = container list; clicking a row
  pushes the detail page (a dashboard that embeds the streaming log view — there
  is no separate logs page).
- The list is clamped to ~600px (`adw::Clamp`) so rows stay readable in the wide
  window rather than stretching edge to edge.
- Each container is an `adw::ActionRow`: title = name, subtitle = image + ports
  (deliberately no status text — the chip carries the state), a status-chip
  prefix, a start/stop button suffix, and a menu button for restart/remove. The
  row is activatable — clicking it opens the detail page.
- **The status chip is a shared `StatusChip` `WidgetTemplate`** (a pill with a
  coloured dot + label), used by both the row and the detail page. Its
  `state → label/variant` mapping lives in `status_chip.rs`. While a start/stop
  (or restart/remove) is in flight, the chip shows a neutral transitional label —
  "Starting…", "Stopping…", … — on both screens; the detail page also swaps its
  start/stop button for a spinner. `AppModel` forwards the action result to the
  open detail page so its feedback clears on failure too, not just on a state
  flip.
- The detail page is responsive (`adw::BreakpointBin` at `min-width: 720px`): the
  four stat cards reflow from 2×2 to a single row of four (`gtk::FlowBox`), and the
  info column (details + ports) sits above the logs when narrow, beside them when
  wide. The CPU and memory cards each embed a `Sparkline` component.
- The embedded log panel is a fixed-dark **terminal** — its own `.log-terminal`
  stylesheet forces a dark background and light text regardless of the app theme.
- **Preferences** is an `adw::PreferencesDialog` with two groups: **Appearance**
  (a segmented System/Light/Dark theme control, applied live via
  `adw::StyleManager`) and **Logs** (the default wrap and timestamp toggles for
  new log panels). Its values persist through `settings.rs`.
- Empty, no-results, and disconnected states: `adw::StatusPage`.
- Errors: `adw::ToastOverlay` wrapping the content.
- **Use libadwaita widgets, not raw GTK equivalents.** `adw::ActionRow` over a
  hand-built `gtk::Box`; `adw::PreferencesGroup` over a labelled frame. That's
  where the native feel comes from — accent colour, dark mode, and adaptive
  layout are then free.
- No custom CSS unless there is no libadwaita widget for the job. If you think
  you need custom CSS, say why first. Each component that needs CSS owns it and
  installs it from `main` via its own `install_css` (there's no shared stylesheet
  module). **Two exceptions exist so far**:
  - The `.status-chip` pill in `components/status_chip.rs`, because libadwaita
    has no chip/badge widget — its `.badge` is the view-switcher bubble, and the
    colour classes only tint text. It's tonal (a tint of the state colour behind
    matching text) and uses Adwaita's own named colours, so it follows the theme.
  - The `.log-terminal` look in `components/logs_view.rs`, which *deliberately*
    ignores the theme: a fixed dark background and light text so logs read like a
    console in either light or dark mode. Note the colours are pinned on the
    `TextView`'s `textview` node, not just inherited — the theme sets that node's
    colour explicitly, and an inherited value loses to it (light mode gave
    black-on-dark until this was fixed).

## Refresh strategy

**Phase 1: poll.** ✅ Built. `glib::timeout` every 2s → `AppMsg::Refresh` → list
containers. Boring, correct.

Two things were added on top, both load-bearing:

- **The timer is removed while the window isn't visible**, keyed off
  `gtk::Window::is_suspended`. Measured, the poll costs ~0.085% of a core —
  nothing — but it wakes the CPU ~2.6×/s forever, and wakeups are what cost
  laptop battery. Don't reintroduce a timer that ticks and skips work: a timer
  that fires is a wakeup regardless.
- **Rows are updated in place, not rebuilt.** Rebuilding on each poll destroys
  every widget 30×/minute and tears down transient state like an open popover.
  Reconcile by id; rebuild only when membership changes.

**Phase 2 (later): events.** `docker.events()` returns a stream; subscribe via a
relm4 `command` and push messages on container start/stop/die. Polling now works
end to end, so this is unblocked — but it's an optimisation for *latency*, not
resources. See ARCHITECTURE.md's "What the app actually costs".

## Scope

In scope for v1:

- ✅ List containers (name, image, status, ports), including stopped ones
- ✅ Start / stop / restart / remove
- ✅ View logs (`docker.logs()` with `follow: true`, tail 200)

**All three v1 features are built** (logs shipped in #10 with follow-scroll, a
wrap toggle and a timestamp toggle; #18 moved them into the detail page and
dropped the standalone logs page). The `.desktop` file, icon, installer, and
the "no containers" empty-state `adw::StatusPage` are all done too. v1 is
complete.

Beyond v1, the detail page (#13–#18) adds a resource dashboard — status/uptime
cards, live CPU/memory graphs from the `stats` stream — and the embedded logs,
laid out responsively. Note `resource graphs` appears in the "stay lean" list
below; they were built anyway because they're genuinely useful here, which is
exactly the "flag the drift, then build it if it helps" posture that list now
takes.

Since then, a run of small, in-scope refinements (all merged): a header **search**
that filters the list by name/image (#27); a wider default window with a clamped
list (#28); the CPU/memory graph extracted into a reusable `Sparkline`, a header
container count, transitional start/stop **feedback** on the chip, and the chip
itself pulled into a shared `StatusChip` widget (#29); and a **Preferences**
dialog with persisted settings — the two log defaults plus a light/dark/system
theme override — backed by a `glib::KeyFile` config file, along with the
fixed-dark terminal log look (#30). None of these are Docker-Desktop drift; they
polish what's already there.

**A reminder to stay lean, not a hard ban** (revised — was "explicitly out of
scope"): image builds, `docker compose`, volumes, networks, registries, `exec`
into a container, resource graphs, multi-host. These aren't the default focus —
the app is a personal, single-machine container manager, not Docker Desktop. But
the author may add any of them when it's genuinely useful to *them*.

So the rule is no longer "refuse." It's: when a change drifts toward Docker
Desktop, **say so** — name the cost and the direction — so it's a conscious
choice, then build it if the author wants it. Flag, don't gatekeep.

## Commands

```bash
cargo run                  # dev
cargo build --release
cargo test                 # pure unit tests; no daemon needed
cargo clippy --all-targets -- -D warnings   # must pass clean before any commit
cargo fmt
```

`--all-targets` is not decoration: plain `cargo clippy` does **not** lint test
code, and it missed a real lint the first time tests existed.

System deps (CachyOS / Arch):

```bash
sudo pacman -S --needed base-devel pkgconf rust gtk4 libadwaita librsvg
```

Rust must be ≥ 1.93 (relm4 0.11's MSRV) — `cargo` refuses to build below it.
libadwaita must be ≥ 1.5, for `adw::AlertDialog`.

The Docker daemon is socket-activated: `sudo systemctl enable --now docker.socket`.
For rootless: `systemctl --user enable --now docker`.

## Conventions

- `cargo clippy --all-targets -- -D warnings` is the bar, not `cargo build`.
- Commits: conventional commits (`feat:`, `fix:`, `refactor:`).
- **Licence: GPL-3.0-or-later.** Full text in `COPYING`; declared in
  `Cargo.toml`. Every source file carries the two-line SPDX header
  (`SPDX-FileCopyrightText` + `SPDX-License-Identifier: GPL-3.0-or-later`) — new
  `.rs` files get it too.
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
