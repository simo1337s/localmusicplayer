//! Spotify library import through librespot's own session (the "spclient" endpoints the
//! official apps use). Unlike the public Web API this isn't rate limited per client id, so
//! syncing a big library works even when the shared Web API client id is throttled.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use http::{HeaderMap, HeaderValue, Method};
use librespot_core::session::Session;
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use librespot_protocol::extension_kind::ExtensionKind;
use librespot_protocol::metadata;
use librespot_protocol::playlist4_external::SelectedListContent;
use protobuf::{CodedInputStream, CodedOutputStream, EnumOrUnknown, Message};

use crate::model::{Source, Track};
use crate::providers::spotify_api::SpotifyPlaylistMeta;

/// Track metadata is fetched in batches of this many URIs.
const METADATA_BATCH: usize = 100;
const ROOTLIST_PAGE: usize = 120;

/// A playlist with its item URIs (`spotify:track:…`) and when each was added.
pub struct PlaylistContents {
    pub meta: SpotifyPlaylistMeta,
    pub items: Vec<(String, i64)>,
}

/// The user's playlists in library order (followed and own), as base62 ids.
pub async fn rootlist(session: &Session) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut from = 0;
    loop {
        let bytes = session
            .spclient()
            .get_rootlist(from, Some(ROOTLIST_PAGE))
            .await
            .map_err(|e| anyhow!("loading your playlists: {e}"))?;
        let list = SelectedListContent::parse_from_bytes(&bytes).context("parsing rootlist")?;
        let items = &list.contents.items;
        for item in items.iter() {
            if let Some(id) = playlist_id_from_uri(item.uri()) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        from += items.len();
        let total = list.length().max(0) as usize;
        if items.len() < ROOTLIST_PAGE || from >= total || items.is_empty() {
            break;
        }
    }
    Ok(ids)
}

/// `spotify:playlist:ID` or legacy `spotify:user:NAME:playlist:ID` -> `ID`.
pub fn playlist_id_from_uri(uri: &str) -> Option<String> {
    let parts: Vec<&str> = uri.split(':').collect();
    match parts.as_slice() {
        ["spotify", "playlist", id] => Some(id.to_string()),
        ["spotify", "user", _, "playlist", id] => Some(id.to_string()),
        _ => None,
    }
}

/// Loads one playlist with all its items.
pub async fn playlist(session: &Session, id: &str) -> Result<PlaylistContents> {
    let mut items: Vec<(String, i64)> = Vec::new();
    let mut endpoint = format!("/playlist/v2/playlist/{id}");
    let mut meta: Option<SpotifyPlaylistMeta> = None;
    for _ in 0..200 {
        let bytes = session
            .spclient()
            .request(&Method::GET, &endpoint, None, None)
            .await
            .map_err(|e| anyhow!("loading playlist {id}: {e}"))?;
        let list = SelectedListContent::parse_from_bytes(&bytes).context("parsing playlist")?;
        if meta.is_none() {
            meta = Some(playlist_meta(id, &list));
        }
        let before = items.len();
        for item in list.contents.items.iter() {
            let added = item.attributes.timestamp() / 1000;
            items.push((item.uri().to_string(), added));
        }
        let total = list.length().max(0) as usize;
        let truncated = list.contents.truncated();
        if items.len() >= total || items.len() == before || !truncated && list.contents.items.len() >= total {
            break;
        }
        endpoint = format!("/playlist/v2/playlist/{id}?from={}&length=500", items.len());
    }
    let mut meta = meta.ok_or_else(|| anyhow!("empty playlist response"))?;
    meta.total = items.len() as u32;
    Ok(PlaylistContents { meta, items })
}

fn playlist_meta(id: &str, list: &SelectedListContent) -> SpotifyPlaylistMeta {
    let attrs = &list.attributes;
    let art = attrs
        .picture_size
        .iter()
        .find(|p| p.target_name() == "default")
        .or_else(|| attrs.picture_size.first())
        .map(|p| p.url().to_string())
        .filter(|u| u.starts_with("http"))
        .or_else(|| (!attrs.picture().is_empty()).then(|| image_url(attrs.picture())));
    SpotifyPlaylistMeta {
        id: id.to_string(),
        name: attrs.name().to_string(),
        description: attrs.description().to_string(),
        art,
        total: list.length().max(0) as u32,
        snapshot_id: hex(list.revision()),
        owner: list
            .contents
            .meta_items
            .first()
            .map(|m| m.owner_username().to_string())
            .unwrap_or_default(),
    }
}

/// Liked Songs (the "collection" set), newest first.
pub async fn liked(session: &Session) -> Result<Vec<(String, i64)>> {
    let username = session.username();
    let mut headers = HeaderMap::new();
    let ct = HeaderValue::from_static("application/vnd.collection-v2.spotify.proto");
    headers.insert(http::header::CONTENT_TYPE, ct.clone());
    headers.insert(http::header::ACCEPT, ct);
    let mut token = String::new();
    let mut out = Vec::new();
    for _ in 0..500 {
        let body = encode_page_request(&username, "collection", &token, 300)?;
        let bytes = session
            .spclient()
            .request(
                &Method::POST,
                "/collection/v2/paging",
                Some(headers.clone()),
                Some(&body),
            )
            .await
            .map_err(|e| anyhow!("loading Liked Songs: {e}"))?;
        let page = decode_page_response(&bytes)?;
        out.extend(
            page.items
                .into_iter()
                .filter(|i| !i.is_removed && i.uri.starts_with("spotify:track:"))
                .map(|i| (i.uri, i.added_at)),
        );
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    out.sort_by_key(|item| std::cmp::Reverse(item.1));
    Ok(out)
}

/// Fetches metadata for `spotify:track:` URIs in batches.
pub async fn tracks(session: &Session, uris: &[String]) -> Result<HashMap<String, Track>> {
    let mut out = HashMap::with_capacity(uris.len());
    for chunk in uris.chunks(METADATA_BATCH) {
        let request = BatchedEntityRequest {
            entity_request: chunk
                .iter()
                .map(|uri| EntityRequest {
                    entity_uri: uri.clone(),
                    query: vec![ExtensionQuery {
                        extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let response = session
            .spclient()
            .get_extended_metadata(request)
            .await
            .map_err(|e| anyhow!("loading track details: {e}"))?;
        for array in response.extended_metadata.iter() {
            for data in array.extension_data.iter() {
                let Some(any) = data.extension_data.as_ref() else {
                    continue;
                };
                let Ok(track) = metadata::Track::parse_from_bytes(&any.value) else {
                    continue;
                };
                if let Some(t) = convert_track(&data.entity_uri, &track) {
                    out.insert(t.id.clone(), t);
                }
            }
        }
    }
    Ok(out)
}

/// The user's display name, falling back to the username.
pub async fn display_name(session: &Session) -> String {
    let username = session.username();
    match session.spclient().get_user_profile(&username, Some(0), Some(0)).await {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .filter(|n| !n.is_empty())
            .unwrap_or(username),
        Err(_) => username,
    }
}

fn convert_track(uri: &str, t: &metadata::Track) -> Option<Track> {
    if !uri.starts_with("spotify:track:") || t.name().is_empty() {
        return None;
    }
    let artist = t
        .artist
        .iter()
        .map(|a| a.name())
        .filter(|n| !n.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let album = t.album.as_ref();
    let images = album.map(|a| {
        let group: Vec<&metadata::Image> = a.cover_group.image.iter().collect();
        if group.is_empty() {
            a.cover.iter().collect()
        } else {
            group
        }
    });
    let art = images.and_then(|imgs| {
        // DEFAULT is ~300px: plenty for every place we draw covers.
        imgs.iter()
            .find(|i| i.size() == metadata::image::Size::DEFAULT)
            .or_else(|| imgs.iter().find(|i| i.size() == metadata::image::Size::LARGE))
            .or_else(|| imgs.first())
            .map(|i| image_url(i.file_id()))
    });
    Some(Track {
        id: uri.to_string(),
        source: Source::Spotify,
        title: t.name().to_string(),
        artist,
        album: album.map(|a| a.name().to_string()).unwrap_or_default(),
        duration_ms: t.duration().max(0) as u64,
        track_no: (t.number() > 0).then_some(t.number() as u32),
        art,
        uri: uri.to_string(),
        added_at: 0,
    })
}

fn image_url(file_id: &[u8]) -> String {
    format!("https://i.scdn.co/image/{}", hex(file_id))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// collection2v2.proto isn't compiled into librespot-protocol, and the two messages we need
// are tiny, so they're encoded by hand.

#[derive(Debug, Default, PartialEq)]
struct CollectionItem {
    uri: String,
    added_at: i64,
    is_removed: bool,
}

#[derive(Debug, Default, PartialEq)]
struct PageResponse {
    items: Vec<CollectionItem>,
    next_page_token: String,
}

fn encode_page_request(username: &str, set: &str, token: &str, limit: i32) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut os = CodedOutputStream::vec(&mut buf);
        os.write_string(1, username)?;
        os.write_string(2, set)?;
        if !token.is_empty() {
            os.write_string(3, token)?;
        }
        os.write_int32(4, limit)?;
        os.flush()?;
    }
    Ok(buf)
}

fn decode_page_response(bytes: &[u8]) -> Result<PageResponse> {
    let mut page = PageResponse::default();
    let mut is = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        let (field, wire) = (tag >> 3, tag & 7);
        match (field, wire) {
            (1, 2) => page.items.push(decode_item(&is.read_bytes()?)?),
            (2, 2) => page.next_page_token = is.read_string()?,
            _ => is.skip_field(protobuf::rt::WireType::new(wire).ok_or_else(|| anyhow!("bad wire type"))?)?,
        }
    }
    Ok(page)
}

fn decode_item(bytes: &[u8]) -> Result<CollectionItem> {
    let mut item = CollectionItem::default();
    let mut is = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        let (field, wire) = (tag >> 3, tag & 7);
        match (field, wire) {
            (1, 2) => item.uri = is.read_string()?,
            (2, 0) => item.added_at = is.read_int32()? as i64,
            (3, 0) => item.is_removed = is.read_bool()?,
            _ => is.skip_field(protobuf::rt::WireType::new(wire).ok_or_else(|| anyhow!("bad wire type"))?)?,
        }
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_uris() {
        assert_eq!(
            playlist_id_from_uri("spotify:playlist:37i9dQZF1DX"),
            Some("37i9dQZF1DX".into())
        );
        assert_eq!(
            playlist_id_from_uri("spotify:user:bob:playlist:abc"),
            Some("abc".into())
        );
        assert_eq!(playlist_id_from_uri("spotify:start-group:123:Folder"), None);
        assert_eq!(playlist_id_from_uri("spotify:end-group:123"), None);
    }

    #[test]
    fn collection_paging_roundtrip() {
        let req = encode_page_request("user", "collection", "tok", 300).unwrap();
        // field 1 "user", field 2 "collection", field 3 "tok", field 4 = 300
        assert_eq!(&req[..6], &[0x0a, 4, b'u', b's', b'e', b'r']);
        assert_eq!(&req[req.len() - 3..], &[0x20, 0xac, 0x02]);

        // Build a response: two items (one removed) and a next page token.
        let mut item1 = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut item1);
            os.write_string(1, "spotify:track:aaa").unwrap();
            os.write_int32(2, 1_700_000_000).unwrap();
            os.flush().unwrap();
        }
        let mut item2 = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut item2);
            os.write_string(1, "spotify:track:bbb").unwrap();
            os.write_bool(3, true).unwrap();
            os.write_string(4, "spotify:album:x").unwrap();
            os.flush().unwrap();
        }
        let mut resp = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut resp);
            os.write_bytes(1, &item1).unwrap();
            os.write_bytes(1, &item2).unwrap();
            os.write_string(2, "next").unwrap();
            os.write_string(3, "sync").unwrap();
            os.flush().unwrap();
        }
        let page = decode_page_response(&resp).unwrap();
        assert_eq!(page.next_page_token, "next");
        assert_eq!(
            page.items,
            vec![
                CollectionItem {
                    uri: "spotify:track:aaa".into(),
                    added_at: 1_700_000_000,
                    is_removed: false
                },
                CollectionItem {
                    uri: "spotify:track:bbb".into(),
                    added_at: 0,
                    is_removed: true
                },
            ]
        );
    }

    #[test]
    fn converts_track_metadata() {
        let mut t = metadata::Track::new();
        t.set_name("Get Lucky".into());
        t.set_duration(248_000);
        t.set_number(8);
        let mut a1 = metadata::Artist::new();
        a1.set_name("Daft Punk".into());
        let mut a2 = metadata::Artist::new();
        a2.set_name("Pharrell Williams".into());
        t.artist = vec![a1, a2];
        let mut album = metadata::Album::new();
        album.set_name("Random Access Memories".into());
        let mut small = metadata::Image::new();
        small.set_file_id(vec![0xab, 0xcd]);
        small.set_size(metadata::image::Size::SMALL);
        let mut def = metadata::Image::new();
        def.set_file_id(vec![0x01, 0x02]);
        def.set_size(metadata::image::Size::DEFAULT);
        album.cover_group.mut_or_insert_default().image = vec![small, def];
        t.album = protobuf::MessageField::some(album);

        let out = convert_track("spotify:track:abc", &t).unwrap();
        assert_eq!(out.id, "spotify:track:abc");
        assert_eq!(out.artist, "Daft Punk, Pharrell Williams");
        assert_eq!(out.album, "Random Access Memories");
        assert_eq!(out.duration_ms, 248_000);
        assert_eq!(out.track_no, Some(8));
        assert_eq!(out.art.as_deref(), Some("https://i.scdn.co/image/0102"));
        assert!(convert_track("spotify:episode:x", &t).is_none());
    }
}
