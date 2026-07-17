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
use relm4::gtk::gdk;
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

use crate::docker::client;

/// A chatty container follows forever, so the buffer is bounded: past this many
/// lines the oldest are dropped. 5000 lines is a few MB — plenty of scrollback,
/// nowhere near unbounded.
const MAX_LINES: i32 = 5000;

/// Blank space below each entry, so a wrapped multi-row line is still visually
/// one entry, distinct from the next.
const ENTRY_SPACING: i32 = 6;

/// How much to dim the timestamp relative to the theme's text colour — the same
/// ~55% opacity libadwaita's `.dim-label` uses.
const TIMESTAMP_DIM: f32 = 0.55;

pub struct LogsInit {
    pub docker: Docker,
    pub id: String,
    /// The container name, shown as the page title.
    pub title: String,
}

pub struct LogsPage {
    title: String,
    /// Long lines wrap. Toggled from the header, drives wrap mode via `#[watch]`.
    wrap: bool,
    /// Pinned to the bottom (following). False while the user reads scrollback,
    /// true again at the bottom — so following doesn't yank the view away.
    follow: bool,
    view: gtk::TextView,
    /// The tag on every timestamp. Timestamps are *always* inserted; this tag's
    /// `invisible` property shows or hides them all at once, so toggling never
    /// touches the buffer content and the scroll position doesn't move. Copying
    /// excludes invisible text, so a hidden timestamp doesn't leak into the
    /// clipboard.
    ts_tag: gtk::TextTag,
    /// A chunk that didn't end on a newline, awaiting the rest of its line.
    pending: String,
    /// The scroll position, watched for follow. `Option` only because it's
    /// captured after the widget tree exists; `Some` for the component's life.
    vadj: Option<gtk::Adjustment>,
}

#[derive(Debug)]
pub enum LogsInput {
    SetWrap(bool),
    SetTimestamps(bool),
    /// The scroll position changed; recompute whether we're at the bottom.
    ScrolledManually,
    /// The content resized (a line arrived); stay pinned if following.
    ContentGrew,
}

/// Messages from the streaming command, arriving as the stream produces them.
#[derive(Debug)]
pub enum LogsCmd {
    Chunk(String),
    Failed(String),
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
                    // NavigationView adds the back button on the start side.
                    pack_end = &gtk::ToggleButton {
                        set_icon_name: "format-justify-fill-symbolic",
                        set_tooltip_text: Some("Wrap long lines"),
                        set_active: true,
                        connect_toggled[sender] => move |button| {
                            sender.input(LogsInput::SetWrap(button.is_active()));
                        },
                    },
                    pack_end = &gtk::ToggleButton {
                        set_icon_name: "document-open-recent-symbolic",
                        set_tooltip_text: Some("Show timestamps"),
                        set_active: false,
                        connect_toggled[sender] => move |button| {
                            sender.input(LogsInput::SetTimestamps(button.is_active()));
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
                        set_pixels_below_lines: ENTRY_SPACING,
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
        // Timestamps start hidden (off by default). Colour is set on realize
        // from the theme, below.
        let ts_tag = gtk::TextTag::builder()
            .name("timestamp")
            .invisible(true)
            .build();
        view.buffer().tag_table().add(&ts_tag);

        // Dim the timestamp relative to the *theme's* text colour, so it reads
        // right in both light and dark. `WidgetExt::color` only resolves once the
        // view is realized, so apply then; the page is rebuilt on every open, so
        // this also re-runs whenever the theme has since changed.
        let tag = ts_tag.clone();
        view.connect_realize(move |view| {
            let fg = view.color();
            let dim = gdk::RGBA::new(fg.red(), fg.green(), fg.blue(), fg.alpha() * TIMESTAMP_DIM);
            tag.set_foreground_rgba(Some(&dim));
        });

        let mut model = LogsPage {
            title: init.title,
            wrap: true,
            follow: true,
            view: view.clone(),
            ts_tag,
            pending: String::new(),
            vadj: None,
        };
        let widgets = view_output!();

        // `view` now has the ScrolledWindow's vertical adjustment — the real
        // scroll position. Watch it for follow, routing both signals through
        // `update` so the `follow` flag stays in the model, not an `Rc<Cell>`.
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
        // async block, so the borrowing stream stays valid for the whole loop.
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
                        if out.send(msg).is_err() {
                            return;
                        }
                    }
                    out.send(LogsCmd::Ended).ok();
                })
                .drop_on_shutdown()
                .boxed()
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            LogsInput::SetWrap(wrap) => self.wrap = wrap,

            LogsInput::SetTimestamps(show) => {
                // Just flip the tag's visibility — the timestamps are already in
                // the buffer. No rebuild, so the scroll position is untouched.
                self.ts_tag.set_invisible(!show);
            }

            LogsInput::ScrolledManually => {
                if let Some(vadj) = &self.vadj {
                    // At the bottom if the last pixel of content is visible. A
                    // small tolerance absorbs rounding; a real scroll is many px.
                    let distance = vadj.upper() - vadj.page_size() - vadj.value();
                    self.follow = distance < 1.0;
                }
            }

            LogsInput::ContentGrew => {
                // Driving the adjustment (rather than `scroll_to_iter`, which
                // misfires before layout) lands at the bottom even on first
                // paint, because `changed` fires after the new size is known.
                if let Some(vadj) = &self.vadj
                    && self.follow
                {
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
            LogsCmd::Chunk(text) => self.feed(&text),
            LogsCmd::Failed(reason) => {
                self.flush_pending();
                self.insert(None, &format!("— stream error: {reason} —"));
            }
            LogsCmd::Ended => {
                self.flush_pending();
                self.insert(None, "— end of logs —");
            }
        }
    }
}

/// Split off Docker's leading RFC3339 timestamp, keeping only `HH:MM:SS`.
///
/// A timestamped line looks like `2026-07-17T16:12:55.569805817Z the message`.
/// The returned `&str` borrows the message. If it doesn't parse (a synthetic
/// line, or timestamps somehow absent), there's no time and the whole thing is
/// the message.
fn split_timestamp(raw: &str) -> (Option<String>, &str) {
    if let Some((stamp, message)) = raw.split_once(' ')
        && let Some(time) = hms(stamp)
    {
        return (Some(time), message);
    }
    (None, raw)
}

/// `2026-07-17T16:12:55.569805817Z` -> `16:12:55`, or `None` if not that shape.
fn hms(stamp: &str) -> Option<String> {
    let t = stamp.find('T')?;
    let time = stamp.get(t + 1..t + 9)?;
    let bytes = time.as_bytes();
    if bytes.len() == 8 && bytes[2] == b':' && bytes[5] == b':' {
        Some(time.to_owned())
    } else {
        None
    }
}

impl LogsPage {
    /// Break a chunk into whole lines, buffering any trailing partial one.
    fn feed(&mut self, chunk: &str) {
        self.pending.push_str(chunk);
        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=newline).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            let (time, message) = split_timestamp(line);
            self.insert(time.as_deref(), message);
        }
    }

    /// Turn a leftover partial line (no trailing newline) into an entry.
    fn flush_pending(&mut self) {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            let (time, message) = split_timestamp(&line);
            self.insert(time.as_deref(), message);
        }
    }

    /// Append one entry: the timestamp (always tagged, hidden unless toggled on),
    /// then the message, then a newline. Trims the oldest lines past the cap.
    fn insert(&self, time: Option<&str>, message: &str) {
        let buffer = self.view.buffer();

        if let Some(time) = time {
            let mut end = buffer.end_iter();
            buffer.insert_with_tags(&mut end, &format!("{time}  "), &[&self.ts_tag]);
        }
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, message);
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, "\n");

        let excess = buffer.line_count() - MAX_LINES;
        if excess > 0
            && let Some(mut cut) = buffer.iter_at_line(excess)
        {
            let mut start = buffer.start_iter();
            buffer.delete(&mut start, &mut cut);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_docker_timestamp_to_hms() {
        let (time, message) = split_timestamp("2026-07-17T16:12:55.569805817Z checkpoint starting");
        assert_eq!(time.as_deref(), Some("16:12:55"));
        assert_eq!(message, "checkpoint starting");
    }

    #[test]
    fn a_line_without_a_timestamp_is_all_message() {
        let (time, message) = split_timestamp("— end of logs —");
        assert!(time.is_none());
        assert_eq!(message, "— end of logs —");
    }

    #[test]
    fn keeps_the_apps_own_timestamp_in_the_message() {
        // Docker's stamp is stripped; postgres's own stays part of the message.
        let (time, message) =
            split_timestamp("2026-07-17T16:12:55.5Z 2026-07-17 16:12:55 UTC [27] LOG: hi");
        assert_eq!(time.as_deref(), Some("16:12:55"));
        assert_eq!(message, "2026-07-17 16:12:55 UTC [27] LOG: hi");
    }

    #[test]
    fn rejects_a_leading_token_that_isnt_a_timestamp() {
        let (time, message) = split_timestamp("hello world");
        assert!(time.is_none());
        assert_eq!(message, "hello world");
    }
}
