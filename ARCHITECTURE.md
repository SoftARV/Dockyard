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
cargo run                                # dev build, launches the window
RUST_LOG=debug cargo run                 # same, with our tracing logs on stdout
cargo test                               # unit tests; no daemon needed
cargo clippy --all-targets -- -D warnings  # the bar. --all-targets also lints tests
cargo fmt                                # format
cargo build --release                    # optimised binary at target/release/dockyard
```

To install it as a real desktop app (icon, app-grid launcher), into `~/.local`
with no sudo:

```bash
make install     # build --release, copy to ~/.local, refresh caches
make uninstall   # remove everything it installed
make check       # fmt --check + clippy --all-targets + test (the commit bar)
```

The icon and app-grid launcher only appear for the **installed** app. On
Wayland `cargo run` won't show the icon regardless — see "How the app finds its
own icon". Install with `make install` and launch Dockyard from the app grid.

**Only one copy runs at a time.** GTK apps register their app ID on D-Bus, so a
second `cargo run` while the first is open silently hands off to the running
window and exits 0 — no error, no second window. If a run seems to do nothing,
that's why. Close the first window, or:

```bash
pkill -f target/debug/dockyard
```

Requirements: Rust ≥ 1.93, gtk4 ≥ 4.10, libadwaita ≥ 1.5, and a reachable Docker
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
  main.rs                     bootstrap: tracing, RelmApp, app ID, icon
  app.rs                      root Component — the store + reducer + view
  docker/
    client.rs                 socket discovery, connect, ping, API wrappers
    types.rs                  our Container/ContainerState/Port/ContainerDetail/Stats
  components/
    container_row.rs          FactoryComponent -> adw::ActionRow
    container_detail.rs       Component -> responsive detail dashboard; embeds LogsView
    logs_view.rs              Component -> embeddable Box, streaming log view
    status_chip.rs            shared state -> chip label + colour-variant class
main.rs also carries the one custom stylesheet (the .status-chip pill).
data/
  dev.miguelrincon.Dockyard.desktop    launcher entry
  icons/hicolor/.../apps/dev.miguelrincon.Dockyard.{png,svg}
Makefile                      make install -> ~/.local; make uninstall; make check
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

1. You click **Stop** on a row. The GTK button emits `clicked`, which sends
   `ContainerRowInput::ToggleClicked` to the row itself.
2. The row reads its *current* state to decide what that click meant, and sends
   the intent *up*: `sender.output(ContainerRowOutput::Stop(id))`. The row does
   **not** call Docker. It doesn't know Docker exists.
3. `app.rs`'s `.forward(...)` maps that output into the parent's action:
   `ContainerRowOutput::Stop(id) => AppMsg::Stop(id)`.
4. `AppModel::update` matches `AppMsg::Stop(id)` and calls `dispatch(...)`,
   which marks the container busy (the row swaps its button for a spinner),
   spawns a **command** — an async task on a worker thread — and returns
   immediately. The reducer is done in microseconds.
5. The task runs `client::stop_container(...).await` off the main thread, then
   sends back `CommandMsg::ActionDone { id, result }`.
6. relm4 delivers that to `update_cmd` **on the main thread**, so it's safe to
   touch the model and widgets there. The row stops spinning; a failure becomes
   a toast.
7. `update_cmd` fires an immediate `Refresh` rather than waiting up to 2s for
   the poll, and the row redraws with the container's new state.

The important shape: **rows emit intent, the root decides.** All Docker I/O
lives in one reducer, so there's exactly one place where actions are ordered and
errors are handled — the same reason you keep `fetch` out of React components.

---

## Rust things you'll hit in this codebase

### Streaming, and cancelling a stream

Every Docker call except one is a `oneshot_command`: fire an async request, get
one message back. Logs are the exception — they *follow*, producing output for
as long as you watch. That needs the other half of relm4's async API, and it's
worth understanding because it's the first genuinely new shape since the
Redux-with-a-compiler core.

`oneshot_command(async { ... })` runs a future that resolves to **one**
`CommandOutput`. `command(|out, shutdown| ...)` instead hands you:

- `out` — a `Sender`, so you push **many** `CommandOutput`s over time (one per
  log chunk), each landing in `update_cmd` on the main thread.
- `shutdown` — a `ShutdownReceiver`, the cancellation handle.

The idiom, straight from relm4's own examples:

```rust
sender.command(|out, shutdown| {
    shutdown
        .register(async move {
            let mut stream = client::logs(&docker, &id);
            while let Some(item) = stream.next().await {
                if out.send(chunk).is_err() { return; }   // receiver gone
            }
        })
        .drop_on_shutdown()   // <- the important bit
        .boxed()
});
```

`drop_on_shutdown()` is what makes this clean. It says: when this component
shuts down, *drop the future*. Dropping a Rust future mid-`await` cancels it —
the `stream.next()` is abandoned, the HTTP connection to Docker closes, the
follow stops. No cancellation flag to check, no token to thread through.

The trick is arranging for "component shuts down" to mean "user left the detail
page". Since #18 the log view is no longer a page of its own — it's a
`LogsView` (an embeddable `Box`) that the **detail page** owns. So the lifecycle
is a two-level cascade:

- `AppModel` holds `detail: Option<Controller<ContainerDetailPage>>`. The detail
  page, in turn, holds a `Controller<LogsView>`. **Each owning handle keeps its
  child — and its child's streams — alive.**
- Navigate-back fires `NavigationView::popped`; the reducer sets
  `self.detail = None`; the detail controller drops, which drops the `LogsView`
  controller it owns; both components shut down; `drop_on_shutdown` cancels the
  stats stream and the log follow together.

So the streams live exactly as long as the detail page is on screen, enforced by
ownership rather than by remembering to clean up. This was verified, not
assumed: with a container logging every 0.3s, chunks arrived while the page was
open and *stopped the instant it closed* — a leaked follow would have kept
printing.

(Think of `Controller<LogsView>` as an owning handle to a running child, a bit
like a React ref to a component whose `useEffect` cleanup is tied to the ref
being dropped — except here the compiler guarantees the cleanup runs.)

Both streams inside the detail page share one twist: they only run while the
container runs — Docker closes the `stats` stream and ends the `logs --follow`
stream when it stops. A guard flag (`stats_active` for stats, `streaming` for
logs) lets the 2-second re-inspect restart the stream when the container comes
back up, so **starting a container from inside the detail view revives both its
graphs and its logs**. The detail page emits `LogsInput::EnsureStreaming` on
each inspect where the container is up; `LogsView` opens a stream if it doesn't
already have one (and clears its buffer first, so a restarted container's
re-served `--tail` doesn't print twice). `drop_on_shutdown` still cancels
everything on navigate-back, and the graph data lives in the model, redrawn each
sample via relm4's `DrawHandler` (a cairo surface) — no `Rc<RefCell>`.

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

### What the app actually costs

Measured, not guessed — release build, three interleaved 90s runs, sampling
`/proc/<pid>/stat`:

| | CPU (one core) | wakeups/s | RSS |
| --- | --- | --- | --- |
| poll every 2s | **~0.085%** | ~2.6 | 107.9 MB |
| poll off | ~0.004% | ~0.3 | 107.8 MB |

Read that carefully, because it's counterintuitive:

- **The poll costs ~1 second of CPU every 20 minutes.** Each tick is ~1ms: one
  HTTP request over a unix socket plus a JSON parse.
- **It costs zero memory.** RSS is identical with it on and off, and doesn't
  grow — the per-poll `String`s aren't leaking.
- **108 MB is the price of GTK**, not of anything we wrote. gnome-calendar, a
  first-party GTK4/libadwaita app, sits at 82 MB. If "lightweight" means
  memory, the app *is* GTK and there's no polling decision that changes it.

So the poll is optimised for **wakeups, not cycles**. ~2.6/s forever, including
for a window minimised on another workspace, is what costs laptop battery —
waking the CPU out of deep C-states. `is_suspended` gating removes the timer
outright whenever the window isn't visible; a timer that fires is a wakeup even
if it decides to do nothing.

Two methodology notes, learned the hard way:

- **One sample is worthless here.** The first release run showed `poll=off`
  using *6× more* CPU than `poll=2s`. Pure noise: `CLK_TCK` is 100, so a tick
  is 10ms and the entire signal is 3–10 ticks. Only repeated interleaved runs
  separated signal from noise.
- **Measure the binary you shipped.** An early run tested a stale binary
  because a `pkill` in the command chain killed the `cargo build` before it ran.

### Never block the main thread

GTK is single-threaded: one thread owns every widget and paints every frame. If
`update()` blocks for 3 seconds, the window doesn't redraw or drag for 3
seconds. Docker calls take unbounded time, so they *all* go through commands,
which run on tokio worker threads and post results back. That's what CLAUDE.md
rule 4 protects.

### Rebuilding widgets hides staleness bugs

The first cut of the list rebuilt every row on every 2s poll. That looked
harmless and was not: it destroyed and recreated every widget 30 times a
minute, so an open popover — parented to a row's menu button — was torn down
mid-interaction and appeared to close by itself.

Rebuilding also *masked* two bugs, which surfaced the moment rows persisted:

- **Closures capture once.** `connect_clicked[running = ...]` freezes `running`
  at widget-build time. While rows were rebuilt every 2s that was invisible;
  the moment they persist, the button offers the wrong action forever. Route
  the click through an `Input` and read the model at click time instead.
- **`add_css_class` appends, `set_css_classes` replaces.** Under `#[watch]` the
  appending form accumulates, so a container that ran and then exited ends up
  styled `success` *and* `dim-label`.

If you ever find yourself rebuilding widgets to make state changes show up,
that's a `#[watch]` you haven't written yet.

### Swap widgets with a `gtk::Stack`, not with `visible`

Toggling two widgets' `visible` looks like the obvious way to swap a button for
a spinner. It isn't: they have different natural sizes (a flat icon button is
~34px, a spinner ~16px), so the slot resizes and everything beside it jumps.

A `gtk::Stack` is the tool — homogeneous by default, it allocates its largest
child's size to every child, so the footprint is stable while the contents
change. Set `visible_child_name` *after* adding the children; naming a child
that doesn't exist yet is a GTK-CRITICAL.

### libadwaita versions are a build-time floor

relm4's `gnome_*` features gate which libadwaita widgets exist at all. The
default is `gnome_42`, which doesn't have `adw::ToolbarView` or
`adw::NavigationView` (libadwaita 1.4) — the code simply won't compile.

We pin **`gnome_46`** (libadwaita 1.5), which `adw::AlertDialog` needs. This is
a floor on what users must have installed, so it's a real cost, not just a
number: GNOME 46 shipped Mar 2024 and is what Ubuntu 24.04 LTS carries. Raise
it only for a widget you actually need.

The version ladder is worth internalising, because it decides what you're
allowed to use:

| relm4 feature | libadwaita | Notable |
| --- | --- | --- |
| `gnome_42` (default) | 1.0 | `ActionRow`, `StatusPage`, `Toast` |
| `gnome_45` | 1.4 | `ToolbarView`, `NavigationView` |
| **`gnome_46`** ← us | **1.5** | **`AlertDialog`** |
| `gnome_47` | 1.6 | `MessageDialog` becomes deprecated |

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
| The poll is silent; only user-initiated refresh spins | A spinner blinking every 2s forever is worse than no feedback. `AppMsg::ManualRefresh` exists purely to draw that line. |
| Actions refresh immediately on completion | Waiting up to 2s for the next poll made even fast actions feel broken. |
| `gnome_46` for `adw::AlertDialog` | `AlertDialog` needs libadwaita 1.5. `MessageDialog` works at 1.4 but is deprecated from 1.6, so it would break `clippy -D warnings` on any later bump. Floor is GNOME 46 (Mar 2024) = Ubuntu 24.04 LTS. |
| Update rows in place; rebuild only when membership changes | The first cut rebuilt every row on every poll. That destroys widgets 30 times a minute, and an open popover — parented to a row's menu button — died with it. Cheapness was never the issue; rebuilding throws away interaction state. |
| `remove_container` isn't forced | Removing a running container should fail loudly rather than silently kill it. |
| Sort by name | Docker returns newest-first; a list that reorders under your cursor every 2s is worse than a stable one. |
| `tracing-subscriber` added | Not in CLAUDE.md's stack table, but `main.rs`'s job of "tracing init" is impossible without it. `env-filter` gives `RUST_LOG`. |

---

## Status and timeline

All dates 17 Jul 2026 — the app went from empty repo to working in one sitting.

### Shipped

**[#1] Scaffold — connect to Docker and list containers**

- Cargo project, pinned stack, `.gitignore`. Needed rustup `stable` 1.91 → 1.97,
  because relm4 0.11 requires ≥1.93 and `cargo` refuses outright below it.
- **Socket discovery** (`docker/client.rs`): `DOCKER_HOST` → rootless →
  rootful, `ping()` verification, and errors that name their fix. All three
  branches exercised; none panic.
- **Type boundary** (`docker/types.rs`): bollard stops here.
- **Container list**: `FactoryVecDeque<ContainerRow>` into an
  `adw::PreferencesGroup`, 2s poll, status icon, ports, start/stop, and a
  restart/remove menu.
- **Lifecycle actions**: start / stop / restart / remove, all off-thread, all
  failures becoming toasts.

**[#2] Rows update in place**

The poll rebuilt every row, destroying widgets 30 times a minute and closing
any open popover. Now reconciled by id; rebuild only when membership changes.
Exposed and fixed the two staleness bugs described above.

**[#3] Action feedback and remove confirmation**

- Rows spin while an action is in flight; `ActionDone` refreshes immediately
  rather than waiting for the poll.
- The refresh button spins only for a refresh *you* asked for.
- Remove confirms via `adw::AlertDialog`. Raised the floor to `gnome_46`.
- Fixed the menu not dismissing, and the action slot resizing mid-swap.

**[#5] First tests, and pausing the poll when hidden**

- **10 tests** over `Container::from_summary` — the first in the project. Pure,
  no daemon, 0.00s. Mutation-checked: breaking the slash-stripping fails
  exactly the two name tests and leaves the other eight green.
- The poll stops while the window isn't visible, via
  `gtk::Window::is_suspended`. See "What the app actually costs" — this buys
  wakeups, not CPU. Both halves confirmed by hand: minimise stops the poll
  (after GNOME's ~4s lag), restoring starts it again. The resume path needed a
  human, since Wayland refuses a scripted `present()`.
- Established that plain `cargo clippy` doesn't lint tests; the bar is now
  `--all-targets`.

**[#8] Readable error toasts**

- A failed remove produced a 234-char toast; the client now trims a bollard
  error to its reason clause (`container is running`) and the app names the
  container and verb. Verified end to end against the daemon: 51 chars.

**[#9] Icon, `.desktop` entry, and a Makefile installer**

- The app has an identity now: it shows its own icon and launches from the app
  grid. Icons live under `data/icons/hicolor` named for the app ID — the shared
  name is what ties launcher, window (Wayland `app_id`) and themed icon
  together. `make install` lands everything in `~/.local` without sudo.
- The `.desktop` is plain, not `.desktop.in`: no build system means nothing to
  substitute.

**[#10] Streaming log view** — the last v1 feature, and the first that streams.

- Clicking a row's logs button pushes a `LogsPage` onto an `adw::NavigationView`
  (the list is now its root page). It follows `docker.logs()` into a monospace
  `TextView`. Uses `command` + `drop_on_shutdown`, so the stream is cancelled
  exactly when the page closes — verified: zero chunks after navigate-back,
  against a container logging continuously. See "Streaming, and cancelling a
  stream".
- **Follow-scroll** driven by the scroll *adjustment*, not `scroll_to_iter`
  (which misfires before layout, so the page used to open at the top). Snaps to
  the bottom when content settles *if* you're already there, so it doesn't yank
  the view while you read scrollback.
- **Wrap toggle** in the header for the wide lines (~285 chars) container logs
  produce; **timestamp toggle** (off by default) that shows Docker's own
  RFC3339 stamp reduced to `HH:MM:SS`, dimmed — off by default because many
  apps print their own and Docker's would double it.
- Stayed with `GtkTextView` rather than a widget-per-line list, to keep native
  cross-line copy-paste — the one thing you most want from a log pane.

`cargo clippy --all-targets -- -D warnings` clean throughout; 22 unit tests.

**[#12] Empty state** — "no containers" now shows an `adw::StatusPage` instead
of a blank group. A `gtk::Stack` flips between the list and the status page on
`containers.is_empty()`, rather than an `if` that would re-parent the factory's
list widget every time the last container goes.

**v1 feature-complete** — list, start/stop/restart/remove, logs, empty state,
and desktop integration are all built.

**Beyond v1 (scope relaxed to a reminder).** The out-of-scope list became a
"stay lean, flag drift" reminder rather than a ban, which unblocked:

**[#13] Log-view polish** — scroll-preserving timestamp toggle (via an invisible
tag, not a rebuild), theme-adaptive timestamp colour, a labelled options menu.

**[#14] Container detail dashboard** — clicking a row pushes a detail page: a
status *chip* (the one bit of custom CSS — no libadwaita chip widget exists),
live uptime, details and ports as cards, and a start/stop button (`adw::Button
Content`, icon + label). Re-inspects every 2s so the chip/button/uptime stay
live; the same status chip is reused in the list rows. This is where the app
starts moving from "manager" toward "monitor" — a conscious, flagged choice.

**[#15] Resource graphs** — CPU and memory tiles on the detail page: current
value plus a cairo sparkline (relm4's `DrawHandler`, no charting dependency).
`client::stats` streams `docker.stats`; CPU% is the cpu/precpu delta Docker puts
in each frame, unit-tested. The stream runs only while the container is up and
restarts when it comes back.

**[#18] Logs move into the detail page, responsively** — the standalone logs
page is gone. `LogsPage` (an `adw::NavigationPage`) became `LogsView`, an
embeddable `Box` the detail page owns; the per-row logs button and
`AppMsg::ShowLogs` were removed, so logs are reached only by opening a
container. The detail page is now responsive via an `adw::BreakpointBin` at
`min-width: 720px`: the four stat cards reflow from 2×2 to a single row (a
`gtk::FlowBox` whose `min/max-children-per-line` the breakpoint bumps to 4), and
the info column (details + ports) sits above the logs when narrow, beside them
when wide (the breakpoint flips a `Box`'s `orientation`). A follow-up fix: logs
subscribe the same way stats do — a `streaming` guard plus an `EnsureStreaming`
message the detail page emits on each inspect where the container is up — so
starting a stopped container *from the detail view* revives its logs. The buffer
is cleared on re-subscribe, because `docker logs --tail` re-serves the pre-stop
lines and would otherwise print them twice. See "Streaming, and cancelling a
stream" for the two-level ownership cascade that cancels both streams on
navigate-back.

**[#22–#23] Detail layout, refined** — the wide side-by-side split began 50/50
(a homogeneous box), then moved to **40/60** in favour of the logs: a `gtk::Grid`
with `column-homogeneous` and 2:3 column spans set on the breakpoint's *layout
children*, so the 64-char container ID no longer inflates the info column and
starves the log pane. The log panel also gained `overflow: hidden` so its
`TextView` clips to the `.card` radius — it reads as a proper card now, not a
square poking out of a rounded one.

**[#24] Licensed GPL-3.0-or-later** — the project is formally open source: the
full GPLv3 in `COPYING`, `license = "GPL-3.0-or-later"` in `Cargo.toml`, a README
statement and badge, and the two-line REUSE/SPDX header atop every source file.
GPL keeps distributed derivatives open (the GNOME-app norm); GTK/libadwaita being
LGPL never forced the choice, and relm4 (MIT/Apache) and bollard (Apache-2.0) are
GPL-compatible.

**[#25] A primary menu** — the GNOME hamburger (`open-menu-symbolic`) in the
header, holding **Refresh** (Ctrl+R / F5), **About** (an `adw::AboutDialog`), and
**Quit** (Ctrl+Q); Refresh moved off its own header button. This is the first use
of relm4's **actions**, and it's forced: a `gio::Menu` item can only invoke a
`GAction` — it can't send a relm4 message. So the menu is a real menu model (the
`menu!` macro) whose items name actions in a "win" group registered on the
window. The actions stay thin: Refresh and About just post an `AppMsg`, so the
work still lands in the one reducer; only Quit acts directly
(`main_application().quit()`). One wrinkle: an action's `enabled` can't be
`#[watch]`ed from `view!`, so to keep the old refresh button's "disabled until
connected" behaviour the model holds the `GAction` handle and flips it
imperatively in the `Connected` handler — the same escape-hatch reasoning as the
held `nav`/`toast_overlay` handles.

### How the app finds its own icon (and why Wayland is the twist)

The instinct — "the app sets its window icon" — is **wrong on Wayland**, and
getting this wrong cost a debugging round worth writing down.

On Wayland a client *cannot* set its own toplevel icon. There's no protocol for
it. GNOME Shell matches the running window to a `.desktop` file and takes the
icon from that file's `Icon=`. **So only the installed app shows an icon —
`cargo run` never does on Wayland**, and installing the `.desktop` doesn't
change that, which is the part that's easy to get wrong.

The matching is not purely `app_id`. GNOME also weighs the executable: our dev
binary is `target/debug/dockyard`, the launcher's `Exec=` resolves to
`~/.local/bin/dockyard`, and the mismatch is enough that the Shell treats a
`cargo run` window as an unassociated app with no icon — even with the
`.desktop` installed. (This was predicted to work and then tested; it doesn't.
Two rounds of wrong model, so it's spelled out here.)

So what is `setup_icon` in `main.rs` for? It's the standard idiom, and it *does*
work on **X11** and some other compositors, where a client sets its own window
icon from the theme: `set_default_icon_name(APP_ID)` names it and
`add_search_path("data/icons")` lets the dev build resolve it pre-install. The
search path also covers future *in-app* icon use. All harmless no-ops on
Wayland.

A verification lesson worth keeping: `IconTheme::has_icon` returning true only
proved GTK could resolve the name — it never proved the icon would *appear*,
because on Wayland the Shell decides the window icon and never asks GTK. Testing
the resolvable layer felt like testing the visible one. It wasn't.

The single shared string `dev.miguelrincon.Dockyard` is the app ID, the
`.desktop` filename, the `Icon=` value, and the icon filename. That's not
repetition — it's the join key GNOME uses to connect a running window to its
launcher and icon, which is why no `StartupWMClass` is needed.

### Testing the parts the compiler can't reach

The app narrates itself, so most behaviour can be checked by watching the log
rather than squinting at pixels:

```bash
RUST_LOG=dockyard=debug cargo run 2>&1 | grep --line-buffered -E "poll|visibility|rebuilding"
```

- **Poll gating.** Minimise (`Super`+`H`) or switch workspace, wait ~5s for
  GNOME's lag: `suspended=true` → `stopping poll`. Restore: `suspended=false` →
  `starting poll`. Both halves must appear; if `starting poll` doesn't, the app
  is silently frozen.
- **Rows update in place.** `rebuilding rows` should appear exactly *once*, at
  startup. If it fires on every poll, the reconcile has broken and open
  popovers will slam shut.
- **State changes reach the UI.** Change the world from another terminal —
  `docker stop <name>` — and watch the row follow. Do it while minimised to
  test that resuming refreshes immediately rather than waiting for a tick.
- **Errors become toasts.** Use a throwaway container and try to remove it while
  it's running. `remove_container` is deliberately unforced, so Docker refuses:

  ```bash
  docker run -d --name dockyard-test alpine sleep 3600
  # in the app: ⋯ -> Remove -> confirm
  docker rm -f dockyard-test        # cleanup
  ```

  Expect a toast carrying Docker's refusal, the row's spinner stopping, and the
  container still running afterwards. Non-destructive and repeatable — the
  failed remove leaves the container alone, which is the whole point of not
  forcing it. Worth checking the toast is actually *readable*: bollard's errors
  are verbose and an `adw::Toast` is one truncating line.

  **Don't use a port conflict as the fixture.** It looks perfect — two
  containers bound to the same host port can never run at once — and it is a
  trap. A start that fails on port allocation can leave the container detached
  from its network (`NetworkMode` set, `NetworkSettings.Networks` empty). From
  then on it starts *successfully* with no network, no published port and no
  error, because with no network there's nothing to publish a port on. The
  fixture silently stops being a fixture, and you're left testing nothing.

Only one copy runs at a time (D-Bus app ID), so `pkill -f target/debug/dockyard`
before each run or the new one hands off to the old one and exits silently.

### What testing actually caught

Worth recording, because it shaped how we work: **every bug so far was found by
running the app, not by the compiler.** The popover closing, the missing
feedback, the menu staying open, the slot shifting — clippy was clean for all
four. Rust removes whole categories of bug, and none of them were these. Budget
for driving the UI, not just for green builds.

### Next

**v1 is complete** — list, lifecycle actions, logs, the empty state, and
desktop integration are all done — and the detail dashboard (status/uptime
cards, live CPU/memory graphs, embedded logs, responsive layout) is built on top
of it. The two log-view polish items once listed here — the timestamp toggle
resetting the scroll position, and the fixed-grey timestamp colour — were both
resolved in #13 (invisible-tag toggle, theme-adaptive colour).

No committed next feature. Candidates, only if they earn their keep on this one
machine: `docker.events()` to replace polling (below), or surfacing container
health beyond "running" (see "Known rough edges").

### Later, deliberately (v2)

- **Events instead of polling** — `docker.events()` via a `command`. Only once
  polling is proven, per CLAUDE.md phase 2. It's a latency win, not a resource
  one — see "What the app actually costs".

### Known rough edges

- `ContainerState::is_running()` counts `Restarting` as running, so the button
  offers "stop" mid-restart. Defensible, not thought through.
- **"Running" doesn't mean "working", and we can't tell.** A start that fails on
  port allocation can leave a container detached from its network; it then
  starts fine with only loopback, reachable by nothing. We draw it as an
  ordinary "Up 3 minutes" row with no ports — accurate, because that is exactly
  what Docker reports, and still misleading. The absent port is the only tell.
  Surfacing it properly would mean inspecting networks, which CLAUDE.md puts
  out of scope, so this stays a known blind spot rather than a TODO.
- GNOME takes ~4s to mark a window suspended, so the poll lingers briefly after
  you minimise. Expected, not a bug — don't go looking for a faster signal.
- `tokio` is in `Cargo.toml` but not used directly — relm4 owns the tokio
  runtime. `futures-util` *is* used, by the logs stream.
- The row menu is a hand-built `gtk::Popover` of plain buttons, not a
  `gtk::PopoverMenu` with a menu model. It works, but it needs manual
  dismissal and lacks the keyboard and screen-reader behaviour a real menu
  gets free. The header's primary menu (#25) shows the model-based way now, so
  converting the row menu is a matter of following that pattern if it grows past
  restart/remove.
- The rootless socket path is only reachable on a rootless install; on this
  machine it's tested by faking `XDG_RUNTIME_DIR`.

### Stay lean — flag the drift, don't gatekeep

Image builds, `docker compose`, volumes, networks, registries, `exec`,
multi-host. None are the default focus; the app is a personal, single-machine
container manager, not Docker Desktop. This was a hard "out of scope" list once
— it's now a reminder, matching CLAUDE.md's revised scope. When a change grows
toward Docker Desktop, **name the cost and the direction** so it's a conscious
choice, then build it if it's genuinely useful here. Resource graphs used to sit
on this list; #15 built them anyway, precisely because they earned it — that's
the posture, not an exception to it.
