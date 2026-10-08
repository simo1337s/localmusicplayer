//! MultiMusic: a lightweight native music player for local files, Spotify and SoundCloud.

mod config;
mod downloader;
mod http;
mod integrations;
mod library;
mod links;
mod memory;
mod model;
mod player;
mod providers;
mod service;
mod ui;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, Paths};
use crate::service::{Command, Shared};

fn main() -> anyhow::Result<()> {
    memory::tune();
    // How Spotify's audio stream shows up in pavucontrol / the PipeWire graph. Set before
    // any other thread exists, as required for set_var.
    std::env::set_var("PULSE_PROP_application.name", "MultiMusic");
    std::env::set_var("PULSE_PROP_application.icon_name", "multimusic");
    std::env::set_var("PULSE_PROP_stream.description", "Spotify");
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MULTIMUSIC_LOG")
                .unwrap_or_else(|_| EnvFilter::new("multimusic=info,librespot=warn,warn")),
        )
        .init();

    let paths = Paths::new();
    let cfg = Config::load(&paths);

    // Two workers are plenty: everything heavy is IO bound or runs in mpv/librespot threads.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_name("multimusic-rt")
        .enable_all()
        .build()?;

    let shared = Arc::new(Shared::default());
    let (cmd, service) = service::start(rt.handle().clone(), shared.clone(), paths.clone(), cfg.clone());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MultiMusic")
            .with_app_id("multimusic")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([880.0, 560.0])
            .with_icon(Arc::new(app_icon())),
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        ..Default::default()
    };
    let handle = rt.handle().clone();
    let ui_cmd = cmd.clone();
    let result = eframe::run_native(
        "MultiMusic",
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, shared, ui_cmd, cfg, paths, handle)))),
    );

    // Let the service stop mpv, clear Discord and save the session.
    let _ = cmd.send(Command::Quit);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !service.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    rt.shutdown_timeout(Duration::from_millis(500));
    result.map_err(|e| anyhow!("{e}"))
}

/// Window/taskbar icon, from the same artwork as the launcher icon and the in-app logo.
fn app_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon-256.png"))
        .expect("bundled icon is a valid PNG")
        .to_rgba8();
    egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    }
}
