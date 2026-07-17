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
    /// Whether long lines wrap. Toggled from the header; drives the view's wrap
    /// mode via `#[watch]`.
    wrap: bool,
    /// Whether we're pinned to the bottom (following). Set false when the user
    /// scrolls up, true again when they return to the bottom — so following
    /// doesn't yank the view away while they read scrollback.
    follow: bool,
    /// Held so `update_cmd` can append to the buffer. A refcounted GTK handle,
    /// same escape hatch as `AppModel`'s `toast_overlay`.
    view: gtk::TextView,
    /// The scroll position, watched to implement follow. `Option` only because
    /// it's captured after the widget tree exists (the `TextView` gets its
    /// adjustment from the `ScrolledWindow` when adopted); it's `Some` for the
    /// component's whole life.
    vadj: Option<gtk::Adjustment>,
}

#[derive(Debug)]
pub enum LogsInput {
    /// Turn line wrapping on or off.
    SetWrap(bool),
    /// The scroll position changed; recompute whether we're at the bottom.
    ScrolledManually,
    /// The content resized (a line arrived); stay pinned to the bottom if
    /// following.
    ContentGrew,
}

/// Messages from the streaming command. Not `Input` — these arrive on the
/// command channel as the stream produces them.
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
    type Input = LogsInput;
    type Output = ();
    type CommandOutput = LogsCmd;

    view! {
        adw::NavigationPage {
            set_title: &model.title,

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    // NavigationView fills in the back button on the start side
                    // automatically; this is the only control we add.
                    pack_end = &gtk::ToggleButton {
                        set_icon_name: "format-justify-fill-symbolic",
                        set_tooltip_text: Some("Wrap long lines"),
                        set_active: true,
                        connect_toggled[sender] => move |button| {
                            sender.input(LogsInput::SetWrap(button.is_active()));
                        },
                    },
                },

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
                        // WordChar wraps mid-word if a token is longer than the
                        // pane (a 285-char log line has no spaces to break on).
                        #[watch]
                        set_wrap_mode: if model.wrap {
                            gtk::WrapMode::WordChar
                        } else {
                            gtk::WrapMode::None
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
        let view = gtk::TextView::new();
        let mut model = LogsPage {
            title: init.title,
            wrap: true,
            follow: true,
            view: view.clone(),
            vadj: None,
        };
        let widgets = view_output!();

        // Now that `view` is inside the ScrolledWindow, it has that window's
        // vertical adjustment — the real scroll position. Watch it to implement
        // follow, routing both signals through `update` so the `follow` flag
        // stays in the model rather than an `Rc<Cell>`.
        if let Some(vadj) = view.vadjustment() {
            let on_scroll = sender.input_sender().clone();
            vadj.connect_value_changed(move |_| {
                on_scroll.send(LogsInput::ScrolledManually).ok();
            });
            let on_grow = sender.input_sender().clone();
            vadj.connect_changed(move |_| {
                on_grow.send(LogsInput::ContentGrew).ok();
            });
            model.vadj = Some(vadj);
        }

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

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>, _root: &Self::Root) {
        if let LogsInput::SetWrap(wrap) = msg {
            self.wrap = wrap;
            return;
        }

        let Some(vadj) = &self.vadj else {
            return;
        };
        match msg {
            LogsInput::SetWrap(_) => {} // handled above

            LogsInput::ScrolledManually => {
                // At the bottom if the last pixel of content is visible. A small
                // tolerance absorbs rounding; a real scroll-up is many pixels.
                let distance = vadj.upper() - vadj.page_size() - vadj.value();
                self.follow = distance < 1.0;
            }

            LogsInput::ContentGrew => {
                // `scroll_to_iter`/`_mark` misfire before the view is laid out,
                // which left the page opening scrolled to the top. Driving the
                // adjustment works because `changed` fires *after* the new size
                // is known — so this lands at the bottom even on first paint.
                if self.follow {
                    vadj.set_value(vadj.upper() - vadj.page_size());
                }
            }
        }
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
    /// Insert text and trim the buffer. Scrolling is *not* done here — inserting
    /// changes the content height, which fires the adjustment's `changed` signal
    /// and lands in `ContentGrew`.
    fn append(&self, text: &str) {
        let buffer = self.view.buffer();

        let mut end = buffer.end_iter();
        buffer.insert(&mut end, text);

        let excess = buffer.line_count() - MAX_LINES;
        if excess > 0
            && let Some(mut cut) = buffer.iter_at_line(excess)
        {
            let mut start = buffer.start_iter();
            buffer.delete(&mut start, &mut cut);
        }
    }
}
