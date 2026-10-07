//! Medley: a lightweight native music player for local files, Spotify and SoundCloud.

mod config;
mod http;
mod integrations;
mod library;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MEDLEY_LOG").unwrap_or_else(|_| EnvFilter::new("medley=info,librespot=warn,warn")),
        )
        .init();

    let paths = Paths::new();
    let cfg = Config::load(&paths);

    // Two workers are plenty: everything heavy is IO bound or runs in mpv/librespot threads.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_name("medley-rt")
        .enable_all()
        .build()?;

    let shared = Arc::new(Shared::default());
    let (cmd, service) = service::start(rt.handle().clone(), shared.clone(), paths.clone(), cfg.clone());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Medley")
            .with_app_id("medley")
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
        "Medley",
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

/// Window icon: a rounded violet square with sound bars, drawn at startup (no image assets).
fn app_icon() -> egui::IconData {
    let size = 64usize;
    let mut rgba = vec![0u8; size * size * 4];
    let bars = [0.35f32, 0.7, 1.0, 0.55, 0.8];
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded-square mask.
            let r = 14.0;
            let cx = fx.clamp(r, size as f32 - r);
            let cy = fy.clamp(r, size as f32 - r);
            let inside = (fx - cx).powi(2) + (fy - cy).powi(2) <= r * r;
            if !inside {
                continue;
            }
            let t = fy / size as f32;
            let (mut cr, mut cg, mut cb) = (
                (0x8b as f32 * (1.0 - t) + 0x4f as f32 * t) as u8,
                (0x7c as f32 * (1.0 - t) + 0x3a as f32 * t) as u8,
                (0xf6 as f32 * (1.0 - t) + 0xc8 as f32 * t) as u8,
            );
            // Sound bars.
            let bar_w = 6.0;
            let gap = 4.0;
            let total = bars.len() as f32 * bar_w + (bars.len() as f32 - 1.0) * gap;
            let start = (size as f32 - total) / 2.0;
            for (i, h) in bars.iter().enumerate() {
                let bx = start + i as f32 * (bar_w + gap);
                let bh = 36.0 * h;
                let top = (size as f32 - bh) / 2.0;
                if fx >= bx && fx <= bx + bar_w && fy >= top && fy <= top + bh {
                    (cr, cg, cb) = (255, 255, 255);
                }
            }
            let i = (y * size + x) * 4;
            rgba[i..i + 4].copy_from_slice(&[cr, cg, cb, 255]);
        }
    }
    egui::IconData {
        rgba,
        width: size as u32,
        height: size as u32,
    }
}
