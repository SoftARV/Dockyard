// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! One container, rendered as an `adw::ActionRow`.

use relm4::adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};

use crate::components::status_chip;
use crate::docker::types::Container;

/// What a row asks the parent to do. Rows never touch Docker themselves — they
/// emit an intent and `AppModel::update` owns the decision, which keeps all
/// Docker I/O in one reducer (CLAUDE.md rule 4).
#[derive(Debug)]
pub enum ContainerRowOutput {
    Start(String),
    Stop(String),
    Restart(String),
    Remove(String),
    ShowDetails(String),
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

/// Close the menu a button lives in.
///
/// A hand-built `gtk::Popover` full of plain buttons has no idea that clicking
/// one ought to dismiss it — `autohide` only covers clicks *outside* the
/// popover. A `gtk::PopoverMenu` driven by a menu model would dismiss itself,
/// but that means GAction plumbing for a two-item menu, so we close it by hand.
///
/// The button can't hold a reference to its own popover (the popover is built
/// around it), so walk up the widget tree instead. `ancestor` returns `None`
/// rather than panicking if the shape ever changes.
fn dismiss_menu(button: &gtk::Button) {
    if let Some(popover) = button
        .ancestor(gtk::Popover::static_type())
        .and_downcast::<gtk::Popover>()
    {
        popover.popdown();
    }
}

impl ContainerRow {
    /// Lets the parent match rows against incoming containers without cloning.
    pub fn id(&self) -> &str {
        &self.container.id
    }

    pub fn name(&self) -> &str {
        &self.container.name
    }

    /// The row's container data, for handing to the detail page.
    pub fn container(&self) -> &Container {
        &self.container
    }

    /// "postgres:17-alpine · 5432:5432"
    ///
    /// Deliberately no status text: uptime and health ("Up 39 minutes
    /// (healthy)") add noise, and the running/exited state is already shown by
    /// the status chip on the left. Image and published ports are what's left.
    fn subtitle(&self) -> String {
        let mut parts = vec![self.container.image.clone()];

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

            // Clicking the row body (not a suffix button) opens the detail page.
            set_activatable: true,
            connect_activated[sender, id = self.container.id.clone()] => move |_| {
                sender.output(ContainerRowOutput::ShowDetails(id.clone())).ok();
            },

            // The status chip (same one the detail page uses) replaces the old
            // coloured status icon. `set_css_classes` replaces the whole list,
            // so the previous variant doesn't accumulate across state changes.
            add_prefix = &gtk::Label {
                set_valign: gtk::Align::Center,
                #[watch]
                set_label: status_chip::label(self.container.state),
                #[watch]
                set_css_classes: &["status-chip", status_chip::variant(self.container.state)],
            },

            // The button and its spinner share one Stack, so the row keeps a
            // single stable slot. As two separate suffixes they had different
            // natural sizes, so swapping them resized the slot and shunted
            // everything to the right of it sideways. A Stack allocates the
            // largest child's size to all of them, so the controls hold still
            // while the contents swap.
            add_suffix = &gtk::Stack {
                set_valign: gtk::Align::Center,
                set_hhomogeneous: true,
                set_vhomogeneous: true,

                add_named[Some("action")] = &gtk::Button {
                    #[watch]
                    set_icon_name: if self.container.state.is_running() {
                        "media-playback-stop-symbolic"
                    } else {
                        "media-playback-start-symbolic"
                    },
                    #[watch]
                    set_tooltip_text: Some(if self.container.state.is_running() {
                        "Stop"
                    } else {
                        "Start"
                    }),
                    add_css_class: "flat",
                    connect_clicked => ContainerRowInput::ToggleClicked,
                },

                // Stopping a container can take the full 10s SIGTERM grace
                // period before SIGKILL — long enough to look like nothing
                // happened.
                add_named[Some("busy")] = &gtk::Spinner {
                    // Keep the spinner at its natural size, centred in the
                    // button's slot, rather than stretched to fill it.
                    set_halign: gtk::Align::Center,
                    set_valign: gtk::Align::Center,
                    // Only spin while shown; a hidden spinner still burns frames.
                    #[watch]
                    set_spinning: self.busy,
                },

                // Set after the children exist: naming a child that hasn't been
                // added yet is a GTK-CRITICAL.
                #[watch]
                set_visible_child_name: if self.busy { "busy" } else { "action" },
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
                            connect_clicked[sender, id = self.container.id.clone()] => move |button| {
                                dismiss_menu(button);
                                sender.output(ContainerRowOutput::Restart(id.clone())).ok();
                            },
                        },

                        gtk::Button {
                            set_label: "Remove",
                            add_css_class: "flat",
                            add_css_class: "destructive-action",
                            // Dismiss before the dialog opens, so the menu isn't
                            // left hanging behind it.
                            connect_clicked[sender, id = self.container.id.clone()] => move |button| {
                                dismiss_menu(button);
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
