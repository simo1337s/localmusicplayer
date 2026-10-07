//! The background service: owns playback, the database and every network integration.
//! The UI talks to it with [`Command`]s and reads state from [`Shared`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::{Config, Paths};
use crate::integrations::discord::{Discord, Presence};
use crate::integrations::lastfm::{Lastfm, ScrobbleTracker};
use crate::integrations::lyrics::LyricsFetcher;
use crate::integrations::mpris::Mpris;
use crate::library::{self, liked_playlist, Db, Library, LIKED_ID};
use crate::model::{
    normalize_artist, normalize_title, now_unix, AudioQuality, ImportedPlaylist, Lyrics, Playlist, PlaylistKind,
    RepeatMode, Source, Track,
};
use crate::player::mpv::{Mpv, MpvEvent, MpvOptions};
use crate::player::queue::Queue;
use crate::player::spotify::{self, SpotifyAuth, SpotifyEngine, SpotifyEvent};
use crate::providers::apple_music::{self, AppleMusicApi};
use crate::providers::soundcloud::SoundCloud;
use crate::providers::spotify_api::{self, SpotifyApi, SpotifyPlaylistMeta};

/// Requests from the UI (and MPRIS).
#[derive(Debug, Clone)]
pub enum Command {
    Play {
        tracks: Vec<Track>,
        start: usize,
        context: String,
    },
    TogglePause,
    Pause,
    Resume,
    Next,
    Previous,
    Seek(f64),
    SeekRelative(f64),
    SetVolume(f32),
    SetShuffle(bool),
    CycleRepeat,
    Enqueue(Vec<Track>),
    PlayNext(Vec<Track>),
    JumpTo(usize),
    RemoveUpcoming(usize),
    ClearUpcoming,
    ToggleLike(Track),
    CreatePlaylist {
        name: String,
        tracks: Vec<Track>,
    },
    AddToPlaylist {
        playlist_id: String,
        tracks: Vec<Track>,
    },
    RemoveFromPlaylist {
        playlist_id: String,
        index: usize,
    },
    RenamePlaylist {
        playlist_id: String,
        name: String,
    },
    DeletePlaylist(String),
    ImportM3u(PathBuf),
    ExportM3u {
        playlist_id: String,
        path: PathBuf,
    },
    ImportAppleXml(PathBuf),
    ImportAppleApi,
    Rescan,
    /// Ask mpv which output devices exist (for the device picker).
    ListAudioDevices,
    SpotifyLogin,
    /// Authorize the user's own developer app for Web API calls.
    SpotifyWebApiLogin,
    SpotifyLogout,
    SyncSpotify,
    SyncSoundCloud,
    LastfmLogin,
    LastfmLogout,
    Search(String),
    UpdateConfig(Box<Config>),
    Raise,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayStatus {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

/// Snapshot of playback for the UI.
#[derive(Debug, Clone)]
pub struct PlayerView {
    pub current: Option<Track>,
    /// The track actually playing when `current` had to be resolved (Apple Music imports).
    pub via: Option<Track>,
    pub status: PlayStatus,
    pub position: f64,
    pub position_at: Instant,
    pub duration: f64,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub upcoming: Vec<Track>,
    pub up_next_len: usize,
    pub context: String,
    /// Format of what's playing (codec, bit depth, sample rate).
    pub quality: Option<AudioQuality>,
}

impl Default for PlayerView {
    fn default() -> Self {
        PlayerView {
            current: None,
            via: None,
            status: PlayStatus::Stopped,
            position: 0.0,
            position_at: Instant::now(),
            duration: 0.0,
            volume: 70.0,
            shuffle: false,
            repeat: RepeatMode::Off,
            upcoming: Vec::new(),
            up_next_len: 0,
            context: String::new(),
            quality: None,
        }
    }
}

impl PlayerView {
    /// Position extrapolated to now.
    pub fn position_now(&self) -> f64 {
        let p = if self.status == PlayStatus::Playing {
            self.position + self.position_at.elapsed().as_secs_f64()
        } else {
            self.position
        };
        if self.duration > 0.0 {
            p.min(self.duration)
        } else {
            p
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum AccountStatus {
    #[default]
    Off,
    Working(String),
    Connected(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub at: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    pub spotify: Vec<Track>,
    pub soundcloud: Vec<Track>,
    pub pending: u8,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LyricsState {
    pub track_id: String,
    pub loading: bool,
    pub lyrics: Option<Lyrics>,
}

/// Everything else the UI shows that isn't library or playback.
#[derive(Debug, Clone, Default)]
pub struct Feed {
    pub toasts: Vec<Toast>,
    pub search: SearchState,
    pub lyrics: LyricsState,
    pub spotify: AccountStatus,
    /// True while a Spotify login is stored, whatever the current status message says.
    pub spotify_logged_in: bool,
    /// Status of the optional own-app Web API login.
    pub spotify_web_api: AccountStatus,
    pub soundcloud: AccountStatus,
    pub lastfm: AccountStatus,
    pub scan: Option<(usize, usize)>,
    /// mpv output devices as (id, description), filled on request.
    pub audio_devices: Vec<(String, String)>,
    pub raise: bool,
    pub quit: bool,
}

/// State shared between the service and the UI.
#[derive(Default)]
pub struct Shared {
    pub library: RwLock<Library>,
    pub player: RwLock<PlayerView>,
    pub feed: RwLock<Feed>,
    pub ctx: OnceLock<egui::Context>,
}

impl Shared {
    pub fn repaint(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    fn toast(&self, kind: ToastKind, text: impl Into<String>) {
        let text = text.into();
        match kind {
            ToastKind::Error => tracing::warn!("{text}"),
            ToastKind::Info => tracing::info!("{text}"),
        }
        let mut feed = self.feed.write().unwrap();
        feed.toasts.push(Toast {
            text,
            kind,
            at: Instant::now(),
        });
        if feed.toasts.len() > 5 {
            feed.toasts.remove(0);
        }
        drop(feed);
        self.repaint();
    }

    fn info(&self, text: impl Into<String>) {
        self.toast(ToastKind::Info, text);
    }

    fn error(&self, text: impl Into<String>) {
        self.toast(ToastKind::Error, text);
    }
}

/// Results of background jobs, sent back into the service loop.
enum Internal {
    StreamReady {
        seq: u64,
        track: Track,
        url: String,
    },
    Resolved {
        seq: u64,
        original: Track,
        resolved: Option<Track>,
    },
    LoadFailed {
        seq: u64,
        error: String,
    },
    SpotifyReady(Result<SpotifyEngine>),
    SpotifySynced(Result<SpotifySync>),
    SoundCloudSynced(Result<SoundCloudSync>),
    AppleImported(Result<(Vec<Track>, Vec<ImportedPlaylist>)>),
    M3uImported(Result<ImportedPlaylist>),
    ScanDone(library::scanner::ScanResult),
    Lyrics {
        track_id: String,
        lyrics: Option<Lyrics>,
    },
    Search {
        query: String,
        spotify: Result<Vec<Track>>,
        soundcloud: Result<Vec<Track>>,
    },
    LastfmSession(Result<(String, String)>),
    Quality {
        track_id: String,
        quality: Option<AudioQuality>,
    },
    AudioDevices(Vec<(String, String)>),
    /// Status text while the Spotify library syncs.
    SyncProgress(String),
}

struct SpotifySync {
    user: String,
    /// Playlists in Spotify's order with their track ids. `None` = unchanged since last sync.
    playlists: Vec<(SpotifyPlaylistMeta, Option<Vec<String>>)>,
    /// Liked Songs track ids, newest first.
    liked: Vec<String>,
    /// New or updated track metadata.
    tracks: Vec<Track>,
}

struct SoundCloudSync {
    user: String,
    likes: Vec<Track>,
    playlists: Vec<ImportedPlaylist>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    None,
    Mpv,
    Spotify,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedSession {
    tracks: Vec<String>,
    index: usize,
    position: f64,
    context: String,
    shuffle: bool,
    repeat: RepeatMode,
}

pub struct Service {
    shared: Arc<Shared>,
    paths: Paths,
    cfg: Config,
    db: Db,
    http: reqwest::Client,
    cmd_tx: UnboundedSender<Command>,
    internal_tx: UnboundedSender<Internal>,

    queue: Queue,
    engine: Engine,
    status: PlayStatus,
    position: f64,
    position_at: Instant,
    duration: f64,
    /// Playable track for the current queue entry (differs for resolved imports).
    playing: Option<Track>,
    quality: Option<AudioQuality>,
    load_seq: u64,
    started_reported: bool,
    resume_position: Option<f64>,
    consecutive_failures: u32,

    mpv: Option<Mpv>,
    mpv_tx: UnboundedSender<MpvEvent>,
    mpv_preloaded: Option<String>,

    spotify_auth: Arc<SpotifyAuth>,
    /// Optional login with the user's own developer app, for Web API calls.
    spotify_web_auth: Option<Arc<SpotifyAuth>>,
    spotify: Option<SpotifyEngine>,
    spotify_connecting: bool,
    /// A sync was requested before the session was connected.
    spotify_sync_pending: bool,
    spotify_syncing: bool,
    spotify_tx: UnboundedSender<SpotifyEvent>,
    spotify_api: Arc<SpotifyApi>,

    soundcloud: Arc<SoundCloud>,
    lyrics: Arc<LyricsFetcher>,
    lastfm: Option<Arc<Lastfm>>,
    scrobble: ScrobbleTracker,
    discord: Discord,
    mpris: Mpris,
    last_tick: Instant,
}

/// Starts the service on its own thread, driving futures on `rt`. Returns the command sender.
pub fn start(
    rt: tokio::runtime::Handle,
    shared: Arc<Shared>,
    paths: Paths,
    cfg: Config,
) -> (UnboundedSender<Command>, std::thread::JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let tx = cmd_tx.clone();
    let handle = std::thread::Builder::new()
        .name("multimusic-service".into())
        .spawn(move || {
            rt.block_on(async move {
                match Service::new(shared.clone(), paths, cfg, tx) {
                    Ok((svc, mpv_rx, sp_rx, int_rx)) => svc.run(cmd_rx, mpv_rx, sp_rx, int_rx).await,
                    Err(e) => {
                        shared.error(format!("Failed to start: {e:#}"));
                    }
                }
            });
        })
        .expect("spawn service thread");
    (cmd_tx, handle)
}

type Receivers = (
    UnboundedReceiver<MpvEvent>,
    UnboundedReceiver<SpotifyEvent>,
    UnboundedReceiver<Internal>,
);
type Started = (
    Service,
    UnboundedReceiver<MpvEvent>,
    UnboundedReceiver<SpotifyEvent>,
    UnboundedReceiver<Internal>,
);

impl Service {
    fn new(shared: Arc<Shared>, paths: Paths, cfg: Config, cmd_tx: UnboundedSender<Command>) -> Result<Started> {
        let db = Db::open(&paths.database())?;
        let lib = Library::load(&db)?;
        *shared.library.write().unwrap() = lib;

        let http = crate::http::client();
        let (mpv_tx, mpv_rx) = mpsc::unbounded_channel();
        let (spotify_tx, sp_rx) = mpsc::unbounded_channel();
        let (internal_tx, int_rx): (UnboundedSender<Internal>, UnboundedReceiver<Internal>) = mpsc::unbounded_channel();
        let receivers: Receivers = (mpv_rx, sp_rx, int_rx);

        let spotify_auth = Arc::new(SpotifyAuth::new(&cfg.spotify, &paths.spotify_dir()));
        let spotify_web_auth = SpotifyAuth::web_api(&cfg.spotify, &paths.spotify_dir()).map(Arc::new);
        let soundcloud = Arc::new(SoundCloud::new(
            http.clone(),
            &cfg.soundcloud.client_id,
            &cfg.soundcloud.oauth_token,
        ));
        let lyrics = Arc::new(LyricsFetcher::new(
            http.clone(),
            paths.lyrics_cache(),
            cfg.lyrics.online,
        ));
        let lastfm = make_lastfm(&cfg, &http, &paths);
        let discord = Discord::spawn(
            cfg.discord.enabled,
            &cfg.discord.app_id,
            cfg.discord.song_as_activity_name,
        );
        let mpris = Mpris::new(cmd_tx.clone());

        let svc = Service {
            shared,
            paths,
            db,
            http: http.clone(),
            cmd_tx,
            internal_tx,
            queue: Queue::default(),
            engine: Engine::None,
            status: PlayStatus::Stopped,
            position: 0.0,
            position_at: Instant::now(),
            duration: 0.0,
            playing: None,
            quality: None,
            load_seq: 0,
            started_reported: false,
            resume_position: None,
            consecutive_failures: 0,
            mpv: None,
            mpv_tx,
            mpv_preloaded: None,
            spotify_auth,
            spotify_web_auth,
            spotify: None,
            spotify_connecting: false,
            spotify_sync_pending: false,
            spotify_syncing: false,
            spotify_tx,
            spotify_api: Arc::new(SpotifyApi::new(http)),
            soundcloud,
            lyrics,
            lastfm,
            scrobble: ScrobbleTracker::new(),
            discord,
            mpris,
            last_tick: Instant::now(),
            cfg,
        };
        let (a, b, c) = receivers;
        Ok((svc, a, b, c))
    }

    async fn run(
        mut self,
        mut cmd_rx: UnboundedReceiver<Command>,
        mut mpv_rx: UnboundedReceiver<MpvEvent>,
        mut sp_rx: UnboundedReceiver<SpotifyEvent>,
        mut int_rx: UnboundedReceiver<Internal>,
    ) {
        self.startup();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    if matches!(cmd, Command::Quit) {
                        break;
                    }
                    self.handle(cmd).await;
                }
                Some(ev) = mpv_rx.recv() => self.on_mpv(ev).await,
                Some(ev) = sp_rx.recv() => self.on_spotify(ev).await,
                Some(msg) = int_rx.recv() => self.on_internal(msg).await,
                _ = tick.tick() => self.on_tick().await,
            }
        }
        self.shutdown().await;
    }

    fn startup(&mut self) {
        self.publish_accounts();
        self.restore_session();
        if self.cfg.library.scan_on_startup && !self.cfg.library.folders.is_empty() {
            self.start_scan();
        }
        if self.cfg.spotify.enabled && self.spotify_auth.has_login() {
            self.connect_spotify();
            self.start_spotify_sync();
        }
        if self.cfg.soundcloud.enabled && self.soundcloud_configured() {
            let last: i64 = self
                .db
                .get_kv("soundcloud_synced_at")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if now_unix() - last > 6 * 3600 {
                self.start_soundcloud_sync();
            } else {
                self.set_account(
                    |f| &mut f.soundcloud,
                    AccountStatus::Connected(self.db.get_kv("soundcloud_user").unwrap_or_default()),
                );
            }
        }
        if let Some(lfm) = self.lastfm.clone() {
            tokio::spawn(async move {
                let _ = lfm.flush_queue().await;
            });
        }
        self.publish_player();
        self.publish_queue();
    }

    async fn shutdown(&mut self) {
        self.save_session();
        self.discord.shutdown();
        if let Some(sp) = self.spotify.take() {
            sp.shutdown();
        }
        if let Some(mpv) = self.mpv.take() {
            mpv.quit().await;
        }
        let mut feed = self.shared.feed.write().unwrap();
        feed.quit = true;
        drop(feed);
        self.shared.repaint();
    }

    // ---------------------------------------------------------------- commands

    async fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Play { tracks, start, context } => {
                self.resume_position = None;
                if self.queue.set_context(tracks, start, context).is_some() {
                    self.consecutive_failures = 0;
                    self.play_current(0.0).await;
                }
                self.publish_queue();
            }
            Command::TogglePause => {
                if self.status == PlayStatus::Playing {
                    self.pause().await;
                } else {
                    self.resume().await;
                }
            }
            Command::Pause => self.pause().await,
            Command::Resume => self.resume().await,
            Command::Next => {
                self.consecutive_failures = 0;
                if self.queue.advance(true).is_some() {
                    self.play_current(0.0).await;
                } else {
                    self.stop().await;
                }
                self.publish_queue();
            }
            Command::Previous => {
                if self.current_position() > 3.0 {
                    self.seek(0.0).await;
                } else if self.queue.previous().is_some() {
                    self.play_current(0.0).await;
                    self.publish_queue();
                }
            }
            Command::Seek(secs) => self.seek(secs).await,
            Command::SeekRelative(d) => {
                let p = self.current_position() + d;
                self.seek(p).await;
            }
            Command::SetVolume(v) => {
                let v = v.clamp(0.0, 100.0);
                self.cfg.playback.volume = v;
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.set_volume(v).await;
                }
                if let Some(sp) = &self.spotify {
                    sp.set_volume(v);
                }
                self.mpris.set_volume(v);
                self.publish_player();
            }
            Command::SetShuffle(on) => {
                self.queue.set_shuffle(on);
                self.after_queue_change().await;
            }
            Command::CycleRepeat => {
                self.queue.repeat = self.queue.repeat.cycle();
                self.after_queue_change().await;
            }
            Command::Enqueue(tracks) => {
                let n = tracks.len();
                if self.queue.is_empty() {
                    self.queue.set_context(tracks, 0, "Queue".into());
                    self.play_current(0.0).await;
                } else {
                    self.queue.enqueue(tracks);
                }
                self.shared
                    .info(format!("Added {n} track{} to the queue", if n == 1 { "" } else { "s" }));
                self.after_queue_change().await;
            }
            Command::PlayNext(tracks) => {
                if self.queue.is_empty() {
                    self.queue.set_context(tracks, 0, "Queue".into());
                    self.play_current(0.0).await;
                } else {
                    self.queue.play_next(tracks);
                }
                self.after_queue_change().await;
            }
            Command::JumpTo(i) => {
                if self.queue.jump_to(i).is_some() {
                    self.play_current(0.0).await;
                }
                self.publish_queue();
            }
            Command::RemoveUpcoming(i) => {
                self.queue.remove(i);
                self.after_queue_change().await;
            }
            Command::ClearUpcoming => {
                self.queue.clear_upcoming();
                self.after_queue_change().await;
            }
            Command::ToggleLike(track) => self.toggle_like(track),
            Command::CreatePlaylist { name, tracks } => {
                let id = format!("custom:{:x}{:04x}", now_unix(), rand::random::<u16>());
                let p = Playlist {
                    id,
                    name: name.clone(),
                    kind: PlaylistKind::Custom,
                    remote_id: None,
                    description: String::new(),
                    art: tracks.iter().find_map(|t| t.art.clone()),
                    track_ids: tracks.iter().map(|t| t.id.clone()).collect(),
                };
                self.store_tracks(&tracks);
                self.save_playlists(vec![p], &[]);
                self.shared.info(format!("Created playlist “{name}”"));
            }
            Command::AddToPlaylist { playlist_id, tracks } => {
                self.store_tracks(&tracks);
                let updated = {
                    let mut lib = self.shared.library.write().unwrap();
                    lib.playlist_mut(&playlist_id).map(|p| {
                        p.track_ids.extend(tracks.iter().map(|t| t.id.clone()));
                        if p.art.is_none() {
                            p.art = tracks.iter().find_map(|t| t.art.clone());
                        }
                        p.clone()
                    })
                };
                if let Some(p) = updated {
                    self.shared.info(format!("Added {} to “{}”", tracks.len(), p.name));
                    self.save_playlists(vec![p], &[]);
                }
            }
            Command::RemoveFromPlaylist { playlist_id, index } => {
                let updated = {
                    let mut lib = self.shared.library.write().unwrap();
                    lib.playlist_mut(&playlist_id).and_then(|p| {
                        (index < p.track_ids.len()).then(|| {
                            p.track_ids.remove(index);
                            p.clone()
                        })
                    })
                };
                if let Some(p) = updated {
                    self.save_playlists(vec![p], &[]);
                }
            }
            Command::RenamePlaylist { playlist_id, name } => {
                let updated = {
                    let mut lib = self.shared.library.write().unwrap();
                    lib.playlist_mut(&playlist_id).map(|p| {
                        p.name = name;
                        p.clone()
                    })
                };
                if let Some(p) = updated {
                    self.save_playlists(vec![p], &[]);
                }
            }
            Command::DeletePlaylist(id) => {
                if id != LIKED_ID {
                    self.save_playlists(vec![], &[id]);
                }
            }
            Command::ImportM3u(path) => {
                let shared = self.shared.clone();
                let tx = self.internal_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = library::m3u::import(&path, &|id| shared.library.read().unwrap().get(id).cloned());
                    let _ = tx.send(Internal::M3uImported(result));
                });
            }
            Command::ExportM3u { playlist_id, path } => {
                let tracks = {
                    let lib = self.shared.library.read().unwrap();
                    lib.playlist(&playlist_id)
                        .map(|p| lib.tracks_for(&p.track_ids))
                        .unwrap_or_default()
                };
                match library::m3u::export(&path, &tracks) {
                    Ok(n) => self
                        .shared
                        .info(format!("Exported {n} local tracks to {}", path.display())),
                    Err(e) => self.shared.error(format!("Export failed: {e:#}")),
                }
            }
            Command::ImportAppleXml(path) => {
                self.shared.info("Importing Apple Music library…");
                let tx = self.internal_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let r = apple_music::import_library_xml(&path).map(|l| (l.tracks, l.playlists));
                    let _ = tx.send(Internal::AppleImported(r));
                });
            }
            Command::ImportAppleApi => self.import_apple_api(),
            Command::Rescan => self.start_scan(),
            Command::ListAudioDevices => {
                let binary = self.cfg.playback.mpv_path.clone();
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let devices = crate::player::mpv::list_audio_devices(&binary).await;
                    let _ = tx.send(Internal::AudioDevices(devices));
                });
            }
            Command::SpotifyLogin => self.spotify_login(),
            Command::SpotifyLogout => {
                if let Some(sp) = self.spotify.take() {
                    sp.shutdown();
                }
                if self.engine == Engine::Spotify {
                    self.engine = Engine::None;
                    self.set_status(PlayStatus::Stopped);
                }
                self.spotify_auth.logout();
                if let Some(a) = &self.spotify_web_auth {
                    a.logout();
                }
                spotify::clear_credentials(&self.paths.spotify_dir());
                self.set_account(|f| &mut f.spotify, AccountStatus::Off);
            }
            Command::SyncSpotify => self.start_spotify_sync(),
            Command::SpotifyWebApiLogin => self.spotify_web_api_login(),
            Command::SyncSoundCloud => self.start_soundcloud_sync(),
            Command::LastfmLogin => self.lastfm_login(),
            Command::LastfmLogout => {
                self.cfg.lastfm.session_key.clear();
                self.cfg.lastfm.username.clear();
                self.save_config();
                self.lastfm = make_lastfm(&self.cfg, &self.http, &self.paths);
                self.publish_accounts();
            }
            Command::Search(q) => self.search(q),
            Command::UpdateConfig(cfg) => self.update_config(*cfg).await,
            Command::Raise => {
                self.shared.feed.write().unwrap().raise = true;
                self.shared.repaint();
            }
            Command::Quit => {}
        }
    }

    // ---------------------------------------------------------------- playback

    fn current_position(&self) -> f64 {
        if self.status == PlayStatus::Playing {
            self.position + self.position_at.elapsed().as_secs_f64()
        } else {
            self.position
        }
    }

    fn set_position(&mut self, secs: f64) {
        self.position = secs.max(0.0);
        self.position_at = Instant::now();
    }

    fn set_status(&mut self, status: PlayStatus) {
        if status != self.status {
            // Freeze the extrapolated position when leaving "playing".
            let p = self.current_position();
            self.status = status;
            self.set_position(p);
        }
        self.publish_player();
        self.update_presence();
    }

    /// Starts playing the queue's current entry from `start` seconds.
    async fn play_current(&mut self, start: f64) {
        let Some(track) = self.queue.current().cloned() else {
            self.stop().await;
            return;
        };
        self.load_seq += 1;
        self.started_reported = false;
        self.playing = None;
        self.duration = track.duration_secs();
        self.set_position(start);
        self.status = PlayStatus::Loading;
        self.publish_player();
        self.request_lyrics(&track);
        self.dispatch(track, start).await;
    }

    /// Plays a track on the engine matching its source.
    async fn dispatch(&mut self, track: Track, start: f64) {
        let seq = self.load_seq;
        match track.source {
            Source::Local => {
                let path = track.uri.clone();
                self.start_mpv(track, &path, start).await;
            }
            Source::SoundCloud => {
                let sc = self.soundcloud.clone();
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let msg = match sc.stream_url(&track).await {
                        Ok(url) => Internal::StreamReady { seq, track, url },
                        Err(e) => Internal::LoadFailed {
                            seq,
                            error: format!("SoundCloud: {e:#}"),
                        },
                    };
                    let _ = tx.send(msg);
                });
            }
            Source::Spotify => self.start_spotify(track, start).await,
            Source::AppleMusic => {
                if let Some(t) = self.cached_resolution(&track) {
                    Box::pin(self.dispatch_resolved(track, Some(t), start)).await;
                    return;
                }
                let local = self
                    .shared
                    .library
                    .read()
                    .unwrap()
                    .local_match(&track.artist, &track.title)
                    .cloned();
                if local.is_some() {
                    Box::pin(self.dispatch_resolved(track, local, start)).await;
                    return;
                }
                let tx = self.internal_tx.clone();
                let resolver = self.resolver();
                tokio::spawn(async move {
                    let resolved = resolver.resolve(&track).await;
                    let _ = tx.send(Internal::Resolved {
                        seq,
                        original: track,
                        resolved,
                    });
                });
            }
        }
    }

    async fn dispatch_resolved(&mut self, original: Track, resolved: Option<Track>, start: f64) {
        match resolved {
            Some(t) if t.source != Source::AppleMusic => {
                let _ = self.db.set_kv(
                    &format!("resolve:{}", original.id),
                    &serde_json::to_string(&t).unwrap_or_default(),
                );
                {
                    let mut pv = self.shared.player.write().unwrap();
                    pv.via = Some(t.clone());
                }
                self.dispatch(t, start).await;
            }
            _ => {
                let msg = format!("No playable match for “{} – {}”", original.artist, original.title);
                self.load_failed(msg).await;
            }
        }
    }

    fn cached_resolution(&self, track: &Track) -> Option<Track> {
        self.db
            .get_kv(&format!("resolve:{}", track.id))
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Login used for Web API calls: the user's own app if authorized, else the main login.
    fn web_auth(&self) -> Option<Arc<SpotifyAuth>> {
        if !self.cfg.spotify.enabled {
            return None;
        }
        match &self.spotify_web_auth {
            Some(a) if a.has_login() => Some(a.clone()),
            _ => self.spotify_auth.has_login().then(|| self.spotify_auth.clone()),
        }
    }

    fn resolver(&self) -> Resolver {
        Resolver {
            spotify: self.web_auth().map(|auth| (auth, self.spotify_api.clone())),
            soundcloud: self.cfg.soundcloud.enabled.then(|| self.soundcloud.clone()),
        }
    }

    async fn ensure_mpv(&mut self) -> Result<()> {
        if self.mpv.is_some() {
            return Ok(());
        }
        let opts = MpvOptions {
            binary: self.cfg.playback.mpv_path.clone(),
            volume: self.cfg.playback.volume,
            replaygain: self.cfg.playback.replaygain && !self.cfg.playback.bit_perfect,
            gapless: self.cfg.playback.gapless,
            audio_device: self.cfg.playback.audio_device.clone(),
            exclusive: self.cfg.playback.bit_perfect,
        };
        self.mpv = Some(Mpv::spawn(&opts, self.mpv_tx.clone()).await?);
        Ok(())
    }

    async fn start_mpv(&mut self, track: Track, url: &str, start: f64) {
        if self.engine == Engine::Spotify {
            if let Some(sp) = &self.spotify {
                sp.stop();
            }
        }
        if let Err(e) = self.ensure_mpv().await {
            self.load_failed(format!("{e:#}")).await;
            return;
        }
        self.engine = Engine::Mpv;
        self.mpv_preloaded = None;
        self.playing = Some(track);
        if let Some(mpv) = &self.mpv {
            if let Err(e) = mpv.load(url, start).await {
                self.load_failed(format!("mpv: {e:#}")).await;
            }
        }
    }

    async fn start_spotify(&mut self, track: Track, start: f64) {
        if !self.cfg.spotify.enabled || !self.spotify_auth.has_login() {
            self.load_failed("Log in to Spotify in Settings to play Spotify tracks".into())
                .await;
            return;
        }
        if self.spotify.is_none() {
            self.set_account(|f| &mut f.spotify, AccountStatus::Working("Connecting…".into()));
            let r = SpotifyEngine::connect(
                &self.spotify_auth,
                &self.paths.spotify_dir(),
                self.spotify_audio_cache(),
                &self.cfg.spotify,
                self.cfg.playback.volume,
                self.spotify_tx.clone(),
            )
            .await;
            match r {
                Ok(engine) => {
                    self.set_account(|f| &mut f.spotify, AccountStatus::Connected(engine.username.clone()));
                    self.spotify = Some(engine);
                }
                Err(e) => {
                    self.set_account(|f| &mut f.spotify, AccountStatus::Error(format!("{e:#}")));
                    self.load_failed(format!("Spotify: {e:#}")).await;
                    return;
                }
            }
        }
        if self.engine == Engine::Mpv {
            if let Some(mpv) = &self.mpv {
                let _ = mpv.stop().await;
            }
        }
        let auth = self.spotify_auth.clone();
        let Some(sp) = self.spotify.as_mut() else { return };
        if let Err(e) = sp.ensure_session(&auth).await {
            self.load_failed(format!("Spotify: {e:#}")).await;
            return;
        }
        self.engine = Engine::Spotify;
        self.playing = Some(track.clone());
        if let Err(e) = sp.load(&track.uri, (start * 1000.0) as u32) {
            self.load_failed(format!("{e:#}")).await;
        }
    }

    fn spotify_audio_cache(&self) -> Option<PathBuf> {
        self.cfg
            .spotify
            .cache_audio
            .then(|| self.paths.cache_dir.join("spotify"))
    }

    async fn load_failed(&mut self, error: String) {
        self.shared.error(error);
        self.consecutive_failures += 1;
        if self.consecutive_failures >= 5 {
            self.shared.error("Too many tracks failed in a row, stopping");
            self.consecutive_failures = 0;
            self.stop().await;
            return;
        }
        if self.queue.advance(true).is_some() {
            Box::pin(self.play_current(0.0)).await;
            self.publish_queue();
        } else {
            self.stop().await;
        }
    }

    /// Called once the engine actually started producing audio for a new track.
    fn on_track_started(&mut self) {
        if self.started_reported {
            return;
        }
        self.started_reported = true;
        self.consecutive_failures = 0;
        let Some(track) = self.queue.current().cloned() else {
            return;
        };
        self.detect_quality();
        let now = now_unix();
        let _ = self.db.record_play(&track.id, now);
        {
            let mut lib = self.shared.library.write().unwrap();
            lib.recent.retain(|id| id != &track.id);
            lib.recent.insert(0, track.id.clone());
            lib.recent.truncate(50);
            if !lib.tracks.contains_key(&track.id) {
                lib.tracks.insert(track.id.clone(), track.clone());
                let _ = self.db.upsert_tracks(std::slice::from_ref(&track), &HashMap::new());
            }
            lib.version += 1;
        }
        self.scrobble.start(&track.id, track.duration_ms, now);
        if let Some(lfm) = self.lastfm.clone() {
            let t = track.clone();
            tokio::spawn(async move {
                if let Err(e) = lfm.now_playing(&t).await {
                    tracing::debug!("last.fm now playing: {e:#}");
                }
            });
        }
        self.save_session();
    }

    /// Works out the format of the track that just started.
    fn detect_quality(&mut self) {
        self.quality = None;
        let Some(t) = self.playing.clone() else { return };
        match t.source {
            Source::Local => {
                let tx = self.internal_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let quality = library::quality::read(std::path::Path::new(&t.uri));
                    let _ = tx.send(Internal::Quality {
                        track_id: t.id,
                        quality,
                    });
                });
            }
            Source::Spotify => {
                self.quality = Some(AudioQuality {
                    codec: "Ogg Vorbis".into(),
                    bitrate_kbps: Some(self.cfg.spotify.bitrate as u32),
                    ..Default::default()
                });
            }
            // SoundCloud streams are inspected through mpv once loaded.
            Source::SoundCloud | Source::AppleMusic => {}
        }
    }

    async fn mpv_stream_quality(&self) -> Option<AudioQuality> {
        let mpv = self.mpv.as_ref()?;
        let codec = mpv
            .command(serde_json::json!(["get_property", "audio-codec-name"]))
            .await
            .ok()?;
        let bitrate = mpv
            .command(serde_json::json!(["get_property", "audio-bitrate"]))
            .await
            .ok()
            .and_then(|v| v.as_f64());
        let rate = mpv
            .command(serde_json::json!(["get_property", "audio-params/samplerate"]))
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .map(|r| r as u32);
        Some(AudioQuality::from_mpv(codec.as_str()?, bitrate, rate))
    }

    async fn pause(&mut self) {
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.set_pause(true).await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.pause();
                }
            }
            Engine::None => {}
        }
        if self.status == PlayStatus::Playing {
            self.set_status(PlayStatus::Paused);
        }
    }

    async fn resume(&mut self) {
        if self.playing.is_none() || self.engine == Engine::None {
            // Nothing loaded yet (fresh start with a restored session).
            if self.queue.current().is_some() {
                let start = self.resume_position.take().unwrap_or(0.0);
                self.play_current(start).await;
            }
            return;
        }
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.set_pause(false).await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.play();
                }
            }
            Engine::None => {}
        }
        if self.status == PlayStatus::Paused {
            self.set_status(PlayStatus::Playing);
        }
    }

    async fn seek(&mut self, secs: f64) {
        let secs = if self.duration > 0.0 {
            secs.clamp(0.0, self.duration - 0.5)
        } else {
            secs.max(0.0)
        };
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.seek(secs).await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.seek(secs);
                }
            }
            Engine::None => {
                self.resume_position = Some(secs);
            }
        }
        self.set_position(secs);
        self.publish_player();
        self.update_presence();
    }

    async fn stop(&mut self) {
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.stop().await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.stop();
                }
            }
            Engine::None => {}
        }
        self.engine = Engine::None;
        self.playing = None;
        self.set_position(0.0);
        self.set_status(PlayStatus::Stopped);
    }

    async fn after_queue_change(&mut self) {
        self.publish_queue();
        self.publish_player();
        self.refresh_mpv_preload().await;
    }

    /// Queues the next local file in mpv so the transition is gapless.
    async fn refresh_mpv_preload(&mut self) {
        if self.engine != Engine::Mpv || !self.cfg.playback.gapless {
            return;
        }
        let Some(mpv) = &self.mpv else { return };
        let next = self.queue.peek_next().cloned();
        let want = next
            .as_ref()
            .filter(|t| t.source == Source::Local)
            .map(|t| t.id.clone());
        if want == self.mpv_preloaded {
            return;
        }
        let _ = mpv.clear_upcoming().await;
        self.mpv_preloaded = None;
        if let Some(t) = next.filter(|t| t.source == Source::Local) {
            if mpv.append(&t.uri).await.is_ok() {
                self.mpv_preloaded = Some(t.id);
            }
        }
    }

    async fn on_mpv(&mut self, ev: MpvEvent) {
        if ev == MpvEvent::Died {
            self.mpv = None;
            self.mpv_preloaded = None;
            if self.engine == Engine::Mpv {
                self.engine = Engine::None;
                self.playing = None;
                self.set_status(PlayStatus::Stopped);
                self.shared.error("mpv stopped unexpectedly");
            }
            return;
        }
        if self.engine != Engine::Mpv {
            return;
        }
        match ev {
            MpvEvent::FileLoaded => {
                let start = self.resume_position.take().unwrap_or(self.position);
                self.set_position(start);
                self.status = PlayStatus::Playing;
                self.on_track_started();
                if self.playing.as_ref().is_some_and(|t| t.source == Source::SoundCloud) {
                    self.quality = self.mpv_stream_quality().await;
                }
                self.publish_player();
                self.update_presence();
                self.refresh_mpv_preload().await;
            }
            MpvEvent::Pause(p) => {
                if self.status == PlayStatus::Playing || self.status == PlayStatus::Paused {
                    self.set_status(if p { PlayStatus::Paused } else { PlayStatus::Playing });
                }
            }
            MpvEvent::Duration(d) => {
                if d > 0.0 {
                    self.duration = d;
                    self.publish_player();
                }
            }
            MpvEvent::EndFile { reason, error } => match reason.as_str() {
                "eof" => {
                    if let Some(pre) = self.mpv_preloaded.take() {
                        // mpv continues into the appended file by itself.
                        let next = self.queue.advance(false);
                        if next.as_ref().map(|t| &t.id) == Some(&pre) {
                            self.load_seq += 1;
                            self.started_reported = false;
                            self.playing = next.clone();
                            self.duration = next.map(|t| t.duration_secs()).unwrap_or(0.0);
                            self.set_position(0.0);
                            if let Some(t) = self.queue.current().cloned() {
                                self.request_lyrics(&t);
                            }
                            self.publish_queue();
                            self.publish_player();
                            return;
                        }
                        self.play_current(0.0).await;
                        self.publish_queue();
                    } else if self.queue.advance(false).is_some() {
                        self.play_current(0.0).await;
                        self.publish_queue();
                    } else {
                        self.stop().await;
                        self.publish_queue();
                    }
                }
                "error" => {
                    let name = self.playing.as_ref().map(|t| t.title.clone()).unwrap_or_default();
                    self.load_failed(format!(
                        "Can't play “{name}”: {}",
                        error.unwrap_or_else(|| "unknown error".into())
                    ))
                    .await;
                }
                _ => {}
            },
            MpvEvent::StartFile { .. } | MpvEvent::Died => {}
        }
    }

    async fn on_spotify(&mut self, ev: SpotifyEvent) {
        if self.engine != Engine::Spotify {
            return;
        }
        match ev {
            SpotifyEvent::Loading => {}
            SpotifyEvent::Playing { position_ms } => {
                self.set_position(position_ms as f64 / 1000.0);
                self.status = PlayStatus::Playing;
                self.on_track_started();
                self.publish_player();
                self.update_presence();
            }
            SpotifyEvent::Paused { position_ms } => {
                self.set_position(position_ms as f64 / 1000.0);
                self.set_status(PlayStatus::Paused);
            }
            SpotifyEvent::Seeked { position_ms } => {
                self.set_position(position_ms as f64 / 1000.0);
                self.publish_player();
            }
            SpotifyEvent::TimeToPreload => {
                if let (Some(next), Some(sp)) = (self.queue.peek_next(), &self.spotify) {
                    if next.source == Source::Spotify {
                        sp.preload(&next.uri);
                    }
                }
            }
            SpotifyEvent::EndOfTrack => {
                if self.queue.advance(false).is_some() {
                    self.play_current(0.0).await;
                } else {
                    self.stop().await;
                }
                self.publish_queue();
            }
            SpotifyEvent::Unavailable => {
                let name = self.playing.as_ref().map(|t| t.title.clone()).unwrap_or_default();
                self.load_failed(format!("“{name}” is not available on Spotify in your region"))
                    .await;
            }
            SpotifyEvent::Stopped => {}
        }
    }

    async fn on_tick(&mut self) {
        let dt = self.last_tick.elapsed();
        self.last_tick = Instant::now();
        let playing = self.status == PlayStatus::Playing;

        if playing && self.engine == Engine::Mpv {
            if let Some(mpv) = &self.mpv {
                if let Some(pos) = mpv.time_pos().await {
                    self.set_position(pos);
                }
            }
        }

        self.scrobble.tick(playing, dt);
        if self.scrobble.should_scrobble() {
            self.scrobble.mark_scrobbled();
            if let (Some(lfm), Some((_, started))) = (self.lastfm.clone(), self.scrobble.current()) {
                let track = self.queue.current().cloned();
                if let Some(t) = track {
                    tokio::spawn(async move {
                        if let Err(e) = lfm.scrobble(&t, started).await {
                            tracing::warn!("scrobble failed: {e:#}");
                        }
                    });
                }
            }
        }
        if playing {
            self.mpris.update(self.queue.current(), true, self.current_position());
        }
        // Expire toasts.
        let expired = {
            let mut feed = self.shared.feed.write().unwrap();
            let before = feed.toasts.len();
            feed.toasts.retain(|t| t.at.elapsed() < Duration::from_secs(6));
            before != feed.toasts.len()
        };
        if expired {
            self.shared.repaint();
        }
    }

    // ---------------------------------------------------------------- publishing

    fn publish_player(&mut self) {
        {
            let mut pv = self.shared.player.write().unwrap();
            let current = self.queue.current().cloned();
            if pv.current.as_ref().map(|t| &t.id) != current.as_ref().map(|t| &t.id) {
                pv.via = None;
            }
            if let (Some(c), Some(p)) = (&current, &self.playing) {
                if c.id != p.id {
                    pv.via = Some(p.clone());
                }
            }
            pv.current = current;
            pv.status = self.status;
            pv.position = self.position;
            pv.position_at = self.position_at;
            pv.duration = self.duration;
            pv.volume = self.cfg.playback.volume;
            pv.shuffle = self.queue.shuffle;
            pv.repeat = self.queue.repeat;
            pv.context = self.queue.context_name.clone();
            pv.quality = self.quality.clone();
        }
        self.mpris.update(
            self.queue.current(),
            self.status == PlayStatus::Playing,
            self.current_position(),
        );
        self.shared.repaint();
    }

    fn publish_queue(&mut self) {
        {
            let mut pv = self.shared.player.write().unwrap();
            pv.upcoming = self.queue.upcoming(200);
            pv.up_next_len = self.queue.up_next_len();
        }
        self.shared.repaint();
    }

    fn update_presence(&self) {
        let presence = self
            .queue
            .current()
            .filter(|_| self.status != PlayStatus::Stopped)
            .map(|t| {
                let mut track = t.clone();
                // Resolved imports have no art of their own; borrow the playing version's cover.
                if track.art.is_none() {
                    track.art = self.playing.as_ref().and_then(|p| p.art.clone());
                }
                Presence {
                    track,
                    position_secs: self.current_position(),
                    playing: self.status == PlayStatus::Playing,
                }
            });
        self.discord.set(presence);
    }

    fn set_account(&self, field: impl FnOnce(&mut Feed) -> &mut AccountStatus, status: AccountStatus) {
        {
            let mut feed = self.shared.feed.write().unwrap();
            *field(&mut feed) = status;
        }
        self.shared.repaint();
    }

    fn publish_accounts(&self) {
        let spotify = if self.spotify_auth.has_login() {
            AccountStatus::Connected(self.db.get_kv("spotify_user").unwrap_or_else(|| "Logged in".into()))
        } else {
            AccountStatus::Off
        };
        let lastfm = if !self.cfg.lastfm.session_key.is_empty() {
            AccountStatus::Connected(self.cfg.lastfm.username.clone())
        } else {
            AccountStatus::Off
        };
        let web_api = match &self.spotify_web_auth {
            Some(a) if a.has_login() => AccountStatus::Connected(String::new()),
            _ => AccountStatus::Off,
        };
        let mut feed = self.shared.feed.write().unwrap();
        feed.spotify_logged_in = self.spotify_auth.has_login();
        if !matches!(feed.spotify_web_api, AccountStatus::Working(_)) {
            feed.spotify_web_api = web_api;
        }
        if !matches!(feed.spotify, AccountStatus::Working(_)) {
            feed.spotify = spotify;
        }
        if !matches!(feed.lastfm, AccountStatus::Working(_)) {
            feed.lastfm = lastfm;
        }
        drop(feed);
        self.shared.repaint();
    }

    fn request_lyrics(&self, track: &Track) {
        {
            let mut feed = self.shared.feed.write().unwrap();
            if feed.lyrics.track_id == track.id {
                return;
            }
            feed.lyrics = LyricsState {
                track_id: track.id.clone(),
                loading: self.cfg.lyrics.enabled,
                lyrics: None,
            };
        }
        if !self.cfg.lyrics.enabled {
            return;
        }
        let fetcher = self.lyrics.clone();
        let tx = self.internal_tx.clone();
        let t = track.clone();
        // Imported Apple Music songs: look lyrics up by metadata, which is what LRCLIB needs anyway.
        tokio::spawn(async move {
            let lyrics = fetcher.fetch(&t).await;
            let _ = tx.send(Internal::Lyrics { track_id: t.id, lyrics });
        });
    }

    // ---------------------------------------------------------------- library

    fn store_tracks(&mut self, tracks: &[Track]) {
        let new: Vec<Track> = {
            let lib = self.shared.library.read().unwrap();
            tracks
                .iter()
                .filter(|t| !lib.tracks.contains_key(&t.id))
                .cloned()
                .collect()
        };
        if new.is_empty() {
            return;
        }
        if let Err(e) = self.db.upsert_tracks(&new, &HashMap::new()) {
            self.shared.error(format!("Database error: {e:#}"));
        }
        let mut lib = self.shared.library.write().unwrap();
        for t in new {
            lib.tracks.insert(t.id.clone(), t);
        }
    }

    /// Saves/replaces playlists, deletes `remove`, then reindexes.
    fn save_playlists(&mut self, playlists: Vec<Playlist>, remove: &[String]) {
        {
            let mut lib = self.shared.library.write().unwrap();
            for p in playlists {
                match lib.playlists.iter_mut().find(|x| x.id == p.id) {
                    Some(existing) => *existing = p,
                    None => lib.playlists.push(p),
                }
            }
            lib.playlists.retain(|p| !remove.contains(&p.id));
            lib.reindex();
            for (i, p) in lib.playlists.iter().enumerate() {
                if let Err(e) = self.db.save_playlist(p, i as i64) {
                    tracing::error!("saving playlist {}: {e:#}", p.id);
                }
            }
        }
        for id in remove {
            let _ = self.db.delete_playlist(id);
        }
        self.shared.repaint();
    }

    fn toggle_like(&mut self, track: Track) {
        self.store_tracks(std::slice::from_ref(&track));
        let (liked_now, playlist) = {
            let mut lib = self.shared.library.write().unwrap();
            if lib.playlist(LIKED_ID).is_none() {
                lib.playlists.insert(0, liked_playlist());
            }
            let p = lib.playlist_mut(LIKED_ID).expect("liked playlist");
            let liked_now = if let Some(i) = p.track_ids.iter().position(|id| id == &track.id) {
                p.track_ids.remove(i);
                false
            } else {
                p.track_ids.insert(0, track.id.clone());
                true
            };
            (liked_now, p.clone())
        };
        self.save_playlists(vec![playlist], &[]);

        if let (Source::Spotify, Some(auth)) = (track.source, self.web_auth()) {
            let api = self.spotify_api.clone();
            let t = track.clone();
            tokio::spawn(async move {
                if let Ok(token) = auth.token().await {
                    if let Err(e) = api.set_liked(&token, &t.uri, liked_now).await {
                        tracing::warn!("Spotify like sync failed: {e:#}");
                    }
                }
            });
        }
        if let Some(lfm) = self.lastfm.clone() {
            tokio::spawn(async move {
                let _ = lfm.love(&track, liked_now).await;
            });
        }
    }

    fn start_scan(&mut self) {
        let folders = self.cfg.library.folders.clone();
        if folders.is_empty() {
            return;
        }
        let known = self.db.local_mtimes().unwrap_or_default();
        let shared = self.shared.clone();
        let tx = self.internal_tx.clone();
        shared.feed.write().unwrap().scan = Some((0, 0));
        tokio::task::spawn_blocking(move || {
            let progress = |done: usize, total: usize| {
                shared.feed.write().unwrap().scan = Some((done, total));
                shared.repaint();
            };
            let result = library::scanner::scan(&folders, &known, &progress);
            let _ = tx.send(Internal::ScanDone(result));
        });
    }

    fn merge_scan(&mut self, result: library::scanner::ScanResult) {
        let changed = result.changed.len();
        if let Err(e) = self.db.upsert_tracks(&result.changed, &result.mtimes) {
            self.shared.error(format!("Database error: {e:#}"));
        }
        let _ = self.db.delete_tracks(&result.removed);
        {
            let mut lib = self.shared.library.write().unwrap();
            for id in &result.removed {
                lib.tracks.remove(id);
            }
            for t in result.changed {
                lib.tracks.insert(t.id.clone(), t);
            }
            lib.reindex();
        }
        self.shared.feed.write().unwrap().scan = None;
        if changed > 0 || !result.removed.is_empty() {
            self.shared.info(format!(
                "Library updated: {} files, {changed} new or changed, {} removed",
                result.total_files,
                result.removed.len()
            ));
        }
        self.shared.repaint();
    }

    /// Merges playlists imported from a service. Playlists of `kind` that aren't in `keep_ids`
    /// are removed when `exclusive` is set (i.e. deleted on the service).
    fn merge_imported(&mut self, tracks: Vec<Track>, playlists: Vec<Playlist>, prune_kinds: &[PlaylistKind]) {
        if let Err(e) = self.db.upsert_tracks(&tracks, &HashMap::new()) {
            self.shared.error(format!("Database error: {e:#}"));
            return;
        }
        {
            let mut lib = self.shared.library.write().unwrap();
            for t in tracks {
                lib.tracks.insert(t.id.clone(), t);
            }
        }
        let keep: HashSet<String> = playlists.iter().map(|p| p.id.clone()).collect();
        let remove: Vec<String> = {
            let lib = self.shared.library.read().unwrap();
            lib.playlists
                .iter()
                .filter(|p| prune_kinds.contains(&p.kind) && !keep.contains(&p.id))
                .map(|p| p.id.clone())
                .collect()
        };
        self.save_playlists(playlists, &remove);
        if !remove.is_empty() {
            if let Ok(n) = self.db.prune_orphans() {
                if n > 0 {
                    let mut lib = self.shared.library.write().unwrap();
                    let referenced: HashSet<String> =
                        lib.playlists.iter().flat_map(|p| p.track_ids.iter().cloned()).collect();
                    lib.tracks
                        .retain(|id, t| t.source == Source::Local || referenced.contains(id));
                    lib.reindex();
                }
            }
        }
    }

    // ---------------------------------------------------------------- spotify

    fn spotify_login(&mut self) {
        let auth = self.spotify_auth.clone();
        let tx = self.cmd_tx.clone();
        let shared = self.shared.clone();
        self.set_account(
            |f| &mut f.spotify,
            AccountStatus::Working("Waiting for browser login…".into()),
        );
        tokio::spawn(async move {
            match auth.login().await {
                Ok(_) => {
                    let _ = tx.send(Command::SyncSpotify);
                }
                Err(e) => {
                    shared.feed.write().unwrap().spotify = AccountStatus::Error(format!("{e:#}"));
                    shared.repaint();
                }
            }
        });
    }

    fn connect_spotify(&mut self) {
        if self.spotify.is_some() || self.spotify_connecting {
            return;
        }
        self.spotify_connecting = true;
        let auth = self.spotify_auth.clone();
        let dir = self.paths.spotify_dir();
        let cache = self.spotify_audio_cache();
        let cfg = self.cfg.spotify.clone();
        let vol = self.cfg.playback.volume;
        let ev = self.spotify_tx.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = SpotifyEngine::connect(&auth, &dir, cache, &cfg, vol, ev).await;
            let _ = tx.send(Internal::SpotifyReady(r));
        });
    }

    fn start_spotify_sync(&mut self) {
        if !self.spotify_auth.has_login() || self.spotify_syncing {
            return;
        }
        // The library is imported through the playback session, so connect first.
        let Some(session) = self.spotify.as_ref().and_then(|e| e.session()) else {
            self.spotify_sync_pending = true;
            self.set_account(|f| &mut f.spotify, AccountStatus::Working("Connecting…".into()));
            if self.spotify.is_some() {
                // Session dropped: reconnect from scratch.
                if let Some(sp) = self.spotify.take() {
                    sp.shutdown();
                }
            }
            self.connect_spotify();
            return;
        };
        self.spotify_syncing = true;
        self.set_account(|f| &mut f.spotify, AccountStatus::Working("Syncing playlists…".into()));
        let known: HashSet<String> = {
            let lib = self.shared.library.read().unwrap();
            lib.tracks
                .values()
                .filter(|t| t.source == Source::Spotify)
                .map(|t| t.id.clone())
                .collect()
        };
        let web = self.web_auth().map(|a| (a, self.spotify_api.clone()));
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let progress_tx = tx.clone();
            let progress = move |text: String| {
                let _ = progress_tx.send(Internal::SyncProgress(text));
            };
            let r = match spotify_sync_internal(session, known, progress).await {
                Ok(sync) => Ok(sync),
                Err(e) => {
                    tracing::warn!("Spotify library import via session failed: {e:#}; trying the Web API");
                    match web {
                        Some((auth, api)) => spotify_sync_web(&auth, &api, &HashMap::new())
                            .await
                            .map_err(|e2| anyhow!("{e:#} (Web API: {})", friendly_spotify_error(&e2))),
                        None => Err(e),
                    }
                }
            };
            let _ = tx.send(Internal::SpotifySynced(r));
        });
    }

    /// Fallback when no playback session can be opened (e.g. connection refused).
    fn start_spotify_web_sync(&mut self) {
        let Some(auth) = self.web_auth() else { return };
        if self.spotify_syncing {
            return;
        }
        self.spotify_syncing = true;
        self.set_account(|f| &mut f.spotify, AccountStatus::Working("Syncing playlists…".into()));
        let api = self.spotify_api.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = spotify_sync_web(&auth, &api, &HashMap::new())
                .await
                .map_err(|e| anyhow!("{}", friendly_spotify_error(&e)));
            let _ = tx.send(Internal::SpotifySynced(r));
        });
    }

    fn spotify_web_api_login(&mut self) {
        let Some(auth) = self.spotify_web_auth.clone() else {
            self.shared
                .error("Enter your Spotify app's client ID in Settings → Spotify → Advanced first");
            return;
        };
        self.set_account(
            |f| &mut f.spotify_web_api,
            AccountStatus::Working("Waiting for browser login…".into()),
        );
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let status = match auth.login().await {
                Ok(_) => AccountStatus::Connected(String::new()),
                Err(e) => AccountStatus::Error(format!("{e:#}")),
            };
            shared.feed.write().unwrap().spotify_web_api = status;
            shared.repaint();
        });
    }

    fn merge_spotify(&mut self, sync: SpotifySync) {
        let _ = self.db.set_kv("spotify_user", &sync.user);
        let mut tracks = Vec::new();
        let mut playlists = Vec::new();
        let existing: HashMap<String, Vec<String>> = {
            let lib = self.shared.library.read().unwrap();
            lib.playlists
                .iter()
                .map(|p| (p.id.clone(), p.track_ids.clone()))
                .collect()
        };
        tracks.extend(sync.tracks);
        let liked_ids = sync.liked;
        playlists.push(Playlist {
            id: "spotify:liked".into(),
            name: "Liked Songs".into(),
            kind: PlaylistKind::SpotifyLiked,
            remote_id: None,
            description: "Your liked songs on Spotify".into(),
            art: None,
            track_ids: liked_ids,
        });
        let count = sync.playlists.len();
        for (meta, list) in sync.playlists {
            let id = format!("spotify:{}", meta.id);
            let track_ids = match list {
                Some(ids) => {
                    let _ = self
                        .db
                        .set_kv(&format!("spotify_snapshot:{}", meta.id), &meta.snapshot_id);
                    ids
                }
                None => existing.get(&id).cloned().unwrap_or_default(),
            };
            playlists.push(Playlist {
                id,
                name: meta.name,
                kind: PlaylistKind::Spotify,
                remote_id: Some(meta.id),
                description: meta.description,
                art: meta.art,
                track_ids,
            });
        }
        self.merge_imported(tracks, playlists, &[PlaylistKind::Spotify, PlaylistKind::SpotifyLiked]);
        self.set_account(|f| &mut f.spotify, AccountStatus::Connected(sync.user));
        self.shared.info(format!("Spotify synced: {count} playlists"));
        let _ = self.db.set_kv("spotify_synced_at", &now_unix().to_string());
        self.connect_spotify();
    }

    // ---------------------------------------------------------------- soundcloud

    fn soundcloud_configured(&self) -> bool {
        !self.cfg.soundcloud.profile_url.trim().is_empty() || !self.cfg.soundcloud.oauth_token.trim().is_empty()
    }

    fn start_soundcloud_sync(&mut self) {
        if !self.soundcloud_configured() {
            self.shared.error("Set your SoundCloud profile URL in Settings first");
            return;
        }
        self.set_account(|f| &mut f.soundcloud, AccountStatus::Working("Syncing…".into()));
        let sc = self.soundcloud.clone();
        let profile = self.cfg.soundcloud.profile_url.trim().to_string();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = async {
                let user = if profile.is_empty() {
                    sc.me().await?
                } else {
                    sc.resolve_user(&profile).await?
                };
                let likes = sc.likes(user.id).await?;
                let playlists = sc.playlists(user.id).await.unwrap_or_else(|e| {
                    tracing::warn!("SoundCloud playlists: {e:#}");
                    Vec::new()
                });
                Ok(SoundCloudSync {
                    user: user.username,
                    likes,
                    playlists,
                })
            }
            .await;
            let _ = tx.send(Internal::SoundCloudSynced(r));
        });
    }

    fn merge_soundcloud(&mut self, sync: SoundCloudSync) {
        let mut tracks = Vec::new();
        let mut playlists = vec![Playlist {
            id: "soundcloud:likes".into(),
            name: "SoundCloud Likes".into(),
            kind: PlaylistKind::SoundCloudLikes,
            remote_id: None,
            description: format!("Tracks liked by {}", sync.user),
            art: sync.likes.iter().find_map(|t| t.art.clone()),
            track_ids: sync.likes.iter().map(|t| t.id.clone()).collect(),
        }];
        tracks.extend(sync.likes);
        let count = sync.playlists.len();
        for p in sync.playlists {
            playlists.push(Playlist {
                id: format!("soundcloud:pl:{}", p.remote_id),
                name: p.name,
                kind: PlaylistKind::SoundCloud,
                remote_id: Some(p.remote_id),
                description: p.description,
                art: p.art,
                track_ids: p.tracks.iter().map(|t| t.id.clone()).collect(),
            });
            tracks.extend(p.tracks);
        }
        self.merge_imported(
            tracks,
            playlists,
            &[PlaylistKind::SoundCloud, PlaylistKind::SoundCloudLikes],
        );
        let _ = self.db.set_kv("soundcloud_user", &sync.user);
        let _ = self.db.set_kv("soundcloud_synced_at", &now_unix().to_string());
        self.set_account(|f| &mut f.soundcloud, AccountStatus::Connected(sync.user));
        self.shared
            .info(format!("SoundCloud synced: likes + {count} playlists"));
    }

    // ---------------------------------------------------------------- apple music

    fn import_apple_api(&mut self) {
        let c = &self.cfg.apple_music;
        if c.user_token.trim().is_empty() {
            self.shared.error("Paste your Apple Music user token in Settings first");
            return;
        }
        self.shared.info("Importing from Apple Music…");
        let http = self.http.clone();
        let (dev, user, store) = (c.developer_token.clone(), c.user_token.clone(), c.storefront.clone());
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = async {
                let dev = if dev.trim().is_empty() {
                    apple_music::scrape_developer_token(&http).await?
                } else {
                    dev
                };
                let store = if store.trim().is_empty() {
                    "us".to_string()
                } else {
                    store
                };
                let api = AppleMusicApi::new(http, dev.trim(), user.trim(), &store);
                let songs = api.library_songs().await?;
                let playlists = api.library_playlists().await?;
                Ok((songs, playlists))
            }
            .await;
            let _ = tx.send(Internal::AppleImported(r));
        });
    }

    fn merge_apple(&mut self, songs: Vec<Track>, imported: Vec<ImportedPlaylist>) {
        let mut tracks = songs.clone();
        let mut playlists = Vec::new();
        if !songs.is_empty() {
            playlists.push(Playlist {
                id: "applemusic:library".into(),
                name: "Apple Music Library".into(),
                kind: PlaylistKind::AppleMusic,
                remote_id: None,
                description: "Songs imported from Apple Music".into(),
                art: songs.iter().find_map(|t| t.art.clone()),
                track_ids: songs.iter().map(|t| t.id.clone()).collect(),
            });
        }
        let count = imported.len();
        for p in imported {
            playlists.push(Playlist {
                id: format!("applemusic:{}", p.remote_id),
                name: p.name,
                kind: PlaylistKind::AppleMusic,
                remote_id: Some(p.remote_id),
                description: p.description,
                art: p.art.or_else(|| p.tracks.iter().find_map(|t| t.art.clone())),
                track_ids: p.tracks.iter().map(|t| t.id.clone()).collect(),
            });
            tracks.extend(p.tracks);
        }
        self.merge_imported(tracks, playlists, &[]);
        self.shared.info(format!(
            "Apple Music imported: {} songs, {count} playlists",
            songs.len()
        ));
    }

    // ---------------------------------------------------------------- search, last.fm

    fn search(&mut self, query: String) {
        let q = query.trim().to_string();
        {
            let mut feed = self.shared.feed.write().unwrap();
            feed.search = SearchState {
                query: q.clone(),
                pending: 1,
                ..Default::default()
            };
        }
        if q.is_empty() {
            self.shared.feed.write().unwrap().search.pending = 0;
            return;
        }
        let spotify = self.web_auth().map(|auth| (auth, self.spotify_api.clone()));
        let sc = self.cfg.soundcloud.enabled.then(|| self.soundcloud.clone());
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let sp_fut = async {
                match &spotify {
                    Some((auth, api)) => api.search(&auth.token().await?, &q, 20).await,
                    None => Ok(Vec::new()),
                }
            };
            let sc_fut = async {
                match &sc {
                    Some(sc) => sc.search(&q, 20).await,
                    None => Ok(Vec::new()),
                }
            };
            let (spotify, soundcloud) = tokio::join!(sp_fut, sc_fut);
            let _ = tx.send(Internal::Search {
                query: q,
                spotify,
                soundcloud,
            });
        });
    }

    fn lastfm_login(&mut self) {
        let Some(lfm) = self.lastfm.clone() else {
            self.shared.error("Enter your Last.fm API key and secret first");
            return;
        };
        self.set_account(
            |f| &mut f.lastfm,
            AccountStatus::Working("Approve MultiMusic in your browser…".into()),
        );
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let r = async {
                let token = lfm.get_token().await?;
                let _ = open::that_detached(lfm.auth_url(&token));
                // Poll until the user approved access (up to 3 minutes).
                for _ in 0..60 {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    match lfm.get_session(&token).await {
                        Ok(s) => return Ok(s),
                        Err(e) if crate::integrations::lastfm::is_pending_auth(&e) => continue,
                        Err(e) => return Err(e),
                    }
                }
                Err(anyhow!("timed out waiting for Last.fm approval"))
            }
            .await;
            let _ = tx.send(Internal::LastfmSession(r));
        });
    }

    async fn update_config(&mut self, mut cfg: Config) {
        // Secrets obtained by the service are not editable in the UI copy.
        cfg.lastfm.session_key = self.cfg.lastfm.session_key.clone();
        cfg.lastfm.username = self.cfg.lastfm.username.clone();
        let old = std::mem::replace(&mut self.cfg, cfg);

        if old.playback != self.cfg.playback {
            if let Some(mpv) = &self.mpv {
                let p = &self.cfg.playback;
                let _ = mpv
                    .command(serde_json::json!([
                        "set_property",
                        "audio-exclusive",
                        if p.bit_perfect { "yes" } else { "no" }
                    ]))
                    .await;
                let _ = mpv
                    .command(serde_json::json!([
                        "set_property",
                        "replaygain",
                        if p.replaygain && !p.bit_perfect { "track" } else { "no" }
                    ]))
                    .await;
                let _ = mpv
                    .command(serde_json::json!([
                        "set_property",
                        "gapless-audio",
                        if p.gapless { "weak" } else { "no" }
                    ]))
                    .await;
                let dev = if p.audio_device.is_empty() {
                    "auto"
                } else {
                    p.audio_device.as_str()
                };
                let _ = mpv
                    .command(serde_json::json!(["set_property", "audio-device", dev]))
                    .await;
            }
            if old.playback.mpv_path != self.cfg.playback.mpv_path && self.engine != Engine::Mpv {
                if let Some(mpv) = self.mpv.take() {
                    mpv.quit().await;
                }
            }
        }
        if old.discord != self.cfg.discord {
            let d = &self.cfg.discord;
            self.discord.configure(d.enabled, &d.app_id, d.song_as_activity_name);
            self.update_presence();
        }
        if old.lastfm != self.cfg.lastfm {
            self.lastfm = make_lastfm(&self.cfg, &self.http, &self.paths);
        }
        if old.soundcloud != self.cfg.soundcloud {
            self.soundcloud = Arc::new(SoundCloud::new(
                self.http.clone(),
                &self.cfg.soundcloud.client_id,
                &self.cfg.soundcloud.oauth_token,
            ));
        }
        if old.lyrics != self.cfg.lyrics {
            self.lyrics = Arc::new(LyricsFetcher::new(
                self.http.clone(),
                self.paths.lyrics_cache(),
                self.cfg.lyrics.online,
            ));
        }
        if old.spotify.web_api_client_id != self.cfg.spotify.web_api_client_id
            || old.spotify.web_api_redirect_port != self.cfg.spotify.web_api_redirect_port
        {
            self.spotify_web_auth = SpotifyAuth::web_api(&self.cfg.spotify, &self.paths.spotify_dir()).map(Arc::new);
            self.publish_accounts();
        }
        if old.spotify != self.cfg.spotify {
            if old.spotify.client_id != self.cfg.spotify.client_id
                || old.spotify.redirect_port != self.cfg.spotify.redirect_port
            {
                self.spotify_auth = Arc::new(SpotifyAuth::new(&self.cfg.spotify, &self.paths.spotify_dir()));
                self.publish_accounts();
            }
            // Bitrate/normalisation apply on the next connection.
            if self.engine != Engine::Spotify {
                if let Some(sp) = self.spotify.take() {
                    sp.shutdown();
                }
            }
        }
        if old.library.folders != self.cfg.library.folders {
            self.start_scan();
        }
        self.save_config();
    }

    fn save_config(&self) {
        if let Err(e) = self.cfg.save(&self.paths) {
            self.shared.error(format!("Couldn't save settings: {e:#}"));
        }
    }

    // ---------------------------------------------------------------- session

    fn save_session(&self) {
        let mut tracks: Vec<Track> = Vec::new();
        if let Some(c) = self.queue.current() {
            tracks.push(c.clone());
        }
        tracks.extend(self.queue.upcoming(500));
        if tracks.is_empty() {
            return;
        }
        let session = SavedSession {
            tracks: tracks.iter().map(|t| t.id.clone()).collect(),
            index: 0,
            position: self.current_position(),
            context: self.queue.context_name.clone(),
            shuffle: self.queue.shuffle,
            repeat: self.queue.repeat,
        };
        if let Ok(text) = serde_json::to_string(&session) {
            let _ = self.db.set_kv("session", &text);
        }
    }

    fn restore_session(&mut self) {
        let Some(session) = self
            .db
            .get_kv("session")
            .and_then(|s| serde_json::from_str::<SavedSession>(&s).ok())
        else {
            return;
        };
        let tracks = self.shared.library.read().unwrap().tracks_for(&session.tracks);
        if tracks.is_empty() {
            return;
        }
        self.queue.repeat = session.repeat;
        self.queue.set_context(tracks, session.index, session.context);
        self.queue.shuffle = session.shuffle;
        if let Some(t) = self.queue.current().cloned() {
            self.duration = t.duration_secs();
            self.request_lyrics(&t);
        }
        self.resume_position = Some(session.position);
        self.set_position(session.position);
        self.status = PlayStatus::Paused;
    }

    // ---------------------------------------------------------------- internal results

    async fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::StreamReady { seq, track, url } => {
                if seq == self.load_seq {
                    let start = self.position;
                    self.start_mpv(track, &url, start).await;
                }
            }
            Internal::Resolved {
                seq,
                original,
                resolved,
            } => {
                if seq == self.load_seq {
                    let start = self.position;
                    self.dispatch_resolved(original, resolved, start).await;
                }
            }
            Internal::LoadFailed { seq, error } => {
                if seq == self.load_seq {
                    self.load_failed(error).await;
                }
            }
            Internal::SpotifyReady(r) => {
                self.spotify_connecting = false;
                match r {
                    Ok(engine) => {
                        if self.spotify.is_none() {
                            self.spotify = Some(engine);
                        } else {
                            engine.shutdown();
                        }
                        if std::mem::take(&mut self.spotify_sync_pending) {
                            self.start_spotify_sync();
                        } else if !self.spotify_syncing {
                            let name = self.db.get_kv("spotify_user").unwrap_or_default();
                            self.set_account(|f| &mut f.spotify, AccountStatus::Connected(name));
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Spotify connect failed: {e:#}");
                        self.set_account(
                            |f| &mut f.spotify,
                            AccountStatus::Error(format!("Couldn't connect: {e:#}")),
                        );
                        if std::mem::take(&mut self.spotify_sync_pending) {
                            self.start_spotify_web_sync();
                        }
                    }
                }
            }
            Internal::SpotifySynced(r) => {
                self.spotify_syncing = false;
                match r {
                    Ok(sync) => self.merge_spotify(sync),
                    Err(e) => {
                        self.set_account(|f| &mut f.spotify, AccountStatus::Error(format!("Sync failed: {e:#}")));
                        self.shared.error(format!("Spotify sync failed: {e:#}"));
                    }
                }
            }
            Internal::SoundCloudSynced(r) => match r {
                Ok(sync) => self.merge_soundcloud(sync),
                Err(e) => {
                    self.set_account(|f| &mut f.soundcloud, AccountStatus::Error(format!("{e:#}")));
                    self.shared.error(format!("SoundCloud sync failed: {e:#}"));
                }
            },
            Internal::AppleImported(r) => match r {
                Ok((songs, playlists)) => self.merge_apple(songs, playlists),
                Err(e) => self.shared.error(format!("Apple Music import failed: {e:#}")),
            },
            Internal::M3uImported(r) => match r {
                Ok(p) => {
                    let name = p.name.clone();
                    let playlist = Playlist {
                        id: format!("m3u:{}", p.remote_id),
                        name: p.name,
                        kind: PlaylistKind::M3u,
                        remote_id: Some(p.remote_id),
                        description: p.description,
                        art: p.art,
                        track_ids: p.tracks.iter().map(|t| t.id.clone()).collect(),
                    };
                    self.merge_imported(p.tracks, vec![playlist], &[]);
                    self.shared.info(format!("Imported playlist “{name}”"));
                }
                Err(e) => self.shared.error(format!("Import failed: {e:#}")),
            },
            Internal::ScanDone(result) => self.merge_scan(result),
            Internal::Lyrics { track_id, lyrics } => {
                let mut feed = self.shared.feed.write().unwrap();
                if feed.lyrics.track_id == track_id {
                    feed.lyrics.loading = false;
                    feed.lyrics.lyrics = lyrics;
                }
                drop(feed);
                self.shared.repaint();
            }
            Internal::Search {
                query,
                spotify,
                soundcloud,
            } => {
                let mut feed = self.shared.feed.write().unwrap();
                if feed.search.query == query {
                    feed.search.pending = 0;
                    match spotify {
                        Ok(t) => feed.search.spotify = t,
                        Err(e) => feed
                            .search
                            .errors
                            .push(format!("Spotify: {}", friendly_spotify_error(&e))),
                    }
                    match soundcloud {
                        Ok(t) => feed.search.soundcloud = t,
                        Err(e) => feed.search.errors.push(format!("SoundCloud: {e:#}")),
                    }
                }
                drop(feed);
                self.shared.repaint();
            }
            Internal::Quality { track_id, quality } => {
                if self.playing.as_ref().is_some_and(|t| t.id == track_id) {
                    self.quality = quality;
                    self.publish_player();
                }
            }
            Internal::SyncProgress(text) => {
                if self.spotify_syncing {
                    self.set_account(|f| &mut f.spotify, AccountStatus::Working(text));
                }
            }
            Internal::AudioDevices(devices) => {
                self.shared.feed.write().unwrap().audio_devices = devices;
                self.shared.repaint();
            }
            Internal::LastfmSession(r) => match r {
                Ok((key, user)) => {
                    self.cfg.lastfm.session_key = key;
                    self.cfg.lastfm.username = user.clone();
                    self.cfg.lastfm.enabled = true;
                    self.save_config();
                    self.lastfm = make_lastfm(&self.cfg, &self.http, &self.paths);
                    self.set_account(|f| &mut f.lastfm, AccountStatus::Connected(user));
                }
                Err(e) => self.set_account(|f| &mut f.lastfm, AccountStatus::Error(format!("{e:#}"))),
            },
        }
    }
}

fn make_lastfm(cfg: &Config, http: &reqwest::Client, paths: &Paths) -> Option<Arc<Lastfm>> {
    let l = &cfg.lastfm;
    if l.api_key.trim().is_empty() || l.api_secret.trim().is_empty() {
        return None;
    }
    let lfm = Lastfm::new(
        http.clone(),
        l.api_key.trim(),
        l.api_secret.trim(),
        if l.enabled { l.session_key.trim() } else { "" },
        paths.data_dir.join("scrobble-queue.json"),
    );
    Some(Arc::new(lfm))
}

/// Imports playlists and Liked Songs through the playback session (not rate limited like the
/// public Web API). Only tracks we don't know yet get their metadata fetched.
async fn spotify_sync_internal(
    session: librespot_core::session::Session,
    known: HashSet<String>,
    progress: impl Fn(String),
) -> Result<SpotifySync> {
    use crate::providers::spotify_internal as si;
    use futures_util::StreamExt;

    /// Spotify requests that hang shouldn't stall the sync forever.
    async fn timed<T>(what: &str, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        tokio::time::timeout(Duration::from_secs(45), fut)
            .await
            .map_err(|_| anyhow!("timed out {what}"))?
    }

    progress("Loading your playlists…".into());
    let user = tokio::time::timeout(Duration::from_secs(15), si::display_name(&session))
        .await
        .unwrap_or_else(|_| session.username());
    let ids = timed("loading your playlists", si::rootlist(&session)).await?;
    let total = ids.len();
    let mut lists = Vec::with_capacity(total);
    {
        let mut results = futures_util::stream::iter(ids)
            .map(|id| {
                let session = session.clone();
                async move {
                    let result = timed("loading a playlist", si::playlist(&session, &id)).await;
                    (id, result)
                }
            })
            .buffered(4);
        let mut done = 0;
        while let Some((id, result)) = results.next().await {
            done += 1;
            progress(format!("Loading playlists… {done}/{total}"));
            match result {
                Ok(p) => lists.push(p),
                Err(e) => tracing::warn!("skipping Spotify playlist {id}: {e:#}"),
            }
        }
    }
    progress("Loading Liked Songs…".into());
    let liked = timed("loading Liked Songs", si::liked(&session))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("Spotify Liked Songs: {e:#}");
            Vec::new()
        });

    let mut need = Vec::new();
    let mut seen = HashSet::new();
    let all = lists
        .iter()
        .flat_map(|p| p.items.iter().map(|(u, _)| u))
        .chain(liked.iter().map(|(u, _)| u));
    for uri in all {
        if uri.starts_with("spotify:track:") && !known.contains(uri) && seen.insert(uri.as_str()) {
            need.push(uri.clone());
        }
    }
    let mut fetched: HashMap<String, Track> = HashMap::with_capacity(need.len());
    {
        let chunks: Vec<Vec<String>> = need.chunks(500).map(|c| c.to_vec()).collect();
        let mut batches = futures_util::stream::iter(chunks)
            .map(|chunk| {
                let session = session.clone();
                async move {
                    let result = timed("loading song details", si::tracks(&session, &chunk)).await;
                    (chunk.len(), result)
                }
            })
            .buffered(2);
        let mut done = 0;
        while let Some((n, result)) = batches.next().await {
            done += n;
            progress(format!("Loading song details… {done}/{}", need.len()));
            fetched.extend(result?);
        }
    }
    for (uri, at) in &liked {
        if let Some(t) = fetched.get_mut(uri) {
            t.added_at = *at;
        }
    }
    let have = |uri: &String| known.contains(uri) || fetched.contains_key(uri);
    let playlists = lists
        .into_iter()
        .map(|p| {
            let ids = p.items.iter().map(|(u, _)| u).filter(|u| have(u)).cloned().collect();
            (p.meta, Some(ids))
        })
        .collect();
    let liked_ids = liked.iter().map(|(u, _)| u).filter(|u| have(u)).cloned().collect();
    Ok(SpotifySync {
        user,
        playlists,
        liked: liked_ids,
        tracks: fetched.into_values().collect(),
    })
}

/// Fallback import through the public Web API.
async fn spotify_sync_web(
    auth: &SpotifyAuth,
    api: &SpotifyApi,
    known: &HashMap<String, String>,
) -> Result<SpotifySync> {
    let mut token = auth.token().await?;
    let user = match api.me(&token).await {
        Ok(u) => u,
        Err(e) if spotify_api::is_unauthorized(&e) => {
            auth.invalidate();
            token = auth.token().await?;
            api.me(&token).await?
        }
        Err(e) => return Err(e),
    };
    let metas = api.playlists(&token).await?;
    let mut playlists = Vec::with_capacity(metas.len());
    let mut tracks = Vec::new();
    for meta in metas {
        if known.get(&meta.id) == Some(&meta.snapshot_id) && !meta.snapshot_id.is_empty() {
            playlists.push((meta, None));
            continue;
        }
        // Tokens last an hour; refresh between playlists for huge libraries.
        token = auth.token().await?;
        match api.playlist_tracks(&token, &meta.id).await {
            Ok(list) => {
                playlists.push((meta, Some(list.iter().map(|t| t.id.clone()).collect())));
                tracks.extend(list);
            }
            Err(e) => {
                tracing::warn!("skipping playlist {}: {e:#}", meta.name);
                playlists.push((meta, None));
            }
        }
    }
    let liked = api.liked_tracks(&auth.token().await?).await?;
    let liked_ids = liked.iter().map(|t| t.id.clone()).collect();
    tracks.extend(liked);
    Ok(SpotifySync {
        user: if user.display_name.is_empty() {
            user.id
        } else {
            user.display_name
        },
        playlists,
        liked: liked_ids,
        tracks,
    })
}

/// Turns Web API errors into something actionable.
fn friendly_spotify_error(e: &anyhow::Error) -> String {
    if spotify_api::error_status(e) == Some(429) || format!("{e:#}").contains("429") {
        "Spotify's shared Web API key is rate limited right now. For reliable search, add your own \
         Spotify app in Settings → Spotify → Advanced."
            .into()
    } else {
        format!("{e:#}")
    }
}

/// Finds a playable version of an imported (Apple Music) song on Spotify or SoundCloud.
struct Resolver {
    spotify: Option<(Arc<SpotifyAuth>, Arc<SpotifyApi>)>,
    soundcloud: Option<Arc<SoundCloud>>,
}

impl Resolver {
    async fn resolve(&self, track: &Track) -> Option<Track> {
        let first_artist = track.artist.split([',', '&']).next().unwrap_or("").trim();
        let query = format!("{} {}", first_artist, track.title);
        if let Some((auth, api)) = &self.spotify {
            if let Ok(token) = auth.token().await {
                if let Ok(results) = api.search(&token, &query, 10).await {
                    if let Some(t) = best_match(track, &results) {
                        return Some(t);
                    }
                }
            }
        }
        if let Some(sc) = &self.soundcloud {
            if let Ok(results) = sc.search(&query, 10).await {
                if let Some(t) = best_match(track, &results) {
                    return Some(t);
                }
            }
        }
        None
    }
}

/// Picks the candidate that is clearly the same song (title + artist, close duration).
pub fn best_match(target: &Track, candidates: &[Track]) -> Option<Track> {
    let title = normalize_title(&target.title);
    let artist = normalize_artist(&target.artist);
    candidates
        .iter()
        .filter_map(|c| {
            let ct = normalize_title(&c.title);
            let ca = normalize_artist(&c.artist);
            let title_ok = ct == title || (!title.is_empty() && (ct.contains(&title) || title.contains(&ct)));
            // SoundCloud uploads often put the artist in the title instead.
            let artist_ok = ca == artist
                || (!artist.is_empty() && (ca.contains(&artist) || artist.contains(&ca) || ct.contains(&artist)));
            if !title_ok || !artist_ok {
                return None;
            }
            let dur_diff = if target.duration_ms > 0 && c.duration_ms > 0 {
                (target.duration_ms as i64 - c.duration_ms as i64).unsigned_abs()
            } else {
                0
            };
            if dur_diff > 10_000 {
                return None;
            }
            let exact = (ct == title) as u64 + (ca == artist) as u64;
            Some((c, (2 - exact) * 100_000 + dur_diff))
        })
        .min_by_key(|(_, score)| *score)
        .map(|(c, _)| c.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(source: Source, artist: &str, title: &str, dur: u64) -> Track {
        Track {
            id: format!("{}:{artist}{title}", source.as_str()),
            source,
            title: title.into(),
            artist: artist.into(),
            album: String::new(),
            duration_ms: dur,
            track_no: None,
            art: None,
            uri: String::new(),
            added_at: 0,
        }
    }

    #[test]
    fn best_match_prefers_exact_and_close_duration() {
        let target = track(
            Source::AppleMusic,
            "Daft Punk",
            "Get Lucky (feat. Pharrell Williams)",
            248_000,
        );
        let candidates = vec![
            track(Source::Spotify, "Daft Punk", "Get Lucky - Radio Edit", 250_000),
            track(Source::Spotify, "Daft Punk, Pharrell Williams", "Get Lucky", 248_500),
            track(Source::Spotify, "Some Cover Band", "Get Lucky", 248_000),
        ];
        let m = best_match(&target, &candidates).unwrap();
        assert_eq!(m.artist, "Daft Punk, Pharrell Williams");
    }

    #[test]
    fn best_match_rejects_wrong_songs() {
        let target = track(Source::AppleMusic, "Adele", "Hello", 295_000);
        let candidates = vec![
            track(Source::Spotify, "Lionel Richie", "Hello", 250_000),
            track(Source::Spotify, "Adele", "Hello", 400_000),
        ];
        assert!(best_match(&target, &candidates).is_none());
    }

    #[test]
    fn position_extrapolates_only_when_playing() {
        let mut pv = PlayerView {
            position: 10.0,
            position_at: Instant::now() - Duration::from_secs(2),
            duration: 11.0,
            status: PlayStatus::Paused,
            ..Default::default()
        };
        assert_eq!(pv.position_now(), 10.0);
        pv.status = PlayStatus::Playing;
        assert_eq!(pv.position_now(), 11.0); // clamped to duration
    }
}
