//! The background service: owns playback, the database and every network integration.
//! The UI talks to it with [`Command`]s and reads state from [`Shared`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::{Config, Paths};
use crate::downloader::{self, Downloader, Saved};
use crate::integrations::discord::{Discord, Presence};
use crate::integrations::genius;
use crate::integrations::lastfm::{Lastfm, ScrobbleTracker};
use crate::integrations::lyrics::LyricsFetcher;
use crate::integrations::mpris::Mpris;
use crate::library::{self, liked_playlist, Db, Library, LIKED_ID};
use crate::links::{self, LinkKind, Target};
use crate::model::{
    normalize_artist, normalize_title, now_unix, ArtistHit, AudioQuality, ImportedPlaylist, Lyrics, Playlist,
    PlaylistKind, RepeatMode, Source, Track,
};
use crate::player::mpv::{Mpv, MpvEvent, MpvOptions, MpvSender};
use crate::player::queue::Queue;
use crate::player::spotify::{self, SpotifyAuth, SpotifyDeck, SpotifyEngine, SpotifyEvent, SpotifySender};
use crate::providers::apple_music::{self, AppleMusicApi};
use crate::providers::soundcloud::{self, ScResolved, ScUser, SoundCloud};
use crate::providers::spotify_api::{self, SpotifyApi, SpotifyPlaylistMeta};
use crate::providers::youtube::YtDlp;

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
    /// Removes rows of a playlist: each one's position and song (the song wins when the
    /// position no longer holds it).
    RemoveFromPlaylist {
        playlist_id: String,
        rows: Vec<(usize, String)>,
    },
    /// Adds the songs behind pasted links (Spotify or SoundCloud songs, albums and playlists,
    /// or music files), in order.
    AddLinksToPlaylist {
        playlist_id: String,
        links: Vec<String>,
    },
    RenamePlaylist {
        playlist_id: String,
        name: String,
    },
    DeletePlaylist(String),
    /// Shows a message (for things the UI does on its own, like copying songs).
    Notify(String),
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
    /// Load an artist / album / playlist / song page: a page key or a pasted link.
    OpenPage(String),
    /// Save songs as files: SoundCloud songs directly, Spotify and Apple Music songs from a
    /// matching YouTube or SoundCloud upload.
    Download(Vec<Track>),
    /// Remove finished and failed entries from the Downloads list.
    ClearDownloads,
    /// Stop a waiting or running download (by track id).
    CancelDownload(String),
    /// Stop every waiting and running download.
    CancelDownloads,
    /// Find out whether this yt-dlp program runs (answer in `Feed::ytdlp`).
    CheckYtDlp(String),
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
    /// Spotify artists and SoundCloud profiles, best matches first.
    pub artists: Vec<ArtistHit>,
    pub spotify_pending: bool,
    pub soundcloud_pending: bool,
    pub errors: Vec<String>,
}

impl SearchState {
    pub fn pending(&self) -> bool {
        self.spotify_pending || self.soundcloud_pending
    }
}

/// An artist / album / playlist / song page from one of the services.
#[derive(Debug, Clone, Default)]
pub struct PageState {
    /// The page key or link it was opened with.
    pub key: String,
    pub loading: bool,
    pub error: Option<String>,
    /// "Artist", "Album", "Playlist" or "Song".
    pub kind: String,
    pub source: Option<Source>,
    pub title: String,
    pub subtitle: String,
    pub image: Option<String>,
    /// Artist pictures are drawn round.
    pub round: bool,
    pub tracks: Vec<Track>,
    /// The page on the service's website.
    pub external_url: Option<String>,
    /// Still adding the artist's songs from this other service.
    pub merging: Option<Source>,
}

/// One song in the Downloads list.
#[derive(Debug, Clone)]
pub struct DownloadItem {
    pub track: Track,
    pub state: DownloadState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DownloadState {
    Queued,
    /// Progress 0..=1.
    Running(f32),
    /// `from`: where the audio came from ("original file", "YouTube", …).
    Done {
        path: PathBuf,
        from: String,
    },
    Failed(String),
}

impl DownloadState {
    pub fn active(&self) -> bool {
        matches!(self, DownloadState::Queued | DownloadState::Running(_))
    }
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
    pub page: PageState,
    pub lyrics: LyricsState,
    /// Downloads started this session, oldest first.
    pub downloads: Vec<DownloadItem>,
    /// Downloaded songs: track id → file.
    pub downloaded: HashMap<String, PathBuf>,
    /// The yt-dlp program last checked, and its version or why it doesn't run.
    pub ytdlp: Option<(String, Result<String, String>)>,
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
    LinksResolved {
        playlist_id: String,
        tracks: Vec<Track>,
        failed: usize,
    },
    ScanDone(library::scanner::ScanResult),
    Lyrics {
        track_id: String,
        lyrics: Option<Lyrics>,
    },
    SearchResults {
        query: String,
        source: Source,
        tracks: Result<Vec<Track>>,
        artists: Vec<ArtistHit>,
    },
    /// The client_id SoundCloud accepted, saved so the next start needn't scrape one.
    SoundCloudClientId(String),
    PageLoaded {
        key: String,
        result: Result<PageState>,
    },
    /// A short link was expanded to `url`.
    ShortLink {
        key: String,
        result: Result<String>,
    },
    LastfmSession(Result<(String, String)>),
    Quality {
        track_id: String,
        quality: Option<AudioQuality>,
    },
    AudioDevices(Vec<(String, String)>),
    DownloadDone {
        track: Track,
        result: Result<Saved>,
    },
    YtDlpChecked {
        program: String,
        result: Result<String, String>,
    },
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

/// Bumped when the scanner learns to read more from files, so the library is read once again.
const SCAN_VERSION: &str = "2";

/// How often crossfade timing is checked and volumes are stepped.
const FADE_STEP: Duration = Duration::from_millis(50);
/// Longest crossfade offered in the settings.
const MAX_CROSSFADE: f32 = 12.0;
/// Shortest fade, used when the previous track is almost over.
const MIN_FADE: f64 = 0.3;

/// The previous track during a crossfade, fading out on its own player.
struct Fading {
    deck: Deck,
    /// When that track runs out.
    ends_at: Instant,
    /// Start and length of the fade-out, set once the next track is audible.
    ramp: Option<(Instant, f64)>,
}

/// A player taken out of service to fade out.
enum Deck {
    Mpv(Box<Mpv>),
    Spotify(SpotifyDeck),
}

impl Deck {
    async fn set_volume(&self, volume: f32) {
        match self {
            Deck::Mpv(mpv) => {
                let _ = mpv.set_volume(volume).await;
            }
            Deck::Spotify(deck) => deck.set_volume(volume),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FadeIn {
    /// The next track is loading; it starts silent.
    Waiting,
    Ramping {
        start: Instant,
        len: f64,
    },
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
    /// An idle mpv kept for the next crossfade.
    mpv_spare: Option<Mpv>,
    mpv_next_id: u64,
    mpv_tx: MpvSender,
    mpv_preloaded: Option<String>,
    /// The previous track fading out on its own player during a crossfade.
    fading: Option<Fading>,
    /// The current track's fade-in during a crossfade.
    fade_in: Option<FadeIn>,

    spotify_auth: Arc<SpotifyAuth>,
    /// Optional login with the user's own developer app, for Web API calls.
    spotify_web_auth: Option<Arc<SpotifyAuth>>,
    spotify: Option<SpotifyEngine>,
    spotify_connecting: bool,
    /// A sync was requested before the session was connected.
    spotify_sync_pending: bool,
    spotify_syncing: bool,
    spotify_tx: SpotifySender,
    spotify_api: Arc<SpotifyApi>,
    /// A Spotify page waiting for the session to connect.
    spotify_pending_page: Option<String>,
    /// Recently loaded pages, so going back doesn't reload them.
    page_cache: Vec<PageState>,
    /// Apple Music developer token (from the settings, or scraped once).
    apple_dev_token: Arc<tokio::sync::Mutex<Option<String>>>,

    soundcloud: Arc<SoundCloud>,
    lyrics: Arc<LyricsFetcher>,
    lastfm: Option<Arc<Lastfm>>,
    scrobble: ScrobbleTracker,
    discord: Discord,
    mpris: Mpris,
    last_tick: Instant,

    /// Running downloads by track id: their task and work folder.
    download_jobs: HashMap<String, (tokio::task::AbortHandle, PathBuf)>,
    download_seq: u64,
    /// Title and error (if any) of each download finished since the queue was last empty.
    download_batch: Vec<(String, Option<String>)>,
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

type MpvReceiver = UnboundedReceiver<(u64, MpvEvent)>;
type SpotifyReceiver = UnboundedReceiver<(u64, SpotifyEvent)>;
type Receivers = (MpvReceiver, SpotifyReceiver, UnboundedReceiver<Internal>);
type Started = (Service, MpvReceiver, SpotifyReceiver, UnboundedReceiver<Internal>);

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
        let lyrics = Arc::new(
            LyricsFetcher::new(http.clone(), paths.lyrics_cache(), cfg.lyrics.online).with_genius(genius::shared()),
        );
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
            mpv_spare: None,
            mpv_next_id: 1,
            mpv_tx,
            mpv_preloaded: None,
            fading: None,
            fade_in: None,
            spotify_auth,
            spotify_web_auth,
            spotify: None,
            spotify_connecting: false,
            spotify_sync_pending: false,
            spotify_syncing: false,
            spotify_tx,
            spotify_api: Arc::new(SpotifyApi::new(http)),
            spotify_pending_page: None,
            page_cache: Vec::new(),
            apple_dev_token: Arc::new(tokio::sync::Mutex::new(None)),
            soundcloud,
            lyrics,
            lastfm,
            scrobble: {
                let mut tracker = ScrobbleTracker::new();
                tracker.set_instant(cfg.lastfm.scrobble_instantly);
                tracker
            },
            discord,
            mpris,
            last_tick: Instant::now(),
            download_jobs: HashMap::new(),
            download_seq: 0,
            download_batch: Vec::new(),
            cfg,
        };
        let (a, b, c) = receivers;
        Ok((svc, a, b, c))
    }

    async fn run(
        mut self,
        mut cmd_rx: UnboundedReceiver<Command>,
        mut mpv_rx: MpvReceiver,
        mut sp_rx: SpotifyReceiver,
        mut int_rx: UnboundedReceiver<Internal>,
    ) {
        self.startup();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Crossfade timing and volume ramps; only runs while one could happen.
        let mut fade_tick = tokio::time::interval(FADE_STEP);
        fade_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let fade_active = self.fade_watch_needed();
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    if matches!(cmd, Command::Quit) {
                        break;
                    }
                    self.handle(cmd).await;
                }
                Some((id, ev)) = mpv_rx.recv() => self.on_mpv(id, ev).await,
                Some((id, ev)) = sp_rx.recv() => self.on_spotify(id, ev).await,
                Some(msg) = int_rx.recv() => self.on_internal(msg).await,
                _ = tick.tick() => self.on_tick().await,
                _ = fade_tick.tick(), if fade_active => self.on_fade_tick().await,
            }
        }
        self.shutdown().await;
    }

    fn startup(&mut self) {
        self.publish_accounts();
        self.check_app_secret();
        self.load_downloads();
        self.clear_download_leftovers();
        self.restore_session();
        if self.cfg.library.scan_on_startup && !self.cfg.library.folders.is_empty() {
            self.start_scan();
        }
        if self.cfg.spotify.enabled && self.spotify_auth.has_login() {
            self.connect_spotify();
            self.start_spotify_sync();
        }
        if self.cfg.soundcloud.enabled {
            self.warm_up_soundcloud();
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
        self.cancel_fades().await;
        if let Some(sp) = self.spotify.take() {
            sp.shutdown();
        }
        if let Some(mpv) = self.mpv.take() {
            mpv.quit().await;
        }
        if let Some(mpv) = self.mpv_spare.take() {
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
                // During a crossfade the ramps apply the new volume on their next step.
                if self.fade_in.is_none() {
                    if let Some(mpv) = &self.mpv {
                        let _ = mpv.set_volume(v).await;
                    }
                    if let Some(sp) = &self.spotify {
                        sp.set_volume(v);
                    }
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
            Command::AddToPlaylist { playlist_id, tracks } => self.add_to_playlist(&playlist_id, &tracks),
            Command::AddLinksToPlaylist { playlist_id, links } => self.add_links_to_playlist(playlist_id, links),
            Command::RemoveFromPlaylist { playlist_id, mut rows } => {
                let updated = {
                    let mut lib = self.shared.library.write().unwrap();
                    lib.playlist_mut(&playlist_id).and_then(|p| {
                        let before = p.track_ids.len();
                        // Last rows first, so the earlier positions still hold.
                        rows.sort_by_key(|row| std::cmp::Reverse(row.0));
                        for (index, id) in rows {
                            let at = if p.track_ids.get(index) == Some(&id) {
                                Some(index)
                            } else {
                                p.track_ids.iter().position(|t| *t == id)
                            };
                            if let Some(at) = at {
                                p.track_ids.remove(at);
                            }
                        }
                        (p.track_ids.len() != before).then(|| p.clone())
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
            Command::Notify(text) => self.shared.info(text),
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
                self.cancel_fades().await;
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
            Command::OpenPage(key) => self.open_page(key),
            Command::Download(tracks) => self.download(tracks),
            Command::ClearDownloads => {
                self.shared.feed.write().unwrap().downloads.retain(|d| d.state.active());
                self.shared.repaint();
            }
            Command::CancelDownload(id) => self.cancel_downloads(Some(&id)),
            Command::CancelDownloads => self.cancel_downloads(None),
            Command::CheckYtDlp(program) => {
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let result = YtDlp::new(&program, "").version().await.map_err(|e| {
                        if e.is::<crate::providers::youtube::NotInstalled>() {
                            "not installed".to_string()
                        } else {
                            format!("{e:#}")
                        }
                    });
                    let _ = tx.send(Internal::YtDlpChecked { program, result });
                });
            }
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
    /// Starts the queue's current track, cutting a crossfade short (skips, new playlists).
    async fn play_current(&mut self, start: f64) {
        self.cancel_fades().await;
        self.load_current(start).await;
    }

    /// Loads the queue's current track. During a crossfade it starts silent and fades in.
    async fn load_current(&mut self, start: f64) {
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
        // Local files, and downloaded songs from any service, play from the file.
        if let Some(file) = self.file_of(&track) {
            self.start_mpv(track, &file, start).await;
            return;
        }
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

    /// Login for Web API calls about the user's own library (likes, playlists): an app token
    /// from a Client secret can't do those.
    fn user_web_auth(&self) -> Option<Arc<SpotifyAuth>> {
        if !self.cfg.spotify.enabled {
            return None;
        }
        match &self.spotify_web_auth {
            Some(a) if a.has_user_login() => Some(a.clone()),
            _ => self.spotify_auth.has_login().then(|| self.spotify_auth.clone()),
        }
    }

    /// Spotify search, for finding a SoundCloud upload's release there.
    fn spotify_search(&self) -> Option<(Arc<SpotifyAuth>, Arc<SpotifyApi>)> {
        self.web_auth().map(|auth| (auth, self.spotify_api.clone()))
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
        if let Some(spare) = self.mpv_spare.take() {
            self.mpv = Some(spare);
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
        let id = self.mpv_next_id;
        self.mpv_next_id += 1;
        self.mpv = Some(Mpv::spawn(&opts, id, self.mpv_tx.clone()).await?);
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
        let volume = self.start_volume();
        if let Some(mpv) = &self.mpv {
            let _ = mpv.set_volume(volume).await;
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
        let volume = self.start_volume();
        let Some(sp) = self.spotify.as_mut() else { return };
        if let Err(e) = sp.ensure_session(&auth).await {
            self.load_failed(format!("Spotify: {e:#}")).await;
            return;
        }
        self.engine = Engine::Spotify;
        self.playing = Some(track.clone());
        sp.set_volume(volume);
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
            // A crossfade in progress carries on into the next track.
            Box::pin(self.load_current(0.0)).await;
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
        if self.fade_in == Some(FadeIn::Waiting) {
            self.start_fade_ramps();
        }
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
        // In instant mode this scrobbles right away.
        self.scrobble_if_due();
        if let Some(lfm) = self.lastfm.clone() {
            let t = track.clone();
            let spotify = self.spotify_search();
            tokio::spawn(async move {
                let t = lastfm_track(spotify, t).await;
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
        // Local files and downloaded songs: the file's own format.
        if let Some(file) = self.file_of(&t) {
            let tx = self.internal_tx.clone();
            tokio::task::spawn_blocking(move || {
                let quality = library::quality::read(std::path::Path::new(&file));
                let _ = tx.send(Internal::Quality {
                    track_id: t.id,
                    quality,
                });
            });
            return;
        }
        match t.source {
            Source::Local => {}
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
        self.cancel_fades().await;
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
        self.cancel_fades().await;
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
        self.cancel_fades().await;
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
        // A crossfade starts the next track on another mpv instead.
        if self.engine != Engine::Mpv || !self.cfg.playback.gapless || self.will_crossfade() {
            return;
        }
        // Local files and downloaded songs; streams are resolved when they start.
        let next = self
            .queue
            .peek_next()
            .and_then(|t| Some((t.id.clone(), self.file_of(t)?)));
        let Some(mpv) = &self.mpv else { return };
        let want = next.as_ref().map(|(id, _)| id.clone());
        if want == self.mpv_preloaded {
            return;
        }
        let _ = mpv.clear_upcoming().await;
        self.mpv_preloaded = None;
        if let Some((id, file)) = next {
            if mpv.append(&file).await.is_ok() {
                self.mpv_preloaded = Some(id);
            }
        }
    }

    async fn on_mpv(&mut self, id: u64, ev: MpvEvent) {
        if self.mpv.as_ref().map(Mpv::id) != Some(id) {
            // The instance fading out or the spare one: only its exit matters.
            if ev == MpvEvent::Died {
                if self.mpv_spare.as_ref().map(Mpv::id) == Some(id) {
                    self.mpv_spare = None;
                }
                if matches!(&self.fading, Some(Fading { deck: Deck::Mpv(m), .. }) if m.id() == id) {
                    self.fading = None;
                }
            }
            return;
        }
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
                    self.scrobble.set_duration_if_unknown((d * 1000.0) as u64);
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

    async fn on_spotify(&mut self, id: u64, ev: SpotifyEvent) {
        // Events of a player that is fading out (or idle) are stale.
        if self.engine != Engine::Spotify || self.spotify.as_ref().map(SpotifyEngine::current_id) != Some(id) {
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
                if self.will_crossfade() {
                    // The next track goes to another player; preloading here wouldn't help.
                    return;
                }
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
        self.scrobble_if_due();
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

    /// Sends the current track to Last.fm once it qualifies (see [`ScrobbleTracker`]).
    fn scrobble_if_due(&mut self) {
        if !self.scrobble.should_scrobble() {
            return;
        }
        self.scrobble.mark_scrobbled();
        if let (Some(lfm), Some((_, started))) = (self.lastfm.clone(), self.scrobble.current()) {
            if let Some(t) = self.queue.current().cloned() {
                let shared = self.shared.clone();
                let spotify = self.spotify_search();
                tokio::spawn(async move {
                    let t = lastfm_track(spotify, t).await;
                    match lfm.scrobble(&t, started).await {
                        Ok(None) => {}
                        // Usually bad tags on a local file; silently missing scrobbles are worse.
                        Ok(Some(why)) => shared.error(format!("Not scrobbled: {why}")),
                        Err(e) => tracing::warn!("scrobble failed: {e:#}"),
                    }
                });
            }
        }
    }

    // ---------------------------------------------------------------- crossfade

    /// Crossfade length in seconds; 0 when off or not possible (exclusive output devices
    /// can't be opened twice).
    fn crossfade_len(&self) -> f64 {
        let p = &self.cfg.playback;
        if p.bit_perfect || p.audio_device.starts_with("alsa/hw:") {
            0.0
        } else {
            f64::from(p.crossfade.clamp(0.0, MAX_CROSSFADE))
        }
    }

    /// True if the current track will crossfade into the next one (rather than gapless).
    fn will_crossfade(&self) -> bool {
        crossfades_into(
            self.crossfade_len(),
            self.cfg.playback.crossfade_albums,
            self.queue.repeat,
            self.queue.current(),
            self.queue.peek_next(),
        )
    }

    /// Whether the fade timer has anything to do.
    fn fade_watch_needed(&self) -> bool {
        self.fading.is_some()
            || self.fade_in.is_some()
            || (self.status == PlayStatus::Playing && self.crossfade_len() > 0.0)
    }

    async fn on_fade_tick(&mut self) {
        self.maybe_begin_crossfade().await;
        self.step_fades().await;
    }

    /// Starts the next track when the current one is about to end.
    async fn maybe_begin_crossfade(&mut self) {
        if self.fading.is_some()
            || self.fade_in.is_some()
            || self.status != PlayStatus::Playing
            || !self.started_reported
            || self.engine == Engine::None
            || self.duration <= 0.0
            || !self.will_crossfade()
        {
            return;
        }
        let Some(next) = self.queue.peek_next() else { return };
        let len = self.crossfade_len().min(self.duration / 3.0);
        let remaining = self.duration - self.current_position();
        // Start a bit early so the next track has time to load; too close to the end,
        // just let it finish.
        if remaining > len + load_lead(next.source) || remaining < 1.0 {
            return;
        }
        self.begin_crossfade(remaining).await;
    }

    /// Moves the playing track to its own player to fade out and starts the next one silently.
    async fn begin_crossfade(&mut self, remaining: f64) {
        let deck = match self.engine {
            Engine::Mpv => match self.mpv.take() {
                Some(mpv) => {
                    // It must not continue into a preloaded file.
                    let _ = mpv.clear_upcoming().await;
                    Deck::Mpv(Box::new(mpv))
                }
                None => return,
            },
            Engine::Spotify => match self.spotify.as_mut().map(SpotifyEngine::detach_current) {
                Some(Ok(deck)) => Deck::Spotify(deck),
                Some(Err(e)) => {
                    tracing::warn!("crossfade: no second Spotify player: {e:#}");
                    return;
                }
                None => return,
            },
            Engine::None => return,
        };
        tracing::debug!("crossfade: {remaining:.1}s left, starting the next track");
        self.mpv_preloaded = None;
        self.engine = Engine::None;
        self.fading = Some(Fading {
            deck,
            ends_at: Instant::now() + Duration::from_secs_f64(remaining),
            ramp: None,
        });
        self.fade_in = Some(FadeIn::Waiting);
        if self.queue.advance(false).is_some() {
            self.load_current(0.0).await;
        } else {
            self.cancel_fades().await;
        }
        self.publish_queue();
    }

    /// The next track is audible: fade the previous one out and this one in.
    fn start_fade_ramps(&mut self) {
        let now = Instant::now();
        let max = self.crossfade_len();
        let len = match &mut self.fading {
            Some(f) => {
                let left = f.ends_at.saturating_duration_since(now).as_secs_f64();
                let len = max.min(left).max(MIN_FADE);
                f.ramp = Some((now, len));
                len
            }
            // The previous track already ended: just a short fade-in.
            None => MIN_FADE,
        };
        tracing::debug!("crossfade: fading over {len:.1}s");
        self.fade_in = Some(FadeIn::Ramping { start: now, len });
    }

    async fn step_fades(&mut self) {
        let volume = self.cfg.playback.volume;
        let now = Instant::now();
        let mut done = false;
        if let Some(f) = &self.fading {
            match f.ramp {
                Some((start, len)) => {
                    let t = fade_progress(start, len, now);
                    f.deck.set_volume(volume * (1.0 - ease(t))).await;
                    done = t >= 1.0;
                }
                // Nothing took over before the track ran out.
                None => done = now >= f.ends_at + Duration::from_secs(1),
            }
        }
        if done {
            self.finish_fading().await;
        }
        if let Some(FadeIn::Ramping { start, len }) = self.fade_in {
            let t = fade_progress(start, len, now);
            self.set_engine_volume(volume * ease(t)).await;
            if t >= 1.0 {
                self.fade_in = None;
            }
        }
    }

    /// Stops the player that faded out and keeps it for the next crossfade.
    async fn finish_fading(&mut self) {
        let Some(f) = self.fading.take() else { return };
        tracing::debug!("crossfade: done");
        match f.deck {
            Deck::Mpv(mpv) => {
                let _ = mpv.stop().await;
                if self.mpv_spare.is_none() {
                    self.mpv_spare = Some(*mpv);
                } else {
                    mpv.quit().await;
                }
            }
            Deck::Spotify(deck) => match self.spotify.as_mut() {
                Some(sp) => sp.return_deck(deck),
                None => deck.stop(),
            },
        }
    }

    /// Ends a crossfade right away: the previous track stops, the current one goes to full volume.
    async fn cancel_fades(&mut self) {
        self.finish_fading().await;
        if self.fade_in.take().is_some() {
            self.set_engine_volume(self.cfg.playback.volume).await;
        }
    }

    async fn set_engine_volume(&self, volume: f32) {
        match self.engine {
            Engine::Mpv => {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.set_volume(volume).await;
                }
            }
            Engine::Spotify => {
                if let Some(sp) = &self.spotify {
                    sp.set_volume(volume);
                }
            }
            Engine::None => {}
        }
    }

    /// Volume a newly loaded track starts at: silent while it is going to fade in.
    fn start_volume(&self) -> f32 {
        if self.fade_in.is_some() {
            0.0
        } else {
            self.cfg.playback.volume
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
            Some(a) if a.has_user_login() => AccountStatus::Connected(String::new()),
            Some(a) if a.uses_secret() => AccountStatus::Working("Checking your app's Client ID and secret…".into()),
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

        if let (Source::Spotify, Some(auth)) = (track.source, self.user_web_auth()) {
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
        let mut known = self.db.local_mtimes().unwrap_or_default();
        // Files read by an older version are read again once (it now guesses more from names).
        if self.db.get_kv("scan_version").as_deref() != Some(SCAN_VERSION) {
            known.values_mut().for_each(|mtime| *mtime = i64::MIN);
        }
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
        let _ = self.db.set_kv("scan_version", SCAN_VERSION);
        {
            let mut lib = self.shared.library.write().unwrap();
            for id in &result.removed {
                lib.tracks.remove(id);
            }
            for mut t in result.changed {
                // As in the database: a file keeps the date it was first added.
                if let Some(old) = lib.tracks.get(&t.id).filter(|old| old.added_at != 0) {
                    t.added_at = old.added_at;
                }
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
        let Some(session) = self.spotify_session() else {
            self.spotify_sync_pending = true;
            self.set_account(|f| &mut f.spotify, AccountStatus::Working("Connecting…".into()));
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
        let web = self.user_web_auth().map(|a| (a, self.spotify_api.clone()));
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
        let Some(auth) = self.user_web_auth() else { return };
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

    /// With a Client secret, fetches an app token right away so Settings can say whether the
    /// ID and secret work.
    fn check_app_secret(&self) {
        let Some(auth) = self
            .spotify_web_auth
            .clone()
            .filter(|a| a.uses_secret() && !a.has_user_login())
        else {
            return;
        };
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let status = match auth.token().await {
                Ok(_) => AccountStatus::Connected("your app (Client secret) for search and pages".into()),
                Err(e) => AccountStatus::Error(format!("{e:#}")),
            };
            shared.feed.write().unwrap().spotify_web_api = status;
            shared.repaint();
        });
    }

    fn spotify_web_api_login(&mut self) {
        let Some(auth) = self.spotify_web_auth.clone() else {
            self.shared
                .error("Enter your Spotify app's client ID in Settings → Spotify → Advanced first");
            return;
        };
        // Show exactly what goes to Spotify, so a mismatch with the app's settings is visible.
        self.set_account(
            |f| &mut f.spotify_web_api,
            AccountStatus::Working(format!(
                "Waiting for browser login… (sent Client ID {} and Redirect URI {})",
                auth.client_id(),
                auth.redirect()
            )),
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

    /// Searches Spotify and SoundCloud at the same time; each shows up as soon as it answers.
    fn search(&mut self, query: String) {
        let q = query.trim().to_string();
        let spotify = (!q.is_empty())
            .then(|| self.web_auth().map(|auth| (auth, self.spotify_api.clone())))
            .flatten();
        let sc = (!q.is_empty() && self.cfg.soundcloud.enabled).then(|| self.soundcloud.clone());
        self.shared.feed.write().unwrap().search = SearchState {
            query: q.clone(),
            spotify_pending: spotify.is_some(),
            soundcloud_pending: sc.is_some(),
            ..Default::default()
        };
        self.shared.repaint();
        if let Some((auth, api)) = spotify {
            let tx = self.internal_tx.clone();
            let q = q.clone();
            tokio::spawn(async move {
                let search = async { api.search_with_artists(&auth.token().await?, &q, 20).await };
                // Never leave the results spinning.
                let r = tokio::time::timeout(Duration::from_secs(15), search)
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("Spotify didn't answer in time")));
                let (tracks, artists) = match r {
                    Ok((tracks, artists)) => (Ok(tracks), artists),
                    Err(e) => (Err(e), Vec::new()),
                };
                let _ = tx.send(Internal::SearchResults {
                    query: q,
                    source: Source::Spotify,
                    tracks,
                    artists,
                });
            });
        }
        if let Some(sc) = sc {
            let tx = self.internal_tx.clone();
            tokio::spawn(async move {
                let (tracks, users) = tokio::join!(sc.search(&q, 20), sc.search_users(&q, 8));
                let artists = users.unwrap_or_else(|e| {
                    tracing::warn!("SoundCloud artist search: {e:#}");
                    Vec::new()
                });
                let ok = tracks.is_ok();
                let _ = tx.send(Internal::SearchResults {
                    query: q,
                    source: Source::SoundCloud,
                    tracks,
                    artists,
                });
                if ok {
                    if let Ok(id) = sc.client_id().await {
                        let _ = tx.send(Internal::SoundCloudClientId(id));
                    }
                }
            });
        }
    }

    /// Seeds SoundCloud's client_id from the last run and checks it in the background, so the
    /// first search doesn't have to scrape soundcloud.com first.
    fn warm_up_soundcloud(&mut self) {
        let saved = self.db.get_kv("soundcloud_client_id").unwrap_or_default();
        let sc = self.soundcloud.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            sc.seed_client_id(&saved).await;
            if saved.is_empty() {
                match sc.client_id().await {
                    Ok(id) => {
                        let _ = tx.send(Internal::SoundCloudClientId(id));
                    }
                    Err(e) => tracing::debug!("SoundCloud warm-up: {e:#}"),
                }
            }
        });
    }

    // ---------------------------------------------------------------- playlists

    fn add_to_playlist(&mut self, playlist_id: &str, tracks: &[Track]) {
        self.store_tracks(tracks);
        let updated = {
            let mut lib = self.shared.library.write().unwrap();
            lib.playlist_mut(playlist_id).map(|p| {
                p.track_ids.extend(tracks.iter().map(|t| t.id.clone()));
                if p.art.is_none() {
                    p.art = tracks.iter().find_map(|t| t.art.clone());
                }
                p.clone()
            })
        };
        if let Some(p) = updated {
            let songs = if tracks.len() == 1 {
                "1 song".to_string()
            } else {
                format!("{} songs", tracks.len())
            };
            self.shared.info(format!("Added {songs} to “{}”", p.name));
            self.save_playlists(vec![p], &[]);
        }
    }

    fn add_links_to_playlist(&mut self, playlist_id: String, links: Vec<String>) {
        use futures_util::StreamExt;
        let spotify_links = links.iter().any(|l| l.contains("spotify"));
        let session = (spotify_links && self.cfg.spotify.enabled && self.spotify_auth.has_login())
            .then(|| self.spotify_session())
            .flatten();
        let soundcloud = self.soundcloud.clone();
        let http = self.http.clone();
        let shared = self.shared.clone();
        let tx = self.internal_tx.clone();
        if links.len() > 1 {
            self.shared
                .info(format!("Adding the songs behind {} links…", links.len()));
        }
        tokio::spawn(async move {
            let lookups = links.into_iter().map(|link| {
                let (session, soundcloud, http, shared) =
                    (session.clone(), soundcloud.clone(), http.clone(), shared.clone());
                async move { songs_behind_link(&link, session.as_ref(), &soundcloud, &http, &shared).await }
            });
            let found: Vec<Option<Vec<Track>>> = futures_util::stream::iter(lookups).buffered(6).collect().await;
            let failed = found.iter().filter(|f| f.is_none()).count();
            let tracks = found.into_iter().flatten().flatten().collect();
            let _ = tx.send(Internal::LinksResolved {
                playlist_id,
                tracks,
                failed,
            });
        });
    }

    // ---------------------------------------------------------------- pages

    fn set_page(&self, page: PageState) {
        let mut feed = self.shared.feed.write().unwrap();
        if feed.page.key == page.key {
            feed.page = page;
        }
        drop(feed);
        self.shared.repaint();
    }

    fn page_failed(&self, key: &str, error: impl Into<String>) {
        self.set_page(PageState {
            key: key.to_string(),
            error: Some(error.into()),
            ..Default::default()
        });
    }

    fn open_page(&mut self, key: String) {
        let key = key.trim().to_string();
        if let Some(cached) = self.page_cache.iter().find(|p| p.key == key) {
            self.shared.feed.write().unwrap().page = cached.clone();
            self.shared.repaint();
            return;
        }
        self.shared.feed.write().unwrap().page = PageState {
            key: key.clone(),
            loading: true,
            ..Default::default()
        };
        self.shared.repaint();
        match links::target(&key) {
            Some(target) => self.load_page(key, target),
            None => self.page_failed(&key, "That isn't a Spotify, SoundCloud or Apple Music link"),
        }
    }

    fn load_page(&mut self, key: String, target: Target) {
        let tx = self.internal_tx.clone();
        let send = move |key: String, result: Result<PageState>| {
            let _ = tx.send(Internal::PageLoaded { key, result });
        };
        match target {
            // The UI draws these straight from the library and never asks the service.
            Target::LocalArtist(_) => self.page_failed(&key, "Open library artists from Your Library"),
            Target::ArtistName(name) => {
                let soundcloud = self.cfg.soundcloud.enabled.then(|| self.soundcloud.clone());
                let spotify = self.spotify_lookup();
                tokio::spawn(async move {
                    let (on_spotify, on_soundcloud) = tokio::join!(
                        async {
                            match &spotify {
                                Some(l) => spotify_artist(l, &name).await,
                                None => None,
                            }
                        },
                        async {
                            match &soundcloud {
                                Some(sc) => soundcloud_artist(sc, &name).await,
                                None => None,
                            }
                        },
                    );
                    let result = if on_spotify.is_none() && on_soundcloud.is_none() {
                        Err(anyhow!("{name} isn't on Spotify or SoundCloud under that name"))
                    } else {
                        let mut page = PageState {
                            kind: "Artist".into(),
                            title: name,
                            round: true,
                            ..Default::default()
                        };
                        for found in [on_spotify, on_soundcloud].into_iter().flatten() {
                            page.image = page.image.or(found.image);
                            page.external_url = page.external_url.or(Some(found.url));
                            page.source = page.source.or(Some(found.source));
                            add_songs(&mut page.tracks, found.tracks);
                        }
                        page.subtitle = artist_subtitle(&page.tracks);
                        Ok(page)
                    };
                    send(key, result);
                });
            }
            Target::Spotify(kind, id) => {
                if !self.cfg.spotify.enabled || !self.spotify_auth.has_login() {
                    self.page_failed(&key, "Log in to Spotify in Settings to open Spotify pages");
                    return;
                }
                let Some(session) = self.spotify_session() else {
                    self.spotify_pending_page = Some(key);
                    return;
                };
                // Artist pages also get the artist's SoundCloud uploads.
                let soundcloud =
                    (kind == LinkKind::Artist && self.cfg.soundcloud.enabled).then(|| self.soundcloud.clone());
                tokio::spawn(async move {
                    let result = tokio::time::timeout(Duration::from_secs(45), spotify_page(session, kind, &id))
                        .await
                        .unwrap_or_else(|_| Err(anyhow!("Spotify took too long to answer")));
                    match (result, soundcloud) {
                        (Ok(mut page), Some(sc)) => {
                            page.tracks = dedupe_songs(page.tracks);
                            page.merging = Some(Source::SoundCloud);
                            send(key.clone(), Ok(page.clone()));
                            if let Some(found) = soundcloud_artist(&sc, &page.title).await {
                                if add_songs(&mut page.tracks, found.tracks) > 0 {
                                    page.subtitle = artist_subtitle(&page.tracks);
                                }
                            }
                            page.merging = None;
                            send(key, Ok(page));
                        }
                        (result, _) => send(key, result),
                    }
                });
            }
            Target::SoundCloudUser(id) => {
                let sc = self.soundcloud.clone();
                let spotify = self.spotify_lookup();
                tokio::spawn(async move {
                    let result = async {
                        let (user, tracks) = tokio::join!(sc.user(id), sc.user_tracks(id));
                        Ok(soundcloud_user_page(&user?, tracks?))
                    }
                    .await;
                    with_spotify_songs(result, spotify, |page| send(key.clone(), page)).await;
                });
            }
            Target::SoundCloudUrl(url) => {
                let sc = self.soundcloud.clone();
                let spotify = self.spotify_lookup();
                tokio::spawn(async move {
                    let result = async {
                        Ok(match sc.resolve_url(&url).await? {
                            ScResolved::User(user) => {
                                let tracks = sc.user_tracks(user.id).await?;
                                soundcloud_user_page(&user, tracks)
                            }
                            ScResolved::Track(t) => PageState {
                                kind: "Song".into(),
                                source: Some(Source::SoundCloud),
                                title: t.title.clone(),
                                subtitle: t.artist.clone(),
                                image: t.art.clone(),
                                external_url: Some(url),
                                tracks: vec![t],
                                ..Default::default()
                            },
                            ScResolved::Playlist(p) => PageState {
                                kind: "Playlist".into(),
                                source: Some(Source::SoundCloud),
                                subtitle: format!("{} songs", p.tracks.len()),
                                title: p.name,
                                image: p.art,
                                external_url: Some(url),
                                tracks: p.tracks,
                                ..Default::default()
                            },
                        })
                    }
                    .await;
                    with_spotify_songs(result, spotify, |page| send(key.clone(), page)).await;
                });
            }
            Target::AppleMusic { kind, storefront, id } => {
                let http = self.http.clone();
                let configured = self.cfg.apple_music.developer_token.trim().to_string();
                let cache = self.apple_dev_token.clone();
                tokio::spawn(async move {
                    let result = async {
                        let token = {
                            let mut cached = cache.lock().await;
                            match (configured.is_empty(), cached.clone()) {
                                (false, _) => configured,
                                (true, Some(t)) => t,
                                (true, None) => {
                                    let t = apple_music::scrape_developer_token(&http).await?;
                                    *cached = Some(t.clone());
                                    t
                                }
                            }
                        };
                        let api = AppleMusicApi::new(http, &token, "", &storefront);
                        let page = match kind {
                            LinkKind::Artist => api.catalog_artist(&id).await,
                            LinkKind::Album => api.catalog_album(&id).await,
                            LinkKind::Playlist => api.catalog_playlist(&id).await,
                            LinkKind::Track => api.catalog_song(&id).await,
                        };
                        if page.as_ref().is_err_and(apple_music::is_auth_error) {
                            // A scraped token may have expired: scrape a new one next time.
                            *cache.lock().await = None;
                        }
                        let page = page?;
                        Ok(PageState {
                            kind: links::kind_label(kind).into(),
                            source: Some(Source::AppleMusic),
                            title: page.title,
                            subtitle: page.subtitle,
                            image: page.image,
                            round: kind == LinkKind::Artist,
                            tracks: page.tracks,
                            external_url: links::web_url(&format!(
                                "applemusic:{}:{storefront}:{id}",
                                links::kind_name(kind)
                            )),
                            ..Default::default()
                        })
                    }
                    .await;
                    send(key, result);
                });
            }
            Target::Short(url) => {
                let http = self.http.clone();
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let result = expand_short_link(&http, &url).await;
                    let _ = tx.send(Internal::ShortLink { key, result });
                });
            }
        }
    }

    // ---------------------------------------------------------------- downloads

    fn load_downloads(&mut self) {
        // Files that are gone (or on a drive that isn't mounted) are left out, not forgotten.
        let downloaded = self
            .db
            .kv_with_prefix(DOWNLOAD_KEY)
            .into_iter()
            .map(|(id, path)| (id, PathBuf::from(path)))
            .filter(|(_, path)| path.exists())
            .collect();
        self.shared.feed.write().unwrap().downloaded = downloaded;
    }

    /// The file to play for `track` when it is one: a local file or a downloaded song.
    fn file_of(&self, track: &Track) -> Option<String> {
        match track.source {
            Source::Local => Some(track.uri.clone()),
            _ => self.downloaded_file(track),
        }
    }

    fn downloaded_file(&self, track: &Track) -> Option<String> {
        let feed = self.shared.feed.read().unwrap();
        let path = feed.downloaded.get(&track.id)?;
        path.exists().then(|| path.to_string_lossy().into_owned())
    }

    fn download(&mut self, tracks: Vec<Track>) {
        let mut already = 0;
        let mut added: Vec<String> = Vec::new();
        {
            let mut feed = self.shared.feed.write().unwrap();
            let mut seen = HashSet::new();
            for track in tracks {
                // Local files are files already.
                if track.source == Source::Local || !seen.insert(track.id.clone()) {
                    continue;
                }
                if feed.downloaded.get(&track.id).is_some_and(|p| p.exists()) {
                    already += 1;
                    continue;
                }
                match feed.downloads.iter_mut().find(|d| d.track.id == track.id) {
                    Some(d) if d.state.active() => continue,
                    // Failed before, or the file was deleted since: try again.
                    Some(d) => d.state = DownloadState::Queued,
                    None => feed.downloads.push(DownloadItem {
                        track: track.clone(),
                        state: DownloadState::Queued,
                    }),
                }
                added.push(track.title);
            }
        }
        match added.as_slice() {
            [] if already == 1 => self.shared.info("Already downloaded"),
            [] if already > 1 => self.shared.info(format!("All {already} songs are already downloaded")),
            [] => {}
            [title] => self.shared.info(format!("Downloading “{title}”…")),
            many => self.shared.info(format!("Downloading {} songs…", many.len())),
        }
        self.pump_downloads();
    }

    /// Starts queued downloads, a couple at a time.
    fn pump_downloads(&mut self) {
        const PARALLEL: usize = 2;
        while self.download_jobs.len() < PARALLEL {
            let track = {
                let mut feed = self.shared.feed.write().unwrap();
                let Some(item) = feed.downloads.iter_mut().find(|d| d.state == DownloadState::Queued) else {
                    break;
                };
                item.state = DownloadState::Running(0.0);
                item.track.clone()
            };
            // Apple Music songs already matched to Spotify get Spotify's details.
            let spotify_twin = (track.source == Source::AppleMusic)
                .then(|| self.cached_resolution(&track))
                .flatten()
                .filter(|t| t.source == Source::Spotify);
            let wants_spotify = (track.source == Source::Spotify || spotify_twin.is_some())
                && self.cfg.spotify.enabled
                && self.spotify_auth.has_login();
            let spotify = if wants_spotify { self.spotify_session() } else { None };
            let d = &self.cfg.downloads;
            let downloader = Downloader {
                soundcloud: self.soundcloud.clone(),
                http: self.http.clone(),
                lyrics: d.lyrics.then(|| self.lyrics.clone()),
                spotify,
                spotify_twin,
                ytdlp: d
                    .youtube
                    .then(|| YtDlp::new(&d.ytdlp_path, &d.ytdlp_args).with_mp3(d.youtube_mp3)),
            };
            let dir = self.cfg.download_dir(track.source);
            self.download_seq += 1;
            let work = dir.join(downloader::work_dir_name(self.download_seq));
            let job_work = work.clone();
            let job_id = track.id.clone();
            let shared = self.shared.clone();
            let tx = self.internal_tx.clone();
            let task = tokio::spawn(async move {
                let id = track.id.clone();
                let last = std::sync::Mutex::new(Instant::now());
                let progress = |p: f32| {
                    {
                        let mut last = last.lock().unwrap();
                        if p < 1.0 && last.elapsed() < Duration::from_millis(150) {
                            return;
                        }
                        *last = Instant::now();
                    }
                    let mut feed = shared.feed.write().unwrap();
                    if let Some(d) = feed.downloads.iter_mut().find(|d| d.track.id == id) {
                        if let DownloadState::Running(old) = &mut d.state {
                            *old = p;
                        }
                    }
                    drop(feed);
                    shared.repaint();
                };
                let result = downloader.download(&track, &dir, &job_work, &progress).await;
                let _ = tx.send(Internal::DownloadDone { track, result });
            });
            self.download_jobs.insert(job_id, (task.abort_handle(), work));
        }
        self.shared.repaint();
    }

    fn download_finished(&mut self, track: Track, result: Result<Saved>) {
        self.download_jobs.remove(&track.id);
        let state = match result {
            Ok(Saved { path, from }) => {
                let _ = self
                    .db
                    .set_kv(&format!("{DOWNLOAD_KEY}{}", track.id), &path.to_string_lossy());
                self.shared
                    .feed
                    .write()
                    .unwrap()
                    .downloaded
                    .insert(track.id.clone(), path.clone());
                self.add_downloaded_file(&path);
                self.download_batch.push((track.title.clone(), None));
                DownloadState::Done { path, from }
            }
            Err(e) => {
                let error = friendly_download_error(&e);
                tracing::warn!("download of {} failed: {e:#}", track.id);
                self.download_batch.push((track.title.clone(), Some(error.clone())));
                DownloadState::Failed(error)
            }
        };
        let idle = {
            let mut feed = self.shared.feed.write().unwrap();
            if let Some(d) = feed.downloads.iter_mut().find(|d| d.track.id == track.id) {
                d.state = state;
            }
            !feed.downloads.iter().any(|d| d.state.active())
        };
        if idle {
            let batch = std::mem::take(&mut self.download_batch);
            let failed: Vec<_> = batch.iter().filter(|(_, e)| e.is_some()).collect();
            let ok = batch.len() - failed.len();
            match (ok, failed.as_slice()) {
                (0, []) => {}
                (1, []) => self.shared.info(format!("Downloaded “{}”", batch[0].0)),
                (n, []) => self.shared.info(format!("Downloaded {n} songs")),
                (0, [(title, Some(e))]) => self.shared.error(format!("Couldn't download “{title}”: {e}")),
                (0, f) => self
                    .shared
                    .error(format!("{} downloads failed (see Downloads)", f.len())),
                (n, f) => self
                    .shared
                    .error(format!("Downloaded {n} songs, {} failed (see Downloads)", f.len())),
            }
        }
        self.pump_downloads();
    }

    /// Stops the download of `id`, or all of them.
    fn cancel_downloads(&mut self, id: Option<&str>) {
        let cancelled: Vec<String> = {
            let mut feed = self.shared.feed.write().unwrap();
            let picked = |d: &DownloadItem| d.state.active() && id.is_none_or(|id| d.track.id == id);
            let ids = feed
                .downloads
                .iter()
                .filter(|d| picked(d))
                .map(|d| d.track.id.clone())
                .collect();
            feed.downloads.retain(|d| !picked(d));
            ids
        };
        for track_id in &cancelled {
            if let Some((task, work)) = self.download_jobs.remove(track_id) {
                task.abort();
                // yt-dlp / ffmpeg are killed with the task; give them a moment to let go of
                // the folder.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let _ = tokio::fs::remove_dir_all(&work).await;
                });
            }
        }
        if id.is_none() && !cancelled.is_empty() {
            self.shared.info(match cancelled.len() {
                1 => "Download cancelled".to_string(),
                n => format!("{n} downloads cancelled"),
            });
        }
        if !self
            .shared
            .feed
            .read()
            .unwrap()
            .downloads
            .iter()
            .any(|d| d.state.active())
        {
            self.download_batch.clear();
        }
        self.pump_downloads();
    }

    /// Clears work folders a crash or power cut left in the download folders.
    fn clear_download_leftovers(&self) {
        let mut dirs: Vec<PathBuf> = [Source::SoundCloud, Source::Spotify, Source::AppleMusic]
            .into_iter()
            .map(|s| self.cfg.download_dir(s))
            .collect();
        dirs.dedup();
        tokio::task::spawn_blocking(move || {
            for dir in dirs {
                let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(downloader::WORK_PREFIX) {
                        let path = entry.path();
                        let _ = if path.is_dir() {
                            std::fs::remove_dir_all(&path)
                        } else {
                            std::fs::remove_file(&path)
                        };
                    }
                }
            }
        });
    }

    /// Puts a finished download straight into the library when it was saved inside a library
    /// folder (instead of waiting for the next scan).
    fn add_downloaded_file(&mut self, path: &Path) {
        let Some(folder) = self.cfg.library.folders.iter().find(|f| path.starts_with(f)) else {
            return;
        };
        let track = library::scanner::read_track_in(path, Some(folder), None, now_unix());
        let mtimes = HashMap::from([(track.id.clone(), library::scanner::mtime_of(path))]);
        if let Err(e) = self.db.upsert_tracks(std::slice::from_ref(&track), &mtimes) {
            tracing::warn!("couldn't add {} to the library: {e:#}", path.display());
            return;
        }
        let mut lib = self.shared.library.write().unwrap();
        lib.tracks.insert(track.id.clone(), track);
        lib.reindex();
    }

    /// What it takes to look an artist up on Spotify by name (search, then their page through
    /// the session); `None` when Spotify isn't logged in or connected yet.
    fn spotify_lookup(&mut self) -> Option<SpotifyLookup> {
        if !self.cfg.spotify.enabled || !self.spotify_auth.has_login() {
            return None;
        }
        let auth = self.web_auth()?;
        let session = self.spotify_session()?;
        Some(SpotifyLookup {
            auth,
            api: self.spotify_api.clone(),
            session,
        })
    }

    /// The Spotify session, or `None` while (re)connecting.
    fn spotify_session(&mut self) -> Option<librespot_core::session::Session> {
        if let Some(session) = self.spotify.as_ref().and_then(|e| e.session()) {
            return Some(session);
        }
        if let Some(sp) = self.spotify.take() {
            // Session dropped: reconnect from scratch.
            sp.shutdown();
        }
        self.connect_spotify();
        None
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
            for mpv in self.mpv.iter().chain(self.mpv_spare.iter()) {
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
            if old.playback.mpv_path != self.cfg.playback.mpv_path {
                if self.engine != Engine::Mpv {
                    if let Some(mpv) = self.mpv.take() {
                        mpv.quit().await;
                    }
                }
                if let Some(mpv) = self.mpv_spare.take() {
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
            self.scrobble.set_instant(self.cfg.lastfm.scrobble_instantly);
        }
        if old.soundcloud != self.cfg.soundcloud {
            self.soundcloud = Arc::new(SoundCloud::new(
                self.http.clone(),
                &self.cfg.soundcloud.client_id,
                &self.cfg.soundcloud.oauth_token,
            ));
        }
        if old.lyrics != self.cfg.lyrics {
            self.lyrics = Arc::new(
                LyricsFetcher::new(self.http.clone(), self.paths.lyrics_cache(), self.cfg.lyrics.online)
                    .with_genius(genius::shared()),
            );
        }
        if old.spotify.web_api_client_id != self.cfg.spotify.web_api_client_id
            || old.spotify.web_api_redirect() != self.cfg.spotify.web_api_redirect()
            || old.spotify.web_api_client_secret != self.cfg.spotify.web_api_client_secret
        {
            self.spotify_web_auth = SpotifyAuth::web_api(&self.cfg.spotify, &self.paths.spotify_dir()).map(Arc::new);
            self.publish_accounts();
            self.check_app_secret();
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
                        if let Some(key) = self.spotify_pending_page.take() {
                            self.open_page(key);
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
                        if let Some(key) = self.spotify_pending_page.take() {
                            self.page_failed(&key, format!("Couldn't connect to Spotify: {e:#}"));
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
            Internal::LinksResolved {
                playlist_id,
                tracks,
                failed,
            } => {
                if !tracks.is_empty() {
                    self.add_to_playlist(&playlist_id, &tracks);
                }
                if failed > 0 {
                    let spotify = if self.spotify_auth.has_login() {
                        ""
                    } else {
                        " (Spotify links need a Spotify login)"
                    };
                    self.shared.error(format!(
                        "Couldn't find the songs behind {failed} pasted link{}{spotify}",
                        if failed == 1 { "" } else { "s" }
                    ));
                }
            }
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
            Internal::SearchResults {
                query,
                source,
                tracks,
                artists,
            } => {
                let mut feed = self.shared.feed.write().unwrap();
                let search = &mut feed.search;
                if search.query == query {
                    match (source, tracks) {
                        (Source::Spotify, Ok(t)) => search.spotify = t,
                        (_, Ok(t)) => search.soundcloud = t,
                        (Source::Spotify, Err(e)) => {
                            search.errors.push(format!("Spotify: {}", friendly_spotify_error(&e)))
                        }
                        (_, Err(e)) => search
                            .errors
                            .push(format!("{}: {}", source.label(), friendly_net_error(&e))),
                    }
                    match source {
                        Source::Spotify => search.spotify_pending = false,
                        _ => search.soundcloud_pending = false,
                    }
                    search.artists.extend(artists);
                    rank_artists(&mut search.artists, &query);
                }
                drop(feed);
                self.shared.repaint();
            }
            Internal::SoundCloudClientId(id) => {
                if self.db.get_kv("soundcloud_client_id").as_deref() != Some(id.as_str()) {
                    let _ = self.db.set_kv("soundcloud_client_id", &id);
                }
            }
            Internal::PageLoaded { key, result } => {
                let page = match result {
                    Ok(mut page) => {
                        page.key = key;
                        self.page_cache.retain(|p| p.key != page.key);
                        self.page_cache.push(page.clone());
                        if self.page_cache.len() > 8 {
                            self.page_cache.remove(0);
                        }
                        page
                    }
                    Err(e) => {
                        tracing::warn!("loading page {key}: {e:#}");
                        PageState {
                            key,
                            error: Some(friendly_page_error(&e)),
                            ..Default::default()
                        }
                    }
                };
                self.set_page(page);
            }
            Internal::ShortLink { key, result } => {
                if self.shared.feed.read().unwrap().page.key != key {
                    return;
                }
                match result.map(|url| links::target(&url)) {
                    Ok(Some(target)) if !matches!(target, Target::Short(_)) => self.load_page(key, target),
                    Ok(_) => self.page_failed(
                        &key,
                        "That short link doesn't lead to a song, album, playlist or artist",
                    ),
                    Err(e) => self.page_failed(&key, format!("Couldn't open the link: {e:#}")),
                }
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
            Internal::DownloadDone { track, result } => self.download_finished(track, result),
            Internal::YtDlpChecked { program, result } => {
                self.shared.feed.write().unwrap().ytdlp = Some((program, result));
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

/// Database key prefix of downloaded songs (`download:<track id>` → file path).
const DOWNLOAD_KEY: &str = "download:";

/// Short reason for the Downloads list (the song's title is shown next to it).
fn friendly_download_error(e: &anyhow::Error) -> String {
    let offline = e.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|r| r.is_connect() || r.is_timeout())
    });
    if offline {
        return "Couldn't reach SoundCloud (offline?)".into();
    }
    // Drop wrappers that only repeat which song it was.
    let parts: Vec<String> = e
        .chain()
        .map(|c| c.to_string())
        .filter(|m| !(m.starts_with("couldn't load \"") && m.ends_with("from SoundCloud")))
        .collect();
    let text = if parts.is_empty() {
        format!("{e}")
    } else {
        parts.join(": ")
    };
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => "Download failed".into(),
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
    )
    .with_genius(genius::shared());
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

/// Fraction (0..=1) of a fade of `len` seconds that started at `start`.
fn fade_progress(start: Instant, len: f64, now: Instant) -> f32 {
    if len <= 0.0 {
        return 1.0;
    }
    (now.saturating_duration_since(start).as_secs_f64() / len).clamp(0.0, 1.0) as f32
}

/// Smooth start and end of a fade (smoothstep).
fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// How early to start a crossfade so the next track has time to load.
fn load_lead(source: Source) -> f64 {
    match source {
        Source::Local => 0.3,
        Source::Spotify => 1.5,
        Source::SoundCloud => 2.0,
        Source::AppleMusic => 2.5,
    }
}

/// Whether `current` should crossfade into `next`. Songs of the same album stay gapless
/// unless `same_album_too`; repeat-one never fades a song into itself.
fn crossfades_into(
    len: f64,
    same_album_too: bool,
    repeat: RepeatMode,
    current: Option<&Track>,
    next: Option<&Track>,
) -> bool {
    let (Some(current), Some(next)) = (current, next) else {
        return false;
    };
    len > 0.0 && repeat != RepeatMode::One && (same_album_too || !same_album(current, next))
}

fn same_album(a: &Track, b: &Track) -> bool {
    let album = a.album.trim();
    !album.is_empty()
        && album.eq_ignore_ascii_case(b.album.trim())
        && normalize_artist(&a.artist) == normalize_artist(&b.artist)
}

/// Exact and prefix name matches first; Spotify before SoundCloud on ties. Stable, so each
/// service's own relevance order is kept.
fn rank_artists(artists: &mut [ArtistHit], query: &str) {
    let q = query.trim().to_lowercase();
    artists.sort_by_cached_key(|a| {
        let name = a.name.to_lowercase();
        let rank = if name == q {
            0
        } else if name.starts_with(&q) {
            1
        } else if name.contains(&q) {
            2
        } else {
            3
        };
        (rank, a.source != Source::Spotify)
    });
}

async fn spotify_page(session: librespot_core::session::Session, kind: LinkKind, id: &str) -> Result<PageState> {
    use crate::providers::spotify_internal as si;
    let data = match kind {
        LinkKind::Artist => si::artist_page(&session, id).await?,
        LinkKind::Album => si::album_page(&session, id).await?,
        LinkKind::Playlist => si::playlist_page(&session, id).await?,
        LinkKind::Track => si::track_page(&session, id).await?,
    };
    Ok(PageState {
        kind: links::kind_label(kind).into(),
        source: Some(Source::Spotify),
        title: data.title,
        subtitle: data.subtitle,
        image: data.image,
        round: kind == LinkKind::Artist,
        tracks: data.tracks,
        external_url: links::web_url(&format!("spotify:{}:{id}", links::kind_name(kind))),
        ..Default::default()
    })
}

/// The songs a pasted line stands for: a Spotify or SoundCloud song, album or playlist link, or
/// a music file (a path or a `file://` link from a file manager). `None` when it can't be found.
async fn songs_behind_link(
    line: &str,
    session: Option<&librespot_core::session::Session>,
    soundcloud: &SoundCloud,
    http: &reqwest::Client,
    shared: &Shared,
) -> Option<Vec<Track>> {
    let line = line.trim();
    let path = match line.strip_prefix("file://") {
        Some(rest) => urlencoding::decode(rest).map_or_else(|_| rest.to_string(), |p| p.into_owned()),
        None => line.to_string(),
    };
    if path.starts_with('/') {
        let path = PathBuf::from(path);
        if !library::scanner::is_audio_file(&path) || !path.is_file() {
            return None;
        }
        let id = Track::local_id(&path.to_string_lossy());
        if let Some(known) = shared.library.read().unwrap().get(&id).cloned() {
            return Some(vec![known]);
        }
        let read = tokio::task::spawn_blocking(move || library::scanner::read_track(&path, None, now_unix()));
        return read.await.ok().map(|t| vec![t]);
    }
    let mut target = links::target(line)?;
    if let Target::Short(url) = &target {
        target = links::target(&expand_short_link(http, url).await.ok()?)?;
    }
    match target {
        Target::Spotify(kind @ (LinkKind::Track | LinkKind::Album | LinkKind::Playlist), id) => {
            let page = spotify_page(session?.clone(), kind, &id);
            let page = tokio::time::timeout(Duration::from_secs(45), page).await.ok()?.ok()?;
            Some(page.tracks)
        }
        Target::SoundCloudUrl(url) => match soundcloud.resolve_url(&url).await.ok()? {
            ScResolved::Track(t) => Some(vec![t]),
            ScResolved::Playlist(p) => Some(p.tracks),
            ScResolved::User(_) => None,
        },
        _ => None,
    }
}

/// What Last.fm gets for a song. A SoundCloud upload that is also released on Spotify is sent
/// with Spotify's artist, title and album: those are the names Last.fm knows, and the album is
/// where Last.fm and apps like .fmbot take the cover from. So is a local file without tags,
/// whose names were only guessed from the file name. Remembered per song.
async fn lastfm_track(spotify: Option<(Arc<SpotifyAuth>, Arc<SpotifyApi>)>, track: Track) -> Track {
    static RELEASES: OnceLock<std::sync::Mutex<HashMap<String, Option<Track>>>> = OnceLock::new();
    let mut track = track;
    match track.source {
        // Every SoundCloud song: even uploads with an album often credit "A x B" or a label,
        // where Spotify has the names Last.fm knows.
        Source::SoundCloud => {}
        Source::Local => {
            let path = PathBuf::from(&track.uri);
            let missing = tokio::task::spawn_blocking(move || library::scanner::untagged(&path)).await;
            let Ok(missing) = missing else { return track };
            if missing.album {
                // The folder's name is only a guess: Spotify's album or Last.fm's is better.
                track.album.clear();
            }
            if !missing.names {
                return track;
            }
        }
        _ => return track,
    }
    let Some((auth, api)) = spotify else { return track };
    let cache = RELEASES.get_or_init(Default::default);
    let known = cache.lock().unwrap().get(&track.id).cloned();
    let release = match known {
        Some(release) => release,
        None => {
            let (artist, title) = crate::integrations::lyrics::song_names(&track);
            let unknown = artist == library::scanner::UNKNOWN_ARTIST;
            let wanted = Track {
                artist: if unknown { String::new() } else { artist.clone() },
                title: title.clone(),
                ..track.clone()
            };
            let query = if unknown {
                title.clone()
            } else {
                format!("{artist} {title}")
            };
            let found = match auth.token().await {
                Ok(token) => api.search(&token, &query, 10).await.ok(),
                Err(_) => None,
            };
            let Some(results) = found else {
                // Couldn't ask: try again next time.
                return track;
            };
            let release = if unknown {
                same_title_and_length(&wanted, &results)
            } else {
                best_match(&wanted, &results)
            };
            let release = release.filter(|t| !t.album.trim().is_empty());
            let mut cache = cache.lock().unwrap();
            if cache.len() > 2000 {
                cache.clear();
            }
            cache.insert(track.id.clone(), release.clone());
            release
        }
    };
    match release {
        Some(release) => {
            tracing::debug!(
                "lastfm: {} - {} is {} - {} ({}) on Spotify",
                track.artist,
                track.title,
                release.artist,
                release.title,
                release.album
            );
            // Still a SoundCloud (or local) song to Last.fm: a SoundCloud song's album may be
            // swapped for one with a cover.
            Track {
                source: track.source,
                ..release
            }
        }
        None => track,
    }
}

/// For a song with no known artist: the candidate with the same title and nearly the same
/// length.
fn same_title_and_length(target: &Track, candidates: &[Track]) -> Option<Track> {
    let title = normalize_title(&target.title);
    if title.is_empty() || target.duration_ms == 0 {
        return None;
    }
    candidates
        .iter()
        .filter(|c| normalize_title(&c.title) == title && c.duration_ms > 0)
        .map(|c| (c, (target.duration_ms as i64 - c.duration_ms as i64).unsigned_abs()))
        .filter(|(_, diff)| *diff <= 3_000)
        .min_by_key(|(_, diff)| *diff)
        .map(|(c, _)| c.clone())
}

/// Spotify access for finding an artist there by name.
struct SpotifyLookup {
    auth: Arc<SpotifyAuth>,
    api: Arc<SpotifyApi>,
    session: librespot_core::session::Session,
}

/// An artist found on one service by name.
struct ArtistFound {
    source: Source,
    image: Option<String>,
    url: String,
    tracks: Vec<Track>,
}

/// Same artist name, ignoring case, spaces and punctuation ("Yung Lean" = "yunglean").
fn same_artist_name(a: &str, b: &str) -> bool {
    let squash = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    let a = squash(a);
    !a.is_empty() && a == squash(b)
}

/// The artist's SoundCloud profile (exact name only, so nobody else's) and their uploads.
async fn soundcloud_artist(sc: &SoundCloud, name: &str) -> Option<ArtistFound> {
    let users = sc.search_users(name, 8).await.ok()?;
    let hit = users.into_iter().find(|u| same_artist_name(&u.name, name))?;
    let id: u64 = hit.key.strip_prefix("soundcloud:user:")?.parse().ok()?;
    let tracks = sc.user_tracks(id).await.ok()?;
    Some(ArtistFound {
        source: Source::SoundCloud,
        url: links::web_url(&hit.key).unwrap_or_default(),
        image: hit.image,
        tracks,
    })
}

/// The artist on Spotify (exact name only) and their popular songs and latest releases.
async fn spotify_artist(l: &SpotifyLookup, name: &str) -> Option<ArtistFound> {
    let token = l.auth.token().await.ok()?;
    let (_, artists) = l.api.search_with_artists(&token, name, 5).await.ok()?;
    let hit = artists
        .into_iter()
        .find(|a| a.source == Source::Spotify && same_artist_name(&a.name, name))?;
    let id = hit.key.strip_prefix("spotify:artist:")?.to_string();
    let page = crate::providers::spotify_internal::artist_page(&l.session, &id)
        .await
        .ok()?;
    Some(ArtistFound {
        source: Source::Spotify,
        url: format!("https://open.spotify.com/artist/{id}"),
        image: page.image.or(hit.image),
        tracks: dedupe_songs(page.tracks),
    })
}

/// Sends a SoundCloud profile page, then again with the artist's Spotify songs added.
async fn with_spotify_songs(
    result: Result<PageState>,
    spotify: Option<SpotifyLookup>,
    send: impl Fn(Result<PageState>),
) {
    let (Ok(mut page), Some(spotify)) = (result.as_ref().map(Clone::clone), spotify) else {
        send(result);
        return;
    };
    if page.kind != "Artist" {
        send(Ok(page));
        return;
    }
    page.tracks = dedupe_songs(page.tracks);
    page.merging = Some(Source::Spotify);
    send(Ok(page.clone()));
    if let Some(found) = spotify_artist(&spotify, &page.title).await {
        if add_songs(&mut page.tracks, found.tracks) > 0 {
            page.subtitle = artist_subtitle(&page.tracks);
        }
    }
    page.merging = None;
    send(Ok(page));
}

/// "Artist · 42 songs on Spotify and SoundCloud".
fn artist_subtitle(tracks: &[Track]) -> String {
    let on = |s: Source| tracks.iter().filter(|t| t.source == s).count();
    let (spotify, soundcloud) = (on(Source::Spotify), on(Source::SoundCloud));
    let total = tracks.len();
    match (spotify > 0, soundcloud > 0) {
        (true, true) => format!("Artist · {total} songs on Spotify and SoundCloud"),
        (true, false) => format!("Artist · {total} songs on Spotify"),
        (false, true) => format!("Artist · {total} songs on SoundCloud"),
        (false, false) => format!("Artist · {total} songs"),
    }
}

/// What makes two songs the same: the title as people know it ("Artist - Title [Free DL]"
/// uploads cleaned up, "(feat. …)" and "- Remastered" dropped; "(Live)", "(Remix)" kept).
fn song_key(t: &Track) -> String {
    normalize_title(&crate::integrations::lastfm::scrobble_names(t).1)
}

/// Whether `a` and `b` are the same song: same title and about the same length (a little
/// looser across services, which encode differently).
#[cfg(test)]
fn same_song(a: &Track, b: &Track) -> bool {
    same_song_keyed(&song_key(a), a, &song_key(b), b)
}

fn same_song_keyed(ka: &str, a: &Track, kb: &str, b: &Track) -> bool {
    if ka.is_empty() || ka != kb {
        return false;
    }
    let tolerance = if a.source == b.source { 4_000 } else { 12_000 };
    a.duration_ms == 0 || b.duration_ms == 0 || a.duration_ms.abs_diff(b.duration_ms) <= tolerance
}

/// Appends the songs of `extra` that `tracks` doesn't have yet. Returns how many were added.
pub fn add_songs(tracks: &mut Vec<Track>, extra: Vec<Track>) -> usize {
    let mut keys: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        keys.entry(song_key(t)).or_default().push(i);
    }
    let mut added = 0;
    for t in extra {
        let key = song_key(&t);
        let known = keys
            .get(&key)
            .is_some_and(|ids| ids.iter().any(|&i| same_song_keyed(&key, &tracks[i], &key, &t)));
        if !known {
            keys.entry(key).or_default().push(tracks.len());
            tracks.push(t);
            added += 1;
        }
    }
    added
}

/// The list without repeats (a single that is also on the album, the same upload twice).
pub fn dedupe_songs(tracks: Vec<Track>) -> Vec<Track> {
    let mut out = Vec::with_capacity(tracks.len());
    add_songs(&mut out, tracks);
    out
}

fn soundcloud_user_page(user: &ScUser, tracks: Vec<Track>) -> PageState {
    let hit = soundcloud::artist_hit(user);
    PageState {
        kind: "Artist".into(),
        source: Some(Source::SoundCloud),
        title: user.username.clone(),
        subtitle: format!("{} · {} tracks", hit.subtitle, tracks.len()),
        image: user.avatar.clone(),
        round: true,
        tracks,
        external_url: (!user.permalink_url.is_empty()).then(|| user.permalink_url.clone()),
        ..Default::default()
    }
}

/// Follows a short link (spotify.link, on.soundcloud.com) to the page it stands for.
async fn expand_short_link(http: &reqwest::Client, url: &str) -> Result<String> {
    let resp = http.get(url).send().await?.error_for_status()?;
    let landed = resp.url().to_string();
    if links::parse(&landed).is_some_and(|l| !matches!(l, links::Link::Short { .. })) {
        return Ok(landed);
    }
    // Some short links land on an HTML page that links (or script-redirects) to the target.
    let body = resp.text().await.unwrap_or_default();
    links::find_link(&body)
        .and_then(|link| links::web_url(&links::page_key(&link)))
        .ok_or_else(|| anyhow!("{url} didn't lead to a music page"))
}

fn friendly_page_error(e: &anyhow::Error) -> String {
    if apple_music::is_auth_error(e) {
        "Apple Music refused the request. Try again, or paste a developer token in Settings → Apple Music.".into()
    } else {
        friendly_net_error(e)
    }
}

/// Connection problems get a short message instead of a chain of wrapped errors.
fn friendly_net_error(e: &anyhow::Error) -> String {
    let offline = e.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|r| r.is_connect() || r.is_timeout())
    });
    if offline {
        format!("couldn't connect ({e}). Check your internet connection.")
    } else {
        format!("{e:#}")
    }
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
    ranked_matches(target, candidates).into_iter().next()
}

/// Candidates that are the same song as `target`, best first.
pub fn ranked_matches(target: &Track, candidates: &[Track]) -> Vec<Track> {
    let title = normalize_title(&target.title);
    let artist = normalize_artist(&target.artist);
    let mut matches: Vec<(&Track, u64)> = candidates
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
        .collect();
    matches.sort_by_key(|(_, score)| *score);
    matches.into_iter().map(|(c, _)| c.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossfade_decisions() {
        let mut a = track(Source::Local, "Neon Coast", "Intro", 200_000);
        a.album = "Midnight Drive".into();
        let mut b = track(Source::Spotify, "Neon Coast", "Afterglow", 180_000);
        b.album = "Midnight Drive".into();
        let mut c = track(Source::SoundCloud, "Bladee", "Topman", 194_000);
        c.album = "Icedancer".into();
        // Different albums (and sources) fade; the same album stays gapless unless asked.
        assert!(crossfades_into(6.0, false, RepeatMode::Off, Some(&a), Some(&c)));
        assert!(!crossfades_into(6.0, false, RepeatMode::Off, Some(&a), Some(&b)));
        assert!(crossfades_into(6.0, true, RepeatMode::Off, Some(&a), Some(&b)));
        // Off, nothing next, or repeat-one: no crossfade.
        assert!(!crossfades_into(0.0, true, RepeatMode::Off, Some(&a), Some(&c)));
        assert!(!crossfades_into(6.0, true, RepeatMode::Off, Some(&a), None));
        assert!(!crossfades_into(6.0, true, RepeatMode::One, Some(&a), Some(&a)));
        assert!(crossfades_into(6.0, false, RepeatMode::All, Some(&c), Some(&a)));
        // "Greatest Hits" by two artists isn't one album.
        let mut d = track(Source::Local, "Other Band", "Song", 100_000);
        d.album = "midnight drive".into();
        assert!(same_album(&a, &b));
        assert!(!same_album(&a, &d));
        let mut no_album = a.clone();
        no_album.album.clear();
        assert!(!same_album(&no_album, &no_album.clone()));
    }

    #[test]
    fn fade_curves() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert!((ease(0.5) - 0.5).abs() < 1e-6);
        assert!(ease(0.25) < 0.25 && ease(0.75) > 0.75);
        let start = Instant::now();
        assert_eq!(fade_progress(start, 4.0, start), 0.0);
        assert!((fade_progress(start, 4.0, start + Duration::from_secs(1)) - 0.25).abs() < 1e-6);
        assert_eq!(fade_progress(start, 4.0, start + Duration::from_secs(9)), 1.0);
        assert_eq!(fade_progress(start, 0.0, start), 1.0);
        // Streams get more time to load than local files.
        assert!(load_lead(Source::Local) < load_lead(Source::Spotify));
        assert!(load_lead(Source::Spotify) < load_lead(Source::SoundCloud));
    }

    #[test]
    fn artists_rank_exact_then_prefix_then_spotify() {
        let hit = |name: &str, source: Source| ArtistHit {
            key: format!("{}:{name}", source.as_str()),
            name: name.into(),
            image: None,
            source,
            subtitle: String::new(),
        };
        let mut artists = vec![
            hit("The Neon Coast Band", Source::SoundCloud),
            hit("neon coast", Source::SoundCloud),
            hit("Neon Coastline", Source::Spotify),
            hit("Neon Coast", Source::Spotify),
        ];
        rank_artists(&mut artists, "Neon Coast");
        let order: Vec<(&str, Source)> = artists.iter().map(|a| (a.name.as_str(), a.source)).collect();
        assert_eq!(
            order,
            vec![
                ("Neon Coast", Source::Spotify),
                ("neon coast", Source::SoundCloud),
                ("Neon Coastline", Source::Spotify),
                ("The Neon Coast Band", Source::SoundCloud),
            ]
        );
    }

    /// A short link that redirects to a page linking the real target.
    #[tokio::test]
    async fn short_links_are_followed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if head.starts_with("GET /short ") {
                    "HTTP/1.1 302 Found\r\nLocation: /landing\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else {
                    let body = "<html><a href=\"https://open.spotify.com/album/4m2880jivSbbyEGAKfITCa?si=1&amp;x=2\">Open</a></html>";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = expand_short_link(&http, &format!("{base}/short")).await.unwrap();
        assert_eq!(url, "https://open.spotify.com/album/4m2880jivSbbyEGAKfITCa");
        assert_eq!(
            links::target(&url),
            Some(Target::Spotify(LinkKind::Album, "4m2880jivSbbyEGAKfITCa".into()))
        );
    }

    #[test]
    fn download_errors_are_short() {
        let e =
            anyhow!("SoundCloud /tracks/1 returned 404 Not Found").context("couldn't load \"Song\" from SoundCloud");
        assert_eq!(
            friendly_download_error(&e),
            "SoundCloud /tracks/1 returned 404 Not Found"
        );
        let e = anyhow!("\"Song\" can't be downloaded: only a 30 second preview is available (Go+ track)");
        assert!(friendly_download_error(&e).starts_with("\"Song\" can't be downloaded"));
    }

    #[test]
    fn artist_songs_without_repeats() {
        let sp = |title: &str, dur: u64| track(Source::Spotify, "Bladee", title, dur);
        let sc = |title: &str, dur: u64| track(Source::SoundCloud, "bladee", title, dur);
        // A single that is also on the album; "Intro"s of different albums are different songs.
        let spotify = dedupe_songs(vec![
            sp("Waster", 195_000),
            sp("Be Nice 2 Me", 182_000),
            sp("Waster", 195_000),
            sp("Intro", 60_000),
            sp("Intro", 95_000),
            sp("Waster - Remastered 2020", 196_000),
        ]);
        let titles: Vec<&str> = spotify.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["Waster", "Be Nice 2 Me", "Intro", "Intro"]);

        // The same songs on SoundCloud are left out; versions and SoundCloud-only songs stay.
        let mut songs = spotify;
        let added = add_songs(
            &mut songs,
            vec![
                sc("Bladee - Waster [Free DL]", 197_000),
                sc("be nice 2 me", 0),
                sc("Waster (Remix)", 210_000),
                sc("Waster - Slowed", 260_000),
                sc("Unreleased Demo", 120_000),
                sc("Be Nice 2 Me (Live)", 190_000),
            ],
        );
        assert_eq!(added, 4);
        let extra: Vec<&str> = songs[4..].iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            extra,
            vec![
                "Waster (Remix)",
                "Waster - Slowed",
                "Unreleased Demo",
                "Be Nice 2 Me (Live)"
            ]
        );
        assert!(same_song(&sp("Waster", 195_000), &sc("Waster", 205_000)));
        assert!(!same_song(&sp("Waster", 195_000), &sc("Waster", 240_000)));
        assert_eq!(artist_subtitle(&songs), "Artist · 8 songs on Spotify and SoundCloud");
        assert_eq!(artist_subtitle(&songs[..4]), "Artist · 4 songs on Spotify");
    }

    #[test]
    fn artist_names_match_exactly() {
        assert!(same_artist_name("Yung Lean", "yunglean"));
        assert!(same_artist_name("Bladee", "BLADEE"));
        assert!(!same_artist_name("Bladee", "Bladee Fan Page"));
        assert!(!same_artist_name("", ""));
    }

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
    fn songs_without_an_artist_match_by_title_and_length() {
        let mut target = track(Source::Local, "", "Waster", 200_000);
        let candidates = vec![
            track(Source::Spotify, "Someone", "Waster", 150_000),
            track(Source::Spotify, "Bladee", "Waster", 201_500),
            track(Source::Spotify, "Bladee", "Waster (Remix)", 200_000),
        ];
        let found = same_title_and_length(&target, &candidates).unwrap();
        assert_eq!(found.artist, "Bladee");
        // Without a length there's too little to go on.
        target.duration_ms = 0;
        assert!(same_title_and_length(&target, &candidates).is_none());
    }

    #[tokio::test]
    async fn untagged_files_drop_the_folder_album_for_lastfm() {
        let dir = std::env::temp_dir().join(format!("multimusic-lfm-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Bladee - Waster.mp3");
        std::fs::write(&path, b"no tags here").unwrap();
        let mut t = track(Source::Local, "Bladee", "Waster", 200_000);
        t.album = "Downloads".into();
        t.uri = path.to_string_lossy().into();
        let sent = lastfm_track(None, t.clone()).await;
        assert_eq!((sent.artist.as_str(), sent.title.as_str()), ("Bladee", "Waster"));
        assert_eq!(sent.album, "");
        // Other sources are left alone.
        let sc = Track {
            source: Source::Spotify,
            ..t.clone()
        };
        assert_eq!(lastfm_track(None, sc).await.album, "Downloads");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn pasted_files_and_links_become_songs() {
        let dir = std::env::temp_dir().join(format!("multimusic-paste-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Bladee - Waster.mp3");
        std::fs::write(&path, b"no tags").unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let sc = SoundCloud::new(http.clone(), "", "");
        let shared = Shared::default();
        let paste = |line: String| {
            let (sc, http, shared) = (&sc, &http, &shared);
            async move { songs_behind_link(&line, None, sc, http, shared).await }
        };
        // A path, or a file manager's file:// link.
        let found = paste(path.to_string_lossy().into()).await.unwrap();
        assert_eq!(
            (found[0].artist.as_str(), found[0].title.as_str()),
            ("Bladee", "Waster")
        );
        let link = format!("file://{}", path.to_string_lossy().replace(' ', "%20"));
        assert_eq!(paste(link).await.unwrap()[0].id, found[0].id);
        // A song already in the library comes from there.
        let mut known = found[0].clone();
        known.title = "Known".into();
        shared.library.write().unwrap().tracks.insert(known.id.clone(), known);
        assert_eq!(paste(path.to_string_lossy().into()).await.unwrap()[0].title, "Known");
        // Not music, gone, or a Spotify link without a session.
        assert!(paste(dir.to_string_lossy().into()).await.is_none());
        assert!(paste("/nowhere/x.mp3".into()).await.is_none());
        assert!(paste("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC".into())
            .await
            .is_none());
        assert!(paste("hello".into()).await.is_none());
        std::fs::remove_dir_all(dir).unwrap();
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
