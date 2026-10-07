//! Window chrome: sidebar, player bar and the right panel (lyrics / queue).

use egui::{vec2, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Layout, Margin, Pos2, Rect, Sense, Ui, UiBuilder};
use egui_phosphor::regular as icon;

use super::theme::{self, *};
use super::widgets::{self, text_trunc};
use super::{Action, Cx, RightTab, View};
use crate::model::{PlaylistKind, RepeatMode};
use crate::service::{Command, PlayStatus};

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(12))
}

// ------------------------------------------------------------------ sidebar

pub fn sidebar(ui: &mut Ui, cx: &mut Cx, view: &View) {
    egui::Panel::left("sidebar")
        .exact_size(272.0)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(Margin { left: 8, right: 8, top: 8, bottom: 0 }))
        .show(ui, |ui| {
            card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Logo.
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(icon::WAVEFORM).size(24.0).color(cx.accent));
                    ui.label(egui::RichText::new("Medley").font(theme::bold_font(21.0)));
                });
                ui.add_space(8.0);
                nav_item(ui, cx, icon::HOUSE, "Home", View::Home, view);
                nav_item(ui, cx, icon::MAGNIFYING_GLASS, "Search", View::Search, view);
                nav_item(ui, cx, icon::MUSIC_NOTES, "Songs", View::Songs, view);
                nav_item(ui, cx, icon::VINYL_RECORD, "Albums", View::Albums, view);
                nav_item(ui, cx, icon::GEAR, "Settings", View::Settings, view);
            });
            ui.add_space(8.0);
            card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(ui.available_height() - 8.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(icon::BOOKS).size(18.0).color(TEXT_DIM));
                    ui.label(egui::RichText::new("Your Library").font(theme::bold_font(15.0)).color(TEXT_DIM));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button(ui, icon::PLUS, 16.0, TEXT_DIM, "New playlist").clicked() {
                            cx.actions.push(Action::NewPlaylist(Vec::new()));
                        }
                    });
                });
                if let Some((done, total)) = cx.feed.scan {
                    ui.label(
                        egui::RichText::new(if total == 0 {
                            "Scanning folders…".to_string()
                        } else {
                            format!("Scanning {done}/{total}…")
                        })
                        .small()
                        .color(TEXT_FAINT),
                    );
                }
                ui.add_space(4.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let playing_context = cx.player.context.clone();
                    for p in &cx.lib.playlists {
                        if p.kind != PlaylistKind::Liked && p.track_ids.is_empty() && p.kind != PlaylistKind::Custom {
                            continue;
                        }
                        let active = *view == View::Playlist(p.id.clone());
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), Sense::click());
                        if active {
                            ui.painter().rect_filled(rect, CornerRadius::same(8), SELECTED);
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, CornerRadius::same(8), HOVER);
                        }
                        let art = Rect::from_min_size(rect.min + vec2(6.0, 6.0), vec2(44.0, 44.0));
                        widgets::playlist_cover(ui, cx.art, p, art, 6);
                        let x = art.right() + 10.0;
                        let w = rect.right() - x - 6.0;
                        let is_playing = playing_context == p.name;
                        text_trunc(ui, Pos2::new(x, rect.top() + 10.0), &p.name, theme::font(14.0), if is_playing { cx.accent } else { TEXT }, w);
                        let src = match p.kind.source() {
                            Some(s) => format!("{} · {}", theme::source_icon(s), s.label()),
                            None => "Playlist".into(),
                        };
                        text_trunc(
                            ui,
                            Pos2::new(x, rect.top() + 30.0),
                            &format!("{src} · {} songs", p.track_ids.len()),
                            theme::font(12.0),
                            TEXT_DIM,
                            w,
                        );
                        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
                        if resp.clicked() {
                            cx.actions.push(Action::Go(View::Playlist(p.id.clone())));
                        }
                        resp.context_menu(|ui| {
                            ui.set_min_width(180.0);
                            if ui.button(format!("{}  Play", icon::PLAY)).clicked() {
                                cx.actions.push(Action::Cmd(Command::Play {
                                    tracks: cx.lib.tracks_for(&p.track_ids),
                                    start: 0,
                                    context: p.name.clone(),
                                }));
                                ui.close();
                            }
                            if ui.button(format!("{}  Add to queue", icon::LIST_PLUS)).clicked() {
                                cx.actions.push(Action::Cmd(Command::Enqueue(cx.lib.tracks_for(&p.track_ids))));
                                ui.close();
                            }
                            if p.kind.is_editable() && p.kind != PlaylistKind::Liked {
                                ui.separator();
                                if ui.button(format!("{}  Rename", icon::TEXT_ALIGN_LEFT)).clicked() {
                                    cx.actions.push(Action::Rename(p.id.clone(), p.name.clone()));
                                    ui.close();
                                }
                                if ui.button(format!("{}  Delete", icon::TRASH)).clicked() {
                                    cx.actions.push(Action::Delete(p.id.clone()));
                                    ui.close();
                                }
                            } else if p.kind != PlaylistKind::Liked {
                                ui.separator();
                                if ui.button(format!("{}  Remove from Medley", icon::TRASH)).clicked() {
                                    cx.actions.push(Action::Delete(p.id.clone()));
                                    ui.close();
                                }
                            }
                        });
                    }
                    if cx.lib.playlists.len() <= 1 && cx.lib.local.is_empty() {
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new("Add a music folder, or connect Spotify / SoundCloud in Settings to see your playlists here.")
                                .small()
                                .color(TEXT_FAINT),
                        );
                    }
                });
            });
        });
}

fn nav_item(ui: &mut Ui, cx: &mut Cx, glyph: &str, label: &str, target: View, current: &View) {
    let active = *current == target
        || (target == View::Albums && matches!(current, View::Album(_)));
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    let color = if active || resp.hovered() { TEXT } else { TEXT_DIM };
    if active {
        ui.painter().rect_filled(rect, CornerRadius::same(8), theme::with_alpha(Color32::WHITE, 10));
    }
    let font = if active { theme::fill_icon_font(20.0) } else { FontId::proportional(20.0) };
    let glyph = if active { fill_variant(glyph) } else { glyph };
    ui.painter().text(rect.left_center() + vec2(14.0, 0.0), Align2::CENTER_CENTER, glyph, font, color);
    ui.painter().text(
        rect.left_center() + vec2(36.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        if active { theme::bold_font(15.0) } else { theme::font(15.0) },
        color,
    );
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        cx.actions.push(Action::Go(target));
    }
}

/// The filled twin of a regular Phosphor glyph (same codepoint, different font).
fn fill_variant(glyph: &str) -> &str {
    glyph
}

// ------------------------------------------------------------------ player bar

pub fn player_bar(ui: &mut Ui, cx: &mut Cx, right: Option<RightTab>) {
    egui::Panel::bottom("player")
        .exact_size(92.0)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(WINDOW_BG).inner_margin(Margin::symmetric(16, 10)))
        .show(ui, |ui| {
            let full = ui.max_rect();
            let side_w = (full.width() * 0.3).clamp(220.0, 420.0);
            let left = Rect::from_min_size(full.min, vec2(side_w, full.height()));
            let right_r = Rect::from_min_max(Pos2::new(full.right() - side_w, full.top()), full.max);
            let center = Rect::from_min_max(Pos2::new(left.right() + 16.0, full.top()), Pos2::new(right_r.left() - 16.0, full.bottom()));

            now_playing_info(ui, cx, left);
            ui.scope_builder(UiBuilder::new().max_rect(center), |ui| transport(ui, cx));
            ui.scope_builder(UiBuilder::new().max_rect(right_r).layout(Layout::right_to_left(Align::Center)), |ui| {
                extras(ui, cx, right)
            });
        });
}

fn now_playing_info(ui: &mut Ui, cx: &mut Cx, rect: Rect) {
    let Some(t) = cx.player.current.clone() else {
        ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, "Nothing playing", theme::font(13.0), TEXT_FAINT);
        return;
    };
    let art_rect = Rect::from_min_size(Pos2::new(rect.left(), rect.center().y - 32.0), vec2(64.0, 64.0));
    let art_src = t.art.clone().or_else(|| cx.player.via.as_ref().and_then(|v| v.art.clone()));
    widgets::cover(ui, cx.art, art_src.as_deref(), art_rect, 8, widgets::track_fallback(&t));
    let art_resp = ui.interact(art_rect, Id::new("np-art"), Sense::click()).on_hover_text("Now playing view (L)");
    if art_resp.clicked() {
        cx.actions.push(Action::Go(View::NowPlaying));
    }
    let x = art_rect.right() + 14.0;
    let w = rect.right() - x - 40.0;
    let tr = text_trunc(ui, Pos2::new(x, rect.center().y - 22.0), &t.title, theme::bold_font(14.5), TEXT, w);
    let ar = text_trunc(ui, Pos2::new(x, rect.center().y - 2.0), &t.artist, theme::font(12.5), TEXT_DIM, w);
    let title_resp = ui.interact(tr.union(ar), Id::new("np-title"), Sense::click());
    if title_resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        cx.actions.push(Action::Go(View::NowPlaying));
    }
    let via = match &cx.player.via {
        Some(v) => format!("{} via {}", theme::source_icon(v.source), v.source.label()),
        None => format!("{} {}", theme::source_icon(t.source), t.source.label()),
    };
    let src_color = cx.player.via.as_ref().map(|v| v.source).unwrap_or(t.source);
    text_trunc(ui, Pos2::new(x, rect.center().y + 16.0), &via, theme::font(11.5), theme::with_alpha(source_color(src_color), 200), w);

    // Like button.
    let liked = cx.lib.is_liked(&t.id);
    let heart = Rect::from_center_size(Pos2::new(rect.right() - 18.0, rect.center().y - 12.0), vec2(28.0, 28.0));
    let resp = ui.interact(heart, Id::new("np-like"), Sense::click());
    let (glyph, font, color) = if liked {
        (egui_phosphor::fill::HEART, theme::fill_icon_font(18.0), cx.accent)
    } else {
        (icon::HEART, FontId::proportional(18.0), if resp.hovered() { TEXT } else { TEXT_DIM })
    };
    ui.painter().text(heart.center(), Align2::CENTER_CENTER, glyph, font, color);
    if resp.on_hover_text(if liked { "Remove from Liked Songs" } else { "Save to Liked Songs" }).clicked() {
        cx.actions.push(Action::Cmd(Command::ToggleLike(t)));
    }
}

fn transport(ui: &mut Ui, cx: &mut Cx) {
    let p = cx.player;
    let w = ui.available_width();
    ui.vertical_centered(|ui| {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let controls_w = 5.0 * 34.0 + 44.0 + 4.0 * 8.0;
            ui.add_space(((w - controls_w) / 2.0).max(0.0));
            ui.spacing_mut().item_spacing.x = 8.0;
            let shuffle_c = if p.shuffle { cx.accent } else { TEXT_DIM };
            if widgets::icon_button(ui, icon::SHUFFLE, 18.0, shuffle_c, "Shuffle").clicked() {
                cx.actions.push(Action::Cmd(Command::SetShuffle(!p.shuffle)));
            }
            if widgets::icon_button(ui, egui_phosphor::regular::SKIP_BACK, 20.0, TEXT, "Previous").clicked() {
                cx.actions.push(Action::Cmd(Command::Previous));
            }
            let playing = p.status == PlayStatus::Playing;
            if widgets::play_circle(ui, 40.0, Color32::WHITE, playing).clicked() {
                cx.actions.push(Action::Cmd(Command::TogglePause));
            }
            if widgets::icon_button(ui, egui_phosphor::regular::SKIP_FORWARD, 20.0, TEXT, "Next").clicked() {
                cx.actions.push(Action::Cmd(Command::Next));
            }
            let (rep_icon, rep_c) = match p.repeat {
                RepeatMode::Off => (icon::REPEAT, TEXT_DIM),
                RepeatMode::All => (icon::REPEAT, cx.accent),
                RepeatMode::One => (icon::REPEAT_ONCE, cx.accent),
            };
            if widgets::icon_button(ui, rep_icon, 18.0, rep_c, "Repeat").clicked() {
                cx.actions.push(Action::Cmd(Command::CycleRepeat));
            }
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let pos = p.position_now();
            let dur = p.duration;
            let time_w = 44.0;
            let bar_w = (w - 2.0 * time_w - 24.0).max(60.0);
            ui.add_space(((w - bar_w - 2.0 * time_w - 16.0) / 2.0).max(0.0));
            let id = Id::new("seek");
            // While dragging, show the dragged position instead of the playing one.
            let drag: Option<f32> = ui.data(|d| d.get_temp(id));
            let shown = drag.map(|f| f as f64 * dur).unwrap_or(pos);
            ui.add_sized(vec2(time_w, 16.0), egui::Label::new(egui::RichText::new(theme::fmt_time(shown)).size(11.5).color(TEXT_DIM)));
            let frac = if dur > 0.0 { (shown / dur) as f32 } else { 0.0 };
            let (resp, changed) = widgets::bar(ui, id, bar_w, frac, cx.accent);
            if let Some(f) = changed {
                ui.data_mut(|d| d.insert_temp(id, f));
            }
            if resp.drag_stopped() || resp.clicked() {
                if let Some(f) = ui.data(|d| d.get_temp::<f32>(id)) {
                    cx.actions.push(Action::Cmd(Command::Seek(f as f64 * dur)));
                }
                ui.data_mut(|d| d.remove::<f32>(id));
            }
            let total = if dur > 0.0 { theme::fmt_time(dur) } else { "–:––".into() };
            ui.add_sized(vec2(time_w, 16.0), egui::Label::new(egui::RichText::new(total).size(11.5).color(TEXT_DIM)));
        });
    });
}

fn extras(ui: &mut Ui, cx: &mut Cx, right: Option<RightTab>) {
    ui.spacing_mut().item_spacing.x = 4.0;
    // Volume (right-to-left layout: drawn from the right edge).
    let vol = cx.player.volume;
    let id = Id::new("volume");
    let (_, changed) = widgets::bar(ui, id, 110.0, vol / 100.0, cx.accent);
    if let Some(f) = changed {
        cx.actions.push(Action::Cmd(Command::SetVolume(f * 100.0)));
    }
    let vol_icon = if vol <= 0.5 {
        icon::SPEAKER_X
    } else if vol < 50.0 {
        icon::SPEAKER_LOW
    } else {
        icon::SPEAKER_HIGH
    };
    if widgets::icon_button(ui, vol_icon, 18.0, TEXT_DIM, "Mute").clicked() {
        let muted: Option<f32> = ui.data(|d| d.get_temp(Id::new("pre-mute")));
        if vol > 0.5 {
            ui.data_mut(|d| d.insert_temp(Id::new("pre-mute"), vol));
            cx.actions.push(Action::Cmd(Command::SetVolume(0.0)));
        } else {
            cx.actions.push(Action::Cmd(Command::SetVolume(muted.unwrap_or(70.0))));
        }
    }
    ui.add_space(6.0);
    if widgets::icon_button(ui, icon::CORNERS_OUT, 18.0, TEXT_DIM, "Now playing view (L)").clicked() {
        cx.actions.push(Action::Go(View::NowPlaying));
    }
    let q_c = if right == Some(RightTab::Queue) { cx.accent } else { TEXT_DIM };
    if widgets::icon_button(ui, icon::QUEUE, 18.0, q_c, "Queue").clicked() {
        cx.actions.push(Action::RightTab(RightTab::Queue));
    }
    let l_c = if right == Some(RightTab::Lyrics) { cx.accent } else { TEXT_DIM };
    if widgets::icon_button(ui, icon::MICROPHONE_STAGE, 18.0, l_c, "Lyrics").clicked() {
        cx.actions.push(Action::RightTab(RightTab::Lyrics));
    }
}

// ------------------------------------------------------------------ right panel

pub fn right_panel(ui: &mut Ui, cx: &mut Cx, tab: RightTab) {
    egui::Panel::right("right")
        .exact_size(340.0)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(Margin { left: 8, right: 8, top: 8, bottom: 0 }))
        .show(ui, |ui| {
            card().inner_margin(Margin::same(14)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(ui.available_height());
                ui.horizontal(|ui| {
                    for (t, label) in [(RightTab::Lyrics, "Lyrics"), (RightTab::Queue, "Queue")] {
                        let active = t == tab;
                        let r = ui.add(
                            egui::Label::new(
                                egui::RichText::new(label)
                                    .font(theme::bold_font(16.0))
                                    .color(if active { TEXT } else { TEXT_FAINT }),
                            )
                            .sense(Sense::click()),
                        );
                        if active {
                            let y = r.rect.bottom() + 3.0;
                            ui.painter().line_segment(
                                [Pos2::new(r.rect.left(), y), Pos2::new(r.rect.right(), y)],
                                egui::Stroke::new(2.0, cx.accent),
                            );
                        }
                        if r.on_hover_cursor(CursorIcon::PointingHand).clicked() && !active {
                            cx.actions.push(Action::RightTab(t));
                        }
                        ui.add_space(10.0);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button(ui, icon::X, 14.0, TEXT_DIM, "Close").clicked() {
                            cx.actions.push(Action::ToggleRightPanel);
                        }
                    });
                });
                ui.add_space(10.0);
                match tab {
                    RightTab::Lyrics => lyrics(ui, cx, false),
                    RightTab::Queue => queue(ui, cx),
                }
            });
        });
}

/// Synced lyrics. `big` is used by the now playing view.
pub fn lyrics(ui: &mut Ui, cx: &mut Cx, big: bool) {
    let Some(t) = cx.player.current.as_ref() else {
        empty_state(ui, icon::MICROPHONE_STAGE, "Play something to see its lyrics");
        return;
    };
    let state = &cx.feed.lyrics;
    if state.track_id != t.id || state.loading {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(egui::RichText::new("Looking for lyrics…").color(TEXT_DIM));
        });
        return;
    }
    let Some(lyrics) = state.lyrics.as_ref() else {
        empty_state(ui, icon::MICROPHONE_STAGE, "No lyrics found for this song");
        return;
    };
    if lyrics.instrumental {
        empty_state(ui, icon::MUSIC_NOTES, "Instrumental");
        return;
    }
    let size = if big { 30.0 } else { 19.0 };
    let pos_ms = (cx.player.position_now() * 1000.0) as u64;
    let id = Id::new(("lyrics-scroll", big));
    if lyrics.synced.is_empty() {
        egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
            ui.label(egui::RichText::new("Unsynced lyrics").small().color(TEXT_FAINT));
            ui.add_space(6.0);
            ui.label(egui::RichText::new(&lyrics.plain).font(theme::bold_font(size * 0.8)).color(TEXT_DIM));
            provider(ui, &lyrics.provider);
        });
        return;
    }
    let current = lyrics.line_at(pos_ms);
    let last_key = id.with("last");
    let last: Option<Option<usize>> = ui.data(|d| d.get_temp(last_key));
    let changed = last != Some(current);
    // Pause auto-scroll for a few seconds after the user scrolls manually.
    let user_scrolled = ui.rect_contains_pointer(ui.max_rect()) && ui.input(|i| i.smooth_scroll_delta.y != 0.0);
    let hold_key = id.with("hold");
    let now = ui.input(|i| i.time);
    if user_scrolled {
        ui.data_mut(|d| d.insert_temp(hold_key, now));
    }
    let held = ui.data(|d| d.get_temp::<f64>(hold_key)).is_some_and(|t| now - t < 4.0);

    egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(if big { ui.available_height() * 0.3 } else { 8.0 });
            for (i, line) in lyrics.synced.iter().enumerate() {
                let state = match current {
                    Some(c) if i == c => 0,
                    Some(c) if i < c => -1,
                    _ => 1,
                };
                let color = match state {
                    0 => Color32::WHITE,
                    -1 => theme::with_alpha(TEXT_DIM, if big { 150 } else { 170 }),
                    _ => theme::with_alpha(TEXT_FAINT, if big { 200 } else { 255 }),
                };
                let text = if line.text.trim().is_empty() { "♪" } else { line.text.as_str() };
                let resp = ui.add(
                    egui::Label::new(egui::RichText::new(text).font(theme::bold_font(size)).color(color))
                        .wrap()
                        .sense(Sense::click()),
                );
                if resp.hovered() && state != 0 {
                    ui.painter().text(
                        resp.rect.left_top() - vec2(0.0, 2.0),
                        Align2::LEFT_BOTTOM,
                        theme::fmt_time(line.time_ms as f64 / 1000.0),
                        theme::font(10.0),
                        TEXT_FAINT,
                    );
                }
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    cx.actions.push(Action::Cmd(Command::Seek(line.time_ms as f64 / 1000.0)));
                }
                if state == 0 && changed && !held {
                    resp.scroll_to_me(Some(Align::Center));
                }
                ui.add_space(if big { 14.0 } else { 8.0 });
            }
            provider(ui, &lyrics.provider);
            ui.add_space(if big { ui.available_height() * 0.5 } else { 40.0 });
        });
    if changed && !held {
        ui.data_mut(|d| d.insert_temp(last_key, current));
    }
}

fn provider(ui: &mut Ui, name: &str) {
    if !name.is_empty() {
        ui.add_space(16.0);
        ui.label(egui::RichText::new(format!("Lyrics from {name}")).size(11.0).color(TEXT_FAINT));
    }
}

pub fn empty_state(ui: &mut Ui, glyph: &str, text: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(glyph).size(42.0).color(TEXT_FAINT));
        ui.add_space(8.0);
        ui.label(egui::RichText::new(text).color(TEXT_DIM));
    });
}

fn queue(ui: &mut Ui, cx: &mut Cx) {
    if let Some(t) = cx.player.current.clone() {
        ui.label(egui::RichText::new("Now playing").font(theme::bold_font(13.0)).color(TEXT_DIM));
        ui.add_space(4.0);
        queue_row(ui, cx, &t, None, true);
        ui.add_space(10.0);
    }
    let upcoming = &cx.player.upcoming;
    if upcoming.is_empty() {
        empty_state(ui, icon::QUEUE, "Nothing queued");
        return;
    }
    ui.horizontal(|ui| {
        let label = if cx.player.up_next_len > 0 {
            "Next up".to_string()
        } else {
            format!("Next from: {}", cx.player.context)
        };
        ui.label(egui::RichText::new(label).font(theme::bold_font(13.0)).color(TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add(egui::Label::new(egui::RichText::new("Clear").small().color(TEXT_FAINT)).sense(Sense::click())).clicked() {
                cx.actions.push(Action::Cmd(Command::ClearUpcoming));
            }
        });
    });
    ui.add_space(4.0);
    let n = upcoming.len();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 52.0, n, |ui, range| {
        for i in range {
            let t = cx.player.upcoming[i].clone();
            queue_row(ui, cx, &t, Some(i), false);
        }
    });
}

fn queue_row(ui: &mut Ui, cx: &mut Cx, t: &crate::model::Track, index: Option<usize>, current: bool) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(8), HOVER);
    }
    let art = Rect::from_min_size(rect.min + vec2(4.0, 6.0), vec2(40.0, 40.0));
    let src = t.art.clone().or_else(|| if current { cx.player.via.as_ref().and_then(|v| v.art.clone()) } else { None });
    widgets::cover(ui, cx.art, src.as_deref(), art, 5, widgets::track_fallback(t));
    let x = art.right() + 10.0;
    let w = rect.right() - x - 34.0;
    text_trunc(ui, Pos2::new(x, rect.top() + 8.0), &t.title, theme::font(14.0), if current { cx.accent } else { TEXT }, w);
    text_trunc(ui, Pos2::new(x, rect.top() + 28.0), &t.artist, theme::font(12.0), TEXT_DIM, w);
    if let Some(i) = index {
        let x_rect = Rect::from_center_size(Pos2::new(rect.right() - 16.0, rect.center().y), vec2(24.0, 24.0));
        let xr = ui.interact(x_rect, Id::new(("q-remove", i)), Sense::click());
        if resp.hovered() || xr.hovered() {
            ui.painter().text(x_rect.center(), Align2::CENTER_CENTER, icon::X, FontId::proportional(14.0), if xr.hovered() { TEXT } else { TEXT_DIM });
        }
        if xr.clicked() {
            cx.actions.push(Action::Cmd(Command::RemoveUpcoming(i)));
        } else if resp.double_clicked() || resp.clicked() {
            cx.actions.push(Action::Cmd(Command::JumpTo(i)));
        }
    }
    let track = t.clone();
    resp.context_menu(|ui| widgets::track_menu(ui, cx, &track, None));
}
