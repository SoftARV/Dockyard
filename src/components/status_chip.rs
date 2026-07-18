// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared status-chip mapping.
//!
//! The `.status-chip` CSS (the pill shape and per-variant colours) lives in
//! `main.rs`; this just decides, for a given state, the label text and which
//! colour variant class to pair with `status-chip`. Used by both the container
//! list rows and the detail page so they stay in sync.

use crate::docker::types::ContainerState;

/// The human-readable state name shown inside the chip.
pub fn label(state: ContainerState) -> &'static str {
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

/// The colour-variant CSS class, paired with `status-chip`.
pub fn variant(state: ContainerState) -> &'static str {
    match state {
        ContainerState::Running => "running",
        ContainerState::Restarting | ContainerState::Stopping | ContainerState::Paused => "warning",
        ContainerState::Dead => "error",
        _ => "neutral",
    }
}
