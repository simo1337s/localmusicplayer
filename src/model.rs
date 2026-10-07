//! Core data types shared by every part of the app.

use serde::{Deserialize, Serialize};

/// Where a track comes from (and therefore which engine plays it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    Local,
    Spotify,
    SoundCloud,
    /// Imported from Apple Music. Apple Music streams are DRM protected and can't be
    /// played on Linux, so these are resolved to a local/Spotify/SoundCloud match at play time.
    AppleMusic,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Local => "local",
            Source::Spotify => "spotify",
            Source::SoundCloud => "soundcloud",
            Source::AppleMusic => "applemusic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "local" => Source::Local,
            "spotify" => Source::Spotify,
            "soundcloud" => Source::SoundCloud,
            "applemusic" => Source::AppleMusic,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::Local => "Local",
            Source::Spotify => "Spotify",
            Source::SoundCloud => "SoundCloud",
            Source::AppleMusic => "Apple Music",
        }
    }
}

/// A single playable item from any source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Globally unique key, prefixed by source:
    /// `local:/music/a.flac`, `spotify:track:<base62>`, `soundcloud:<numeric id>`, `applemusic:<id>`.
    pub id: String,
    pub source: Source,
    pub title: String,
    /// Display artist string ("A, B" for multiple artists).
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub track_no: Option<u32>,
    /// Cover art: an http(s) URL, or a local path (image file, or audio file with embedded art).
    pub art: Option<String>,
    /// Source specific playback locator:
    /// local file path, `spotify:track:<id>` URI, SoundCloud permalink URL, or empty for Apple Music.
    pub uri: String,
    /// Unix seconds when this was added to the library/playlist (0 = unknown).
    pub added_at: i64,
}

impl Track {
    pub fn local_id(path: &str) -> String {
        format!("local:{path}")
    }

    pub fn soundcloud_id(id: u64) -> String {
        format!("soundcloud:{id}")
    }

    pub fn applemusic_id(id: &str) -> String {
        format!("applemusic:{id}")
    }

    /// Duration in seconds as f64.
    pub fn duration_secs(&self) -> f64 {
        self.duration_ms as f64 / 1000.0
    }

    /// Normalized "artist - title" key used for fuzzy cross-source matching.
    pub fn match_key(&self) -> String {
        match_key(&self.artist, &self.title)
    }
}

/// Lowercases, strips punctuation and common decorations ("(feat. X)", "- Remastered 2011")
/// so the same song from different sources produces the same key.
pub fn normalize_title(s: &str) -> String {
    let lower = s.to_lowercase();
    // Cut decorations that differ between services.
    let mut cut = lower.as_str();
    for marker in [" (feat", " [feat", " feat.", " ft.", " - remaster", " (remaster", " [remaster", " - single", " - radio edit"] {
        if let Some(i) = cut.find(marker) {
            cut = &cut[..i];
        }
    }
    cut.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The first credited artist, normalized.
pub fn normalize_artist(s: &str) -> String {
    let lower = s.to_lowercase();
    let first = lower
        .split([',', '&', ';', '/'])
        .next()
        .unwrap_or("")
        .split(" feat")
        .next()
        .unwrap_or("")
        .split(" x ")
        .next()
        .unwrap_or("");
    first
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn match_key(artist: &str, title: &str) -> String {
    format!("{}\u{1f}{}", normalize_artist(artist), normalize_title(title))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlaylistKind {
    /// User created playlist (can mix sources).
    Custom,
    /// The built-in cross-source "Liked Songs" playlist.
    Liked,
    Spotify,
    SpotifyLiked,
    SoundCloud,
    SoundCloudLikes,
    AppleMusic,
    /// Imported from an .m3u/.m3u8 file.
    M3u,
}

impl PlaylistKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlaylistKind::Custom => "custom",
            PlaylistKind::Liked => "liked",
            PlaylistKind::Spotify => "spotify",
            PlaylistKind::SpotifyLiked => "spotify_liked",
            PlaylistKind::SoundCloud => "soundcloud",
            PlaylistKind::SoundCloudLikes => "soundcloud_likes",
            PlaylistKind::AppleMusic => "applemusic",
            PlaylistKind::M3u => "m3u",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "custom" => PlaylistKind::Custom,
            "liked" => PlaylistKind::Liked,
            "spotify" => PlaylistKind::Spotify,
            "spotify_liked" => PlaylistKind::SpotifyLiked,
            "soundcloud" => PlaylistKind::SoundCloud,
            "soundcloud_likes" => PlaylistKind::SoundCloudLikes,
            "applemusic" => PlaylistKind::AppleMusic,
            "m3u" => PlaylistKind::M3u,
            _ => return None,
        })
    }

    /// Playlists the user can edit inside the app.
    pub fn is_editable(self) -> bool {
        matches!(self, PlaylistKind::Custom | PlaylistKind::Liked | PlaylistKind::M3u)
    }

    pub fn source(self) -> Option<Source> {
        match self {
            PlaylistKind::Spotify | PlaylistKind::SpotifyLiked => Some(Source::Spotify),
            PlaylistKind::SoundCloud | PlaylistKind::SoundCloudLikes => Some(Source::SoundCloud),
            PlaylistKind::AppleMusic => Some(Source::AppleMusic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    /// Local key, e.g. `custom:<uuid>`, `spotify:<playlist id>`, `liked`.
    pub id: String,
    pub name: String,
    pub kind: PlaylistKind,
    /// Id on the remote service, if any.
    pub remote_id: Option<String>,
    pub description: String,
    pub art: Option<String>,
    pub track_ids: Vec<String>,
}

/// A playlist fetched from a remote service (or file) that still has to be merged into the library.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedPlaylist {
    pub remote_id: String,
    pub name: String,
    pub description: String,
    pub art: Option<String>,
    pub tracks: Vec<Track>,
}

/// One line of time-synced lyrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Lyrics {
    /// Sorted by time. Empty if only plain lyrics are available.
    pub synced: Vec<LyricLine>,
    /// Plain text lyrics (always filled when any lyrics exist).
    pub plain: String,
    pub instrumental: bool,
    /// Where they came from ("LRCLIB", "Embedded tag", "Sidecar .lrc").
    pub provider: String,
}

impl Lyrics {
    /// Index of the line that should be highlighted at `pos_ms`.
    pub fn line_at(&self, pos_ms: u64) -> Option<usize> {
        if self.synced.is_empty() || pos_ms < self.synced[0].time_ms {
            return None;
        }
        let idx = self.synced.partition_point(|l| l.time_ms <= pos_ms);
        Some(idx.saturating_sub(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_keys_ignore_decorations() {
        assert_eq!(
            match_key("Daft Punk", "Get Lucky (feat. Pharrell Williams)"),
            match_key("Daft Punk, Pharrell Williams", "Get Lucky")
        );
        assert_eq!(
            match_key("The Beatles", "Let It Be - Remastered 2009"),
            match_key("the beatles", "Let It Be")
        );
    }

    #[test]
    fn lyric_line_lookup() {
        let l = Lyrics {
            synced: vec![
                LyricLine { time_ms: 1000, text: "a".into() },
                LyricLine { time_ms: 2000, text: "b".into() },
                LyricLine { time_ms: 3000, text: "c".into() },
            ],
            ..Default::default()
        };
        assert_eq!(l.line_at(500), None);
        assert_eq!(l.line_at(1000), Some(0));
        assert_eq!(l.line_at(2500), Some(1));
        assert_eq!(l.line_at(99999), Some(2));
    }
}
