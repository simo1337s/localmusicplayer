//! Recognises music links pasted into search: Spotify, SoundCloud and Apple Music
//! artist / album / playlist / track URLs (and Spotify URIs).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Artist,
    Album,
    Playlist,
    Track,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Spotify {
        kind: LinkKind,
        id: String,
    },
    /// Any soundcloud.com page; resolved through SoundCloud's API.
    SoundCloud {
        url: String,
    },
    AppleMusic {
        kind: LinkKind,
        storefront: String,
        id: String,
    },
    /// Short links (spotify.link, on.soundcloud.com) that redirect to one of the above.
    Short {
        url: String,
    },
}

/// True if `input` should be treated as a link instead of a search query.
pub fn looks_like_link(input: &str) -> bool {
    parse(input).is_some()
}

pub fn parse(input: &str) -> Option<Link> {
    let s = input.trim();
    if let Some(rest) = s.strip_prefix("spotify:") {
        let mut parts = rest.split(':');
        let kind = spotify_kind(parts.next()?)?;
        let id = parts.next()?.to_string();
        return valid_spotify_id(&id).then_some(Link::Spotify { kind, id });
    }
    let without_scheme = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let (host, path) = without_scheme.split_once('/').unwrap_or((without_scheme, ""));
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    // Drop query string and fragment.
    let path = path.split(['?', '#']).next().unwrap_or("");
    let segments: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();

    match host {
        "open.spotify.com" | "play.spotify.com" => {
            // Optional locale prefix: /intl-de/artist/...
            let segs: Vec<&str> = segments
                .iter()
                .copied()
                .skip_while(|p| p.starts_with("intl-"))
                .collect();
            let kind = spotify_kind(segs.first()?)?;
            let id = segs.get(1)?.to_string();
            valid_spotify_id(&id).then_some(Link::Spotify { kind, id })
        }
        "spotify.link" | "on.soundcloud.com" | "spoti.fi" => (!segments.is_empty()).then(|| Link::Short {
            url: format!("https://{host}/{}", segments.join("/")),
        }),
        "soundcloud.com" | "m.soundcloud.com" => {
            let first = *segments.first()?;
            const NOT_PROFILES: &[&str] = &[
                "discover",
                "search",
                "you",
                "stream",
                "upload",
                "settings",
                "notifications",
                "messages",
                "charts",
                "pages",
                "terms-of-use",
                "jobs",
                "signin",
                "signup",
                "feed",
            ];
            if NOT_PROFILES.contains(&first) {
                return None;
            }
            Some(Link::SoundCloud {
                url: format!("https://soundcloud.com/{}", segments.join("/")),
            })
        }
        "music.apple.com" | "itunes.apple.com" | "geo.music.apple.com" => {
            // /{storefront}/{kind}/{slug}/{id} or /{storefront}/{kind}/{id}; ?i= points at a song in an album.
            let storefront = segments.first()?.to_string();
            let kind = match *segments.get(1)? {
                "artist" => LinkKind::Artist,
                "album" => LinkKind::Album,
                "playlist" => LinkKind::Playlist,
                "song" => LinkKind::Track,
                _ => return None,
            };
            let id = segments.last()?.to_string();
            if segments.len() < 3 {
                return None;
            }
            let song_in_album = query_param(without_scheme, "i");
            match (kind, song_in_album) {
                (LinkKind::Album, Some(song)) => Some(Link::AppleMusic {
                    kind: LinkKind::Track,
                    storefront,
                    id: song,
                }),
                _ => Some(Link::AppleMusic { kind, storefront, id }),
            }
        }
        _ => None,
    }
}

fn spotify_kind(s: &str) -> Option<LinkKind> {
    Some(match s {
        "artist" => LinkKind::Artist,
        "album" => LinkKind::Album,
        "playlist" => LinkKind::Playlist,
        "track" => LinkKind::Track,
        _ => return None,
    })
}

fn valid_spotify_id(id: &str) -> bool {
    id.len() == 22 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next()?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name && !v.is_empty()).then(|| v.to_string())
    })
}

/// The page key used by the service for a parsed link (SoundCloud and short links resolve later).
pub fn page_key(link: &Link) -> String {
    match link {
        Link::Spotify { kind, id } => format!("spotify:{}:{id}", kind_name(*kind)),
        Link::AppleMusic { kind, storefront, id } => format!("applemusic:{}:{storefront}:{id}", kind_name(*kind)),
        Link::SoundCloud { url } | Link::Short { url } => url.clone(),
    }
}

/// What a page key (or pasted link) refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Artists, songs and playlists from the library itself.
    LocalArtist(String),
    /// An artist by name: their songs on Spotify and SoundCloud together.
    ArtistName(String),
    Spotify(LinkKind, String),
    SoundCloudUser(u64),
    SoundCloudUrl(String),
    AppleMusic {
        kind: LinkKind,
        storefront: String,
        id: String,
    },
    Short(String),
}

/// Page keys: `local:artist:<name>`, `spotify:<kind>:<id>`, `soundcloud:user:<id>`,
/// `applemusic:<kind>:<storefront>:<id>`, or any link [`parse`] understands.
pub fn target(key: &str) -> Option<Target> {
    let key = key.trim();
    if let Some(name) = key.strip_prefix("local:artist:") {
        return (!name.is_empty()).then(|| Target::LocalArtist(name.to_string()));
    }
    if let Some(name) = key.strip_prefix("artist:") {
        return (!name.trim().is_empty()).then(|| Target::ArtistName(name.trim().to_string()));
    }
    if let Some(id) = key.strip_prefix("soundcloud:user:") {
        return id.parse().ok().map(Target::SoundCloudUser);
    }
    if let Some(rest) = key.strip_prefix("applemusic:") {
        let mut parts = rest.splitn(3, ':');
        let kind = match parts.next()? {
            "artist" => LinkKind::Artist,
            "album" => LinkKind::Album,
            "playlist" => LinkKind::Playlist,
            "track" => LinkKind::Track,
            _ => return None,
        };
        let storefront = parts.next().filter(|s| !s.is_empty())?.to_string();
        let id = parts.next().filter(|s| !s.is_empty())?.to_string();
        return Some(Target::AppleMusic { kind, storefront, id });
    }
    Some(match parse(key)? {
        Link::Spotify { kind, id } => Target::Spotify(kind, id),
        Link::SoundCloud { url } => Target::SoundCloudUrl(url),
        Link::AppleMusic { kind, storefront, id } => Target::AppleMusic { kind, storefront, id },
        Link::Short { url } => Target::Short(url),
    })
}

/// Finds the first music link in `text` (e.g. the page a short link redirects to).
pub fn find_link(text: &str) -> Option<Link> {
    const STARTS: &[&str] = &[
        "https://open.spotify.com/",
        "https://soundcloud.com/",
        "https://music.apple.com/",
    ];
    STARTS
        .iter()
        .flat_map(|start| text.match_indices(start).map(|(i, _)| i))
        .filter_map(|i| {
            let end = text[i..]
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '\\'))
                .map_or(text.len(), |n| i + n);
            match parse(&text[i..end].replace("&amp;", "&"))? {
                Link::Short { .. } => None,
                link => Some((i, link)),
            }
        })
        .min_by_key(|(i, _)| *i)
        .map(|(_, link)| link)
}

/// The service's own web page for a page key or link, to open in the browser.
pub fn web_url(key: &str) -> Option<String> {
    match target(key)? {
        Target::Spotify(kind, id) => Some(format!("https://open.spotify.com/{}/{id}", kind_name(kind))),
        Target::SoundCloudUrl(url) | Target::Short(url) => Some(url),
        Target::AppleMusic { kind, storefront, id } => {
            let kind = match kind {
                LinkKind::Track => "song",
                k => kind_name(k),
            };
            Some(format!("https://music.apple.com/{storefront}/{kind}/{id}"))
        }
        Target::LocalArtist(_) | Target::ArtistName(_) | Target::SoundCloudUser(_) => None,
    }
}

/// Human name of a kind, for page headers.
pub fn kind_label(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Artist => "Artist",
        LinkKind::Album => "Album",
        LinkKind::Playlist => "Playlist",
        LinkKind::Track => "Song",
    }
}

pub fn kind_name(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Artist => "artist",
        LinkKind::Album => "album",
        LinkKind::Playlist => "playlist",
        LinkKind::Track => "track",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spotify_links_and_uris() {
        let id = "0TnOYISbd1XYRBk9myaseg";
        for input in [
            format!("https://open.spotify.com/artist/{id}"),
            format!("https://open.spotify.com/artist/{id}?si=abc123"),
            format!("open.spotify.com/intl-de/artist/{id}"),
            format!("spotify:artist:{id}"),
        ] {
            assert_eq!(
                parse(&input),
                Some(Link::Spotify {
                    kind: LinkKind::Artist,
                    id: id.into()
                }),
                "{input}"
            );
        }
        assert_eq!(
            parse(&format!("https://open.spotify.com/playlist/{id}")),
            Some(Link::Spotify {
                kind: LinkKind::Playlist,
                id: id.into()
            })
        );
        assert_eq!(parse("https://open.spotify.com/artist/short"), None);
        assert_eq!(parse("https://open.spotify.com/user/someone"), None);
        assert_eq!(
            page_key(&parse(&format!("spotify:track:{id}")).unwrap()),
            format!("spotify:track:{id}")
        );
    }

    #[test]
    fn soundcloud_links() {
        assert_eq!(
            parse("https://soundcloud.com/forss?utm_source=x"),
            Some(Link::SoundCloud {
                url: "https://soundcloud.com/forss".into()
            })
        );
        assert_eq!(
            parse("m.soundcloud.com/forss/flickermood"),
            Some(Link::SoundCloud {
                url: "https://soundcloud.com/forss/flickermood".into()
            })
        );
        assert_eq!(
            parse("https://soundcloud.com/forss/sets/soulhack"),
            Some(Link::SoundCloud {
                url: "https://soundcloud.com/forss/sets/soulhack".into()
            })
        );
        assert_eq!(parse("https://soundcloud.com/discover"), None);
        assert_eq!(parse("https://soundcloud.com/"), None);
        assert_eq!(
            parse("https://on.soundcloud.com/AbCd1"),
            Some(Link::Short {
                url: "https://on.soundcloud.com/AbCd1".into()
            })
        );
    }

    #[test]
    fn apple_music_links() {
        assert_eq!(
            parse("https://music.apple.com/us/artist/taylor-swift/159260351"),
            Some(Link::AppleMusic {
                kind: LinkKind::Artist,
                storefront: "us".into(),
                id: "159260351".into()
            })
        );
        assert_eq!(
            parse("https://music.apple.com/gb/album/1989/1440935467?i=1440935808"),
            Some(Link::AppleMusic {
                kind: LinkKind::Track,
                storefront: "gb".into(),
                id: "1440935808".into()
            })
        );
        assert_eq!(
            parse("https://music.apple.com/us/playlist/todays-hits/pl.f4d106fed2bd41149aaacabb233eb5eb"),
            Some(Link::AppleMusic {
                kind: LinkKind::Playlist,
                storefront: "us".into(),
                id: "pl.f4d106fed2bd41149aaacabb233eb5eb".into()
            })
        );
        assert_eq!(parse("https://music.apple.com/us/browse"), None);
    }

    #[test]
    fn page_targets() {
        let id = "0TnOYISbd1XYRBk9myaseg";
        assert_eq!(
            target(&format!("spotify:artist:{id}")),
            Some(Target::Spotify(LinkKind::Artist, id.into()))
        );
        assert_eq!(
            target(&format!("https://open.spotify.com/album/{id}")),
            Some(Target::Spotify(LinkKind::Album, id.into()))
        );
        assert_eq!(target("soundcloud:user:1234"), Some(Target::SoundCloudUser(1234)));
        assert_eq!(target("soundcloud:user:abc"), None);
        assert_eq!(
            target("https://soundcloud.com/forss"),
            Some(Target::SoundCloudUrl("https://soundcloud.com/forss".into()))
        );
        assert_eq!(
            target("local:artist:daft punk"),
            Some(Target::LocalArtist("daft punk".into()))
        );
        assert_eq!(target("artist:Bladee"), Some(Target::ArtistName("Bladee".into())));
        assert_eq!(target("artist:  "), None);
        assert_eq!(target("local:artist:"), None);
        let apple = parse("https://music.apple.com/us/artist/taylor-swift/159260351").unwrap();
        assert_eq!(
            target(&page_key(&apple)),
            Some(Target::AppleMusic {
                kind: LinkKind::Artist,
                storefront: "us".into(),
                id: "159260351".into()
            })
        );
        assert_eq!(target("applemusic:1440935808"), None);
        assert_eq!(
            target("https://spotify.link/AbCdEf"),
            Some(Target::Short("https://spotify.link/AbCdEf".into()))
        );
        assert_eq!(target("daft punk"), None);
        assert_eq!(
            web_url(&format!("spotify:album:{id}")).as_deref(),
            Some(format!("https://open.spotify.com/album/{id}").as_str())
        );
        assert_eq!(
            web_url("applemusic:track:gb:1440935808").as_deref(),
            Some("https://music.apple.com/gb/song/1440935808")
        );
        assert_eq!(web_url("soundcloud:user:1"), None);
    }

    #[test]
    fn finds_links_in_redirect_pages() {
        let id = "0TnOYISbd1XYRBk9myaseg";
        let html = format!(
            "<html><a href=\"https://open.spotify.com/artist/{id}?si=x&amp;_branch=1\">open</a> \
             https://soundcloud.com/forss</html>"
        );
        assert_eq!(
            find_link(&html),
            Some(Link::Spotify {
                kind: LinkKind::Artist,
                id: id.into()
            })
        );
        assert_eq!(
            find_link("redirecting to https://soundcloud.com/forss/flickermood"),
            Some(Link::SoundCloud {
                url: "https://soundcloud.com/forss/flickermood".into()
            })
        );
        assert_eq!(find_link("<html>nothing here</html>"), None);
    }

    #[test]
    fn plain_queries_are_not_links() {
        for q in [
            "daft punk",
            "soundcloud",
            "spotify",
            "https://example.com/artist/x",
            "the weeknd blinding lights",
        ] {
            assert!(!looks_like_link(q), "{q}");
        }
    }
}
