//! One container, rendered as an `adw::ActionRow`.

use relm4::adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};

use crate::docker::types::{Container, ContainerState};

/// What a row asks the parent to do. Rows never touch Docker themselves — they
/// emit an intent and `AppModel::update` owns the decision, which keeps all
/// Docker I/O in one reducer (CLAUDE.md rule 4).
#[derive(Debug)]
pub enum ContainerRowOutput {
    Start(String),
    Stop(String),
    Restart(String),
    Remove(String),
    ShowLogs(String),
}

#[derive(Debug)]
pub struct ContainerRow {
    container: Container,
}

impl ContainerRow {
    /// "postgres:17-alpine · Up 39 minutes (healthy) · 5432:5432"
    fn subtitle(&self) -> String {
        let mut parts = vec![self.container.image.clone()];

        if !self.container.status.is_empty() {
            parts.push(self.container.status.clone());
        }

        if !self.container.ports.is_empty() {
            let ports = self
                .container
                .ports
                .iter()
                .map(|port| format!("{}:{}", port.public, port.private))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(ports);
        }

        parts.join(" · ")
    }

    /// libadwaita ships semantic colours for exactly this; no custom CSS needed.
    fn status_icon(&self) -> &'static str {
        match self.container.state {
            ContainerState::Running => "media-playback-start-symbolic",
            ContainerState::Paused => "media-playback-pause-symbolic",
            ContainerState::Restarting | ContainerState::Stopping => "view-refresh-symbolic",
            ContainerState::Dead => "dialog-error-symbolic",
            _ => "media-playback-stop-symbolic",
        }
    }

    fn status_css(&self) -> &'static str {
        match self.container.state {
            ContainerState::Running => "success",
            ContainerState::Restarting | ContainerState::Stopping | ContainerState::Paused => {
                "warning"
            }
            ContainerState::Dead => "error",
            _ => "dim-label",
        }
    }
}

#[relm4::factory(pub)]
impl FactoryComponent for ContainerRow {
    type Init = Container;
    type Input = ();
    type Output = ContainerRowOutput;
    type CommandOutput = ();
    type ParentWidget = adw::PreferencesGroup;

    view! {
        adw::ActionRow {
            set_title: &self.container.name,
            set_subtitle: &self.subtitle(),
            set_subtitle_lines: 1,

            add_prefix = &gtk::Image {
                set_icon_name: Some(self.status_icon()),
                add_css_class: self.status_css(),
            },

            add_suffix = &gtk::Button {
                set_icon_name: if self.container.state.is_running() {
                    "media-playback-stop-symbolic"
                } else {
                    "media-playback-start-symbolic"
                },
                set_valign: gtk::Align::Center,
                set_tooltip_text: Some(if self.container.state.is_running() {
                    "Stop"
                } else {
                    "Start"
                }),
                add_css_class: "flat",
                connect_clicked[sender, id = self.container.id.clone(), running = self.container.state.is_running()] => move |_| {
                    let msg = if running {
                        ContainerRowOutput::Stop(id.clone())
                    } else {
                        ContainerRowOutput::Start(id.clone())
                    };
                    sender.output(msg).ok();
                },
            },

            add_suffix = &gtk::Button {
                set_icon_name: "view-list-symbolic",
                set_valign: gtk::Align::Center,
                set_tooltip_text: Some("Logs"),
                add_css_class: "flat",
                connect_clicked[sender, id = self.container.id.clone()] => move |_| {
                    sender.output(ContainerRowOutput::ShowLogs(id.clone())).ok();
                },
            },

            add_suffix = &gtk::MenuButton {
                set_icon_name: "view-more-symbolic",
                set_valign: gtk::Align::Center,
                set_tooltip_text: Some("More"),
                add_css_class: "flat",

                #[wrap(Some)]
                set_popover = &gtk::Popover {
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 2,

                        gtk::Button {
                            set_label: "Restart",
                            add_css_class: "flat",
                            connect_clicked[sender, id = self.container.id.clone()] => move |_| {
                                sender.output(ContainerRowOutput::Restart(id.clone())).ok();
                            },
                        },

                        gtk::Button {
                            set_label: "Remove",
                            add_css_class: "flat",
                            add_css_class: "destructive-action",
                            connect_clicked[sender, id = self.container.id.clone()] => move |_| {
                                sender.output(ContainerRowOutput::Remove(id.clone())).ok();
                            },
                        },
                    },
                },
            },
        }
    }

    fn init_model(
        container: Self::Init,
        _index: &DynamicIndex,
        _sender: FactorySender<Self>,
    ) -> Self {
        Self { container }
    }
}
