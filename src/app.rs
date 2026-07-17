//! Root component.
//!
//! This is Redux with a compiler: `AppMsg` are the actions, `update` is the sole
//! reducer, and the view is derived from `AppModel`. Nothing here does I/O
//! inline — every Docker call is dispatched as a relm4 `Command` so the GTK main
//! thread never blocks (CLAUDE.md rule 4).

use bollard::Docker;
use relm4::adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::gtk::glib;
use relm4::{Component, ComponentParts, ComponentSender, RelmWidgetExt, adw, gtk};
use tracing::debug;

use crate::components::container_row::{ContainerRow, ContainerRowInput, ContainerRowOutput};
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
    /// Held so `update` can raise toasts. This is a refcounted GTK handle, not
    /// shared model state — cloning it is just a pointer bump, and it's the
    /// standard relm4 escape hatch for widgets that are commanded rather than
    /// declared.
    toast_overlay: adw::ToastOverlay,
}

#[derive(Debug)]
pub enum AppMsg {
    Refresh,
    Start(String),
    Stop(String),
    Restart(String),
    Remove(String),
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
    /// A one-shot action finished; refresh to pick up the new state.
    ActionDone(Result<(), String>),
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

    /// Fire a container action off-thread and refresh once it lands.
    fn dispatch(&self, sender: &ComponentSender<Self>, id: String, action: Action) {
        let Some(docker) = self.docker.clone() else {
            return;
        };

        sender.oneshot_command(async move {
            let result = match action {
                Action::Start => client::start_container(&docker, &id).await,
                Action::Stop => client::stop_container(&docker, &id).await,
                Action::Restart => client::restart_container(&docker, &id).await,
                Action::Remove => client::remove_container(&docker, &id).await,
            };
            // A container can vanish between the poll that drew the row and the
            // click on it, so failure here is routine, not exceptional.
            CommandMsg::ActionDone(result.map_err(|err| format!("{err:#}")))
        });
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
                            set_sensitive: model.docker.is_some(),
                            connect_clicked => AppMsg::Refresh,
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

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
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
                        Err(err) => CommandMsg::ActionDone(Err(format!("{err:#}"))),
                    }
                });
            }

            AppMsg::Start(id) => self.dispatch(&sender, id, Action::Start),
            AppMsg::Stop(id) => self.dispatch(&sender, id, Action::Stop),
            AppMsg::Restart(id) => self.dispatch(&sender, id, Action::Restart),
            AppMsg::Remove(id) => self.dispatch(&sender, id, Action::Remove),

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
                        guard.push_back(container);
                    }
                }
            }

            CommandMsg::ActionDone(result) => {
                if let Err(err) = result {
                    sender.input(AppMsg::Error(err));
                }
            }
        }
    }
}
