// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Root component.
//!
//! This is Redux with a compiler: `AppMsg` are the actions, `update` is the sole
//! reducer, and the view is derived from `AppModel`. Nothing here does I/O
//! inline — every Docker call is dispatched as a relm4 `Command` so the GTK main
//! thread never blocks (CLAUDE.md rule 4).

use std::collections::HashMap;

use bollard::Docker;
use relm4::actions::{AccelsPlus, RelmAction, RelmActionGroup};
use relm4::adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::gtk::{gio, glib};
use relm4::{
    Component, ComponentController, ComponentParts, ComponentSender, Controller, RelmWidgetExt,
    adw, gtk,
};
use tracing::debug;

use crate::components::container_detail::{
    ContainerDetailInit, ContainerDetailPage, DetailInput, DetailOutput,
};
use crate::components::container_row::{
    ContainerRow, ContainerRowInit, ContainerRowInput, ContainerRowOutput, RowPending,
};
use crate::docker::client;
use crate::docker::types::Container;
use crate::settings::{Settings, Theme};

const POLL_INTERVAL_SECS: u32 = 2;

// The primary menu's action group. GTK menu items invoke `GAction`s by name;
// this defines the "win" group and a stateless "about" action in it (fully
// qualified: `win.about`). The group is registered on the window in `init`,
// where the callback bridges the action to an `AppMsg`.
relm4::new_action_group!(AppMenuActionGroup, "win");
relm4::new_stateless_action!(RefreshAction, AppMenuActionGroup, "refresh");
relm4::new_stateless_action!(AboutAction, AppMenuActionGroup, "about");
relm4::new_stateless_action!(QuitAction, AppMenuActionGroup, "quit");
// No menu item — the header toggle button is search's visible affordance — but
// the action still has to exist so the Ctrl+F accelerator has something to fire.
relm4::new_stateless_action!(SearchAction, AppMenuActionGroup, "search");
relm4::new_stateless_action!(PreferencesAction, AppMenuActionGroup, "preferences");

#[derive(Debug)]
pub enum ViewState {
    Loading,
    Ready,
    Disconnected(String),
}

pub struct AppModel {
    docker: Option<Docker>,
    containers: FactoryVecDeque<ContainerRow>,
    /// The full, unfiltered set from the last poll. The factory only holds the
    /// rows the search currently shows, so it can't double as the backing store;
    /// we keep the complete list here and derive the visible rows from it.
    all_containers: Vec<Container>,
    /// The search text, as typed. Empty means no filter. Matching is
    /// case-insensitive (lowercased at compare time), so this stays original-case
    /// for display in the "no matches" page.
    query: String,
    state: ViewState,
    /// The action in flight per container id. The rows own the spinner and the
    /// transitional chip, but the mapping lives here so a row rebuilt mid-action
    /// can be handed its pending state — and its kind — back.
    pending: HashMap<String, Action>,
    /// A refresh the user actually asked for. The 2s poll deliberately doesn't
    /// set this: a spinner blinking every 2s forever is worse than no feedback.
    refreshing: bool,
    /// The poll timer, while it's running.
    ///
    /// Held so it can be removed outright when the window isn't visible, rather
    /// than left ticking and skipping work. Measured, the poll costs ~0.085% of
    /// a core — nothing — but it wakes the CPU ~2.6 times a second forever, and
    /// wakeups are what cost battery on a laptop. Doing that for a window
    /// nobody can see is just waste.
    poll: Option<glib::SourceId>,
    /// The navigation stack: container list at the root, a detail page pushed on
    /// top. Held so `update` can push/pop; a refcounted GTK handle.
    nav: adw::NavigationView,
    /// The detail page, while one is open. Holding the `Controller` keeps it
    /// alive; dropping it (on navigate-back) shuts it and its embedded logs
    /// stream down via `drop_on_shutdown`.
    detail: Option<Controller<ContainerDetailPage>>,
    /// The container id the open detail page is showing, so an `ActionDone` can
    /// be forwarded to it (to clear its "Starting…"/"Stopping…" feedback) only
    /// when it's for that container — not for some other row's action.
    detail_id: Option<String>,
    /// Held so `update` can raise toasts. This is a refcounted GTK handle, not
    /// shared model state — cloning it is just a pointer bump, and it's the
    /// standard relm4 escape hatch for widgets that are commanded rather than
    /// declared.
    toast_overlay: adw::ToastOverlay,
    /// The primary menu's "Refresh" action, held so it can be greyed out until
    /// we're connected — the same reason the old refresh button was disabled
    /// when `docker` was `None`. A menu item's enabled state can't be `#[watch]`ed
    /// from `view!`, so we toggle the `GAction` imperatively instead.
    refresh_action: gio::SimpleAction,
    /// Persisted global settings. Loaded before the app ran and handed in; the
    /// Preferences dialog edits them, and each new detail page reads the log
    /// defaults from here.
    settings: Settings,
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
    /// Open the detail page for a container.
    ShowDetails(String),
    /// The search text changed (live, per keystroke). Re-filters the list in
    /// place without touching Docker.
    SearchChanged(String),
    /// Open the About dialog (from the primary menu).
    ShowAbout,
    /// Open the Preferences dialog (from the primary menu).
    ShowPreferences,
    /// A setting changed in the Preferences dialog. Each updates the model's
    /// `settings` and persists it; the theme one also applies immediately.
    SetLogsWrap(bool),
    SetLogsTimestamps(bool),
    SetTheme(Theme),
    /// The detail page was popped — back button, Escape, swipe. Drops its
    /// controller, which stops the streams it owns (stats and logs).
    PageClosed,
    Error(String),
    /// The window became visible or stopped being visible. GTK reports this for
    /// minimised, fully obscured, *and* on-another-workspace — which is exactly
    /// the question we want answered, and more than "is it focused".
    SuspendedChanged(bool),
}

/// Results coming back from commands, i.e. off-thread work landing back on the
/// main thread. Kept separate from `AppMsg` because relm4 gives commands their
/// own channel — this is the `CommandOutput` associated type.
#[derive(Debug)]
pub enum CommandMsg {
    Connected(Box<Result<Docker, String>>),
    ContainersLoaded(Vec<Container>),
    /// A one-shot action finished. Carries the id so the right row can stop
    /// spinning — several containers can be mid-action at once — and the action
    /// so a failure can say which verb failed.
    ActionDone {
        id: String,
        action: Action,
        result: Result<(), String>,
    },
    /// Listing failed. Distinct from `ActionDone` because no row owns it.
    ListFailed(String),
}

/// The four lifecycle actions, which differ only in which client call they make.
/// Collapsing them here keeps `update` from growing four near-identical arms.
///
/// `pub` only because it rides along in `CommandMsg`, which is the component's
/// public `CommandOutput` type. The module itself isn't exported.
#[derive(Debug, Clone, Copy)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Remove,
}

impl Action {
    /// For "Couldn't {verb} {name}: {reason}".
    fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Remove => "remove",
        }
    }
}

/// Map a lifecycle `Action` to the row's transitional-chip kind. Kept here (not
/// a `From` on the row's type) because the mapping is `AppModel`'s concern — the
/// row just renders whatever kind it's handed.
fn row_pending(action: Action) -> RowPending {
    match action {
        Action::Start => RowPending::Starting,
        Action::Stop => RowPending::Stopping,
        Action::Restart => RowPending::Restarting,
        Action::Remove => RowPending::Removing,
    }
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

        self.set_pending(&id, Some(action));

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
                action,
                // `{err}` not `{err:#}`: the client already reduced this to a
                // toast-sized reason and logged the full text. Flattening the
                // whole chain here is what made the toast 234 characters.
                result: result.map_err(|err| format!("{err}")),
            }
        });
    }

    /// Start the poll, unless it's already running or there's nothing to poll.
    ///
    /// Idempotent: `SuspendedChanged(false)` can arrive when the poll is
    /// already going, and starting a second timer would double the work
    /// silently.
    fn start_poll(&mut self, sender: &ComponentSender<Self>) {
        if self.poll.is_some() || self.docker.is_none() {
            return;
        }

        debug!("starting poll");
        let input = sender.input_sender().clone();
        self.poll = Some(glib::timeout_add_seconds_local(
            POLL_INTERVAL_SECS,
            move || {
                input.send(AppMsg::Refresh).ok();
                glib::ControlFlow::Continue
            },
        ));
    }

    /// Remove the poll timer entirely, so it stops waking the CPU.
    ///
    /// `take()` matters: removing a `SourceId` twice is a programmer error in
    /// glib, so the timer has to be owned and dropped exactly once.
    fn stop_poll(&mut self) {
        if let Some(source) = self.poll.take() {
            debug!("stopping poll");
            source.remove();
        }
    }

    /// The container's name, or a short id if its row has already gone.
    fn name_of(&self, id: &str) -> String {
        self.containers
            .iter()
            .find(|row| row.id() == id)
            .map(|row| row.name().to_owned())
            .unwrap_or_else(|| id.chars().take(12).collect())
    }

    /// Track the in-flight action and tell the row to show or hide its spinner
    /// and transitional chip. `Some(action)` starts it, `None` clears it.
    fn set_pending(&mut self, id: &str, action: Option<Action>) {
        match action {
            Some(action) => {
                self.pending.insert(id.to_owned(), action);
            }
            None => {
                self.pending.remove(id);
            }
        }

        if let Some(index) = self.containers.iter().position(|row| row.id() == id) {
            self.containers.send(
                index,
                ContainerRowInput::SetPending(action.map(row_pending)),
            );
        }
    }

    /// Header subtitle: how many containers there are and how many are running.
    /// Counts the full set, not the filtered view, so it stays a stable summary
    /// of the machine regardless of any active search. Empty until connected, so
    /// the subtitle line stays hidden while loading or disconnected.
    fn header_subtitle(&self) -> String {
        if !matches!(self.state, ViewState::Ready) {
            return String::new();
        }
        let total = self.all_containers.len();
        let running = self
            .all_containers
            .iter()
            .filter(|c| c.state.is_running())
            .count();
        match total {
            0 => "No containers".to_owned(),
            1 => format!("1 container · {running} running"),
            n => format!("{n} containers · {running} running"),
        }
    }

    /// Which `Stack` page to show: the true "no containers" empty state, the
    /// "nothing matches your search" state, or the list itself.
    fn list_page(&self) -> &'static str {
        if self.all_containers.is_empty() {
            "empty"
        } else if self.containers.is_empty() {
            "no-results"
        } else {
            "list"
        }
    }

    /// Reconcile the visible rows from `all_containers` filtered by `query`.
    ///
    /// Called both when a poll delivers a fresh set and when the query changes,
    /// so "the data changed" and "the filter changed" share one path. The filter
    /// matches name and image, case-insensitively; order is preserved because the
    /// client already sorts by name, which keeps the positional id-match valid.
    fn apply_containers(&mut self) {
        let needle = self.query.to_lowercase();
        // An owned, filtered snapshot. The reconcile below borrows `self` mutably
        // (`send`/`guard`), so it can't also hold a shared borrow of
        // `self.all_containers` across the loop. Cloning the matches is the price
        // of keeping the unfiltered list as a separate backing store — cheap at
        // these counts, and only the matches are cloned.
        let filtered: Vec<Container> = self
            .all_containers
            .iter()
            .filter(|c| {
                needle.is_empty()
                    || c.name.to_lowercase().contains(&needle)
                    || c.image.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect();

        // Update rows in place while the visible id set is unchanged, rebuild
        // only when it isn't. The in-place path is what lets a poll refresh
        // status 30×/min without destroying transient widget state (an open
        // popover is parented to a row's menu button and would slam shut on a
        // rebuild); it also means a poll landing mid-search keeps the filtered
        // rows live rather than flickering.
        let unchanged_membership = self.containers.len() == filtered.len()
            && self
                .containers
                .iter()
                .zip(&filtered)
                .all(|(row, container)| row.id() == container.id);

        if unchanged_membership {
            for (index, container) in filtered.into_iter().enumerate() {
                self.containers
                    .send(index, ContainerRowInput::Update(container));
            }
        } else {
            debug!(
                count = filtered.len(),
                "visible set changed, rebuilding rows"
            );
            let mut guard = self.containers.guard();
            guard.clear();
            for container in filtered {
                // Hand the spinner state back, or a container mid-action would
                // stop spinning the instant the visible set changed under it.
                guard.push_back(ContainerRowInit {
                    pending: self.pending.get(&container.id).copied().map(row_pending),
                    container,
                });
            }
        }
    }
}

#[relm4::component(pub)]
impl Component for AppModel {
    type Init = Settings;
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = CommandMsg;

    view! {
        adw::ApplicationWindow {
            set_title: Some("Dockyard"),
            // Opens wide enough (≥720px) that clicking into a container lands on
            // the detail page's wide layout — cards in a row, info beside logs —
            // rather than the narrow fallback. The list doesn't need this width
            // (it's clamped below), but the detail page does.
            set_default_size: (900, 720),

            #[local_ref]
            toast_overlay -> adw::ToastOverlay {
                // The navigation stack. Root page = the container list; the
                // detail page is pushed on top from `update` and pops back here.
                #[local_ref]
                nav -> adw::NavigationView {
                    adw::NavigationPage {
                        set_title: "Dockyard",

                        adw::ToolbarView {
                            add_top_bar = &adw::HeaderBar {
                                // Title plus a live subtitle counting containers.
                                // `adw::WindowTitle` is the standard way to give a
                                // header bar a subtitle; it hides the subtitle line
                                // when the text is empty (loading / disconnected).
                                #[wrap(Some)]
                                set_title_widget = &adw::WindowTitle {
                                    set_title: "Dockyard",
                                    #[watch]
                                    set_subtitle: &model.header_subtitle(),
                                },

                                // The search toggle, on the far left — opposite
                                // the menu button. Two-way-bound in `init` to the
                                // search bar's reveal state, so it stays lit while
                                // search is open and pops out when it closes.
                                #[name = "search_button"]
                                pack_start = &gtk::ToggleButton {
                                    set_icon_name: "system-search-symbolic",
                                    set_tooltip_text: Some("Search"),
                                },

                                // The primary menu — the GNOME hamburger. It's a
                                // real menu model (a `gio::Menu`), not a hand-built
                                // popover, so its items invoke `GAction`s and get
                                // keyboard/screen-reader behaviour for free. Packed
                                // first so it sits at the far right.
                                pack_end = &gtk::MenuButton {
                                    set_icon_name: "open-menu-symbolic",
                                    set_tooltip_text: Some("Main Menu"),
                                    set_menu_model: Some(&primary_menu),
                                },

                                // Refresh moved into the primary menu (with a
                                // Ctrl+R / F5 accelerator); this spinner is the
                                // header's remaining refresh feedback. A local
                                // list takes ~2ms so it's usually unseen — it
                                // earns its place when the daemon is slow.
                                pack_end = &gtk::Spinner {
                                    #[watch]
                                    set_visible: model.refreshing,
                                    #[watch]
                                    set_spinning: model.refreshing,
                                    set_valign: gtk::Align::Center,
                                },
                            },

                            // The search bar, a second top bar just under the
                            // header — it reveals with the usual GNOME slide. Its
                            // entry is built and set as the child in `init` (so its
                            // `search-changed` signal can reach a message), leaving
                            // this a bare, named shell.
                            #[name = "search_bar"]
                            add_top_bar = &gtk::SearchBar {
                                set_show_close_button: true,
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

                                // Connected. A Stack rather than an `if`, so the
                                // factory's list widget isn't re-parented every
                                // time the last container goes or the first
                                // arrives — only the visible page flips.
                                ViewState::Ready => gtk::Stack {
                                    add_named[Some("list")] = &gtk::ScrolledWindow {
                                        set_vexpand: true,

                                        // Clamp the rows to a readable width instead
                                        // of letting them stretch the full (now
                                        // wider) window — a row with its title on the
                                        // far left and controls on the far right,
                                        // metres apart, reads badly. 600/400 are the
                                        // Adwaita preferences-content sizes, the same
                                        // clamp Settings uses, so the list sits
                                        // centred and familiar.
                                        adw::Clamp {
                                            set_maximum_size: 600,
                                            set_tightening_threshold: 400,
                                            set_margin_all: 12,

                                            #[local_ref]
                                            container_group -> adw::PreferencesGroup {},
                                        },
                                    },

                                    add_named[Some("empty")] = &adw::StatusPage {
                                        set_icon_name: Some("package-x-generic-symbolic"),
                                        set_title: "No Containers",
                                        set_description: Some(
                                            "Containers on this machine will appear here.",
                                        ),
                                    },

                                    // Distinct from "empty": containers exist, the
                                    // search just excluded all of them.
                                    add_named[Some("no-results")] = &adw::StatusPage {
                                        set_icon_name: Some("system-search-symbolic"),
                                        set_title: "No Matches",
                                        #[watch]
                                        set_description: Some(&format!(
                                            "No containers match “{}”.",
                                            model.query,
                                        )),
                                    },

                                    // Set after the children exist — naming a
                                    // missing child is a GTK-CRITICAL.
                                    #[watch]
                                    set_visible_child_name: model.list_page(),
                                },
                            },
                        },
                    },
                },
            },
        }
    }

    // The primary menu's model. Sibling of `view!` (the component macro wires
    // `primary_menu` into the tree above). Each item names a `GAction`, resolved
    // against the "win" group we register on the window in `init`.
    menu! {
        primary_menu: {
            section! {
                "Refresh" => RefreshAction,
            },
            section! {
                "Preferences" => PreferencesAction,
                "About Dockyard" => AboutAction,
                "Quit" => QuitAction,
            }
        }
    }

    fn init(
        settings: Self::Init,
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
                ContainerRowOutput::ShowDetails(id) => AppMsg::ShowDetails(id),
            });

        // Build the Refresh action up front so its `GAction` handle can live in
        // the model (to grey it out until connected). It's a thin bridge to
        // `ManualRefresh`, like the About action. Starts disabled; enabled in the
        // `Connected` handler.
        let refresh_sender = sender.input_sender().clone();
        let refresh_action: RelmAction<RefreshAction> = RelmAction::new_stateless(move |_| {
            refresh_sender.send(AppMsg::ManualRefresh).ok();
        });
        refresh_action.set_enabled(false);

        let model = AppModel {
            docker: None,
            containers,
            all_containers: Vec::new(),
            query: String::new(),
            state: ViewState::Loading,
            pending: HashMap::new(),
            refreshing: false,
            poll: None,
            nav: adw::NavigationView::new(),
            detail: None,
            detail_id: None,
            toast_overlay: adw::ToastOverlay::new(),
            refresh_action: refresh_action.gio_action().clone(),
            settings,
        };

        let toast_overlay = model.toast_overlay.clone();
        let nav = model.nav.clone();
        let container_group = model.containers.widget();

        // Built here, not in `view!`, so its `search-changed` signal can be wired
        // to a message and so `init` can hand the same handle both to the search
        // bar (as its child, below) and to the clear-on-close closure.
        let search_entry = gtk::SearchEntry::new();
        search_entry.set_hexpand(true);
        let search_sender = sender.input_sender().clone();
        search_entry.connect_search_changed(move |entry| {
            search_sender
                .send(AppMsg::SearchChanged(entry.text().to_string()))
                .ok();
        });

        let widgets = view_output!();

        // A pop means the user left the detail page (the only thing we push), so
        // let the reducer drop its controller and resume polling.
        let popped = sender.input_sender().clone();
        model.nav.connect_popped(move |_, _| {
            popped.send(AppMsg::PageClosed).ok();
        });

        // Search bar wiring, here rather than in `view!` because it needs the
        // built widgets: the entry becomes the bar's child, the bar captures
        // keystrokes from the whole window (type anywhere to start searching — the
        // GNOME idiom), and the header toggle is bound to the bar's reveal state.
        widgets.search_bar.set_child(Some(&search_entry));
        widgets.search_bar.connect_entry(&search_entry);
        widgets.search_bar.set_key_capture_widget(Some(&root));

        // A bidirectional GObject property binding — the two-way data binding of
        // the GTK world. Flip the toggle and the bar reveals; press Escape or the
        // bar's close button and the toggle pops back out. Because the widgets are
        // the source of truth for their own reveal state, the model needs no
        // "is search open" flag.
        widgets
            .search_button
            .bind_property("active", &widgets.search_bar, "search-mode-enabled")
            .bidirectional()
            .sync_create()
            .build();

        // Closing the bar clears the query so the full list returns. Clearing the
        // entry fires `search-changed`, which routes through `SearchChanged("")` —
        // the one and only "the query changed" path, so there's nothing to keep in
        // sync by hand.
        let clear_entry = search_entry.clone();
        widgets
            .search_bar
            .connect_search_mode_enabled_notify(move |bar| {
                if !bar.is_search_mode() {
                    clear_entry.set_text("");
                }
            });

        // Wire the menu's `win.about` action to a message. The action is GTK's
        // command mechanism (menu items can only invoke actions); we keep it a
        // thin bridge — it just posts `ShowAbout` so the dialog is built in the
        // reducer like every other UI change. Registering the group on the window
        // is what makes `win.about` resolve for the menu inside it.
        let about_sender = sender.input_sender().clone();
        let about_action: RelmAction<AboutAction> = RelmAction::new_stateless(move |_| {
            about_sender.send(AppMsg::ShowAbout).ok();
        });
        // Like About, a thin bridge: the dialog is built in the reducer.
        let prefs_sender = sender.input_sender().clone();
        let preferences_action: RelmAction<PreferencesAction> =
            RelmAction::new_stateless(move |_| {
                prefs_sender.send(AppMsg::ShowPreferences).ok();
            });
        // Quit doesn't touch the model, so it acts directly rather than posting a
        // message like the others — it just tells the application to quit, which
        // closes the window.
        let quit_action: RelmAction<QuitAction> = RelmAction::new_stateless(|_| {
            relm4::main_application().quit();
        });
        // Ctrl+F toggles search. It just flips the header toggle; the binding
        // above turns that into revealing or hiding the bar. Cloning the button
        // into the closure holds a strong ref, but both live as long as the
        // window, so there's nothing to leak.
        let search_button = widgets.search_button.clone();
        let search_action: RelmAction<SearchAction> = RelmAction::new_stateless(move |_| {
            search_button.set_active(!search_button.is_active());
        });
        let mut menu_actions = RelmActionGroup::<AppMenuActionGroup>::new();
        menu_actions.add_action(refresh_action);
        menu_actions.add_action(about_action);
        menu_actions.add_action(preferences_action);
        menu_actions.add_action(quit_action);
        menu_actions.add_action(search_action);
        menu_actions.register_for_widget(&root);

        // Common actions deserve shortcuts; the menu shows them automatically.
        let app = relm4::main_application();
        app.set_accelerators_for_action::<RefreshAction>(&["<primary>r", "F5"]);
        app.set_accelerators_for_action::<QuitAction>(&["<primary>q"]);
        app.set_accelerators_for_action::<SearchAction>(&["<primary>f"]);
        app.set_accelerators_for_action::<PreferencesAction>(&["<primary>comma"]);

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
                        Err(err) => CommandMsg::ListFailed(format!("{err}")),
                    }
                });
            }

            AppMsg::Start(id) => self.dispatch(&sender, id, Action::Start),
            AppMsg::Stop(id) => self.dispatch(&sender, id, Action::Stop),
            AppMsg::Restart(id) => self.dispatch(&sender, id, Action::Restart),
            AppMsg::RemoveConfirmed(id) => self.dispatch(&sender, id, Action::Remove),

            // Removal is destructive and irreversible, so it asks first.
            AppMsg::Remove(id) => {
                let name = self.name_of(&id);

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

            AppMsg::ShowAbout => {
                // A standard adw::AboutDialog, filled from our own metadata. The
                // icon resolves to the installed themed icon (a generic fallback
                // before `make install`); `license_type` renders the full GPL
                // notice, so we don't hand-write it. `Gpl30` is GTK's name for
                // "v3 or later" (`Gpl30Only` would be version-3-only).
                let about = adw::AboutDialog::builder()
                    .application_name("Dockyard")
                    .application_icon(crate::APP_ID)
                    .version(env!("CARGO_PKG_VERSION"))
                    .developer_name("Miguel Rincon")
                    .comments("Manage the Docker containers on your machine, natively.")
                    .website("https://github.com/SoftARV/Dockyard")
                    .issue_url("https://github.com/SoftARV/Dockyard/issues")
                    .license_type(gtk::License::Gpl30)
                    .copyright("© 2026 Miguel Rincon")
                    .build();
                about.present(Some(root));
            }

            AppMsg::ShowPreferences => {
                // Built here like About, but its rows are interactive: each is
                // seeded from the current settings, then wired to a message so the
                // reducer owns the change (update the model, persist, apply). The
                // rows' initial values are set at build time, before the signals
                // are connected, so seeding them doesn't fire a spurious save.
                let dialog = adw::PreferencesDialog::new();
                let page = adw::PreferencesPage::new();

                let logs = adw::PreferencesGroup::builder().title("Logs").build();

                let wrap_row = adw::SwitchRow::builder()
                    .title("Wrap long lines")
                    .subtitle("Default for new log panels")
                    .active(self.settings.logs_wrap)
                    .build();
                let wrap_sender = sender.input_sender().clone();
                wrap_row.connect_active_notify(move |row| {
                    wrap_sender.send(AppMsg::SetLogsWrap(row.is_active())).ok();
                });

                let ts_row = adw::SwitchRow::builder()
                    .title("Show timestamps")
                    .subtitle("Default for new log panels")
                    .active(self.settings.logs_timestamps)
                    .build();
                let ts_sender = sender.input_sender().clone();
                ts_row.connect_active_notify(move |row| {
                    ts_sender
                        .send(AppMsg::SetLogsTimestamps(row.is_active()))
                        .ok();
                });

                logs.add(&wrap_row);
                logs.add(&ts_row);

                let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
                let theme_row = adw::ComboRow::builder().title("Theme").build();
                // A plain string dropdown: Follow system / Light / Dark, indexed
                // by `Theme::as_index`.
                theme_row.set_model(Some(&gtk::StringList::new(&[
                    "Follow system",
                    "Light",
                    "Dark",
                ])));
                theme_row.set_selected(self.settings.theme.as_index());
                let theme_sender = sender.input_sender().clone();
                theme_row.connect_selected_notify(move |row| {
                    theme_sender
                        .send(AppMsg::SetTheme(Theme::from_index(row.selected())))
                        .ok();
                });
                appearance.add(&theme_row);

                page.add(&logs);
                page.add(&appearance);
                dialog.add(&page);
                dialog.present(Some(root));
            }

            AppMsg::SetLogsWrap(wrap) => {
                self.settings.logs_wrap = wrap;
                self.settings.save();
            }

            AppMsg::SetLogsTimestamps(show) => {
                self.settings.logs_timestamps = show;
                self.settings.save();
            }

            AppMsg::SetTheme(theme) => {
                self.settings.theme = theme;
                self.settings.save();
                self.settings.apply_theme();
            }

            AppMsg::ShowDetails(id) => {
                let Some(docker) = self.docker.clone() else {
                    return;
                };
                let Some(container) = self.containers.iter().find(|row| row.id() == id) else {
                    return;
                };

                let controller = ContainerDetailPage::builder()
                    .launch(ContainerDetailInit {
                        docker,
                        container: container.container().clone(),
                        logs_wrap: self.settings.logs_wrap,
                        logs_timestamps: self.settings.logs_timestamps,
                    })
                    // The detail page's start/stop button emits an intent; the
                    // reducer dispatches it, same as a row.
                    .forward(sender.input_sender(), |output| match output {
                        DetailOutput::Start(id) => AppMsg::Start(id),
                        DetailOutput::Stop(id) => AppMsg::Stop(id),
                    });
                self.nav.push(controller.widget());
                self.detail = Some(controller);
                self.detail_id = Some(id);
                self.stop_poll();
            }

            AppMsg::PageClosed => {
                // Dropping the detail controller shuts it — and its embedded
                // logs stream — down. Resume the list.
                self.detail = None;
                self.detail_id = None;
                self.start_poll(&sender);
                sender.input(AppMsg::Refresh);
            }

            AppMsg::SuspendedChanged(suspended) => {
                debug!(suspended, "window visibility changed");
                if suspended {
                    self.stop_poll();
                } else {
                    self.start_poll(&sender);
                    // Whatever we last drew is now as stale as the pause was
                    // long, so refresh immediately rather than showing old
                    // state until the first tick.
                    sender.input(AppMsg::Refresh);
                }
            }

            AppMsg::SearchChanged(query) => {
                self.query = query;
                self.apply_containers();
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
        root: &Self::Root,
    ) {
        match msg {
            CommandMsg::Connected(result) => match *result {
                Ok(docker) => {
                    self.docker = Some(docker);
                    self.state = ViewState::Ready;
                    // Now that there's a daemon to talk to, the menu's Refresh
                    // item (and its Ctrl+R / F5 shortcut) becomes usable.
                    self.refresh_action.set_enabled(true);

                    // GTK already knows whether the window is worth drawing;
                    // "suspended" covers minimised, fully obscured and
                    // on-another-workspace. Let it decide when polling is
                    // pointless rather than guessing from focus.
                    let input = sender.input_sender().clone();
                    root.connect_suspended_notify(move |window| {
                        input
                            .send(AppMsg::SuspendedChanged(window.is_suspended()))
                            .ok();
                    });

                    // Phase 1 refresh strategy: dumb 2s poll. Events come later,
                    // once this works end to end.
                    self.start_poll(&sender);
                    sender.input(AppMsg::Refresh);
                }
                Err(reason) => {
                    self.state = ViewState::Disconnected(reason);
                }
            },

            CommandMsg::ContainersLoaded(containers) => {
                // Store the full set, then let `apply_containers` filter it by the
                // current query and reconcile the rows. The poll fires every 2s,
                // so this runs 30×/min; the in-place reconcile inside
                // `apply_containers` is what keeps that from thrashing widgets.
                self.all_containers = containers;
                self.apply_containers();
                self.refreshing = false;
            }

            CommandMsg::ActionDone { id, action, result } => {
                self.set_pending(&id, None);
                let ok = result.is_ok();

                if let Err(reason) = result {
                    // Name the container rather than echoing Docker's 64-char
                    // id. The row may already be gone, so fall back to a short
                    // id — the toast still beats saying nothing.
                    let name = self.name_of(&id);
                    sender.input(AppMsg::Error(format!(
                        "Couldn't {} {name}: {reason}",
                        action.verb()
                    )));
                }

                // If the open detail page is showing this container, tell it the
                // action settled so it drops its "Starting…"/"Stopping…" feedback
                // — right away on failure (the state won't flip to clear it),
                // and on success once its refresh sees the new state land.
                if self.detail_id.as_deref() == Some(id.as_str())
                    && let Some(detail) = &self.detail
                {
                    detail.sender().emit(DetailInput::ActionFinished(ok));
                }

                // Don't wait up to 2s for the poll to notice: the user just
                // asked for this, so show the result now.
                sender.input(AppMsg::Refresh);
            }

            CommandMsg::ListFailed(reason) => {
                self.refreshing = false;
                sender.input(AppMsg::Error(format!("Couldn't refresh: {reason}")));
            }
        }
    }
}
