//! Spotify playback via librespot (Premium required), plus OAuth token handling
//! shared with the Web API client.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::SpotifyUri;
use librespot_oauth::OAuthClientBuilder;
use librespot_playback::audio_backend;
use librespot_playback::config::{AudioFormat, Bitrate, PlayerConfig};
use librespot_playback::mixer::{self, Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::SpotifyConfig;

pub const SCOPES: &[&str] = &[
    "streaming",
    "user-read-private",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    "user-library-read",
    "user-library-modify",
];

const LOGIN_DONE_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Medley</title>
<style>body{background:#121218;color:#eee;font-family:sans-serif;display:grid;place-items:center;height:100vh;margin:0}
h1{font-weight:600}p{color:#999}</style></head><body><div><h1>Logged in to Spotify ✓</h1>
<p>You can close this tab and go back to Medley.</p></div></body></html>"#;

#[derive(Serialize, Deserialize)]
struct StoredToken {
    client_id: String,
    refresh_token: String,
}

struct AuthState {
    access: Option<(String, Instant)>,
    refresh: Option<String>,
}

/// OAuth (PKCE) login and token refresh. The refresh token is stored in the data dir.
pub struct SpotifyAuth {
    client_id: String,
    redirect_uri: String,
    file: PathBuf,
    state: Mutex<AuthState>,
    /// Serializes refreshes so parallel requests don't refresh twice.
    refresh_lock: tokio::sync::Mutex<()>,
}

impl SpotifyAuth {
    pub fn new(cfg: &SpotifyConfig, dir: &Path) -> SpotifyAuth {
        let _ = std::fs::create_dir_all(dir);
        let file = dir.join("oauth.json");
        let refresh = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str::<StoredToken>(&s).ok())
            .filter(|t| t.client_id == cfg.client_id)
            .map(|t| t.refresh_token);
        SpotifyAuth {
            client_id: cfg.client_id.clone(),
            redirect_uri: format!("http://127.0.0.1:{}/login", cfg.redirect_port),
            file,
            state: Mutex::new(AuthState { access: None, refresh }),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn has_login(&self) -> bool {
        self.state.lock().unwrap().refresh.is_some()
    }

    fn builder(&self) -> Result<librespot_oauth::OAuthClient> {
        OAuthClientBuilder::new(&self.client_id, &self.redirect_uri, SCOPES.to_vec())
            .open_in_browser()
            .with_custom_message(LOGIN_DONE_PAGE)
            .build()
            .map_err(|e| anyhow!("OAuth setup failed: {e}"))
    }

    /// Opens the browser for the Spotify login page and waits for the redirect.
    pub async fn login(&self) -> Result<String> {
        let client = self.builder()?;
        let token = tokio::task::spawn_blocking(move || client.get_access_token())
            .await?
            .map_err(|e| anyhow!("Spotify login failed: {e}"))?;
        self.store(&token);
        Ok(token.access_token)
    }

    fn store(&self, token: &librespot_oauth::OAuthToken) {
        let mut st = self.state.lock().unwrap();
        st.access = Some((token.access_token.clone(), token.expires_at));
        if !token.refresh_token.is_empty() {
            st.refresh = Some(token.refresh_token.clone());
            let stored = StoredToken {
                client_id: self.client_id.clone(),
                refresh_token: token.refresh_token.clone(),
            };
            if let Ok(text) = serde_json::to_string(&stored) {
                let _ = std::fs::write(&self.file, text);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
    }

    /// A valid access token, refreshing it when it is about to expire.
    pub async fn token(&self) -> Result<String> {
        if let Some(t) = self.cached_access() {
            return Ok(t);
        }
        let _guard = self.refresh_lock.lock().await;
        if let Some(t) = self.cached_access() {
            return Ok(t);
        }
        let refresh = self
            .state
            .lock()
            .unwrap()
            .refresh
            .clone()
            .ok_or_else(|| anyhow!("not logged in to Spotify"))?;
        let client = self.builder()?;
        let token = client
            .refresh_token_async(&refresh)
            .await
            .map_err(|e| anyhow!("Spotify token refresh failed: {e}"))?;
        self.store(&token);
        Ok(token.access_token)
    }

    fn cached_access(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        st.access
            .as_ref()
            .filter(|(_, exp)| *exp > Instant::now() + Duration::from_secs(60))
            .map(|(t, _)| t.clone())
    }

    /// Forces the next `token()` call to refresh (e.g. after a 401).
    pub fn invalidate(&self) {
        self.state.lock().unwrap().access = None;
    }

    pub fn logout(&self) {
        let mut st = self.state.lock().unwrap();
        st.access = None;
        st.refresh = None;
        let _ = std::fs::remove_file(&self.file);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpotifyEvent {
    Loading,
    Playing { position_ms: u32 },
    Paused { position_ms: u32 },
    Seeked { position_ms: u32 },
    /// Good moment to preload the next track for gapless playback.
    TimeToPreload,
    EndOfTrack,
    Unavailable,
    Stopped,
}

pub struct SpotifyEngine {
    session: Session,
    player: Arc<Player>,
    mixer: Arc<dyn Mixer>,
    cache: Cache,
    pub username: String,
}

impl SpotifyEngine {
    pub async fn connect(
        auth: &SpotifyAuth,
        dir: &Path,
        audio_cache: Option<PathBuf>,
        cfg: &SpotifyConfig,
        volume: f32,
        events: mpsc::UnboundedSender<SpotifyEvent>,
    ) -> Result<SpotifyEngine> {
        let cache = Cache::new(Some(dir), Some(dir), audio_cache.as_deref(), Some(2 << 30))
            .context("creating Spotify cache")?;
        let session = new_session(auth, &cache).await?;
        let username = session.username();

        let mixer = mixer::find(None).ok_or_else(|| anyhow!("no mixer"))?(MixerConfig::default())
            .map_err(|e| anyhow!("mixer: {e}"))?;
        mixer.set_volume(volume_to_u16(volume));

        let backend = if cfg!(feature = "pulseaudio") {
            audio_backend::find(Some("pulseaudio".into())).or_else(|| audio_backend::find(None))
        } else {
            audio_backend::find(None)
        }
        .ok_or_else(|| anyhow!("no audio backend compiled in"))?;

        let player_config = PlayerConfig {
            bitrate: match cfg.bitrate {
                0..=96 => Bitrate::Bitrate96,
                97..=160 => Bitrate::Bitrate160,
                _ => Bitrate::Bitrate320,
            },
            normalisation: cfg.normalisation,
            gapless: true,
            ..PlayerConfig::default()
        };
        let player = Player::new(player_config, session.clone(), mixer.get_soft_volume(), move || {
            backend(None, AudioFormat::default())
        });

        let mut rx = player.get_player_event_channel();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let mapped = match ev {
                    PlayerEvent::Loading { .. } => SpotifyEvent::Loading,
                    PlayerEvent::Playing { position_ms, .. } => SpotifyEvent::Playing { position_ms },
                    PlayerEvent::Paused { position_ms, .. } => SpotifyEvent::Paused { position_ms },
                    PlayerEvent::Seeked { position_ms, .. } | PlayerEvent::PositionCorrection { position_ms, .. } => {
                        SpotifyEvent::Seeked { position_ms }
                    }
                    PlayerEvent::TimeToPreloadNextTrack { .. } => SpotifyEvent::TimeToPreload,
                    PlayerEvent::EndOfTrack { .. } => SpotifyEvent::EndOfTrack,
                    PlayerEvent::Unavailable { .. } => SpotifyEvent::Unavailable,
                    PlayerEvent::Stopped { .. } => SpotifyEvent::Stopped,
                    _ => continue,
                };
                if events.send(mapped).is_err() {
                    break;
                }
            }
        });

        Ok(SpotifyEngine {
            session,
            player,
            mixer,
            cache,
            username,
        })
    }

    /// Reconnects if the session dropped (network change, suspend, ...).
    pub async fn ensure_session(&mut self, auth: &SpotifyAuth) -> Result<()> {
        if self.session.is_invalid() {
            tracing::info!("Spotify session lost, reconnecting");
            self.session = new_session(auth, &self.cache).await?;
            self.player.set_session(self.session.clone());
        }
        Ok(())
    }

    pub fn load(&self, uri: &str, start_ms: u32) -> Result<()> {
        let uri = SpotifyUri::from_uri(uri).map_err(|e| anyhow!("bad Spotify URI {uri}: {e}"))?;
        self.player.load(uri, true, start_ms);
        Ok(())
    }

    pub fn preload(&self, uri: &str) {
        if let Ok(uri) = SpotifyUri::from_uri(uri) {
            self.player.preload(uri);
        }
    }

    pub fn play(&self) {
        self.player.play();
    }

    pub fn pause(&self) {
        self.player.pause();
    }

    pub fn stop(&self) {
        self.player.stop();
    }

    pub fn seek(&self, secs: f64) {
        self.player.seek((secs.max(0.0) * 1000.0) as u32);
    }

    pub fn set_volume(&self, volume: f32) {
        self.mixer.set_volume(volume_to_u16(volume));
    }

    pub fn shutdown(&self) {
        self.player.stop();
        self.session.shutdown();
    }
}

async fn new_session(auth: &SpotifyAuth, cache: &Cache) -> Result<Session> {
    // Reusable credentials from a previous login avoid an OAuth round trip.
    if let Some(creds) = cache.credentials() {
        let session = Session::new(SessionConfig::default(), Some(cache.clone()));
        match session.connect(creds, true).await {
            Ok(()) => return Ok(session),
            Err(e) => tracing::warn!("stored Spotify credentials rejected: {e}"),
        }
    }
    let token = auth.token().await?;
    let session = Session::new(SessionConfig::default(), Some(cache.clone()));
    session
        .connect(Credentials::with_access_token(token), true)
        .await
        .map_err(|e| anyhow!("Spotify connection failed: {e}"))?;
    Ok(session)
}

fn volume_to_u16(volume: f32) -> u16 {
    ((volume.clamp(0.0, 100.0) / 100.0) * u16::MAX as f32) as u16
}

/// Removes stored librespot credentials (used on logout).
pub fn clear_credentials(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("credentials.json"));
}
