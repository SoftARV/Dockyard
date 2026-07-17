//! The streaming log view — a detail page pushed onto the `NavigationView`.
//!
//! This is the first component that streams. Everything else uses
//! `oneshot_command` (fire an async call, get one result back). Logs need
//! `command`, which hands the closure a `Sender` to push *many* messages over
//! time and a `ShutdownReceiver` to stop. `drop_on_shutdown()` ties the stream's
//! life to the component's: when the parent drops this page's `Controller`
//! (on navigate-back), the future is dropped and the follow stops. No manual
//! cancellation token, no leaked stream running behind a page you've left.

use std::collections::VecDeque;

use bollard::Docker;
use futures_util::{FutureExt, StreamExt};
use relm4::adw::prelude::*;
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

use crate::docker::client;

/// A chatty container follows forever, so the buffer is bounded: past this many
/// lines the oldest are dropped. 5000 lines is a few MB — plenty of scrollback,
/// nowhere near unbounded.
const MAX_LINES: usize = 5000;

/// Blank space below each entry, so a wrapped multi-row line is still visually
/// one entry, distinct from the next.
const ENTRY_SPACING: i32 = 6;

/// Mid-grey for the timestamp. Not theme-perfect, but readable on both light and
/// dark; text tags take an explicit colour, not a CSS class.
const TIMESTAMP_COLOR: &str = "#7f7f7f";

pub struct LogsInit {
    pub docker: Docker,
    pub id: String,
    /// The container name, shown as the page title.
    pub title: String,
}

/// One log entry, kept as structured data so the timestamp toggle can re-render
/// without re-fetching. The buffer is derived from these; this is the truth.
struct LogLine {
    /// `HH:MM:SS` from Docker's timestamp, or empty for lines we synthesised
    /// (errors, end-of-stream) or couldn't parse.
    time: String,
    message: String,
}

pub struct LogsPage {
    title: String,
    /// Long lines wrap. Toggled from the header, drives wrap mode via `#[watch]`.
    wrap: bool,
    /// Show Docker's timestamp per line. Off by default (many apps print their
    /// own); toggling re-renders from `lines`.
    timestamps: bool,
    /// Pinned to the bottom (following). False while the user reads scrollback,
    /// true again at the bottom — so following doesn't yank the view away.
    follow: bool,
    view: gtk::TextView,
    /// The dim tag applied to timestamps.
    ts_tag: gtk::TextTag,
    /// Every line still in view, oldest first. Source of truth for re-rendering.
    lines: VecDeque<LogLine>,
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
        // Build the timestamp tag directly (rather than `create_tag`, which
        // returns an Option we'd have to unwrap) and register it.
        let ts_tag = gtk::TextTag::builder()
            .name("timestamp")
            .foreground(TIMESTAMP_COLOR)
            .build();
        view.buffer().tag_table().add(&ts_tag);

        let mut model = LogsPage {
            title: init.title,
            wrap: true,
            timestamps: false,
            follow: true,
            view: view.clone(),
            ts_tag,
            lines: VecDeque::new(),
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
                self.timestamps = show;
                self.rebuild();
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
                self.push_line(synthetic(&format!("— stream error: {reason} —")));
            }
            LogsCmd::Ended => {
                self.flush_pending();
                self.push_line(synthetic("— end of logs —"));
            }
        }
    }
}

/// A line we generated ourselves (no timestamp).
fn synthetic(message: &str) -> LogLine {
    LogLine {
        time: String::new(),
        message: message.to_owned(),
    }
}

/// Split off Docker's leading RFC3339 timestamp, keeping only `HH:MM:SS`.
///
/// A timestamped line looks like `2026-07-17T16:12:55.569805817Z the message`.
/// If it doesn't parse (a synthetic line, or timestamps somehow absent), the
/// whole thing is the message with no time.
fn parse_line(raw: &str) -> LogLine {
    if let Some((stamp, message)) = raw.split_once(' ')
        && let Some(time) = hms(stamp)
    {
        return LogLine {
            time,
            message: message.to_owned(),
        };
    }
    LogLine {
        time: String::new(),
        message: raw.to_owned(),
    }
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
            self.push_line(parse_line(line));
        }
    }

    /// Turn a leftover partial line (no trailing newline) into an entry.
    fn flush_pending(&mut self) {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.push_line(parse_line(&line));
        }
    }

    /// Record a line and render it, dropping the oldest if over the cap.
    fn push_line(&mut self, line: LogLine) {
        self.render(&line);
        self.lines.push_back(line);
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.delete_first_line();
        }
    }

    /// Append one line's text to the buffer, dimming the timestamp if shown.
    fn render(&self, line: &LogLine) {
        let buffer = self.view.buffer();
        if self.timestamps && !line.time.is_empty() {
            let mut end = buffer.end_iter();
            buffer.insert_with_tags(&mut end, &format!("{}  ", line.time), &[&self.ts_tag]);
        }
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, &line.message);
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, "\n");
    }

    /// Delete the oldest line from the buffer (start up to the second line).
    fn delete_first_line(&self) {
        let buffer = self.view.buffer();
        if let Some(mut second) = buffer.iter_at_line(1) {
            let mut start = buffer.start_iter();
            buffer.delete(&mut start, &mut second);
        }
    }

    /// Re-render the whole buffer from `lines` — used when the timestamp toggle
    /// flips, since that changes every line.
    fn rebuild(&self) {
        let buffer = self.view.buffer();
        buffer.set_text("");
        for line in &self.lines {
            self.render(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_docker_timestamp_to_hms() {
        let line = parse_line("2026-07-17T16:12:55.569805817Z checkpoint starting");
        assert_eq!(line.time, "16:12:55");
        assert_eq!(line.message, "checkpoint starting");
    }

    #[test]
    fn a_line_without_a_timestamp_is_all_message() {
        let line = parse_line("— end of logs —");
        assert!(line.time.is_empty());
        assert_eq!(line.message, "— end of logs —");
    }

    #[test]
    fn keeps_the_apps_own_timestamp_in_the_message() {
        // Docker's stamp is stripped; postgres's own stays part of the message.
        let line = parse_line("2026-07-17T16:12:55.5Z 2026-07-17 16:12:55 UTC [27] LOG: hi");
        assert_eq!(line.time, "16:12:55");
        assert_eq!(line.message, "2026-07-17 16:12:55 UTC [27] LOG: hi");
    }

    #[test]
    fn rejects_a_leading_token_that_isnt_a_timestamp() {
        let line = parse_line("hello world");
        assert!(line.time.is_empty());
        assert_eq!(line.message, "hello world");
    }
}
