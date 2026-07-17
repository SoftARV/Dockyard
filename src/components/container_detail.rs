//! The container detail view — a page pushed onto the `NavigationView` when a
//! row is clicked. Dashboard cards for status, uptime, CPU and memory, plus
//! details and ports.
//!
//! Three things load on open: the `Container` from the list shows at once; a
//! one-shot `inspect` (re-run every 2s) fills in state, start time, command; and
//! a `stats` stream feeds the CPU/memory graphs while the container runs. A 1s
//! timer ticks the uptime.

use bollard::Docker;
use futures_util::{FutureExt, StreamExt};
use relm4::abstractions::DrawHandler;
use relm4::adw::prelude::*;
use relm4::gtk::glib;
use relm4::{Component, ComponentParts, ComponentSender, RelmWidgetExt, adw, gtk};

use crate::components::status_chip;
use crate::docker::client;
use crate::docker::types::{Container, ContainerDetail, Stats};

/// How many recent samples each graph keeps.
const HISTORY: usize = 60;

/// Graph line colours (Adwaita blue-3 / purple-3). Fixed rather than
/// theme-derived, so CPU and memory stay visually distinct.
const CPU_COLOR: gtk::gdk::RGBA = gtk::gdk::RGBA::new(0.384, 0.627, 0.918, 1.0); // #62a0ea
const MEM_COLOR: gtk::gdk::RGBA = gtk::gdk::RGBA::new(0.753, 0.380, 0.796, 1.0); // #c061cb

pub struct ContainerDetailInit {
    pub docker: Docker,
    /// What the list already knows — shown at once, before `inspect` returns.
    pub container: Container,
}

pub struct ContainerDetailPage {
    /// Kept for the periodic re-inspect and to (re)start the stats stream. An
    /// `Arc`-backed handle, cheap to hold.
    docker: Docker,
    container: Container,
    /// Filled in when `inspect` returns; `None` until then.
    detail: Option<ContainerDetail>,
    /// Container start time as Unix seconds, parsed from `detail.started_at`.
    /// Drives the live uptime; `None` when not running or not yet loaded.
    started_unix: Option<i64>,
    /// The most recent stats sample, for the current CPU/memory values.
    latest: Option<Stats>,
    /// Recent CPU % and memory %, oldest first, for the graphs.
    cpu_history: Vec<f64>,
    mem_history: Vec<f64>,
    /// Whether a stats stream is currently running, so we don't start a second
    /// when a re-inspect confirms the container is still up.
    stats_active: bool,
    /// The two graph surfaces. relm4's `DrawHandler` keeps a cairo surface we
    /// repaint from `*_history` on each sample — no `Rc<RefCell>` needed.
    cpu_draw: DrawHandler,
    mem_draw: DrawHandler,
}

#[derive(Debug)]
pub enum DetailInput {
    /// The uptime timer fired; re-render (the `#[watch]` uptime recomputes).
    Tick,
    /// The re-inspect timer fired; fetch fresh state so the chip/button/uptime
    /// stay live (e.g. after the start/stop button, or an external change).
    Refresh,
    /// The start/stop button was clicked. Reads current state to decide which.
    ToggleClicked,
}

/// Intents the page sends up. Like the row, the detail page never calls Docker
/// itself — `AppModel` owns that (CLAUDE.md rule 4).
#[derive(Debug)]
pub enum DetailOutput {
    Start(String),
    Stop(String),
}

#[derive(Debug)]
pub enum DetailCmd {
    Inspected(Result<ContainerDetail, String>),
    /// One resource sample from the stats stream.
    StatsSample(Stats),
    /// The stats stream ended (the container stopped, or it errored).
    StatsEnded,
}

#[relm4::component(pub)]
impl Component for ContainerDetailPage {
    type Init = ContainerDetailInit;
    type Input = DetailInput;
    type Output = DetailOutput;
    type CommandOutput = DetailCmd;

    view! {
        adw::NavigationPage {
            set_title: &model.container.name,

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    // Start/stop with an icon *and* a label. NavigationView owns
                    // the start side (back button), so this sits at the end.
                    pack_end = &gtk::Button {
                        connect_clicked => DetailInput::ToggleClicked,

                        #[wrap(Some)]
                        set_child = &adw::ButtonContent {
                            #[watch]
                            set_icon_name: if model.container.state.is_running() {
                                "media-playback-stop-symbolic"
                            } else {
                                "media-playback-start-symbolic"
                            },
                            #[watch]
                            set_label: if model.container.state.is_running() {
                                "Stop"
                            } else {
                                "Start"
                            },
                        },
                    },
                },

                #[wrap(Some)]
                set_content = &gtk::ScrolledWindow {
                    set_vexpand: true,

                    adw::Clamp {
                        set_margin_all: 18,

                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 18,

                            // Stat tiles: status and uptime, side by side.
                            // CPU/memory join this row in the follow-up.
                            // Homogeneous so the tiles share the width.
                            gtk::Box {
                                set_orientation: gtk::Orientation::Horizontal,
                                set_spacing: 12,
                                set_homogeneous: true,

                                // Status tile — the chip on its own card.
                                gtk::Box {
                                    add_css_class: "card",

                                    gtk::Box {
                                        set_orientation: gtk::Orientation::Vertical,
                                        set_spacing: 6,
                                        set_margin_all: 14,

                                        gtk::Label {
                                            set_label: "STATUS",
                                            add_css_class: "caption",
                                            add_css_class: "dim-label",
                                            set_halign: gtk::Align::Start,
                                        },
                                        gtk::Label {
                                            set_halign: gtk::Align::Start,
                                            #[watch]
                                            set_label: status_chip::label(model.container.state),
                                            #[watch]
                                            set_css_classes: &[
                                                "status-chip",
                                                status_chip::variant(model.container.state),
                                            ],
                                        },
                                    },
                                },

                                // Uptime tile.
                                gtk::Box {
                                    add_css_class: "card",

                                    gtk::Box {
                                        set_orientation: gtk::Orientation::Vertical,
                                        set_spacing: 6,
                                        set_margin_all: 14,

                                        gtk::Label {
                                            set_label: "UPTIME",
                                            add_css_class: "caption",
                                            add_css_class: "dim-label",
                                            set_halign: gtk::Align::Start,
                                        },
                                        gtk::Label {
                                            #[watch]
                                            set_label: &model.uptime(),
                                            add_css_class: "title-2",
                                            add_css_class: "numeric",
                                            set_halign: gtk::Align::Start,
                                        },
                                    },
                                },
                            },

                            // Resource graphs: CPU and memory, side by side.
                            gtk::Box {
                                set_orientation: gtk::Orientation::Horizontal,
                                set_spacing: 12,
                                set_homogeneous: true,

                                gtk::Box {
                                    add_css_class: "card",

                                    gtk::Box {
                                        set_orientation: gtk::Orientation::Vertical,
                                        set_spacing: 8,
                                        set_margin_all: 14,

                                        gtk::Box {
                                            set_orientation: gtk::Orientation::Horizontal,
                                            gtk::Label {
                                                set_label: "CPU",
                                                add_css_class: "caption",
                                                add_css_class: "dim-label",
                                                set_hexpand: true,
                                                set_halign: gtk::Align::Start,
                                            },
                                            gtk::Label {
                                                #[watch]
                                                set_label: &model.cpu_value(),
                                                add_css_class: "caption-heading",
                                                add_css_class: "numeric",
                                                set_halign: gtk::Align::End,
                                            },
                                        },
                                        #[local_ref]
                                        cpu_area -> gtk::DrawingArea {
                                            set_content_height: 44,
                                            set_hexpand: true,
                                        },
                                    },
                                },

                                gtk::Box {
                                    add_css_class: "card",

                                    gtk::Box {
                                        set_orientation: gtk::Orientation::Vertical,
                                        set_spacing: 8,
                                        set_margin_all: 14,

                                        gtk::Box {
                                            set_orientation: gtk::Orientation::Horizontal,
                                            gtk::Label {
                                                set_label: "MEMORY",
                                                add_css_class: "caption",
                                                add_css_class: "dim-label",
                                                set_hexpand: true,
                                                set_halign: gtk::Align::Start,
                                            },
                                            gtk::Label {
                                                #[watch]
                                                set_label: &model.mem_value(),
                                                add_css_class: "caption-heading",
                                                add_css_class: "numeric",
                                                set_halign: gtk::Align::End,
                                            },
                                        },
                                        #[local_ref]
                                        mem_area -> gtk::DrawingArea {
                                            set_content_height: 44,
                                            set_hexpand: true,
                                        },
                                    },
                                },
                            },

                            adw::PreferencesGroup {
                                set_title: "Details",

                                adw::ActionRow {
                                    set_title: "Image",
                                    set_subtitle: &model.container.image,
                                    set_subtitle_selectable: true,
                                },
                                adw::ActionRow {
                                    set_title: "ID",
                                    set_subtitle: &model.container.id,
                                    set_subtitle_selectable: true,
                                },
                                adw::ActionRow {
                                    set_title: "Command",
                                    #[watch]
                                    set_subtitle: &model.command(),
                                    set_subtitle_selectable: true,
                                },
                                adw::ActionRow {
                                    set_title: "Created",
                                    #[watch]
                                    set_subtitle: &model.created(),
                                },
                            },

                            adw::PreferencesGroup {
                                set_title: "Ports",

                                adw::ActionRow {
                                    #[watch]
                                    set_title: &model.ports(),
                                },
                            },
                        },
                    },
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let mut model = ContainerDetailPage {
            docker: init.docker,
            container: init.container,
            detail: None,
            started_unix: None,
            latest: None,
            cpu_history: Vec::new(),
            mem_history: Vec::new(),
            stats_active: false,
            cpu_draw: DrawHandler::new(),
            mem_draw: DrawHandler::new(),
        };
        let cpu_area = model.cpu_draw.drawing_area();
        let mem_area = model.mem_draw.drawing_area();
        let widgets = view_output!();

        // Inspect once now, and re-inspect every 2s so the chip/button/uptime
        // follow the container (the button's effect, or an external change).
        sender.input(DetailInput::Refresh);

        // Start streaming stats if the container is running.
        model.start_stats(&sender);

        // Two timers. Both self-remove when the page is dropped: sending to a
        // closed input channel returns Err, which we turn into `Break`.
        let ticker = sender.input_sender().clone();
        glib::timeout_add_seconds_local(1, move || control_flow(ticker.send(DetailInput::Tick)));
        let refresher = sender.input_sender().clone();
        glib::timeout_add_seconds_local(2, move || {
            control_flow(refresher.send(DetailInput::Refresh))
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            // Nothing to change — the `#[watch]` uptime setter re-runs on any
            // update and recomputes now − start.
            DetailInput::Tick => {}

            DetailInput::ToggleClicked => {
                // Decide here from current state, not in the button's closure,
                // so it's never stale (same reason as the list row).
                let id = self.container.id.clone();
                let out = if self.container.state.is_running() {
                    DetailOutput::Stop(id)
                } else {
                    DetailOutput::Start(id)
                };
                sender.output(out).ok();
            }

            DetailInput::Refresh => {
                let docker = self.docker.clone();
                let id = self.container.id.clone();
                sender.oneshot_command(async move {
                    DetailCmd::Inspected(
                        client::inspect(&docker, &id)
                            .await
                            .map_err(|err| format!("{err}")),
                    )
                });
            }
        }
    }

    fn update_cmd(
        &mut self,
        msg: Self::CommandOutput,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            DetailCmd::Inspected(Ok(detail)) => {
                // Keep the chip, button, and ports in sync with reality — a
                // container gains published ports when it starts.
                self.container.state = detail.state;
                self.container.ports = detail.ports.clone();
                self.started_unix = detail.started_at.as_deref().and_then(parse_unix);
                self.detail = Some(detail);
                // If it just came up (via the button or elsewhere), (re)start
                // the graphs.
                self.start_stats(&sender);
            }
            DetailCmd::Inspected(Err(reason)) => {
                // The basic info (from the list) still shows; just note the gap.
                tracing::warn!(%reason, "inspect failed; showing summary only");
            }

            DetailCmd::StatsSample(stats) => {
                push_capped(&mut self.cpu_history, stats.cpu_percent);
                let mem_pct = if stats.mem_limit > 0 {
                    stats.mem_used as f64 / stats.mem_limit as f64 * 100.0
                } else {
                    0.0
                };
                push_capped(&mut self.mem_history, mem_pct);
                self.latest = Some(stats);
                self.redraw();
            }

            DetailCmd::StatsEnded => {
                // Container stopped or the stream errored; allow a later restart.
                self.stats_active = false;
            }
        }
    }
}

impl ContainerDetailPage {
    /// Start the stats stream if the container is running and one isn't already
    /// going. The stream ends by itself when the container stops (Docker closes
    /// it), which flips `stats_active` back via `StatsEnded`.
    fn start_stats(&mut self, sender: &ComponentSender<Self>) {
        if self.stats_active || !self.container.state.is_running() {
            return;
        }
        self.stats_active = true;

        let docker = self.docker.clone();
        let id = self.container.id.clone();
        sender.command(move |out, shutdown| {
            shutdown
                .register(async move {
                    // `client::stats` uses `filter_map`, whose stream isn't
                    // `Unpin`; pin it so `next()` works.
                    let mut stream = std::pin::pin!(client::stats(&docker, &id));
                    while let Some(item) = stream.next().await {
                        // Skip error frames; the stream ending is what matters.
                        if let Ok(sample) = item
                            && out.send(DetailCmd::StatsSample(sample)).is_err()
                        {
                            return;
                        }
                    }
                    out.send(DetailCmd::StatsEnded).ok();
                })
                .drop_on_shutdown()
                .boxed()
        });
    }

    /// Repaint both graph surfaces from their history.
    fn redraw(&mut self) {
        // CPU can exceed 100% on multiple cores, so scale to the peak seen.
        let cpu_max = self.cpu_history.iter().copied().fold(100.0_f64, f64::max);
        let (cw, ch) = (self.cpu_draw.width(), self.cpu_draw.height());
        draw_graph(
            &self.cpu_draw.get_context(),
            cw,
            ch,
            &self.cpu_history,
            cpu_max,
            CPU_COLOR,
        );

        let (mw, mh) = (self.mem_draw.width(), self.mem_draw.height());
        draw_graph(
            &self.mem_draw.get_context(),
            mw,
            mh,
            &self.mem_history,
            100.0,
            MEM_COLOR,
        );
    }
}

/// Push a value, dropping the oldest once past the history cap.
fn push_capped(history: &mut Vec<f64>, value: f64) {
    history.push(value);
    if history.len() > HISTORY {
        history.remove(0);
    }
}

/// Draw a filled sparkline of `samples` (scaled to `max`) across the surface,
/// in the theme's text `color` so it adapts to light/dark.
fn draw_graph(
    cx: &gtk::cairo::Context,
    width: i32,
    height: i32,
    samples: &[f64],
    max: f64,
    color: gtk::gdk::RGBA,
) {
    // The DrawHandler surface keeps its last contents; clear before repainting.
    cx.set_operator(gtk::cairo::Operator::Clear);
    let _ = cx.paint();
    cx.set_operator(gtk::cairo::Operator::Over);

    let (w, h) = (width as f64, height as f64);
    if samples.len() < 2 || max <= 0.0 || w <= 0.0 {
        return;
    }

    // Anchor the newest sample to the right edge, so the line scrolls left as it
    // fills rather than stretching.
    let step = w / (HISTORY - 1) as f64;
    let point = |i: usize, count: usize| {
        let x = w - (count - 1 - i) as f64 * step;
        let v = (samples[i] / max).clamp(0.0, 1.0);
        (x, h - v * h)
    };
    let n = samples.len();

    cx.new_path();
    let (x0, y0) = point(0, n);
    cx.move_to(x0, y0);
    for i in 1..n {
        let (x, y) = point(i, n);
        cx.line_to(x, y);
    }

    let (r, g, b) = (
        color.red() as f64,
        color.green() as f64,
        color.blue() as f64,
    );
    cx.set_source_rgba(r, g, b, 0.85);
    cx.set_line_width(1.5);
    let _ = cx.stroke_preserve();

    // Fill under the line.
    let (xn, _) = point(n - 1, n);
    cx.line_to(xn, h);
    cx.line_to(x0, h);
    cx.close_path();
    cx.set_source_rgba(r, g, b, 0.12);
    let _ = cx.fill();
}

/// Bytes as a compact "340 MB" / "1.4 GB".
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Keep a repeating timer alive while its input channel is open; stop it once
/// the channel closes (the page was dropped). Saves holding the `SourceId`.
fn control_flow<E>(send_result: Result<(), E>) -> glib::ControlFlow {
    if send_result.is_ok() {
        glib::ControlFlow::Continue
    } else {
        glib::ControlFlow::Break
    }
}

impl ContainerDetailPage {
    /// Live uptime, recomputed each tick. "—" until we know the start time, and
    /// for containers that aren't running.
    fn uptime(&self) -> String {
        let Some(started) = self.started_unix else {
            return "—".to_owned();
        };
        let now = glib::DateTime::now_utc()
            .map(|d| d.to_unix())
            .unwrap_or(started);
        format_duration((now - started).max(0))
    }

    /// Current CPU %, or "—" before the first sample / when stopped.
    fn cpu_value(&self) -> String {
        match self.latest {
            Some(stats) if self.container.state.is_running() => {
                format!("{:.1}%", stats.cpu_percent)
            }
            _ => "—".to_owned(),
        }
    }

    /// Current memory as "used / limit", or "—".
    fn mem_value(&self) -> String {
        match self.latest {
            Some(stats) if self.container.state.is_running() => {
                format!(
                    "{} / {}",
                    human_bytes(stats.mem_used),
                    human_bytes(stats.mem_limit)
                )
            }
            _ => "—".to_owned(),
        }
    }

    fn command(&self) -> String {
        self.detail
            .as_ref()
            .and_then(|d| d.command.clone())
            .unwrap_or_else(|| "—".to_owned())
    }

    fn created(&self) -> String {
        self.detail
            .as_ref()
            .and_then(|d| d.created.as_deref())
            .map(pretty_time)
            .unwrap_or_else(|| "—".to_owned())
    }

    fn ports(&self) -> String {
        if self.container.ports.is_empty() {
            return "No published ports".to_owned();
        }
        self.container
            .ports
            .iter()
            .map(|port| format!("{} → {}", port.public, port.private))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Parse an RFC3339 timestamp to Unix seconds using glib (no date dependency).
fn parse_unix(rfc3339: &str) -> Option<i64> {
    glib::DateTime::from_iso8601(rfc3339, None)
        .ok()
        .map(|d| d.to_unix())
}

/// Seconds → a compact "3h 24m" / "5m 12s" / "8s". Days roll up to "2d 3h".
fn format_duration(secs: i64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Trim an RFC3339 timestamp to a readable "2026-07-17 16:12:55", or return it
/// unchanged if it isn't that shape.
fn pretty_time(rfc3339: &str) -> String {
    match glib::DateTime::from_iso8601(rfc3339, None) {
        Ok(dt) => dt
            .format("%Y-%m-%d %H:%M:%S")
            .map(|s| s.to_string())
            .unwrap_or_else(|_| rfc3339.to_owned()),
        Err(_) => rfc3339.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_by_magnitude() {
        assert_eq!(format_duration(8), "8s");
        assert_eq!(format_duration(312), "5m 12s");
        assert_eq!(format_duration(3600 * 3 + 60 * 24), "3h 24m");
        assert_eq!(format_duration(86400 * 2 + 3600 * 3), "2d 3h");
    }

    #[test]
    fn parses_docker_start_time() {
        // Docker's inspect start time is RFC3339 with nanoseconds.
        assert_eq!(parse_unix("1970-01-01T00:00:10Z"), Some(10));
        assert!(parse_unix("not a time").is_none());
    }
}
