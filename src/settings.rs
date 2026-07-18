// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent global settings.
//!
//! A tiny INI file in the XDG config dir (`~/.config/dockyard/settings.ini`),
//! read and written through `glib::KeyFile`. Deliberately *not* GSettings: that
//! needs a compiled GSchema installed before the app will even start, which
//! breaks the app's `cargo run` workflow; a plain keyfile behaves identically in
//! dev and installed, needs no schema and no new dependency. The cost is that we
//! hand-write the defaults and parsing here — for three keys, nothing.

use std::path::PathBuf;

use relm4::adw;
use relm4::gtk::glib;

/// The window's colour scheme: follow the desktop, or force light/dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Follow the system light/dark preference.
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    /// The libadwaita colour scheme this maps to. `Force*` overrides the system;
    /// `Default` follows it.
    fn color_scheme(self) -> adw::ColorScheme {
        match self {
            Theme::System => adw::ColorScheme::Default,
            Theme::Light => adw::ColorScheme::ForceLight,
            Theme::Dark => adw::ColorScheme::ForceDark,
        }
    }

    /// The stable string written to the config file.
    fn as_key(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    fn from_key(key: &str) -> Self {
        match key {
            "light" => Theme::Light,
            "dark" => Theme::Dark,
            _ => Theme::System,
        }
    }

    /// The row's index in the preferences dropdown (system, light, dark).
    pub fn as_index(self) -> u32 {
        match self {
            Theme::System => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        }
    }

    /// Back from a dropdown index; anything unexpected falls back to `System`.
    pub fn from_index(index: u32) -> Self {
        match index {
            1 => Theme::Light,
            2 => Theme::Dark,
            _ => Theme::System,
        }
    }
}

/// The app's global settings. Loaded once at startup, saved on every change.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Default for the logs "Wrap long lines" toggle.
    pub logs_wrap: bool,
    /// Default for the logs "Show timestamps" toggle.
    pub logs_timestamps: bool,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        // The values the logs view hardcoded before there was a config file:
        // wrap on, timestamps off, follow the system theme.
        Self {
            logs_wrap: true,
            logs_timestamps: false,
            theme: Theme::System,
        }
    }
}

impl Settings {
    /// Load from disk, falling back to the defaults for a missing file or any
    /// missing/malformed key — a broken config should never stop the app.
    pub fn load() -> Self {
        let mut settings = Self::default();

        let keyfile = glib::KeyFile::new();
        if keyfile
            .load_from_file(config_path(), glib::KeyFileFlags::NONE)
            .is_err()
        {
            // No file yet (first run), or unreadable — defaults it is.
            return settings;
        }

        // Each key is independent: a partial or older file keeps the default for
        // whatever it doesn't mention.
        if let Ok(wrap) = keyfile.boolean("logs", "wrap") {
            settings.logs_wrap = wrap;
        }
        if let Ok(timestamps) = keyfile.boolean("logs", "timestamps") {
            settings.logs_timestamps = timestamps;
        }
        if let Ok(theme) = keyfile.string("appearance", "theme") {
            settings.theme = Theme::from_key(&theme);
        }

        settings
    }

    /// Write to disk, creating the config directory if needed. Failures are
    /// logged, not fatal — losing a preference isn't worth crashing over.
    pub fn save(&self) {
        let keyfile = glib::KeyFile::new();
        keyfile.set_boolean("logs", "wrap", self.logs_wrap);
        keyfile.set_boolean("logs", "timestamps", self.logs_timestamps);
        keyfile.set_string("appearance", "theme", self.theme.as_key());

        let path = config_path();
        if let Some(dir) = path.parent()
            && let Err(err) = std::fs::create_dir_all(dir)
        {
            tracing::warn!(%err, "couldn't create the config directory");
            return;
        }
        if let Err(err) = keyfile.save_to_file(&path) {
            tracing::warn!(%err, "couldn't save settings");
        }
    }

    /// Apply the theme to the whole app. Global via `adw::StyleManager`; call at
    /// startup and whenever the theme changes.
    pub fn apply_theme(&self) {
        adw::StyleManager::default().set_color_scheme(self.theme.color_scheme());
    }
}

fn config_path() -> PathBuf {
    glib::user_config_dir()
        .join("dockyard")
        .join("settings.ini")
}
