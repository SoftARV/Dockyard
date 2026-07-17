//! Root component.
//!
//! This is Redux with a compiler: `AppMsg` are the actions, `update` is the sole
//! reducer, and the view is derived from `AppModel`. Nothing here does I/O
//! inline — every Docker call is dispatched as a relm4 `Command` so the GTK main
//! thread never blocks (CLAUDE.md rule 4).

use std::collections::HashSet;

use bollard::Docker;
use relm4::adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::gtk::glib;
use relm4::{Component, ComponentParts, ComponentSender, RelmWidgetExt, adw, gtk};
use tracing::debug;

use crate::components::container_row::{
    ContainerRow, ContainerRowInit, ContainerRowInput, ContainerRowOutput,
};
use crate::docker::client;
use crate::docker::types::Container;

const POLL_INTERVAL_SECS: u32 = 2;

#[derive(Debug)]
pub enum ViewState {
    Loading,
    Ready,
    Disconnected(String),
}

pub struct AppModel {
    docker: Option<Docker>,
    containers: FactoryVecDeque<ContainerRow>,
    state: ViewState,
    /// Containers with an action in flight. The rows own the spinner, but the
    /// set lives here so a row rebuilt mid-action can be given its state back.
    busy: HashSet<String>,
    /// A refresh the user actually asked for. The 2s poll deliberately doesn't
    /// set this: a spinner blinking every 2s forever is worse than no feedback.
    refreshing: bool,
    /// Held so `update` can raise toasts. This is a refcounted GTK handle, not
    /// shared model state — cloning it is just a pointer bump, and it's the
    /// standard relm4 escape hatch for widgets that are commanded rather than
    /// declared.
    toast_overlay: adw::ToastOverlay,
}

#[derive(Debug)]
pub enum AppMsg {
    /// The 2s poll. Silent — no visible indicator.
    Refresh,
    /// The user pressed the refresh button. Shows a spinner.
    ManualRefresh,
    Start(String),
    Stop(String),
    Restart(String),
    /// Asks for confirmation; does not remove anything.
    Remove(String),
    /// The user confirmed the dialog. This is the one that actually removes.
    RemoveConfirmed(String),
    ShowLogs(String),
    Error(String),
}

/// Results coming back from commands, i.e. off-thread work landing back on the
/// main thread. Kept separate from `AppMsg` because relm4 gives commands their
/// own channel — this is the `CommandOutput` associated type.
#[derive(Debug)]
pub enum CommandMsg {
    Connected(Box<Result<Docker, String>>),
    ContainersLoaded(Vec<Container>),
    /// A one-shot action finished. Carries the id so the right row can stop
    /// spinning — several containers can be mid-action at once.
    ActionDone {
        id: String,
        result: Result<(), String>,
    },
    /// Listing failed. Distinct from `ActionDone` because no row owns it.
    ListFailed(String),
}

/// The four lifecycle actions, which differ only in which client call they make.
/// Collapsing them here keeps `update` from growing four near-identical arms.
#[derive(Debug, Clone, Copy)]
enum Action {
    Start,
    Stop,
    Restart,
    Remove,
}

impl AppModel {
    fn toast(&self, message: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(message));
    }

    /// Fire a container action off-thread, spinning the row until it lands.
    fn dispatch(&mut self, sender: &ComponentSender<Self>, id: String, action: Action) {
        let Some(docker) = self.docker.clone() else {
            return;
        };

        self.set_busy(&id, true);

        let action_id = id.clone();
        sender.oneshot_command(async move {
            let result = match action {
                Action::Start => client::start_container(&docker, &action_id).await,
                Action::Stop => client::stop_container(&docker, &action_id).await,
                Action::Restart => client::restart_container(&docker, &action_id).await,
                Action::Remove => client::remove_container(&docker, &action_id).await,
            };
            // A container can vanish between the poll that drew the row and the
            // click on it, so failure here is routine, not exceptional.
            CommandMsg::ActionDone {
                id: action_id,
                result: result.map_err(|err| format!("{err:#}")),
            }
        });
    }

    /// Track the action and tell the row to show or hide its spinner.
    fn set_busy(&mut self, id: &str, busy: bool) {
        if busy {
            self.busy.insert(id.to_owned());
        } else {
            self.busy.remove(id);
        }

        if let Some(index) = self.containers.iter().position(|row| row.id() == id) {
            self.containers
                .send(index, ContainerRowInput::SetBusy(busy));
        }
    }
}

#[relm4::component(pub)]
impl Component for AppModel {
    type Init = ();
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = CommandMsg;

    view! {
        adw::ApplicationWindow {
            set_title: Some("Dockyard"),
            set_default_size: (540, 720),

            #[local_ref]
            toast_overlay -> adw::ToastOverlay {
                adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        pack_end = &gtk::Button {
                            set_icon_name: "view-refresh-symbolic",
                            set_tooltip_text: Some("Refresh"),
                            #[watch]
                            set_visible: !model.refreshing,
                            #[watch]
                            set_sensitive: model.docker.is_some(),
                            connect_clicked => AppMsg::ManualRefresh,
                        },

                        // Only ever shown for a refresh the user asked for. A
                        // local list call takes ~2ms so this usually isn't seen
                        // at all — it earns its place when the daemon is slow.
                        pack_end = &gtk::Spinner {
                            #[watch]
                            set_visible: model.refreshing,
                            #[watch]
                            set_spinning: model.refreshing,
                            set_valign: gtk::Align::Center,
                        },
                    },

                    #[wrap(Some)]
                    set_content = match &model.state {
                        ViewState::Loading => gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_valign: gtk::Align::Center,
                            gtk::Spinner {
                                set_spinning: true,
                                set_size_request: (32, 32),
                            },
                        },

                        ViewState::Disconnected(reason) => adw::StatusPage {
                            set_icon_name: Some("network-offline-symbolic"),
                            set_title: "Docker isn't reachable",
                            #[watch]
                            set_description: Some(reason),
                        },

                        ViewState::Ready => gtk::ScrolledWindow {
                            set_vexpand: true,
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_margin_all: 12,

                                #[local_ref]
                                container_group -> adw::PreferencesGroup {},
                            },
                        },
                    },
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let containers = FactoryVecDeque::builder()
            .launch(adw::PreferencesGroup::default())
            // Rows emit intents; the reducer decides what they mean.
            .forward(sender.input_sender(), |output| match output {
                ContainerRowOutput::Start(id) => AppMsg::Start(id),
                ContainerRowOutput::Stop(id) => AppMsg::Stop(id),
                ContainerRowOutput::Restart(id) => AppMsg::Restart(id),
                ContainerRowOutput::Remove(id) => AppMsg::Remove(id),
                ContainerRowOutput::ShowLogs(id) => AppMsg::ShowLogs(id),
            });

        let model = AppModel {
            docker: None,
            containers,
            state: ViewState::Loading,
            busy: HashSet::new(),
            refreshing: false,
            toast_overlay: adw::ToastOverlay::new(),
        };

        let toast_overlay = model.toast_overlay.clone();
        let container_group = model.containers.widget();
        let widgets = view_output!();

        // Connecting touches the network, so it can't happen inline in `init`.
        sender.oneshot_command(async {
            CommandMsg::Connected(Box::new(
                client::connect().await.map_err(|err| format!("{err:#}")),
            ))
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match msg {
            AppMsg::ManualRefresh => {
                self.refreshing = true;
                sender.input(AppMsg::Refresh);
            }

            AppMsg::Refresh => {
                let Some(docker) = self.docker.clone() else {
                    return;
                };
                // `Docker` is an Arc-backed handle, so this clone is cheap and is
                // the intended way to hand it to a task: the future must be
                // 'static and Send, which borrowing `&self.docker` can't satisfy.
                sender.oneshot_command(async move {
                    match client::list_containers(&docker).await {
                        Ok(containers) => CommandMsg::ContainersLoaded(containers),
                        Err(err) => CommandMsg::ListFailed(format!("{err:#}")),
                    }
                });
            }

            AppMsg::Start(id) => self.dispatch(&sender, id, Action::Start),
            AppMsg::Stop(id) => self.dispatch(&sender, id, Action::Stop),
            AppMsg::Restart(id) => self.dispatch(&sender, id, Action::Restart),
            AppMsg::RemoveConfirmed(id) => self.dispatch(&sender, id, Action::Remove),

            // Removal is destructive and irreversible, so it asks first.
            AppMsg::Remove(id) => {
                let name = self
                    .containers
                    .iter()
                    .find(|row| row.id() == id)
                    .map(|row| row.name().to_owned())
                    .unwrap_or_else(|| id.chars().take(12).collect());

                let dialog = adw::AlertDialog::new(
                    Some("Remove container?"),
                    Some(&format!(
                        "“{name}” will be permanently removed. This can't be undone."
                    )),
                );
                dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
                dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                // Both default and close land on cancel, so Esc or a stray
                // Enter can't destroy anything.
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");

                let input = sender.input_sender().clone();
                dialog.connect_response(None, move |_, response| {
                    if response == "remove" {
                        input.send(AppMsg::RemoveConfirmed(id.clone())).ok();
                    }
                });
                dialog.present(Some(root));
            }

            // TODO: push a logs page onto the NavigationView.
            AppMsg::ShowLogs(id) => {
                let short: String = id.chars().take(12).collect();
                self.toast(&format!("Logs for {short} aren't built yet"));
            }

            AppMsg::Error(message) => {
                tracing::error!(%message);
                self.toast(&message);
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
            CommandMsg::Connected(result) => match *result {
                Ok(docker) => {
                    self.docker = Some(docker);
                    self.state = ViewState::Ready;

                    // Phase 1 refresh strategy: dumb 2s poll. Events come later,
                    // once this works end to end.
                    let input = sender.input_sender().clone();
                    glib::timeout_add_seconds_local(POLL_INTERVAL_SECS, move || {
                        input.send(AppMsg::Refresh).ok();
                        glib::ControlFlow::Continue
                    });

                    sender.input(AppMsg::Refresh);
                }
                Err(reason) => {
                    self.state = ViewState::Disconnected(reason);
                }
            },

            CommandMsg::ContainersLoaded(containers) => {
                // The poll fires every 2s, so what happens here happens 30 times
                // a minute. Rebuilding the rows would destroy and recreate every
                // widget each time, which throws away transient UI state — an
                // open popover is parented to a row's menu button, so it would
                // slam shut on the next tick.
                //
                // Containers are sorted by name, so in the steady state the ids
                // line up positionally and we can update each row in place.
                let unchanged_membership = self.containers.len() == containers.len()
                    && self
                        .containers
                        .iter()
                        .zip(&containers)
                        .all(|(row, container)| row.id() == container.id);

                if unchanged_membership {
                    for (index, container) in containers.into_iter().enumerate() {
                        self.containers
                            .send(index, ContainerRowInput::Update(container));
                    }
                } else {
                    // A container appeared or disappeared. Rebuilding is fine
                    // here: it's rare, and the row set genuinely changed.
                    debug!(
                        count = containers.len(),
                        "container set changed, rebuilding rows"
                    );
                    let mut guard = self.containers.guard();
                    guard.clear();
                    for container in containers {
                        // Hand the spinner state back, or a container being
                        // removed would stop spinning the instant some *other*
                        // container happened to appear.
                        guard.push_back(ContainerRowInit {
                            busy: self.busy.contains(&container.id),
                            container,
                        });
                    }
                }

                self.refreshing = false;
            }

            CommandMsg::ActionDone { id, result } => {
                self.set_busy(&id, false);

                if let Err(err) = result {
                    sender.input(AppMsg::Error(err));
                }

                // Don't wait up to 2s for the poll to notice: the user just
                // asked for this, so show the result now.
                sender.input(AppMsg::Refresh);
            }

            CommandMsg::ListFailed(err) => {
                self.refreshing = false;
                sender.input(AppMsg::Error(err));
            }
        }
    }
}
