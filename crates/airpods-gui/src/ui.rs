//! สร้างหน้าต่างควบคุม AirPods และแสดง state ที่ได้รับจาก `airpodsd`

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use crate::gtk;
use airpods_ipc::{
    DaemonStatus, DeviceInfo, MAX_GAIN_DB, MAX_LIMITER_DB, MIN_GAIN_DB, MIN_LIMITER_DB,
};
use gtk::glib;
use gtk::prelude::*;
use gtk::{Align, Orientation};

use crate::ipc_client::{Client, Command, Event};

#[derive(Clone)]
struct View {
    daemon_badge: gtk::Label,
    device_combo: gtk::ComboBoxText,
    device_state: gtk::Label,
    empty_state: gtk::Box,
    microphone_controls: gtk::Box,
    mic_switch: gtk::Switch,
    gain_spin: gtk::SpinButton,
    limiter_spin: gtk::SpinButton,
    left: BatteryView,
    right: BatteryView,
    power_bridge: gtk::Label,
    error_revealer: gtk::Revealer,
    error_label: gtk::Label,
    updating: Rc<Cell<bool>>,
}

#[derive(Clone)]
struct BatteryView {
    value: gtk::Label,
    bar: gtk::ProgressBar,
}

pub fn build(app: &gtk::Application) {
    let (client, events) = Client::start();

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("AirPods Linux")
        .default_width(720)
        .default_height(720)
        .build();
    window.set_size_request(500, 560);
    window.set_titlebar(Some(&build_titlebar(&client)));

    let (content, view) = build_content(&client);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .build();
    window.set_child(Some(&scroller));

    attach_event_receiver(view, events);
    window.present();
}

fn build_titlebar(client: &Client) -> gtk::HeaderBar {
    let header = gtk::HeaderBar::new();
    header.set_show_title_buttons(true);

    let title = gtk::Label::new(Some("AirPods Linux"));
    title.set_css_classes(&["window-title"]);
    header.set_title_widget(Some(&title));

    let refresh = gtk::Button::with_label("Refresh");
    refresh.set_css_classes(&["header-action"]);
    refresh.set_tooltip_text(Some("Read the latest state from airpodsd"));
    let client = client.clone();
    refresh.connect_clicked(move |_| client.send(Command::Refresh));
    header.pack_end(&refresh);
    header
}

fn build_content(client: &Client) -> (gtk::Box, View) {
    let root = gtk::Box::new(Orientation::Vertical, 28);
    root.set_css_classes(&["app-shell"]);
    root.set_margin_top(28);
    root.set_margin_bottom(36);
    root.set_margin_start(36);
    root.set_margin_end(36);

    let error_label = gtk::Label::new(None);
    error_label.set_wrap(true);
    error_label.set_xalign(0.0);
    let error_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .child(&error_label)
        .build();
    error_revealer.set_css_classes(&["error-banner"]);
    root.append(&error_revealer);

    let (airpods, device_combo, device_state, empty_state, daemon_badge) = build_airpods_section();
    root.append(&airpods);

    let updating = Rc::new(Cell::new(false));
    let (microphone, microphone_controls, mic_switch, gain_spin, limiter_spin) =
        build_microphone_section(client, &updating);
    root.append(&microphone);

    let (battery, left, right, power_bridge) = build_battery_section();
    root.append(&battery);

    connect_device_selection(&device_combo, client, &updating);

    (
        root,
        View {
            daemon_badge,
            device_combo,
            device_state,
            empty_state,
            microphone_controls,
            mic_switch,
            gain_spin,
            limiter_spin,
            left,
            right,
            power_bridge,
            error_revealer,
            error_label,
            updating,
        },
    )
}

fn build_airpods_section() -> (
    gtk::Box,
    gtk::ComboBoxText,
    gtk::Label,
    gtk::Box,
    gtk::Label,
) {
    let (section, card) = settings_section("AirPods");

    let combo = gtk::ComboBoxText::new();
    combo.set_size_request(250, -1);
    combo.set_sensitive(false);
    combo.set_tooltip_text(Some("Select a discovered AirPods device"));
    card.append(&setting_row(
        "Device",
        "Choose the AirPods managed by the daemon.",
        &combo,
    ));
    card.append(&settings_separator());

    let state = gtk::Label::new(Some("Waiting"));
    state.set_css_classes(&["status-value"]);
    card.append(&setting_row(
        "Connection",
        "Live Bluetooth connection state.",
        &state,
    ));
    card.append(&settings_separator());

    let daemon_badge = gtk::Label::new(Some("CONNECTING"));
    daemon_badge.set_css_classes(&["status-pill", "state-warn"]);
    card.append(&setting_row(
        "Daemon",
        "Background service for Bluetooth, PipeWire and battery updates.",
        &daemon_badge,
    ));

    let empty = gtk::Box::new(Orientation::Vertical, 3);
    empty.set_css_classes(&["empty-state"]);
    let empty_title = gtk::Label::new(Some("No AirPods found"));
    empty_title.set_halign(Align::Start);
    empty_title.set_css_classes(&["empty-title"]);
    let empty_body = gtk::Label::new(Some(
        "Connect your AirPods in Ubuntu Bluetooth settings, then refresh.",
    ));
    empty_body.set_halign(Align::Start);
    empty_body.set_wrap(true);
    empty.append(&empty_title);
    empty.append(&empty_body);
    empty.set_visible(false);
    section.append(&empty);

    (section, combo, state, empty, daemon_badge)
}

fn build_microphone_section(
    client: &Client,
    updating: &Rc<Cell<bool>>,
) -> (
    gtk::Box,
    gtk::Box,
    gtk::Switch,
    gtk::SpinButton,
    gtk::SpinButton,
) {
    let (section, card) = settings_section("Virtual microphone");

    let mic_switch = gtk::Switch::new();
    mic_switch.set_valign(Align::Center);
    mic_switch.set_tooltip_text(Some("Start or stop the virtual microphone"));
    let command_client = client.clone();
    let changing = updating.clone();
    mic_switch.connect_active_notify(move |control| {
        if changing.get() {
            return;
        }
        command_client.send(if control.is_active() {
            Command::StartMic
        } else {
            Command::StopMic
        });
    });
    card.append(&setting_row(
        "Microphone",
        "Microphone virtual - Abdulloh's AirPods Pro",
        &mic_switch,
    ));
    card.append(&settings_separator());

    let gain_spin = numeric_control(MIN_GAIN_DB, MAX_GAIN_DB, 1.0);
    let command_client = client.clone();
    let changing = updating.clone();
    gain_spin.connect_value_changed(move |control| {
        if !changing.get() {
            command_client.send(Command::SetGain(control.value()));
        }
    });
    card.append(&setting_row(
        "Input gain",
        "Boost before the limiter, measured in dB.",
        &gain_spin,
    ));
    card.append(&settings_separator());

    let limiter_spin = numeric_control(MIN_LIMITER_DB, MAX_LIMITER_DB, 1.0);
    let command_client = client.clone();
    let changing = updating.clone();
    limiter_spin.connect_value_changed(move |control| {
        if !changing.get() {
            command_client.send(Command::SetLimiter(control.value()));
        }
    });
    card.append(&setting_row(
        "Limiter ceiling",
        "Maximum output level, measured in dBFS.",
        &limiter_spin,
    ));
    card.set_sensitive(false);

    (section, card, mic_switch, gain_spin, limiter_spin)
}

fn build_battery_section() -> (gtk::Box, BatteryView, BatteryView, gtk::Label) {
    let (section, card) = settings_section("Battery");

    let (left_control, left) = battery_control();
    card.append(&setting_row(
        "Left AirPod",
        "Battery level reported to Ubuntu.",
        &left_control,
    ));
    card.append(&settings_separator());

    let (right_control, right) = battery_control();
    card.append(&setting_row(
        "Right AirPod",
        "Battery level reported to Ubuntu.",
        &right_control,
    ));
    card.append(&settings_separator());

    let power_bridge = gtk::Label::new(Some("WAITING"));
    power_bridge.set_css_classes(&["status-pill", "state-warn"]);
    card.append(&setting_row(
        "UPower bridge",
        "Publishes both earbuds as battery devices in Ubuntu.",
        &power_bridge,
    ));

    (section, left, right, power_bridge)
}

fn settings_section(title: &str) -> (gtk::Box, gtk::Box) {
    let section = gtk::Box::new(Orientation::Vertical, 10);
    section.set_css_classes(&["settings-section"]);

    let title = gtk::Label::new(Some(title));
    title.set_halign(Align::Start);
    title.set_css_classes(&["section-title"]);
    section.append(&title);

    let card = gtk::Box::new(Orientation::Vertical, 0);
    card.set_css_classes(&["settings-card"]);
    section.append(&card);
    (section, card)
}

fn setting_row(title: &str, description: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(Orientation::Horizontal, 18);
    row.set_css_classes(&["settings-row"]);

    let copy = gtk::Box::new(Orientation::Vertical, 2);
    copy.set_hexpand(true);
    let title = gtk::Label::new(Some(title));
    title.set_halign(Align::Start);
    title.set_xalign(0.0);
    title.set_css_classes(&["row-title"]);
    let description = gtk::Label::new(Some(description));
    description.set_halign(Align::Start);
    description.set_xalign(0.0);
    description.set_wrap(true);
    description.set_max_width_chars(52);
    description.set_css_classes(&["row-description"]);
    copy.append(&title);
    copy.append(&description);

    control.set_valign(Align::Center);
    row.append(&copy);
    row.append(control);
    row
}

fn settings_separator() -> gtk::Separator {
    let separator = gtk::Separator::new(Orientation::Horizontal);
    separator.set_css_classes(&["settings-separator"]);
    separator
}

fn numeric_control(minimum: f64, maximum: f64, step: f64) -> gtk::SpinButton {
    let control = gtk::SpinButton::with_range(minimum, maximum, step);
    control.set_digits(0);
    control.set_numeric(true);
    control.set_snap_to_ticks(true);
    control.set_width_chars(5);
    control
}

fn battery_control() -> (gtk::Box, BatteryView) {
    let control = gtk::Box::new(Orientation::Vertical, 5);
    control.set_css_classes(&["battery-control"]);
    control.set_size_request(142, -1);

    let value = gtk::Label::new(Some("—"));
    value.set_halign(Align::End);
    value.set_css_classes(&["battery-value"]);
    let bar = gtk::ProgressBar::new();
    bar.set_fraction(0.0);
    bar.set_hexpand(true);
    control.append(&value);
    control.append(&bar);
    (control, BatteryView { value, bar })
}

fn connect_device_selection(combo: &gtk::ComboBoxText, client: &Client, updating: &Rc<Cell<bool>>) {
    let client = client.clone();
    let updating = updating.clone();
    combo.connect_changed(move |combo| {
        if updating.get() {
            return;
        }
        if let Some(address) = combo.active_id() {
            client.send(Command::SelectDevice(address.to_string()));
        }
    });
}

fn attach_event_receiver(view: View, events: mpsc::Receiver<Event>) {
    glib::timeout_add_local(Duration::from_millis(80), move || {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Snapshot {
                    status,
                    devices,
                    battery,
                } => {
                    update_devices(&view, &devices, &status.selected_device);
                    update_status(&view, &status);
                    update_battery(&view.left, battery.left_percent, battery.left_charging);
                    update_battery(&view.right, battery.right_percent, battery.right_charging);
                }
                Event::Status(status) => update_status(&view, &status),
                Event::Battery(battery) => {
                    update_battery(&view.left, battery.left_percent, battery.left_charging);
                    update_battery(&view.right, battery.right_percent, battery.right_charging);
                }
                Event::Devices(devices) => {
                    let selected = view
                        .device_combo
                        .active_id()
                        .map(|address| address.to_string())
                        .unwrap_or_default();
                    update_devices(&view, &devices, &selected);
                }
                Event::Error(message) => {
                    set_state_badge(&view.daemon_badge, "UNAVAILABLE", "state-error");
                    show_error(&view, &message);
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

fn update_devices(view: &View, devices: &[DeviceInfo], selected_address: &str) {
    view.updating.set(true);
    view.device_combo.remove_all();
    for device in devices {
        let connection = if device.connected {
            "connected"
        } else {
            "offline"
        };
        view.device_combo.append(
            Some(&device.address),
            &format!("{}  ·  {connection}", device.name),
        );
    }

    let selected_address = devices
        .iter()
        .find(|device| device.address == selected_address)
        .or_else(|| devices.iter().find(|device| device.selected))
        .map(|device| device.address.as_str());
    if let Some(selected_address) = selected_address {
        view.device_combo.set_active_id(Some(selected_address));
    }
    view.updating.set(false);

    let is_empty = devices.is_empty();
    view.empty_state.set_visible(is_empty);
    view.device_combo.set_visible(!is_empty);
    view.device_combo.set_sensitive(!is_empty);

    let selected = selected_address
        .and_then(|address| devices.iter().find(|device| device.address == address));
    view.device_state.set_text(match selected {
        Some(device) if device.connected => "CONNECTED",
        Some(_) => "NOT CONNECTED",
        None if is_empty => "NO DEVICE",
        None => "SELECT A DEVICE",
    });
}

fn update_status(view: &View, status: &DaemonStatus) {
    let normalized_state = status.state.to_ascii_lowercase();
    let state_class = match normalized_state.as_str() {
        "streaming" | "idle" | "ready" => "state-ok",
        "error" => "state-error",
        _ => "state-warn",
    };
    set_state_badge(
        &view.daemon_badge,
        &status.state.to_uppercase(),
        state_class,
    );

    let has_device = !status.selected_device.is_empty();
    view.microphone_controls.set_sensitive(has_device);

    view.updating.set(true);
    view.mic_switch.set_active(status.mic_active);
    view.gain_spin.set_value(status.gain_db);
    view.limiter_spin.set_value(status.limiter_db);
    view.updating.set(false);

    if status.power_bridge_available {
        set_state_badge(&view.power_bridge, "AVAILABLE", "state-ok");
    } else {
        set_state_badge(&view.power_bridge, "UNAVAILABLE", "state-error");
    }

    if status.last_error.is_empty() {
        clear_error(view);
    } else {
        show_error(view, &status.last_error);
    }
}

fn update_battery(view: &BatteryView, percent: i16, charging: bool) {
    if percent < 0 {
        view.value.set_text("—");
        view.bar.set_fraction(0.0);
        view.bar.set_tooltip_text(Some("Battery level unavailable"));
        return;
    }

    let percent = percent.clamp(0, 100);
    let charging_mark = if charging { "⚡ " } else { "" };
    view.value.set_text(&format!("{charging_mark}{percent}%"));
    view.bar.set_fraction(f64::from(percent) / 100.0);
    let charging_state = if charging { ", charging" } else { "" };
    view.bar
        .set_tooltip_text(Some(&format!("Battery level: {percent}%{charging_state}")));
}

fn set_state_badge(label: &gtk::Label, text: &str, class: &str) {
    label.set_text(text);
    label.remove_css_class("state-ok");
    label.remove_css_class("state-warn");
    label.remove_css_class("state-error");
    label.add_css_class(class);
}

fn show_error(view: &View, message: &str) {
    view.error_label.set_text(message);
    view.error_revealer.set_reveal_child(true);
}

fn clear_error(view: &View) {
    view.error_revealer.set_reveal_child(false);
}
