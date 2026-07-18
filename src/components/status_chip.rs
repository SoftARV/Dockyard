// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The shared status chip: a coloured pill with a dot and a state label.
//!
//! One reusable widget — `StatusChip`, a relm4 `WidgetTemplate` — plus the small
//! mapping from a `ContainerState` (with an optional in-flight override like
//! "Starting…") to the chip's text and colour class. Used by both the list rows
//! and the detail page, so the two can't drift apart.
//!
//! A `WidgetTemplate` rather than a full `Component`: the chip has no state or
//! logic of its own, it's pure structure the parents drive with `#[watch]`. That
//! also lets it embed straight into the factory row without a per-row child
//! controller. The pill shape and per-variant colours are the stylesheet at the
//! bottom of this module (`install_css`); the functions decide the label text
//! and the variant class.

use relm4::gtk::prelude::*;
use relm4::{WidgetTemplate, gtk, set_global_css};

use crate::docker::types::ContainerState;

/// The chip widget: a `.status-chip` pill wrapping a coloured dot and a label.
/// The caller sets the variant class and the label text via `#[watch]`; the dot
/// picks up the matching colour through `.status-chip.<variant> .status-dot` in
/// the stylesheet, so it always tracks the text colour.
#[relm4::widget_template(pub)]
impl WidgetTemplate for StatusChip {
    view! {
        gtk::Box {
            add_css_class: "status-chip",
            set_valign: gtk::Align::Center,
            set_spacing: 6,

            // No name: the parents never touch the dot — its colour comes from
            // the `.status-chip.<variant> .status-dot` CSS, keyed off the variant
            // class the parents set on the root.
            gtk::Box {
                add_css_class: "status-dot",
                set_valign: gtk::Align::Center,
            },

            #[name = "label"]
            gtk::Label {},
        }
    }
}

/// The chip's text: the in-flight override ("Starting…") while an action is
/// running, otherwise the container's state name.
pub fn label(state: ContainerState, transition: Option<&'static str>) -> &'static str {
    transition.unwrap_or_else(|| state_label(state))
}

/// The chip's colour-variant class (paired with `status-chip`): a neutral tint
/// while an action is in flight — the outcome isn't known yet — otherwise the
/// state's own colour.
pub fn variant(state: ContainerState, transition: Option<&str>) -> &'static str {
    if transition.is_some() {
        "neutral"
    } else {
        state_variant(state)
    }
}

/// The human-readable state name.
fn state_label(state: ContainerState) -> &'static str {
    match state {
        ContainerState::Created => "Created",
        ContainerState::Running => "Running",
        ContainerState::Paused => "Paused",
        ContainerState::Restarting => "Restarting",
        ContainerState::Stopping => "Stopping",
        ContainerState::Exited => "Exited",
        ContainerState::Removing => "Removing",
        ContainerState::Dead => "Dead",
        ContainerState::Unknown => "Unknown",
    }
}

/// The colour-variant CSS class for a settled state.
fn state_variant(state: ContainerState) -> &'static str {
    match state {
        ContainerState::Running => "running",
        ContainerState::Restarting | ContainerState::Stopping | ContainerState::Paused => "warning",
        ContainerState::Dead => "error",
        _ => "neutral",
    }
}

/// Install the chip's stylesheet. Global, and must run once after GTK is
/// initialised — call it from `main` at startup. It has to be global: the
/// variant and dot selectors below depend on classes set at runtime, which
/// per-widget CSS (`inline_css`) can't express, so there's no scoped
/// alternative.
///
/// This is the CLAUDE.md-sanctioned custom-CSS exception. libadwaita has no
/// chip/badge widget for a filled, coloured pill (its `.badge` class is wired to
/// the view-switcher's number bubble, and the colour classes only tint text).
/// Colours come from Adwaita's own named colours, so the chip follows the theme
/// (light/dark) for free.
pub fn install_css() {
    set_global_css(CSS);
}

const CSS: &str = "
.status-chip {
    border-radius: 9999px;
    padding: 3px 10px;
    font-weight: bold;
    font-size: 0.8em;
}
/* The dot inside the chip. A small round box; its colour matches the chip's
   text per variant below, so it reads as a status LED next to the label. */
.status-dot {
    min-width: 7px;
    min-height: 7px;
    border-radius: 9999px;
}
/* Tonal: a soft tint of the state colour behind the same colour as text, so the
   text matches the chip. `@success_color` etc. are Adwaita's standalone
   semantic colours, tuned to read on the window background. The dot takes the
   solid variant colour. */
.status-chip.running { background-color: alpha(@success_color, 0.15); color: @success_color; }
.status-chip.warning { background-color: alpha(@warning_color, 0.15); color: @warning_color; }
.status-chip.error   { background-color: alpha(@error_color, 0.15);   color: @error_color; }
.status-chip.neutral {
    background-color: alpha(@window_fg_color, 0.08);
    color: alpha(@window_fg_color, 0.7);
}
.status-chip.running .status-dot { background-color: @success_color; }
.status-chip.warning .status-dot { background-color: @warning_color; }
.status-chip.error   .status-dot { background-color: @error_color; }
.status-chip.neutral .status-dot { background-color: alpha(@window_fg_color, 0.55); }
";
