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
    load_css();
    app.run::<app::AppModel>(());
}

/// Our only custom CSS: the status chip on the detail page.
///
/// libadwaita has no chip/badge widget for a filled, coloured status pill (its
/// `.badge` class is wired to the view-switcher's number bubble), and the colour
/// classes only tint text. So this is the CLAUDE.md-sanctioned exception — "no
/// libadwaita widget for the job". Colours come from Adwaita's own named colours,
/// so the chip follows the theme (light/dark) for free.
const CSS: &str = "
.status-chip {
    border-radius: 9999px;
    padding: 2px 10px;
    font-weight: bold;
    font-size: 0.8em;
}
.status-chip.running { background-color: @success_bg_color; color: @success_fg_color; }
.status-chip.warning { background-color: @warning_bg_color; color: @warning_fg_color; }
.status-chip.error   { background-color: @error_bg_color;   color: @error_fg_color; }
.status-chip.neutral { background-color: alpha(@window_fg_color, 0.12); color: @window_fg_color; }
";

fn load_css() {
    relm4::set_global_css(CSS);
}

/// Point GTK at our icon and name it as the default.
///
/// This does **not** put an icon on the window under Wayland — this machine's
/// setup — and it's worth being blunt about that, because it looks like it
/// should. On Wayland a client cannot set its own toplevel icon at all. GNOME
/// Shell picks the icon by matching the window to an installed `.desktop`
/// (partly on `app_id`, partly on the executable), so only the *installed* app
/// shows an icon; `cargo run` never will, no matter what this function does.
///
/// It's kept because it's the standard idiom and it *does* work on **X11** and
/// some other compositors, where a client sets its own window icon from the
/// theme — `set_default_icon_name` names it, `add_search_path` lets the dev
/// build resolve it pre-install. The search path also covers any future
/// *in-app* icon use (an about dialog). All harmless no-ops on Wayland.
///
/// Must run after `RelmApp::new`, which initialised GTK and the default display.
fn setup_icon() {
    if let Some(display) = gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
    }
    gtk::Window::set_default_icon_name(APP_ID);
}
