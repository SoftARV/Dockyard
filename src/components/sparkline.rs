// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! A small live sparkline: a filled line chart of recent samples.
//!
//! Pulled out of the detail view so the CPU and memory graphs are one widget
//! configured two ways rather than two copies of the same drawing code. This
//! component owns only the graph — the sample history and the cairo drawing.
//! The caption and the current-value label stay on the detail view's cards,
//! where the data to format them lives; the sparkline just takes samples and
//! paints them.
//!
//! relm4's `DrawHandler` keeps a cairo surface we repaint from `history`, so
//! there's no `Rc<RefCell<>>` sharing of the surface — the model owns it and
//! `update` mutates it like any other field.

use relm4::abstractions::DrawHandler;
use relm4::gtk::prelude::*;
use relm4::{Component, ComponentParts, ComponentSender, gtk};

/// How many recent samples the graph keeps. Doubles as the fixed horizontal
/// span, so the line scrolls left as it fills rather than stretching.
const HISTORY: usize = 60;

/// How the vertical axis is scaled to the data.
pub enum Scale {
    /// A fixed maximum. Memory is a percentage of the limit, always 0–100.
    Fixed(f64),
    /// Auto-scale to the tallest sample seen, but never below this floor. CPU
    /// can exceed 100% across cores, so the axis grows past 100 when it must
    /// while keeping 100 as the baseline when it doesn't.
    PeakFloor(f64),
}

pub struct SparklineInit {
    /// Line and fill colour. Fixed per graph (CPU vs memory) rather than
    /// theme-derived, so the two stay visually distinct.
    pub color: gtk::gdk::RGBA,
    pub scale: Scale,
}

pub struct Sparkline {
    color: gtk::gdk::RGBA,
    scale: Scale,
    /// Recent samples, oldest first.
    history: Vec<f64>,
    /// The cairo surface behind the drawing area, owned by the model.
    draw: DrawHandler,
}

#[derive(Debug)]
pub enum SparklineInput {
    /// Add a sample (dropping the oldest once past the cap) and repaint.
    Push(f64),
    /// Repaint at the current size. Sent on resize, since the `DrawHandler`
    /// surface is recreated blank when the drawing area changes size.
    Redraw,
}

#[relm4::component(pub)]
impl Component for Sparkline {
    type Init = SparklineInit;
    type Input = SparklineInput;
    type Output = ();
    type CommandOutput = ();

    view! {
        // A thin box wrapping the DrawHandler's drawing area: the area is built
        // by the handler, so it's embedded by ref rather than by the macro. This
        // box is what the parent card slots in where the raw area used to be.
        gtk::Box {
            #[local_ref]
            area -> gtk::DrawingArea {
                set_content_height: 44,
                set_hexpand: true,

                connect_resize[sender] => move |_, _, _| {
                    sender.input(SparklineInput::Redraw);
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Sparkline {
            color: init.color,
            scale: init.scale,
            history: Vec::new(),
            draw: DrawHandler::new(),
        };
        let area = model.draw.drawing_area();
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            SparklineInput::Push(sample) => {
                self.history.push(sample);
                if self.history.len() > HISTORY {
                    self.history.remove(0);
                }
                self.redraw();
            }
            SparklineInput::Redraw => self.redraw(),
        }
    }
}

impl Sparkline {
    /// Repaint the surface from `history`, scaled per `scale`.
    fn redraw(&mut self) {
        let max = match self.scale {
            Scale::Fixed(m) => m,
            Scale::PeakFloor(floor) => self.history.iter().copied().fold(floor, f64::max),
        };
        let (w, h) = (self.draw.width(), self.draw.height());
        draw_graph(
            &self.draw.get_context(),
            w,
            h,
            &self.history,
            max,
            self.color,
        );
    }
}

/// Draw a filled sparkline of `samples` (scaled to `max`) across the surface,
/// in `color`.
fn draw_graph(
    cx: &gtk::cairo::Context,
    width: i32,
    height: i32,
    samples: &[f64],
    max: f64,
    color: gtk::gdk::RGBA,
) {
    // The DrawHandler surface keeps its last contents; clear before repainting.
    cx.set_operator(gtk::cairo::Operator::Clear);
    let _ = cx.paint();
    cx.set_operator(gtk::cairo::Operator::Over);

    let (w, h) = (width as f64, height as f64);
    if samples.len() < 2 || max <= 0.0 || w <= 0.0 {
        return;
    }

    // Anchor the newest sample to the right edge, so the line scrolls left as it
    // fills rather than stretching.
    let step = w / (HISTORY - 1) as f64;
    let point = |i: usize, count: usize| {
        let x = w - (count - 1 - i) as f64 * step;
        let v = (samples[i] / max).clamp(0.0, 1.0);
        (x, h - v * h)
    };
    let n = samples.len();

    cx.new_path();
    let (x0, y0) = point(0, n);
    cx.move_to(x0, y0);
    for i in 1..n {
        let (x, y) = point(i, n);
        cx.line_to(x, y);
    }

    let (r, g, b) = (
        color.red() as f64,
        color.green() as f64,
        color.blue() as f64,
    );
    cx.set_source_rgba(r, g, b, 0.85);
    cx.set_line_width(1.5);
    let _ = cx.stroke_preserve();

    // Fill under the line.
    let (xn, _) = point(n - 1, n);
    cx.line_to(xn, h);
    cx.line_to(x0, h);
    cx.close_path();
    cx.set_source_rgba(r, g, b, 0.12);
    let _ = cx.fill();
}
