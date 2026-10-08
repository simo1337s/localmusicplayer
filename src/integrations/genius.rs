//! Genius (genius.com): the real artist, title, album and release date of songs whose metadata
//! is messy (SoundCloud uploads), and plain lyrics when LRCLIB has none. Uses the JSON API the
//! website itself uses, so no account is needed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::model::{normalize_artist, normalize_title};

const ROOT: &str = "https://genius.com";
/// Genius turns away clients that don't look like a browser.
const BROWSER_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0 Safari/537.36";

/// A song on Genius.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeniusSong {
    pub id: u64,
    pub title: String,
    /// The primary artist.
    pub artist: String,
    /// Everyone credited, e.g. "Bladee & Ecco2k".
    pub artists: String,
    /// The lyrics page.
    pub url: String,
    pub art: Option<String>,
    /// From the song's own page (see [`Genius::details`]); empty when it is on no album.
    pub album: String,
    /// "2018-06-15" (or less precise).
    pub release_date: String,
}

/// One Genius client for the app, so lookups are shared and remembered.
pub fn shared() -> std::sync::Arc<Genius> {
    static GENIUS: std::sync::OnceLock<std::sync::Arc<Genius>> = std::sync::OnceLock::new();
    GENIUS
        .get_or_init(|| std::sync::Arc::new(Genius::new(crate::http::client())))
        .clone()
}

pub struct Genius {
    http: reqwest::Client,
    root: String,
    /// Matches by "artist\u{1f}title\u{1f}mode" (lowercase); `None` = not on Genius.
    found: Mutex<HashMap<String, Option<GeniusSong>>>,
}

/// Whose upload a song is, which decides how freely Genius may correct the artist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// The artist must match (a typo or capitalisation may differ).
    SameArtist,
    /// A SoundCloud upload: a re-upload channel ("Nightcore Reality", "xyz lyrics") may be
    /// replaced by the song's real artist.
    Upload,
}

impl Genius {
    pub fn new(http: reqwest::Client) -> Genius {
        Genius {
            http,
            root: ROOT.to_string(),
            found: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_root(mut self, root: &str) -> Genius {
        self.root = root.to_string();
        self
    }

    async fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let resp = self
            .http
            .get(format!("{}{path}", self.root))
            .query(query)
            .header(reqwest::header::USER_AGENT, BROWSER_UA)
            .timeout(Duration::from_secs(12))
            .send()
            .await
            .context("Genius request failed")?
            .error_for_status()?;
        resp.json().await.context("unexpected answer from Genius")
    }

    pub async fn search(&self, query: &str) -> Result<Vec<GeniusSong>> {
        let v = self
            .get_json("/api/search/song", &[("q", query), ("per_page", "10")])
            .await?;
        Ok(parse_search(&v))
    }

    /// Adds the album and release date (only the song's own page has them).
    pub async fn details(&self, song: &GeniusSong) -> Result<GeniusSong> {
        let v = self.get_json(&format!("/api/songs/{}", song.id), &[]).await?;
        let s = v.pointer("/response/song").context("Genius returned no song")?;
        let mut out = song.clone();
        let text = |p: &str| {
            s.pointer(p)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        out.album = text("/album/name");
        out.release_date = text("/release_date");
        if let Some(cover) = s.pointer("/album/cover_art_url").and_then(Value::as_str) {
            out.art = Some(cover.to_string());
        }
        Ok(out)
    }

    /// The song on Genius with its album, or `None` when Genius doesn't clearly have it.
    /// Remembered, so repeated plays don't ask again.
    pub async fn find(&self, artist: &str, title: &str, mode: Match) -> Option<GeniusSong> {
        let key = format!("{}\u{1f}{}\u{1f}{mode:?}", artist.to_lowercase(), title.to_lowercase());
        if let Some(known) = self.found.lock().unwrap().get(&key) {
            return known.clone();
        }
        let hits = match self.search(&format!("{artist} {title}")).await {
            Ok(hits) => hits,
            Err(e) => {
                // Network trouble: try again next time.
                tracing::debug!("genius: search for {artist} - {title} failed: {e:#}");
                return None;
            }
        };
        let picked = pick(&hits, artist, title, mode).cloned();
        let found = match picked {
            Some(song) => match self.details(&song).await {
                Ok(full) => Some(full),
                Err(e) => {
                    tracing::debug!("genius: details of {} failed: {e:#}", song.id);
                    Some(song)
                }
            },
            None => None,
        };
        let mut cache = self.found.lock().unwrap();
        if cache.len() > 2000 {
            cache.clear();
        }
        cache.insert(key, found.clone());
        found
    }

    /// Plain lyrics from the song's Genius page.
    pub async fn lyrics(&self, artist: &str, title: &str) -> Result<Option<String>> {
        let Some(song) = self.find(artist, title, Match::SameArtist).await else {
            return Ok(None);
        };
        if song.url.is_empty() {
            return Ok(None);
        }
        let url = if song.url.starts_with("http") {
            song.url.clone()
        } else {
            format!("{}{}", self.root, song.url)
        };
        let html = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, BROWSER_UA)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .context("Genius request failed")?
            .error_for_status()?
            .text()
            .await?;
        Ok(extract_lyrics(&html))
    }
}

/// Songs in a `/api/search/song` (or `/api/search/multi`) answer.
pub fn parse_search(v: &Value) -> Vec<GeniusSong> {
    let mut out: Vec<GeniusSong> = Vec::new();
    let sections = v.pointer("/response/sections").and_then(Value::as_array);
    let hits = sections
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("hits").and_then(Value::as_array))
        .flatten();
    for hit in hits {
        let is_song = hit.get("type").and_then(Value::as_str) == Some("song")
            || hit.pointer("/result/_type").and_then(Value::as_str) == Some("song");
        let Some(r) = hit.get("result").filter(|_| is_song) else {
            continue;
        };
        let text = |p: &str| {
            r.pointer(p)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let Some(id) = r.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if out.iter().any(|s| s.id == id) {
            continue;
        }
        out.push(GeniusSong {
            id,
            title: text("/title"),
            artist: text("/primary_artist/name"),
            artists: text("/artist_names"),
            url: text("/url"),
            art: Some(text("/song_art_image_url")).filter(|a| !a.is_empty()),
            ..GeniusSong::default()
        });
    }
    out
}

/// Words in uploader names of channels that re-upload other people's songs.
const REUPLOADERS: &[&str] = &[
    "nightcore",
    "lyrics",
    "lyric",
    "sped",
    "slowed",
    "reupload",
    "reuploads",
    "archive",
    "uploads",
    "music",
    "records",
    "recordings",
    "tv",
    "channel",
    "radio",
    "vibes",
    "playlist",
    "daycore",
    "8d",
    "audio",
];

fn looks_like_reupload(uploader: &str) -> bool {
    let lower = uploader.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| REUPLOADERS.contains(&w))
}

/// The hit that is this song: same title, and the same artist (or, for a re-upload channel,
/// the top hit with that title).
pub fn pick<'a>(hits: &'a [GeniusSong], artist: &str, title: &str, mode: Match) -> Option<&'a GeniusSong> {
    let want_title = normalize_title(title);
    let want_artist = normalize_artist(artist);
    if want_title.is_empty() {
        return None;
    }
    let squash = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    let same_title = |h: &&GeniusSong| normalize_title(&h.title) == want_title;
    let same_artist = |h: &&GeniusSong| {
        let primary = normalize_artist(&h.artist);
        let everyone = squash(&h.artists);
        !want_artist.is_empty()
            && (primary == want_artist
                || squash(&primary) == squash(&want_artist)
                || everyone.contains(&squash(&want_artist)))
    };
    if let Some(hit) = hits.iter().filter(same_title).find(same_artist) {
        return Some(hit);
    }
    if mode == Match::Upload && looks_like_reupload(artist) {
        // Genius ranks the original first.
        return hits.iter().take(3).find(same_title);
    }
    None
}

/// The lyrics on a Genius song page, as plain text (section headers like "[Chorus]" kept).
pub fn extract_lyrics(html: &str) -> Option<String> {
    const MARK: &str = "data-lyrics-container=\"true\"";
    let mut parts = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(MARK) {
        let open_end = i + rest[i..].find('>')? + 1;
        let (inner, after) = element_contents(&rest[open_end..], "div");
        parts.push(html_to_text(&remove_excluded(inner)));
        rest = after;
    }
    let text = parts.join("\n");
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim) {
        // At most one empty line in a row.
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let text = lines.join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// The inside of an element whose opening tag ended just before `s`, and what follows its
/// closing tag. Nested elements of the same name are skipped.
fn element_contents<'a>(s: &'a str, tag: &str) -> (&'a str, &'a str) {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut depth = 1;
    let mut pos = 0;
    loop {
        let next_open = s[pos..].find(&open).map(|i| pos + i);
        let next_close = s[pos..].find(&close).map(|i| pos + i);
        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                pos = o + open.len();
            }
            (_, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    return (&s[..c], &s[c + close.len()..]);
                }
                pos = c + close.len();
            }
            (_, None) => return (s, ""),
        }
    }
}

/// Drops the parts Genius marks as not part of the lyrics (the "N Contributors" header etc.).
fn remove_excluded(html: &str) -> String {
    const MARK: &str = "data-exclude-from-selection=\"true\"";
    let mut out = String::new();
    let mut rest = html;
    while let Some(i) = rest.find(MARK) {
        let Some(start) = rest[..i].rfind('<') else { break };
        let tag: String = rest[start + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        let Some(open_len) = rest[i..].find('>') else { break };
        out.push_str(&rest[..start]);
        let (_, after) = element_contents(&rest[i + open_len + 1..], &tag);
        rest = after;
    }
    out.push_str(rest);
    out
}

fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + len].trim().to_lowercase();
        if tag == "br" || tag.starts_with("br ") || tag.starts_with("br/") {
            out.push('\n');
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    decode_entities(&out)
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let end = after.find(';').filter(|e| *e <= 10);
        let decoded = end.and_then(|e| {
            let name = &after[..e];
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ if name.starts_with("#x") || name.starts_with("#X") => {
                    char::from_u32(u32::from_str_radix(&name[2..], 16).ok()?)?
                }
                _ if name.starts_with('#') => char::from_u32(name[1..].parse().ok()?)?,
                _ => return None,
            };
            Some((c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &after[e + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: u64, title: &str, artist: &str, artists: &str) -> GeniusSong {
        GeniusSong {
            id,
            title: title.into(),
            artist: artist.into(),
            artists: artists.into(),
            ..GeniusSong::default()
        }
    }

    const SEARCH: &str = r#"{"meta":{"status":200},"response":{"sections":[{"type":"song","hits":[
        {"highlights":[],"index":"song","type":"song","result":{"_type":"song","artist_names":"Bladee",
         "full_title":"Waster by Bladee","id":3820129,"path":"/Bladee-waster-lyrics",
         "primary_artist":{"_type":"artist","id":373340,"name":"Bladee"},
         "song_art_image_url":"https://images.genius.com/waster.jpg","title":"Waster",
         "url":"https://genius.com/Bladee-waster-lyrics"}},
        {"index":"song","type":"song","result":{"_type":"song","artist_names":"Someone Else",
         "id":1,"primary_artist":{"name":"Someone Else"},"title":"Waster","url":"https://genius.com/x"}}
    ],"next_page":2}]}}"#;

    #[test]
    fn parses_search() {
        let hits = parse_search(&serde_json::from_str(SEARCH).unwrap());
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, 3820129);
        assert_eq!((hits[0].title.as_str(), hits[0].artist.as_str()), ("Waster", "Bladee"));
        assert_eq!(hits[0].url, "https://genius.com/Bladee-waster-lyrics");
        assert_eq!(hits[0].art.as_deref(), Some("https://images.genius.com/waster.jpg"));
        assert!(parse_search(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn picks_the_right_song() {
        let hits = vec![
            song(1, "Angel with a Shotgun", "The Cab", "The Cab"),
            song(2, "Waster", "Bladee", "Bladee"),
            song(3, "Be Nice 2 Me", "Bladee", "Bladee & Ecco2k"),
        ];
        // Capitalisation, featured artists and "(feat. …)" don't matter.
        assert_eq!(
            pick(&hits, "bladee", "Waster", Match::SameArtist).map(|s| s.id),
            Some(2)
        );
        assert_eq!(
            pick(&hits, "Ecco2k", "Be Nice 2 Me (feat. Bladee)", Match::SameArtist).map(|s| s.id),
            Some(3)
        );
        // A different artist is not "corrected"…
        assert_eq!(
            pick(&hits, "Some Band", "Angel with a Shotgun", Match::SameArtist),
            None
        );
        assert_eq!(pick(&hits, "Some Band", "Angel with a Shotgun", Match::Upload), None);
        // …unless it is a re-upload channel.
        assert_eq!(
            pick(&hits, "Nightcore Reality", "Angel With A Shotgun", Match::Upload).map(|s| s.id),
            Some(1)
        );
        assert_eq!(
            pick(&hits, "Nightcore Reality", "Angel With A Shotgun", Match::SameArtist),
            None
        );
        assert_eq!(pick(&hits, "Bladee", "Other Song", Match::Upload), None);
    }

    #[test]
    fn lyrics_from_the_song_page() {
        let html = r#"<html><div class="x"><div data-lyrics-container="true" class="Lyrics__Container">
            <div data-exclude-from-selection="true" class="LyricsHeader"><div>12 Contributors</div>Waster Lyrics</div>
            [Intro]<br/>I'm a waster, &quot;yeah&quot;<br><a href="/1"><span class="ReferentFragment">Don&#x27;t you know</span></a><br/>
            <br/><br/>[Chorus]<br/>Rock &amp; roll</div><div class="Ad">ad</div>
            <div data-lyrics-container="true">Last line &#8212; done<br/></div></div></html>"#;
        assert_eq!(
            extract_lyrics(html).unwrap(),
            "[Intro]\nI'm a waster, \"yeah\"\nDon't you know\n\n[Chorus]\nRock & roll\nLast line — done"
        );
        assert_eq!(extract_lyrics("<html>This song is an instrumental</html>"), None);
    }

    /// The whole lookup against a stand-in for genius.com.
    #[tokio::test]
    async fn finds_details_and_lyrics() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let search = SEARCH.replace(
            "https://genius.com/Bladee-waster-lyrics",
            &format!("{base}/Bladee-waster-lyrics"),
        );
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                let body = if path.starts_with("/api/search/song") {
                    search.clone()
                } else if path == "/api/songs/3820129" {
                    r#"{"response":{"song":{"title":"Waster","album":{"name":"Icedancer","cover_art_url":"https://images.genius.com/icedancer.jpg"},"release_date":"2018-06-15"}}}"#.to_string()
                } else if path == "/Bladee-waster-lyrics" {
                    r#"<div data-lyrics-container="true">I'm a waster<br/>Yeah</div>"#.to_string()
                } else {
                    String::new()
                };
                let status = if body.is_empty() { "404 Not Found" } else { "200 OK" };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let genius = Genius::new(reqwest::Client::builder().no_proxy().build().unwrap()).with_root(&base);
        let found = genius.find("bladee", "Waster", Match::SameArtist).await.unwrap();
        assert_eq!((found.artist.as_str(), found.album.as_str()), ("Bladee", "Icedancer"));
        assert_eq!(found.release_date, "2018-06-15");
        assert_eq!(found.art.as_deref(), Some("https://images.genius.com/icedancer.jpg"));
        assert_eq!(
            genius.lyrics("Bladee", "Waster").await.unwrap().as_deref(),
            Some("I'm a waster\nYeah")
        );
        assert_eq!(genius.find("Nobody", "Unknown", Match::SameArtist).await, None);
    }
}
