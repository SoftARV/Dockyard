mod app;
mod components;
mod docker;

use relm4::RelmApp;
use relm4::gtk;
use relm4::gtk::gdk;
use tracing_subscriber::EnvFilter;

const APP_ID: &str = "dev.miguelrincon.Dockyard";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("dockyard=debug")),
        )
        .init();

    // `RelmApp::new` calls `gtk::init()` and — because we enable relm4's
    // `libadwaita` feature — `adw::init()` too, and builds an `adw::Application`
    // rather than a `gtk::Application`. So there's deliberately no adw init here.
    let app = RelmApp::new(APP_ID);
    setup_icon();
    app.run::<app::AppModel>(());
}

/// Point GTK at our icon and name it as the default.
///
/// Careful about what this does and doesn't buy, because it's not obvious. On
/// **Wayland** — this machine — a client cannot set its own toplevel icon at
/// all. GNOME Shell matches the running window to a `.desktop` file by its
/// `app_id` (which equals [`APP_ID`]) and takes the icon from there. It reads
/// those files from its *own* environment, fixed at login, so nothing the app
/// does at runtime — search paths included — can supply one. The icon shows iff
/// `dev.miguelrincon.Dockyard.desktop` is installed where the Shell looks
/// (`make install`). Once it is, `cargo run` inherits the icon too, since the
/// dev binary carries the same `app_id`.
///
/// So why keep this? Two smaller reasons. On **X11** and some other
/// compositors the app *does* set its own window icon from the theme, and there
/// `set_default_icon_name` plus the dev search path make `cargo run` show it
/// without installing. And the search path lets any *in-app* use of the icon
/// (an about dialog, a status page) resolve it by name before install. On
/// Wayland both are harmless no-ops for the window icon.
///
/// Must run after `RelmApp::new`, which initialised GTK and the default display.
fn setup_icon() {
    if let Some(display) = gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
    }
    gtk::Window::set_default_icon_name(APP_ID);
}
