//! In-memory music library, backed by SQLite.

pub mod db;
pub mod m3u;
pub mod scanner;

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::model::{match_key, Playlist, PlaylistKind, Source, Track};

pub use db::Db;

pub const LIKED_ID: &str = "liked";

#[derive(Debug, Clone)]
pub struct Album {
    pub key: String,
    pub name: String,
    pub artist: String,
    pub art: Option<String>,
    pub track_ids: Vec<String>,
}

#[derive(Default)]
pub struct Library {
    pub tracks: HashMap<String, Track>,
    pub playlists: Vec<Playlist>,
    /// Local track ids sorted by artist, album, track number.
    pub local: Vec<String>,
    pub albums: Vec<Album>,
    /// Recently played track ids, newest first.
    pub recent: Vec<String>,
    /// Bumped on every change so views can invalidate caches.
    pub version: u64,
    liked: HashSet<String>,
    /// match key -> local track id, to play imported songs from local files.
    local_match: HashMap<String, String>,
}

impl Library {
    pub fn load(db: &Db) -> Result<Library> {
        let mut lib = Library {
            tracks: db.load_tracks()?.into_iter().map(|t| (t.id.clone(), t)).collect(),
            playlists: db.load_playlists()?,
            recent: db.recent_plays(50)?,
            ..Default::default()
        };
        if !lib.playlists.iter().any(|p| p.id == LIKED_ID) {
            lib.playlists.insert(0, liked_playlist());
        }
        lib.reindex();
        Ok(lib)
    }

    /// Recomputes derived data after tracks/playlists changed.
    pub fn reindex(&mut self) {
        let mut local: Vec<&Track> = self.tracks.values().filter(|t| t.source == Source::Local).collect();
        local.sort_by(|a, b| {
            sort_key(&a.artist)
                .cmp(&sort_key(&b.artist))
                .then_with(|| sort_key(&a.album).cmp(&sort_key(&b.album)))
                .then_with(|| a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
                .then_with(|| a.title.cmp(&b.title))
        });

        let mut albums: Vec<Album> = Vec::new();
        let mut album_index: HashMap<String, usize> = HashMap::new();
        self.local_match.clear();
        for t in &local {
            self.local_match.entry(t.match_key()).or_insert_with(|| t.id.clone());
            let key = format!("{}\u{1f}{}", sort_key(&t.album), sort_key(&t.artist));
            let idx = *album_index.entry(key.clone()).or_insert_with(|| {
                albums.push(Album {
                    key,
                    name: t.album.clone(),
                    artist: t.artist.clone(),
                    art: t.art.clone(),
                    track_ids: Vec::new(),
                });
                albums.len() - 1
            });
            albums[idx].track_ids.push(t.id.clone());
        }
        self.local = local.iter().map(|t| t.id.clone()).collect();
        albums.sort_by(|a, b| sort_key(&a.artist).cmp(&sort_key(&b.artist)).then_with(|| a.name.cmp(&b.name)));
        self.albums = albums;

        self.liked = self
            .playlists
            .iter()
            .find(|p| p.id == LIKED_ID)
            .map(|p| p.track_ids.iter().cloned().collect())
            .unwrap_or_default();
        self.sort_playlists();
        self.version += 1;
    }

    fn sort_playlists(&mut self) {
        fn rank(k: PlaylistKind) -> u8 {
            match k {
                PlaylistKind::Liked => 0,
                PlaylistKind::Custom => 1,
                PlaylistKind::M3u => 2,
                PlaylistKind::SpotifyLiked => 3,
                PlaylistKind::Spotify => 4,
                PlaylistKind::SoundCloudLikes => 5,
                PlaylistKind::SoundCloud => 6,
                PlaylistKind::AppleMusic => 7,
            }
        }
        // Stable: keeps service order (e.g. Spotify's own playlist order) within a kind.
        self.playlists.sort_by_key(|p| rank(p.kind));
    }

    pub fn get(&self, id: &str) -> Option<&Track> {
        self.tracks.get(id)
    }

    pub fn playlist(&self, id: &str) -> Option<&Playlist> {
        self.playlists.iter().find(|p| p.id == id)
    }

    pub fn playlist_mut(&mut self, id: &str) -> Option<&mut Playlist> {
        self.playlists.iter_mut().find(|p| p.id == id)
    }

    pub fn tracks_for(&self, ids: &[String]) -> Vec<Track> {
        ids.iter().filter_map(|id| self.tracks.get(id).cloned()).collect()
    }

    pub fn is_liked(&self, id: &str) -> bool {
        self.liked.contains(id)
    }

    /// Finds a local file for a song from another source.
    pub fn local_match(&self, artist: &str, title: &str) -> Option<&Track> {
        self.local_match
            .get(&match_key(artist, title))
            .and_then(|id| self.tracks.get(id))
    }

    /// Searches local tracks plus every track that is in a playlist.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Track> {
        let terms: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(&Track, u32)> = self
            .tracks
            .values()
            .filter_map(|t| {
                let title = t.title.to_lowercase();
                let hay = format!("{title} {} {}", t.artist.to_lowercase(), t.album.to_lowercase());
                if terms.iter().all(|term| hay.contains(term.as_str())) {
                    // Rank title matches first, then local files.
                    let mut score = 0;
                    if terms.iter().all(|term| title.contains(term.as_str())) {
                        score += 2;
                    }
                    if t.source == Source::Local {
                        score += 1;
                    }
                    Some((t, score))
                } else {
                    None
                }
            })
            .collect();
        hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.title.cmp(&b.0.title)));
        hits.into_iter().take(limit).map(|(t, _)| t.clone()).collect()
    }

    pub fn playlist_position(&self, id: &str) -> i64 {
        self.playlists.iter().position(|p| p.id == id).unwrap_or(0) as i64
    }
}

pub fn liked_playlist() -> Playlist {
    Playlist {
        id: LIKED_ID.into(),
        name: "Liked Songs".into(),
        kind: PlaylistKind::Liked,
        remote_id: None,
        description: "Your favourites from every source".into(),
        art: None,
        track_ids: Vec::new(),
    }
}

/// Case-insensitive sort key that ignores a leading "The ".
pub fn sort_key(s: &str) -> String {
    let l = s.trim().to_lowercase();
    l.strip_prefix("the ").map(str::to_string).unwrap_or(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, artist: &str, album: &str, title: &str, no: u32) -> Track {
        Track {
            id: id.into(),
            source: if id.starts_with("local:") { Source::Local } else { Source::Spotify },
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration_ms: 1,
            track_no: Some(no),
            art: None,
            uri: id.into(),
            added_at: 0,
        }
    }

    #[test]
    fn indexes_albums_and_matches() {
        let mut lib = Library::default();
        for tr in [
            t("local:/b2", "The Band", "B", "Two", 2),
            t("local:/b1", "The Band", "B", "One", 1),
            t("local:/a1", "Abba", "Gold", "Waterloo", 1),
            t("spotify:track:1", "Abba", "Gold", "Waterloo", 1),
        ] {
            lib.tracks.insert(tr.id.clone(), tr);
        }
        lib.playlists.push(liked_playlist());
        lib.reindex();
        assert_eq!(lib.local, vec!["local:/a1", "local:/b1", "local:/b2"]);
        assert_eq!(lib.albums.len(), 2);
        assert_eq!(lib.albums[1].track_ids, vec!["local:/b1", "local:/b2"]);
        assert_eq!(lib.local_match("ABBA", "Waterloo").unwrap().id, "local:/a1");
        let hits = lib.search("waterloo", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source, Source::Local);
    }
}
