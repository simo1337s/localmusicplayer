//! Settings page.

use std::path::PathBuf;

use egui::{Color32, RichText, Ui};
use egui_phosphor::regular as icon;

use super::theme::{self, *};
use super::widgets;
use super::{Action, Cx};
use crate::config::{Config, Paths, SPOTIFY_DEFAULT_CLIENT_ID};
use crate::model::Source;
use crate::service::{AccountStatus, Command};

#[derive(Default)]
pub struct SettingsState {
    new_folder: String,
    import_path: String,
    show_spotify_advanced: bool,
}

fn expand_home(s: &str) -> PathBuf {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new() {
            return home.home_dir().join(rest);
        }
    }
    PathBuf::from(s)
}

fn section(ui: &mut Ui, glyph: &str, color: Color32, title: &str, add: impl FnOnce(&mut Ui)) {
    widgets::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(glyph).size(20.0).color(color));
            ui.label(RichText::new(title).font(theme::bold_font(17.0)));
        });
        ui.add_space(8.0);
        add(ui);
    });
    ui.add_space(14.0);
}

fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(TEXT_FAINT));
}

fn status(ui: &mut Ui, s: &AccountStatus) {
    let (text, color) = match s {
        AccountStatus::Off => ("Not connected".to_string(), TEXT_FAINT),
        AccountStatus::Working(m) => (m.clone(), TEXT_DIM),
        AccountStatus::Connected(name) if name.is_empty() => ("Connected".to_string(), Color32::from_rgb(0x4a, 0xd6, 0x8a)),
        AccountStatus::Connected(name) => (format!("Connected as {name}"), Color32::from_rgb(0x4a, 0xd6, 0x8a)),
        AccountStatus::Error(e) => (e.clone(), DANGER),
    };
    ui.horizontal_wrapped(|ui| {
        if matches!(s, AccountStatus::Working(_)) {
            ui.spinner();
        }
        ui.label(RichText::new(text).color(color));
    });
}

fn text_field(ui: &mut Ui, label: &str, value: &mut String, hint_text: &str, password: bool) {
    ui.label(RichText::new(label).color(TEXT_DIM));
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint_text)
            .password(password)
            .desired_width(f32::INFINITY),
    );
    ui.add_space(4.0);
}

pub fn show(ui: &mut Ui, cx: &mut Cx, cfg: &mut Config, st: &mut SettingsState, paths: &Paths, rss_mb: f32) {
    egui::ScrollArea::vertical().id_salt("settings").auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::new().inner_margin(egui::Margin::same(28)).show(ui, |ui| {
            ui.set_max_width(760.0);
            ui.horizontal(|ui| {
                if widgets::icon_button(ui, icon::CARET_LEFT, 18.0, TEXT, "Back").clicked() {
                    cx.actions.push(Action::Back);
                }
                ui.label(RichText::new("Settings").font(theme::bold_font(30.0)));
            });
            ui.add_space(16.0);

            // ---------------------------------------------------------- library
            section(ui, icon::FOLDER, source_color(Source::Local), "Local library", |ui| {
                let mut remove = None;
                for (i, f) in cfg.library.folders.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon::FOLDER_OPEN).color(TEXT_DIM));
                        ui.label(f.display().to_string());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if widgets::icon_button(ui, icon::X, 13.0, TEXT_DIM, "Remove folder").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    cfg.library.folders.remove(i);
                }
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut st.new_folder)
                            .hint_text("~/Music  (or drop a folder onto the window)")
                            .desired_width(ui.available_width() - 90.0),
                    );
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Add").clicked() || enter) && !st.new_folder.trim().is_empty() {
                        let p = expand_home(&st.new_folder);
                        if !cfg.library.folders.contains(&p) {
                            cfg.library.folders.push(p);
                        }
                        st.new_folder.clear();
                    }
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut cfg.library.scan_on_startup, "Rescan on startup");
                    if ui.button(format!("{}  Rescan now", icon::ARROWS_CLOCKWISE)).clicked() {
                        cx.actions.push(Action::Cmd(Command::Rescan));
                    }
                });
                hint(ui, "Only new or changed files are read again, so rescans are quick.");
            });

            // ---------------------------------------------------------- spotify
            section(ui, icon::SPOTIFY_LOGO, source_color(Source::Spotify), "Spotify", |ui| {
                status(ui, &cx.feed.spotify);
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let logged_in = matches!(cx.feed.spotify, AccountStatus::Connected(_) | AccountStatus::Working(_));
                    if logged_in {
                        if ui.button(format!("{}  Sync playlists", icon::ARROWS_CLOCKWISE)).clicked() {
                            cx.actions.push(Action::Cmd(Command::SyncSpotify));
                        }
                        if ui.button("Log out").clicked() {
                            cx.actions.push(Action::Cmd(Command::SpotifyLogout));
                        }
                    } else if widgets::pill(ui, "Log in with Spotify", source_color(Source::Spotify), Color32::BLACK).clicked() {
                        cx.actions.push(Action::Cmd(Command::SpotifyLogin));
                    }
                });
                hint(ui, "Opens Spotify's login page in your browser. Playback needs Spotify Premium; free accounts can still import playlists.");
                ui.add_space(6.0);
                ui.checkbox(&mut cfg.spotify.enabled, "Enable Spotify");
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Quality").color(TEXT_DIM));
                    egui::ComboBox::from_id_salt("bitrate")
                        .selected_text(format!("{} kbps", cfg.spotify.bitrate))
                        .show_ui(ui, |ui| {
                            for b in [96u16, 160, 320] {
                                ui.selectable_value(&mut cfg.spotify.bitrate, b, format!("{b} kbps"));
                            }
                        });
                    ui.checkbox(&mut cfg.spotify.normalisation, "Normalize volume");
                });
                ui.checkbox(&mut cfg.spotify.cache_audio, "Cache audio on disk (saves bandwidth, up to 2 GB)");
                ui.collapsing("Advanced", |ui| {
                    st.show_spotify_advanced = true;
                    text_field(ui, "Client ID", &mut cfg.spotify.client_id, SPOTIFY_DEFAULT_CLIENT_ID, false);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Redirect port").color(TEXT_DIM));
                        ui.add(egui::DragValue::new(&mut cfg.spotify.redirect_port).range(1024..=65535));
                        if ui.button("Reset").clicked() {
                            cfg.spotify.client_id = SPOTIFY_DEFAULT_CLIENT_ID.into();
                            cfg.spotify.redirect_port = 8898;
                        }
                    });
                    hint(ui, "Use your own developer app (redirect URI http://127.0.0.1:<port>/login) if the shared client gets rate limited.");
                });
            });

            // ---------------------------------------------------------- soundcloud
            section(ui, icon::SOUNDCLOUD_LOGO, source_color(Source::SoundCloud), "SoundCloud", |ui| {
                status(ui, &cx.feed.soundcloud);
                ui.add_space(4.0);
                ui.checkbox(&mut cfg.soundcloud.enabled, "Enable SoundCloud");
                text_field(ui, "Profile URL", &mut cfg.soundcloud.profile_url, "https://soundcloud.com/yourname", false);
                text_field(ui, "OAuth token (optional, for private likes/playlists)", &mut cfg.soundcloud.oauth_token, "2-123456-…", true);
                hint(ui, "Find it in your browser on soundcloud.com: DevTools → Application → Cookies → oauth_token.");
                ui.add_space(4.0);
                if ui.button(format!("{}  Sync likes & playlists", icon::ARROWS_CLOCKWISE)).clicked() {
                    cx.actions.push(Action::Cmd(Command::SyncSoundCloud));
                }
            });

            // ---------------------------------------------------------- apple music
            section(ui, icon::APPLE_LOGO, source_color(Source::AppleMusic), "Apple Music", |ui| {
                hint(
                    ui,
                    "Apple Music streams are DRM-protected and can't play on Linux. Medley imports your library and \
                     playlists, then plays each song from your local files, Spotify or SoundCloud.",
                );
                ui.add_space(6.0);
                ui.label(RichText::new("Import a Library.xml / .m3u file").color(TEXT_DIM));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut st.import_path)
                            .hint_text("~/Downloads/Library.xml")
                            .desired_width(ui.available_width() - 90.0),
                    );
                    if ui.button("Import").clicked() && !st.import_path.trim().is_empty() {
                        let p = expand_home(&st.import_path);
                        let lower = p.to_string_lossy().to_lowercase();
                        if lower.ends_with(".m3u") || lower.ends_with(".m3u8") {
                            cx.actions.push(Action::Cmd(Command::ImportM3u(p)));
                        } else {
                            cx.actions.push(Action::Cmd(Command::ImportAppleXml(p)));
                        }
                        st.import_path.clear();
                    }
                });
                hint(ui, "Music app on macOS / iTunes on Windows: File → Library → Export Library…");
                ui.add_space(8.0);
                ui.collapsing("Import with the Apple Music API", |ui| {
                    text_field(ui, "Media user token", &mut cfg.apple_music.user_token, "media-user-token cookie from music.apple.com", true);
                    text_field(ui, "Developer token (optional)", &mut cfg.apple_music.developer_token, "fetched automatically when empty", true);
                    text_field(ui, "Storefront", &mut cfg.apple_music.storefront, "us", false);
                    if ui.button(format!("{}  Import library & playlists", icon::ARROWS_CLOCKWISE)).clicked() {
                        cx.actions.push(Action::Cmd(Command::ImportAppleApi));
                    }
                });
            });

            // ---------------------------------------------------------- last.fm
            section(ui, icon::LASTFM_LOGO, Color32::from_rgb(0xd5, 0x10, 0x07), "Last.fm scrobbling", |ui| {
                status(ui, &cx.feed.lastfm);
                ui.add_space(4.0);
                ui.checkbox(&mut cfg.lastfm.enabled, "Scrobble what I listen to");
                text_field(ui, "API key", &mut cfg.lastfm.api_key, "from last.fm/api/account/create", false);
                text_field(ui, "API secret", &mut cfg.lastfm.api_secret, "", true);
                ui.horizontal(|ui| {
                    let connected = matches!(cx.feed.lastfm, AccountStatus::Connected(_));
                    if connected {
                        if ui.button("Disconnect").clicked() {
                            cx.actions.push(Action::Cmd(Command::LastfmLogout));
                        }
                    } else if ui.button(format!("{}  Connect account", icon::SIGN_IN)).clicked() {
                        cx.actions.push(Action::Cmd(Command::LastfmLogin));
                    }
                });
                hint(ui, "Tracks scrobble after half their length (or 4 minutes). Offline scrobbles are queued and sent later.");
            });

            // ---------------------------------------------------------- discord
            section(ui, icon::DISCORD_LOGO, Color32::from_rgb(0x58, 0x65, 0xf2), "Discord Rich Presence", |ui| {
                ui.checkbox(&mut cfg.discord.enabled, "Show what I'm listening to on Discord");
                text_field(ui, "Application ID", &mut cfg.discord.app_id, "e.g. 1234567890123456789", false);
                ui.checkbox(&mut cfg.discord.song_as_activity_name, "Show the song title as \"Listening to …\"");
                hint(
                    ui,
                    "Create a free app at discord.com/developers/applications (name it e.g. \"Medley\") and paste its \
                     Application ID. Works with the Discord desktop app, Vesktop and arRPC.",
                );
            });

            // ---------------------------------------------------------- playback
            section(ui, icon::HEADPHONES, cx.accent, "Playback", |ui| {
                ui.checkbox(&mut cfg.playback.gapless, "Gapless playback");
                ui.checkbox(&mut cfg.playback.replaygain, "Use ReplayGain tags (local files)");
                ui.checkbox(&mut cfg.lyrics.enabled, "Show lyrics");
                ui.checkbox(&mut cfg.lyrics.online, "Fetch lyrics from LRCLIB when there are no local lyrics");
                text_field(ui, "mpv binary", &mut cfg.playback.mpv_path, "mpv", false);
                text_field(ui, "Audio device (mpv, optional)", &mut cfg.playback.audio_device, "auto — e.g. pipewire/alsa_output…", false);
                hint(ui, "Local files and SoundCloud play through mpv; Spotify plays through librespot.");
            });

            // ---------------------------------------------------------- appearance
            section(ui, icon::SPARKLE, cx.accent, "Appearance", |ui| {
                ui.checkbox(&mut cfg.ui.dynamic_accent, "Tint the interface with the colours of the current cover");
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Accent colour").color(TEXT_DIM));
                    ui.color_edit_button_srgb(&mut cfg.ui.accent);
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Interface scale").color(TEXT_DIM));
                    let r = ui.add(egui::Slider::new(&mut cfg.ui.scale, 0.75..=1.75).step_by(0.05));
                    if r.drag_stopped() || r.lost_focus() {
                        ui.ctx().set_zoom_factor(cfg.ui.scale);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Covers kept in memory").color(TEXT_DIM));
                    ui.add(egui::DragValue::new(&mut cfg.ui.art_cache_size).range(32..=2000));
                });
                hint(ui, "Fewer cached covers = less RAM. Takes effect after a restart.");
            });

            // ---------------------------------------------------------- about
            section(ui, icon::WAVEFORM, cx.accent, "About", |ui| {
                ui.label(format!("Medley {}", env!("CARGO_PKG_VERSION")));
                ui.label(RichText::new(format!("Memory in use: {rss_mb:.0} MB (mpv runs as a separate process)")).color(TEXT_DIM));
                ui.label(RichText::new(format!("Config: {}", paths.config_file().display())).color(TEXT_FAINT).small());
                ui.label(RichText::new(format!("Data: {}", paths.data_dir.display())).color(TEXT_FAINT).small());
                ui.add_space(4.0);
                hint(ui, "Shortcuts: Space play/pause · ←/→ seek · Ctrl+←/→ prev/next · ↑/↓ volume · Ctrl+F search · L lyrics view");
            });
        });
    });
}
