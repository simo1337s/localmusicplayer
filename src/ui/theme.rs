//! Colours, fonts and egui style.

use std::path::{Path, PathBuf};

use egui::epaint::Shadow;
use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, TextStyle, Visuals};

use crate::model::Source;

// Graphite surfaces with warm off-white text, matching the logo's stacked tiles.
/// Window background (sidebar and player dock sit directly on it).
pub const WINDOW_BG: Color32 = Color32::from_rgb(0x0e, 0x0e, 0x10);
/// Main view and right panel.
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x16, 0x19);
/// Cards, inputs and the player dock.
pub const CARD: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x22);
/// Hovered rows and buttons.
pub const HOVER: Color32 = Color32::from_rgb(0x27, 0x27, 0x2c);
pub const SELECTED: Color32 = Color32::from_rgb(0x30, 0x30, 0x36);
pub const TEXT: Color32 = Color32::from_rgb(0xee, 0xec, 0xe7);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xa4, 0xa3, 0xa9);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x6c, 0x6b, 0x72);
pub const DANGER: Color32 = Color32::from_rgb(0xef, 0x6b, 0x6b);

/// Corner radius of panels and cards ("tiles").
pub const RADIUS: u8 = 14;

pub fn source_color(s: Source) -> Color32 {
    match s {
        Source::Local => Color32::from_rgb(0x8f, 0xa3, 0xc4),
        Source::Spotify => Color32::from_rgb(0x1e, 0xd7, 0x60),
        Source::SoundCloud => Color32::from_rgb(0xff, 0x6a, 0x1a),
        Source::AppleMusic => Color32::from_rgb(0xfa, 0x41, 0x5a),
    }
}

pub fn source_icon(s: Source) -> &'static str {
    use egui_phosphor::regular as i;
    match s {
        Source::Local => i::FOLDER,
        Source::Spotify => i::SPOTIFY_LOGO,
        Source::SoundCloud => i::SOUNDCLOUD_LOGO,
        Source::AppleMusic => i::APPLE_LOGO,
    }
}

pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// Font family for Phosphor icons. Icons need their own family because text fonts such as
/// Inter ship glyphs in the Private Use Area that would shadow the icon codepoints.
pub fn icons() -> FontFamily {
    FontFamily::Name("icons".into())
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, icons())
}

pub fn fill_icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons-fill".into()))
}

/// "icon  label" text for buttons and menus, with the icon in the icon font.
pub fn ic(glyph: &str, text: impl Into<String>) -> egui::WidgetText {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    let icon_fmt = TextFormat {
        font_id: icon_font(15.0),
        color: Color32::PLACEHOLDER,
        valign: egui::Align::Center,
        ..Default::default()
    };
    let text_fmt = TextFormat {
        font_id: font(14.0),
        color: Color32::PLACEHOLDER,
        valign: egui::Align::Center,
        ..Default::default()
    };
    job.append(glyph, 0.0, icon_fmt);
    job.append(&text.into(), 8.0, text_fmt);
    job.into()
}

/// Paints an icon optically centred on `center`: by the glyph's ink, not its line box, so
/// icons of any size line up with each other. Returns the painted ink rect.
pub fn paint_icon(
    painter: &egui::Painter,
    center: egui::Pos2,
    glyph: &str,
    font: FontId,
    color: Color32,
) -> egui::Rect {
    let galley = painter.layout_no_wrap(glyph.to_string(), font, color);
    let ink = galley.mesh_bounds;
    let pos = (center - ink.center().to_vec2()).round();
    painter.galley(pos, galley, color);
    ink.translate(pos.to_vec2())
}

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn bold_font(size: f32) -> FontId {
    FontId::new(size, bold())
}

/// Uploads the bundled logo once at startup.
pub fn load_logo(ctx: &egui::Context) -> egui::TextureHandle {
    let img = image::load_from_memory(include_bytes!("../../assets/icon-64.png"))
        .expect("bundled logo is a valid PNG")
        .to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    ctx.load_texture("multimusic-logo", color, egui::TextureOptions::LINEAR)
}

/// The logo at `size` points, as a widget.
pub fn logo(ui: &mut egui::Ui, texture: egui::TextureId, size: f32) -> egui::Response {
    ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(
        texture,
        egui::vec2(size, size),
    )))
}

/// Soft drop shadow under artwork.
pub fn art_shadow(ui: &egui::Ui, rect: egui::Rect, radius: u8) {
    let shadow = Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(90),
    };
    ui.painter().add(shadow.as_shape(rect, CornerRadius::same(radius)));
}

/// Mixes `a` towards `b` by `t` (0..1).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

pub fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// Readable text colour on top of `bg`.
pub fn on_color(bg: Color32) -> Color32 {
    let lum = 0.299 * bg.r() as f32 + 0.587 * bg.g() as f32 + 0.114 * bg.b() as f32;
    if lum > 150.0 {
        Color32::from_rgb(0x10, 0x10, 0x14)
    } else {
        Color32::WHITE
    }
}

pub fn apply_style(ctx: &egui::Context, accent: Color32) {
    let mut v = Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = PANEL;
    v.extreme_bg_color = CARD;
    v.faint_bg_color = Color32::from_rgb(0x1a, 0x1a, 0x1e);
    v.code_bg_color = CARD;
    v.window_fill = Color32::from_rgb(0x22, 0x22, 0x27);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, Color32::from_rgb(0x33, 0x33, 0x3a));
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    v.hyperlink_color = accent;
    v.selection.bg_fill = with_alpha(accent, 90);
    v.selection.stroke = Stroke::new(1.0, accent);
    v.slider_trailing_fill = true;

    let r = CornerRadius::same(8);
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.weak_bg_fill = PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x2a, 0x2a, 0x30));
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    v.widgets.noninteractive.corner_radius = r;
    // bg_fill: checkbox boxes, slider rails. weak_bg_fill: buttons, combo boxes.
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x41);
    v.widgets.inactive.weak_bg_fill = CARD;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x48, 0x48, 0x50);
    v.widgets.hovered.weak_bg_fill = HOVER;
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    v.widgets.hovered.corner_radius = r;
    v.widgets.hovered.expansion = 0.0;
    v.widgets.active.bg_fill = SELECTED;
    v.widgets.active.weak_bg_fill = SELECTED;
    v.widgets.active.bg_stroke = Stroke::NONE;
    v.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    v.widgets.active.corner_radius = r;
    v.widgets.active.expansion = 0.0;
    v.widgets.open = v.widgets.active;

    ctx.set_visuals(v);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.slider_rail_height = 4.0;
        style.spacing.menu_margin = Margin::same(8);
        style.spacing.window_margin = Margin::same(16);
        style.spacing.scroll = egui::style::ScrollStyle::floating();
        style.spacing.scroll.bar_width = 8.0;
        style.text_styles = [
            (TextStyle::Small, font(12.0)),
            (TextStyle::Body, font(14.0)),
            (TextStyle::Button, font(14.0)),
            (TextStyle::Heading, bold_font(22.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        style.animation_time = 0.12;
    });
}

/// Font files looked up in the system's font folders (and the fonts that come with the Windows
/// and macOS versions). The first family found wins.
const TEXT_FONTS: &[(&str, &[&str])] = &[
    (
        "Inter",
        &[
            "Inter-Regular.ttf",
            "Inter-Regular.otf",
            "InterVariable.ttf",
            "Inter.ttc",
            "Inter[opsz,wght].ttf",
        ],
    ),
    ("Noto Sans", &["NotoSans-Regular.ttf"]),
    ("Cantarell", &["Cantarell-Regular.otf", "Cantarell-VF.otf"]),
    ("DejaVu Sans", &["DejaVuSans.ttf"]),
    // Windows and macOS system fonts.
    ("Segoe UI", &["segoeui.ttf"]),
    (
        "Helvetica Neue",
        &["HelveticaNeue.ttc", "Helvetica.ttc", "Arial.ttf", "arial.ttf"],
    ),
];
const BOLD_FONTS: &[&str] = &[
    "Inter-SemiBold.ttf",
    "Inter-SemiBold.otf",
    "Inter-Bold.ttf",
    "NotoSans-SemiBold.ttf",
    "NotoSans-Bold.ttf",
    "Cantarell-Bold.otf",
    "DejaVuSans-Bold.ttf",
    "seguisb.ttf",
    "segoeuib.ttf",
    "Arial Bold.ttf",
    "arialbd.ttf",
];
/// Fallbacks for symbols in names (☆, ✞, ♡, arrows, dingbats...) that text fonts lack.
const SYMBOL_FONTS: &[&str] = &[
    "NotoSansSymbols2-Regular.ttf",
    "NotoSansSymbols-Regular.ttf",
    "DejaVuSans.ttf",
    "NotoSansMath-Regular.ttf",
    "seguisym.ttf",
    "Apple Symbols.ttf",
];
/// Fallbacks for Japanese/Chinese/Korean titles, smallest first.
const CJK_FONTS: &[&str] = &[
    "NotoSansCJK-Regular.ttc",
    "NotoSansCJKjp-Regular.otf",
    "NotoSansJP-Regular.otf",
    "SourceHanSans-Regular.otc",
    "SourceHanSans-Regular.ttc",
    "DroidSansFallbackFull.ttf",
    "DroidSansFallback.ttf",
    "wqy-microhei.ttc",
    "wqy-zenhei.ttc",
    "ipag.ttf",
    "ipagp.ttf",
    "fonts-japanese-gothic.ttf",
    // Windows
    "YuGothM.ttc",
    "msyh.ttc",
    "meiryo.ttc",
    "msgothic.ttc",
    "malgun.ttf",
    // macOS
    "Hiragino Sans GB.ttc",
    "ヒラギノ角ゴシック W3.ttc",
    "AppleSDGothicNeo.ttc",
    "Arial Unicode.ttf",
];

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Fonts that come with Sumo: next to the program (Windows) or in the app's
    // Resources (macOS).
    if let Some(dir) = crate::tools::exe_dir() {
        dirs.push(dir.join("fonts"));
        dirs.push(dir.join("..").join("Resources").join("fonts"));
    }
    if cfg!(windows) {
        let windir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
        dirs.push(windir.join("Fonts"));
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Microsoft").join("Windows").join("Fonts"));
        }
        return dirs;
    }
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(home) = directories::BaseDirs::new() {
            dirs.push(home.home_dir().join("Library").join("Fonts"));
        }
        return dirs;
    }
    dirs.push(PathBuf::from("/usr/share/fonts"));
    dirs.push(PathBuf::from("/usr/local/share/fonts"));
    if let Some(home) = directories::BaseDirs::new() {
        dirs.push(home.data_dir().join("fonts"));
        dirs.push(home.home_dir().join(".fonts"));
    }
    dirs
}

/// The first of `names` (file names, any case) installed on the system. The font folders
/// are scanned once.
fn find_font(names: &[&str]) -> Option<PathBuf> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static INDEX: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        let mut index = HashMap::new();
        for dir in font_dirs() {
            for entry in walkdir::WalkDir::new(&dir).max_depth(4).into_iter().flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if name.ends_with(".ttf") || name.ends_with(".otf") || name.ends_with(".ttc") || name.ends_with(".otc")
                {
                    index.entry(name).or_insert_with(|| entry.into_path());
                }
            }
        }
        index
    });
    names.iter().find_map(|n| index.get(&n.to_lowercase()).cloned())
}

/// Memory-maps a font file. Only the glyph pages actually used end up in RAM, which
/// matters for multi-megabyte CJK fonts. Maps are kept for the process lifetime.
fn load(path: &Path) -> Option<FontData> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static MAPS: Mutex<Option<HashMap<PathBuf, &'static [u8]>>> = Mutex::new(None);
    let mut maps = MAPS.lock().ok()?;
    let maps = maps.get_or_insert_with(HashMap::new);
    if let Some(bytes) = maps.get(path) {
        return Some(FontData::from_static(bytes));
    }
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: font files are not modified while we run; worst case a glyph renders wrong.
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let bytes: &'static [u8] = Box::leak(Box::new(map));
    maps.insert(path.to_path_buf(), bytes);
    Some(FontData::from_static(bytes))
}

/// Uses a system UI font when one is installed (no font is bundled, which keeps the
/// binary and memory small), plus Phosphor icons and an optional CJK fallback.
pub fn setup_fonts(ctx: &egui::Context, cjk: bool) {
    let mut fonts = FontDefinitions::default();
    let mut regular_name = None;
    for (family, files) in TEXT_FONTS {
        if let Some(data) = find_font(files).and_then(|p| load(&p)) {
            fonts.font_data.insert((*family).into(), data.into());
            regular_name = Some(family.to_string());
            break;
        }
    }
    if let Some(name) = &regular_name {
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, name.clone());
    }
    let mut bold_stack = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(data) = find_font(BOLD_FONTS).and_then(|p| load(&p)) {
        fonts.font_data.insert("bold".into(), data.into());
        bold_stack.insert(0, "bold".into());
    }
    fonts.families.insert(bold(), bold_stack);

    // Icons get dedicated families with Phosphor first (it maps a-z for ligatures, so it
    // can't go first in the text families).
    fonts
        .font_data
        .insert("phosphor".into(), egui_phosphor::Variant::Regular.font_data().into());
    fonts
        .font_data
        .insert("phosphor-fill".into(), egui_phosphor::Variant::Fill.font_data().into());
    let text_stack = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut icon_stack = vec!["phosphor".to_string()];
    icon_stack.extend(text_stack.iter().cloned());
    fonts.families.insert(icons(), icon_stack);
    let mut fill_stack = vec!["phosphor-fill".to_string(), "phosphor".to_string()];
    fill_stack.extend(text_stack.iter().cloned());
    fonts.families.insert(FontFamily::Name("icons-fill".into()), fill_stack);

    // Symbol fallbacks go last, after egui's own emoji fonts.
    for (i, file) in SYMBOL_FONTS.iter().enumerate() {
        if let Some(data) = find_font(&[file]).and_then(|p| load(&p)) {
            let name = format!("symbols-{i}");
            fonts.font_data.insert(name.clone(), data.into());
            for fam in [FontFamily::Proportional, bold()] {
                if let Some(stack) = fonts.families.get_mut(&fam) {
                    stack.push(name.clone());
                }
            }
        }
    }
    if cjk {
        if let Some(data) = find_font(CJK_FONTS).and_then(|p| load(&p)) {
            fonts.font_data.insert("cjk".into(), data.into());
            for fam in [FontFamily::Proportional, bold()] {
                if let Some(stack) = fonts.families.get_mut(&fam) {
                    stack.push("cjk".into());
                }
            }
        }
    }
    ctx.set_fonts(fonts);
}

/// True if any character needs a CJK font.
pub fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af | 0xff00..=0xffef)
    })
}

pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn fmt_total(ms: u64) -> String {
    let mins = ms / 60_000;
    if mins >= 60 {
        format!("{} h {} min", mins / 60, mins % 60)
    } else {
        format!("{mins} min")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(61.4), "1:01");
        assert_eq!(fmt_time(3725.0), "1:02:05");
        assert_eq!(fmt_total(3_900_000), "1 h 5 min");
        assert_eq!(fmt_total(240_000), "4 min");
    }

    #[test]
    fn cjk_detection() {
        assert!(has_cjk("夜に駆ける"));
        assert!(has_cjk("아이유"));
        assert!(!has_cjk("Daft Punk"));
    }
}
