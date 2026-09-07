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
    controls: gtk::Box,
    mic_button: gtk::Button,
    mic_active: Rc<Cell<bool>>,
    gain_scale: gtk::Scale,
    gain_value: gtk::Label,
    limiter_scale: gtk::Scale,
    limiter_value: gtk::Label,
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
        .title("AirPods")
        .default_width(760)
        .default_height(680)
        .build();
    window.set_size_request(440, 560);

    let (content, view) = build_content(&client);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .build();
    window.set_child(Some(&scroller));

    attach_event_receiver(view, events);
    window.present();
}

fn build_content(client: &Client) -> (gtk::Box, View) {
    let root = gtk::Box::new(Orientation::Vertical, 24);
    root.set_css_classes(&["app-shell"]);
    root.set_margin_top(32);
    root.set_margin_bottom(36);
    root.set_margin_start(36);
    root.set_margin_end(36);

    let (header, daemon_badge) = build_header(client);
    root.append(&header);

    let error_label = gtk::Label::new(None);
    error_label.set_wrap(true);
    error_label.set_xalign(0.0);
    let error_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .child(&error_label)
        .build();
    error_revealer.set_css_classes(&["error-banner"]);
    root.append(&error_revealer);

    let (device_section, device_combo, device_state, empty_state) = build_device_section();
    root.append(&device_section);

    let updating = Rc::new(Cell::new(false));
    let (controls, mic_button, mic_active, gain_scale, gain_value, limiter_scale, limiter_value) =
        build_audio_controls(client, &updating);
    root.append(&controls);

    let (battery_section, left, right, power_bridge) = build_battery_section();
    root.append(&battery_section);

    connect_device_selection(&device_combo, client, &updating);

    (
        root,
        View {
            daemon_badge,
            device_combo,
            device_state,
            empty_state,
            controls,
            mic_button,
            mic_active,
            gain_scale,
            gain_value,
            limiter_scale,
            limiter_value,
            left,
            right,
            power_bridge,
            error_revealer,
            error_label,
            updating,
        },
    )
}

fn build_header(client: &Client) -> (gtk::Box, gtk::Label) {
    let header = gtk::Box::new(Orientation::Horizontal, 16);
    header.set_css_classes(&["header"]);

    let heading = gtk::Box::new(Orientation::Vertical, 2);
    heading.set_hexpand(true);
    let eyebrow = gtk::Label::new(Some("AIRPODS / UBUNTU"));
    eyebrow.set_halign(Align::Start);
    eyebrow.set_css_classes(&["eyebrow"]);
    let title = gtk::Label::new(Some("Listening, clearly."));
    title.set_halign(Align::Start);
    title.set_css_classes(&["title"]);
    heading.append(&eyebrow);
    heading.append(&title);

    let daemon_badge = gtk::Label::new(Some("CONNECTING"));
    daemon_badge.set_css_classes(&["daemon-badge", "state-warn"]);

    let refresh = gtk::Button::with_mnemonic("_Refresh");
    refresh.set_css_classes(&["quiet-action"]);
    refresh.set_tooltip_text(Some("Read the latest state from airpodsd"));
    let client = client.clone();
    refresh.connect_clicked(move |_| client.send(Command::Refresh));

    header.append(&heading);
    header.append(&daemon_badge);
    header.append(&refresh);
    (header, daemon_badge)
}

fn build_device_section() -> (gtk::Box, gtk::ComboBoxText, gtk::Label, gtk::Box) {
    let section = gtk::Box::new(Orientation::Vertical, 12);
    let label = section_label("DEVICE", "Choose the AirPods managed by the daemon");
    section.append(&label);

    let row = gtk::Box::new(Orientation::Horizontal, 12);
    row.set_css_classes(&["device-row"]);
    let combo = gtk::ComboBoxText::new();
    combo.set_hexpand(true);
    combo.set_tooltip_text(Some("Select a discovered AirPods device"));
    let state = gtk::Label::new(Some("Waiting for airpodsd"));
    state.set_css_classes(&["connection-state", "data"]);
    row.append(&combo);
    row.append(&state);
    section.append(&row);

    let empty = gtk::Box::new(Orientation::Vertical, 4);
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

    (section, combo, state, empty)
}

#[allow(clippy::type_complexity)]
fn build_audio_controls(
    client: &Client,
    updating: &Rc<Cell<bool>>,
) -> (
    gtk::Box,
    gtk::Button,
    Rc<Cell<bool>>,
    gtk::Scale,
    gtk::Label,
    gtk::Scale,
    gtk::Label,
) {
    let section = gtk::Box::new(Orientation::Vertical, 16);
    section.set_css_classes(&["control-surface"]);
    section.append(&section_label(
        "MICROPHONE",
        "AAC-ELD decoded into the AirPodsHiRes PipeWire source",
    ));

    let mic_active = Rc::new(Cell::new(false));
    let mic_button = gtk::Button::with_mnemonic("_Start microphone");
    mic_button.set_css_classes(&["primary-action"]);
    mic_button.set_hexpand(true);
    mic_button.set_tooltip_text(Some("Start or stop the virtual microphone"));
    let active = mic_active.clone();
    let command_client = client.clone();
    mic_button.connect_clicked(move |_| {
        let command = if active.get() {
            Command::StopMic
        } else {
            Command::StartMic
        };
        command_client.send(command);
    });
    section.append(&mic_button);

    let (gain_row, gain_scale, gain_value) = control_row(
        "Input gain",
        "Boost before the limiter",
        MIN_GAIN_DB,
        MAX_GAIN_DB,
        1.0,
    );
    let command_client = client.clone();
    let changing = updating.clone();
    gain_scale.connect_value_changed(move |scale| {
        if !changing.get() {
            command_client.send(Command::SetGain(scale.value()));
        }
    });
    let value_label = gain_value.clone();
    gain_scale.connect_value_changed(move |scale| {
        value_label.set_text(&format!("{:.0} dB", scale.value()));
    });
    section.append(&gain_row);

    let (limiter_row, limiter_scale, limiter_value) = control_row(
        "Limiter ceiling",
        "Keep sudden peaks controlled",
        MIN_LIMITER_DB,
        MAX_LIMITER_DB,
        1.0,
    );
    let command_client = client.clone();
    let changing = updating.clone();
    limiter_scale.connect_value_changed(move |scale| {
        if !changing.get() {
            command_client.send(Command::SetLimiter(scale.value()));
        }
    });
    let value_label = limiter_value.clone();
    limiter_scale.connect_value_changed(move |scale| {
        value_label.set_text(&format!("{:.0} dBFS", scale.value()));
    });
    section.append(&limiter_row);

    (
        section,
        mic_button,
        mic_active,
        gain_scale,
        gain_value,
        limiter_scale,
        limiter_value,
    )
}

fn control_row(
    title: &str,
    description: &str,
    minimum: f64,
    maximum: f64,
    step: f64,
) -> (gtk::Box, gtk::Scale, gtk::Label) {
    let row = gtk::Box::new(Orientation::Vertical, 8);
    let heading = gtk::Box::new(Orientation::Horizontal, 12);
    let copy = gtk::Box::new(Orientation::Vertical, 1);
    copy.set_hexpand(true);
    let title = gtk::Label::new(Some(title));
    title.set_halign(Align::Start);
    title.set_css_classes(&["control-title"]);
    let description = gtk::Label::new(Some(description));
    description.set_halign(Align::Start);
    description.set_css_classes(&["control-description"]);
    copy.append(&title);
    copy.append(&description);
    let value = gtk::Label::new(Some("—"));
    value.set_css_classes(&["data", "control-value"]);
    heading.append(&copy);
    heading.append(&value);

    let scale = gtk::Scale::with_range(Orientation::Horizontal, minimum, maximum, step);
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.set_increments(step, step * 5.0);
    row.append(&heading);
    row.append(&scale);
    (row, scale, value)
}

fn build_battery_section() -> (gtk::Box, BatteryView, BatteryView, gtk::Label) {
    let section = gtk::Box::new(Orientation::Vertical, 16);
    section.append(&section_label(
        "BATTERY",
        "Each earbud reports independently through airpodsd",
    ));

    let pair = gtk::Box::new(Orientation::Horizontal, 22);
    pair.set_halign(Align::Center);
    pair.set_homogeneous(true);
    pair.set_css_classes(&["battery-pair"]);
    let (left_unit, left) = battery_capsule("L", "LEFT");
    let (right_unit, right) = battery_capsule("R", "RIGHT");
    pair.append(&left_unit);
    pair.append(&right_unit);
    section.append(&pair);
    let power_bridge = gtk::Label::new(Some("UPower bridge: waiting"));
    power_bridge.set_css_classes(&["power-bridge", "data"]);
    section.append(&power_bridge);
    (section, left, right, power_bridge)
}

fn battery_capsule(side: &str, name: &str) -> (gtk::Box, BatteryView) {
    let unit = gtk::Box::new(Orientation::Vertical, 0);
    unit.set_halign(Align::Center);
    unit.set_css_classes(&["earbud-unit"]);

    let capsule = gtk::Box::new(Orientation::Vertical, 5);
    capsule.set_css_classes(&["battery-capsule"]);
    capsule.set_valign(Align::Center);
    let side_label = gtk::Label::new(Some(side));
    side_label.set_css_classes(&["battery-side", "data"]);
    let value = gtk::Label::new(Some("—"));
    value.set_css_classes(&["battery-value", "data"]);
    let name_label = gtk::Label::new(Some(name));
    name_label.set_css_classes(&["battery-name"]);
    let bar = gtk::ProgressBar::new();
    bar.set_fraction(0.0);
    capsule.append(&side_label);
    capsule.append(&value);
    capsule.append(&name_label);
    capsule.append(&bar);

    let stem = gtk::Box::new(Orientation::Vertical, 0);
    stem.set_css_classes(&["earbud-stem"]);
    unit.append(&capsule);
    unit.append(&stem);
    (unit, BatteryView { value, bar })
}

fn section_label(eyebrow: &str, description: &str) -> gtk::Box {
    let heading = gtk::Box::new(Orientation::Vertical, 2);
    let label = gtk::Label::new(Some(eyebrow));
    label.set_halign(Align::Start);
    label.set_css_classes(&["section-label"]);
    let description = gtk::Label::new(Some(description));
    description.set_halign(Align::Start);
    description.set_wrap(true);
    description.set_css_classes(&["section-description"]);
    heading.append(&label);
    heading.append(&description);
    heading
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
                    set_daemon_badge(&view, "UNAVAILABLE", "state-error");
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

    let selected = selected_address
        .and_then(|address| devices.iter().find(|device| device.address == address));
    let state = match selected {
        Some(device) if device.connected => "CONNECTED",
        Some(_) => "NOT CONNECTED",
        None if is_empty => "NO DEVICE",
        None => "SELECT A DEVICE",
    };
    view.device_state.set_text(state);
}

fn update_status(view: &View, status: &DaemonStatus) {
    let normalized_state = status.state.to_ascii_lowercase();
    let state_class = match normalized_state.as_str() {
        "streaming" | "idle" | "ready" => "state-ok",
        "error" => "state-error",
        _ => "state-warn",
    };
    set_daemon_badge(view, &status.state.to_uppercase(), state_class);

    let has_device = !status.selected_device.is_empty();
    view.controls.set_sensitive(has_device);
    view.mic_button.set_sensitive(has_device);
    view.mic_active.set(status.mic_active);
    view.mic_button.set_label(if status.mic_active {
        "Stop microphone"
    } else {
        "Start microphone"
    });

    view.updating.set(true);
    view.gain_scale.set_value(status.gain_db);
    view.limiter_scale.set_value(status.limiter_db);
    view.updating.set(false);
    view.gain_value
        .set_text(&format!("{:.0} dB", status.gain_db));
    view.limiter_value
        .set_text(&format!("{:.0} dBFS", status.limiter_db));
    view.power_bridge
        .set_text(if status.power_bridge_available {
            "UPower bridge: online"
        } else {
            "UPower bridge: unavailable"
        });

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

fn set_daemon_badge(view: &View, text: &str, class: &str) {
    view.daemon_badge.set_text(text);
    view.daemon_badge.remove_css_class("state-ok");
    view.daemon_badge.remove_css_class("state-warn");
    view.daemon_badge.remove_css_class("state-error");
    view.daemon_badge.add_css_class(class);
}

fn show_error(view: &View, message: &str) {
    view.error_label.set_text(message);
    view.error_revealer.set_reveal_child(true);
}

fn clear_error(view: &View) {
    view.error_revealer.set_reveal_child(false);
}
