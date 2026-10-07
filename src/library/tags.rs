//! Writes tags (and cover art) into downloaded files.

use std::path::Path;

use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::items::Timestamp;
use lofty::tag::Tag;

/// Everything known about a song, for its file's tags. Empty fields are left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub title: String,
    /// All credited artists ("A, B").
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub genre: String,
    /// "2021", "2021-03" or "2021-03-05".
    pub date: String,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub isrc: String,
    pub label: String,
    pub copyright: String,
    /// The song's page (written as the comment).
    pub url: String,
    /// Plain or time-synced (LRC) lyrics.
    pub lyrics: String,
    /// Cover art to download, best first. Not written itself.
    pub cover_urls: Vec<String>,
}

/// Writes `meta` and `cover` (JPEG or PNG) into the file. With `keep_existing`, only fills in
/// what the file doesn't have yet (for files their uploader tagged).
pub fn write(path: &Path, meta: &Metadata, cover: Option<&[u8]>, keep_existing: bool) -> Result<()> {
    let mut file = lofty::read_from_path(path).with_context(|| format!("couldn't read {}", path.display()))?;
    let kind = file.primary_tag_type();
    if file.tag(kind).is_none() {
        file.insert_tag(Tag::new(kind));
    }
    let tag = file.tag_mut(kind).context("no tag")?;

    let texts = [
        (ItemKey::TrackTitle, &meta.title),
        (ItemKey::TrackArtist, &meta.artist),
        (ItemKey::AlbumTitle, &meta.album),
        (ItemKey::AlbumArtist, &meta.album_artist),
        (ItemKey::Genre, &meta.genre),
        (ItemKey::Isrc, &meta.isrc),
        (ItemKey::Label, &meta.label),
        (ItemKey::CopyrightMessage, &meta.copyright),
        (ItemKey::Comment, &meta.url),
        (ItemKey::Lyrics, &meta.lyrics),
    ];
    for (key, value) in texts {
        let value = value.trim();
        let has = tag.get_string(key).is_some_and(|v| !v.trim().is_empty());
        if !value.is_empty() && !(keep_existing && has) {
            let stored = tag.insert_text(key, value.to_string());
            // ID3 has no label field (taggers use the publisher frame) and keeps lyrics, synced
            // ones in LRC form too, in USLT. Other fields a format can't hold are left out.
            let fallback = match key {
                ItemKey::Label => Some(ItemKey::Publisher),
                ItemKey::Lyrics => Some(ItemKey::UnsyncLyrics),
                _ => None,
            };
            if let Some(other) = fallback.filter(|_| !stored) {
                tag.insert_text(other, value.to_string());
            }
        }
    }
    let numbers = [
        (meta.track, tag.track(), 0),
        (meta.track_total, tag.track_total(), 1),
        (meta.disc, tag.disk(), 2),
        (meta.disc_total, tag.disk_total(), 3),
    ];
    for (value, old, which) in numbers {
        let Some(n) = value.filter(|n| *n > 0) else { continue };
        if keep_existing && old.is_some() {
            continue;
        }
        match which {
            0 => tag.set_track(n),
            1 => tag.set_track_total(n),
            2 => tag.set_disk(n),
            _ => tag.set_disk_total(n),
        }
    }
    if let Ok(date) = meta.date.trim().parse::<Timestamp>() {
        if !(keep_existing && tag.date().is_some()) {
            tag.set_date(date);
        }
    }
    let has_cover = tag.pictures().iter().any(|p| p.pic_type() == PictureType::CoverFront);
    if let Some((data, mime)) = cover.and_then(|d| Some((d, image_mime(d)?))) {
        if !(keep_existing && has_cover) {
            tag.remove_picture_type(PictureType::CoverFront);
            tag.push_picture(
                Picture::unchecked(data.to_vec())
                    .pic_type(PictureType::CoverFront)
                    .mime_type(mime)
                    .build(),
            );
        }
    }
    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("couldn't write tags to {}", path.display()))
}

fn image_mime(data: &[u8]) -> Option<MimeType> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(MimeType::Jpeg)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(MimeType::Png)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny valid WAV file (0.1 s of silence).
    fn wav() -> Vec<u8> {
        let samples = 4410u32;
        let data_len = samples * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&44100u32.to_le_bytes());
        out.extend_from_slice(&88200u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(out.len() + data_len as usize, 0);
        out
    }

    #[test]
    fn writes_full_metadata_and_cover() {
        let path = std::env::temp_dir().join(format!("multimusic-tags-{}.wav", std::process::id()));
        std::fs::write(&path, wav()).unwrap();
        let cover = [&[0xFF, 0xD8, 0xFF, 0xE0][..], &[0u8; 32]].concat();
        let meta = Metadata {
            title: "Song".into(),
            artist: "Someone, Other".into(),
            album: "The Album".into(),
            album_artist: "Someone".into(),
            genre: "Electronic".into(),
            date: "2021-03-05".into(),
            track: Some(3),
            track_total: Some(12),
            disc: Some(1),
            disc_total: Some(2),
            isrc: "USUM72100001".into(),
            label: "Label".into(),
            copyright: "℗ 2021 Label".into(),
            url: "https://open.spotify.com/track/x".into(),
            lyrics: "[00:01.00]Hello\n[00:02.50]World".into(),
            cover_urls: vec![],
        };
        write(&path, &meta, Some(&cover), false).unwrap();

        let file = lofty::read_from_path(&path).unwrap();
        let tag = file.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Song"));
        assert_eq!(tag.artist().as_deref(), Some("Someone, Other"));
        assert_eq!(tag.album().as_deref(), Some("The Album"));
        assert_eq!(tag.get_string(ItemKey::AlbumArtist), Some("Someone"));
        assert_eq!(tag.genre().as_deref(), Some("Electronic"));
        assert_eq!(tag.get_string(ItemKey::Isrc), Some("USUM72100001"));
        let label = tag.get_string(ItemKey::Label).or(tag.get_string(ItemKey::Publisher));
        assert_eq!(label, Some("Label"));
        assert_eq!(tag.get_string(ItemKey::CopyrightMessage), Some("℗ 2021 Label"));
        assert_eq!((tag.track(), tag.track_total()), (Some(3), Some(12)));
        assert_eq!((tag.disk(), tag.disk_total()), (Some(1), Some(2)));
        let date = tag.date().unwrap();
        assert_eq!((date.year, date.month, date.day), (2021, Some(3), Some(5)));
        assert_eq!(tag.pictures().len(), 1);
        assert_eq!(tag.pictures()[0].mime_type(), Some(&MimeType::Jpeg));

        // The app reads the same details (and synced lyrics) back.
        let track = super::super::scanner::read_track(&path, None, 0);
        assert_eq!(
            (track.title.as_str(), track.artist.as_str()),
            ("Song", "Someone, Other")
        );
        assert_eq!((track.album.as_str(), track.track_no), ("The Album", Some(3)));
        assert_eq!(track.duration_ms, 100);
        let lyrics = crate::integrations::lyrics::local_lyrics(&path).unwrap();
        assert_eq!(lyrics.synced.len(), 2);
        assert_eq!(lyrics.synced[1].time_ms, 2500);

        // Filling in keeps what is there.
        let more = Metadata {
            title: "Other".into(),
            genre: "Pop".into(),
            ..Metadata::default()
        };
        write(&path, &more, None, true).unwrap();
        let file = lofty::read_from_path(&path).unwrap();
        let tag = file.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Song"));
        assert_eq!(tag.genre().as_deref(), Some("Electronic"));
        assert_eq!(tag.pictures().len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unknown_images_are_skipped() {
        assert_eq!(image_mime(b"GIF89a"), None);
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\n...."), Some(MimeType::Png));
    }
}
