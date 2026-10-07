//! Saving songs as files. SoundCloud songs are downloaded directly. Spotify and Apple Music
//! audio is DRM-protected, so for those the same recording is found on YouTube (with yt-dlp) or
//! SoundCloud and downloaded from there. Every file is then tagged with the song's details
//! (from Spotify when it is a Spotify song), its cover and its lyrics.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use librespot_core::session::Session;
use tracing::{info, warn};

use crate::integrations::lyrics::{self, LyricsFetcher};
use crate::library::tags::{self, Metadata};
use crate::model::{Source, Track};
use crate::providers::soundcloud::{self, DownloadKind, SoundCloud};
use crate::providers::{spotify_internal, youtube};

pub type Progress<'a> = &'a (dyn Fn(f32) + Send + Sync);

pub struct Downloader {
    pub soundcloud: Arc<SoundCloud>,
    pub http: reqwest::Client,
    /// Lyrics to embed; `None` leaves them out.
    pub lyrics: Option<Arc<LyricsFetcher>>,
    /// For the full details of Spotify songs.
    pub spotify: Option<Session>,
    /// yt-dlp binary; empty = don't look on YouTube.
    pub ytdlp: String,
}

/// A saved song.
#[derive(Debug)]
pub struct Saved {
    pub path: PathBuf,
    /// Where the audio came from, for the Downloads list.
    pub from: String,
}

impl Downloader {
    pub async fn download(&self, track: &Track, dir: &Path, progress: Progress<'_>) -> Result<Saved> {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("couldn't create {}", dir.display()))?;
        let (path, mut meta, from, keep_existing) = match track.source {
            Source::SoundCloud => {
                let saved = self.soundcloud.download(track, dir, progress).await?;
                let original = saved.kind == DownloadKind::Original;
                let from = if original { "original file" } else { "SoundCloud" };
                (saved.path, saved.meta, from.to_string(), original)
            }
            Source::Spotify | Source::AppleMusic => {
                let (path, from) = self.find_and_download(track, dir, progress).await?;
                (path, self.details(track).await, from, false)
            }
            Source::Local => bail!("this song is already a file on your computer"),
        };
        if meta.album.trim().is_empty() {
            // Released on its own: a single.
            meta.album = meta.title.clone();
        }
        if let Some(fetcher) = &self.lyrics {
            if let Some(found) = fetcher.fetch(track).await {
                meta.lyrics = lyrics::to_tag_text(&found);
            }
        }
        let cover = self.cover(&meta.cover_urls).await;
        if make_taggable(&path).await {
            let (file, meta_, cover_) = (path.clone(), meta.clone(), cover);
            let tagged =
                tokio::task::spawn_blocking(move || tags::write(&file, &meta_, cover_.as_deref(), keep_existing)).await;
            match tagged {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("{e:#}"),
                Err(e) => warn!("tagging {} failed: {e}", path.display()),
            }
        }
        let path = rename(path, dir, &soundcloud::file_stem(&meta.artist, &meta.title)).await?;
        info!("saved \"{}\" ({from}) to {}", meta.title, path.display());
        Ok(Saved { path, from })
    }

    /// The song's details: everything Spotify knows for Spotify songs, else what the library
    /// has.
    async fn details(&self, track: &Track) -> Metadata {
        if track.source == Source::Spotify {
            if let Some(session) = &self.spotify {
                match spotify_internal::track_details(session, &track.id).await {
                    Ok(meta) if !meta.title.is_empty() => return meta,
                    Ok(_) => {}
                    Err(e) => warn!("Spotify details of {}: {e:#}", track.id),
                }
            }
        }
        basic_metadata(track)
    }

    /// Finds the same recording on YouTube or SoundCloud and downloads it.
    async fn find_and_download(&self, track: &Track, dir: &Path, progress: Progress<'_>) -> Result<(PathBuf, String)> {
        let artist = lyrics::first_artist(&track.artist);
        let title = lyrics::clean_title(&track.title);
        let duration = (track.duration_ms > 0).then(|| track.duration_ms as f64 / 1000.0);

        let mut youtube_problem = None;
        if self.ytdlp.is_empty() {
            youtube_problem = Some("YouTube is turned off in Settings".to_string());
        } else {
            match youtube::search(&self.ytdlp, &format!("{artist} - {title}"), 10).await {
                Ok(videos) => {
                    let ranked = youtube::rank(&videos, &track.title, &track.artist, duration);
                    if ranked.is_empty() {
                        youtube_problem = Some("no matching video on YouTube".into());
                    }
                    for video in ranked.into_iter().take(3) {
                        match youtube::download(&self.ytdlp, &video.id, dir, progress).await {
                            Ok(path) => return Ok((path, "YouTube".into())),
                            Err(e) => {
                                warn!("YouTube {} for {}: {e:#}", video.id, track.id);
                                let fatal = e.to_string().contains("ffmpeg");
                                youtube_problem = Some(format!("{e:#}"));
                                if fatal {
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) if e.is::<youtube::NotInstalled>() => {
                    youtube_problem = Some("install yt-dlp to look on YouTube too (sudo pacman -S yt-dlp)".into());
                }
                Err(e) => youtube_problem = Some(format!("{e:#}")),
            }
        }

        // An upload of the same recording on SoundCloud.
        let soundcloud_problem = match self.soundcloud.search(&format!("{artist} {title}"), 10).await {
            Ok(found) => {
                let mut problem = "no full-length upload on SoundCloud".to_string();
                for candidate in crate::service::ranked_matches(track, &found).into_iter().take(2) {
                    match self.soundcloud.download(&candidate, dir, progress).await {
                        Ok(saved) => return Ok((saved.path, "SoundCloud".into())),
                        Err(e) => problem = format!("SoundCloud: {e:#}"),
                    }
                }
                problem
            }
            Err(e) => format!("SoundCloud: {e:#}"),
        };
        match youtube_problem {
            Some(yt) => bail!("Not found: {soundcloud_problem}; {yt}"),
            None => bail!("Not found: {soundcloud_problem}"),
        }
    }

    /// The first cover that downloads.
    async fn cover(&self, urls: &[String]) -> Option<Vec<u8>> {
        for url in urls.iter().filter(|u| u.starts_with("http")) {
            if let Some(bytes) = fetch_image(&self.http, url).await {
                return Some(bytes);
            }
        }
        None
    }
}

/// Details from the library's copy of the song (no Spotify session, or Apple Music).
pub fn basic_metadata(track: &Track) -> Metadata {
    let url = match track.source {
        Source::Spotify => track
            .id
            .strip_prefix("spotify:track:")
            .map(|id| format!("https://open.spotify.com/track/{id}"))
            .unwrap_or_default(),
        _ if track.uri.starts_with("http") => track.uri.clone(),
        _ => String::new(),
    };
    Metadata {
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        album_artist: lyrics::first_artist(&track.artist),
        track: track.track_no,
        url,
        cover_urls: track.art.iter().cloned().collect(),
        ..Metadata::default()
    }
}

/// Cover art; at most a few MB.
async fn fetch_image(http: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    let resp = http.get(url).send().await.ok()?.error_for_status().ok()?;
    if resp.content_length().is_some_and(|n| n > 8 << 20) {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    (bytes.len() <= 8 << 20).then(|| bytes.to_vec())
}

/// Fragmented MP4 (SoundCloud's AAC streams) can't take tags; ffmpeg rewrites it as a regular
/// MP4 without re-encoding. Returns whether the file can be tagged.
async fn make_taggable(path: &Path) -> bool {
    let is_mp4 = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("m4a"));
    if !is_mp4 || !is_fragmented_mp4(path).await {
        return true;
    }
    let tmp = path.with_extension("remux.m4a");
    let status = tokio::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-map", "0:a", "-c", "copy", "-movflags", "+faststart"])
        .arg(&tmp)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await;
    match status {
        Ok(s) if s.success() && tokio::fs::rename(&tmp, path).await.is_ok() => true,
        _ => {
            let _ = tokio::fs::remove_file(&tmp).await;
            warn!(
                "{} is fragmented MP4 and ffmpeg couldn't rewrite it; leaving it untagged",
                path.display()
            );
            false
        }
    }
}

/// Whether the top level of an MP4 file has fragments (`moof` boxes).
async fn is_fragmented_mp4(path: &Path) -> bool {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return false;
    };
    let mut pos = 0u64;
    for _ in 0..64 {
        let mut header = [0u8; 16];
        if file.seek(std::io::SeekFrom::Start(pos)).await.is_err() || file.read_exact(&mut header[..8]).await.is_err() {
            return false;
        }
        let kind = &header[4..8];
        if kind == b"moof" {
            return true;
        }
        let mut size = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
        if size == 1 {
            if file.read_exact(&mut header[8..16]).await.is_err() {
                return false;
            }
            size = u64::from_be_bytes(header[8..16].try_into().unwrap());
        }
        if size < 8 {
            return false;
        }
        pos += size;
    }
    false
}

/// Renames a fresh download to "Artist - Title.ext" (unless it already has that name).
async fn rename(path: PathBuf, dir: &Path, stem: &str) -> Result<PathBuf> {
    let current = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if current == stem || current.starts_with(&format!("{stem} (")) {
        return Ok(path);
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "mp3".into());
    let target = soundcloud::unique_path(dir, stem, &ext);
    tokio::fs::rename(&path, &target)
        .await
        .with_context(|| format!("couldn't save {}", target.display()))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_details_from_the_library() {
        let t = Track {
            id: "spotify:track:abc".into(),
            source: Source::Spotify,
            title: "Song".into(),
            artist: "A, B".into(),
            album: "Album".into(),
            duration_ms: 1000,
            track_no: Some(4),
            art: Some("https://i.scdn.co/image/x".into()),
            uri: "spotify:track:abc".into(),
            added_at: 0,
        };
        let m = basic_metadata(&t);
        assert_eq!(m.url, "https://open.spotify.com/track/abc");
        assert_eq!((m.album_artist.as_str(), m.track), ("A", Some(4)));
        assert_eq!(m.cover_urls, vec!["https://i.scdn.co/image/x".to_string()]);
    }

    #[tokio::test]
    async fn fragmented_mp4_is_detected() {
        let dir = std::env::temp_dir().join(format!("multimusic-mp4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = |boxes: &[(&[u8; 4], u32)]| {
            let mut out = Vec::new();
            for (kind, size) in boxes {
                out.extend_from_slice(&size.to_be_bytes());
                out.extend_from_slice(&kind[..]);
                out.resize(out.len() + *size as usize - 8, 0);
            }
            out
        };
        let plain = dir.join("plain.m4a");
        std::fs::write(&plain, mp4(&[(b"ftyp", 16), (b"moov", 24), (b"mdat", 40)])).unwrap();
        assert!(!is_fragmented_mp4(&plain).await);
        let fragmented = dir.join("frag.m4a");
        std::fs::write(
            &fragmented,
            mp4(&[(b"ftyp", 16), (b"moov", 24), (b"moof", 16), (b"mdat", 40)]),
        )
        .unwrap();
        assert!(is_fragmented_mp4(&fragmented).await);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A real fragmented AAC file (like SoundCloud's HLS streams) is rewritten and then tagged.
    /// Needs ffmpeg, which mpv depends on.
    #[tokio::test]
    async fn fragmented_aac_gets_tags() {
        let dir = std::env::temp_dir().join(format!("multimusic-aac-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.m4a");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=44100:cl=mono",
                "-t",
                "1",
            ])
            .args(["-c:a", "aac", "-movflags", "frag_keyframe+empty_moov"])
            .arg(&path)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg not available, skipping");
            return;
        }
        assert!(is_fragmented_mp4(&path).await);
        assert!(make_taggable(&path).await);
        assert!(!is_fragmented_mp4(&path).await);
        let meta = Metadata {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            ..Metadata::default()
        };
        tags::write(&path, &meta, None, false).unwrap();
        let track = crate::library::scanner::read_track(&path, None, 0);
        assert_eq!((track.title.as_str(), track.album.as_str()), ("Song", "Album"));
        assert!(track.duration_ms > 900, "{}", track.duration_ms);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
