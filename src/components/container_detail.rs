// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The container detail view — a page pushed onto the `NavigationView` when a
//! row is clicked. Dashboard cards for status, uptime, CPU and memory, plus
//! details, ports, and an embedded live log panel.
//!
//! Three things load on open: the `Container` from the list shows at once; a
//! one-shot `inspect` (re-run every 2s) fills in state, start time, command; and
//! a `stats` stream feeds the CPU/memory graphs while the container runs. A 1s
//! timer ticks the uptime.
//!
//! The layout is responsive via an `adw::BreakpointBin`. Below 720px the four
//! stat cards stack 2×2 and the info/log panels stack vertically; at or above
//! 720px the cards form one row of four and info sits left of the logs.

use bollard::Docker;
use futures_util::{FutureExt, StreamExt};
use relm4::adw::prelude::*;
use relm4::gtk::glib;
use relm4::{
    Component, ComponentController, ComponentParts, ComponentSender, Controller, RelmWidgetExt,
    adw, gtk,
};

use crate::components::logs_view::{LogsInit, LogsInput, LogsView};
use crate::components::sparkline::{Scale, Sparkline, SparklineInit, SparklineInput};
use crate::components::status_chip;
use crate::docker::client;
use crate::docker::types::{Container, ContainerDetail, Stats};

/// Window width at which the layout switches from stacked to side-by-side.
const WIDE_BREAKPOINT: &str = "min-width: 720px";

/// Graph line colours (Adwaita blue-3 / purple-3), handed to each `Sparkline`.
/// Fixed rather than theme-derived, so CPU and memory stay visually distinct.
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
    /// The most recent stats sample, for the current CPU/memory value labels.
    latest: Option<Stats>,
    /// Whether a stats stream is currently running, so we don't start a second
    /// when a re-inspect confirms the container is still up.
    stats_active: bool,
    /// The two graphs. Each owns its own history and drawing; we just feed them
    /// samples. Holding the `Controller`s keeps them alive with this page.
    cpu_graph: Controller<Sparkline>,
    mem_graph: Controller<Sparkline>,
    /// The embedded log panel. Holding its `Controller` is what keeps the log
    /// stream alive; when this page's controller is dropped (navigate-back), so
    /// is this, which shuts the stream down via `drop_on_shutdown`.
    logs: Controller<LogsView>,
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
                #[name = "breakpoint_bin"]
                set_content = &adw::BreakpointBin {
                    // The BreakpointBin measures its own (window) width, so it
                    // needs a minimum. Below this the window can't shrink further.
                    set_size_request: (300, 200),

                    #[wrap(Some)]
                    set_child = &gtk::ScrolledWindow {
                        set_vexpand: true,

                        adw::Clamp {
                            // Wide enough that side-by-side has room at ≥720px,
                            // capped so it doesn't sprawl on an ultrawide monitor.
                            set_maximum_size: 1400,
                            set_tightening_threshold: 800,
                            set_margin_all: 18,

                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_spacing: 18,

                                // The four stat cards. A FlowBox reflows them:
                                // 2 per line narrow (2×2), bumped to 4 (one row)
                                // by the breakpoint below. Homogeneous so they
                                // share the width equally.
                                #[name = "cards"]
                                gtk::FlowBox {
                                    set_orientation: gtk::Orientation::Horizontal,
                                    set_selection_mode: gtk::SelectionMode::None,
                                    set_homogeneous: true,
                                    set_column_spacing: 12,
                                    set_row_spacing: 12,
                                    set_min_children_per_line: 2,
                                    set_max_children_per_line: 2,

                                    // Status card — the chip on its own card.
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

                                    // Uptime card.
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

                                    // CPU card. The sparkline is a child
                                    // component appended to this card's body in
                                    // `init`, below the caption/value header.
                                    gtk::Box {
                                        add_css_class: "card",

                                        #[name = "cpu_card"]
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
                                        },
                                    },

                                    // Memory card. Same shape as the CPU card; its
                                    // sparkline is appended in `init` too.
                                    gtk::Box {
                                        add_css_class: "card",

                                        #[name = "mem_card"]
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
                                        },
                                    },
                                },

                                // Info (details + ports) and the logs. A grid so
                                // the two can split by a fixed ratio when wide.
                                // Narrow default: both at column 0, stacked in
                                // rows 0 and 1 (one column → full width). The
                                // breakpoint moves the logs up beside the info and
                                // spans the columns 2:3, i.e. a 40/60 split, using
                                // `column-homogeneous` equal columns. `hexpand` on
                                // the children makes the columns fill the width.
                                #[name = "body"]
                                gtk::Grid {
                                    set_column_spacing: 18,
                                    set_row_spacing: 18,
                                    set_column_homogeneous: true,
                                    set_vexpand: true,

                                    #[name = "info"]
                                    attach[0, 0, 1, 1] = &gtk::Box {
                                        set_orientation: gtk::Orientation::Vertical,
                                        set_spacing: 18,
                                        set_hexpand: true,

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
                                    // The logs are attached at (0, 1) in `init`,
                                    // since it's an existing controller widget.
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
        // The log panel streams on its own; holding the controller keeps that
        // stream alive. `.detach()` because it reports nothing back.
        let logs = LogsView::builder()
            .launch(LogsInit {
                docker: init.docker.clone(),
                id: init.container.id.clone(),
            })
            .detach();

        // The two graphs, configured with their colour and axis scaling. CPU can
        // exceed 100% across cores, so its axis grows to the peak; memory is a
        // percentage of the limit, so it's fixed at 100. Neither reports back, so
        // `.detach()`.
        let cpu_graph = Sparkline::builder()
            .launch(SparklineInit {
                color: CPU_COLOR,
                scale: Scale::PeakFloor(100.0),
            })
            .detach();
        let mem_graph = Sparkline::builder()
            .launch(SparklineInit {
                color: MEM_COLOR,
                scale: Scale::Fixed(100.0),
            })
            .detach();

        let mut model = ContainerDetailPage {
            docker: init.docker,
            container: init.container,
            detail: None,
            started_unix: None,
            latest: None,
            stats_active: false,
            cpu_graph,
            mem_graph,
            logs,
        };
        let widgets = view_output!();

        // Slot each sparkline into its card body, below the caption/value header.
        // Like the logs panel, these are existing controller widgets, so they're
        // appended here rather than built in the `view!`.
        widgets.cpu_card.append(model.cpu_graph.widget());
        widgets.mem_card.append(model.mem_graph.widget());

        // Attach the logs panel below the info column (row 1) — the stacked
        // default. It's an existing controller widget, so it's attached here
        // rather than in the `view!` grid. `hexpand` makes the equal grid columns
        // fill the width; the panel's own root already vexpands, which lets its
        // row take the remaining height.
        widgets.body.attach(model.logs.widget(), 0, 1, 1, 1);
        model.logs.widget().set_hexpand(true);

        // Responsive layout. At ≥720px the four cards form one row and the
        // info/logs grid goes side-by-side. A `Breakpoint` records the original
        // values and restores them below the threshold, so the narrow stack needs
        // no undoing. Parse can't really fail on a constant, but rule 5 forbids
        // `unwrap`: on the impossible error we just keep the stacked layout.
        if let Ok(condition) = adw::BreakpointCondition::parse(WIDE_BREAKPOINT) {
            let breakpoint = adw::Breakpoint::new(condition);
            breakpoint.add_setter(
                &widgets.cards,
                "min-children-per-line",
                Some(&4u32.to_value()),
            );
            breakpoint.add_setter(
                &widgets.cards,
                "max-children-per-line",
                Some(&4u32.to_value()),
            );

            // The 40/60 split lives on the grid's layout children. Side-by-side,
            // info spans 2 of the 5 equal columns (40%) and the logs move up to
            // row 0 spanning the other 3 (60%). Restored to the stacked
            // single-column positions when the breakpoint lifts. Setting these
            // by property (rather than `set_column_span` etc.) is what lets the
            // breakpoint capture and later restore the originals.
            if let Some(grid) = widgets.body.layout_manager() {
                let info_lc = grid.layout_child(&widgets.info);
                let logs_lc = grid.layout_child(model.logs.widget());
                breakpoint.add_setter(&info_lc, "column-span", Some(&2i32.to_value()));
                breakpoint.add_setter(&logs_lc, "column", Some(&2i32.to_value()));
                breakpoint.add_setter(&logs_lc, "row", Some(&0i32.to_value()));
                breakpoint.add_setter(&logs_lc, "column-span", Some(&3i32.to_value()));
            }

            widgets.breakpoint_bin.add_breakpoint(breakpoint);
        }

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
                // the graphs and the log stream. Both self-guard against a
                // double subscribe, so sending on every running poll is fine.
                self.start_stats(&sender);
                if self.container.state.is_running() {
                    self.logs.sender().emit(LogsInput::EnsureStreaming);
                }
            }
            DetailCmd::Inspected(Err(reason)) => {
                // The basic info (from the list) still shows; just note the gap.
                tracing::warn!(%reason, "inspect failed; showing summary only");
            }

            DetailCmd::StatsSample(stats) => {
                let mem_pct = if stats.mem_limit > 0 {
                    stats.mem_used as f64 / stats.mem_limit as f64 * 100.0
                } else {
                    0.0
                };
                // Hand each graph its sample; the value labels beside them read
                // `latest` via #[watch] on the next update pass.
                self.cpu_graph
                    .sender()
                    .emit(SparklineInput::Push(stats.cpu_percent));
                self.mem_graph.sender().emit(SparklineInput::Push(mem_pct));
                self.latest = Some(stats);
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
