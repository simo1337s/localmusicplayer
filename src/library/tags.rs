//! Writes tags (and cover art) into downloaded files.

use std::path::Path;

use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::Tag;

/// What to write. Empty fields are left as they are.
#[derive(Debug, Default)]
pub struct Tags<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    /// Where the file came from (written as the comment).
    pub comment: &'a str,
    /// JPEG or PNG data for the front cover.
    pub cover: Option<&'a [u8]>,
    /// Only fill in what the file doesn't have yet (for files tagged by their uploader).
    pub keep_existing: bool,
}

pub fn write(path: &Path, tags: &Tags) -> Result<()> {
    let mut file = lofty::read_from_path(path).with_context(|| format!("couldn't read {}", path.display()))?;
    let kind = file.primary_tag_type();
    if file.tag(kind).is_none() {
        file.insert_tag(Tag::new(kind));
    }
    let tag = file.tag_mut(kind).context("no tag")?;
    let keep = tags.keep_existing;
    let value = |new: &str, old: Option<std::borrow::Cow<str>>| {
        let old_set = old.is_some_and(|o| !o.trim().is_empty());
        Some(new.trim())
            .filter(|v| !v.is_empty() && !(keep && old_set))
            .map(str::to_owned)
    };
    if let Some(v) = value(tags.title, tag.title()) {
        tag.set_title(v);
    }
    if let Some(v) = value(tags.artist, tag.artist()) {
        tag.set_artist(v);
    }
    if let Some(v) = value(tags.album, tag.album()) {
        tag.set_album(v);
    }
    if let Some(v) = value(tags.comment, tag.comment()) {
        tag.set_comment(v);
    }
    let has_cover = tag.pictures().iter().any(|p| p.pic_type() == PictureType::CoverFront);
    if let Some(data) = tags.cover.filter(|_| !(keep && has_cover)) {
        if let Some(mime) = image_mime(data) {
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
    fn writes_tags_and_cover() {
        let path = std::env::temp_dir().join(format!("multimusic-tags-{}.wav", std::process::id()));
        std::fs::write(&path, wav()).unwrap();
        let cover = [&[0xFF, 0xD8, 0xFF, 0xE0][..], &[0u8; 32]].concat();
        write(
            &path,
            &Tags {
                title: "Song",
                artist: "Someone",
                album: "",
                comment: "https://soundcloud.com/someone/song",
                cover: Some(&cover),
                keep_existing: false,
            },
        )
        .unwrap();
        // Filling in keeps what is there.
        write(
            &path,
            &Tags {
                title: "Other",
                album: "Album",
                keep_existing: true,
                ..Tags::default()
            },
        )
        .unwrap();

        let file = lofty::read_from_path(&path).unwrap();
        let tag = file.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Song"));
        assert_eq!(tag.artist().as_deref(), Some("Someone"));
        assert_eq!(tag.album().as_deref(), Some("Album"));
        assert_eq!(tag.pictures().len(), 1);
        assert_eq!(tag.pictures()[0].mime_type(), Some(&MimeType::Jpeg));
        // The scanner reads the same tags back.
        let track = super::super::scanner::read_track(&path, None, 0);
        assert_eq!((track.title.as_str(), track.artist.as_str()), ("Song", "Someone"));
        assert_eq!(track.duration_ms, 100);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unknown_images_are_skipped() {
        assert_eq!(image_mime(b"GIF89a"), None);
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\n...."), Some(MimeType::Png));
    }
}
