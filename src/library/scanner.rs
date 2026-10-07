//! Local music folder scanning.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use lofty::config::ParseOptions;
use lofty::prelude::*;
use lofty::probe::Probe;

use crate::model::{now_unix, Source, Track};

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "m4b", "mp4", "aac", "alac", "wav", "wave", "aif",
    "aiff", "aifc", "ape", "wv", "mpc", "wma", "mka", "dsf", "dff", "spx", "tta",
];

const COVER_NAMES: &[&str] = &["cover", "folder", "front", "album", "albumart", "albumartsmall", "artwork"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub struct ScanResult {
    /// New or modified tracks.
    pub changed: Vec<Track>,
    /// mtimes of `changed` tracks.
    pub mtimes: HashMap<String, i64>,
    /// Local track ids that no longer exist on disk.
    pub removed: Vec<String>,
    pub total_files: usize,
}

fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Scans `folders` and returns what changed compared to `known` (track id -> mtime).
/// `progress(done, total)` is called periodically from worker threads.
pub fn scan(
    folders: &[PathBuf],
    known: &HashMap<String, i64>,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> ScanResult {
    let mut files = Vec::new();
    for folder in folders {
        for entry in walkdir::WalkDir::new(folder).follow_links(true).into_iter().flatten() {
            if entry.file_type().is_file() && is_audio_file(entry.path()) {
                files.push(entry.into_path());
            }
        }
    }
    let total = files.len();

    let mut seen = HashSet::with_capacity(total);
    let mut todo = Vec::new();
    for path in files {
        let id = Track::local_id(&path.to_string_lossy());
        let mtime = mtime_of(&path);
        if known.get(&id) != Some(&mtime) {
            todo.push((path, mtime));
        }
        seen.insert(id);
    }
    let removed: Vec<String> = known.keys().filter(|id| !seen.contains(*id)).cloned().collect();

    // Tag parsing is IO + CPU bound, spread it over a few threads.
    let done = AtomicUsize::new(total - todo.len());
    progress(done.load(Ordering::Relaxed), total);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 4);
    let chunk = todo.len().div_ceil(threads).max(1);
    let cover_cache = std::sync::Mutex::new(HashMap::<PathBuf, Option<String>>::new());
    let now = now_unix();

    let changed: Vec<(Track, i64)> = std::thread::scope(|s| {
        let handles: Vec<_> = todo
            .chunks(chunk)
            .map(|part| {
                let done = &done;
                let cover_cache = &cover_cache;
                s.spawn(move || {
                    let mut out = Vec::with_capacity(part.len());
                    for (path, mtime) in part {
                        let cover = path.parent().and_then(|dir| {
                            let mut cache = cover_cache.lock().unwrap();
                            cache
                                .entry(dir.to_path_buf())
                                .or_insert_with(|| find_cover(dir))
                                .clone()
                        });
                        out.push((read_track(path, cover, now), *mtime));
                        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if n % 50 == 0 {
                            progress(n, total);
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    progress(total, total);

    let mut mtimes = HashMap::with_capacity(changed.len());
    let changed = changed
        .into_iter()
        .map(|(t, m)| {
            mtimes.insert(t.id.clone(), m);
            t
        })
        .collect();
    ScanResult {
        changed,
        mtimes,
        removed,
        total_files: total,
    }
}

/// Reads tags of one file. Never fails: falls back to the file name.
pub fn read_track(path: &Path, cover: Option<String>, added_at: i64) -> Track {
    let path_str = path.to_string_lossy().to_string();
    let mut track = Track {
        id: Track::local_id(&path_str),
        source: Source::Local,
        title: path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        artist: String::new(),
        album: path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        duration_ms: 0,
        track_no: None,
        // No folder image: point at the audio file itself, the art loader reads embedded pictures.
        art: Some(cover.unwrap_or_else(|| path_str.clone())),
        uri: path_str,
        added_at,
    };

    let parsed = Probe::open(path)
        .map(|p| p.options(ParseOptions::new().read_cover_art(false)))
        .and_then(|p| p.read());
    match parsed {
        Ok(file) => {
            track.duration_ms = file.properties().duration().as_millis() as u64;
            if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
                if let Some(t) = tag.title().filter(|t| !t.trim().is_empty()) {
                    track.title = t.trim().to_string();
                }
                if let Some(a) = tag.artist().filter(|a| !a.trim().is_empty()) {
                    track.artist = a.trim().to_string();
                } else if let Some(a) = tag.get_string(ItemKey::AlbumArtist) {
                    track.artist = a.trim().to_string();
                }
                if let Some(a) = tag.album().filter(|a| !a.trim().is_empty()) {
                    track.album = a.trim().to_string();
                }
                track.track_no = tag.track();
            }
        }
        Err(e) => tracing::debug!("could not read tags of {}: {e}", path.display()),
    }
    if track.artist.is_empty() {
        track.artist = "Unknown Artist".into();
    }
    track
}

/// Finds a cover image in an album folder.
pub fn find_cover(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut fallback = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if COVER_NAMES.contains(&stem.as_str()) {
            return Some(path.to_string_lossy().to_string());
        }
        if fallback.is_none() {
            fallback = Some(path.to_string_lossy().to_string());
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_audio_extensions() {
        assert!(is_audio_file(Path::new("/a/b/song.FLAC")));
        assert!(is_audio_file(Path::new("x.opus")));
        assert!(!is_audio_file(Path::new("cover.jpg")));
        assert!(!is_audio_file(Path::new("noext")));
    }

    #[test]
    fn scan_finds_new_and_removed_files() {
        let dir = std::env::temp_dir().join(format!("medley-scan-test-{}", std::process::id()));
        let album = dir.join("Artist").join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 Song.mp3"), b"not really audio").unwrap();
        std::fs::write(album.join("cover.jpg"), b"jpg").unwrap();

        let mut known = HashMap::new();
        known.insert("local:/gone.mp3".to_string(), 1);
        let result = scan(std::slice::from_ref(&dir), &known, &|_, _| {});
        assert_eq!(result.total_files, 1);
        assert_eq!(result.changed.len(), 1);
        assert_eq!(result.removed, vec!["local:/gone.mp3".to_string()]);
        let t = &result.changed[0];
        assert_eq!(t.title, "01 Song");
        assert_eq!(t.album, "Album");
        assert!(t.art.as_deref().unwrap().ends_with("cover.jpg"));

        // Unchanged files are skipped on the next scan.
        let known: HashMap<String, i64> = result.mtimes.clone();
        let again = scan(std::slice::from_ref(&dir), &known, &|_, _| {});
        assert!(again.changed.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
