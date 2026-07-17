//! The streaming log view — a detail page pushed onto the `NavigationView`.
//!
//! This is the first component that streams. Everything else uses
//! `oneshot_command` (fire an async call, get one result back). Logs need
//! `command`, which hands the closure a `Sender` to push *many* messages over
//! time and a `ShutdownReceiver` to stop. `drop_on_shutdown()` ties the stream's
//! life to the component's: when the parent drops this page's `Controller`
//! (on navigate-back), the future is dropped and the follow stops. No manual
//! cancellation token, no leaked stream running behind a page you've left.

use bollard::Docker;
use futures_util::{FutureExt, StreamExt};
use relm4::adw::prelude::*;
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

use crate::docker::client;

/// A chatty container follows forever, so the buffer is bounded: past this many
/// lines the oldest are dropped. 5000 lines of logs is a few MB of text — plenty
/// of scrollback, nowhere near unbounded.
const MAX_LINES: i32 = 5000;

pub struct LogsInit {
    pub docker: Docker,
    pub id: String,
    /// The container name, shown as the page title.
    pub title: String,
}

pub struct LogsPage {
    title: String,
    /// Held so `update_cmd` can append to it off the view. A refcounted GTK
    /// handle, same escape hatch as `AppModel`'s `toast_overlay`.
    view: gtk::TextView,
}

/// Messages from the streaming command. Not `AppMsg`-style input — these arrive
/// on the command channel as the stream produces them.
#[derive(Debug)]
pub enum LogsCmd {
    /// A decoded chunk of output.
    Chunk(String),
    /// The stream errored (daemon went away, container removed mid-follow).
    Failed(String),
    /// The stream ended cleanly — a non-following container that has stopped.
    Ended,
}

#[relm4::component(pub)]
impl Component for LogsPage {
    type Init = LogsInit;
    type Input = ();
    type Output = ();
    type CommandOutput = LogsCmd;

    view! {
        adw::NavigationPage {
            set_title: &model.title,

            adw::ToolbarView {
                // An empty HeaderBar is enough — NavigationView fills in the
                // back button automatically for a pushed page.
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::ScrolledWindow {
                    set_vexpand: true,

                    #[local_ref]
                    view -> gtk::TextView {
                        set_editable: false,
                        set_cursor_visible: false,
                        set_monospace: true,
                        set_left_margin: 8,
                        set_right_margin: 8,
                        set_top_margin: 8,
                        set_bottom_margin: 8,
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
        let view = gtk::TextView::new();
        let model = LogsPage {
            title: init.title,
            view: view.clone(),
        };
        let widgets = view_output!();

        // The streaming command. `docker` and `id` are moved in and owned by the
        // async block, so the borrowing stream from `client::logs` stays valid
        // for the whole loop.
        let docker = init.docker;
        let id = init.id;
        sender.command(move |out, shutdown| {
            shutdown
                .register(async move {
                    let mut stream = client::logs(&docker, &id);
                    while let Some(item) = stream.next().await {
                        let msg = match item {
                            Ok(chunk) => LogsCmd::Chunk(chunk),
                            Err(reason) => LogsCmd::Failed(reason),
                        };
                        // The receiver is gone once the page is dropped; stop.
                        if out.send(msg).is_err() {
                            return;
                        }
                    }
                    out.send(LogsCmd::Ended).ok();
                })
                // Cancel the follow when this component shuts down.
                .drop_on_shutdown()
                .boxed()
        });

        ComponentParts { model, widgets }
    }

    fn update_cmd(
        &mut self,
        msg: Self::CommandOutput,
        _sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            LogsCmd::Chunk(text) => self.append(&text),
            LogsCmd::Failed(reason) => self.append(&format!("\n— stream error: {reason} —\n")),
            LogsCmd::Ended => self.append("\n— end of logs —\n"),
        }
    }
}

impl LogsPage {
    fn append(&self, text: &str) {
        let buffer = self.view.buffer();

        let mut end = buffer.end_iter();
        buffer.insert(&mut end, text);

        // Drop the oldest lines once we exceed the cap.
        let excess = buffer.line_count() - MAX_LINES;
        if excess > 0
            && let Some(mut cut) = buffer.iter_at_line(excess)
        {
            let mut start = buffer.start_iter();
            buffer.delete(&mut start, &mut cut);
        }

        // Follow: keep the newest line in view. Always scrolls to the bottom,
        // like `docker logs -f` — reading scrollback while following isn't
        // supported yet.
        let mut end = buffer.end_iter();
        self.view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
    }
}
