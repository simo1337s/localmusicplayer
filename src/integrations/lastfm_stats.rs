//! Your Last.fm profile: who you are, what you played most over a period, how much you
//! listened when, and what you played last. Everything comes from Last.fm's read-only calls
//! (`user.get*`), which need the API key but no login.
//!
//! Last.fm has top lists for the last 7 days up to all time; "Today" is worked out here from
//! the day's scrobbles. Last.fm shows a grey star instead of artist pictures, so artists get
//! the cover of their best-known album instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::lastfm::Lastfm;
use crate::clock;
use crate::model::now_unix;

/// Last.fm's picture for "no picture".
const PLACEHOLDER: &str = "2a96cbd8b46e442fc41c2b86b821562f";
/// Entries in a top list.
const TOP_LIMIT: usize = 50;
/// Scrobbles in the recent list.
const RECENT_LIMIT: usize = 50;
/// Pages of 200 scrobbles read for "Today" at most.
const TODAY_PAGES: u32 = 10;
/// Artists whose tags make up the genre chart.
const TAG_ARTISTS: usize = 15;
/// After a failure, wait this long before asking again by itself.
pub const RETRY_AFTER: Duration = Duration::from_secs(60);

/// A stretch of time stats are shown for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    /// Since midnight.
    Today,
    #[default]
    Week,
    Month,
    Quarter,
    HalfYear,
    Year,
    Overall,
}

impl Period {
    pub const ALL: [Period; 7] = [
        Period::Today,
        Period::Week,
        Period::Month,
        Period::Quarter,
        Period::HalfYear,
        Period::Year,
        Period::Overall,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Period::Today => "Today",
            Period::Week => "Last 7 days",
            Period::Month => "Last 30 days",
            Period::Quarter => "Last 3 months",
            Period::HalfYear => "Last 6 months",
            Period::Year => "Last 12 months",
            Period::Overall => "All time",
        }
    }

    /// The same, inside a sentence ("12 scrobbles today").
    pub fn phrase(self) -> &'static str {
        match self {
            Period::Today => "today",
            Period::Week => "in the last 7 days",
            Period::Month => "in the last 30 days",
            Period::Quarter => "in the last 3 months",
            Period::HalfYear => "in the last 6 months",
            Period::Year => "in the last 12 months",
            Period::Overall => "of all time",
        }
    }

    /// Last.fm's name for it in `user.getTop*` (Today has none).
    fn api(self) -> Option<&'static str> {
        match self {
            Period::Today => None,
            Period::Week => Some("7day"),
            Period::Month => Some("1month"),
            Period::Quarter => Some("3month"),
            Period::HalfYear => Some("6month"),
            Period::Year => Some("12month"),
            Period::Overall => Some("overall"),
        }
    }

    fn days(self) -> Option<i64> {
        match self {
            Period::Week => Some(7),
            Period::Month => Some(30),
            Period::Quarter => Some(90),
            Period::HalfYear => Some(180),
            Period::Year => Some(365),
            Period::Today | Period::Overall => None,
        }
    }
}

/// How the listening chart is split up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityRange {
    /// The last 7 days, a bar per day.
    #[default]
    Week,
    /// The last 30 days, a bar per day.
    Month,
    /// The last 12 months, a bar per month.
    Year,
    /// Every year since joining, a bar per year.
    Years,
}

impl ActivityRange {
    pub const ALL: [ActivityRange; 4] = [
        ActivityRange::Week,
        ActivityRange::Month,
        ActivityRange::Year,
        ActivityRange::Years,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ActivityRange::Week => "Last 7 days",
            ActivityRange::Month => "Last 30 days",
            ActivityRange::Year => "Last 12 months",
            ActivityRange::Years => "Every year",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TopKind {
    Artists,
    Albums,
    Tracks,
}

/// Something the profile page shows, loaded on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatsRequest {
    /// Name, picture, totals.
    User,
    /// The last scrobbles (and what's playing now).
    Recent,
    Top(TopKind, Period),
    Summary(Period),
    Activity(ActivityRange),
    Tags(Period),
}

impl StatsRequest {
    /// How long an answer is shown before it is asked for again.
    pub fn fresh_for(self) -> Duration {
        let minutes = |m: u64| Duration::from_secs(m * 60);
        match self {
            StatsRequest::Recent => Duration::from_secs(45),
            StatsRequest::Top(_, Period::Today) | StatsRequest::Summary(Period::Today) => minutes(2),
            StatsRequest::Tags(Period::Today) => minutes(5),
            StatsRequest::User | StatsRequest::Summary(_) | StatsRequest::Activity(_) => minutes(5),
            StatsRequest::Top(..) | StatsRequest::Tags(_) => minutes(15),
        }
    }
}

/// The account (`user.getInfo`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProfileUser {
    pub name: String,
    pub real_name: String,
    pub url: String,
    pub image: Option<String>,
    pub country: String,
    pub scrobbles: u64,
    pub artists: u64,
    pub albums: u64,
    pub tracks: u64,
    /// Loved tracks (`None` when Last.fm didn't say).
    pub loved: Option<u64>,
    /// When the account was made (unix time).
    pub registered: i64,
    /// Last.fm Pro.
    pub subscriber: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scrobble {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub image: Option<String>,
    pub url: String,
    /// When it was scrobbled; `None` while it's playing.
    pub at: Option<i64>,
    pub loved: bool,
}

/// An artist, album or song in a top list.
#[derive(Debug, Clone, PartialEq)]
pub struct TopItem {
    pub name: String,
    /// The artist of an album or song (empty for artists).
    pub artist: String,
    pub plays: u64,
    pub image: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TopList {
    pub items: Vec<TopItem>,
    /// Different artists / albums / songs played in the period.
    pub total: u64,
}

/// Totals for a period.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub scrobbles: u64,
    pub artists: u64,
    pub albums: u64,
    pub tracks: u64,
    /// Days the period covers, for the daily average (0 for today).
    pub days: f64,
}

/// A bar of the listening chart.
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    /// Under the bar: "Mon", "12", "Oct", "2024".
    pub label: String,
    /// On hover: "Monday 12 Oct", "October 2026".
    pub detail: String,
    pub plays: u64,
}

/// A genre and how much of the listening it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct TagShare {
    pub name: String,
    /// 0..=1.
    pub share: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatsData {
    User(ProfileUser),
    Recent(Vec<Scrobble>),
    Top(TopList),
    Summary(Summary),
    Activity(Vec<Bucket>),
    Tags(Vec<TagShare>),
}

/// One request's answer as the UI sees it. Old data stays shown while it reloads.
#[derive(Debug, Clone, Default)]
pub struct Fetch {
    pub data: Option<StatsData>,
    pub loading: bool,
    pub error: Option<String>,
    /// When the data (or error) arrived; `None` = ask again.
    pub at: Option<Instant>,
}

impl Fetch {
    /// Whether to ask for it (again) now.
    pub fn wanted(&self, req: StatsRequest) -> bool {
        if self.loading {
            return false;
        }
        match self.at {
            None => true,
            Some(at) if self.error.is_some() => at.elapsed() > RETRY_AFTER,
            Some(at) => at.elapsed() > req.fresh_for(),
        }
    }
}

/// Pictures looked up for artists and songs Last.fm lists without one.
#[derive(Debug, Clone, PartialEq)]
pub enum ArtLookup {
    Pending,
    Found(String),
    Missing,
}

/// Everything the UI shows of the Last.fm profile (`Feed::profile`).
#[derive(Debug, Clone, Default)]
pub struct ProfileFeed {
    /// Whose stats these are.
    pub user: String,
    pub stats: HashMap<StatsRequest, Fetch>,
    /// By [`art_key`].
    pub art: HashMap<String, ArtLookup>,
}

impl ProfileFeed {
    pub fn get(&self, req: StatsRequest) -> Option<&Fetch> {
        self.stats.get(&req)
    }

    /// The picture found for an artist (`title` empty) or song, if any.
    pub fn art(&self, artist: &str, title: &str) -> Option<&ArtLookup> {
        self.art.get(&art_key(artist, title))
    }
}

/// Key of a picture lookup: an artist's (`title` empty) or a song's.
pub fn art_key(artist: &str, title: &str) -> String {
    format!("{}\u{1f}{}", artist.trim().to_lowercase(), title.trim().to_lowercase())
}

// ------------------------------------------------------------------ reading answers

fn num(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        // `{"#text": "..."}` (names in some answers) or `{"name": "..."}` (extended ones).
        Some(Value::Object(o)) => o
            .get("name")
            .or_else(|| o.get("#text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// The best real picture in an `image` list (Last.fm lists sizes small to large, with a grey
/// star or nothing when it has no picture).
pub fn best_image(v: Option<&Value>) -> Option<String> {
    let list = v?.as_array()?;
    let url = |i: &Value| {
        i.get("#text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| u.starts_with("http") && !u.contains(PLACEHOLDER))
            .map(str::to_string)
    };
    let sized = |size: &str| {
        list.iter()
            .filter(|i| i.get("size").and_then(Value::as_str) == Some(size))
            .find_map(url)
    };
    sized("extralarge")
        .or_else(|| sized("large"))
        .or_else(|| list.iter().rev().find_map(url))
}

/// Last.fm answers a one-item list with the item on its own.
fn items(v: Option<&Value>) -> Vec<&Value> {
    match v {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(o @ Value::Object(_)) => vec![o],
        _ => Vec::new(),
    }
}

pub fn parse_user(v: &Value) -> anyhow::Result<ProfileUser> {
    let u = v.get("user").context("Last.fm sent no profile")?;
    Ok(ProfileUser {
        name: text(u.get("name")),
        real_name: text(u.get("realname")),
        url: text(u.get("url")),
        image: best_image(u.get("image")),
        country: text(u.get("country")).replace("None", ""),
        scrobbles: num(u.get("playcount")),
        artists: num(u.get("artist_count")),
        albums: num(u.get("album_count")),
        tracks: num(u.get("track_count")),
        loved: None,
        registered: num(u.pointer("/registered/unixtime")).max(num(u.pointer("/registered/#text"))) as i64,
        subscriber: num(u.get("subscriber")) == 1,
    })
}

/// `user.getRecentTracks`: the scrobbles, how many there are in all, and the number of pages.
pub fn parse_recent(v: &Value) -> (Vec<Scrobble>, u64, u64) {
    let r = v.get("recenttracks");
    let attr = r.and_then(|r| r.get("@attr"));
    let list = items(r.and_then(|r| r.get("track")))
        .into_iter()
        .map(|t| Scrobble {
            artist: text(t.get("artist")),
            title: text(t.get("name")),
            album: text(t.get("album")),
            image: best_image(t.get("image")),
            url: text(t.get("url")),
            at: t
                .pointer("/@attr/nowplaying")
                .is_none_or(|p| p.as_str() != Some("true"))
                .then(|| num(t.pointer("/date/uts")) as i64),
            loved: num(t.get("loved")) == 1,
        })
        .filter(|s| !s.artist.is_empty() && !s.title.is_empty())
        .collect();
    (
        list,
        num(attr.and_then(|a| a.get("total"))),
        num(attr.and_then(|a| a.get("totalPages"))),
    )
}

/// `user.getTopArtists` / `getTopAlbums` / `getTopTracks`.
pub fn parse_top(v: &Value, kind: TopKind) -> TopList {
    let (list, item) = match kind {
        TopKind::Artists => ("topartists", "artist"),
        TopKind::Albums => ("topalbums", "album"),
        TopKind::Tracks => ("toptracks", "track"),
    };
    let l = v.get(list);
    let items = items(l.and_then(|l| l.get(item)))
        .into_iter()
        .map(|i| TopItem {
            name: text(i.get("name")),
            artist: if kind == TopKind::Artists {
                String::new()
            } else {
                text(i.get("artist"))
            },
            plays: num(i.get("playcount")),
            image: best_image(i.get("image")),
            url: text(i.get("url")),
        })
        .filter(|i| !i.name.is_empty())
        .collect();
    TopList {
        items,
        total: num(l.and_then(|l| l.pointer("/@attr/total"))),
    }
}

/// `artist.getTopTags`: (tag, weight 0-100), most used first.
pub fn parse_tags(v: &Value) -> Vec<(String, u64)> {
    items(v.pointer("/toptags/tag"))
        .into_iter()
        .map(|t| (text(t.get("name")).to_lowercase(), num(t.get("count"))))
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

/// Tags that say nothing about the music.
fn junk_tag(tag: &str, artist: &str) -> bool {
    tag.contains("favo")
        || tag.contains("seen live")
        || tag.contains("vocalist")
        || tag.contains("albums i own")
        || tag.contains("listeners")
        || tag.len() < 2
        || tag == artist.to_lowercase()
}

/// An artist, how often they were played, and their tags with weights.
pub type ArtistTags = (String, u64, Vec<(String, u64)>);

/// Genres for artists played `plays` times each: the part of the listening each tag covers.
pub fn tag_shares(artists: &[ArtistTags]) -> Vec<TagShare> {
    let total: u64 = artists.iter().map(|(_, plays, _)| plays).sum();
    if total == 0 {
        return Vec::new();
    }
    let mut weights: HashMap<&str, f64> = HashMap::new();
    for (artist, plays, tags) in artists {
        // An artist's own top tags, weighted by how strongly Last.fm users tag them so.
        for (tag, weight) in tags.iter().filter(|(t, w)| *w >= 10 && !junk_tag(t, artist)).take(6) {
            *weights.entry(tag).or_default() += *plays as f64 * (*weight).min(100) as f64 / 100.0;
        }
    }
    let mut shares: Vec<TagShare> = weights
        .into_iter()
        .map(|(name, w)| TagShare {
            name: name.to_string(),
            share: (w / total as f64).min(1.0) as f32,
        })
        .collect();
    shares.sort_by(|a, b| b.share.total_cmp(&a.share).then_with(|| a.name.cmp(&b.name)));
    shares.truncate(10);
    shares
}

/// Top artists, albums or songs worked out from scrobbles (for "Today").
pub fn aggregate(scrobbles: &[Scrobble], kind: TopKind) -> TopList {
    let mut found: HashMap<String, TopItem> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for s in scrobbles.iter().filter(|s| s.at.is_some()) {
        let (name, artist) = match kind {
            TopKind::Artists => (&s.artist, ""),
            TopKind::Albums if s.album.is_empty() => continue,
            TopKind::Albums => (&s.album, s.artist.as_str()),
            TopKind::Tracks => (&s.title, s.artist.as_str()),
        };
        let key = art_key(artist, name);
        let item = found.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            TopItem {
                name: name.clone(),
                artist: artist.to_string(),
                plays: 0,
                // Scrobbles come with the album cover, which fits albums and songs.
                image: if kind == TopKind::Artists {
                    None
                } else {
                    s.image.clone()
                },
                url: if kind == TopKind::Tracks {
                    s.url.clone()
                } else {
                    String::new()
                },
            }
        });
        item.plays += 1;
        if item.image.is_none() && kind != TopKind::Artists {
            item.image = s.image.clone();
        }
    }
    let total = found.len() as u64;
    // Most played first; on a tie, whichever was played most recently.
    let mut items: Vec<TopItem> = order.into_iter().filter_map(|k| found.remove(&k)).collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.plays));
    items.truncate(TOP_LIMIT);
    TopList { items, total }
}

/// Totals worked out from scrobbles (for "Today").
pub fn summarize(scrobbles: &[Scrobble]) -> Summary {
    let done: Vec<Scrobble> = scrobbles.iter().filter(|s| s.at.is_some()).cloned().collect();
    Summary {
        scrobbles: done.len() as u64,
        artists: aggregate(&done, TopKind::Artists).total,
        albums: aggregate(&done, TopKind::Albums).total,
        tracks: aggregate(&done, TopKind::Tracks).total,
        days: 0.0,
    }
}

/// The bars of a listening chart as (label, detail, from, to), oldest first.
pub fn buckets(range: ActivityRange, now: i64, offset: i64, registered: i64) -> Vec<(String, String, i64, i64)> {
    let today = clock::local_day(now, offset);
    match range {
        ActivityRange::Week | ActivityRange::Month => {
            let n = if range == ActivityRange::Week { 7 } else { 30 };
            (0..n)
                .rev()
                .map(|back| {
                    let day = today - back;
                    let (_, m, d) = clock::civil_from_days(day);
                    let name = clock::WEEKDAYS[clock::weekday(day)];
                    let label = if range == ActivityRange::Week {
                        name.to_string()
                    } else {
                        d.to_string()
                    };
                    let detail = match back {
                        0 => "Today".to_string(),
                        1 => "Yesterday".to_string(),
                        _ => format!("{name} {d} {}", clock::MONTHS[m as usize - 1]),
                    };
                    (
                        label,
                        detail,
                        clock::day_start(day, offset),
                        clock::day_start(day + 1, offset),
                    )
                })
                .collect()
        }
        ActivityRange::Year => {
            let (y, m, _) = clock::civil_from_days(today);
            (0..12)
                .rev()
                .map(|back| {
                    let month = m as i64 - back;
                    let start = clock::month_start(y, month, offset);
                    let (yy, mm, _) = clock::civil_from_days(clock::local_day(start, offset));
                    (
                        clock::MONTHS[mm as usize - 1].to_string(),
                        format!("{} {yy}", clock::MONTHS[mm as usize - 1]),
                        start,
                        clock::month_start(y, month + 1, offset),
                    )
                })
                .collect()
        }
        ActivityRange::Years => {
            let (this_year, _, _) = clock::civil_from_days(today);
            let first = if registered > 0 {
                clock::civil_from_days(clock::local_day(registered, offset)).0
            } else {
                this_year
            };
            // At most the last 25 years.
            (first.max(this_year - 24)..=this_year)
                .map(|y| {
                    (
                        y.to_string(),
                        y.to_string(),
                        clock::month_start(y, 1, offset),
                        clock::month_start(y + 1, 1, offset),
                    )
                })
                .collect()
        }
    }
}

// ------------------------------------------------------------------ loading

/// Scrobbles since midnight: when they were read, the midnight, and the scrobbles.
type DayScrobbles = (Instant, i64, Arc<Vec<Scrobble>>);

/// Loads profile stats for one user, politely: a few calls at a time, with answers that are
/// needed by several requests (the day's scrobbles, top lists, artist tags) kept for a while.
pub struct ProfileStats {
    lastfm: Arc<Lastfm>,
    pub user: String,
    permits: tokio::sync::Semaphore,
    info: tokio::sync::Mutex<Option<(Instant, ProfileUser)>>,
    /// The day's scrobbles: when read, and the midnight they start at.
    today: tokio::sync::Mutex<Option<DayScrobbles>>,
    tops: Mutex<HashMap<(TopKind, Period), (Instant, TopList)>>,
    tags: Mutex<HashMap<String, Vec<(String, u64)>>>,
    art: Mutex<HashMap<String, Option<String>>>,
}

impl ProfileStats {
    pub fn new(lastfm: Arc<Lastfm>, user: &str) -> ProfileStats {
        ProfileStats {
            lastfm,
            user: user.trim().to_string(),
            permits: tokio::sync::Semaphore::new(3),
            info: tokio::sync::Mutex::new(None),
            today: tokio::sync::Mutex::new(None),
            tops: Mutex::new(HashMap::new()),
            tags: Mutex::new(HashMap::new()),
            art: Mutex::new(HashMap::new()),
        }
    }

    /// Whether this is for that account and Last.fm setup.
    pub fn serves(&self, lastfm: &Arc<Lastfm>, user: &str) -> bool {
        Arc::ptr_eq(&self.lastfm, lastfm) && self.user == user.trim()
    }

    async fn call(&self, method: &str, params: &[(&str, String)]) -> anyhow::Result<Value> {
        let _permit = self.permits.acquire().await?;
        let mut all = vec![("user".to_string(), self.user.clone())];
        all.extend(params.iter().map(|(k, v)| (k.to_string(), v.clone())));
        tokio::time::timeout(Duration::from_secs(20), self.lastfm.public(method, all))
            .await
            .context("Last.fm took too long to answer")?
    }

    pub async fn load(&self, req: StatsRequest) -> anyhow::Result<StatsData> {
        Ok(match req {
            StatsRequest::User => StatsData::User(self.user_info(true).await?),
            StatsRequest::Recent => StatsData::Recent(self.recent().await?),
            StatsRequest::Top(kind, period) => StatsData::Top(self.top(kind, period, true).await?),
            StatsRequest::Summary(period) => StatsData::Summary(self.summary(period).await?),
            StatsRequest::Activity(range) => StatsData::Activity(self.activity(range).await?),
            StatsRequest::Tags(period) => StatsData::Tags(self.genres(period).await?),
        })
    }

    async fn user_info(&self, fresh: bool) -> anyhow::Result<ProfileUser> {
        let mut info = self.info.lock().await;
        if let Some((at, user)) = info.as_ref() {
            if !fresh || at.elapsed() < Duration::from_secs(30) {
                return Ok(user.clone());
            }
        }
        let loved_params = [("limit", "1".to_string())];
        let (answer, loved) = tokio::join!(
            self.call("user.getInfo", &[]),
            self.call("user.getLovedTracks", &loved_params)
        );
        let mut user = parse_user(&answer?)?;
        user.loved = loved
            .ok()
            .and_then(|v| v.pointer("/lovedtracks/@attr/total").map(|t| num(Some(t))));
        *info = Some((Instant::now(), user.clone()));
        Ok(user)
    }

    async fn recent(&self) -> anyhow::Result<Vec<Scrobble>> {
        let v = self
            .call(
                "user.getRecentTracks",
                &[("limit", RECENT_LIMIT.to_string()), ("extended", "1".into())],
            )
            .await?;
        let (mut list, _, _) = parse_recent(&v);
        // The song playing now comes on top of a full page.
        list.truncate(RECENT_LIMIT + 1);
        Ok(list)
    }

    /// Everything scrobbled since midnight, read once for all of Today's numbers.
    async fn today(&self) -> anyhow::Result<Arc<Vec<Scrobble>>> {
        let offset = clock::utc_offset();
        let now = now_unix();
        let midnight = clock::day_start(clock::local_day(now, offset), offset);
        let mut cached = self.today.lock().await;
        if let Some((at, since, list)) = cached.as_ref() {
            if *since == midnight && at.elapsed() < Duration::from_secs(60) {
                return Ok(list.clone());
            }
        }
        let mut all = Vec::new();
        let mut page = 1;
        loop {
            let v = self
                .call(
                    "user.getRecentTracks",
                    &[
                        ("limit", "200".into()),
                        ("from", midnight.to_string()),
                        ("page", page.to_string()),
                    ],
                )
                .await?;
            let (list, _, pages) = parse_recent(&v);
            all.extend(list.into_iter().filter(|s| s.at.is_some()));
            if page as u64 >= pages || page >= TODAY_PAGES {
                break;
            }
            page += 1;
        }
        let all = Arc::new(all);
        *cached = Some((Instant::now(), midnight, all.clone()));
        Ok(all)
    }

    async fn top(&self, kind: TopKind, period: Period, fresh: bool) -> anyhow::Result<TopList> {
        let Some(api) = period.api() else {
            return Ok(aggregate(&self.today().await?, kind));
        };
        let keep = if fresh {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(600)
        };
        if let Some((at, list)) = self.tops.lock().unwrap().get(&(kind, period)) {
            if at.elapsed() < keep {
                return Ok(list.clone());
            }
        }
        let method = match kind {
            TopKind::Artists => "user.getTopArtists",
            TopKind::Albums => "user.getTopAlbums",
            TopKind::Tracks => "user.getTopTracks",
        };
        let v = self
            .call(method, &[("period", api.into()), ("limit", TOP_LIMIT.to_string())])
            .await?;
        let list = parse_top(&v, kind);
        self.tops
            .lock()
            .unwrap()
            .insert((kind, period), (Instant::now(), list.clone()));
        Ok(list)
    }

    /// How many different artists / albums / songs were played in a period.
    async fn distinct(&self, kind: TopKind, period: Period) -> anyhow::Result<u64> {
        if let Some((at, list)) = self.tops.lock().unwrap().get(&(kind, period)) {
            if at.elapsed() < Duration::from_secs(600) {
                return Ok(list.total);
            }
        }
        let method = match kind {
            TopKind::Artists => "user.getTopArtists",
            TopKind::Albums => "user.getTopAlbums",
            TopKind::Tracks => "user.getTopTracks",
        };
        let api = period.api().unwrap_or("overall");
        let v = self
            .call(method, &[("period", api.into()), ("limit", "1".into())])
            .await?;
        Ok(parse_top(&v, kind).total)
    }

    /// Scrobbles from `from` to `to` (unix times; `to` = now when `None`).
    async fn count(&self, from: i64, to: Option<i64>) -> anyhow::Result<u64> {
        let mut params = vec![("limit", "1".to_string()), ("from", from.to_string())];
        if let Some(to) = to {
            params.push(("to", (to - 1).to_string()));
        }
        let v = self.call("user.getRecentTracks", &params).await?;
        Ok(parse_recent(&v).1)
    }

    async fn summary(&self, period: Period) -> anyhow::Result<Summary> {
        match (period, period.days()) {
            (Period::Today, _) => Ok(summarize(&self.today().await?)),
            (_, Some(days)) => {
                let since = now_unix() - days * 86_400;
                let (scrobbles, artists, albums, tracks) = tokio::join!(
                    self.count(since, None),
                    self.distinct(TopKind::Artists, period),
                    self.distinct(TopKind::Albums, period),
                    self.distinct(TopKind::Tracks, period),
                );
                Ok(Summary {
                    scrobbles: scrobbles?,
                    artists: artists?,
                    albums: albums?,
                    tracks: tracks?,
                    days: days as f64,
                })
            }
            _ => {
                let u = self.user_info(false).await?;
                let days = if u.registered > 0 {
                    ((now_unix() - u.registered) as f64 / 86_400.0).max(1.0)
                } else {
                    0.0
                };
                Ok(Summary {
                    scrobbles: u.scrobbles,
                    artists: u.artists,
                    albums: u.albums,
                    tracks: u.tracks,
                    days,
                })
            }
        }
    }

    async fn activity(&self, range: ActivityRange) -> anyhow::Result<Vec<Bucket>> {
        let registered = if range == ActivityRange::Years {
            self.user_info(false).await?.registered
        } else {
            0
        };
        let now = now_unix();
        let bars = buckets(range, now, clock::utc_offset(), registered);
        let counts = futures_util::future::join_all(
            bars.iter()
                .map(|(_, _, from, to)| self.count(*from, (*to <= now).then_some(*to))),
        )
        .await;
        bars.into_iter()
            .zip(counts)
            .map(|((label, detail, _, _), plays)| {
                Ok(Bucket {
                    label,
                    detail,
                    plays: plays?,
                })
            })
            .collect()
    }

    async fn artist_tags(&self, artist: &str) -> Vec<(String, u64)> {
        let key = artist.to_lowercase();
        if let Some(tags) = self.tags.lock().unwrap().get(&key) {
            return tags.clone();
        }
        let answer = self
            .call(
                "artist.getTopTags",
                &[("artist", artist.to_string()), ("autocorrect", "1".into())],
            )
            .await;
        let tags = match answer {
            Ok(v) => parse_tags(&v),
            // Try again another time.
            Err(_) => return Vec::new(),
        };
        self.tags.lock().unwrap().insert(key, tags.clone());
        tags
    }

    async fn genres(&self, period: Period) -> anyhow::Result<Vec<TagShare>> {
        let top = self.top(TopKind::Artists, period, false).await?;
        let artists: Vec<&TopItem> = top.items.iter().take(TAG_ARTISTS).collect();
        let tags = futures_util::future::join_all(artists.iter().map(|a| self.artist_tags(&a.name))).await;
        let weighted: Vec<ArtistTags> = artists
            .iter()
            .zip(tags)
            .map(|(a, tags)| (a.name.clone(), a.plays, tags))
            .collect();
        Ok(tag_shares(&weighted))
    }

    /// A picture for an artist (`title` empty: the cover of their best-known album) or a song
    /// (its album's cover).
    pub async fn picture(&self, artist: &str, title: &str) -> Option<String> {
        let key = art_key(artist, title);
        if let Some(found) = self.art.lock().unwrap().get(&key) {
            return found.clone();
        }
        let answer = if title.is_empty() {
            self.call(
                "artist.getTopAlbums",
                &[
                    ("artist", artist.to_string()),
                    ("limit", "3".into()),
                    ("autocorrect", "1".into()),
                ],
            )
            .await
            .map(|v| {
                items(v.pointer("/topalbums/album"))
                    .into_iter()
                    .find_map(|a| best_image(a.get("image")))
            })
        } else {
            self.call(
                "track.getInfo",
                &[
                    ("artist", artist.to_string()),
                    ("track", title.to_string()),
                    ("autocorrect", "1".into()),
                ],
            )
            .await
            .map(|v| best_image(v.pointer("/track/album/image")))
        };
        let found = match answer {
            Ok(found) => found,
            // Nothing known about it is an answer too; trouble reaching Last.fm isn't.
            Err(e) if e.downcast_ref::<super::lastfm::LastfmError>().is_some() => None,
            Err(_) => return None,
        };
        self.art.lock().unwrap().insert(key, found.clone());
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn image(url: &str) -> Value {
        json!([
            {"size": "small", "#text": url.replace("300x300", "34s")},
            {"size": "large", "#text": url.replace("300x300", "174s")},
            {"size": "extralarge", "#text": url}
        ])
    }

    #[test]
    fn reads_a_profile() {
        let v = json!({"user": {
            "name": "someone", "realname": "Some One", "url": "https://www.last.fm/user/someone",
            "image": image("https://lastfm.freetls.fastly.net/i/u/300x300/abc.png"),
            "country": "Finland", "playcount": "150316", "artist_count": "12749",
            "album_count": "26658", "track_count": "57066",
            "registered": {"unixtime": "1037793040", "#text": 1037793040}, "subscriber": "0"
        }});
        let u = parse_user(&v).unwrap();
        assert_eq!(u.name, "someone");
        assert_eq!(u.real_name, "Some One");
        assert_eq!(
            u.image.as_deref(),
            Some("https://lastfm.freetls.fastly.net/i/u/300x300/abc.png")
        );
        assert_eq!(
            (u.scrobbles, u.artists, u.albums, u.tracks),
            (150_316, 12_749, 26_658, 57_066)
        );
        assert_eq!(u.registered, 1_037_793_040);
        assert!(!u.subscriber);
        // No picture: no image.
        let v = json!({"user": {"name": "x", "image": [{"size": "small", "#text": ""}], "country": "None"}});
        let u = parse_user(&v).unwrap();
        assert_eq!(u.image, None);
        assert_eq!(u.country, "");
        assert!(parse_user(&json!({})).is_err());
    }

    #[test]
    fn reads_recent_scrobbles() {
        let v = json!({"recenttracks": {
            "track": [
                {"artist": {"name": "Bladee"}, "name": "Waster", "album": {"#text": "Icedancer"},
                 "image": image("https://x/300x300/a.jpg"), "url": "https://www.last.fm/music/Bladee/_/Waster",
                 "@attr": {"nowplaying": "true"}, "loved": "1"},
                {"artist": {"#text": "Yung Lean"}, "name": "Ginseng Strip 2002", "album": {"#text": ""},
                 "image": image(&format!("https://x/300x300/{PLACEHOLDER}.png")),
                 "date": {"uts": "1700000000", "#text": "14 Nov 2023, 22:13"}, "loved": "0"}
            ],
            "@attr": {"user": "someone", "totalPages": "3006", "page": "1", "perPage": "50", "total": "150316"}
        }});
        let (list, total, pages) = parse_recent(&v);
        assert_eq!((total, pages), (150_316, 3006));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].at, None);
        assert!(list[0].loved);
        assert_eq!(list[0].album, "Icedancer");
        assert_eq!(list[1].artist, "Yung Lean");
        assert_eq!(list[1].at, Some(1_700_000_000));
        // The grey star isn't a picture.
        assert_eq!(list[1].image, None);
        // One scrobble comes as an object, not a list.
        let v = json!({"recenttracks": {"track": {"artist": {"#text": "A"}, "name": "B",
            "date": {"uts": "5"}}, "@attr": {"total": "1", "totalPages": "1"}}});
        assert_eq!(parse_recent(&v).0.len(), 1);
    }

    #[test]
    fn reads_top_lists() {
        let v = json!({"topalbums": {"album": [
            {"artist": {"name": "Bladee"}, "image": image("https://x/300x300/b.jpg"), "playcount": "41",
             "@attr": {"rank": "1"}, "name": "Eversince", "url": "u"},
            {"artist": {"name": "Ecco2k"}, "image": [], "playcount": 7, "name": "E", "url": "u"}
        ], "@attr": {"total": "873"}}});
        let l = parse_top(&v, TopKind::Albums);
        assert_eq!(l.total, 873);
        assert_eq!(l.items[0].name, "Eversince");
        assert_eq!(l.items[0].artist, "Bladee");
        assert_eq!(l.items[0].plays, 41);
        assert_eq!(l.items[1].plays, 7);
        assert_eq!(l.items[1].image, None);
        let v = json!({"topartists": {"artist": [{"name": "Bladee", "playcount": "9",
            "image": image(&format!("https://x/300x300/{PLACEHOLDER}.png"))}], "@attr": {"total": "1"}}});
        let l = parse_top(&v, TopKind::Artists);
        assert_eq!(l.items[0].artist, "");
        assert_eq!(l.items[0].image, None);
    }

    fn scrobble(artist: &str, title: &str, album: &str, at: i64) -> Scrobble {
        Scrobble {
            artist: artist.into(),
            title: title.into(),
            album: album.into(),
            image: (!album.is_empty()).then(|| format!("https://x/{album}.jpg")),
            url: String::new(),
            at: Some(at),
            loved: false,
        }
    }

    #[test]
    fn today_is_worked_out_from_scrobbles() {
        let list = vec![
            scrobble("Bladee", "Waster", "Icedancer", 9),
            scrobble("Yung Lean", "Ginseng Strip 2002", "Unknown Death 2002", 8),
            scrobble("bladee", "Waster", "Icedancer", 7),
            scrobble("Bladee", "Be Nice 2 Me", "", 6),
            Scrobble {
                at: None,
                ..scrobble("Bladee", "Now", "", 10)
            },
        ];
        let artists = aggregate(&list, TopKind::Artists);
        assert_eq!(artists.total, 2);
        assert_eq!((artists.items[0].name.as_str(), artists.items[0].plays), ("Bladee", 3));
        let tracks = aggregate(&list, TopKind::Tracks);
        assert_eq!(tracks.total, 3);
        assert_eq!((tracks.items[0].name.as_str(), tracks.items[0].plays), ("Waster", 2));
        assert_eq!(tracks.items[0].image.as_deref(), Some("https://x/Icedancer.jpg"));
        // Songs without an album don't make an album.
        let albums = aggregate(&list, TopKind::Albums);
        assert_eq!(albums.total, 2);
        let s = summarize(&list);
        assert_eq!((s.scrobbles, s.artists, s.albums, s.tracks), (4, 2, 2, 3));
    }

    #[test]
    fn genres_follow_the_listening() {
        let tags = |list: &[(&str, u64)]| list.iter().map(|(t, w)| (t.to_string(), *w)).collect();
        let artists = vec![
            (
                "Bladee".to_string(),
                30,
                tags(&[("cloud rap", 100), ("seen live", 90), ("bladee", 50), ("swedish", 40)]),
            ),
            (
                "Yung Lean".to_string(),
                10,
                tags(&[("cloud rap", 100), ("hip-hop", 60)]),
            ),
        ];
        let shares = tag_shares(&artists);
        assert_eq!(shares[0].name, "cloud rap");
        assert!((shares[0].share - 1.0).abs() < 1e-6);
        assert!(shares.iter().all(|s| s.name != "seen live" && s.name != "bladee"));
        let swedish = shares.iter().find(|s| s.name == "swedish").unwrap();
        assert!((swedish.share - 0.3).abs() < 1e-6, "{}", swedish.share);
        assert!(tag_shares(&[]).is_empty());
    }

    #[test]
    fn chart_bars() {
        let offset = 3 * 3600;
        // Thursday 8 Oct 2026, 15:00 local time.
        let now = clock::day_start(clock::days_from_civil(2026, 10, 8), offset) + 15 * 3600;
        let week = buckets(ActivityRange::Week, now, offset, 0);
        assert_eq!(week.len(), 7);
        assert_eq!(week[6].0, "Thu");
        assert_eq!(week[6].1, "Today");
        assert_eq!(week[0].0, "Fri");
        assert_eq!(week[0].1, "Fri 2 Oct");
        // Bars follow on from each other.
        assert!(week.windows(2).all(|w| w[0].3 == w[1].2));
        assert!(week[6].2 <= now && now < week[6].3);
        let month = buckets(ActivityRange::Month, now, offset, 0);
        assert_eq!(month.len(), 30);
        assert_eq!(month[29].0, "8");
        let year = buckets(ActivityRange::Year, now, offset, 0);
        assert_eq!(year.len(), 12);
        assert_eq!(year[11].1, "Oct 2026");
        assert_eq!(year[0].1, "Nov 2025");
        assert!(year.windows(2).all(|w| w[0].3 == w[1].2));
        let joined = clock::day_start(clock::days_from_civil(2019, 6, 1), offset);
        let years = buckets(ActivityRange::Years, now, offset, joined);
        assert_eq!(years.first().unwrap().0, "2019");
        assert_eq!(years.last().unwrap().0, "2026");
        assert_eq!(buckets(ActivityRange::Years, now, offset, 0).len(), 1);
    }

    #[test]
    fn answers_stay_fresh_for_a_while() {
        let req = StatsRequest::Top(TopKind::Artists, Period::Week);
        let mut f = Fetch::default();
        assert!(f.wanted(req));
        f.loading = true;
        assert!(!f.wanted(req));
        f.loading = false;
        f.at = Some(Instant::now());
        assert!(!f.wanted(req));
        f.error = Some("offline".into());
        assert!(!f.wanted(req));
    }

    /// The stats are asked of Last.fm with the right calls, and the day's scrobbles are read
    /// once for all of Today's numbers.
    #[tokio::test]
    async fn loads_stats_from_lastfm() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split(' ').nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let reply = if path.contains("method=user.getInfo") {
                    json!({"user": {"name": "someone", "playcount": "1000",
                        "registered": {"unixtime": "1500000000"}}})
                } else if path.contains("method=user.getLovedTracks") {
                    json!({"lovedtracks": {"track": [], "@attr": {"total": "12"}}})
                } else if path.contains("method=user.getRecentTracks") && path.contains("limit=200") {
                    json!({"recenttracks": {"track": [
                        {"artist": {"#text": "Bladee"}, "name": "Waster", "album": {"#text": "Icedancer"},
                         "date": {"uts": "100"}},
                        {"artist": {"#text": "Bladee"}, "name": "Waster", "album": {"#text": "Icedancer"},
                         "date": {"uts": "90"}}
                    ], "@attr": {"total": "2", "totalPages": "1"}}})
                } else if path.contains("method=user.getRecentTracks") {
                    json!({"recenttracks": {"track": [], "@attr": {"total": "321", "totalPages": "321"}}})
                } else if path.contains("method=user.getTopArtists") {
                    json!({"topartists": {"artist": [{"name": "Bladee", "playcount": "40"}],
                        "@attr": {"total": "55"}}})
                } else if path.contains("method=artist.getTopAlbums") {
                    json!({"topalbums": {"album": [{"name": "Eversince",
                        "image": image("https://x/300x300/e.jpg")}]}})
                } else if path.contains("method=track.getInfo") {
                    json!({"error": 6, "message": "Track not found"})
                } else {
                    json!({"topalbums": {"album": [], "@attr": {"total": "7"}},
                           "toptracks": {"track": [], "@attr": {"total": "9"}}})
                }
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let lfm = Arc::new(
            Lastfm::new(
                http,
                "key",
                "secret",
                "",
                std::env::temp_dir().join("unused-queue.json"),
            )
            .with_root(&format!("{base}/2.0/")),
        );
        let stats = ProfileStats::new(lfm.clone(), "someone");
        assert!(stats.serves(&lfm, "someone"));
        assert!(!stats.serves(&lfm, "someone else"));

        let StatsData::User(u) = stats.load(StatsRequest::User).await.unwrap() else {
            panic!()
        };
        assert_eq!((u.name.as_str(), u.scrobbles, u.loved), ("someone", 1000, Some(12)));

        let StatsData::Summary(s) = stats.load(StatsRequest::Summary(Period::Week)).await.unwrap() else {
            panic!()
        };
        assert_eq!(
            (s.scrobbles, s.artists, s.albums, s.tracks, s.days),
            (321, 55, 7, 9, 7.0)
        );

        let StatsData::Top(t) = stats
            .load(StatsRequest::Top(TopKind::Tracks, Period::Today))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!((t.items[0].name.as_str(), t.items[0].plays), ("Waster", 2));
        let StatsData::Summary(s) = stats.load(StatsRequest::Summary(Period::Today)).await.unwrap() else {
            panic!()
        };
        assert_eq!(s.scrobbles, 2);

        let StatsData::Activity(bars) = stats.load(StatsRequest::Activity(ActivityRange::Week)).await.unwrap() else {
            panic!()
        };
        assert_eq!(bars.len(), 7);
        assert!(bars.iter().all(|b| b.plays == 321));

        assert_eq!(
            stats.picture("Bladee", "").await.as_deref(),
            Some("https://x/300x300/e.jpg")
        );
        assert_eq!(stats.picture("Bladee", "Nope").await, None);

        let seen_log = seen.clone();
        let seen = seen.lock().unwrap().clone();
        // Every call is for this user, with the key and no signature.
        assert!(seen.iter().all(|p| p.contains("api_key=key") && !p.contains("api_sig")));
        assert!(seen
            .iter()
            .filter(|p| p.contains("method=user."))
            .all(|p| p.contains("user=someone")));
        // Today's scrobbles were read once for both of Today's numbers.
        assert_eq!(seen.iter().filter(|p| p.contains("limit=200")).count(), 1);
        // A picture is looked up once.
        stats.picture("Bladee", "").await;
        let lookups = seen_log
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.contains("artist.getTopAlbums"))
            .count();
        assert_eq!(lookups, 1);
    }
}
