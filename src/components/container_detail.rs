//! The container detail view — a page pushed onto the `NavigationView` when a
//! row is clicked. Cards for status/uptime, details, and ports.
//!
//! (Resource graphs come in a follow-up; this is the data-only version.)
//!
//! Two loads on open: the `Container` we already have from the list is shown
//! immediately, and a one-shot `inspect` fills in start time, command and
//! created time when it lands. A 1s timer ticks the uptime.

use bollard::Docker;
use relm4::adw::prelude::*;
use relm4::gtk::glib;
use relm4::{Component, ComponentParts, ComponentSender, RelmWidgetExt, adw, gtk};

use crate::components::status_chip;
use crate::docker::client;
use crate::docker::types::{Container, ContainerDetail};

pub struct ContainerDetailInit {
    pub docker: Docker,
    /// What the list already knows — shown at once, before `inspect` returns.
    pub container: Container,
}

pub struct ContainerDetailPage {
    container: Container,
    /// Filled in when `inspect` returns; `None` until then.
    detail: Option<ContainerDetail>,
    /// Container start time as Unix seconds, parsed from `detail.started_at`.
    /// Drives the live uptime; `None` when not running or not yet loaded.
    started_unix: Option<i64>,
}

#[derive(Debug)]
pub enum DetailInput {
    /// The uptime timer fired; re-render (the `#[watch]` uptime recomputes).
    Tick,
}

#[derive(Debug)]
pub enum DetailCmd {
    Inspected(Result<ContainerDetail, String>),
}

#[relm4::component(pub)]
impl Component for ContainerDetailPage {
    type Init = ContainerDetailInit;
    type Input = DetailInput;
    type Output = ();
    type CommandOutput = DetailCmd;

    view! {
        adw::NavigationPage {
            set_title: &model.container.name,

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

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
        let model = ContainerDetailPage {
            container: init.container,
            detail: None,
            started_unix: None,
        };
        let widgets = view_output!();

        // Fetch the richer detail off-thread.
        let docker = init.docker;
        let id = model.container.id.clone();
        sender.oneshot_command(async move {
            DetailCmd::Inspected(
                client::inspect(&docker, &id)
                    .await
                    .map_err(|err| format!("{err}")),
            )
        });

        // Tick the uptime once a second. Held nowhere: the component owns the
        // whole page, so the timer stops when the page (and this sender) is
        // dropped on navigate-back.
        let ticker = sender.input_sender().clone();
        glib::timeout_add_seconds_local(1, move || {
            ticker.send(DetailInput::Tick).ok();
            glib::ControlFlow::Continue
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            // Nothing to change — the `#[watch]` uptime setter re-runs on any
            // update and recomputes now − start.
            DetailInput::Tick => {}
        }
    }

    fn update_cmd(
        &mut self,
        msg: Self::CommandOutput,
        _sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            DetailCmd::Inspected(Ok(detail)) => {
                self.started_unix = detail.started_at.as_deref().and_then(parse_unix);
                self.detail = Some(detail);
            }
            DetailCmd::Inspected(Err(reason)) => {
                // The basic info (from the list) still shows; just note the gap.
                tracing::warn!(%reason, "inspect failed; showing summary only");
            }
        }
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
