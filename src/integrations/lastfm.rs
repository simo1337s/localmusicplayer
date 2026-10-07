//! Last.fm: authentication, now playing, scrobbling (with an offline queue) and loving tracks.
//!
//! API docs: <https://www.last.fm/api/scrobbling>. Every write call is a signed, form-encoded
//! POST. Scrobbles that fail for transient reasons (network, 5xx, rate limiting) are kept in a
//! JSON queue file and resubmitted in batches by [`Lastfm::flush_queue`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{now_unix, Track};

pub const API_ROOT: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTH_URL: &str = "https://www.last.fm/api/auth/";

/// Max scrobbles per `track.scrobble` request.
pub const BATCH_SIZE: usize = 50;
/// Max queued scrobbles kept on disk (oldest are dropped first).
pub const QUEUE_CAP: usize = 2000;
/// Last.fm rejects scrobbles with a timestamp older than this.
pub const MAX_SCROBBLE_AGE_SECS: i64 = 14 * 24 * 60 * 60;

/// Error codes (<https://www.last.fm/api/errorcodes>).
const ERR_OPERATION_FAILED: i64 = 8;
const ERR_INVALID_SESSION: i64 = 9;
const ERR_SERVICE_OFFLINE: i64 = 11;
const ERR_UNAUTHORIZED_TOKEN: i64 = 14;
const ERR_TEMPORARY: i64 = 16;
const ERR_RATE_LIMIT: i64 = 29;

/// Computes `api_sig`: params sorted by key, `key + value` concatenated, secret appended,
/// md5 as lowercase hex. `format` and `callback` are not part of the signature.
pub fn sign(params: &[(&str, &str)], secret: &str) -> String {
    let mut sorted: Vec<&(&str, &str)> = params
        .iter()
        .filter(|(k, _)| *k != "format" && *k != "callback")
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = Md5::new();
    for (k, v) in sorted {
        hasher.update(k.as_bytes());
        hasher.update(v.as_bytes());
    }
    hasher.update(secret.as_bytes());
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A failed Last.fm call.
#[derive(Debug, Clone, PartialEq)]
pub enum LastfmError {
    /// `{ "error": code, "message": "..." }` returned by the API.
    Api { code: i64, message: String },
    /// Non-success HTTP status without a parseable API error.
    Http { status: u16 },
}

impl fmt::Display for LastfmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LastfmError::Api { code, message } => write!(f, "Last.fm error {code}: {message}"),
            LastfmError::Http { status } => write!(f, "Last.fm returned HTTP {status}"),
        }
    }
}

impl std::error::Error for LastfmError {}

fn api_error_code(err: &anyhow::Error) -> Option<i64> {
    match err.downcast_ref::<LastfmError>() {
        Some(LastfmError::Api { code, .. }) => Some(*code),
        _ => None,
    }
}

/// True for `auth.getSession` failing because the user hasn't approved the token yet (code 14).
pub fn is_pending_auth(err: &anyhow::Error) -> bool {
    api_error_code(err) == Some(ERR_UNAUTHORIZED_TOKEN)
}

/// True for the session key being invalid/revoked (code 9): the user must re-authenticate.
pub fn is_auth_error(err: &anyhow::Error) -> bool {
    api_error_code(err) == Some(ERR_INVALID_SESSION)
}

/// True when retrying later may succeed: network errors, HTTP 5xx/429, and Last.fm codes
/// 8 (operation failed), 11 (service offline), 16 (temporarily unavailable), 29 (rate limit).
pub fn is_transient(err: &anyhow::Error) -> bool {
    if err.chain().any(|e| e.downcast_ref::<reqwest::Error>().is_some()) {
        return true;
    }
    match err.downcast_ref::<LastfmError>() {
        Some(LastfmError::Api { code, .. }) => {
            matches!(
                *code,
                ERR_OPERATION_FAILED | ERR_SERVICE_OFFLINE | ERR_TEMPORARY | ERR_RATE_LIMIT
            )
        }
        Some(LastfmError::Http { status }) => *status >= 500 || *status == 429,
        None => false,
    }
}

/// Turns a response body into JSON or a [`LastfmError`].
fn parse_response(status: u16, body: &str) -> anyhow::Result<Value> {
    let json: Option<Value> = serde_json::from_str(body).ok();
    if let Some(v) = &json {
        if let Some(code) = v.get("error").and_then(Value::as_i64) {
            let message = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            return Err(LastfmError::Api { code, message }.into());
        }
    }
    if !(200..300).contains(&status) {
        return Err(LastfmError::Http { status }.into());
    }
    json.context("Last.fm returned invalid JSON")
}

/// Last.fm prefers the main artist: "A, B" -> "A".
pub fn scrobble_artist(artist: &str) -> &str {
    let artist = artist.trim();
    match artist.split_once(", ") {
        Some((first, _)) if !first.trim().is_empty() => first.trim(),
        _ => artist,
    }
}

/// One scrobble, as stored in the offline queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedScrobble {
    pub artist: String,
    pub track: String,
    #[serde(default)]
    pub album: String,
    /// Seconds, 0 = unknown.
    #[serde(default)]
    pub duration: u64,
    /// Unix seconds when playback started.
    pub timestamp: i64,
}

impl QueuedScrobble {
    /// `None` when the track lacks an artist or title (Last.fm would reject it).
    pub fn from_track(track: &Track, started_at: i64) -> Option<Self> {
        let artist = scrobble_artist(&track.artist);
        let title = track.title.trim();
        if artist.is_empty() || title.is_empty() {
            return None;
        }
        Some(QueuedScrobble {
            artist: artist.to_string(),
            track: title.to_string(),
            album: track.album.trim().to_string(),
            duration: track.duration_ms.saturating_add(500) / 1000,
            timestamp: started_at,
        })
    }
}

/// Drops scrobbles Last.fm would reject for being older than 14 days.
pub fn prune_queue(queue: Vec<QueuedScrobble>, now: i64) -> Vec<QueuedScrobble> {
    queue
        .into_iter()
        .filter(|s| now.saturating_sub(s.timestamp) < MAX_SCROBBLE_AGE_SECS)
        .collect()
}

/// Keeps at most [`QUEUE_CAP`] entries, dropping the oldest (front) ones.
fn cap_queue(queue: &mut Vec<QueuedScrobble>) {
    if queue.len() > QUEUE_CAP {
        let excess = queue.len() - QUEUE_CAP;
        queue.drain(..excess);
    }
}

/// Reads the queue file; a missing or corrupt file is an empty queue.
pub async fn load_queue(path: &Path) -> Vec<QueuedScrobble> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!("lastfm: ignoring corrupt scrobble queue {}: {e}", path.display());
            Vec::new()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            tracing::warn!("lastfm: can't read scrobble queue {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// Writes the queue atomically (temp file + rename). An empty queue removes the file.
pub async fn save_queue(path: &Path, queue: &[QueuedScrobble]) -> anyhow::Result<()> {
    if queue.is_empty() {
        match tokio::fs::remove_file(path).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(dir).await?;
    }
    let json = serde_json::to_vec_pretty(queue)?;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    tokio::fs::write(&tmp, json)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// `track.scrobble` batch parameters (`artist[i]`, `track[i]`, ...).
fn batch_params(batch: &[QueuedScrobble]) -> Vec<(String, String)> {
    let mut params = Vec::with_capacity(batch.len() * 5);
    for (i, s) in batch.iter().enumerate() {
        params.push((format!("artist[{i}]"), s.artist.clone()));
        params.push((format!("track[{i}]"), s.track.clone()));
        params.push((format!("timestamp[{i}]"), s.timestamp.to_string()));
        if !s.album.is_empty() {
            params.push((format!("album[{i}]"), s.album.clone()));
        }
        if s.duration > 0 {
            params.push((format!("duration[{i}]"), s.duration.to_string()));
        }
    }
    params
}

fn session_from(v: &Value) -> anyhow::Result<(String, String)> {
    let session = v.get("session").context("Last.fm response has no session")?;
    let key = session
        .get("key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("Last.fm session has no key")?;
    let name = session.get("name").and_then(Value::as_str).unwrap_or_default();
    Ok((key.to_string(), name.to_string()))
}

pub struct Lastfm {
    http: reqwest::Client,
    api_key: String,
    api_secret: String,
    session_key: Option<String>,
    queue_path: PathBuf,
    /// Serializes access to the queue file.
    queue_lock: tokio::sync::Mutex<()>,
}

impl Lastfm {
    pub fn new(http: reqwest::Client, api_key: &str, api_secret: &str, session_key: &str, queue_path: PathBuf) -> Self {
        let session_key = session_key.trim();
        Lastfm {
            http,
            api_key: api_key.trim().to_string(),
            api_secret: api_secret.trim().to_string(),
            session_key: (!session_key.is_empty()).then(|| session_key.to_string()),
            queue_path,
            queue_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn is_authenticated(&self) -> bool {
        self.session_key.is_some()
    }

    /// Adds `method`, `api_key`, optionally `sk`, then `api_sig` and `format=json`.
    fn signed(
        &self,
        method: &str,
        mut params: Vec<(String, String)>,
        with_session: bool,
    ) -> anyhow::Result<Vec<(String, String)>> {
        if self.api_key.is_empty() || self.api_secret.is_empty() {
            anyhow::bail!("Last.fm API key/secret not configured");
        }
        params.push(("method".into(), method.into()));
        params.push(("api_key".into(), self.api_key.clone()));
        if with_session {
            let sk = self.session_key.as_ref().context("not logged in to Last.fm")?;
            params.push(("sk".into(), sk.clone()));
        }
        let refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let sig = sign(&refs, &self.api_secret);
        params.push(("api_sig".into(), sig));
        params.push(("format".into(), "json".into()));
        Ok(params)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> anyhow::Result<Value> {
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let body = resp.text().await?;
        parse_response(status, &body)
    }

    async fn get(&self, method: &str, params: Vec<(String, String)>) -> anyhow::Result<Value> {
        let params = self.signed(method, params, false)?;
        self.send(self.http.get(API_ROOT).query(&params)).await
    }

    async fn post(&self, method: &str, params: Vec<(String, String)>, with_session: bool) -> anyhow::Result<Value> {
        let params = self.signed(method, params, with_session)?;
        self.send(self.http.post(API_ROOT).form(&params)).await
    }

    /// `auth.getToken`: a request token the user approves at [`Lastfm::auth_url`].
    pub async fn get_token(&self) -> anyhow::Result<String> {
        let v = self.get("auth.getToken", Vec::new()).await?;
        v.get("token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .context("Last.fm returned no token")
    }

    pub fn auth_url(&self, token: &str) -> String {
        format!(
            "{AUTH_URL}?api_key={}&token={}",
            urlencoding::encode(&self.api_key),
            urlencoding::encode(token)
        )
    }

    /// `auth.getSession` -> `(session_key, username)`. Until the user approved the token this
    /// fails with an error for which [`is_pending_auth`] is true.
    pub async fn get_session(&self, token: &str) -> anyhow::Result<(String, String)> {
        let v = self
            .get("auth.getSession", vec![("token".into(), token.into())])
            .await?;
        session_from(&v)
    }

    /// `track.updateNowPlaying`. Tracks without artist/title are silently skipped.
    pub async fn now_playing(&self, track: &Track) -> anyhow::Result<()> {
        let Some(s) = QueuedScrobble::from_track(track, 0) else {
            return Ok(());
        };
        let mut params = vec![("artist".into(), s.artist), ("track".into(), s.track)];
        if !s.album.is_empty() {
            params.push(("album".into(), s.album));
        }
        if s.duration > 0 {
            params.push(("duration".into(), s.duration.to_string()));
        }
        self.post("track.updateNowPlaying", params, true).await?;
        Ok(())
    }

    async fn submit(&self, batch: &[QueuedScrobble]) -> anyhow::Result<()> {
        let v = self.post("track.scrobble", batch_params(batch), true).await?;
        if let Some(attr) = v.pointer("/scrobbles/@attr") {
            let count = |k: &str| {
                attr.get(k)
                    .and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
                    .unwrap_or(0)
            };
            let ignored = count("ignored");
            if ignored > 0 {
                tracing::warn!(
                    "lastfm: {ignored} of {} scrobbles ignored by Last.fm",
                    count("accepted") + ignored
                );
            }
        }
        Ok(())
    }

    async fn enqueue(&self, scrobble: QueuedScrobble) -> anyhow::Result<()> {
        let _guard = self.queue_lock.lock().await;
        let mut queue = load_queue(&self.queue_path).await;
        queue.push(scrobble);
        cap_queue(&mut queue);
        save_queue(&self.queue_path, &queue).await
    }

    /// `track.scrobble`. Transient failures (network, 5xx, rate limiting) and an invalid
    /// session queue the scrobble for later; transient ones return `Ok(())`. After a
    /// successful scrobble the queue is flushed too.
    pub async fn scrobble(&self, track: &Track, started_at: i64) -> anyhow::Result<()> {
        let Some(s) = QueuedScrobble::from_track(track, started_at) else {
            tracing::debug!("lastfm: not scrobbling {:?}: missing artist or title", track.id);
            return Ok(());
        };
        if !self.is_authenticated() {
            anyhow::bail!("not logged in to Last.fm");
        }
        match self.submit(std::slice::from_ref(&s)).await {
            Ok(()) => {
                if let Err(e) = self.flush_queue().await {
                    tracing::debug!("lastfm: flushing scrobble queue failed: {e:#}");
                }
                Ok(())
            }
            Err(e) if is_transient(&e) => {
                tracing::info!("lastfm: scrobble failed ({e:#}), queued for later");
                self.enqueue(s).await
            }
            Err(e) if is_auth_error(&e) => {
                // Keep it so it can be submitted after re-authenticating.
                if let Err(qe) = self.enqueue(s).await {
                    tracing::warn!("lastfm: can't queue scrobble: {qe:#}");
                }
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Submits queued scrobbles in batches of 50. Returns how many were submitted.
    /// Entries older than 14 days are dropped; on a transient failure the rest stays queued.
    pub async fn flush_queue(&self) -> anyhow::Result<usize> {
        if !self.is_authenticated() {
            return Ok(0);
        }
        let _guard = self.queue_lock.lock().await;
        let loaded = load_queue(&self.queue_path).await;
        if loaded.is_empty() {
            return Ok(0);
        }
        let loaded_len = loaded.len();
        let mut queue = prune_queue(loaded, now_unix());
        if queue.len() < loaded_len {
            tracing::info!(
                "lastfm: dropped {} queued scrobbles older than 14 days",
                loaded_len - queue.len()
            );
        }

        let mut submitted = 0;
        let mut result = Ok(());
        while !queue.is_empty() {
            let n = queue.len().min(BATCH_SIZE);
            match self.submit(&queue[..n]).await {
                Ok(()) => {
                    submitted += n;
                    queue.drain(..n);
                }
                Err(e) if is_transient(&e) || is_auth_error(&e) => {
                    result = Err(e);
                    break;
                }
                Err(e) => {
                    // Malformed entries would block the queue forever; drop this batch.
                    tracing::warn!("lastfm: dropping {n} queued scrobbles rejected by Last.fm: {e:#}");
                    queue.drain(..n);
                }
            }
        }

        save_queue(&self.queue_path, &queue).await?;
        if submitted > 0 {
            tracing::info!("lastfm: submitted {submitted} queued scrobbles");
        }
        result.map(|()| submitted)
    }

    /// `track.love` / `track.unlove`.
    pub async fn love(&self, track: &Track, love: bool) -> anyhow::Result<()> {
        let artist = scrobble_artist(&track.artist);
        let title = track.title.trim();
        if artist.is_empty() || title.is_empty() {
            anyhow::bail!("track has no artist or title");
        }
        let params = vec![
            ("artist".into(), artist.to_string()),
            ("track".into(), title.to_string()),
        ];
        let method = if love { "track.love" } else { "track.unlove" };
        self.post(method, params, true).await?;
        Ok(())
    }
}

/// Last.fm scrobble rules: a track qualifies once it is longer than 30 seconds and has been
/// played for half its duration or 4 minutes, whichever comes first. Paused time doesn't count.
#[derive(Debug, Default)]
pub struct ScrobbleTracker {
    current: Option<Tracked>,
}

#[derive(Debug)]
struct Tracked {
    track_id: String,
    duration_ms: u64,
    started_at: i64,
    listened: Duration,
    scrobbled: bool,
}

const MIN_TRACK_LEN: Duration = Duration::from_secs(30);
const MAX_LISTEN_REQUIRED: Duration = Duration::from_secs(240);

impl ScrobbleTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new track started playing (resets all progress).
    pub fn start(&mut self, track_id: &str, duration_ms: u64, started_at_unix: i64) {
        self.current = Some(Tracked {
            track_id: track_id.to_string(),
            duration_ms,
            started_at: started_at_unix,
            listened: Duration::ZERO,
            scrobbled: false,
        });
    }

    /// Call periodically with the wall time since the previous tick; it only counts while playing.
    pub fn tick(&mut self, playing: bool, elapsed: Duration) {
        if let Some(t) = self.current.as_mut().filter(|_| playing) {
            t.listened = t.listened.saturating_add(elapsed);
        }
    }

    pub fn should_scrobble(&self) -> bool {
        let Some(t) = &self.current else {
            return false;
        };
        let duration = Duration::from_millis(t.duration_ms);
        !t.scrobbled && duration > MIN_TRACK_LEN && t.listened >= (duration / 2).min(MAX_LISTEN_REQUIRED)
    }

    pub fn mark_scrobbled(&mut self) {
        if let Some(t) = self.current.as_mut() {
            t.scrobbled = true;
        }
    }

    /// `(track_id, started_at)` of the tracked track.
    pub fn current(&self) -> Option<(&str, i64)> {
        self.current.as_ref().map(|t| (t.track_id.as_str(), t.started_at))
    }

    /// Time actually spent playing the current track.
    #[cfg(test)]
    pub fn listened(&self) -> Duration {
        self.current.as_ref().map(|t| t.listened).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Source;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn entry(n: i64) -> QueuedScrobble {
        QueuedScrobble {
            artist: format!("Artist {n}"),
            track: format!("Track {n}"),
            album: if n % 2 == 0 { String::new() } else { "Album".into() },
            duration: 180,
            timestamp: n,
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "multimusic-lastfm-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ))
            .join(name)
    }

    fn track(artist: &str, title: &str) -> Track {
        Track {
            id: "local:/x.flac".into(),
            source: Source::Local,
            title: title.into(),
            artist: artist.into(),
            album: String::new(),
            duration_ms: 200_400,
            track_no: None,
            art: None,
            uri: "/x.flac".into(),
            added_at: 0,
        }
    }

    #[test]
    fn signature_known_vector() {
        let params = [("api_key", "xxx"), ("method", "auth.getSession"), ("token", "yyy")];
        // md5("api_keyxxxmethodauth.getSessiontokenyyyzzz")
        assert_eq!(sign(&params, "zzz"), "75df1fdb6b738160924a52b1732fdde7");
        // Order independent; format/callback excluded.
        let shuffled = [
            ("token", "yyy"),
            ("format", "json"),
            ("method", "auth.getSession"),
            ("callback", "cb"),
            ("api_key", "xxx"),
        ];
        assert_eq!(sign(&shuffled, "zzz"), "75df1fdb6b738160924a52b1732fdde7");
        assert_eq!(sign(&[], ""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn signed_params_contain_sig_and_format() {
        let fm = Lastfm::new(reqwest::Client::new(), "xxx", "zzz", "", PathBuf::from("q.json"));
        assert!(!fm.is_authenticated());
        let p = fm
            .signed("auth.getSession", vec![("token".into(), "yyy".into())], false)
            .unwrap();
        let get = |k: &str| p.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.as_str());
        assert_eq!(get("api_sig"), Some("75df1fdb6b738160924a52b1732fdde7"));
        assert_eq!(get("format"), Some("json"));
        // Session required but missing.
        assert!(fm.signed("track.love", Vec::new(), true).is_err());
        let fm = Lastfm::new(reqwest::Client::new(), "k", "s", " sk ", PathBuf::from("q.json"));
        assert!(fm.is_authenticated());
        assert!(fm.auth_url("t o").ends_with("?api_key=k&token=t%20o"));
    }

    #[test]
    fn api_errors() {
        let err = parse_response(403, r#"{"error":14,"message":"Unauthorized Token"}"#).unwrap_err();
        assert!(is_pending_auth(&err));
        assert!(!is_transient(&err));
        assert!(err.to_string().contains("14"));

        let err = parse_response(200, r#"{"error":29,"message":"Rate limit exceeded"}"#).unwrap_err();
        assert!(is_transient(&err));
        assert!(!is_pending_auth(&err));

        let err = parse_response(503, "<html>down</html>").unwrap_err();
        assert!(is_transient(&err));
        let err = parse_response(400, "nope").unwrap_err();
        assert!(!is_transient(&err));

        let err = parse_response(403, r#"{"error":9,"message":"Invalid session key"}"#).unwrap_err();
        assert!(is_auth_error(&err));

        assert!(parse_response(200, r#"{"token":"abc"}"#).is_ok());
        assert!(parse_response(200, "not json").is_err());
    }

    #[test]
    fn scrobble_fields() {
        let s = QueuedScrobble::from_track(&track("Daft Punk, Pharrell Williams", " Get Lucky "), 42).unwrap();
        assert_eq!(s.artist, "Daft Punk");
        assert_eq!(s.track, "Get Lucky");
        assert_eq!(s.duration, 200);
        assert_eq!(s.timestamp, 42);
        assert_eq!(scrobble_artist("Simon & Garfunkel"), "Simon & Garfunkel");
        assert!(QueuedScrobble::from_track(&track("", "Title"), 0).is_none());
        assert!(QueuedScrobble::from_track(&track("Artist", " "), 0).is_none());

        let params = batch_params(&[entry(1), entry(2)]);
        let keys: Vec<&str> = params.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "artist[0]",
                "track[0]",
                "timestamp[0]",
                "album[0]",
                "duration[0]",
                "artist[1]",
                "track[1]",
                "timestamp[1]",
                "duration[1]"
            ]
        );
    }

    #[test]
    fn tracker_short_track_never_scrobbles() {
        let mut t = ScrobbleTracker::new();
        t.start("a", 30_000, 100);
        t.tick(true, secs(30));
        assert!(!t.should_scrobble());
        t.start("unknown", 0, 100);
        t.tick(true, secs(1000));
        assert!(!t.should_scrobble());
    }

    #[test]
    fn tracker_half_duration_rule() {
        let mut t = ScrobbleTracker::new();
        assert!(!t.should_scrobble());
        assert_eq!(t.current(), None);
        t.start("a", 200_000, 100);
        assert_eq!(t.current(), Some(("a", 100)));
        t.tick(true, secs(99));
        assert!(!t.should_scrobble());
        t.tick(true, secs(1));
        assert!(t.should_scrobble());
    }

    #[test]
    fn tracker_four_minute_cap() {
        let mut t = ScrobbleTracker::new();
        t.start("long", 20 * 60_000, 0);
        t.tick(true, secs(239));
        assert!(!t.should_scrobble());
        t.tick(true, secs(1));
        assert!(t.should_scrobble());
    }

    #[test]
    fn tracker_paused_time_not_counted() {
        let mut t = ScrobbleTracker::new();
        t.start("a", 100_000, 0);
        t.tick(false, secs(500));
        assert!(!t.should_scrobble());
        t.tick(true, secs(30));
        t.tick(false, secs(60));
        assert_eq!(t.listened(), secs(30));
        assert!(!t.should_scrobble());
        t.tick(true, secs(20));
        assert!(t.should_scrobble());
    }

    #[test]
    fn tracker_only_once_and_reset_on_start() {
        let mut t = ScrobbleTracker::new();
        t.start("a", 100_000, 5);
        t.tick(true, secs(60));
        assert!(t.should_scrobble());
        t.mark_scrobbled();
        assert!(!t.should_scrobble());
        t.tick(true, secs(600));
        assert!(!t.should_scrobble());
        // Replaying (new start) counts again.
        t.start("a", 100_000, 500);
        assert!(!t.should_scrobble());
        t.tick(true, secs(50));
        assert!(t.should_scrobble());
        assert_eq!(t.current(), Some(("a", 500)));
    }

    #[test]
    fn prune_drops_old_scrobbles() {
        let now = 10_000_000;
        let queue = vec![
            entry(now - MAX_SCROBBLE_AGE_SECS - 1),
            entry(now - MAX_SCROBBLE_AGE_SECS),
            entry(now - MAX_SCROBBLE_AGE_SECS + 60),
            entry(now - 10),
        ];
        let kept = prune_queue(queue, now);
        assert_eq!(
            kept.iter().map(|s| s.timestamp).collect::<Vec<_>>(),
            [now - MAX_SCROBBLE_AGE_SECS + 60, now - 10]
        );
    }

    #[test]
    fn queue_cap_drops_oldest() {
        let mut q: Vec<_> = (0..QUEUE_CAP as i64 + 5).map(entry).collect();
        cap_queue(&mut q);
        assert_eq!(q.len(), QUEUE_CAP);
        assert_eq!(q[0].timestamp, 5);
    }

    #[tokio::test]
    async fn queue_persistence_round_trip() {
        let path = temp_path("scrobbles.json");
        assert!(load_queue(&path).await.is_empty());

        let queue: Vec<_> = (1..=3).map(entry).collect();
        save_queue(&path, &queue).await.unwrap();
        assert_eq!(load_queue(&path).await, queue);
        assert!(!path.with_extension("json.tmp").exists());

        // Missing optional fields default.
        tokio::fs::write(&path, r#"[{"artist":"a","track":"t","timestamp":5}]"#)
            .await
            .unwrap();
        let q = load_queue(&path).await;
        assert_eq!(q[0].album, "");
        assert_eq!(q[0].duration, 0);

        // Corrupt file = empty queue.
        tokio::fs::write(&path, "{oops").await.unwrap();
        assert!(load_queue(&path).await.is_empty());

        // Saving an empty queue removes the file.
        save_queue(&path, &[]).await.unwrap();
        assert!(!path.exists());

        // Enqueue through the client (no network involved).
        let fm = Lastfm::new(reqwest::Client::new(), "k", "s", "", path.clone());
        fm.enqueue(entry(7)).await.unwrap();
        fm.enqueue(entry(8)).await.unwrap();
        assert_eq!(load_queue(&path).await, vec![entry(7), entry(8)]);
        // Not authenticated: flushing is a no-op that keeps the queue.
        assert_eq!(fm.flush_queue().await.unwrap(), 0);
        assert_eq!(load_queue(&path).await.len(), 2);

        if let Some(dir) = path.parent() {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}
