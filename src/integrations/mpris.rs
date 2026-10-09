//! System media controls: MPRIS on Linux (media keys, playerctl, waybar/polybar modules,
//! desktop widgets), the media overlay and keys on Windows, Now Playing on macOS.

use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::model::Track;
use crate::service::Command;

pub struct Mpris {
    controls: Option<MediaControls>,
    last_track: Option<String>,
}

impl Mpris {
    /// `window` is the main window's handle, which Windows needs (it has none before the
    /// window opens).
    pub fn new(commands: UnboundedSender<Command>, window: Option<isize>) -> Mpris {
        let config = PlatformConfig {
            display_name: "Sumo",
            dbus_name: "multimusic",
            hwnd: window.map(|h| h as *mut std::ffi::c_void),
        };
        let available = if cfg!(target_os = "linux") {
            session_bus_available()
        } else if cfg!(windows) {
            window.is_some()
        } else {
            true
        };
        let controls = available
            .then(|| MediaControls::new(config).ok())
            .flatten()
            .and_then(|mut c| {
                let result = c.attach(move |event: MediaControlEvent| {
                    let cmd = match event {
                        MediaControlEvent::Play => Command::Resume,
                        MediaControlEvent::Pause => Command::Pause,
                        MediaControlEvent::Toggle => Command::TogglePause,
                        MediaControlEvent::Next => Command::Next,
                        MediaControlEvent::Previous => Command::Previous,
                        MediaControlEvent::Stop => Command::Pause,
                        MediaControlEvent::Seek(dir) => Command::SeekRelative(match dir {
                            SeekDirection::Forward => 10.0,
                            SeekDirection::Backward => -10.0,
                        }),
                        MediaControlEvent::SeekBy(dir, by) => Command::SeekRelative(match dir {
                            SeekDirection::Forward => by.as_secs_f64(),
                            SeekDirection::Backward => -by.as_secs_f64(),
                        }),
                        MediaControlEvent::SetPosition(MediaPosition(pos)) => Command::Seek(pos.as_secs_f64()),
                        MediaControlEvent::SetVolume(v) => Command::SetVolume((v * 100.0) as f32),
                        MediaControlEvent::Raise => Command::Raise,
                        MediaControlEvent::Quit => Command::Quit,
                        MediaControlEvent::OpenUri(_) => return,
                    };
                    let _ = commands.send(cmd);
                });
                match result {
                    Ok(()) => Some(c),
                    Err(e) => {
                        tracing::warn!("MPRIS unavailable: {e:?}");
                        None
                    }
                }
            });
        Mpris {
            controls,
            last_track: None,
        }
    }

    pub fn update(&mut self, track: Option<&Track>, playing: bool, position: f64) {
        let Some(c) = self.controls.as_mut() else { return };
        let id = track.map(|t| t.id.clone());
        if id != self.last_track {
            self.last_track = id;
            let cover = track.and_then(|t| t.art.as_deref()).and_then(|a| {
                if a.starts_with("http") {
                    Some(a.to_string())
                } else if is_image(a) {
                    Some(file_url(a))
                } else {
                    None
                }
            });
            let meta = match track {
                Some(t) => MediaMetadata {
                    title: Some(&t.title),
                    album: Some(&t.album),
                    artist: Some(&t.artist),
                    cover_url: cover.as_deref(),
                    duration: (t.duration_ms > 0).then(|| Duration::from_millis(t.duration_ms)),
                },
                None => MediaMetadata::default(),
            };
            let _ = c.set_metadata(meta);
        }
        let progress = Some(MediaPosition(Duration::from_secs_f64(position.max(0.0))));
        let playback = match (track, playing) {
            (None, _) => MediaPlayback::Stopped,
            (Some(_), true) => MediaPlayback::Playing { progress },
            (Some(_), false) => MediaPlayback::Paused { progress },
        };
        let _ = c.set_playback(playback);
    }

    pub fn set_volume(&mut self, volume: f32) {
        // Only MPRIS has a volume.
        #[cfg(target_os = "linux")]
        if let Some(c) = self.controls.as_mut() {
            let _ = c.set_volume(volume as f64 / 100.0);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = volume;
    }
}

/// souvlaki panics on its D-Bus thread when there is no session bus, so check first.
fn session_bus_available() -> bool {
    if let Ok(addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        // unix:path=/run/user/1000/bus[,guid=...]
        return match addr.strip_prefix("unix:path=") {
            Some(rest) => std::path::Path::new(rest.split(',').next().unwrap_or(rest)).exists(),
            None => !addr.is_empty(),
        };
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| std::path::Path::new(&d).join("bus").exists())
        .unwrap_or(false)
}

/// A local cover as a `file://` URL (Windows paths need a slash before the drive).
fn file_url(path: &str) -> String {
    let path = path.replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

fn is_image(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp"].iter().any(|e| lower.ends_with(e))
}
