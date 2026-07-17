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

/// What a row needs to exist. Carries `busy` so a row rebuilt while an action
/// is in flight doesn't lose its spinner.
#[derive(Debug)]
pub struct ContainerRowInit {
    pub container: Container,
    pub busy: bool,
}

#[derive(Debug)]
pub enum ContainerRowInput {
    /// Fresh data for this row from the poll, applied in place.
    Update(Container),
    /// An action on this container started or finished.
    SetBusy(bool),
    /// The start/stop button was clicked.
    ///
    /// The decision deliberately happens here rather than in the button's
    /// closure. A closure captures its values once, when the widget is built —
    /// so a captured `is_running` would be frozen at whatever the state was
    /// then, and every later poll would leave it more wrong. Reading
    /// `self.container` at click time is always current.
    ToggleClicked,
}

#[derive(Debug)]
pub struct ContainerRow {
    container: Container,
    /// An action is in flight for this container. Owned by the row so it
    /// survives an `Update` from the poll, which only replaces the container.
    busy: bool,
}

impl ContainerRow {
    /// Lets the parent match rows against incoming containers without cloning.
    pub fn id(&self) -> &str {
        &self.container.id
    }

    pub fn name(&self) -> &str {
        &self.container.name
    }

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
    type Init = ContainerRowInit;
    type Input = ContainerRowInput;
    type Output = ContainerRowOutput;
    type CommandOutput = ();
    type ParentWidget = adw::PreferencesGroup;

    view! {
        adw::ActionRow {
            #[watch]
            set_title: &self.container.name,
            #[watch]
            set_subtitle: &self.subtitle(),
            set_subtitle_lines: 1,

            add_prefix = &gtk::Image {
                #[watch]
                set_icon_name: Some(self.status_icon()),
                // `set_css_classes` replaces the list; `add_css_class` appends.
                // Under #[watch] the appending form would accumulate, so a
                // container that ran and then exited would end up styled both
                // "success" and "dim-label" at once.
                #[watch]
                set_css_classes: &[self.status_css()],
            },

            // Stands in for the start/stop button while Docker works. Stopping a
            // container can take the full 10s SIGTERM grace period before
            // SIGKILL, which is long enough to look like nothing happened.
            add_suffix = &gtk::Spinner {
                #[watch]
                set_visible: self.busy,
                // Only spin when shown; a hidden spinner still burns frames.
                #[watch]
                set_spinning: self.busy,
                set_valign: gtk::Align::Center,
            },

            add_suffix = &gtk::Button {
                #[watch]
                set_visible: !self.busy,
                #[watch]
                set_icon_name: if self.container.state.is_running() {
                    "media-playback-stop-symbolic"
                } else {
                    "media-playback-start-symbolic"
                },
                set_valign: gtk::Align::Center,
                #[watch]
                set_tooltip_text: Some(if self.container.state.is_running() {
                    "Stop"
                } else {
                    "Start"
                }),
                add_css_class: "flat",
                connect_clicked => ContainerRowInput::ToggleClicked,
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
                // Don't offer restart/remove on a container mid-action.
                #[watch]
                set_sensitive: !self.busy,

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

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            container: init.container,
            busy: init.busy,
        }
    }

    fn update(&mut self, msg: Self::Input, sender: FactorySender<Self>) {
        match msg {
            // Swapping the data is enough: the #[watch] setters above re-run
            // against the new value and mutate only the widgets that changed.
            ContainerRowInput::Update(container) => self.container = container,

            ContainerRowInput::SetBusy(busy) => self.busy = busy,

            ContainerRowInput::ToggleClicked => {
                let id = self.container.id.clone();
                let msg = if self.container.state.is_running() {
                    ContainerRowOutput::Stop(id)
                } else {
                    ContainerRowOutput::Start(id)
                };
                sender.output(msg).ok();
            }
        }
    }
}
