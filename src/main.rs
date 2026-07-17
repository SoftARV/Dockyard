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

/// Make the app show its own icon.
///
/// Two separate things have to line up. `set_default_icon_name` tells GTK which
/// themed icon every window should wear — resolved by name, the same name as the
/// app ID and the `.desktop` `Icon=`. Installed, that name is found in the
/// system icon theme under `~/.local/share/icons/hicolor`.
///
/// Running from `cargo`, though, nothing is installed, so the name would resolve
/// to nothing and the window would fall back to a generic icon. Adding the
/// repo's `data/icons` to the search path fixes that for development, and is
/// simply absent (harmless) once installed. Must run after `RelmApp::new`, which
/// is what initialised GTK and created the default display.
fn setup_icon() {
    if let Some(display) = gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
    }
    gtk::Window::set_default_icon_name(APP_ID);
}
