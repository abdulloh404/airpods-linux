//! เริ่ม GTK application และบังคับใช้ X11 backend ก่อนเปิด display

mod ipc_client;
mod ui;

use gtk::prelude::*;
use gtk4 as gtk;

const APP_ID: &str = "io.github.abdulloh404.AirPods.Gui";

fn main() {
    gtk::gdk::set_allowed_backends("x11");

    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| install_css());
    app.connect_activate(ui::build);
    app.run();
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("style.css"));

    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
