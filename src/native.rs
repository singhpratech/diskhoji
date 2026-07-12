//! Standalone native window (egui) — the default way Diskhoji runs.

use crate::scan::{self, Store};
use crate::treemap::{self, DirRect, Rect as TRect, SLOT_DIRAGG, SLOT_SMALL};
use crate::{
    disk_usage, list_roots, open_with_default, reveal_in_file_manager, start_scan, App as Core,
    RootEntry,
};
use eframe::egui::{
    self, Align2, Color32, FontFamily, FontId, Pos2, Rect, RichText, Rounding, Sense, Stroke,
    TextStyle, Vec2,
};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

fn fmt_bytes(n: u64) -> String {
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 || v >= 100.0 {
        format!("{:.0} {}", v, UNITS[u])
    } else if v >= 10.0 {
        format!("{:.1} {}", v, UNITS[u])
    } else {
        format!("{:.2} {}", v, UNITS[u])
    }
}

fn fmt_n(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_pct(p: f64) -> String {
    if p >= 10.0 {
        format!("{:.0}%", p)
    } else {
        format!("{:.1}%", p)
    }
}

fn fmt_time(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} ms", ms)
    }
}

fn hex(c: &str) -> Color32 {
    let b = u32::from_str_radix(&c[1..], 16).unwrap_or(0);
    Color32::from_rgb((b >> 16) as u8, (b >> 8) as u8, b as u8)
}

// ---------- theme ----------

#[derive(Clone, Copy, PartialEq)]
pub enum ThemeKind {
    Dark,
    Light,
}

struct Theme {
    bg: Color32,
    well: Color32,
    panel: Color32,
    panel2: Color32,
    line: Color32,
    ink: Color32,
    ink2: Color32,
    ink3: Color32,
    acc: Color32,
    danger: Color32,
    slots: [Color32; 8],
    other: Color32,
    diragg: Color32,
    small: Color32,
    heat: [Color32; 5],
}

fn theme(kind: ThemeKind) -> Theme {
    match kind {
        ThemeKind::Dark => Theme {
            bg: hex("#0E1116"),
            well: hex("#0A0D12"),
            panel: hex("#141922"),
            panel2: hex("#1B2130"),
            line: hex("#232C3A"),
            ink: hex("#E8ECF4"),
            ink2: hex("#A6B0C3"),
            ink3: hex("#7A879E"),
            acc: hex("#E5B96B"),
            danger: hex("#E66767"),
            slots: [
                hex("#3987e5"),
                hex("#199e70"),
                hex("#c98500"),
                hex("#008300"),
                hex("#9085e9"),
                hex("#e66767"),
                hex("#d55181"),
                hex("#d95926"),
            ],
            other: hex("#566072"),
            diragg: hex("#3E4654"),
            small: hex("#2E3542"),
            heat: [
                hex("#1D2531"),
                hex("#0E4429"),
                hex("#006D32"),
                hex("#26A641"),
                hex("#39D353"),
            ],
        },
        ThemeKind::Light => Theme {
            bg: hex("#F2EEE6"),
            well: hex("#E9E4D8"),
            panel: hex("#FAF7F0"),
            panel2: hex("#F1ECE0"),
            line: hex("#D9D2C3"),
            ink: hex("#1D2532"),
            ink2: hex("#4A566B"),
            ink3: hex("#75808F"),
            acc: hex("#9C7326"),
            danger: hex("#B03A3A"),
            slots: [
                hex("#2F6FBF"),
                hex("#0F7A52"),
                hex("#9A6A00"),
                hex("#1F6B00"),
                hex("#6F63C9"),
                hex("#C24848"),
                hex("#AD3C64"),
                hex("#B0451C"),
            ],
            other: hex("#8A94A6"),
            diragg: hex("#B9C0CC"),
            small: hex("#C9CFD8"),
            heat: [
                hex("#E4DFD2"),
                hex("#9BE9A8"),
                hex("#40C463"),
                hex("#30A14E"),
                hex("#216E39"),
            ],
        },
    }
}

impl Theme {
    fn slot_color(&self, s: u8) -> Color32 {
        match s {
            0..=7 => self.slots[s as usize],
            SLOT_SMALL => self.small,
            SLOT_DIRAGG => self.diragg,
            _ => self.other,
        }
    }
}

fn lighten(c: Color32, f: f32) -> Color32 {
    let l = |v: u8| (v as f32 + (255.0 - v as f32) * f) as u8;
    Color32::from_rgb(l(c.r()), l(c.g()), l(c.b()))
}

fn darken(c: Color32, f: f32) -> Color32 {
    let d = |v: u8| (v as f32 * (1.0 - f)) as u8;
    Color32::from_rgb(d(c.r()), d(c.g()), d(c.b()))
}

// ---------- state ----------

#[derive(Clone, Copy, PartialEq)]
enum View {
    Map,
    Heat,
    Rings,
}

impl View {
    fn code(self) -> u8 {
        match self {
            View::Map => 0,
            View::Heat => 1,
            View::Rings => 2,
        }
    }
}

struct Sector {
    id: u32,
    r0: f32,
    r1: f32,
    a0: f32,
    a1: f32,
    slot: u8,
    d: u8, // 0 file, 1 dir, 2 merged-small
    name: String,
    size: u64,
}

struct ExtRow {
    name: String,
    bytes: u64,
    files: u64,
    slot: u8,
}

struct BigRow {
    id: u32,
    name: String,
    size: u64,
    slot: u8,
}

struct Snap {
    root: String,
    bytes: u64,
    files: u64,
    dirs: u64,
    errors: u64,
    elapsed: u64,
    disk_total: u64,
    disk_free: u64,
    exts: Vec<ExtRow>,
    largest: Vec<BigRow>,
}

struct HeatCell {
    id: u32,
    rect: Rect,
    lvl: u8,
    slot: u8,
    dir: bool,
    name: String,
    size: u64,
}

enum Act {
    Zoom(u32),
    ZoomParent(u32),
    Select(u32),
    AskDelete(u32),
    Reveal(u32),
    Open(u32),
    CopyPath(u32),
    ToggleExt(u8),
}

#[derive(Clone)]
struct DelTarget {
    id: u32,
    gen: u64,
    name: String,
    path: String,
    size: u64,
    dir: bool,
    files: u32,
}

pub struct Native {
    core: Arc<Core>,
    theme_kind: ThemeKind,
    auto_update: bool, // opt-in: check GitHub for a newer version on launch (default off)
    view: View,
    snap: Option<Snap>,
    gen_seen: u64,
    zoom: u32,
    crumbs: Vec<(u32, String)>,
    rects: Vec<TRect>,
    dir_rects: Vec<DirRect>,
    heat_cells: Vec<HeatCell>,
    heat_more: u32,
    sectors: Vec<Sector>,
    ring_label: (String, String),
    cached_mesh: egui::Mesh,
    cached_labels: Vec<(Pos2, String, bool)>,
    picked: Arc<std::sync::Mutex<Option<Option<String>>>>,
    picking: bool,
    ctx_target: Option<(u32, String, u64, bool, u8)>,
    map_key: (u64, u32, u32, u32, u8),
    sel: Option<u32>,
    ext_sel: Option<u8>,
    delete_target: Option<DelTarget>,
    pending_delete: Option<(Arc<std::sync::Mutex<Option<std::io::Result<()>>>>, DelTarget)>,
    deleted_since_scan: bool,
    toast: Option<(String, Instant, bool)>,
    pending_copy: Option<String>,
    path_input: String,
    roots: Vec<RootEntry>,
    scan_started: Option<Instant>,
    prefs_path: PathBuf,
    logo: Option<egui::TextureHandle>,
    show_landing: bool,
    show_about: bool,
    update: Arc<std::sync::Mutex<UpdateState>>,
}

#[cfg(target_os = "linux")]
fn pick_folder_blocking() -> Option<String> {
    for (cmd, args) in [
        ("zenity", &["--file-selection", "--directory"][..]),
        ("kdialog", &["--getexistingdirectory", "."][..]),
    ] {
        match std::process::Command::new(cmd).args(args).output() {
            Ok(out) if out.status.success() => {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
            Ok(_) => return None, // dialog shown, user cancelled
            Err(_) => continue,   // tool not installed, try the next
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn pick_folder_blocking() -> Option<String> {
    let out = std::process::Command::new("osascript")
        .args(["-e", "POSIX path of (choose folder)"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(windows)]
fn pick_folder_blocking() -> Option<String> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms; \
             $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
             if ($d.ShowDialog() -eq 'OK') { $d.SelectedPath }",
        ])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

// ---------- update checker (GitHub releases, no extra deps: shells to curl) ----------

#[derive(Clone)]
enum UpdateState {
    Idle, // default: no check has run — the app has made no network call
    Checking,
    UpToDate,
    Available { ver: String, url: String },
    Failed,
}

fn parse_ver(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split(|c: char| c == '.' || c == '-' || c == '+');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next().unwrap_or("0").parse().unwrap_or(0);
    Some((a, b, c))
}

fn fetch_latest_release() -> Option<(String, String)> {
    let out = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "8",
            "-H",
            "Accept: application/vnd.github+json",
            "https://api.github.com/repos/singhpratech/diskhoji/releases/latest",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let url = v
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("https://github.com/singhpratech/diskhoji/releases")
        .to_string();
    Some((tag, url))
}

fn spawn_update_check(slot: Arc<std::sync::Mutex<UpdateState>>) {
    if let Ok(mut g) = slot.lock() {
        *g = UpdateState::Checking;
    }
    std::thread::spawn(move || {
        let cur = parse_ver(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0));
        let st = match fetch_latest_release() {
            Some((tag, url)) => match parse_ver(&tag) {
                Some(latest) if latest > cur => UpdateState::Available { ver: tag, url },
                Some(_) => UpdateState::UpToDate,
                None => UpdateState::Failed,
            },
            None => UpdateState::Failed,
        };
        if let Ok(mut g) = slot.lock() {
            *g = st;
        }
    });
}

fn dirs_config() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(x);
    }
    #[cfg(windows)]
    if let Ok(a) = std::env::var("APPDATA") {
        return PathBuf::from(a);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
}

fn apply_style(ctx: &egui::Context, kind: ThemeKind) {
    let t = theme(kind);
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, FontId::new(26.0, FontFamily::Monospace)),
        (TextStyle::Body, FontId::new(16.5, FontFamily::Monospace)),
        (TextStyle::Monospace, FontId::new(16.5, FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(16.0, FontFamily::Monospace)),
        (TextStyle::Small, FontId::new(13.5, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(14.0, 7.0);
    style.spacing.scroll.bar_width = 10.0;

    let mut v = match kind {
        ThemeKind::Dark => egui::Visuals::dark(),
        ThemeKind::Light => egui::Visuals::light(),
    };
    v.panel_fill = t.panel;
    v.window_fill = t.panel2;
    v.extreme_bg_color = t.well;
    v.faint_bg_color = t.panel2;
    v.override_text_color = Some(t.ink);
    v.selection.bg_fill = t.acc.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, t.acc);
    v.hyperlink_color = t.acc;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, t.line);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.ink2);
    v.widgets.inactive.bg_fill = t.panel2;
    v.widgets.inactive.weak_bg_fill = t.panel2;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, t.line);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, t.ink2);
    v.widgets.hovered.bg_fill = lighten(t.panel2, 0.06);
    v.widgets.hovered.weak_bg_fill = lighten(t.panel2, 0.06);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, t.acc.gamma_multiply(0.7));
    v.widgets.hovered.fg_stroke = Stroke::new(1.2, t.ink);
    v.widgets.active.bg_fill = t.acc.gamma_multiply(0.3);
    v.widgets.active.fg_stroke = Stroke::new(1.2, t.ink);
    v.window_stroke = Stroke::new(1.0, t.line);
    style.visuals = v;
    ctx.set_style(style);
}

impl Native {
    fn new(cc: &eframe::CreationContext<'_>, core: Arc<Core>) -> Self {
        let prefs_path = dirs_config().join("diskhoji-prefs");
        let theme_kind = match std::fs::read_to_string(&prefs_path) {
            Ok(s) if s.trim() == "light" => ThemeKind::Light,
            _ => ThemeKind::Dark,
        };
        // Update checks are OPT-IN. With no opt-in file, the app never touches the network.
        let auto_update = std::fs::read_to_string(dirs_config().join("diskhoji-updatecheck"))
            .map(|s| s.trim() == "on")
            .unwrap_or(false);
        let roots = list_roots();
        let path_input = roots
            .first()
            .map(|r| r.path.clone())
            .unwrap_or_else(|| "/".into());
        apply_style(&cc.egui_ctx, theme_kind);
        let logo = image::load_from_memory(include_bytes!("../assets/icon-64.png"))
            .ok()
            .map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let ci = egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    rgba.as_raw(),
                );
                cc.egui_ctx
                    .load_texture("logo", ci, egui::TextureOptions::LINEAR)
            });
        Self {
            core,
            theme_kind,
            view: View::Map,
            snap: None,
            gen_seen: 0,
            zoom: 0,
            crumbs: Vec::new(),
            rects: Vec::new(),
            dir_rects: Vec::new(),
            heat_cells: Vec::new(),
            heat_more: 0,
            sectors: Vec::new(),
            ring_label: (String::new(), String::new()),
            cached_mesh: egui::Mesh::default(),
            cached_labels: Vec::new(),
            picked: Arc::new(std::sync::Mutex::new(None)),
            picking: false,
            ctx_target: None,
            map_key: (u64::MAX, 0, 0, 0, 0),
            sel: None,
            ext_sel: None,
            delete_target: None,
            pending_delete: None,
            deleted_since_scan: false,
            toast: None,
            pending_copy: None,
            path_input,
            roots,
            scan_started: None,
            prefs_path,
            logo,
            show_landing: false,
            show_about: false,
            auto_update,
            update: {
                let s = Arc::new(std::sync::Mutex::new(UpdateState::Idle));
                if auto_update {
                    spawn_update_check(s.clone()); // only when the user has opted in
                }
                s
            },
        }
    }

    fn about_window(&mut self, ctx: &egui::Context, t: &Theme) {
        if !self.show_about {
            return;
        }
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("about_dim"))
            .fixed_pos(screen.min)
            .order(egui::Order::Middle)
            .show(ctx, |ui| {
                ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha(150));
            });

        let mut open = true;
        let mut recheck = false;
        let update = self.update.lock().map(|g| g.clone()).unwrap_or(UpdateState::Failed);
        let green = hex("#199e70");
        egui::Window::new("")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                egui::Frame::default()
                    .fill(t.panel2)
                    .stroke(Stroke::new(1.0, t.line))
                    .rounding(12.0)
                    .inner_margin(0.0),
            )
            .show(ctx, |ui| {
                ui.set_width(440.0);
                // hero band
                egui::Frame::default()
                    .fill(t.well)
                    .rounding(egui::Rounding { nw: 12.0, ne: 12.0, sw: 0.0, se: 0.0 })
                    .inner_margin(egui::Margin::symmetric(24.0, 22.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if let Some(tex) = &self.logo {
                                ui.add(
                                    egui::Image::new(tex)
                                        .fit_to_exact_size(Vec2::splat(64.0))
                                        .rounding(14.0),
                                );
                            }
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new("Diskhoji").size(28.0).strong().color(t.acc),
                                );
                                ui.label(
                                    RichText::new(concat!("version ", env!("CARGO_PKG_VERSION")))
                                        .size(13.5)
                                        .color(t.ink3),
                                );
                                ui.label(
                                    RichText::new("every byte, accounted for.")
                                        .size(13.5)
                                        .color(t.ink2),
                                );
                            });
                        });
                    });

                ui.add_space(2.0);
                egui::Frame::default()
                    .inner_margin(egui::Margin::symmetric(24.0, 18.0))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(
                                "A fast, native disk-space analyzer — cushion treemap, heatmap \
                                 grid, and radial rings in one small Rust binary. Linux · macOS · \
                                 Windows.",
                            )
                            .size(13.5)
                            .color(t.ink2),
                        );
                        ui.add_space(16.0);

                        // ----- update status band (Notepad++-style) -----
                        egui::Frame::default()
                            .fill(t.well)
                            .stroke(Stroke::new(1.0, t.line))
                            .rounding(8.0)
                            .inner_margin(12.0)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                match &update {
                                    UpdateState::Idle => {
                                        ui.label(
                                            RichText::new(
                                                "Diskhoji doesn't check on its own. Ask it to \
                                                 look — only your version number is compared \
                                                 against GitHub, nothing about you is sent.",
                                            )
                                            .size(12.5)
                                            .color(t.ink3),
                                        );
                                        ui.add_space(8.0);
                                        let dark_ink = if self.theme_kind == ThemeKind::Dark {
                                            hex("#161006")
                                        } else {
                                            hex("#FFFFFF")
                                        };
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new("Check for updates")
                                                        .strong()
                                                        .color(dark_ink),
                                                )
                                                .fill(t.acc),
                                            )
                                            .clicked()
                                        {
                                            recheck = true;
                                        }
                                    }
                                    UpdateState::Checking => {
                                        ui.label(
                                            RichText::new("Checking for updates…")
                                                .size(13.5)
                                                .color(t.ink2),
                                        );
                                    }
                                    UpdateState::UpToDate => {
                                        ui.label(
                                            RichText::new("You're on the latest version")
                                                .size(13.5)
                                                .color(green),
                                        );
                                    }
                                    UpdateState::Available { ver, url } => {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!("Update available — {}", ver))
                                                    .size(14.0)
                                                    .strong()
                                                    .color(t.acc),
                                            );
                                        });
                                        ui.label(
                                            RichText::new(
                                                "Close Diskhoji, download the new build, and \
                                                 reinstall (your scans aren't affected).",
                                            )
                                            .size(12.5)
                                            .color(t.ink3),
                                        );
                                        ui.add_space(8.0);
                                        ui.horizontal(|ui| {
                                            let dark_ink = if self.theme_kind == ThemeKind::Dark {
                                                hex("#161006")
                                            } else {
                                                hex("#FFFFFF")
                                            };
                                            if ui
                                                .add(
                                                    egui::Button::new(
                                                        RichText::new("Download update")
                                                            .strong()
                                                            .color(dark_ink),
                                                    )
                                                    .fill(t.acc),
                                                )
                                                .clicked()
                                            {
                                                open_with_default(url);
                                            }
                                            if ui.button("Release notes").clicked() {
                                                open_with_default(url);
                                            }
                                        });
                                    }
                                    UpdateState::Failed => {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new("Couldn't reach GitHub")
                                                    .size(13.5)
                                                    .color(t.ink3),
                                            );
                                            if ui.button("Retry").clicked() {
                                                recheck = true;
                                            }
                                        });
                                    }
                                }
                                if !matches!(update, UpdateState::Checking | UpdateState::Idle) {
                                    ui.add_space(6.0);
                                    if ui
                                        .link(RichText::new("Check again").size(12.5).color(t.ink3))
                                        .clicked()
                                    {
                                        recheck = true;
                                    }
                                }
                                ui.add_space(8.0);
                                ui.separator();
                                ui.add_space(6.0);
                                let mut auto = self.auto_update;
                                if ui
                                    .checkbox(
                                        &mut auto,
                                        RichText::new("Check for updates when Diskhoji starts")
                                            .size(12.5)
                                            .color(t.ink2),
                                    )
                                    .changed()
                                {
                                    self.auto_update = auto;
                                    let _ = std::fs::create_dir_all(dirs_config());
                                    let _ = std::fs::write(
                                        dirs_config().join("diskhoji-updatecheck"),
                                        if auto { "on" } else { "off" },
                                    );
                                    if auto {
                                        recheck = true; // check now, since they just turned it on
                                    }
                                }
                                ui.label(
                                    RichText::new(
                                        "Off by default — with this unchecked, Diskhoji makes \
                                         no network connection at all.",
                                    )
                                    .size(11.5)
                                    .color(t.ink3),
                                );
                            });

                        ui.add_space(16.0);
                        // ----- credits -----
                        egui::Grid::new("about_grid")
                            .num_columns(2)
                            .spacing([16.0, 8.0])
                            .show(ui, |ui| {
                                ui.label(RichText::new("CREATED BY").color(t.ink3).size(12.0));
                                ui.hyperlink_to(
                                    RichText::new("Prateek Singh").size(14.0).strong(),
                                    "https://theaivibe.org/about",
                                );
                                ui.end_row();
                                ui.label(RichText::new("WEBSITE").color(t.ink3).size(12.0));
                                ui.hyperlink_to("diskhoji.org", "https://diskhoji.org");
                                ui.end_row();
                                ui.label(RichText::new("SOURCE").color(t.ink3).size(12.0));
                                ui.hyperlink_to(
                                    "github.com/singhpratech/diskhoji",
                                    "https://github.com/singhpratech/diskhoji",
                                );
                                ui.end_row();
                                ui.label(RichText::new("LICENSE").color(t.ink3).size(12.0));
                                ui.label(RichText::new("MIT").size(14.0).color(t.ink2));
                                ui.end_row();
                            });

                        ui.add_space(18.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Close").clicked() {
                                self.show_about = false;
                            }
                        });
                    });
            });
        if !open {
            self.show_about = false;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_about = false;
        }
        if recheck {
            spawn_update_check(self.update.clone());
        }
    }

    fn toast(&mut self, msg: impl Into<String>, bad: bool) {
        self.toast = Some((msg.into(), Instant::now(), bad));
    }

    fn invalidate_map(&mut self) {
        self.map_key = (u64::MAX, 0, 0, 0, 0);
    }

    fn snapshot(&mut self) {
        let core = self.core.clone();
        let guard = core.store.read().unwrap();
        let Some(s) = guard.as_ref() else { return };
        let (disk_total, disk_free) = disk_usage(&s.root_path);
        let mut ext_ids: Vec<usize> =
            (0..s.exts.len()).filter(|e| s.exts[*e].bytes > 0).collect();
        ext_ids.sort_unstable_by(|a, b| s.exts[*b].bytes.cmp(&s.exts[*a].bytes));
        ext_ids.truncate(14);
        let exts = ext_ids
            .iter()
            .map(|e| ExtRow {
                name: s.exts[*e].name.clone(),
                bytes: s.exts[*e].bytes,
                files: s.exts[*e].files,
                slot: s.exts[*e].slot,
            })
            .collect();
        let largest = s
            .largest
            .iter()
            .filter(|id| s.nodes[**id as usize].alive)
            .map(|id| {
                let n = &s.nodes[*id as usize];
                BigRow {
                    id: *id,
                    name: n.name.to_string(),
                    size: n.size,
                    slot: s.exts[n.ext as usize].slot,
                }
            })
            .collect();
        self.snap = Some(Snap {
            root: s.root_path.clone(),
            bytes: s.nodes[0].size,
            files: s.nodes[0].files as u64,
            dirs: s.dirs,
            errors: s.errors,
            elapsed: s.elapsed_ms,
            disk_total,
            disk_free,
            exts,
            largest,
        });
        self.gen_seen = s.generation;
    }

    fn reset_view_state(&mut self, root: String) {
        self.zoom = 0;
        self.sel = None;
        self.ext_sel = None;
        // node ids are arena indices reused across scans — anything holding
        // an id from the old generation must die with it
        self.delete_target = None;
        self.ctx_target = None;
        self.crumbs = vec![(0, root)];
        self.invalidate_map();
    }

    fn rebuild_crumbs(&mut self, store: &Store) {
        let mut c: Vec<(u32, String)> = scan::ancestors_of(store, self.zoom)
            .into_iter()
            .map(|a| {
                let name = if a == 0 {
                    store.root_path.clone()
                } else {
                    store.nodes[a as usize].name.to_string()
                };
                (a, name)
            })
            .collect();
        let zname = if self.zoom == 0 {
            store.root_path.clone()
        } else {
            store.nodes[self.zoom as usize].name.to_string()
        };
        c.push((self.zoom, zname));
        self.crumbs = c;
    }

    fn relayout(&mut self, store: &Store, w: f32, h: f32) {
        let key = (
            store.generation,
            self.zoom,
            w as u32,
            h as u32,
            self.view.code(),
        );
        if key == self.map_key {
            return;
        }
        self.map_key = key;
        match self.view {
            View::Map => {
                let (r, d) = treemap::layout(store, self.zoom, w as f64, h as f64);
                self.rects = r;
                self.dir_rects = d;
                self.build_map_cache();
            }
            View::Heat => self.build_heat(store, w, h),
            View::Rings => self.build_rings(store, w, h),
        }
    }

    /// The cushion mesh is expensive (40k rects → 160k vertices) — build it
    /// once per (generation, zoom, size, view, theme) and clone per frame.
    fn build_map_cache(&mut self) {
        let t = theme(self.theme_kind);
        let mut mesh = egui::Mesh::default();
        for r in &self.rects {
            let (x, y, w, h) = (r.x, r.y, r.w.max(0.6), r.h.max(0.6));
            let c = t.slot_color(r.s);
            let tl = lighten(c, 0.20);
            let br = darken(c, 0.26);
            let i0 = mesh.vertices.len() as u32;
            mesh.colored_vertex(Pos2::new(x, y), tl);
            mesh.colored_vertex(Pos2::new(x + w, y), c);
            mesh.colored_vertex(Pos2::new(x + w, y + h), br);
            mesh.colored_vertex(Pos2::new(x, y + h), c);
            mesh.add_triangle(i0, i0 + 1, i0 + 2);
            mesh.add_triangle(i0, i0 + 2, i0 + 3);
        }
        self.cached_mesh = mesh;

        self.cached_labels.clear();
        let mut placed: Vec<Rect> = Vec::new();
        let mut fits = |est: Rect| {
            if placed.iter().any(|b| b.intersects(est)) {
                false
            } else {
                placed.push(est);
                true
            }
        };
        for d in &self.dir_rects {
            if d.w < 110.0 || d.h < 30.0 {
                continue;
            }
            let text = truncate_to(&format!("{}/", d.n), d.w - 14.0, 13.0);
            let pos = Pos2::new(d.x + 6.0, d.y + 5.0);
            let est = Rect::from_min_size(pos, Vec2::new(text.len() as f32 * 8.2, 17.0));
            if fits(est) {
                self.cached_labels.push((pos, text, true));
            }
        }
        for r in &self.rects {
            if r.d != 0 || r.w < 130.0 || r.h < 40.0 {
                continue;
            }
            let text = truncate_to(&r.n, r.w - 14.0, 13.0);
            let pos = Pos2::new(r.x + 6.0, r.y + r.h - 22.0);
            let est = Rect::from_min_size(pos, Vec2::new(text.len() as f32 * 8.2, 17.0));
            if fits(est) {
                self.cached_labels.push((pos, text, false));
            }
        }
    }

    fn build_rings(&mut self, store: &Store, w: f32, h: f32) {
        self.sectors.clear();
        let max_r = (w.min(h) / 2.0 - 24.0).max(60.0);
        let hole = max_r * 0.22;
        const DEPTH: usize = 5;
        let thick = (max_r - hole) / DEPTH as f32;
        const TAU: f32 = std::f32::consts::TAU;

        fn rec(
            store: &Store,
            sectors: &mut Vec<Sector>,
            id: u32,
            depth: usize,
            a0: f32,
            a1: f32,
            hole: f32,
            thick: f32,
        ) {
            if depth >= 5 {
                return;
            }
            let n = &store.nodes[id as usize];
            let mut kids: Vec<u32> = (n.first_child..n.first_child + n.child_count)
                .filter(|c| store.nodes[*c as usize].alive && store.nodes[*c as usize].size > 0)
                .collect();
            if kids.is_empty() {
                return;
            }
            kids.sort_unstable_by(|a, b| {
                store.nodes[*b as usize].size.cmp(&store.nodes[*a as usize].size)
            });
            let total = n.size.max(1) as f64;
            let r0 = hole + depth as f32 * thick;
            let r1 = r0 + thick - 2.0;
            let span_all = a1 - a0;
            const MIN_SPAN: f32 = 0.008;
            let mut a = a0;
            for (ix, c) in kids.iter().enumerate() {
                let k = &store.nodes[*c as usize];
                let span = (k.size as f64 / total) as f32 * span_all;
                if span < MIN_SPAN {
                    let rest: u64 =
                        kids[ix..].iter().map(|c| store.nodes[*c as usize].size).sum();
                    let rspan = (rest as f64 / total) as f32 * span_all;
                    if rspan > 0.002 {
                        sectors.push(Sector {
                            id,
                            r0,
                            r1,
                            a0: a,
                            a1: a + rspan,
                            slot: SLOT_SMALL,
                            d: 2,
                            name: format!("{} small items", kids.len() - ix),
                            size: rest,
                        });
                    }
                    break;
                }
                let slot = if k.is_dir {
                    SLOT_DIRAGG
                } else {
                    store.exts[k.ext as usize].slot
                };
                sectors.push(Sector {
                    id: *c,
                    r0,
                    r1,
                    a0: a,
                    a1: a + span,
                    slot,
                    d: k.is_dir as u8,
                    name: k.name.to_string(),
                    size: k.size,
                });
                if k.is_dir && span > 0.02 {
                    rec(store, sectors, *c, depth + 1, a, a + span, hole, thick);
                }
                a += span;
            }
        }
        rec(store, &mut self.sectors, self.zoom, 0, 0.0, TAU, hole, thick);
        let zn = &store.nodes[self.zoom as usize];
        let zname = if self.zoom == 0 {
            store.root_path.clone()
        } else {
            zn.name.to_string()
        };
        self.ring_label = (zname, fmt_bytes(zn.size));
    }

    fn build_heat(&mut self, store: &Store, w: f32, h: f32) {
        self.heat_cells.clear();
        let n = &store.nodes[self.zoom as usize];
        let mut kids: Vec<u32> = (n.first_child..n.first_child + n.child_count)
            .filter(|c| store.nodes[*c as usize].alive && store.nodes[*c as usize].size > 0)
            .collect();
        kids.sort_unstable_by(|a, b| {
            store.nodes[*b as usize].size.cmp(&store.nodes[*a as usize].size)
        });
        self.heat_more = kids.len().saturating_sub(2000) as u32;
        kids.truncate(2000);
        if kids.is_empty() {
            return;
        }
        let pad = 20.0_f32;
        let leg = 40.0_f32;
        let aw = (w - pad * 2.0).max(40.0);
        let ah = (h - pad * 2.0 - leg).max(40.0);
        let mut pitch = ((aw * ah) / kids.len() as f32).sqrt().floor().clamp(10.0, 46.0);
        while pitch > 10.0
            && (kids.len() as f32 / (aw / pitch).floor().max(1.0)).ceil() * pitch > ah
        {
            pitch -= 1.0;
        }
        let cols = (aw / pitch).floor().max(1.0) as usize;
        let gap = (pitch * 0.18).max(2.0);
        let max_lg = (1.0 + store.nodes[kids[0] as usize].size as f64).log2();
        let min_lg = (1.0 + store.nodes[*kids.last().unwrap() as usize].size as f64).log2();
        for (ix, id) in kids.iter().enumerate() {
            let k = &store.nodes[*id as usize];
            let lg = (1.0 + k.size as f64).log2();
            let lvl = if max_lg > min_lg {
                ((lg - min_lg) / (max_lg - min_lg) * 4.999).floor().min(4.0) as u8
            } else {
                4
            };
            let x = pad + (ix % cols) as f32 * pitch;
            let y = pad + (ix / cols) as f32 * pitch;
            self.heat_cells.push(HeatCell {
                id: *id,
                rect: Rect::from_min_size(Pos2::new(x, y), Vec2::splat(pitch - gap)),
                lvl,
                slot: if k.is_dir {
                    SLOT_DIRAGG
                } else {
                    store.exts[k.ext as usize].slot
                },
                dir: k.is_dir,
                name: k.name.to_string(),
                size: k.size,
            });
        }
    }

    fn begin_scan(&mut self, path: String) {
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            self.toast(format!("not a directory: {}", path), true);
            return;
        }
        self.path_input = path;
        self.scan_started = Some(Instant::now());
        start_scan(self.core.clone(), p);
    }

    fn path_for(&self, id: u32) -> Option<String> {
        let core = self.core.clone();
        let guard = core.store.read().unwrap();
        guard.as_ref().and_then(|s| {
            if (id as usize) < s.nodes.len() {
                Some(scan::path_of(s, id))
            } else {
                None
            }
        })
    }

    fn apply(&mut self, a: Act) {
        match a {
            Act::Zoom(id) => {
                // zooming into a leaf file gives a dead view — go to its parent
                let core = self.core.clone();
                let guard = core.store.read().unwrap();
                let target = guard
                    .as_ref()
                    .map(|s| {
                        if (id as usize) < s.nodes.len() && !s.nodes[id as usize].is_dir {
                            s.nodes[id as usize].parent
                        } else {
                            id
                        }
                    })
                    .unwrap_or(id);
                drop(guard);
                self.zoom = target;
                self.invalidate_map();
                self.crumbs.clear();
            }
            Act::ZoomParent(id) => {
                let core = self.core.clone();
                let guard = core.store.read().unwrap();
                let parent = guard
                    .as_ref()
                    .filter(|s| (id as usize) < s.nodes.len())
                    .map(|s| s.nodes[id as usize].parent)
                    .unwrap_or(0);
                drop(guard);
                self.zoom = parent;
                self.invalidate_map();
                self.crumbs.clear();
            }
            Act::Select(id) => self.sel = Some(id),
            Act::ToggleExt(s) => {
                self.ext_sel = if self.ext_sel == Some(s) { None } else { Some(s) };
            }
            Act::AskDelete(id) => {
                let core = self.core.clone();
                let guard = core.store.read().unwrap();
                if let Some(s) = guard.as_ref() {
                    if (id as usize) < s.nodes.len() && s.nodes[id as usize].alive && id != 0 {
                        let n = &s.nodes[id as usize];
                        self.delete_target = Some(DelTarget {
                            id,
                            gen: s.generation,
                            name: n.name.to_string(),
                            path: scan::path_of(s, id),
                            size: n.size,
                            dir: n.is_dir,
                            files: n.files,
                        });
                    }
                }
            }
            Act::Reveal(id) => {
                if let Some(p) = self.path_for(id) {
                    reveal_in_file_manager(&p);
                }
            }
            Act::Open(id) => {
                if let Some(p) = self.path_for(id) {
                    open_with_default(&p);
                }
            }
            Act::CopyPath(id) => {
                self.pending_copy = self.path_for(id);
            }
        }
    }

    /// Validate against the generation captured when the modal opened, then
    /// run the filesystem removal on a worker thread — never on the UI thread,
    /// never while holding the store lock.
    fn do_delete(&mut self, tgt: DelTarget) {
        {
            let core = self.core.clone();
            let guard = core.store.read().unwrap();
            let Some(s) = guard.as_ref() else { return };
            if s.generation != tgt.gen {
                self.toast("scan changed underneath — delete cancelled", true);
                return;
            }
            if tgt.id == 0
                || tgt.id as usize >= s.nodes.len()
                || !s.nodes[tgt.id as usize].alive
                || scan::path_of(s, tgt.id) != tgt.path
            {
                self.toast("refusing: delete target no longer valid", true);
                return;
            }
        }
        let slot: Arc<std::sync::Mutex<Option<std::io::Result<()>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let worker = slot.clone();
        let path = tgt.path.clone();
        let is_dir = tgt.dir;
        std::thread::spawn(move || {
            // re-stat right before acting: refuse if the path changed kind
            let result = match std::fs::symlink_metadata(&path) {
                Err(e) => Err(e),
                Ok(md) if is_dir && md.is_dir() => std::fs::remove_dir_all(&path),
                Ok(md) if !is_dir && !md.is_dir() => std::fs::remove_file(&path),
                Ok(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "path changed type on disk — refusing",
                )),
            };
            if let Ok(mut g) = worker.lock() {
                *g = Some(result);
            }
        });
        self.pending_delete = Some((slot, tgt));
    }

    fn poll_pending_delete(&mut self, ctx: &egui::Context) {
        let Some((slot, tgt)) = &self.pending_delete else { return };
        ctx.request_repaint_after(std::time::Duration::from_millis(150));
        let done = slot.lock().ok().and_then(|mut g| g.take());
        let Some(result) = done else { return };
        let tgt = tgt.clone();
        self.pending_delete = None;
        match result {
            Err(e) => self.toast(format!("delete failed: {}", e), true),
            Ok(()) => {
                let core = self.core.clone();
                let mut guard = core.store.write().unwrap();
                if let Some(s) = guard.as_mut() {
                    // the fs object is gone; only fix up the model if it still
                    // describes the generation we deleted from
                    if s.generation == tgt.gen
                        && (tgt.id as usize) < s.nodes.len()
                        && s.nodes[tgt.id as usize].alive
                    {
                        let (freed, _files) = scan::remove_subtree(s, tgt.id);
                        while self.zoom != 0 && !s.nodes[self.zoom as usize].alive {
                            self.zoom = s.nodes[self.zoom as usize].parent;
                        }
                        drop(guard);
                        self.sel = None;
                        self.deleted_since_scan = true;
                        self.snapshot();
                        self.invalidate_map();
                        self.crumbs.clear();
                        self.toast(
                            format!("Deleted {} — {} freed", tgt.name, fmt_bytes(freed)),
                            false,
                        );
                        return;
                    }
                }
                drop(guard);
                self.toast(format!("Deleted {}", tgt.name), false);
            }
        }
    }
}

// ---------- eframe app ----------

impl eframe::App for Native {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let t = theme(self.theme_kind);
        let scanning = self.core.prog.scanning.load(Ordering::SeqCst);

        if !scanning {
            let gen = {
                let core = self.core.clone();
                let g = core.store.read().unwrap();
                g.as_ref().map(|s| s.generation).unwrap_or(0)
            };
            if gen != self.gen_seen && gen > 0 {
                self.snapshot();
                let root = self.snap.as_ref().map(|s| s.root.clone()).unwrap_or_default();
                self.reset_view_state(root);
                self.show_landing = false;
                self.deleted_since_scan = false;
            }
        }

        if scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
            self.scanning_screen(ctx, &t);
            return;
        }
        if self.show_landing || self.snap.is_none() {
            self.landing_screen(ctx, &t);
            return;
        }
        self.dashboard(ctx, &t);
    }
}

impl Native {
    // ----- landing -----
    fn landing_screen(&mut self, ctx: &egui::Context, t: &Theme) {
        let mut start: Option<String> = None;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(t.bg))
            .show(ctx, |ui| {
                if self.picking {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(200));
                    if let Ok(mut g) = self.picked.lock() {
                        if let Some(done) = g.take() {
                            self.picking = false;
                            if let Some(p) = done {
                                self.path_input = p;
                            }
                        }
                    }
                }
                if self.snap.is_some() && ui.button("← Back").clicked() {
                    self.show_landing = false;
                }
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.14);
                    ui.horizontal(|ui| {
                        let total_w = 300.0;
                        ui.add_space((ui.available_width() - total_w).max(0.0) / 2.0);
                        if let Some(tex) = &self.logo {
                            ui.add(
                                egui::Image::new(tex)
                                    .fit_to_exact_size(Vec2::splat(52.0))
                                    .rounding(12.0),
                            );
                        }
                        ui.label(
                            RichText::new("Diskhoji").size(38.0).strong().color(t.acc),
                        );
                    });
                    ui.add_space(6.0);
                    ui.label(RichText::new("every byte, accounted for.").size(17.0).color(t.ink2));
                    ui.add_space(34.0);
                    let w = 660.0_f32.min(ui.available_width() - 40.0);
                    ui.allocate_ui(Vec2::new(w, ui.available_height()), |ui| {
                        ui.label(RichText::new("SCAN A FOLDER").size(13.0).color(t.ink3));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let te = egui::TextEdit::singleline(&mut self.path_input)
                                .desired_width(w - 300.0)
                                .font(TextStyle::Body);
                            let resp = ui.add(te);
                            if ui.button("Browse…").clicked() && !self.picking {
                                self.picking = true;
                                let slot = self.picked.clone();
                                std::thread::spawn(move || {
                                    let r = pick_folder_blocking();
                                    if let Ok(mut g) = slot.lock() {
                                        *g = Some(r);
                                    }
                                });
                            }
                            let dark_ink = if self.theme_kind == ThemeKind::Dark {
                                hex("#161006")
                            } else {
                                hex("#FFFFFF")
                            };
                            let go = ui.add(
                                egui::Button::new(RichText::new("Scan").strong().color(dark_ink))
                                    .fill(t.acc),
                            );
                            if go.clicked()
                                || (resp.lost_focus()
                                    && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                            {
                                start = Some(self.path_input.clone());
                            }
                        });
                        ui.add_space(22.0);
                        ui.label(RichText::new("OR PICK A VOLUME").size(13.0).color(t.ink3));
                        ui.add_space(6.0);
                        let roots = self.roots.clone();
                        for chunk in roots.chunks(2) {
                            ui.columns(2, |cols| {
                                for (i, r) in chunk.iter().enumerate() {
                                    let used = if r.total > 0 {
                                        (r.total - r.free) as f32 / r.total as f32
                                    } else {
                                        0.0
                                    };
                                    let text = format!(
                                        "{}\n{}\n{} free of {}",
                                        r.label,
                                        r.path,
                                        fmt_bytes(r.free),
                                        fmt_bytes(r.total)
                                    );
                                    let btn = egui::Button::new(RichText::new(text).size(14.5))
                                        .fill(t.panel)
                                        .stroke(Stroke::new(1.0, t.line));
                                    let resp = cols[i].add_sized(
                                        Vec2::new(cols[i].available_width(), 78.0),
                                        btn,
                                    );
                                    let bar = Rect::from_min_size(
                                        resp.rect.left_bottom() + Vec2::new(12.0, -13.0),
                                        Vec2::new(resp.rect.width() - 24.0, 5.0),
                                    );
                                    cols[i].painter().rect_filled(bar, 2.0, t.line);
                                    let mut fb = bar;
                                    fb.set_right(bar.left() + bar.width() * used);
                                    cols[i].painter().rect_filled(fb, 2.0, t.ink3);
                                    if resp.clicked() {
                                        start = Some(r.path.clone());
                                    }
                                }
                            });
                            ui.add_space(4.0);
                        }
                        ui.add_space(14.0);
                        ui.label(
                            RichText::new(
                                "Scanning stays on one filesystem, and Diskhoji makes no \
                                 network connection on its own — only if you opt into update checks.",
                            )
                            .size(13.5)
                            .color(t.ink3),
                        );
                    });
                });
            });
        if let Some(p) = start {
            self.begin_scan(p);
        }
        self.paint_toast(ctx, t);
    }

    // ----- scanning -----
    fn scanning_screen(&mut self, ctx: &egui::Context, t: &Theme) {
        let mut cancel = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(t.bg))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.24);
                    ui.label(RichText::new("● SCANNING").size(14.0).color(t.acc));
                    ui.add_space(4.0);
                    ui.label(RichText::new(&self.path_input).size(14.0).color(t.ink3));
                    ui.add_space(18.0);
                    let bytes = self.core.prog.bytes.load(Ordering::Relaxed);
                    ui.label(RichText::new(fmt_bytes(bytes)).size(54.0).strong());
                    ui.add_space(16.0);
                    let files = self.core.prog.files.load(Ordering::Relaxed);
                    let dirs = self.core.prog.dirs.load(Ordering::Relaxed);
                    let el = self
                        .scan_started
                        .map(|s| s.elapsed().as_secs_f32())
                        .unwrap_or(0.0);
                    ui.label(
                        RichText::new(format!(
                            "{} files   ·   {} folders   ·   {:.1} s",
                            fmt_n(files),
                            fmt_n(dirs),
                            el
                        ))
                        .size(18.0)
                        .color(t.ink2),
                    );
                    ui.add_space(10.0);
                    let cur = self.core.prog.current.lock().unwrap().clone();
                    ui.label(RichText::new(cur).size(13.0).color(t.ink3));
                    ui.add_space(22.0);
                    if ui.button("Cancel scan").clicked() {
                        cancel = true;
                    }
                });
            });
        if cancel {
            self.core.prog.cancel.store(true, Ordering::SeqCst);
        }
    }

    // ----- dashboard -----
    fn dashboard(&mut self, ctx: &egui::Context, t: &Theme) {
        // a persisted zoom from an older build could exceed the new cap
        let zf = ctx.zoom_factor();
        if zf > 1.6 || zf < 0.8 {
            ctx.set_zoom_factor(zf.clamp(0.8, 1.6));
        }
        self.poll_pending_delete(ctx);
        if let Some((_, tgt)) = &self.pending_delete {
            let name = tgt.name.clone();
            egui::Area::new(egui::Id::new("del_chip"))
                .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 70.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::default()
                        .fill(t.panel2)
                        .stroke(Stroke::new(1.0, t.acc))
                        .rounding(7.0)
                        .inner_margin(egui::Margin::symmetric(14.0, 8.0))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(format!("Deleting {}…", name)).size(14.0),
                            );
                        });
                });
        }
        let core = self.core.clone();
        let mut acts: Vec<Act> = Vec::new();
        let mut rescan: Option<String> = None;
        let mut retarget = false;
        let mut theme_flip = false;

        if self.delete_target.is_none() {
            if ctx.input(|i| i.key_pressed(egui::Key::Backspace)) && self.crumbs.len() > 1 {
                acts.push(Act::Zoom(self.crumbs[self.crumbs.len() - 2].0));
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
                if let Some(sel) = self.sel {
                    if sel != 0 {
                        acts.push(Act::AskDelete(sel));
                    }
                }
            }
        } else if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.delete_target = None;
        }
        if self.delete_target.is_none()
            && self.ext_sel.is_some()
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.ext_sel = None;
        }

        // header
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::default()
                    .fill(t.panel)
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(tex) = &self.logo {
                        ui.add(
                            egui::Image::new(tex)
                                .fit_to_exact_size(Vec2::splat(26.0))
                                .rounding(6.0),
                        );
                    } else {
                        ui.label(RichText::new("▦").size(22.0).color(t.acc));
                    }
                    ui.label(RichText::new("Diskhoji").size(17.0).strong());
                    ui.label(
                        RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                            .size(13.0)
                            .color(t.ink3),
                    );
                    ui.add_space(10.0);
                    // Right-side controls reserve space from the right edge; the
                    // path + action buttons then fill the middle, and the path
                    // truncates instead of colliding with the controls.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Zoom controls FIRST so they pin to the right edge and
                        // don't drift under the cursor as the UI scales — pressing
                        // A+ repeatedly can no longer land on the theme toggle.
                        // Text size as a menu: you pick a discrete size once, so
                        // no repeatedly-clicked button that drifts under the cursor.
                        let z = ctx.zoom_factor();
                        ui.menu_button(format!("Text {:.0}%", z * 100.0), |ui| {
                            ui.set_min_width(150.0);
                            for (lbl, val) in [
                                ("Small — 90%", 0.9_f32),
                                ("Normal — 100%", 1.0),
                                ("Large — 120%", 1.2),
                                ("Larger — 140%", 1.4),
                            ] {
                                let on = (z - val).abs() < 0.03;
                                if ui.selectable_label(on, lbl).clicked() {
                                    ctx.set_zoom_factor(val);
                                    ui.close_menu();
                                }
                            }
                        });
                        let label = match self.theme_kind {
                            ThemeKind::Dark => "Light",
                            ThemeKind::Light => "Dark",
                        };
                        if ui.button(label).clicked() {
                            theme_flip = true;
                        }
                        if ui.button("About").clicked() {
                            self.show_about = true;
                        }
                        if let UpdateState::Available { ver, .. } =
                            self.update.lock().map(|g| g.clone()).unwrap_or(UpdateState::Failed)
                        {
                            let dark_ink = if self.theme_kind == ThemeKind::Dark {
                                hex("#161006")
                            } else {
                                hex("#FFFFFF")
                            };
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(format!("Update {}", ver))
                                            .strong()
                                            .color(dark_ink),
                                    )
                                    .fill(t.acc),
                                )
                                .on_hover_text("A newer version is available")
                                .clicked()
                            {
                                self.show_about = true;
                            }
                        }
                        ui.add_space(8.0);
                        // middle band, left-aligned, path shrinks to fit
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            if ui.button("⟳ Rescan").clicked() {
                                if let Some(s) = &self.snap {
                                    rescan = Some(s.root.clone());
                                }
                            }
                            if ui.button("Change target").clicked() {
                                retarget = true;
                            }
                            if let Some(s) = &self.snap {
                                ui.add_space(6.0);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&s.root).size(14.0).color(t.ink2),
                                    )
                                    .wrap_mode(egui::TextWrapMode::Truncate),
                                )
                                .on_hover_text(&s.root);
                            }
                        });
                    });
                });
            });

        // stats
        egui::TopBottomPanel::top("stats")
            .frame(
                egui::Frame::default()
                    .fill(t.panel)
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0)),
            )
            .show(ctx, |ui| {
                if let Some(s) = &self.snap {
                    let unreadable_hover = {
                        let v = core
                            .prog
                            .err_paths
                            .lock()
                            .map(|v| v.clone())
                            .unwrap_or_default();
                        if v.is_empty() {
                            String::new()
                        } else {
                            let mut h = String::from(
                                "Not readable by your user (permission denied):\n",
                            );
                            for p in v.iter().take(14) {
                                h.push_str("  ");
                                h.push_str(p);
                                h.push('\n');
                            }
                            if v.len() > 14 {
                                h.push_str("  … and more");
                            }
                            h
                        }
                    };
                    let used_pct = if s.disk_total > s.disk_free {
                        100.0 * s.bytes as f64 / (s.disk_total - s.disk_free) as f64
                    } else {
                        0.0
                    };
                    let tiles: [(&str, String, String); 5] = [
                        ("SCANNED", fmt_bytes(s.bytes), format!("{} of used disk", fmt_pct(used_pct))),
                        (
                            "FILES",
                            fmt_n(s.files),
                            if s.errors > 0 {
                                format!("{} unreadable", fmt_n(s.errors))
                            } else {
                                "all readable".into()
                            },
                        ),
                        ("FOLDERS", fmt_n(s.dirs), "one filesystem".into()),
                        (
                            "LARGEST FILE",
                            s.largest.first().map(|b| fmt_bytes(b.size)).unwrap_or("—".into()),
                            s.largest.first().map(|b| b.name.clone()).unwrap_or_default(),
                        ),
                        (
                            "SCAN TIME",
                            fmt_time(s.elapsed),
                            format!(
                                "{} files/s",
                                fmt_n(if s.elapsed > 0 { s.files * 1000 / s.elapsed } else { 0 })
                            ),
                        ),
                    ];
                    ui.columns(5, |cols| {
                        for (i, (lbl, val, sub)) in tiles.iter().enumerate() {
                            egui::Frame::default()
                                .fill(t.panel2)
                                .stroke(Stroke::new(1.0, t.line))
                                .rounding(8.0)
                                .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                                .show(&mut cols[i], |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(
                                        RichText::new(*lbl).size(12.5).color(t.ink3),
                                    );
                                    let vsize = (ui.available_width()
                                        / (val.chars().count().max(4) as f32 * 0.64))
                                        .clamp(15.0, 27.0);
                                    ui.label(RichText::new(val).size(vsize).strong());
                                    let hover = if i == 1 && !unreadable_hover.is_empty() {
                                        unreadable_hover.as_str()
                                    } else {
                                        sub.as_str()
                                    };
                                    ui.label(
                                        RichText::new(truncate_to(
                                            sub,
                                            ui.available_width(),
                                            13.0,
                                        ))
                                        .size(13.0)
                                        .color(t.ink3),
                                    )
                                    .on_hover_text(hover);
                                });
                        }
                    });
                }
            });

        // status bar
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::default()
                    .fill(t.panel)
                    .inner_margin(egui::Margin::symmetric(16.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let msg = if let Some(sel) = self.sel {
                        let guard = core.store.read().unwrap();
                        guard
                            .as_ref()
                            .filter(|s| (sel as usize) < s.nodes.len() && s.nodes[sel as usize].alive)
                            .map(|s| {
                                let n = &s.nodes[sel as usize];
                                format!(
                                    "{} · {} · {} of total",
                                    scan::path_of(s, sel),
                                    fmt_bytes(n.size),
                                    fmt_pct(
                                        100.0 * n.size as f64 / s.nodes[0].size.max(1) as f64
                                    )
                                )
                            })
                            .unwrap_or_else(|| "ready".into())
                    } else {
                        "ready · dbl-click to zoom · right-click for actions".into()
                    };
                    let left_w = (ui.available_width() - 460.0).max(200.0);
                    ui.label(
                        RichText::new(truncate_to(&msg, left_w, 14.0))
                            .size(14.0)
                            .color(t.ink2),
                    )
                    .on_hover_text(&msg);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(s) = &self.snap {
                            let free = if s.disk_total > 0 {
                                format!(" · {} free", fmt_bytes(s.disk_free))
                            } else {
                                String::new()
                            };
                            ui.label(
                                RichText::new(format!(
                                    "{} files · {} folders · {}{}",
                                    fmt_n(s.files),
                                    fmt_n(s.dirs),
                                    fmt_time(s.elapsed),
                                    free
                                ))
                                .size(13.0)
                                .color(t.ink3),
                            );
                        }
                    });
                });
            });

        // left tree
        egui::SidePanel::left("tree")
            .frame(
                egui::Frame::default()
                    .fill(t.panel)
                    .inner_margin(egui::Margin::symmetric(8.0, 8.0)),
            )
            .default_width(380.0)
            .width_range(280.0..=560.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("EXPLORER").size(12.5).color(t.ink3));
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let guard = core.store.read().unwrap();
                        if let Some(s) = guard.as_ref() {
                            draw_tree(ui, s, 0, 0, t, self.sel, &mut acts, self.deleted_since_scan);
                        }
                    });
            });

        // right panels
        egui::SidePanel::right("types")
            .frame(
                egui::Frame::default()
                    .fill(t.panel)
                    .inner_margin(egui::Margin::symmetric(10.0, 8.0)),
            )
            .default_width(340.0)
            .width_range(260.0..=480.0)
            .show(ctx, |ui| {
                let half = ui.available_height() / 2.0 - 24.0;
                ui.label(RichText::new("FILE TYPES").size(12.5).color(t.ink3));
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt("types_scroll")
                    .max_height(half)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.types_panel(ui, t, &mut acts);
                    });
                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new("LARGEST FILES").size(12.5).color(t.ink3));
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt("largest_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.largest_panel(ui, t, &mut acts);
                    });
            });

        // center map
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(t.bg)
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if self.crumbs.len() > 1 {
                        if ui
                            .button("↑ up")
                            .on_hover_text("Go up one level (Backspace)")
                            .clicked()
                        {
                            acts.push(Act::Zoom(self.crumbs[self.crumbs.len() - 2].0));
                        }
                    }
                    for (v, label) in [
                        (View::Map, "▦ map"),
                        (View::Heat, "▤ heat"),
                        (View::Rings, "○ rings"),
                    ] {
                        if ui.selectable_label(self.view == v, label).clicked()
                            && self.view != v
                        {
                            self.view = v;
                            self.invalidate_map();
                        }
                    }
                    if self.ext_sel.is_some() && ui.button("✕ clear filter").clicked() {
                        self.ext_sel = None;
                    }
                    ui.separator();
                    egui::ScrollArea::horizontal()
                        .id_salt("crumb_scroll")
                        .max_height(30.0)
                        .stick_to_right(true)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let crumbs = self.crumbs.clone();
                                for (i, (id, name)) in crumbs.iter().enumerate() {
                                    if i > 0 {
                                        ui.label(RichText::new("/").color(t.ink3));
                                    }
                                    let last = i == crumbs.len() - 1;
                                    let disp = truncate_to(name, 260.0, 14.0);
                                    if last {
                                        ui.label(RichText::new(disp).strong());
                                    } else if ui
                                        .link(RichText::new(disp).color(t.ink2))
                                        .clicked()
                                    {
                                        acts.push(Act::Zoom(*id));
                                    }
                                }
                            });
                        });
                });
                ui.add_space(4.0);

                let avail = ui.available_size();
                let (outer, resp) = ui.allocate_exact_size(avail, Sense::click());
                let painter = ui.painter_at(outer);
                painter.rect_filled(outer, 8.0, t.well);
                painter.rect_stroke(outer, 8.0, Stroke::new(1.0, t.line));

                {
                    let guard = core.store.read().unwrap();
                    if let Some(s) = guard.as_ref() {
                        self.relayout(s, outer.width(), outer.height());
                        if self.crumbs.last().map(|c| c.0) != Some(self.zoom) {
                            self.rebuild_crumbs(s);
                        }
                    }
                }

                let origin = outer.min;
                let hover_pos = resp.hover_pos();
                match self.view {
                    View::Map => self.paint_map(&painter, origin, t),
                    View::Heat => self.paint_heat(&painter, origin, outer, t),
                    View::Rings => self.paint_rings(&painter, outer, t),
                }

                let hit: Option<(u32, String, u64, bool, u8)> = hover_pos.and_then(|p| {
                    let lx = p.x - origin.x;
                    let ly = p.y - origin.y;
                    match self.view {
                        View::Map => self
                            .rects
                            .iter()
                            .find(|r| {
                                lx >= r.x && lx < r.x + r.w && ly >= r.y && ly < r.y + r.h
                            })
                            .map(|r| (r.i, r.n.clone(), r.z, r.d != 0, r.d)),
                        View::Heat => self
                            .heat_cells
                            .iter()
                            .find(|c| c.rect.contains(Pos2::new(lx, ly)))
                            .map(|c| {
                                (c.id, c.name.clone(), c.size, c.dir, if c.dir { 1 } else { 0 })
                            }),
                        View::Rings => {
                            let cc = outer.center();
                            let dx = p.x - cc.x;
                            let dy = p.y - cc.y;
                            let r = (dx * dx + dy * dy).sqrt();
                            let mut a = dy.atan2(dx);
                            if a < 0.0 {
                                a += std::f32::consts::TAU;
                            }
                            self.sectors
                                .iter()
                                .find(|s| r >= s.r0 && r < s.r1 && a >= s.a0 && a < s.a1)
                                .map(|s| (s.id, s.name.clone(), s.size, s.d == 1, s.d))
                        }
                    }
                });

                if let Some((id, name, size, _isdir, kind)) = &hit {
                    let zsize = {
                        let guard = core.store.read().unwrap();
                        guard
                            .as_ref()
                            .map(|s| s.nodes[self.zoom as usize].size)
                            .unwrap_or(1)
                    };
                    let extra = match kind {
                        1 => "folder · double-click to zoom",
                        2 => "many small items · double-click to zoom",
                        _ => "",
                    };
                    egui::show_tooltip_at_pointer(
                        ui.ctx(),
                        ui.layer_id(),
                        egui::Id::new("map_tip"),
                        |ui| {
                            ui.set_min_width(250.0);
                            ui.set_max_width(420.0);
                            ui.label(
                                RichText::new(truncate_to(name, 400.0, 15.5))
                                    .strong()
                                    .size(15.5),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{} · {} of this view",
                                    fmt_bytes(*size),
                                    fmt_pct(100.0 * *size as f64 / zsize.max(1) as f64)
                                ))
                                .size(14.0),
                            );
                            if !extra.is_empty() {
                                ui.label(RichText::new(extra).size(12.5).color(t.ink3));
                            }
                        },
                    );

                    if resp.clicked() {
                        acts.push(Act::Select(*id));
                    }
                    if resp.double_clicked() {
                        match kind {
                            1 | 2 => acts.push(Act::Zoom(*id)),
                            _ => {
                                let guard = core.store.read().unwrap();
                                if let Some(s) = guard.as_ref() {
                                    if let Some(p) = scan::ancestors_of(s, *id).last() {
                                        acts.push(Act::Zoom(*p));
                                    }
                                }
                            }
                        }
                    }
                }
                // pin the target at the moment of right-click so the menu
                // stays stable while the pointer travels over it
                if resp.secondary_clicked() {
                    self.ctx_target = hit.clone();
                }
                let menu_target = self.ctx_target.clone();
                resp.context_menu(|ui| {
                    if let Some((id, name, size, isdir, _)) = &menu_target {
                        node_menu(ui, *id, name, *size, *isdir, &mut acts);
                    } else {
                        ui.close_menu();
                    }
                });
            });

        self.delete_modal(ctx, t);
        self.about_window(ctx, t);
        self.paint_toast(ctx, t);

        for a in acts {
            self.apply(a);
        }
        if theme_flip {
            self.theme_kind = match self.theme_kind {
                ThemeKind::Dark => ThemeKind::Light,
                ThemeKind::Light => ThemeKind::Dark,
            };
            apply_style(ctx, self.theme_kind);
            self.invalidate_map(); // cached mesh colors are theme-baked
            let _ = std::fs::create_dir_all(dirs_config());
            let _ = std::fs::write(
                &self.prefs_path,
                if self.theme_kind == ThemeKind::Light { "light" } else { "dark" },
            );
        }
        if let Some(p) = rescan {
            self.begin_scan(p);
        }
        if retarget {
            self.show_landing = true;
            self.roots = list_roots();
        }
    }

    fn types_panel(&self, ui: &mut egui::Ui, t: &Theme, acts: &mut Vec<Act>) {
        let Some(s) = &self.snap else { return };
        let total = s.bytes.max(1);
        for e in &s.exts {
            let c = t.slot_color(e.slot);
            let selectable = e.slot < 8;
            let active = self.ext_sel == Some(e.slot);
            let label = if e.name.is_empty() {
                "no extension".to_string()
            } else {
                format!(".{}", e.name)
            };
            let frac = e.bytes as f32 / total as f32;
            let resp = ui.allocate_response(
                Vec2::new(ui.available_width(), 48.0),
                if selectable { Sense::click() } else { Sense::hover() },
            );
            let r = resp.rect;
            if active {
                ui.painter().rect_filled(r, 5.0, t.acc.gamma_multiply(0.12));
            } else if resp.hovered() && selectable {
                ui.painter().rect_filled(r, 5.0, t.ink.gamma_multiply(0.05));
            }
            let p = ui.painter();
            p.rect_filled(
                Rect::from_min_size(r.min + Vec2::new(6.0, 9.0), Vec2::splat(12.0)),
                3.0,
                c,
            );
            p.text(
                r.min + Vec2::new(26.0, 4.0),
                Align2::LEFT_TOP,
                &label,
                FontId::new(15.5, FontFamily::Monospace),
                t.ink,
            );
            p.text(
                Pos2::new(r.max.x - 8.0, r.min.y + 4.0),
                Align2::RIGHT_TOP,
                fmt_bytes(e.bytes),
                FontId::new(14.5, FontFamily::Monospace),
                t.ink2,
            );
            let bar = Rect::from_min_size(
                r.min + Vec2::new(26.0, 30.0),
                Vec2::new((r.width() - 140.0).max(30.0), 6.0),
            );
            p.rect_filled(bar, 3.0, t.line);
            let mut fb = bar;
            fb.set_right(bar.left() + bar.width() * frac.max(0.005));
            p.rect_filled(fb, 3.0, c);
            p.text(
                Pos2::new(r.max.x - 8.0, r.min.y + 27.0),
                Align2::RIGHT_TOP,
                format!("{} files", fmt_n(e.files)),
                FontId::new(12.5, FontFamily::Monospace),
                t.ink3,
            );
            if selectable && resp.clicked() {
                acts.push(Act::ToggleExt(e.slot));
            }
        }
    }

    fn largest_panel(&self, ui: &mut egui::Ui, t: &Theme, acts: &mut Vec<Act>) {
        let Some(s) = &self.snap else { return };
        for (i, b) in s.largest.iter().enumerate() {
            let resp =
                ui.allocate_response(Vec2::new(ui.available_width(), 30.0), Sense::click());
            let r = resp.rect;
            if resp.hovered() {
                ui.painter().rect_filled(r, 5.0, t.ink.gamma_multiply(0.05));
            }
            let p = ui.painter();
            p.text(
                r.min + Vec2::new(4.0, 6.0),
                Align2::LEFT_TOP,
                format!("{:>2}", i + 1),
                FontId::new(13.0, FontFamily::Monospace),
                t.ink3,
            );
            p.rect_filled(
                Rect::from_min_size(r.min + Vec2::new(30.0, 10.0), Vec2::splat(10.0)),
                2.0,
                t.slot_color(b.slot),
            );
            let shown = truncate_to(&b.name, (r.width() - 150.0).max(40.0), 15.0);
            let clipped = shown.ends_with('…');
            p.text(
                r.min + Vec2::new(48.0, 6.0),
                Align2::LEFT_TOP,
                shown,
                FontId::new(15.0, FontFamily::Monospace),
                t.ink,
            );
            p.text(
                Pos2::new(r.max.x - 6.0, r.min.y + 6.0),
                Align2::RIGHT_TOP,
                fmt_bytes(b.size),
                FontId::new(14.5, FontFamily::Monospace),
                t.ink2,
            );
            if resp.clicked() {
                acts.push(Act::Select(b.id));
            }
            let resp = if clipped {
                resp.on_hover_text(&b.name)
            } else {
                resp
            };
            resp.context_menu(|ui| {
                node_menu(ui, b.id, &b.name, b.size, false, acts);
            });
        }
    }

    fn paint_map(&self, p: &egui::Painter, o: Pos2, t: &Theme) {
        let mut mesh = self.cached_mesh.clone();
        mesh.translate(o.to_vec2());
        p.add(egui::Shape::mesh(mesh));

        if let Some(slot) = self.ext_sel {
            let dim = if self.theme_kind == ThemeKind::Dark {
                Color32::from_black_alpha(190)
            } else {
                Color32::from_white_alpha(200)
            };
            for r in &self.rects {
                if r.s != slot {
                    p.rect_filled(
                        Rect::from_min_size(
                            Pos2::new(o.x + r.x, o.y + r.y),
                            Vec2::new(r.w, r.h),
                        ),
                        0.0,
                        dim,
                    );
                }
            }
        }

        for d in &self.dir_rects {
            if d.p > 5 {
                continue;
            }
            let a = (0.42 - d.p as f32 * 0.08).max(0.08);
            p.rect_stroke(
                Rect::from_min_size(Pos2::new(o.x + d.x, o.y + d.y), Vec2::new(d.w, d.h)),
                0.0,
                Stroke::new(1.0, Color32::from_black_alpha((a * 255.0) as u8)),
            );
        }

        let label_col = if self.theme_kind == ThemeKind::Dark {
            Color32::from_rgba_unmultiplied(238, 242, 248, 220)
        } else {
            Color32::from_rgba_unmultiplied(20, 26, 36, 235)
        };
        let dir_col = label_col.gamma_multiply(0.92);
        let font = FontId::new(13.0, FontFamily::Monospace);
        for (pos, text, is_dir) in &self.cached_labels {
            p.text(
                Pos2::new(o.x + pos.x, o.y + pos.y),
                Align2::LEFT_TOP,
                text,
                font.clone(),
                if *is_dir { dir_col } else { label_col },
            );
        }
        if let Some(sel) = self.sel {
            let target = self
                .rects
                .iter()
                .find(|r| r.i == sel && r.d == 0)
                .or_else(|| self.rects.iter().find(|r| r.i == sel))
                .map(|r| Rect::from_min_size(Pos2::new(o.x + r.x, o.y + r.y), Vec2::new(r.w, r.h)))
                .or_else(|| {
                    self.dir_rects.iter().find(|d| d.i == sel).map(|d| {
                        Rect::from_min_size(Pos2::new(o.x + d.x, o.y + d.y), Vec2::new(d.w, d.h))
                    })
                });
            if let Some(r) = target {
                p.rect_stroke(r, 0.0, Stroke::new(2.0, t.ink));
            }
        }
    }

    fn paint_heat(&self, p: &egui::Painter, o: Pos2, outer: Rect, t: &Theme) {
        for c in &self.heat_cells {
            let r = c.rect.translate(o.to_vec2());
            let dimmed = self.ext_sel.map(|s| c.slot != s).unwrap_or(false);
            let col = if dimmed {
                t.heat[c.lvl as usize].gamma_multiply(0.25)
            } else {
                t.heat[c.lvl as usize]
            };
            let rounding = Rounding::same((r.width() * 0.22).min(5.0));
            p.rect_filled(r, rounding, col);
            if c.dir {
                p.rect_stroke(r, rounding, Stroke::new(1.0, t.ink.gamma_multiply(0.45)));
            }
            if self.sel == Some(c.id) {
                p.rect_stroke(r.expand(1.5), Rounding::same(5.0), Stroke::new(2.0, t.ink));
            }
        }
        let ly = outer.max.y - 26.0;
        let lx = outer.max.x - 190.0;
        let font = FontId::new(13.0, FontFamily::Monospace);
        p.text(
            Pos2::new(lx - 8.0, ly + 7.0),
            Align2::RIGHT_CENTER,
            "less",
            font.clone(),
            t.ink3,
        );
        for (i, c) in t.heat.iter().enumerate() {
            p.rect_filled(
                Rect::from_min_size(Pos2::new(lx + i as f32 * 19.0, ly), Vec2::splat(14.0)),
                3.0,
                *c,
            );
        }
        p.text(
            Pos2::new(lx + 5.0 * 19.0 + 6.0, ly + 7.0),
            Align2::LEFT_CENTER,
            "more",
            font.clone(),
            t.ink3,
        );
        let cap = format!(
            "{} items · one cell each · ring = folder · color = size (log){}",
            fmt_n(self.heat_cells.len() as u64),
            if self.heat_more > 0 {
                format!(" · {} smaller not shown", fmt_n(self.heat_more as u64))
            } else {
                String::new()
            }
        );
        let cap_w = (outer.width() - 280.0).max(80.0);
        p.text(
            Pos2::new(outer.min.x + 18.0, ly + 7.0),
            Align2::LEFT_CENTER,
            truncate_to(&cap, cap_w, 13.0),
            font,
            t.ink3,
        );
    }

    fn paint_rings(&self, p: &egui::Painter, outer: Rect, t: &Theme) {
        let c = outer.center();
        let mut mesh = egui::Mesh::default();
        for s in &self.sectors {
            let dimmed = self.ext_sel.map(|x| s.slot != x).unwrap_or(false);
            let mut col = t.slot_color(s.slot);
            if dimmed {
                col = col.gamma_multiply(0.22);
            }
            let steps = (((s.a1 - s.a0) / 0.05).ceil() as usize).clamp(2, 220);
            let gap = 0.0015_f32.min((s.a1 - s.a0) * 0.08);
            let (a0, a1) = (s.a0 + gap, s.a1 - gap);
            let inner = lighten(col, 0.12);
            let outer_c = darken(col, 0.14);
            let mut prev: Option<(u32, u32)> = None;
            for i in 0..=steps {
                let a = a0 + (a1 - a0) * i as f32 / steps as f32;
                let (sin, cos) = a.sin_cos();
                let vi = mesh.vertices.len() as u32;
                mesh.colored_vertex(Pos2::new(c.x + cos * s.r0, c.y + sin * s.r0), inner);
                mesh.colored_vertex(Pos2::new(c.x + cos * s.r1, c.y + sin * s.r1), outer_c);
                if let Some((v0, v1)) = prev {
                    mesh.add_triangle(v0, v1, vi);
                    mesh.add_triangle(v1, vi + 1, vi);
                }
                prev = Some((vi, vi + 1));
            }
        }
        p.add(egui::Shape::mesh(mesh));

        // selection outline traces the sector
        if let Some(sel) = self.sel {
            if let Some(s) = self.sectors.iter().find(|s| s.id == sel && s.d != 2) {
                let steps = (((s.a1 - s.a0) / 0.05).ceil() as usize).clamp(2, 220);
                let mut pts = Vec::with_capacity(steps * 2 + 2);
                for i in 0..=steps {
                    let a = s.a0 + (s.a1 - s.a0) * i as f32 / steps as f32;
                    let (sin, cos) = a.sin_cos();
                    pts.push(Pos2::new(c.x + cos * s.r1, c.y + sin * s.r1));
                }
                for i in (0..=steps).rev() {
                    let a = s.a0 + (s.a1 - s.a0) * i as f32 / steps as f32;
                    let (sin, cos) = a.sin_cos();
                    pts.push(Pos2::new(c.x + cos * s.r0, c.y + sin * s.r0));
                }
                p.add(egui::Shape::closed_line(pts, Stroke::new(2.0, t.ink)));
            }
        }

        // center label
        p.text(
            c - Vec2::new(0.0, 12.0),
            Align2::CENTER_CENTER,
            truncate_to(&self.ring_label.0, 170.0, 15.0),
            FontId::new(15.0, FontFamily::Monospace),
            t.ink,
        );
        p.text(
            c + Vec2::new(0.0, 12.0),
            Align2::CENTER_CENTER,
            &self.ring_label.1,
            FontId::new(14.0, FontFamily::Monospace),
            t.ink3,
        );
    }

    fn delete_modal(&mut self, ctx: &egui::Context, t: &Theme) {
        let Some(tgt) = self.delete_target.clone() else {
            return;
        };
        let DelTarget { id: _, gen: _, name, path, size, dir, files } = tgt.clone();
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("modal_dim"))
            .fixed_pos(screen.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                // swallow every click outside the dialog: the modal is modal
                let resp = ui.allocate_response(screen.size(), egui::Sense::click());
                ui.painter()
                    .rect_filled(screen, 0.0, Color32::from_black_alpha(160));
                let _ = resp;
            });
        let mut close = false;
        let mut confirm = false;
        egui::Window::new(RichText::new("Delete permanently?").size(19.0).strong())
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_width(470.0);
                egui::Frame::default()
                    .fill(t.well)
                    .stroke(Stroke::new(1.0, t.line))
                    .inner_margin(12.0)
                    .rounding(6.0)
                    .show(ui, |ui| {
                        ui.label(RichText::new(&name).strong().size(16.0));
                        ui.label(RichText::new(&path).size(13.0).color(t.ink3));
                        ui.label(
                            RichText::new(format!(
                                "{}{}",
                                fmt_bytes(size),
                                if dir {
                                    format!(" · {} files", fmt_n(files as u64))
                                } else {
                                    String::new()
                                }
                            ))
                            .size(14.0)
                            .color(t.ink2),
                        );
                    });
                ui.add_space(8.0);
                ui.label(
                    RichText::new("This bypasses the trash. There is no undo.")
                        .color(t.danger)
                        .size(14.5),
                );
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let del = egui::Button::new(
                        RichText::new("Delete permanently").color(t.danger).size(15.0),
                    )
                    .stroke(Stroke::new(1.0, t.danger));
                    if ui.add(del).clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.delete_target = None;
        }
        if confirm {
            self.delete_target = None;
            self.do_delete(tgt);
        }
    }

    fn paint_toast(&mut self, ctx: &egui::Context, t: &Theme) {
        if let Some(p) = self.pending_copy.take() {
            ctx.copy_text(p);
            self.toast("Path copied", false);
        }
        let Some((msg, at, bad)) = self.toast.clone() else { return };
        if at.elapsed().as_secs_f32() > 3.2 {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
        egui::Area::new(egui::Id::new("toast"))
            .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -44.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(t.panel2)
                    .stroke(Stroke::new(1.0, if bad { t.danger } else { t.acc }))
                    .rounding(7.0)
                    .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                    .show(ui, |ui| {
                        ui.label(RichText::new(msg).size(15.0));
                    });
            });
    }
}

fn node_menu(ui: &mut egui::Ui, id: u32, name: &str, size: u64, is_dir: bool, acts: &mut Vec<Act>) {
    ui.set_min_width(280.0);
    ui.label(RichText::new(truncate_to(name, 380.0, 15.0)).strong());
    ui.label(RichText::new(fmt_bytes(size)).small());
    ui.separator();
    if ui.button("↗  Open").clicked() {
        acts.push(Act::Open(id));
        ui.close_menu();
    }
    if ui.button("▸  Reveal in file manager").clicked() {
        acts.push(Act::Reveal(id));
        ui.close_menu();
    }
    if ui.button("⧉  Copy path").clicked() {
        acts.push(Act::CopyPath(id));
        ui.close_menu();
    }
    if is_dir {
        if ui.button("⌕  Zoom here").clicked() {
            acts.push(Act::Zoom(id));
            ui.close_menu();
        }
    } else if ui.button("⌕  Zoom to parent folder").clicked() {
        acts.push(Act::ZoomParent(id));
        ui.close_menu();
    }
    ui.separator();
    if id == 0 {
        ui.add_enabled(false, egui::Button::new("✕  Delete permanently…"));
    } else if ui
        .button(RichText::new("✕  Delete permanently…").color(hex("#E66767")))
        .clicked()
    {
        acts.push(Act::AskDelete(id));
        ui.close_menu();
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_tree(
    ui: &mut egui::Ui,
    s: &Store,
    id: u32,
    depth: usize,
    t: &Theme,
    sel: Option<u32>,
    acts: &mut Vec<Act>,
    dirty: bool,
) {
    if depth > 32 {
        return;
    }
    let n = &s.nodes[id as usize];
    const CAP: usize = 400;
    // arena order is already size-descending from the scan; only a delete can
    // make it stale (dead nodes + shifted ancestor sizes), so sort only then
    let (kids, more) = if dirty {
        let mut k: Vec<u32> = (n.first_child..n.first_child + n.child_count)
            .filter(|c| s.nodes[*c as usize].alive)
            .collect();
        k.sort_unstable_by(|a, b| s.nodes[*b as usize].size.cmp(&s.nodes[*a as usize].size));
        let more = k.len().saturating_sub(CAP);
        k.truncate(CAP);
        (k, more)
    } else {
        let total = n.child_count as usize;
        let kids: Vec<u32> =
            (n.first_child..n.first_child + (total.min(CAP)) as u32).collect();
        (kids, total.saturating_sub(CAP))
    };
    let parent_size = n.size.max(1);

    for c in kids {
        let k = &s.nodes[c as usize];
        let pct = 100.0 * k.size as f64 / parent_size as f64;
        if k.is_dir {
            let cid = egui::Id::new((s.generation, c, "tree"));
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), cid, false)
                .show_header(ui, |ui| {
                    tree_row(ui, t, k.name.as_ref(), pct, k.size, None, sel == Some(c), acts, c, true);
                })
                .body(|ui| {
                    draw_tree(ui, s, c, depth + 1, t, sel, acts, dirty);
                });
        } else {
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                let slot = s.exts[k.ext as usize].slot;
                tree_row(
                    ui,
                    t,
                    k.name.as_ref(),
                    pct,
                    k.size,
                    Some(t.slot_color(slot)),
                    sel == Some(c),
                    acts,
                    c,
                    false,
                );
            });
        }
    }
    if more > 0 {
        ui.label(
            RichText::new(format!("… {} smaller items not shown", fmt_n(more as u64)))
                .size(13.0)
                .color(t.ink3),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn tree_row(
    ui: &mut egui::Ui,
    t: &Theme,
    name: &str,
    pct: f64,
    size: u64,
    chip: Option<Color32>,
    selected: bool,
    acts: &mut Vec<Act>,
    id: u32,
    is_dir: bool,
) {
    let resp = ui.allocate_response(Vec2::new(ui.available_width(), 26.0), Sense::click());
    let r = resp.rect;
    if selected {
        ui.painter().rect_filled(r, 4.0, t.acc.gamma_multiply(0.13));
    } else if resp.hovered() {
        ui.painter().rect_filled(r, 4.0, t.ink.gamma_multiply(0.05));
    }
    // percent-of-parent as a colored fill: files wear their type color,
    // folders wear lapis — the row itself becomes the bar chart
    let mut fill = r;
    fill.set_right(r.left() + r.width() * (pct as f32 / 100.0));
    let fill_col = chip.unwrap_or(t.slots[0]).gamma_multiply(0.20);
    ui.painter().rect_filled(fill, 4.0, fill_col);

    let p = ui.painter();
    let x = r.min.x + 4.0;
    if let Some(c) = chip {
        p.rect_filled(
            Rect::from_min_size(Pos2::new(x, r.center().y - 5.0), Vec2::splat(10.0)),
            2.0,
            c,
        );
    } else {
        p.rect_stroke(
            Rect::from_min_size(Pos2::new(x, r.center().y - 5.0), Vec2::splat(10.0)),
            2.0,
            Stroke::new(1.5, t.ink3),
        );
    }
    let shown = truncate_to(name, (r.width() - 170.0).max(40.0), 15.0);
    let clipped = shown.ends_with('…');
    p.text(
        Pos2::new(x + 18.0, r.center().y),
        Align2::LEFT_CENTER,
        shown,
        FontId::new(15.0, FontFamily::Monospace),
        t.ink,
    );
    p.text(
        Pos2::new(r.max.x - 96.0, r.center().y),
        Align2::RIGHT_CENTER,
        fmt_pct(pct),
        FontId::new(13.0, FontFamily::Monospace),
        t.ink3,
    );
    p.text(
        Pos2::new(r.max.x - 6.0, r.center().y),
        Align2::RIGHT_CENTER,
        fmt_bytes(size),
        FontId::new(14.0, FontFamily::Monospace),
        t.ink2,
    );

    if resp.clicked() {
        acts.push(Act::Select(id));
    }
    if resp.double_clicked() && is_dir {
        acts.push(Act::Zoom(id));
    }
    let resp = if clipped {
        resp.on_hover_text(name)
    } else {
        resp
    };
    resp.context_menu(|ui| {
        node_menu(ui, id, name, size, is_dir, acts);
    });
}

fn truncate_to(s: &str, max_px: f32, char_px: f32) -> String {
    let max_chars = (max_px / (char_px * 0.62)).max(4.0) as usize;
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", cut)
    }
}

// ---------- entry ----------

pub fn run(core: Arc<Core>, initial: Option<PathBuf>) {
    if let Some(p) = initial {
        if p.is_dir() {
            start_scan(core.clone(), p);
        }
    }
    let icon_bytes = include_bytes!("../assets/icon-256.png");
    let icon = image::load_from_memory(icon_bytes).ok().map(|i| {
        let rgba = i.to_rgba8();
        let (w, h) = rgba.dimensions();
        egui::IconData {
            rgba: rgba.into_raw(),
            width: w,
            height: h,
        }
    });
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1580.0, 980.0])
        .with_min_inner_size([900.0, 620.0])
        .with_maximized(true)
        .with_title("Diskhoji")
        .with_app_id("diskhoji");
    if let Some(ic) = icon {
        viewport = viewport.with_icon(Arc::new(ic));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "Diskhoji",
        options,
        Box::new(move |cc| Ok(Box::new(Native::new(cc, core)))),
    );
}
