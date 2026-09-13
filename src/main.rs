use async_channel::{bounded, Receiver, Sender};
use ffmpeg_sidecar::command::ffmpeg_is_installed;
use ffmpeg_sidecar::download::auto_download;
use ffmpeg_sidecar::paths::ffmpeg_path;
use filetime::{set_file_times, FileTime};
use gio::ListStore;
use glib::clone;
use glib::prelude::*;
use glib::subclass::prelude::*;
use gtk4::gdk::Display;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box, Button, ColumnView, ColumnViewColumn,
    CssProvider, Entry, FileChooserAction, FileChooserNative, Grid,
    Image, Label, MessageDialog, Orientation, Overlay, ProgressBar, ResponseType,
    Scale, ScrolledWindow, Settings, SignalListItemFactory, SingleSelection,
    Stack, StackTransitionType,
};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const SUPPORTED_EXTS: &[&str] = &["mp4", "mkv", "mov", "avi", "wmv", "flv", "webm", "m4v"];
const MAX_UI_STORE_ITEMS: u32 = 600;

// ---------------- GObject Item Model ----------------

mod imp {
    use super::*;
    use glib::Properties;
    use std::cell::{Cell, RefCell};

    #[derive(Default, Properties)]
    #[properties(wrapper_type = super::VideoItem)]
    pub struct VideoItem {
        #[property(get, set)]
        pub id: Cell<u64>,
        #[property(get, set)]
        pub name: RefCell<String>,
        #[property(get, set)]
        pub path_display: RefCell<String>,
        #[property(get, set)]
        pub size_display: RefCell<String>,
        #[property(get, set)]
        pub status: RefCell<String>,
        #[property(get, set)]
        pub progress: Cell<f64>,
        #[property(get, set)]
        pub state: RefCell<String>,
        #[property(get, set)]
        pub can_skip: Cell<bool>,
        pub path: RefCell<PathBuf>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for VideoItem {
        const NAME: &'static str = "VideoItem";
        type Type = super::VideoItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for VideoItem {}
}

glib::wrapper! {
    pub struct VideoItem(ObjectSubclass<imp::VideoItem>);
}

impl VideoItem {
    pub fn new(id: u64, path: PathBuf) -> Self {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let path_display = path.to_string_lossy().to_string();
        let initial_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let size_display = format!("{:.1} MB", initial_size as f64 / (1024.0 * 1024.0));

        let item: Self = glib::Object::builder()
            .property("id", id)
            .property("name", &name)
            .property("path-display", &path_display)
            .property("size-display", &size_display)
            .property("status", &"Queued".to_string())
            .property("progress", 0.0f64)
            .property("state", &"queued".to_string())
            .property("can-skip", true)
            .build();
        *item.imp().path.borrow_mut() = path;
        item
    }
}

// ---------------- Inter-Thread Messages ----------------

enum WorkerMsg {
    ScanStats { scanned: usize, candidate_count: usize },
    ScanComplete { total_candidates: usize, encoder_name: String, worker_count: usize },
    ItemEnqueued { id: u64, path: PathBuf },
    ItemStarted { id: u64, name: String },
    ItemProgress { id: u64, fraction: f64, status: String },
    ItemCompleted { id: u64, status: String, size_display: String, is_success: bool, is_error: bool },
    StatsUpdate { processed: usize, skipped: usize, total_saved_mb: f64, overall_fraction: f64 },
    OverallDone { total_saved_mb: f64, processed: usize, skipped: usize, elapsed_secs: u64 },
    Stopped,
    Error(String),
}

#[derive(Clone)]
struct AppConfig {
    folder: PathBuf,
    meta_key: String,
    meta_val: String,
    quality_level: u32,
    manual_threads: u32,
}

// ---------------- OS Theme Synchronization ----------------

fn is_system_dark_mode() -> bool {
    #[cfg(target_os = "macos")]
    {
        Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().eq_ignore_ascii_case("Dark"))
            .unwrap_or(false)
    }

    #[cfg(target_os = "linux")]
    {
        Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "color-scheme"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains("prefer-dark"))
            .unwrap_or(false)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

fn sync_system_theme() {
    if let Some(settings) = Settings::default() {
        let is_dark = is_system_dark_mode();
        let current = settings.property::<bool>("gtk-application-prefer-dark-theme");
        if current != is_dark {
            settings.set_gtk_application_prefer_dark_theme(is_dark);
        }
    }
}

// ---------------- Main UI Entry ----------------

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id("com.vault.video_optimizer")
        .build();

    app.connect_startup(|_| {
        load_custom_css();
    });

    app.connect_activate(build_ui);
    app.run()
}

fn load_custom_css() {
    let provider = CssProvider::new();
    provider.load_from_data(
        "
        window,
        window.background {
            background-color: #121316;
            color: #ececf1;
        }

        .hero-card {
            background-color: #1a1c22;
            border: 1px solid #2b2e38;
            border-radius: 12px;
            padding: 28px 24px;
        }

        .hero-title {
            font-size: 20px;
            font-weight: 700;
            color: #ffffff;
            letter-spacing: -0.3px;
        }

        .hero-subtitle {
            font-size: 13px;
            color: #8c92a4;
        }

        .path-chip {
            background-color: #121316;
            border: 1px solid #282a33;
            border-radius: 6px;
            padding: 7px 12px;
        }

        .path-text {
            font-family: monospace;
            font-size: 12px;
            color: #cbd5e1;
        }

        /* Settings Card & Panel */
        .settings-card {
            background-color: #181a20;
            border: 1px solid #2b2e38;
            border-radius: 10px;
            padding: 16px 20px;
        }

        .settings-title {
            font-size: 15px;
            font-weight: 700;
            color: #ffffff;
        }

        .settings-desc {
            font-size: 12px;
            color: #838a9a;
        }

        .settings-entry {
            background-color: #101114;
            color: #ffffff;
            border: 1px solid #2c303c;
            border-radius: 6px;
            padding: 5px 10px;
            font-family: monospace;
            font-size: 12px;
        }

        .btn-action-primary {
            background-color: #2563eb;
            color: #ffffff;
            font-weight: 600;
            font-size: 13.5px;
            border-radius: 8px;
            padding: 10px 24px;
            border: 1px solid #3b82f6;
        }

        .btn-action-primary:hover {
            background-color: #1d4ed8;
        }

        .btn-action-primary:disabled {
            background-color: #22242c;
            color: #555b68;
            border-color: #2c2f38;
        }

        .btn-secondary {
            background-color: #242630;
            color: #f1f5f9;
            border: 1px solid #363946;
            border-radius: 6px;
            font-weight: 600;
            padding: 7px 16px;
        }

        .btn-secondary:hover {
            background-color: #2d303d;
        }

        .btn-icon {
            padding: 8px;
            border-radius: 6px;
            background-color: #242630;
            border: 1px solid #363946;
        }

        .hud-card {
            background-color: #191b21;
            border: 1px solid #262933;
            border-radius: 8px;
            padding: 10px 14px;
        }

        .hud-pill {
            background-color: #22242c;
            border: 1px solid #2f323c;
            border-radius: 6px;
            padding: 4px 10px;
            font-size: 11.5px;
            font-weight: 600;
            color: #94a3b8;
        }

        .hud-pill.active-hardware {
            border-color: rgba(34, 197, 94, 0.4);
            color: #4ade80;
            background-color: rgba(34, 197, 94, 0.1);
        }

        .grid-header {
            background-color: #181a20;
            border: 1px solid #252833;
            border-bottom: none;
            padding: 8px 12px;
            font-size: 11.5px;
            font-weight: 700;
            color: #828a9b;
        }

        columnview {
            background-color: #15161b;
            border: 1px solid #242630;
            border-radius: 0px;
        }

        columnview row {
            border-radius: 0px;
            margin: 0px;
            padding: 0px;
            border-bottom: 1px solid #1c1e26;
            background-color: transparent;
        }

        columnview row cell {
            padding: 0px;
            margin: 0px;
            border-radius: 0px;
            background-color: transparent;
        }

        /* Ambient Row Progress Bar */
        .ambient-row-progress trough {
            min-height: 48px;
            border-radius: 0px;
            margin: 0px;
            padding: 0px;
            background-color: transparent;
            border: none;
        }

        .ambient-row-progress progress {
            min-height: 48px;
            border-radius: 0px;
            margin: 0px;
            padding: 0px;
            background-color: rgba(234, 179, 8, 0.22);
            border: none;
        }

        .ambient-row-progress.state-done progress {
            background-color: rgba(34, 197, 94, 0.25);
        }

        .ambient-row-progress.state-error progress {
            background-color: rgba(239, 68, 68, 0.28);
        }

        .row-title {
            font-size: 12.5px;
            font-weight: 600;
            color: #ffffff;
        }

        .row-subtitle {
            font-size: 10.5px;
            color: #6e7585;
            font-family: monospace;
        }

        .row-size-text {
            font-size: 11.5px;
            font-family: monospace;
            font-weight: 600;
            color: #94a3b8;
        }

        .status-badge {
            border-radius: 4px;
            padding: 3px 8px;
            font-size: 11px;
            font-weight: 600;
            font-feature-settings: 'tnum';
            background-color: #1e2027;
            border: 1px solid #2e313c;
            color: #94a3b8;
        }

        .status-badge.state-done {
            background-color: rgba(34, 197, 94, 0.15);
            border-color: rgba(34, 197, 94, 0.35);
            color: #4ade80;
        }

        .status-badge.state-error {
            background-color: rgba(239, 68, 68, 0.15);
            border-color: rgba(239, 68, 68, 0.35);
            color: #f87171;
        }

        .status-badge.state-processing {
            background-color: rgba(234, 179, 8, 0.15);
            border-color: rgba(234, 179, 8, 0.35);
            color: #facc15;
        }

        .btn-skip {
            padding: 2px 10px;
            font-size: 11px;
            font-weight: 600;
            border-radius: 4px;
            background-color: #1e2027;
            color: #cbd5e1;
            border: 1px solid #31343f;
        }

        .btn-skip:hover {
            background-color: #272a33;
            color: #ffffff;
        }

        .btn-skip:disabled {
            opacity: 0.25;
            background-color: #15161b;
            border-color: #22242c;
            color: #4b5260;
        }

        .metrics-card {
            background-color: #1a1c22;
            border: 1px solid #262933;
            border-radius: 8px;
            padding: 10px 14px;
        }

        .bottom-bar trough {
            min-height: 6px;
            border-radius: 3px;
            background-color: #121316;
            border: none;
        }

        .bottom-bar progress {
            min-height: 6px;
            border-radius: 3px;
            background-color: #3b82f6;
            border: none;
        }

        .stat-pill {
            font-size: 11.5px;
            font-weight: 600;
            font-feature-settings: 'tnum';
            color: #94a3b8;
        }
        ",
    );

    if let Some(display) = Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_ui(app: &Application) {
    sync_system_theme();
    glib::timeout_add_local(Duration::from_secs(2), move || {
        sync_system_theme();
        glib::ControlFlow::Continue
    });

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Vault Video Optimizer Studio")
        .default_width(620)
        .default_height(460)
        .resizable(false)
        .build();

    let root_stack = Stack::builder()
        .transition_type(StackTransitionType::SlideLeftRight)
        .transition_duration(200)
        .build();

    // Stored Persistent App Settings
    let current_meta_key = Arc::new(Mutex::new("comment".to_string()));
    let current_meta_val = Arc::new(Mutex::new("optimized_by_vault_v4".to_string()));
    let current_quality = Arc::new(Mutex::new(50u32));
    let current_threads = Arc::new(Mutex::new(0u32)); // 0 = Auto

    // ---------------- VIEW 1: Launch Screen ----------------
    let setup_view = Box::new(Orientation::Vertical, 14);
    setup_view.set_margin_top(24);
    setup_view.set_margin_bottom(24);
    setup_view.set_margin_start(32);
    setup_view.set_margin_end(32);
    setup_view.set_valign(gtk4::Align::Center);

    // Top Header with Title and Settings Gear Button
    let top_nav = Box::new(Orientation::Horizontal, 10);
    let hero_icon = Image::from_icon_name("folder-videos-symbolic");
    hero_icon.set_pixel_size(36);

    let title_vbox = Box::new(Orientation::Vertical, 2);
    let title_label = Label::builder()
        .label("Vault Video Optimizer Studio")
        .xalign(0.0)
        .css_classes(["hero-title"])
        .build();
    let desc_label = Label::builder()
        .label("High-speed parallel batch compression engine.")
        .xalign(0.0)
        .css_classes(["hero-subtitle"])
        .build();
    title_vbox.append(&title_label);
    title_vbox.append(&desc_label);

    let settings_nav_btn = Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Open Settings Panel")
        .css_classes(["btn-icon"])
        .valign(gtk4::Align::Center)
        .build();

    top_nav.append(&hero_icon);
    top_nav.append(&title_vbox);
    top_nav.append(&Label::builder().hexpand(true).build()); // Spacer
    top_nav.append(&settings_nav_btn);
    setup_view.append(&top_nav);

    let hero_card = Box::new(Orientation::Vertical, 12);
    hero_card.add_css_class("hero-card");

    let path_container = Box::new(Orientation::Horizontal, 8);
    path_container.add_css_class("path-chip");
    path_container.set_halign(gtk4::Align::Center);
    path_container.set_visible(false);

    let path_chip_icon = Image::from_icon_name("folder-symbolic");
    let path_label = Label::builder()
        .xalign(0.5)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .max_width_chars(42)
        .css_classes(["path-text"])
        .build();

    path_container.append(&path_chip_icon);
    path_container.append(&path_label);
    hero_card.append(&path_container);

    let browse_btn = Button::builder()
        .label("Browse Target Folder...")
        .halign(gtk4::Align::Center)
        .css_classes(["btn-secondary"])
        .build();
    hero_card.append(&browse_btn);

    setup_view.append(&hero_card);

    let start_btn = Button::builder()
        .label("Start Optimization")
        .sensitive(false)
        .css_classes(["btn-action-primary"])
        .height_request(44)
        .build();
    setup_view.append(&start_btn);

    root_stack.add_named(&setup_view, Some("setup"));

    // ---------------- VIEW 2: Dedicated Settings Panel ----------------
    let settings_view = Box::new(Orientation::Vertical, 14);
    settings_view.set_margin_top(20);
    settings_view.set_margin_bottom(20);
    settings_view.set_margin_start(28);
    settings_view.set_margin_end(28);

    let settings_header = Box::new(Orientation::Horizontal, 10);
    let settings_title = Label::builder()
        .label("Settings & Configuration")
        .xalign(0.0)
        .css_classes(["hero-title"])
        .hexpand(true)
        .build();
    let settings_back_btn = Button::builder()
        .label("Cancel")
        .css_classes(["btn-secondary"])
        .build();
    settings_header.append(&settings_title);
    settings_header.append(&settings_back_btn);
    settings_view.append(&settings_header);

    let settings_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();

    let settings_content = Box::new(Orientation::Vertical, 12);
    settings_content.set_margin_top(6);

    // Section 1: Metadata Tag Management
    let tag_card = Box::new(Orientation::Vertical, 8);
    tag_card.add_css_class("settings-card");

    let tag_card_title = Label::builder()
        .label("Metadata Tag Marker")
        .xalign(0.0)
        .css_classes(["settings-title"])
        .build();
    let tag_card_desc = Label::builder()
        .label("Marker embedded into optimized files to skip redundant passes in recursive scans.")
        .xalign(0.0)
        .css_classes(["settings-desc"])
        .build();
    tag_card.append(&tag_card_title);
    tag_card.append(&tag_card_desc);

    let tag_grid = Grid::builder().row_spacing(8).column_spacing(12).margin_top(6).build();
    let key_lbl = Label::builder().label("Tag Key:").xalign(0.0).build();
    let key_entry = Entry::builder().text("comment").css_classes(["settings-entry"]).hexpand(true).build();
    let val_lbl = Label::builder().label("Tag Value:").xalign(0.0).build();
    let val_entry = Entry::builder().text("optimized_by_vault_v4").css_classes(["settings-entry"]).hexpand(true).build();

    tag_grid.attach(&key_lbl, 0, 0, 1, 1);
    tag_grid.attach(&key_entry, 1, 0, 1, 1);
    tag_grid.attach(&val_lbl, 0, 1, 1, 1);
    tag_grid.attach(&val_entry, 1, 1, 1, 1);
    tag_card.append(&tag_grid);
    settings_content.append(&tag_card);

    // Section 2: Quality & Compression Level
    let quality_card = Box::new(Orientation::Vertical, 8);
    quality_card.add_css_class("settings-card");

    let quality_title = Label::builder()
        .label("Encoding Quality (Target VBR)")
        .xalign(0.0)
        .css_classes(["settings-title"])
        .build();
    let quality_desc = Label::builder()
        .label("Lower values yield smaller files; higher values preserve original pixel fidelity.")
        .xalign(0.0)
        .css_classes(["settings-desc"])
        .build();
    quality_card.append(&quality_title);
    quality_card.append(&quality_desc);

    let quality_box = Box::new(Orientation::Horizontal, 12);
    let quality_scale = Scale::with_range(Orientation::Horizontal, 20.0, 80.0, 1.0);
    quality_scale.set_value(50.0);
    quality_scale.set_hexpand(true);
    let quality_val_lbl = Label::builder().label("Level: 50 (Standard)").xalign(1.0).build();

    quality_scale.connect_value_changed(clone!(
        #[weak] quality_val_lbl,
        move |s| {
            let val = s.value().round() as u32;
            let desc = if val < 40 { "High Compression" } else if val > 60 { "High Fidelity" } else { "Balanced" };
            quality_val_lbl.set_text(&format!("Level: {val} ({desc})"));
        }
    ));

    quality_box.append(&quality_scale);
    quality_box.append(&quality_val_lbl);
    quality_card.append(&quality_box);
    settings_content.append(&quality_card);

    // Section 3: Concurrency Thread Control
    let thread_card = Box::new(Orientation::Vertical, 8);
    thread_card.add_css_class("settings-card");

    let thread_title = Label::builder()
        .label("Parallel Workers")
        .xalign(0.0)
        .css_classes(["settings-title"])
        .build();
    let thread_desc = Label::builder()
        .label("Set to 0 for automatic hardware core assignment, or limit concurrent encoding workers manually.")
        .xalign(0.0)
        .css_classes(["settings-desc"])
        .build();
    thread_card.append(&thread_title);
    thread_card.append(&thread_desc);

    let thread_box = Box::new(Orientation::Horizontal, 12);
    let thread_scale = Scale::with_range(Orientation::Horizontal, 0.0, 8.0, 1.0);
    thread_scale.set_value(0.0);
    thread_scale.set_hexpand(true);
    let thread_val_lbl = Label::builder().label("Auto (Balanced)").xalign(1.0).build();

    thread_scale.connect_value_changed(clone!(
        #[weak] thread_val_lbl,
        move |s| {
            let val = s.value().round() as u32;
            if val == 0 {
                thread_val_lbl.set_text("Auto (Balanced)");
            } else {
                thread_val_lbl.set_text(&format!("{val} Concurrent Workers"));
            }
        }
    ));

    thread_box.append(&thread_scale);
    thread_box.append(&thread_val_lbl);
    thread_card.append(&thread_box);
    settings_content.append(&thread_card);

    settings_scroll.set_child(Some(&settings_content));
    settings_view.append(&settings_scroll);

    // Save Settings Button
    let save_settings_btn = Button::builder()
        .label("Apply Settings & Return")
        .css_classes(["btn-action-primary"])
        .height_request(40)
        .build();
    settings_view.append(&save_settings_btn);

    root_stack.add_named(&settings_view, Some("settings"));

    // ---------------- VIEW 3: Executive Data Grid ----------------
    let process_view = Box::new(Orientation::Vertical, 8);
    process_view.set_margin_top(12);
    process_view.set_margin_bottom(12);
    process_view.set_margin_start(12);
    process_view.set_margin_end(12);

    let hud_card = Box::new(Orientation::Horizontal, 10);
    hud_card.add_css_class("hud-card");

    let encoder_hud_pill = Label::builder()
        .label("Probing Encoder...")
        .css_classes(["hud-pill", "active-hardware"])
        .build();
    let thread_hud_pill = Label::builder()
        .label("Threads: --")
        .css_classes(["hud-pill"])
        .build();
    let tag_hud_pill = Label::builder()
        .label("Tag: comment=...")
        .css_classes(["hud-pill"])
        .build();

    hud_card.append(&encoder_hud_pill);
    hud_card.append(&thread_hud_pill);
    hud_card.append(&tag_hud_pill);
    process_view.append(&hud_card);

    let header_box = Box::new(Orientation::Horizontal, 12);
    header_box.add_css_class("grid-header");
    let h1 = Label::builder().label("Video File").xalign(0.0).hexpand(true).build();
    let h2 = Label::builder().label("Size Delta").xalign(0.0).width_request(170).build();
    let h3 = Label::builder().label("Status & Telemetry").xalign(1.0).width_request(210).build();
    let h4 = Label::builder().label("Action").xalign(0.5).width_request(70).build();
    header_box.append(&h1);
    header_box.append(&h2);
    header_box.append(&h3);
    header_box.append(&h4);
    process_view.append(&header_box);

    let store = ListStore::new::<VideoItem>();
    let selection_model = SingleSelection::new(Some(store.clone()));
    let column_view = ColumnView::new(Some(selection_model));
    column_view.set_show_row_separators(false);
    column_view.set_show_column_separators(false);

    let skipped_ids = Arc::new(Mutex::new(HashSet::<u64>::new()));
    let active_children: Arc<Mutex<HashMap<u64, Child>>> = Arc::new(Mutex::new(HashMap::new()));

    let row_factory = SignalListItemFactory::new();
    row_factory.connect_setup(|_, list_item| {
        let overlay = Overlay::builder().hexpand(true).vexpand(true).build();

        let progress_bar = ProgressBar::builder()
            .show_text(false)
            .hexpand(true)
            .vexpand(true)
            .css_classes(["ambient-row-progress"])
            .build();

        let row_content = Box::new(Orientation::Horizontal, 12);
        row_content.set_margin_start(12);
        row_content.set_margin_end(12);
        row_content.set_margin_top(6);
        row_content.set_margin_bottom(6);
        row_content.set_hexpand(true);

        let col1_box = Box::new(Orientation::Horizontal, 10);
        col1_box.set_hexpand(true);
        let icon = Image::from_icon_name("video-x-generic-symbolic");
        icon.set_opacity(0.6);

        let labels_box = Box::new(Orientation::Vertical, 2);
        let name_label = Label::builder()
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .css_classes(["row-title"])
            .build();
        let path_label = Label::builder()
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .css_classes(["row-subtitle"])
            .build();
        labels_box.append(&name_label);
        labels_box.append(&path_label);
        col1_box.append(&icon);
        col1_box.append(&labels_box);

        let size_label = Label::builder()
            .xalign(0.0)
            .width_request(170)
            .css_classes(["row-size-text"])
            .build();

        let status_box = Box::new(Orientation::Horizontal, 0);
        status_box.set_width_request(210);
        status_box.set_halign(gtk4::Align::End);
        status_box.set_valign(gtk4::Align::Center);
        let status_badge = Label::builder()
            .xalign(0.5)
            .css_classes(["status-badge"])
            .build();
        status_box.append(&status_badge);

        let action_box = Box::new(Orientation::Horizontal, 0);
        action_box.set_width_request(70);
        action_box.set_halign(gtk4::Align::Center);
        action_box.set_valign(gtk4::Align::Center);
        let skip_btn = Button::builder()
            .label("Skip")
            .css_classes(["btn-skip"])
            .build();
        action_box.append(&skip_btn);

        row_content.append(&col1_box);
        row_content.append(&size_label);
        row_content.append(&status_box);
        row_content.append(&action_box);

        overlay.set_child(Some(&progress_bar));
        overlay.add_overlay(&row_content);

        list_item.downcast_ref::<gtk4::ListItem>().unwrap().set_child(Some(&overlay));
    });

    row_factory.connect_bind(clone!(
        #[strong] skipped_ids,
        #[strong] active_children,
        move |_, list_item| {
            let item = list_item
                .downcast_ref::<gtk4::ListItem>()
                .unwrap()
                .item()
                .and_downcast::<VideoItem>()
                .unwrap();
            let overlay = list_item
                .downcast_ref::<gtk4::ListItem>()
                .unwrap()
                .child()
                .and_downcast::<Overlay>()
                .unwrap();

            let progress_bar = overlay.child().unwrap().downcast::<ProgressBar>().unwrap();
            let row_content = overlay.last_child().unwrap().downcast::<Box>().unwrap();

            let col1_box = row_content.first_child().unwrap().downcast::<Box>().unwrap();
            let labels_box = col1_box.last_child().unwrap().downcast::<Box>().unwrap();
            let name_label = labels_box.first_child().unwrap().downcast::<Label>().unwrap();
            let path_label = labels_box.last_child().unwrap().downcast::<Label>().unwrap();

            let size_label = col1_box.next_sibling().unwrap().downcast::<Label>().unwrap();
            let status_box = size_label.next_sibling().unwrap().downcast::<Box>().unwrap();
            let status_badge = status_box.first_child().unwrap().downcast::<Label>().unwrap();

            let action_box = status_box.next_sibling().unwrap().downcast::<Box>().unwrap();
            let skip_btn = action_box.first_child().unwrap().downcast::<Button>().unwrap();

            item.bind_property("name", &name_label, "label").sync_create().build();
            item.bind_property("path-display", &path_label, "label").sync_create().build();
            item.bind_property("size-display", &size_label, "label").sync_create().build();
            item.bind_property("status", &status_badge, "label").sync_create().build();
            item.bind_property("progress", &progress_bar, "fraction").sync_create().build();
            item.bind_property("can-skip", &skip_btn, "sensitive").sync_create().build();

            item.connect_notify_local(
                Some("state"),
                clone!(
                    #[weak] progress_bar,
                    #[weak] status_badge,
                    move |item, _| {
                        progress_bar.remove_css_class("state-done");
                        progress_bar.remove_css_class("state-error");
                        status_badge.remove_css_class("state-done");
                        status_badge.remove_css_class("state-error");
                        status_badge.remove_css_class("state-processing");

                        match item.state().as_str() {
                            "done" => {
                                progress_bar.add_css_class("state-done");
                                status_badge.add_css_class("state-done");
                            }
                            "error" => {
                                progress_bar.add_css_class("state-error");
                                status_badge.add_css_class("state-error");
                            }
                            "processing" => {
                                status_badge.add_css_class("state-processing");
                            }
                            _ => {}
                        }
                    }
                ),
            );

            progress_bar.remove_css_class("state-done");
            progress_bar.remove_css_class("state-error");
            status_badge.remove_css_class("state-done");
            status_badge.remove_css_class("state-error");
            status_badge.remove_css_class("state-processing");

            match item.state().as_str() {
                "done" => {
                    progress_bar.add_css_class("state-done");
                    status_badge.add_css_class("state-done");
                }
                "error" => {
                    progress_bar.add_css_class("state-error");
                    status_badge.add_css_class("state-error");
                }
                "processing" => {
                    status_badge.add_css_class("state-processing");
                }
                _ => {}
            }

            let skipped_ids = skipped_ids.clone();
            let active_children = active_children.clone();

            skip_btn.connect_clicked(clone!(
                #[weak] item,
                move |btn| {
                    let id = item.id();
                    btn.set_sensitive(false);
                    item.set_can_skip(false);
                    item.set_status("Skipping...");

                    if let Ok(mut set) = skipped_ids.lock() {
                        set.insert(id);
                    }

                    let mut child_to_kill = None;
                    if let Ok(mut map) = active_children.lock() {
                        if let Some(child) = map.remove(&id) {
                            child_to_kill = Some(child);
                        }
                    }

                    if let Some(mut child) = child_to_kill {
                        thread::spawn(move || {
                            let _ = child.kill();
                        });
                    }
                }
            ));
        }
    ));

    let row_col = ColumnViewColumn::builder()
        .factory(&row_factory)
        .expand(true)
        .build();
    column_view.append_column(&row_col);

    let scroll_window = ScrolledWindow::builder()
        .child(&column_view)
        .vexpand(true)
        .build();
    process_view.append(&scroll_window);

    // Bottom Action Dock
    let metrics_card = Box::new(Orientation::Vertical, 8);
    metrics_card.add_css_class("metrics-card");

    let top_stat_row = Box::new(Orientation::Horizontal, 12);
    let global_status_label = Label::builder()
        .label("Initializing pipeline...")
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["stat-pill"])
        .build();
    let stats_label = Label::builder()
        .label("Queued: 0  •  Processed: 0  •  Saved: 0.0 MB")
        .xalign(1.0)
        .css_classes(["stat-pill"])
        .build();
    top_stat_row.append(&global_status_label);
    top_stat_row.append(&stats_label);

    let overall_progress_bar = ProgressBar::builder()
        .show_text(false)
        .fraction(0.0)
        .css_classes(["bottom-bar"])
        .build();

    let control_row = Box::new(Orientation::Horizontal, 10);
    let stop_btn = Button::builder()
        .label("Stop Pipeline")
        .css_classes(["destructive-action", "btn-secondary"])
        .hexpand(true)
        .build();
    let back_btn = Button::builder()
        .label("Back to Directory")
        .css_classes(["btn-secondary"])
        .hexpand(true)
        .sensitive(false)
        .build();
    control_row.append(&stop_btn);
    control_row.append(&back_btn);

    metrics_card.append(&top_stat_row);
    metrics_card.append(&overall_progress_bar);
    metrics_card.append(&control_row);
    process_view.append(&metrics_card);

    root_stack.add_named(&process_view, Some("process"));
    window.set_child(Some(&root_stack));

    // Internal pipeline trackers
    let chosen_path: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let is_running = Arc::new(AtomicBool::new(false));
    let stop_requested = Arc::new(AtomicBool::new(false));

    // Navigation: Open Settings View
    settings_nav_btn.connect_clicked(clone!(
        #[weak] root_stack,
        move |_| {
            root_stack.set_visible_child_name("settings");
        }
    ));

    // Navigation: Cancel / Back from Settings View
    settings_back_btn.connect_clicked(clone!(
        #[weak] root_stack,
        move |_| {
            root_stack.set_visible_child_name("setup");
        }
    ));

    // Save Settings Button
    save_settings_btn.connect_clicked(clone!(
        #[weak] root_stack,
        #[weak] key_entry,
        #[weak] val_entry,
        #[weak] quality_scale,
        #[weak] thread_scale,
        #[strong] current_meta_key,
        #[strong] current_meta_val,
        #[strong] current_quality,
        #[strong] current_threads,
        move |_| {
            let k = key_entry.text().to_string();
            let v = val_entry.text().to_string();
            let q = quality_scale.value().round() as u32;
            let t = thread_scale.value().round() as u32;

            *current_meta_key.lock().unwrap() = if k.trim().is_empty() { "comment".into() } else { k.trim().into() };
            *current_meta_val.lock().unwrap() = if v.trim().is_empty() { "optimized_by_vault_v4".into() } else { v.trim().into() };
            *current_quality.lock().unwrap() = q;
            *current_threads.lock().unwrap() = t;

            root_stack.set_visible_child_name("setup");
        }
    ));

    // Browse Folder Action
    browse_btn.connect_clicked(clone!(
        #[weak] window,
        #[weak] path_container,
        #[weak] path_label,
        #[weak] start_btn,
        #[strong] chosen_path,
        move |_| {
            let chooser = FileChooserNative::new(
                Some("Select Video Folder"),
                Some(&window),
                FileChooserAction::SelectFolder,
                Some("Select"),
                Some("Cancel"),
            );

            let chosen_path = chosen_path.clone();
            chooser.connect_response(clone!(
                #[weak] path_container,
                #[weak] path_label,
                #[weak] start_btn,
                move |dialog, response| {
                    if response == ResponseType::Accept {
                        if let Some(file) = dialog.file() {
                            if let Some(path) = file.path() {
                                path_label.set_text(&path.to_string_lossy());
                                path_container.set_visible(true);
                                start_btn.set_sensitive(true);
                                if let Ok(mut lock) = chosen_path.lock() {
                                    *lock = Some(path);
                                }
                            }
                        }
                    }
                }
            ));
            chooser.show();
        }
    ));

    back_btn.connect_clicked(clone!(
        #[weak] root_stack,
        #[weak] window,
        move |_| {
            window.set_resizable(false);
            window.set_default_size(620, 460);
            root_stack.set_visible_child_name("setup");
        }
    ));

    // MessageDialog confirmation before stopping pipeline
    stop_btn.connect_clicked(clone!(
        #[weak] window,
        #[weak] stop_btn,
        #[weak] global_status_label,
        #[strong] stop_requested,
        #[strong] active_children,
        move |_| {
            let dialog = MessageDialog::new(
                Some(&window),
                gtk4::DialogFlags::MODAL,
                gtk4::MessageType::Question,
                gtk4::ButtonsType::OkCancel,
                "Stop Video Optimization?",
            );
            dialog.set_secondary_text(Some(
                "Active conversions will be halted and temporary partial files will be cleaned up.",
            ));

            dialog.connect_response(clone!(
                #[weak] stop_btn,
                #[weak] global_status_label,
                #[strong] stop_requested,
                #[strong] active_children,
                move |dlg, response| {
                    if response == ResponseType::Ok {
                        stop_btn.set_sensitive(false);
                        stop_requested.store(true, Ordering::SeqCst);
                        global_status_label.set_text("Terminating active workers...");

                        let map_clone = active_children.clone();
                        thread::spawn(move || {
                            if let Ok(mut map) = map_clone.lock() {
                                for (_, mut child) in map.drain() {
                                    let _ = child.kill();
                                }
                            }
                        });
                    }
                    dlg.destroy();
                }
            ));

            dialog.show();
        }
    ));

    // Start Optimization Action
    start_btn.connect_clicked(clone!(
        #[weak] root_stack,
        #[weak] window,
        #[weak] global_status_label,
        #[weak] stats_label,
        #[weak] overall_progress_bar,
        #[weak] stop_btn,
        #[weak] back_btn,
        #[weak] store,
        #[weak] encoder_hud_pill,
        #[weak] thread_hud_pill,
        #[weak] tag_hud_pill,
        #[strong] chosen_path,
        #[strong] is_running,
        #[strong] stop_requested,
        #[strong] active_children,
        #[strong] skipped_ids,
        #[strong] current_meta_key,
        #[strong] current_meta_val,
        #[strong] current_quality,
        #[strong] current_threads,
        move |_| {
            let target_folder = match chosen_path.lock().unwrap().clone() {
                Some(p) => p,
                None => return,
            };

            let config = AppConfig {
                folder: target_folder,
                meta_key: current_meta_key.lock().unwrap().clone(),
                meta_val: current_meta_val.lock().unwrap().clone(),
                quality_level: *current_quality.lock().unwrap(),
                manual_threads: *current_threads.lock().unwrap(),
            };

            tag_hud_pill.set_text(&format!("Tag: {}={}", config.meta_key, config.meta_val));

            window.set_resizable(true);
            window.set_default_size(1050, 680);
            root_stack.set_visible_child_name("process");

            is_running.store(true, Ordering::SeqCst);
            stop_requested.store(false, Ordering::SeqCst);
            skipped_ids.lock().unwrap().clear();
            active_children.lock().unwrap().clear();

            stop_btn.set_visible(true);
            stop_btn.set_sensitive(true);
            back_btn.set_sensitive(false);
            store.remove_all();
            overall_progress_bar.set_fraction(0.0);
            global_status_label.set_text("Analyzing storage tree...");

            let (sender, receiver) = bounded::<WorkerMsg>(2000);

            glib::spawn_future_local(clone!(
                #[weak] global_status_label,
                #[weak] stats_label,
                #[weak] overall_progress_bar,
                #[weak] stop_btn,
                #[weak] back_btn,
                #[weak] store,
                #[weak] encoder_hud_pill,
                #[weak] thread_hud_pill,
                #[strong] is_running,
                async move {
                    handle_ui_events(
                        receiver,
                        store,
                        global_status_label,
                        stats_label,
                        overall_progress_bar,
                        stop_btn,
                        back_btn,
                        encoder_hud_pill,
                        thread_hud_pill,
                        is_running,
                    ).await;
                }
            ));

            let children_ref = active_children.clone();
            let stop_ref = stop_requested.clone();
            let skipped_ref = skipped_ids.clone();

            thread::spawn(move || {
                run_production_pipeline(config, sender, stop_ref, children_ref, skipped_ref);
            });
        }
    ));

    window.present();
}

// ---------------- UI Event Dispatcher ----------------

async fn handle_ui_events(
    receiver: Receiver<WorkerMsg>,
    store: ListStore,
    global_status: Label,
    stats_label: Label,
    overall_progress: ProgressBar,
    stop_btn: Button,
    back_btn: Button,
    encoder_hud: Label,
    thread_hud: Label,
    is_running: Arc<AtomicBool>,
) {
    while let Ok(msg) = receiver.recv().await {
        match msg {
            WorkerMsg::ScanStats { scanned, candidate_count } => {
                global_status.set_text(&format!(
                    "Scanning storage tree... Scanned: {scanned} files | Queued: {candidate_count}"
                ));
            }
            WorkerMsg::ScanComplete { total_candidates, encoder_name, worker_count } => {
                encoder_hud.set_text(&format!("GPU: {encoder_name}"));
                thread_hud.set_text(&format!("Workers: {worker_count} Threads"));
                global_status.set_text(&format!("Ready. Encoding {total_candidates} videos..."));
                stats_label.set_text(&format!("Queued: {total_candidates}  •  Processed: 0  •  Saved: 0.0 MB"));
            }
            WorkerMsg::ItemEnqueued { id, path } => {
                if store.n_items() < MAX_UI_STORE_ITEMS {
                    store.append(&VideoItem::new(id, path));
                }
            }
            WorkerMsg::ItemStarted { id, name } => {
                let mut found = false;
                for i in 0..store.n_items() {
                    if let Some(item) = store.item(i).and_downcast::<VideoItem>() {
                        if item.id() == id {
                            item.set_state("processing");
                            item.set_status("Processing...");
                            item.set_progress(0.0);
                            item.set_can_skip(true);
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    if store.n_items() >= MAX_UI_STORE_ITEMS {
                        store.remove(0);
                    }
                    let item = VideoItem::new(id, PathBuf::from(&name));
                    item.set_state("processing");
                    item.set_status("Processing...");
                    store.append(&item);
                }
            }
            WorkerMsg::ItemProgress { id, fraction, status } => {
                for i in 0..store.n_items() {
                    if let Some(item) = store.item(i).and_downcast::<VideoItem>() {
                        if item.id() == id {
                            item.set_progress(fraction);
                            item.set_status(&status[..]);
                            break;
                        }
                    }
                }
            }
            WorkerMsg::ItemCompleted { id, status, size_display, is_success, is_error } => {
                for i in 0..store.n_items() {
                    if let Some(item) = store.item(i).and_downcast::<VideoItem>() {
                        if item.id() == id {
                            item.set_status(&status[..]);
                            item.set_size_display(&size_display[..]);
                            item.set_progress(1.0);
                            item.set_can_skip(false);
                            if is_success {
                                item.set_state("done");
                            } else if is_error {
                                item.set_state("error");
                            } else {
                                item.set_state("skipped");
                            }
                            break;
                        }
                    }
                }
            }
            WorkerMsg::StatsUpdate { processed, skipped, total_saved_mb, overall_fraction } => {
                stats_label.set_text(&format!(
                    "Processed: {processed}  •  Skipped: {skipped}  •  Saved: {:.1} MB",
                    total_saved_mb
                ));
                overall_progress.set_fraction(overall_fraction);
            }
            WorkerMsg::OverallDone { total_saved_mb, processed, skipped, elapsed_secs } => {
                overall_progress.set_fraction(1.0);
                global_status.set_text(&format!(
                    "Batch Finished in {}s  •  Converted: {processed}  •  Skipped: {skipped}  •  Saved: {:.2} MB",
                    elapsed_secs, total_saved_mb
                ));
                is_running.store(false, Ordering::SeqCst);
                stop_btn.set_visible(false);
                stop_btn.set_sensitive(false);
                back_btn.set_sensitive(true);
            }
            WorkerMsg::Stopped => {
                global_status.set_text("Pipeline stopped by user.");
                is_running.store(false, Ordering::SeqCst);
                stop_btn.set_visible(false);
                stop_btn.set_sensitive(false);
                back_btn.set_sensitive(true);
            }
            WorkerMsg::Error(err) => {
                global_status.set_text(&format!("Execution failed: {err}"));
                is_running.store(false, Ordering::SeqCst);
                stop_btn.set_visible(false);
                stop_btn.set_sensitive(false);
                back_btn.set_sensitive(true);
            }
        }
    }
}

// ---------------- Production Pipeline Engine ----------------

#[derive(Clone)]
struct VideoTask {
    id: u64,
    path: PathBuf,
}

fn run_production_pipeline(
    cfg: AppConfig,
    sender: Sender<WorkerMsg>,
    stop_requested: Arc<AtomicBool>,
    active_children: Arc<Mutex<HashMap<u64, Child>>>,
    skipped_ids: Arc<Mutex<HashSet<u64>>>,
) {
    let start_time = Instant::now();

    if !ffmpeg_is_installed() {
        if let Err(e) = auto_download() {
            let _ = sender.send_blocking(WorkerMsg::Error(format!("FFmpeg setup failed: {e}")));
            return;
        }
    }

    let (encoder_name, encoder_args, is_hardware) = detect_best_available_encoder(cfg.quality_level);
    let num_cpus = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    
    // Choose between manual thread assignment or auto-scaled limit
    let worker_count = if cfg.manual_threads > 0 {
        cfg.manual_threads as usize
    } else if is_hardware {
        (num_cpus / 2).clamp(2, 4)
    } else {
        (num_cpus / 4).clamp(1, 2)
    };

    let mut tasks: Vec<VideoTask> = Vec::with_capacity(8192);
    let mut scanned_count = 0;
    let mut last_scan_ui_update = Instant::now();
    let mut current_id: u64 = 0;

    for entry in WalkDir::new(&cfg.folder).into_iter().filter_map(|e| e.ok()) {
        if stop_requested.load(Ordering::SeqCst) {
            let _ = sender.send_blocking(WorkerMsg::Stopped);
            return;
        }

        let path = entry.path();
        let is_video = path.is_file()
            && !entry.file_name().to_string_lossy().starts_with('.')
            && path.extension().and_then(|s| s.to_str()).map_or(false, |ext| {
                SUPPORTED_EXTS.contains(&ext.to_lowercase().as_str())
            });

        if is_video {
            scanned_count += 1;

            if !has_fast_metadata_tag(path, &cfg.meta_val) {
                current_id += 1;
                let task = VideoTask { id: current_id, path: path.to_path_buf() };
                tasks.push(task);

                if current_id <= MAX_UI_STORE_ITEMS as u64 {
                    let _ = sender.send_blocking(WorkerMsg::ItemEnqueued {
                        id: current_id,
                        path: path.to_path_buf(),
                    });
                }
            }

            if last_scan_ui_update.elapsed() >= Duration::from_millis(150) {
                let _ = sender.send_blocking(WorkerMsg::ScanStats {
                    scanned: scanned_count,
                    candidate_count: tasks.len(),
                });
                last_scan_ui_update = Instant::now();
            }
        }
    }

    let total_tasks = tasks.len();
    let _ = sender.send_blocking(WorkerMsg::ScanComplete {
        total_candidates: total_tasks,
        encoder_name: encoder_name.clone(),
        worker_count,
    });

    if total_tasks == 0 {
        let _ = sender.send_blocking(WorkerMsg::OverallDone {
            total_saved_mb: 0.0,
            processed: 0,
            skipped: 0,
            elapsed_secs: start_time.elapsed().as_secs(),
        });
        return;
    }

    let (task_tx, task_rx) = bounded::<VideoTask>(tasks.len());
    for task in tasks {
        let _ = task_tx.send_blocking(task);
    }
    drop(task_tx);

    let processed_counter = Arc::new(AtomicUsize::new(0));
    let skipped_counter = Arc::new(AtomicUsize::new(0));
    let total_saved_bytes = Arc::new(Mutex::new(0i64));
    let completed_tasks = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();

    for _ in 0..worker_count {
        let rx = task_rx.clone();
        let s = sender.clone();
        let stop_flag = stop_requested.clone();
        let active_map = active_children.clone();
        let skipped_set = skipped_ids.clone();
        let proc_cnt = processed_counter.clone();
        let skip_cnt = skipped_counter.clone();
        let saved_agg = total_saved_bytes.clone();
        let comp_tasks = completed_tasks.clone();
        let enc_args = encoder_args.clone();
        let meta_key = cfg.meta_key.clone();
        let meta_val = cfg.meta_val.clone();

        let handle = thread::spawn(move || {
            while let Ok(task) = rx.recv_blocking() {
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }

                let id = task.id;
                let video = task.path;

                let was_skipped_early = {
                    let guard = skipped_set.lock().unwrap();
                    guard.contains(&id)
                };

                let orig_bytes = fs::metadata(&video).map(|m| m.len()).unwrap_or(0);
                let orig_mb = orig_bytes as f64 / (1024.0 * 1024.0);

                if was_skipped_early {
                    let sk = skip_cnt.fetch_add(1, Ordering::SeqCst) + 1;
                    let pr = proc_cnt.load(Ordering::SeqCst);
                    let done_cnt = comp_tasks.fetch_add(1, Ordering::SeqCst) + 1;
                    let saved_mb = *saved_agg.lock().unwrap() as f64 / (1024.0 * 1024.0);

                    let _ = s.send_blocking(WorkerMsg::ItemCompleted {
                        id,
                        status: "Skipped by User".into(),
                        size_display: format!("{orig_mb:.1} MB (Unchanged)"),
                        is_success: false,
                        is_error: false,
                    });
                    let _ = s.send_blocking(WorkerMsg::StatsUpdate {
                        processed: pr,
                        skipped: sk,
                        total_saved_mb: saved_mb,
                        overall_fraction: done_cnt as f64 / total_tasks as f64,
                    });
                    continue;
                }

                let file_name = video.file_name().unwrap_or_default().to_string_lossy().to_string();
                let _ = s.send_blocking(WorkerMsg::ItemStarted {
                    id,
                    name: file_name,
                });

                let duration = get_duration_fast(&video);
                if duration <= 0.0 {
                    let _ = s.send_blocking(WorkerMsg::ItemCompleted {
                        id,
                        status: "Invalid Duration".into(),
                        size_display: format!("{orig_mb:.1} MB"),
                        is_success: false,
                        is_error: true,
                    });
                    comp_tasks.fetch_add(1, Ordering::SeqCst);
                    continue;
                }

                let original_timestamps = fs::metadata(&video).ok().map(|m| {
                    (FileTime::from_last_access_time(&m), FileTime::from_last_modification_time(&m))
                });

                let parent = video.parent().unwrap_or_else(|| Path::new("."));
                let temp_path = parent.join(format!(".tmp_vault_{id}.mp4"));
                let meta_arg = format!("{meta_key}={meta_val}");

                let mut cmd = Command::new(ffmpeg_path());
                cmd.args(["-hide_banner", "-y", "-i"])
                    .arg(&video)
                    .args(&enc_args)
                    .args([
                        "-map", "0",
                        "-c:a", "aac",
                        "-b:a", "128k",
                        "-c:s", "copy",
                        "-movflags", "+faststart+use_metadata_tags",
                        "-metadata", &meta_arg,
                        "-progress", "pipe:2",
                        "-nostats",
                    ])
                    .arg(&temp_path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = s.send_blocking(WorkerMsg::ItemCompleted {
                            id,
                            status: format!("Error: {e}"),
                            size_display: format!("{orig_mb:.1} MB"),
                            is_success: false,
                            is_error: true,
                        });
                        comp_tasks.fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                };

                let stderr = child.stderr.take();
                if let Ok(mut map) = active_map.lock() {
                    map.insert(id, child);
                }

                if let Some(err_stream) = stderr {
                    let reader = BufReader::new(err_stream);
                    let mut last_ui_update = Instant::now();
                    let mut current_speed = 1.0f64;

                    for line in reader.lines().flatten() {
                        if stop_flag.load(Ordering::SeqCst) {
                            break;
                        }

                        if let Some(spd_str) = line.strip_prefix("speed=") {
                            let clean = spd_str.trim().trim_end_matches('x');
                            if let Ok(spd) = clean.parse::<f64>() {
                                if spd > 0.05 {
                                    current_speed = spd;
                                }
                            }
                        }

                        if let Some(us_str) = line.strip_prefix("out_time_us=") {
                            if let Ok(us) = us_str.trim().parse::<f64>() {
                                let sec = us / 1_000_000.0;
                                let frac = (sec / duration).clamp(0.0, 1.0);

                                if last_ui_update.elapsed() >= Duration::from_millis(150) {
                                    let remaining_secs = ((duration - sec).max(0.0) / current_speed).round() as u64;
                                    let _ = s.send_blocking(WorkerMsg::ItemProgress {
                                        id,
                                        fraction: frac,
                                        status: format!("{:.1}% • {:.1}x • {}s", frac * 100.0, current_speed, remaining_secs),
                                    });
                                    last_ui_update = Instant::now();
                                }
                            }
                        }
                    }
                }

                let exit_status = {
                    let mut map = active_map.lock().unwrap();
                    if let Some(mut child) = map.remove(&id) {
                        child.wait().ok()
                    } else {
                        None
                    }
                };

                if stop_flag.load(Ordering::SeqCst) {
                    let _ = fs::remove_file(&temp_path);
                    return;
                }

                let was_skipped_midway = {
                    let guard = skipped_set.lock().unwrap();
                    guard.contains(&id)
                };

                if was_skipped_midway {
                    let _ = fs::remove_file(&temp_path);
                    skip_cnt.fetch_add(1, Ordering::SeqCst);
                    let _ = s.send_blocking(WorkerMsg::ItemCompleted {
                        id,
                        status: "Skipped by User".into(),
                        size_display: format!("{orig_mb:.1} MB"),
                        is_success: false,
                        is_error: false,
                    });
                } else if let Some(status) = exit_status {
                    let is_file_healthy = status.success() && verify_container_integrity(&temp_path);

                    if is_file_healthy {
                        let new_bytes = fs::metadata(&temp_path).map(|m| m.len()).unwrap_or(0);
                        let new_mb = new_bytes as f64 / (1024.0 * 1024.0);

                        if new_mb < orig_mb {
                            let _ = fs::rename(&temp_path, &video);

                            if let Some((atime, mtime)) = original_timestamps {
                                let _ = set_file_times(&video, atime, mtime);
                            }

                            let saved_bytes = (orig_bytes as i64) - (new_bytes as i64);
                            if saved_bytes > 0 {
                                *saved_agg.lock().unwrap() += saved_bytes;
                            }
                            proc_cnt.fetch_add(1, Ordering::SeqCst);

                            let pct_reduction = ((orig_mb - new_mb) / orig_mb) * 100.0;
                            let _ = s.send_blocking(WorkerMsg::ItemCompleted {
                                id,
                                status: format!("Done (-{:.1} MB)", orig_mb - new_mb),
                                size_display: format!("{orig_mb:.1}M → {new_mb:.1}M (-{pct_reduction:.0}%)"),
                                is_success: true,
                                is_error: false,
                            });
                        } else {
                            let _ = fs::remove_file(&temp_path);
                            update_metadata_only(&video, &meta_key, &meta_val);
                            skip_cnt.fetch_add(1, Ordering::SeqCst);
                            let _ = s.send_blocking(WorkerMsg::ItemCompleted {
                                id,
                                status: format!("Skipped ({:.1}MB -> {:.1}MB)", orig_mb, new_mb),
                                size_display: format!("{orig_mb:.1}M → {new_mb:.1}M"),
                                is_success: false,
                                is_error: false,
                            });
                        }
                    } else {
                        let _ = fs::remove_file(&temp_path);
                        let _ = s.send_blocking(WorkerMsg::ItemCompleted {
                            id,
                            status: "Integrity Failed".into(),
                            size_display: format!("{orig_mb:.1} MB"),
                            is_success: false,
                            is_error: true,
                        });
                    }
                } else {
                    let _ = fs::remove_file(&temp_path);
                }

                let pr = proc_cnt.load(Ordering::SeqCst);
                let sk = skip_cnt.load(Ordering::SeqCst);
                let done_cnt = comp_tasks.fetch_add(1, Ordering::SeqCst) + 1;
                let saved_mb = *saved_agg.lock().unwrap() as f64 / (1024.0 * 1024.0);

                let _ = s.send_blocking(WorkerMsg::StatsUpdate {
                    processed: pr,
                    skipped: sk,
                    total_saved_mb: saved_mb,
                    overall_fraction: done_cnt as f64 / total_tasks as f64,
                });
            }
        });

        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.join();
    }

    if stop_requested.load(Ordering::SeqCst) {
        let _ = sender.send_blocking(WorkerMsg::Stopped);
    } else {
        let final_saved_mb = *total_saved_bytes.lock().unwrap() as f64 / (1024.0 * 1024.0);
        let _ = sender.send_blocking(WorkerMsg::OverallDone {
            total_saved_mb: final_saved_mb,
            processed: processed_counter.load(Ordering::SeqCst),
            skipped: skipped_counter.load(Ordering::SeqCst),
            elapsed_secs: start_time.elapsed().as_secs(),
        });
    }
}

// ---------------- Verification & Detection Utilities ----------------

fn verify_container_integrity(path: &Path) -> bool {
    Command::new(ffmpeg_path())
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn detect_best_available_encoder(quality: u32) -> (String, Vec<String>, bool) {
    let q_str = quality.to_string();

    // 1. Apple Silicon & Intel Mac VideoToolbox (HEVC / H.264)
    if test_ffmpeg_encoder("hevc_videotoolbox", &["-q:v", &q_str, "-tag:v", "hvc1"]) {
        return (
            "Apple VideoToolbox HEVC".to_string(),
            vec![
                "-c:v".into(), "hevc_videotoolbox".into(),
                "-q:v".into(), q_str,
                "-tag:v".into(), "hvc1".into(),
                "-realtime".into(), "0".into(),
            ],
            true,
        );
    }
    if test_ffmpeg_encoder("h264_videotoolbox", &["-q:v", &q_str]) {
        return (
            "Apple VideoToolbox H.264".to_string(),
            vec![
                "-c:v".into(), "h264_videotoolbox".into(),
                "-q:v".into(), q_str,
                "-realtime".into(), "0".into(),
            ],
            true,
        );
    }

    // 2. NVIDIA NVENC
    let nv_cq = ((100 - quality.clamp(1, 99)) * 51 / 100).to_string(); // map to 0-51 scale
    if test_ffmpeg_encoder("hevc_nvenc", &["-cq", &nv_cq, "-tag:v", "hvc1"]) {
        return (
            "NVIDIA NVENC HEVC".to_string(),
            vec![
                "-c:v".into(), "hevc_nvenc".into(),
                "-cq".into(), nv_cq,
                "-preset".into(), "p4".into(),
                "-tag:v".into(), "hvc1".into(),
            ],
            true,
        );
    }
    if test_ffmpeg_encoder("h264_nvenc", &["-cq", &nv_cq]) {
        return (
            "NVIDIA NVENC H.264".to_string(),
            vec![
                "-c:v".into(), "h264_nvenc".into(),
                "-cq".into(), nv_cq,
                "-preset".into(), "p4".into(),
            ],
            true,
        );
    }

    // 3. Intel QuickSync QSV
    if test_ffmpeg_encoder("hevc_qsv", &["-global_quality", &nv_cq, "-tag:v", "hvc1"]) {
        return (
            "Intel QuickSync HEVC".to_string(),
            vec![
                "-c:v".into(), "hevc_qsv".into(),
                "-global_quality".into(), nv_cq,
                "-tag:v".into(), "hvc1".into(),
            ],
            true,
        );
    }

    // 4. Universal Software CPU Fallback
    let crf = ((100 - quality.clamp(1, 99)) * 51 / 100).to_string();
    (
        "Software libx264 (Universal CPU)".to_string(),
        vec![
            "-c:v".into(), "libx264".into(),
            "-preset".into(), "veryfast".into(),
            "-crf".into(), crf,
        ],
        false,
    )
}

fn test_ffmpeg_encoder(codec_name: &str, extra_args: &[&str]) -> bool {
    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-hide_banner", "-f", "lavfi", "-i", "nullsrc=s=64x64:d=0.05", "-c:v", codec_name]);
    cmd.args(extra_args);
    cmd.args(["-f", "null", "-"]);
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

fn has_fast_metadata_tag(video: &Path, expected_val: &str) -> bool {
    let mut file = match File::open(video) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return false;
    }

    let needle = expected_val.as_bytes();
    let mut buffer = [0u8; 65536];

    let read_bytes = file.read(&mut buffer).unwrap_or(0);
    if buffer[..read_bytes].windows(needle.len()).any(|w| w == needle) {
        return true;
    }

    if len > 65536 {
        if file.seek(SeekFrom::End(-65536)).is_ok() {
            let read_bytes = file.read(&mut buffer).unwrap_or(0);
            if buffer[..read_bytes].windows(needle.len()).any(|w| w == needle) {
                return true;
            }
        }
    }

    false
}

fn get_duration_fast(video: &Path) -> f64 {
    let output = Command::new(ffmpeg_path())
        .args(["-hide_banner", "-i"])
        .arg(video)
        .output();

    if let Ok(out) = output {
        let s = String::from_utf8_lossy(&out.stderr);
        if let Some(pos) = s.find("Duration: ") {
            if let Some(time) = s[pos + 10..].split(',').next() {
                let p: Vec<&str> = time.trim().split(':').collect();
                if p.len() == 3 {
                    let h: f64 = p[0].parse().unwrap_or(0.0);
                    let m: f64 = p[1].parse().unwrap_or(0.0);
                    let sec: f64 = p[2].parse().unwrap_or(0.0);
                    return h * 3600.0 + m * 60.0 + sec;
                }
            }
        }
    }
    0.0
}

fn update_metadata_only(video: &Path, key: &str, val: &str) {
    let tmp = video.with_extension("meta.mp4");
    let arg = format!("{key}={val}");
    let ok = Command::new(ffmpeg_path())
        .args(["-hide_banner", "-y", "-i"])
        .arg(video)
        .args(["-c", "copy", "-movflags", "+faststart+use_metadata_tags", "-metadata", &arg])
        .arg(&tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if ok {
        let _ = fs::rename(&tmp, video);
    } else {
        let _ = fs::remove_file(&tmp);
    }
}