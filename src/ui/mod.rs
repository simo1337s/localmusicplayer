//! The egui front-end.

mod art;
mod panels;
mod settings;
pub mod theme;
mod views;
mod widgets;

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Color32, Key, Modifiers};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{Config, Paths};
use crate::library::Library;
use crate::model::Track;
use crate::service::{Command, Feed, PlayStatus, PlayerView, Shared};

use art::ArtCache;

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Home,
    Search,
    Songs,
    Albums,
    Album(String),
    Playlist(String),
    NowPlaying,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightTab {
    Lyrics,
    Queue,
}

/// Things widgets ask for; applied after the frame is drawn.
#[derive(Debug, Clone)]
pub enum Action {
    Cmd(Command),
    Go(View),
    Back,
    NewPlaylist(Vec<Track>),
    Rename(String, String),
    Delete(String),
    OpenUrl(String),
    RightTab(RightTab),
    ToggleRightPanel,
}

/// Per-frame context handed to every view.
pub struct Cx<'a> {
    pub lib: &'a Library,
    pub player: &'a PlayerView,
    pub feed: &'a Feed,
    pub art: &'a mut ArtCache,
    pub accent: Color32,
    pub actions: &'a mut Vec<Action>,
}

enum Dialog {
    NewPlaylist { name: String, tracks: Vec<Track> },
    Rename { id: String, name: String },
    Delete { id: String, name: String },
}

pub struct App {
    shared: Arc<Shared>,
    cmd: UnboundedSender<Command>,
    pub(crate) cfg: Config,
    sent_cfg: Config,
    cfg_changed_at: Option<Instant>,
    paths: Paths,
    art: ArtCache,
    view: View,
    history: Vec<View>,
    right_tab: RightTab,
    accent: Color32,
    styled_accent: Color32,
    search_text: String,
    search_sent: String,
    search_changed_at: Option<Instant>,
    search_cache: (String, u64, Vec<String>),
    filter_text: String,
    dialog: Option<Dialog>,
    settings: settings::SettingsState,
    cjk_loaded: bool,
    cjk_checked_version: u64,
    focus_search: bool,
    mem_checked: Instant,
    pub(crate) rss_mb: f32,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared: Arc<Shared>,
        cmd: UnboundedSender<Command>,
        cfg: Config,
        paths: Paths,
        rt: tokio::runtime::Handle,
    ) -> App {
        let ctx = cc.egui_ctx.clone();
        let _ = shared.ctx.set(ctx.clone());
        let accent = Color32::from_rgb(cfg.ui.accent[0], cfg.ui.accent[1], cfg.ui.accent[2]);
        theme::setup_fonts(&ctx, false);
        theme::apply_style(&ctx, accent);
        ctx.set_zoom_factor(cfg.ui.scale.clamp(0.6, 2.5));
        let art = ArtCache::new(rt, paths.art_cache(), cfg.ui.art_cache_size);
        App {
            shared,
            cmd,
            sent_cfg: cfg.clone(),
            cfg,
            cfg_changed_at: None,
            paths,
            art,
            view: View::Home,
            history: Vec::new(),
            right_tab: RightTab::Lyrics,
            accent,
            styled_accent: accent,
            search_text: String::new(),
            search_sent: String::new(),
            search_changed_at: None,
            search_cache: (String::new(), 0, Vec::new()),
            filter_text: String::new(),
            dialog: None,
            settings: settings::SettingsState::default(),
            cjk_loaded: false,
            cjk_checked_version: u64::MAX,
            focus_search: false,
            mem_checked: Instant::now() - Duration::from_secs(60),
            rss_mb: 0.0,
        }
    }

    fn send(&self, c: Command) {
        let _ = self.cmd.send(c);
    }

    fn go(&mut self, v: View) {
        if v != self.view {
            self.history.push(std::mem::replace(&mut self.view, v));
            if self.history.len() > 50 {
                self.history.remove(0);
            }
            self.filter_text.clear();
            if self.view == View::Search {
                self.focus_search = true;
            }
        }
    }

    fn apply(&mut self, ctx: &egui::Context, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Cmd(c) => self.send(c),
                Action::Go(v) => self.go(v),
                Action::Back => {
                    if let Some(v) = self.history.pop() {
                        self.view = v;
                    }
                }
                Action::NewPlaylist(tracks) => {
                    self.dialog = Some(Dialog::NewPlaylist {
                        name: String::new(),
                        tracks,
                    })
                }
                Action::Rename(id, name) => self.dialog = Some(Dialog::Rename { id, name }),
                Action::Delete(id) => {
                    let name = self
                        .shared
                        .library
                        .read()
                        .unwrap()
                        .playlist(&id)
                        .map(|p| p.name.clone())
                        .unwrap_or_default();
                    self.dialog = Some(Dialog::Delete { id, name });
                }
                Action::OpenUrl(url) => {
                    let _ = open::that_detached(url);
                }
                Action::RightTab(t) => {
                    if self.cfg.ui.show_right_panel && self.right_tab == t {
                        self.cfg.ui.show_right_panel = false;
                    } else {
                        self.right_tab = t;
                        self.cfg.ui.show_right_panel = true;
                    }
                    self.cfg_changed_at = Some(Instant::now());
                }
                Action::ToggleRightPanel => {
                    self.cfg.ui.show_right_panel = !self.cfg.ui.show_right_panel;
                    self.cfg_changed_at = Some(Instant::now());
                }
            }
        }
        let _ = ctx;
    }

    fn keyboard(&mut self, ctx: &egui::Context, player: &PlayerView) {
        if ctx.text_edit_focused() {
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                ctx.memory_mut(|m| m.stop_text_input());
            }
            return;
        }
        // Clicked buttons/rows keep keyboard focus, which would make Space press them
        // again. Media-player shortcuts win over widget keyboard navigation.
        if let Some(id) = ctx.memory(|m| m.focused()) {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
        let (space, left, right, up, down, ctrl, f, esc, l, q) = ctx.input(|i| {
            (
                i.key_pressed(Key::Space),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.modifiers.matches_logically(Modifiers::COMMAND),
                i.key_pressed(Key::F),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::L),
                i.key_pressed(Key::Q),
            )
        });
        if space {
            self.send(Command::TogglePause);
        }
        if left {
            self.send(if ctrl {
                Command::Previous
            } else {
                Command::SeekRelative(-5.0)
            });
        }
        if right {
            self.send(if ctrl {
                Command::Next
            } else {
                Command::SeekRelative(5.0)
            });
        }
        if up || down {
            let v = player.volume + if up { 5.0 } else { -5.0 };
            self.send(Command::SetVolume(v));
        }
        if ctrl && f {
            self.go(View::Search);
            self.focus_search = true;
        }
        if l {
            if self.view == View::NowPlaying {
                self.apply(ctx, vec![Action::Back]);
            } else {
                self.go(View::NowPlaying);
            }
        }
        if esc && self.view == View::NowPlaying {
            self.apply(ctx, vec![Action::Back]);
        }
        if ctrl && q {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Dropped folders join the library, .m3u files and Library.xml get imported.
    fn dropped_files(&mut self, ctx: &egui::Context) {
        let files: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        for path in files {
            let lower = path.to_string_lossy().to_lowercase();
            if path.is_dir() {
                if !self.cfg.library.folders.contains(&path) {
                    self.cfg.library.folders.push(path);
                    self.cfg_changed_at = Some(Instant::now() - Duration::from_secs(5));
                }
            } else if lower.ends_with(".m3u") || lower.ends_with(".m3u8") {
                self.send(Command::ImportM3u(path));
            } else if lower.ends_with(".xml") {
                self.send(Command::ImportAppleXml(path));
            }
        }
    }

    fn update_accent(&mut self, ctx: &egui::Context, player: &PlayerView) {
        let base = Color32::from_rgb(self.cfg.ui.accent[0], self.cfg.ui.accent[1], self.cfg.ui.accent[2]);
        let target = if self.cfg.ui.dynamic_accent {
            player
                .current
                .as_ref()
                .and_then(|t| {
                    t.art
                        .clone()
                        .or_else(|| player.via.as_ref().and_then(|v| v.art.clone()))
                })
                .and_then(|a| {
                    // Make sure the thumbnail is requested so its colour gets computed.
                    let _ = self.art.get(Some(&a), art::THUMB);
                    self.art.accent(&a)
                })
                .unwrap_or(base)
        } else {
            base
        };
        // Animate the accent change.
        let anim = |id: &str, v: u8| ctx.animate_value_with_time(egui::Id::new(id), v as f32, 0.6).round() as u8;
        self.accent = Color32::from_rgb(
            anim("acc-r", target.r()),
            anim("acc-g", target.g()),
            anim("acc-b", target.b()),
        );
        let d = |a: u8, b: u8| (a as i16 - b as i16).unsigned_abs();
        let s = self.styled_accent;
        if d(s.r(), self.accent.r()) + d(s.g(), self.accent.g()) + d(s.b(), self.accent.b()) > 6
            || (self.accent == target && s != target)
        {
            theme::apply_style(ctx, self.accent);
            self.styled_accent = self.accent;
        }
    }

    fn maybe_load_cjk(&mut self, ctx: &egui::Context, lib: &Library) {
        if self.cjk_loaded || self.cjk_checked_version == lib.version {
            return;
        }
        self.cjk_checked_version = lib.version;
        // Only pay for a CJK font when the library actually needs one.
        let needed = lib
            .tracks
            .values()
            .take(20_000)
            .any(|t| theme::has_cjk(&t.title) || theme::has_cjk(&t.artist) || theme::has_cjk(&t.album))
            || lib.playlists.iter().any(|p| theme::has_cjk(&p.name));
        if needed {
            theme::setup_fonts(ctx, true);
            self.cjk_loaded = true;
        }
    }

    fn sync_config(&mut self) {
        if self.cfg != self.sent_cfg {
            let changed = *self.cfg_changed_at.get_or_insert_with(Instant::now);
            if changed.elapsed() > Duration::from_millis(700) {
                self.send(Command::UpdateConfig(Box::new(self.cfg.clone())));
                self.sent_cfg = self.cfg.clone();
                self.cfg_changed_at = None;
            }
        } else {
            self.cfg_changed_at = None;
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.as_mut() else { return };
        let mut close = false;
        let mut submit: Option<Command> = None;
        let modal = egui::Modal::new(egui::Id::new("dialog")).show(ctx, |ui| {
            ui.set_width(360.0);
            match dialog {
                Dialog::NewPlaylist { name, tracks } => {
                    ui.label(egui::RichText::new("New playlist").font(theme::bold_font(18.0)));
                    ui.add_space(8.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("Playlist name")
                            .desired_width(f32::INFINITY),
                    );
                    r.request_focus();
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (widgets::pill(ui, "Create", self.accent, theme::on_color(self.accent)).clicked() || enter)
                            && !name.trim().is_empty()
                        {
                            submit = Some(Command::CreatePlaylist {
                                name: name.trim().to_string(),
                                tracks: std::mem::take(tracks),
                            });
                        }
                        if widgets::pill(ui, "Cancel", theme::CARD, theme::TEXT).clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Rename { id, name } => {
                    ui.label(egui::RichText::new("Rename playlist").font(theme::bold_font(18.0)));
                    ui.add_space(8.0);
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    r.request_focus();
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (widgets::pill(ui, "Save", self.accent, theme::on_color(self.accent)).clicked() || enter)
                            && !name.trim().is_empty()
                        {
                            submit = Some(Command::RenamePlaylist {
                                playlist_id: id.clone(),
                                name: name.trim().to_string(),
                            });
                        }
                        if widgets::pill(ui, "Cancel", theme::CARD, theme::TEXT).clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Delete { id, name } => {
                    ui.label(egui::RichText::new("Delete playlist?").font(theme::bold_font(18.0)));
                    ui.add_space(6.0);
                    ui.label(format!(
                        "“{name}” will be removed from Medley. Songs stay in your library."
                    ));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if widgets::pill(ui, "Delete", theme::DANGER, Color32::WHITE).clicked() {
                            submit = Some(Command::DeletePlaylist(id.clone()));
                        }
                        if widgets::pill(ui, "Cancel", theme::CARD, theme::TEXT).clicked() {
                            close = true;
                        }
                    });
                }
            }
        });
        if modal.should_close() {
            close = true;
        }
        if let Some(c) = submit {
            if let Command::DeletePlaylist(id) = &c {
                if self.view == View::Playlist(id.clone()) {
                    self.view = View::Home;
                }
            }
            self.send(c);
            close = true;
        }
        if close {
            self.dialog = None;
        }
    }

    fn toasts(&self, ctx: &egui::Context, feed: &Feed) {
        if feed.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -110.0))
            .interactable(false)
            .show(ctx, |ui| {
                for t in feed.toasts.iter().rev().take(3) {
                    let fill = match t.kind {
                        crate::service::ToastKind::Info => Color32::from_rgb(0x2a, 0x2a, 0x36),
                        crate::service::ToastKind::Error => Color32::from_rgb(0x5a, 0x22, 0x2a),
                    };
                    egui::Frame::new()
                        .fill(fill)
                        .corner_radius(egui::CornerRadius::same(10))
                        .inner_margin(egui::Margin::symmetric(16, 10))
                        .shadow(egui::epaint::Shadow {
                            offset: [0, 4],
                            blur: 16,
                            spread: 0,
                            color: Color32::from_black_alpha(120),
                        })
                        .show(ui, |ui| {
                            ui.set_max_width(520.0);
                            ui.label(egui::RichText::new(&t.text).color(theme::TEXT));
                        });
                    ui.add_space(6.0);
                }
            });
    }

    fn check_memory(&mut self) {
        if self.mem_checked.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.mem_checked = Instant::now();
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            if let Some(line) = status.lines().find(|l| l.starts_with("VmRSS:")) {
                let kb: f32 = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                self.rss_mb = kb / 1024.0;
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.art.poll(&ctx);
        self.dropped_files(&ctx);

        let shared = self.shared.clone();
        if shared.feed.read().unwrap().quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        {
            let mut feed = shared.feed.write().unwrap();
            if std::mem::take(&mut feed.raise) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        let player_guard = shared.player.read().unwrap();
        let player: &PlayerView = &player_guard;
        self.update_accent(&ctx, player);
        self.keyboard(&ctx, player);
        let lib_guard = shared.library.read().unwrap();
        let feed_guard = shared.feed.read().unwrap();
        self.maybe_load_cjk(&ctx, &lib_guard);

        let mut actions = Vec::new();
        {
            // Window background.
            ui.painter().rect_filled(ui.max_rect(), 0.0, theme::WINDOW_BG);
            let accent = self.accent;
            let show_right = self.cfg.ui.show_right_panel && self.view != View::NowPlaying;
            let right_tab = self.right_tab;

            let mut cx = Cx {
                lib: &lib_guard,
                player,
                feed: &feed_guard,
                art: &mut self.art,
                accent,
                actions: &mut actions,
            };

            panels::player_bar(ui, &mut cx, show_right.then_some(right_tab));
            panels::sidebar(ui, &mut cx, &self.view);
            if show_right {
                panels::right_panel(ui, &mut cx, right_tab);
            }
            let mut state = views::ViewState {
                view: &self.view,
                can_go_back: !self.history.is_empty(),
                search_text: &mut self.search_text,
                search_cache: &mut self.search_cache,
                filter_text: &mut self.filter_text,
                focus_search: &mut self.focus_search,
            };
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(theme::PANEL)
                        .corner_radius(egui::CornerRadius::same(theme::RADIUS))
                        .outer_margin(egui::Margin {
                            left: 0,
                            right: if show_right { 0 } else { 8 },
                            top: 8,
                            bottom: 0,
                        }),
                )
                .show(ui, |ui| {
                    if self.view == View::Settings {
                        settings::show(ui, &mut cx, &mut self.cfg, &mut self.settings, &self.paths, self.rss_mb);
                    } else {
                        views::show(ui, &mut cx, &mut state);
                    }
                });
        }
        self.toasts(&ctx, &feed_guard);

        // Debounced remote search while typing.
        if self.view == View::Search && self.search_text.trim() != self.search_sent {
            let changed = *self.search_changed_at.get_or_insert_with(Instant::now);
            if changed.elapsed() > Duration::from_millis(450) {
                self.search_sent = self.search_text.trim().to_string();
                self.search_changed_at = None;
                actions.push(Action::Cmd(Command::Search(self.search_sent.clone())));
            } else {
                ctx.request_repaint_after(Duration::from_millis(150));
            }
        }

        // Repaint pacing: only while something moves.
        if player.status == PlayStatus::Playing {
            let fast =
                self.view == View::NowPlaying || (self.cfg.ui.show_right_panel && self.right_tab == RightTab::Lyrics);
            ctx.request_repaint_after(Duration::from_millis(if fast { 100 } else { 500 }));
        } else if player.status == PlayStatus::Loading || feed_guard.scan.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        drop(feed_guard);
        drop(lib_guard);
        drop(player_guard);

        self.apply(&ctx, actions);
        self.dialogs(&ctx);
        self.sync_config();
        self.check_memory();

        if ctx.input(|i| i.viewport().close_requested()) {
            // Persist UI settings right away on exit.
            if self.cfg != self.sent_cfg {
                self.send(Command::UpdateConfig(Box::new(self.cfg.clone())));
                self.sent_cfg = self.cfg.clone();
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::WINDOW_BG.to_normalized_gamma_f32()
    }
}
