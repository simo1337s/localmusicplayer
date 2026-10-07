//! Main content views.

use std::sync::OnceLock;

use egui::{vec2, Align, Align2, Color32, CornerRadius, CursorIcon, Layout, Margin, Pos2, Rect, Sense, Ui};
use egui_phosphor::regular as icon;

use super::panels;
use super::theme::{self, *};
use super::widgets::{self, text_trunc, TableOpts};
use super::{Action, Cx, View};
use crate::library::Album;
use crate::model::{Playlist, PlaylistKind, Source, Track};
use crate::service::{Command, PlayStatus};

pub struct ViewState<'a> {
    pub view: &'a View,
    pub can_go_back: bool,
    pub search_text: &'a mut String,
    pub search_cache: &'a mut (String, u64, Vec<String>),
    pub filter_text: &'a mut String,
    pub focus_search: &'a mut bool,
}

pub fn show(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    match st.view.clone() {
        View::Home => home(ui, cx, st),
        View::Search => search(ui, cx, st),
        View::Songs => songs(ui, cx, st),
        View::Albums => albums(ui, cx, st),
        View::Album(key) => album(ui, cx, st, &key),
        View::Playlist(id) => playlist(ui, cx, st, &id),
        View::NowPlaying => now_playing(ui, cx),
        View::Settings => {}
    }
}

/// Scrollable page with an accent gradient behind its header.
fn page(ui: &mut Ui, id: &str, tint: Color32, content: impl FnOnce(&mut Ui, Rect, f32)) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let origin = ui.max_rect().top();
            let full = ui.max_rect();
            let grad = Rect::from_min_size(full.min, vec2(full.width(), 340.0));
            rounded_top_gradient(ui, grad, theme::with_alpha(tint, 110), theme::with_alpha(tint, 0));
            egui::Frame::new()
                .inner_margin(Margin {
                    left: 28,
                    right: 28,
                    top: 20,
                    bottom: 28,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    content(ui, viewport, origin)
                });
        });
}

fn rounded_top_gradient(ui: &Ui, rect: Rect, top: Color32, bottom: Color32) {
    let r = RADIUS as f32;
    ui.painter().rect_filled(
        Rect::from_min_size(rect.min, vec2(rect.width(), r)),
        CornerRadius {
            nw: RADIUS,
            ne: RADIUS,
            sw: 0,
            se: 0,
        },
        top,
    );
    widgets::gradient(ui, Rect::from_min_max(rect.min + vec2(0.0, r), rect.max), top, bottom);
}

fn back_button(ui: &mut Ui, cx: &mut Cx, st: &ViewState) {
    if st.can_go_back {
        if widgets::icon_button(ui, icon::CARET_LEFT, 18.0, TEXT, "Back").clicked() {
            cx.actions.push(Action::Back);
        }
        ui.add_space(4.0);
    }
}

fn greeting() -> &'static str {
    static HOUR: OnceLock<Option<u32>> = OnceLock::new();
    let hour = HOUR.get_or_init(|| {
        std::process::Command::new("date")
            .arg("+%H")
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
    });
    match hour {
        Some(5..=11) => "Good morning",
        Some(12..=17) => "Good afternoon",
        Some(_) => "Good evening",
        None => "Welcome back",
    }
}

// ------------------------------------------------------------------ home

fn home(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    let tint = cx.accent;
    page(ui, "home", tint, |ui, _viewport, _origin| {
        ui.horizontal(|ui| {
            back_button(ui, cx, st);
            ui.label(egui::RichText::new(greeting()).font(theme::bold_font(30.0)));
        });
        ui.add_space(14.0);

        let playlists: Vec<&Playlist> = cx.lib.playlists.iter().filter(|p| !p.track_ids.is_empty()).collect();

        if playlists.is_empty() && cx.lib.local.is_empty() {
            onboarding(ui, cx);
            return;
        }

        // Shortcut cards.
        let shortcuts: Vec<&Playlist> = playlists.iter().take(8).copied().collect();
        if !shortcuts.is_empty() {
            let gap = 12.0;
            let cols = if ui.available_width() > 900.0 { 4 } else { 2 };
            let w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
            for row in shortcuts.chunks(cols) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for p in row {
                        shortcut_card(ui, cx, p, w);
                    }
                });
                ui.add_space(gap * 0.6);
            }
            ui.add_space(18.0);
        }

        // Recently played.
        let recent: Vec<&Track> = cx.lib.recent.iter().filter_map(|id| cx.lib.get(id)).take(12).collect();
        if !recent.is_empty() {
            widgets::heading(ui, "Recently played");
            let tracks: Vec<Track> = recent.iter().map(|t| (*t).clone()).collect();
            let n = recent.len().min(tiles_per_row(ui, 160.0));
            widgets::grid(ui, n, 160.0, |ui, i, w| {
                let t = recent[i];
                let (resp, play) = widgets::tile(ui, w, &t.title, &t.artist, cx.accent, |ui, r| {
                    widgets::cover(ui, cx.art, t.art.as_deref(), r, 8, widgets::track_fallback(t))
                });
                if play || resp.double_clicked() {
                    cx.actions.push(Action::Cmd(Command::Play {
                        tracks: tracks.clone(),
                        start: i,
                        context: "Recently played".into(),
                    }));
                }
                let track = t.clone();
                resp.context_menu(|ui| widgets::track_menu(ui, cx, &track, None));
            });
            ui.add_space(10.0);
        }

        if !playlists.is_empty() {
            widgets::heading(ui, "Your playlists");
            widgets::grid(ui, playlists.len(), 168.0, |ui, i, w| {
                let p = playlists[i];
                let sub = match p.kind.source() {
                    Some(s) => format!("{} · {} songs", s.label(), p.track_ids.len()),
                    None => format!("{} songs", p.track_ids.len()),
                };
                let (resp, play) = widgets::tile(ui, w, &p.name, &sub, cx.accent, |ui, r| {
                    widgets::playlist_cover(ui, cx.art, p, r, 8)
                });
                if play {
                    play_playlist(cx, p, false);
                } else if resp.clicked() {
                    cx.actions.push(Action::Go(View::Playlist(p.id.clone())));
                }
            });
        }

        if !cx.lib.albums.is_empty() {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                widgets::heading(ui, "Albums in your library");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(egui::Label::new(egui::RichText::new("Show all").color(TEXT_DIM)).sense(Sense::click()))
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .clicked()
                    {
                        cx.actions.push(Action::Go(View::Albums));
                    }
                });
            });
            let n = cx.lib.albums.len().min(tiles_per_row(ui, 168.0) * 2);
            let albums: Vec<&Album> = cx.lib.albums.iter().take(n).collect();
            album_grid(ui, cx, &albums);
        }
    });
}

fn tiles_per_row(ui: &Ui, min_w: f32) -> usize {
    (((ui.available_width() + 20.0) / (min_w + 20.0)).floor() as usize).max(1)
}

fn shortcut_card(ui: &mut Ui, cx: &mut Cx, p: &Playlist, w: f32) {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 60.0), Sense::click());
    let fill = if resp.hovered() {
        theme::with_alpha(Color32::WHITE, 34)
    } else {
        theme::with_alpha(Color32::WHITE, 18)
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    let art = Rect::from_min_size(rect.min, vec2(60.0, 60.0));
    widgets::playlist_cover(ui, cx.art, p, art, 8);
    text_trunc(
        ui,
        Pos2::new(art.right() + 12.0, rect.center().y - 9.0),
        &p.name,
        theme::bold_font(14.0),
        TEXT,
        rect.right() - art.right() - 64.0,
    );
    if resp.hovered() {
        let c = Pos2::new(rect.right() - 26.0, rect.center().y);
        let over = ui.rect_contains_pointer(Rect::from_center_size(c, vec2(36.0, 36.0)));
        ui.painter().circle_filled(
            c,
            17.0,
            if over {
                theme::mix(cx.accent, Color32::WHITE, 0.15)
            } else {
                cx.accent
            },
        );
        ui.painter().text(
            c + vec2(1.0, 0.0),
            Align2::CENTER_CENTER,
            egui_phosphor::fill::PLAY,
            theme::fill_icon_font(15.0),
            theme::on_color(cx.accent),
        );
        if over && resp.clicked() {
            play_playlist(cx, p, false);
            return;
        }
    }
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        cx.actions.push(Action::Go(View::Playlist(p.id.clone())));
    }
}

fn onboarding(ui: &mut Ui, cx: &mut Cx) {
    widgets::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width().min(640.0));
        ui.label(egui::RichText::new("Let's bring in your music").font(theme::bold_font(20.0)));
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "MultiMusic plays your local files, Spotify (Premium) and SoundCloud in one place, \
                 with synced lyrics, Discord status and Last.fm scrobbling.",
            )
            .color(TEXT_DIM),
        );
        ui.add_space(12.0);
        for (glyph, color, text) in [
            (
                icon::FOLDER,
                source_color(Source::Local),
                "Add your music folders (or drop a folder onto this window)",
            ),
            (
                icon::SPOTIFY_LOGO,
                source_color(Source::Spotify),
                "Log in to Spotify to import playlists and Liked Songs",
            ),
            (
                icon::SOUNDCLOUD_LOGO,
                source_color(Source::SoundCloud),
                "Add your SoundCloud profile to import likes and playlists",
            ),
            (
                icon::APPLE_LOGO,
                source_color(Source::AppleMusic),
                "Import your Apple Music library (drop Library.xml here)",
            ),
        ] {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(glyph)
                        .family(theme::icons())
                        .size(18.0)
                        .color(color),
                );
                ui.label(text);
            });
        }
        ui.add_space(12.0);
        if widgets::pill(ui, "Open Settings", cx.accent, theme::on_color(cx.accent)).clicked() {
            cx.actions.push(Action::Go(View::Settings));
        }
    });
}

fn album_grid(ui: &mut Ui, cx: &mut Cx, albums: &[&Album]) {
    widgets::grid(ui, albums.len(), 168.0, |ui, i, w| {
        let a = albums[i];
        let first = a.track_ids.first().and_then(|id| cx.lib.get(id));
        let (resp, play) = widgets::tile(ui, w, &a.name, &a.artist, cx.accent, |ui, r| {
            widgets::cover(
                ui,
                cx.art,
                a.art.as_deref(),
                r,
                8,
                (theme::mix(source_color(Source::Local), PANEL, 0.4), icon::VINYL_RECORD),
            )
        });
        let _ = first;
        if play {
            cx.actions.push(Action::Cmd(Command::Play {
                tracks: cx.lib.tracks_for(&a.track_ids),
                start: 0,
                context: a.name.clone(),
            }));
        } else if resp.clicked() {
            cx.actions.push(Action::Go(View::Album(a.key.clone())));
        }
    });
}

fn play_playlist(cx: &mut Cx, p: &Playlist, shuffle: bool) {
    let tracks = cx.lib.tracks_for(&p.track_ids);
    if tracks.is_empty() {
        return;
    }
    cx.actions.push(Action::Cmd(Command::SetShuffle(shuffle)));
    let start = if shuffle {
        rand::random_range(0..tracks.len())
    } else {
        0
    };
    cx.actions.push(Action::Cmd(Command::Play {
        tracks,
        start,
        context: p.name.clone(),
    }));
}

// ------------------------------------------------------------------ list pages

/// Case-insensitive filter over title/artist/album.
fn filter<'a>(tracks: Vec<&'a Track>, needle: &str) -> Vec<&'a Track> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return tracks;
    }
    let terms: Vec<&str> = needle.split_whitespace().collect();
    tracks
        .into_iter()
        .filter(|t| {
            let hay = format!("{} {} {}", t.title, t.artist, t.album).to_lowercase();
            terms.iter().all(|term| hay.contains(term))
        })
        .collect()
}

struct Header<'a> {
    kind: String,
    title: &'a str,
    description: &'a str,
    meta: String,
    source: Option<Source>,
}

/// Big header (cover + titles) followed by the action row. Returns (play, shuffle).
fn list_header(
    ui: &mut Ui,
    cx: &mut Cx,
    st: &mut ViewState,
    h: &Header,
    draw_art: impl FnOnce(&Ui, Rect, &mut Cx),
) -> (bool, bool) {
    ui.horizontal(|ui| back_button(ui, cx, st));
    ui.add_space(4.0);
    let art_size = if ui.available_width() > 700.0 { 200.0 } else { 140.0 };
    ui.horizontal(|ui| {
        let (art_rect, _) = ui.allocate_exact_size(vec2(art_size, art_size), Sense::hover());
        theme::art_shadow(ui, art_rect, 10);
        draw_art(ui, art_rect, cx);
        ui.add_space(14.0);
        ui.vertical(|ui| {
            ui.set_min_height(art_size);
            ui.add_space(art_size - 128.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&h.kind).size(12.5).color(TEXT));
                if let Some(s) = h.source {
                    widgets::source_badge(ui, s);
                }
            });
            let w = ui.available_width();
            let size = if h.title.chars().count() > 28 { 30.0 } else { 44.0 };
            let (r, _) = ui.allocate_exact_size(vec2(w, size + 10.0), Sense::hover());
            text_trunc(ui, r.min, h.title, theme::bold_font(size), TEXT, w);
            if !h.description.is_empty() {
                let (r, _) = ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
                text_trunc(ui, r.min, h.description, theme::font(13.0), TEXT_DIM, w);
            }
            ui.label(egui::RichText::new(&h.meta).size(13.0).color(TEXT_DIM));
        });
    });
    ui.add_space(18.0);
    let mut play = false;
    let mut shuffle = false;
    ui.horizontal(|ui| {
        let playing_here = cx.player.context == h.title && cx.player.status == PlayStatus::Playing;
        if widgets::play_circle(ui, 54.0, cx.accent, playing_here).clicked() {
            if playing_here {
                cx.actions.push(Action::Cmd(Command::Pause));
            } else if cx.player.context == h.title && cx.player.status == PlayStatus::Paused {
                cx.actions.push(Action::Cmd(Command::Resume));
            } else {
                play = true;
            }
        }
        ui.add_space(6.0);
        if widgets::icon_button(ui, icon::SHUFFLE, 26.0, TEXT_DIM, "Shuffle play").clicked() {
            shuffle = true;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::TextEdit::singleline(st.filter_text)
                    .hint_text(theme::ic(icon::MAGNIFYING_GLASS, "Filter"))
                    .desired_width(200.0),
            );
        });
    });
    ui.add_space(10.0);
    (play, shuffle)
}

fn songs(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    let tint = theme::mix(source_color(Source::Local), cx.accent, 0.5);
    page(ui, "songs", tint, |ui, viewport, origin| {
        let all: Vec<&Track> = cx.lib.local.iter().filter_map(|id| cx.lib.get(id)).collect();
        let total_ms: u64 = all.iter().map(|t| t.duration_ms).sum();
        let header = Header {
            kind: "LIBRARY".into(),
            title: "Songs",
            description: "Every local file in your music folders",
            meta: format!("{} songs · {}", all.len(), theme::fmt_total(total_ms)),
            source: None,
        };
        let (play, shuffle) = list_header(ui, cx, st, &header, |ui, r, _| {
            widgets::placeholder(ui, r, Color32::from_rgb(0x4b, 0x6c, 0xd8), icon::MUSIC_NOTES, 10)
        });
        let tracks = filter(all, st.filter_text);
        if play || shuffle {
            play_list(cx, &tracks, shuffle, "Songs");
        }
        if tracks.is_empty() {
            panels::empty_state(ui, icon::FOLDER, "No local songs yet — add a music folder in Settings");
            return;
        }
        let opts = TableOpts {
            id: "songs",
            context: "Songs",
            playlist: None,
            show_album: true,
            show_header: true,
        };
        widgets::track_table(ui, cx, &tracks, &opts, viewport, origin);
    });
}

fn play_list(cx: &mut Cx, tracks: &[&Track], shuffle: bool, context: &str) {
    if tracks.is_empty() {
        return;
    }
    cx.actions.push(Action::Cmd(Command::SetShuffle(shuffle)));
    let start = if shuffle {
        rand::random_range(0..tracks.len())
    } else {
        0
    };
    cx.actions.push(Action::Cmd(Command::Play {
        tracks: tracks.iter().map(|t| (*t).clone()).collect(),
        start,
        context: context.to_string(),
    }));
}

fn albums(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    let tint = cx.accent;
    page(ui, "albums", tint, |ui, _viewport, _origin| {
        ui.horizontal(|ui| {
            back_button(ui, cx, st);
            ui.label(egui::RichText::new("Albums").font(theme::bold_font(30.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(st.filter_text)
                        .hint_text(theme::ic(icon::MAGNIFYING_GLASS, "Filter"))
                        .desired_width(200.0),
                );
            });
        });
        ui.add_space(14.0);
        let needle = st.filter_text.trim().to_lowercase();
        let list: Vec<&Album> = cx
            .lib
            .albums
            .iter()
            .filter(|a| {
                needle.is_empty()
                    || a.name.to_lowercase().contains(&needle)
                    || a.artist.to_lowercase().contains(&needle)
            })
            .collect();
        if list.is_empty() {
            panels::empty_state(ui, icon::VINYL_RECORD, "No albums yet");
            return;
        }
        album_grid(ui, cx, &list);
    });
}

fn album(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, key: &str) {
    let Some(a) = cx.lib.albums.iter().find(|a| a.key == key) else {
        panels::empty_state(ui, icon::VINYL_RECORD, "Album not found");
        return;
    };
    let accent = a.art.as_deref().and_then(|src| cx.art.accent(src)).unwrap_or(cx.accent);
    page(ui, "album", accent, |ui, viewport, origin| {
        let all: Vec<&Track> = a.track_ids.iter().filter_map(|id| cx.lib.get(id)).collect();
        let total_ms: u64 = all.iter().map(|t| t.duration_ms).sum();
        let header = Header {
            kind: "ALBUM".into(),
            title: &a.name,
            description: "",
            meta: format!("{} · {} songs · {}", a.artist, all.len(), theme::fmt_total(total_ms)),
            source: None,
        };
        let art = a.art.clone();
        let (play, shuffle) = list_header(ui, cx, st, &header, |ui, r, cx| {
            widgets::cover(
                ui,
                cx.art,
                art.as_deref(),
                r,
                10,
                (theme::mix(source_color(Source::Local), PANEL, 0.4), icon::VINYL_RECORD),
            )
        });
        let tracks = filter(all, st.filter_text);
        if play || shuffle {
            play_list(cx, &tracks, shuffle, &a.name);
        }
        let opts = TableOpts {
            id: "album",
            context: &a.name,
            playlist: None,
            show_album: false,
            show_header: true,
        };
        widgets::track_table(ui, cx, &tracks, &opts, viewport, origin);
    });
}

fn playlist(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState, id: &str) {
    let Some(p) = cx.lib.playlist(id) else {
        panels::empty_state(ui, icon::PLAYLIST, "Playlist not found");
        return;
    };
    let tint = if matches!(
        p.kind,
        PlaylistKind::Liked | PlaylistKind::SpotifyLiked | PlaylistKind::SoundCloudLikes
    ) {
        widgets::playlist_fallback(p).0
    } else {
        p.art.as_deref().and_then(|a| cx.art.accent(a)).unwrap_or(cx.accent)
    };
    page(ui, "playlist", tint, |ui, viewport, origin| {
        let all: Vec<&Track> = p.track_ids.iter().filter_map(|id| cx.lib.get(id)).collect();
        let total_ms: u64 = all.iter().map(|t| t.duration_ms).sum();
        let header = Header {
            kind: if p.kind.source().is_some() || p.kind == PlaylistKind::M3u {
                "PLAYLIST ·".into()
            } else {
                "PLAYLIST".into()
            },
            title: &p.name,
            description: &p.description,
            meta: format!("{} songs · {}", all.len(), theme::fmt_total(total_ms)),
            source: if p.kind == PlaylistKind::M3u {
                Some(Source::Local)
            } else {
                p.kind.source()
            },
        };
        let (play, shuffle) = list_header(ui, cx, st, &header, |ui, r, cx| {
            widgets::playlist_cover(ui, cx.art, p, r, 10)
        });
        // Playlist actions.
        ui.horizontal(|ui| {
            match p.kind {
                PlaylistKind::Spotify | PlaylistKind::SpotifyLiked => {
                    if ui
                        .button(theme::ic(icon::ARROWS_CLOCKWISE, "Sync with Spotify"))
                        .clicked()
                    {
                        cx.actions.push(Action::Cmd(Command::SyncSpotify));
                    }
                }
                PlaylistKind::SoundCloud | PlaylistKind::SoundCloudLikes => {
                    if ui
                        .button(theme::ic(icon::ARROWS_CLOCKWISE, "Sync with SoundCloud"))
                        .clicked()
                    {
                        cx.actions.push(Action::Cmd(Command::SyncSoundCloud));
                    }
                }
                PlaylistKind::Custom | PlaylistKind::M3u => {
                    if ui.button(theme::ic(icon::TEXT_ALIGN_LEFT, "Rename")).clicked() {
                        cx.actions.push(Action::Rename(p.id.clone(), p.name.clone()));
                    }
                    if ui.button(theme::ic(icon::TRASH, "Delete")).clicked() {
                        cx.actions.push(Action::Delete(p.id.clone()));
                    }
                }
                _ => {}
            }
            if p.track_ids.iter().any(|id| id.starts_with("local:"))
                && ui.button(theme::ic(icon::ARROW_SQUARE_OUT, "Export .m3u")).clicked()
            {
                let dir = directories::UserDirs::new()
                    .and_then(|u| u.audio_dir().map(|d| d.to_path_buf()))
                    .unwrap_or_else(std::env::temp_dir);
                let safe: String = p
                    .name
                    .chars()
                    .map(|c| {
                        if c.is_alphanumeric() || c == ' ' || c == '-' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                cx.actions.push(Action::Cmd(Command::ExportM3u {
                    playlist_id: p.id.clone(),
                    path: dir.join(format!("{safe}.m3u")),
                }));
            }
            if p.kind == PlaylistKind::AppleMusic {
                ui.label(
                    egui::RichText::new(icon::APPLE_LOGO)
                        .family(theme::icons())
                        .color(TEXT_FAINT),
                );
                ui.label(
                    egui::RichText::new("Songs play from your local files, Spotify or SoundCloud")
                        .small()
                        .color(TEXT_FAINT),
                );
            }
        });
        ui.add_space(6.0);
        let tracks = filter(all, st.filter_text);
        if play || shuffle {
            play_list(cx, &tracks, shuffle, &p.name);
        }
        if tracks.is_empty() {
            let msg = if p.kind == PlaylistKind::Liked {
                "Songs you like (♥) from any source show up here"
            } else {
                "This playlist is empty"
            };
            panels::empty_state(ui, icon::MUSIC_NOTES, msg);
            return;
        }
        let opts = TableOpts {
            id: "playlist",
            context: &p.name,
            playlist: Some(p),
            show_album: true,
            show_header: true,
        };
        widgets::track_table(ui, cx, &tracks, &opts, viewport, origin);
    });
}

// ------------------------------------------------------------------ search

fn search(ui: &mut Ui, cx: &mut Cx, st: &mut ViewState) {
    let tint = cx.accent;
    page(ui, "search", tint, |ui, viewport, origin| {
        ui.horizontal(|ui| {
            back_button(ui, cx, st);
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width().min(520.0), 46.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(23), CARD);
            ui.painter().text(
                rect.left_center() + vec2(22.0, 0.0),
                Align2::CENTER_CENTER,
                icon::MAGNIFYING_GLASS,
                theme::icon_font(18.0),
                TEXT_DIM,
            );
            let edit_rect = Rect::from_min_max(rect.min + vec2(42.0, 11.0), rect.max - vec2(16.0, 9.0));
            let resp = ui.put(
                edit_rect,
                egui::TextEdit::singleline(st.search_text)
                    .hint_text("What do you want to listen to?")
                    .frame(egui::Frame::NONE)
                    .font(theme::font(15.0)),
            );
            if std::mem::take(st.focus_search) {
                resp.request_focus();
            }
        });
        ui.add_space(18.0);

        let q = st.search_text.trim().to_string();
        if q.is_empty() {
            search_tips(ui, cx);
            return;
        }

        // Library results (cached until the library changes).
        if st.search_cache.0 != q || st.search_cache.1 != cx.lib.version {
            let hits = cx.lib.search(&q, 50);
            *st.search_cache = (q.clone(), cx.lib.version, hits.into_iter().map(|t| t.id).collect());
        }
        let local: Vec<&Track> = st.search_cache.2.iter().filter_map(|id| cx.lib.get(id)).collect();
        let search = &cx.feed.search;
        let feed_matches = search.query == q;
        let spotify: Vec<Track> = if feed_matches {
            search.spotify.clone()
        } else {
            Vec::new()
        };
        let soundcloud: Vec<Track> = if feed_matches {
            search.soundcloud.clone()
        } else {
            Vec::new()
        };
        let pending = !feed_matches || search.pending > 0;
        let errors = if feed_matches {
            search.errors.clone()
        } else {
            Vec::new()
        };

        let ctx_name = format!("Search: {q}");
        if !local.is_empty() {
            widgets::heading(ui, "In your library");
            let opts = TableOpts {
                id: "s-local",
                context: &ctx_name,
                playlist: None,
                show_album: true,
                show_header: false,
            };
            widgets::track_table(ui, cx, &local, &opts, viewport, origin);
            ui.add_space(16.0);
        }
        let spotify_on = cx.feed.spotify_logged_in;
        if spotify_on {
            widgets::heading_icon(ui, icon::SPOTIFY_LOGO, source_color(Source::Spotify), "Spotify");
            if spotify.is_empty() {
                remote_status(ui, pending);
            } else {
                let refs: Vec<&Track> = spotify.iter().collect();
                let opts = TableOpts {
                    id: "s-spotify",
                    context: &ctx_name,
                    playlist: None,
                    show_album: true,
                    show_header: false,
                };
                widgets::track_table(ui, cx, &refs, &opts, viewport, origin);
            }
            ui.add_space(16.0);
        }
        if !soundcloud.is_empty() || pending {
            widgets::heading_icon(
                ui,
                icon::SOUNDCLOUD_LOGO,
                source_color(Source::SoundCloud),
                "SoundCloud",
            );
            if soundcloud.is_empty() {
                remote_status(ui, pending);
            } else {
                let refs: Vec<&Track> = soundcloud.iter().collect();
                let opts = TableOpts {
                    id: "s-sc",
                    context: &ctx_name,
                    playlist: None,
                    show_album: false,
                    show_header: false,
                };
                widgets::track_table(ui, cx, &refs, &opts, viewport, origin);
            }
        }
        for e in errors {
            ui.label(egui::RichText::new(e).small().color(DANGER));
        }
        if local.is_empty() && spotify.is_empty() && soundcloud.is_empty() && !pending {
            panels::empty_state(ui, icon::MAGNIFYING_GLASS, &format!("No results for “{q}”"));
        }
    });
}

fn remote_status(ui: &mut Ui, pending: bool) {
    ui.horizontal(|ui| {
        if pending {
            ui.spinner();
            ui.label(egui::RichText::new("Searching…").color(TEXT_DIM));
        } else {
            ui.label(egui::RichText::new("No results").color(TEXT_FAINT));
        }
    });
}

fn search_tips(ui: &mut Ui, cx: &mut Cx) {
    widgets::heading(ui, "Search everywhere");
    let sources = [
        (Source::Local, "Your library", "Local files and every imported playlist"),
        (
            Source::Spotify,
            "Spotify",
            "Log in under Settings to search the whole catalogue",
        ),
        (Source::SoundCloud, "SoundCloud", "Searches SoundCloud's public tracks"),
        (
            Source::AppleMusic,
            "Apple Music",
            "Imported songs play via a matching source",
        ),
    ];
    widgets::grid(ui, sources.len(), 200.0, |ui, i, w| {
        let (s, title, sub) = sources[i];
        let (rect, _) = ui.allocate_exact_size(vec2(w, 110.0), Sense::hover());
        let base = source_color(s);
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), theme::mix(base, PANEL, 0.55));
        ui.painter().text(
            rect.left_top() + vec2(16.0, 16.0),
            Align2::LEFT_TOP,
            title,
            theme::bold_font(18.0),
            Color32::WHITE,
        );
        text_trunc(
            ui,
            rect.left_top() + vec2(16.0, 44.0),
            sub,
            theme::font(12.0),
            theme::with_alpha(Color32::WHITE, 200),
            w - 70.0,
        );
        ui.painter().text(
            rect.right_bottom() - vec2(16.0, 12.0),
            Align2::RIGHT_BOTTOM,
            theme::source_icon(s),
            theme::icon_font(40.0),
            theme::with_alpha(Color32::WHITE, 170),
        );
    });
    let _ = cx;
}

// ------------------------------------------------------------------ now playing

fn now_playing(ui: &mut Ui, cx: &mut Cx) {
    let full = ui.max_rect();
    let tint = cx.accent;
    rounded_top_gradient(
        ui,
        Rect::from_min_size(full.min, vec2(full.width(), full.height())),
        theme::with_alpha(tint, 150),
        theme::with_alpha(tint, 10),
    );
    let Some(t) = cx.player.current.clone() else {
        panels::empty_state(ui, icon::VINYL_RECORD, "Nothing is playing");
        return;
    };
    egui::Frame::new().inner_margin(Margin::same(28)).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.horizontal(|ui| {
            if widgets::icon_button(ui, icon::CORNERS_IN, 18.0, TEXT, "Close (Esc)").clicked() {
                cx.actions.push(Action::Back);
            }
            ui.label(
                egui::RichText::new(format!("PLAYING FROM {}", cx.player.context.to_uppercase()))
                    .size(12.0)
                    .color(TEXT_DIM),
            );
        });
        ui.add_space(12.0);
        let avail = ui.available_size();
        let wide = avail.x > 860.0;
        let art_src = t
            .art
            .clone()
            .or_else(|| cx.player.via.as_ref().and_then(|v| v.art.clone()));
        if wide {
            let art_size = (avail.y - 120.0).min(avail.x * 0.42).clamp(200.0, 560.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(art_size);
                    let (r, _) = ui.allocate_exact_size(vec2(art_size, art_size), Sense::hover());
                    theme::art_shadow(ui, r, 12);
                    widgets::cover(ui, cx.art, art_src.as_deref(), r, 12, widgets::track_fallback(&t));
                    ui.add_space(18.0);
                    track_titles(ui, cx, &t, art_size);
                });
                ui.add_space(40.0);
                ui.vertical(|ui| {
                    ui.set_height(avail.y);
                    panels::lyrics(ui, cx, true);
                });
            });
        } else {
            let art_size = (avail.x * 0.5).clamp(140.0, 320.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(art_size, art_size), Sense::hover());
                widgets::cover(ui, cx.art, art_src.as_deref(), r, 12, widgets::track_fallback(&t));
                ui.add_space(16.0);
                ui.vertical(|ui| track_titles(ui, cx, &t, avail.x - art_size - 20.0));
            });
            ui.add_space(16.0);
            panels::lyrics(ui, cx, true);
        }
    });
}

fn track_titles(ui: &mut Ui, cx: &mut Cx, t: &Track, w: f32) {
    let (r, _) = ui.allocate_exact_size(vec2(w, 36.0), Sense::hover());
    text_trunc(ui, r.min, &t.title, theme::bold_font(28.0), TEXT, w);
    let (r, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
    text_trunc(ui, r.min, &t.artist, theme::font(17.0), TEXT_DIM, w);
    if !t.album.is_empty() {
        let (r, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
        text_trunc(ui, r.min, &t.album, theme::font(13.5), TEXT_FAINT, w);
    }
    ui.add_space(4.0);
    let source = cx.player.via.as_ref().map(|v| v.source).unwrap_or(t.source);
    widgets::source_badge(ui, source);
    if let Some(q) = &cx.player.quality {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(w.min(320.0), 20.0), Sense::hover());
            let used = text_trunc(
                ui,
                r.left_top() + vec2(0.0, 2.0),
                &q.label(),
                theme::font(12.5),
                TEXT_DIM,
                r.width() - 70.0,
            );
            widgets::quality_badge(ui, Pos2::new(used.right() + 8.0, used.center().y), q);
        });
    }
}
