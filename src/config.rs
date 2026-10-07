//! User configuration (`~/.config/multimusic/config.toml`) and XDG paths.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Spotify's own desktop client id. librespot uses it for streaming, and it is
/// allowed to request the library/playlist scopes too, so no developer app is needed.
pub const SPOTIFY_DEFAULT_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub library: LibraryConfig,
    pub playback: PlaybackConfig,
    pub spotify: SpotifyConfig,
    pub soundcloud: SoundCloudConfig,
    pub apple_music: AppleMusicConfig,
    pub lastfm: LastfmConfig,
    pub discord: DiscordConfig,
    pub lyrics: LyricsConfig,
    pub ui: UiConfig,
}

impl Default for Config {
    fn default() -> Self {
        let music_dir = directories::UserDirs::new()
            .and_then(|u| u.audio_dir().map(Path::to_path_buf))
            .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join("Music")));
        Config {
            library: LibraryConfig {
                folders: music_dir.into_iter().collect(),
                scan_on_startup: true,
            },
            playback: PlaybackConfig::default(),
            spotify: SpotifyConfig::default(),
            soundcloud: SoundCloudConfig::default(),
            apple_music: AppleMusicConfig::default(),
            lastfm: LastfmConfig::default(),
            discord: DiscordConfig::default(),
            lyrics: LyricsConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct LibraryConfig {
    pub folders: Vec<PathBuf>,
    pub scan_on_startup: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PlaybackConfig {
    /// 0..=100
    pub volume: f32,
    /// mpv binary used for local files and SoundCloud streams.
    pub mpv_path: String,
    /// Apply ReplayGain tags to local files.
    pub replaygain: bool,
    pub gapless: bool,
    /// Optional mpv `--audio-device`, e.g. `pipewire/alsa_output.usb-...`. Empty = default.
    pub audio_device: String,
    /// Bit-perfect output for lossless files: exclusive device access, no ReplayGain.
    pub bit_perfect: bool,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        PlaybackConfig {
            volume: 70.0,
            mpv_path: "mpv".into(),
            replaygain: true,
            gapless: true,
            audio_device: String::new(),
            bit_perfect: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SpotifyConfig {
    pub enabled: bool,
    /// 96, 160 or 320 kbps.
    pub bitrate: u16,
    pub normalisation: bool,
    /// OAuth client id used to log in (playback + library). Defaults to Spotify's desktop client id.
    pub client_id: String,
    /// Loopback port registered as redirect URI for the client id.
    pub redirect_port: u16,
    /// Optional client id of your own Spotify developer app, used for Web API calls
    /// (search, likes). The shared default client id is often rate limited.
    pub web_api_client_id: String,
    /// Redirect port registered for `web_api_client_id` (http://127.0.0.1:<port>/login).
    pub web_api_redirect_port: u16,
    /// Keep downloaded audio in ~/.cache/multimusic/spotify (uses disk, saves bandwidth).
    pub cache_audio: bool,
    /// Spotify audio output: "auto" (PipeWire/PulseAudio when available), "pulseaudio" or "alsa".
    pub audio_output: String,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        SpotifyConfig {
            enabled: true,
            bitrate: 320,
            normalisation: true,
            client_id: SPOTIFY_DEFAULT_CLIENT_ID.into(),
            redirect_port: 8898,
            web_api_client_id: String::new(),
            web_api_redirect_port: 8899,
            cache_audio: false,
            audio_output: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SoundCloudConfig {
    pub enabled: bool,
    /// Your profile, e.g. `https://soundcloud.com/yourname`. Used to import public likes and playlists.
    pub profile_url: String,
    /// Optional `oauth_token` cookie from soundcloud.com, needed for private playlists/likes.
    pub oauth_token: String,
    /// Optional API client id override (scraped from soundcloud.com automatically when empty).
    pub client_id: String,
}

impl Default for SoundCloudConfig {
    fn default() -> Self {
        SoundCloudConfig {
            enabled: true,
            profile_url: String::new(),
            oauth_token: String::new(),
            client_id: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct AppleMusicConfig {
    /// Developer token (JWT) from music.apple.com. Optional, only needed for API import.
    pub developer_token: String,
    /// `media-user-token` cookie from music.apple.com.
    pub user_token: String,
    /// Storefront country code, e.g. `us`.
    pub storefront: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct LastfmConfig {
    pub enabled: bool,
    pub api_key: String,
    pub api_secret: String,
    /// Filled in after authorizing.
    pub session_key: String,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DiscordConfig {
    pub enabled: bool,
    /// Application id from https://discord.com/developers/applications
    pub app_id: String,
    /// Show the song title as the activity name ("Listening to <song>").
    pub song_as_activity_name: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        DiscordConfig {
            enabled: true,
            app_id: String::new(),
            song_as_activity_name: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LyricsConfig {
    pub enabled: bool,
    /// Fetch from lrclib.net when no local lyrics exist.
    pub online: bool,
}

impl Default for LyricsConfig {
    fn default() -> Self {
        LyricsConfig {
            enabled: true,
            online: true,
        }
    }
}

/// The logo's off-white.
pub const DEFAULT_ACCENT: [u8; 3] = [0xe9, 0xe6, 0xdf];
/// The accent older versions used by default; replaced by the new default on load.
const OLD_DEFAULT_ACCENT: [u8; 3] = [0x8b, 0x7c, 0xf6];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// Accent colour (buttons, the playing song).
    pub accent: [u8; 3],
    /// Colour page backgrounds with the dominant colour of the current cover.
    pub dynamic_accent: bool,
    /// UI zoom factor.
    pub scale: f32,
    pub show_right_panel: bool,
    /// Show the sidebar as a narrow strip of icons and covers.
    pub collapse_library: bool,
    /// Width of the expanded sidebar (drag its edge to change it).
    pub sidebar_width: f32,
    /// Width of the lyrics / queue panel.
    pub right_panel_width: f32,
    /// Max decoded cover images kept in memory.
    pub art_cache_size: usize,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            accent: DEFAULT_ACCENT,
            dynamic_accent: true,
            scale: 1.0,
            show_right_panel: true,
            collapse_library: false,
            sidebar_width: 248.0,
            right_panel_width: 352.0,
            art_cache_size: 200,
        }
    }
}

/// XDG directories used by the app.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn new() -> Self {
        let dirs = directories::ProjectDirs::from("", "", "multimusic");
        let (config_dir, data_dir, cache_dir) = match dirs {
            Some(d) => (
                d.config_dir().to_path_buf(),
                d.data_dir().to_path_buf(),
                d.cache_dir().to_path_buf(),
            ),
            None => {
                let base = std::env::temp_dir().join("multimusic");
                (base.join("config"), base.join("data"), base.join("cache"))
            }
        };
        // The app used to be called Medley: carry its settings, library and logins over.
        if let Some(old) = directories::ProjectDirs::from("", "", "medley") {
            migrate_dir(old.config_dir(), &config_dir);
            migrate_dir(old.data_dir(), &data_dir);
            migrate_dir(old.cache_dir(), &cache_dir);
        }
        for d in [&config_dir, &data_dir, &cache_dir] {
            let _ = std::fs::create_dir_all(d);
        }
        Paths {
            config_dir,
            data_dir,
            cache_dir,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("library.db")
    }

    pub fn spotify_dir(&self) -> PathBuf {
        self.data_dir.join("spotify")
    }

    pub fn art_cache(&self) -> PathBuf {
        self.cache_dir.join("art")
    }

    pub fn lyrics_cache(&self) -> PathBuf {
        self.cache_dir.join("lyrics")
    }
}

/// Moves `old` to `new` if `new` doesn't exist yet.
fn migrate_dir(old: &Path, new: &Path) {
    if !old.is_dir() || new.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(old, new) {
        Ok(()) => tracing::info!("moved {} to {}", old.display(), new.display()),
        Err(e) => tracing::warn!("could not move {} to {}: {e}", old.display(), new.display()),
    }
}

impl Default for Paths {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Config {
        let file = paths.config_file();
        match std::fs::read_to_string(&file) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    cfg.upgrade();
                    cfg
                }
                Err(e) => {
                    tracing::error!("invalid config {}: {e}; using defaults", file.display());
                    Config::default()
                }
            },
            Err(_) => {
                let cfg = Config::default();
                let _ = cfg.save(paths);
                cfg
            }
        }
    }

    /// Adjusts settings saved by older versions.
    fn upgrade(&mut self) {
        if self.ui.accent == OLD_DEFAULT_ACCENT {
            self.ui.accent = DEFAULT_ACCENT;
        }
    }

    pub fn save(&self, paths: &Paths) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        let file = paths.config_file();
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        // The file holds API secrets, keep it private.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(tmp, file)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_directory_once() {
        let base = std::env::temp_dir().join(format!("multimusic-migrate-{}", std::process::id()));
        let old = base.join("medley");
        let new = base.join("multimusic");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("library.db"), b"x").unwrap();
        migrate_dir(&old, &new);
        assert!(new.join("library.db").exists());
        assert!(!old.exists());
        // An existing new directory is never overwritten.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("library.db"), b"old").unwrap();
        migrate_dir(&old, &new);
        assert_eq!(std::fs::read(new.join("library.db")).unwrap(), b"x");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn config_roundtrip() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn partial_config_uses_defaults() {
        let cfg: Config = toml::from_str("[playback]\nvolume = 30.0\n").unwrap();
        assert_eq!(cfg.playback.volume, 30.0);
        assert_eq!(cfg.playback.mpv_path, "mpv");
        assert_eq!(cfg.spotify.client_id, SPOTIFY_DEFAULT_CLIENT_ID);
    }

    #[test]
    fn old_default_accent_is_replaced() {
        let mut cfg: Config = toml::from_str("[ui]\naccent = [139, 124, 246]\n").unwrap();
        cfg.upgrade();
        assert_eq!(cfg.ui.accent, DEFAULT_ACCENT);
        // A colour the user picked is kept.
        let mut cfg: Config = toml::from_str("[ui]\naccent = [200, 10, 10]\n").unwrap();
        cfg.upgrade();
        assert_eq!(cfg.ui.accent, [200, 10, 10]);
    }
}
