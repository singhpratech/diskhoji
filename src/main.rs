#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod elevate;
mod native;
mod scan;
mod treemap;
mod voyage;

use scan::{Progress, ScanOptions, Store};
use serde::{Deserialize, Serialize};
use std::io::Read;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tiny_http::{Header, Method, Response, Server};

const UI: &str = include_str!("../ui/index.html");

struct App {
    store: RwLock<Option<Store>>,
    prog: Progress,
    generation: std::sync::atomic::AtomicU64,
    /// Per-run secret embedded in the served UI; POSTs must echo it back in
    /// X-Diskhoji-Token, so a random website can't drive the localhost API.
    token: String,
    /// skip-hidden / exclude patterns, shared by native and --web
    opts: RwLock<ScanOptions>,
    /// what the last scan targeted, so Rescan repeats it exactly
    last_target: std::sync::Mutex<Option<ScanTarget>>,
}

#[derive(Clone, Debug, PartialEq)]
enum ScanTarget {
    Path(PathBuf),
    AllVolumes,
}

fn opts_path() -> PathBuf {
    native::dirs_config().join("diskhoji-scanopts.json")
}

fn load_opts() -> ScanOptions {
    std::fs::read_to_string(opts_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_opts(o: &ScanOptions) {
    let _ = std::fs::create_dir_all(native::dirs_config());
    if let Ok(s) = serde_json::to_string_pretty(o) {
        let _ = std::fs::write(opts_path(), s);
    }
}

/// Every distinct local filesystem worth scanning together — "All local
/// drives" in WinDirStat. The Home shortcut is dropped when it lives on a
/// volume already in the list.
fn all_volumes() -> Vec<(String, u64, u64)> {
    let roots = list_roots();
    let mut out: Vec<(String, u64, u64)> = Vec::new();
    #[cfg(unix)]
    let mut seen_dev: Vec<u64> = Vec::new();
    for r in roots.iter().filter(|r| r.label != "Home") {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let Ok(md) = std::fs::metadata(&r.path) else { continue };
            if seen_dev.contains(&md.dev()) {
                continue;
            }
            seen_dev.push(md.dev());
        }
        if out.iter().any(|o| o.0 == r.path) {
            continue;
        }
        out.push((r.path.clone(), r.total, r.free));
    }
    if out.is_empty() {
        if let Some(h) = roots.iter().find(|r| r.label == "Home") {
            out.push((h.path.clone(), h.total, h.free));
        }
    }
    out
}

fn gen_token() -> String {
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_ok();
    if !ok {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let seed = t.as_nanos() as u64 ^ ((std::process::id() as u64) << 32);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(i as u64 * 1442695040888963407)
                >> ((i % 8) * 8)) as u8;
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

// ---------- API payloads ----------

#[derive(Serialize)]
struct StatusResp {
    state: &'static str,
    files: u64,
    dirs: u64,
    bytes: u64,
    errors: u64,
    skipped: u64,
    current: String,
    generation: u64,
    root: Option<String>,
}

#[derive(Serialize)]
struct ChildResp {
    id: u32,
    name: String,
    size: u64,
    files: u32,
    subdirs: u32,
    mtime: i64,
    dir: bool,
    slot: u8,
}

#[derive(Serialize)]
struct NodeResp {
    id: u32,
    name: String,
    path: String,
    size: u64,
    files: u32,
    subdirs: u32,
    mtime: i64,
    dir: bool,
    protected: bool,
    parent_size: u64,
    total: u64,
    generation: u64,
    ancestors: Vec<u32>,
    ancestor_names: Vec<String>,
    children: Vec<ChildResp>,
    more: u32,
}

#[derive(Serialize)]
struct ExtResp {
    ext: String,
    desc: &'static str,
    bytes: u64,
    files: u64,
    slot: u8,
}

#[derive(Serialize)]
struct SearchHit {
    id: u32,
    name: String,
    path: String,
    size: u64,
    mtime: i64,
    dir: bool,
    slot: u8,
}

#[derive(Serialize)]
struct BigFileResp {
    id: u32,
    name: String,
    path: String,
    size: u64,
    slot: u8,
}

#[derive(Serialize)]
struct SummaryResp {
    root: String,
    bytes: u64,
    files: u64,
    dirs: u64,
    errors: u64,
    elapsed_ms: u64,
    generation: u64,
    disk_total: u64,
    disk_free: u64,
    skipped: u64,
    multi: bool,
    is_volume: bool,
    volumes: Vec<scan::VolInfo>,
    exts: Vec<ExtResp>,
    largest: Vec<BigFileResp>,
}

#[derive(Serialize)]
struct TreemapResp {
    generation: u64,
    id: u32,
    size: u64,
    rects: Vec<treemap::Rect>,
    dirs: Vec<treemap::DirRect>,
}

#[derive(Serialize, Clone)]
struct RootEntry {
    path: String,
    label: String,
    total: u64,
    free: u64,
}

#[derive(Deserialize)]
struct ScanReq {
    #[serde(default)]
    path: String,
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
struct IdReq {
    id: u32,
    #[serde(default)]
    generation: Option<u64>,
    /// "trash" (default) or "permanent"
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Deserialize)]
struct OptsReq {
    #[serde(default)]
    skip_hidden: bool,
    #[serde(default)]
    excludes: Vec<String>,
}

// ---------- helpers ----------

#[cfg(unix)]
fn disk_usage(path: &str) -> (u64, u64) {
    let c = match std::ffi::CString::new(path) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut vfs) } == 0 {
        let total = vfs.f_blocks as u64 * vfs.f_frsize as u64;
        let free = vfs.f_bavail as u64 * vfs.f_frsize as u64;
        (total, free)
    } else {
        (0, 0)
    }
}

#[cfg(windows)]
fn disk_usage(path: &str) -> (u64, u64) {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut avail = 0u64;
    let mut total = 0u64;
    let mut free = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    if ok != 0 {
        (total, avail)
    } else {
        (0, 0)
    }
}

fn list_roots() -> Vec<RootEntry> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let (total, free) = disk_usage(&home);
        roots.push(RootEntry { path: home, label: "Home".into(), total, free });
    }
    #[cfg(windows)]
    {
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if std::fs::metadata(&root).is_ok() {
                let (total, free) = disk_usage(&root);
                if total > 0 {
                    roots.push(RootEntry {
                        path: root,
                        label: format!("{}: drive", letter as char),
                        total,
                        free,
                    });
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let allowed = [
            "ext4", "ext3", "ext2", "btrfs", "xfs", "zfs", "f2fs", "jfs", "reiserfs", "vfat",
            "exfat", "ntfs", "ntfs3", "fuseblk",
        ];
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                let mut it = line.split_whitespace();
                let (Some(_dev), Some(mp), Some(fstype)) = (it.next(), it.next(), it.next()) else {
                    continue;
                };
                if !allowed.contains(&fstype) {
                    continue;
                }
                let mp = mp.replace("\\040", " ");
                if mp.starts_with("/snap") || mp.starts_with("/boot/efi") || mp.starts_with("/var/snap") {
                    continue;
                }
                if roots.iter().any(|r| r.path == mp) {
                    continue;
                }
                let (total, free) = disk_usage(&mp);
                let label = if mp == "/" {
                    "System /".to_string()
                } else {
                    mp.rsplit('/').next().unwrap_or(&mp).to_string()
                };
                roots.push(RootEntry { path: mp, label, total, free });
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let (total, free) = disk_usage("/");
        roots.push(RootEntry { path: "/".into(), label: "System /".into(), total, free });
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            for ent in rd.flatten() {
                let p = ent.path().to_string_lossy().into_owned();
                if roots.iter().any(|r| r.path == p) {
                    continue;
                }
                let (total, free) = disk_usage(&p);
                let label = ent.file_name().to_string_lossy().into_owned();
                roots.push(RootEntry { path: p, label, total, free });
            }
        }
    }
    roots
}

#[cfg(target_os = "linux")]
fn uri_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn reveal_in_file_manager(path: &str) {
    #[cfg(windows)]
    {
        let _ = Command::new("explorer").arg(format!("/select,{}", path)).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let uri = format!("file://{}", uri_encode(path));
        let ok = Command::new("dbus-send")
            .args([
                "--session",
                "--print-reply",
                "--dest=org.freedesktop.FileManager1",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{}", uri),
                "string:",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            let parent = Path::new(path).parent().unwrap_or(Path::new("/"));
            let _ = Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

fn open_with_default(path: &str) {
    #[cfg(windows)]
    let _ = Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(path).spawn();
}

/// Open the user's terminal at a folder (for a file: its parent).
fn open_terminal(path: &str) {
    let dir = if std::path::Path::new(path).is_dir() {
        path.to_string()
    } else {
        std::path::Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    };
    #[cfg(windows)]
    {
        // Windows Terminal if present, else a classic console
        if Command::new("wt").args(["-d", &dir]).spawn().is_ok() {
            return;
        }
        let _ = Command::new("cmd")
            .args(["/C", "start", "cmd", "/K", &format!("cd /d \"{}\"", dir)])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").args(["-a", "Terminal", &dir]).spawn();
    #[cfg(target_os = "linux")]
    {
        if let Ok(term) = std::env::var("TERMINAL") {
            if Command::new(&term).current_dir(&dir).spawn().is_ok() {
                return;
            }
        }
        for (cmd, args) in [
            ("x-terminal-emulator", &[][..]),
            ("gnome-terminal", &["--working-directory", dir.as_str()][..]),
            ("konsole", &["--workdir", dir.as_str()][..]),
            ("ptyxis", &["--working-directory", dir.as_str()][..]),
            ("kgx", &["--working-directory", dir.as_str()][..]),
            ("xfce4-terminal", &["--working-directory", dir.as_str()][..]),
            ("tilix", &["--working-directory", dir.as_str()][..]),
            ("alacritty", &["--working-directory", dir.as_str()][..]),
            ("kitty", &["--directory", dir.as_str()][..]),
            ("wezterm", &["start", "--cwd", dir.as_str()][..]),
            ("foot", &["--working-directory", dir.as_str()][..]),
            ("xterm", &[][..]),
        ] {
            if Command::new(cmd).args(args).current_dir(&dir).spawn().is_ok() {
                return;
            }
        }
    }
}

/// Remove a path: to the system trash / recycle bin, or for good. The kind
/// is re-checked right before acting so a path that changed type is refused.
fn remove_path(path: &str, is_dir: bool, to_trash: bool) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if is_dir != md.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "path changed type on disk — refusing",
        ));
    }
    if to_trash {
        return trash::delete(path).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("could not move to trash: {}", e))
        });
    }
    if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// (items in the trash, whether counting/emptying is supported here)
fn trash_info() -> (u64, bool) {
    #[cfg(any(target_os = "linux", windows))]
    {
        match trash::os_limited::list() {
            Ok(items) => (items.len() as u64, true),
            Err(_) => (0, true),
        }
    }
    #[cfg(target_os = "macos")]
    {
        // no listing API on macOS; Finder empties it for us
        let n = std::env::var("HOME")
            .ok()
            .and_then(|h| std::fs::read_dir(format!("{}/.Trash", h)).ok())
            .map(|rd| rd.count() as u64)
            .unwrap_or(0);
        (n, true)
    }
}

fn empty_trash() -> Result<u64, String> {
    #[cfg(any(target_os = "linux", windows))]
    {
        let items = trash::os_limited::list().map_err(|e| e.to_string())?;
        let n = items.len() as u64;
        trash::os_limited::purge_all(items).map_err(|e| e.to_string())?;
        Ok(n)
    }
    #[cfg(target_os = "macos")]
    {
        let (n, _) = trash_info();
        let st = Command::new("osascript")
            .args(["-e", "tell application \"Finder\" to empty trash"])
            .status()
            .map_err(|e| e.to_string())?;
        if st.success() {
            Ok(n)
        } else {
            Err("Finder refused to empty the trash".into())
        }
    }
}

/// Open an external page in the user's normal browser tab (About links).
fn open_link(url: &str) {
    #[cfg(windows)]
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    {
        if Command::new("xdg-open").arg(url).spawn().is_ok() {
            return;
        }
        for b in ["sensible-browser", "x-www-browser", "firefox", "google-chrome", "chromium"] {
            if Command::new(b).arg(url).spawn().is_ok() {
                return;
            }
        }
    }
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    {
        // App-mode window (no tabs, no URL bar) when a Chromium-family
        // browser exists; plain browser tab otherwise.
        for b in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "brave-browser",
            "microsoft-edge",
        ] {
            if Command::new(b)
                .arg(format!("--app={url}"))
                .arg("--new-window")
                .spawn()
                .is_ok()
            {
                return;
            }
        }
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

fn url_decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() + 0 && i + 2 <= b.len() - 1 => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(c) => {
                        out.push(c);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn safe_name(p: &str) -> String {
    let s: String = p
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "root".into() } else { s.chars().take(60).collect() }
}

/// Disk total/free behind a store: the volume for a single scan, the sum of
/// all volumes for an all-volumes scan.
pub fn store_disk_usage(s: &Store) -> (u64, u64) {
    if s.multi {
        s.volumes.iter().fold((0, 0), |a, v| (a.0 + v.total, a.1 + v.free))
    } else {
        disk_usage(&s.root_path)
    }
}

/// WinDirStat-style pseudo blocks for a volume root: free space and the
/// "unknown" remainder (used on disk but not in the tree: MFT, unreadable
/// folders, files excluded from this scan).
pub fn treemap_extras(s: &Store, id: u32, show_free: bool, show_unknown: bool) -> Vec<(String, u64, u8, u8)> {
    let mut out = Vec::new();
    if !show_free && !show_unknown {
        return out;
    }
    let Some(v) = scan::volume_of(s, id) else { return out };
    let used = v.total.saturating_sub(v.free);
    let unknown = used.saturating_sub(s.nodes[id as usize].size);
    if show_free && v.free > 0 {
        out.push(("Free space".to_string(), v.free, treemap::SLOT_FREE, 3));
    }
    if show_unknown && unknown > 0 {
        out.push(("Unknown".to_string(), unknown, treemap::SLOT_UNKNOWN, 4));
    }
    out
}

fn query_u64(url: &str, key: &str) -> Option<u64> {
    let q = url.split_once('?')?.1;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return v.parse().ok();
            }
        }
    }
    None
}

fn json_response<T: Serialize>(v: &T) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body).with_header(
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
    )
}

fn err_response(code: u16, msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::json!({ "error": msg });
    Response::from_data(serde_json::to_vec(&body).unwrap())
        .with_status_code(code)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
}

fn start_scan(app: Arc<App>, target: ScanTarget) {
    app.prog.reset();
    app.prog.scanning.store(true, Ordering::SeqCst);
    *app.last_target.lock().unwrap() = Some(target.clone());
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let opts = app.opts.read().unwrap().clone();
        let (result, root_path) = match &target {
            ScanTarget::Path(path) => (
                scan::scan_root(path, &app.prog, &opts),
                path.to_string_lossy().into_owned(),
            ),
            ScanTarget::AllVolumes => (
                scan::scan_all(&all_volumes(), &app.prog, &opts),
                scan::ALL_VOLUMES.to_string(),
            ),
        };
        let elapsed = t0.elapsed().as_millis() as u64;
        if let Some(r) = result {
            let generation = app.generation.fetch_add(1, Ordering::SeqCst) + 1;
            let store = scan::flatten(
                r.root,
                scan::FlattenMeta {
                    root_path,
                    elapsed_ms: elapsed,
                    generation,
                    errors: app.prog.errors.load(Ordering::Relaxed),
                    skipped: app.prog.skipped.load(Ordering::Relaxed),
                    multi: r.multi,
                    is_volume: r.is_volume,
                    volumes: r.volumes,
                },
            );
            *app.store.write().unwrap() = Some(store);
        }
        app.prog.scanning.store(false, Ordering::SeqCst);
    });
}

/// Rescan one folder in place (WinDirStat "Refresh selected"): scans just that
/// path, splices it into the existing tree and publishes a new generation.
/// Returns the refreshed node's id in the new generation.
fn rescan_subtree(app: Arc<App>, id: u32, gen: u64) -> Result<(), String> {
    let (path, is_multi_vol) = {
        let store = app.store.read().unwrap();
        let s = store.as_ref().ok_or("no scan yet")?;
        if s.generation != gen {
            return Err("scan changed — refresh first".into());
        }
        if id as usize >= s.nodes.len() || !s.nodes[id as usize].alive || !s.nodes[id as usize].is_dir {
            return Err("not a live folder".into());
        }
        if s.multi && id == 0 {
            return Err("use Rescan for all volumes".into());
        }
        (scan::path_of(s, id), s.multi && s.nodes[id as usize].parent == 0)
    };
    if app.prog.scanning.swap(true, Ordering::SeqCst) {
        return Err("scan already running".into());
    }
    app.prog.reset();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let opts = app.opts.read().unwrap().clone();
        let p = PathBuf::from(&path);
        let fresh = scan::scan_root(&p, &app.prog, &opts);
        let elapsed = t0.elapsed().as_millis() as u64;
        if let Some(r) = fresh {
            let mut guard = app.store.write().unwrap();
            if let Some(s) = guard.as_ref() {
                if s.generation == gen && (id as usize) < s.nodes.len() {
                    let generation = app.generation.fetch_add(1, Ordering::SeqCst) + 1;
                    let mut root = r.root;
                    if is_multi_vol {
                        root.name = path.clone();
                    }
                    let prev_elapsed = s.elapsed_ms;
                    let (new, _nid) = scan::replace_subtree(
                        s,
                        id,
                        root,
                        generation,
                        prev_elapsed.max(elapsed),
                        s.errors + app.prog.errors.load(Ordering::Relaxed),
                        s.skipped + app.prog.skipped.load(Ordering::Relaxed),
                    );
                    *guard = Some(new);
                }
            }
        }
        app.prog.scanning.store(false, Ordering::SeqCst);
    });
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Size,
    Name,
    Items,
    Date,
}

/// Children of `id` that are alive, ordered by `key` (`desc` = biggest /
/// newest / z→a first; name default is a→z).
pub fn sorted_children(store: &Store, id: u32, key: SortKey, desc: bool) -> Vec<u32> {
    let n = &store.nodes[id as usize];
    let mut kids: Vec<u32> = (n.first_child..n.first_child + n.child_count)
        .filter(|c| store.nodes[*c as usize].alive)
        .collect();
    let nd = &store.nodes;
    match key {
        SortKey::Size => kids.sort_unstable_by(|a, b| nd[*b as usize].size.cmp(&nd[*a as usize].size)),
        SortKey::Items => kids.sort_unstable_by(|a, b| {
            let ia = nd[*a as usize].files as u64 + nd[*a as usize].subdirs as u64;
            let ib = nd[*b as usize].files as u64 + nd[*b as usize].subdirs as u64;
            ib.cmp(&ia).then(nd[*b as usize].size.cmp(&nd[*a as usize].size))
        }),
        SortKey::Date => kids.sort_unstable_by(|a, b| {
            nd[*b as usize].mtime.cmp(&nd[*a as usize].mtime).then(nd[*b as usize].size.cmp(&nd[*a as usize].size))
        }),
        SortKey::Name => kids.sort_unstable_by(|a, b| {
            // folders first, then case-insensitive name
            nd[*b as usize]
                .is_dir
                .cmp(&nd[*a as usize].is_dir)
                .then_with(|| nd[*a as usize].name.to_lowercase().cmp(&nd[*b as usize].name.to_lowercase()))
        }),
    }
    // name sorts a→z by default ("asc"); the others biggest/newest first ("desc")
    let natural = key == SortKey::Name;
    if desc == natural {
        kids.reverse();
    }
    kids
}

fn parse_sort(url: &str) -> (SortKey, bool) {
    let q = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut key = SortKey::Size;
    let mut desc = true;
    for pair in q.split('&') {
        match pair.split_once('=') {
            Some(("sort", v)) => {
                key = match v {
                    "name" => SortKey::Name,
                    "items" => SortKey::Items,
                    "date" => SortKey::Date,
                    _ => SortKey::Size,
                }
            }
            Some(("dir", v)) => desc = v != "asc",
            _ => {}
        }
    }
    (key, desc)
}

fn node_resp(store: &Store, id: u32, key: SortKey, desc: bool) -> NodeResp {
    let n = &store.nodes[id as usize];
    let mut kids = sorted_children(store, id, key, desc);
    const CAP: usize = 1500;
    let more = kids.len().saturating_sub(CAP) as u32;
    kids.truncate(CAP);
    let children = kids
        .iter()
        .map(|c| {
            let k = &store.nodes[*c as usize];
            ChildResp {
                id: *c,
                name: k.name.to_string(),
                size: k.size,
                files: k.files,
                subdirs: k.subdirs,
                mtime: k.mtime,
                dir: k.is_dir,
                slot: if k.is_dir { 254 } else { store.exts[k.ext as usize].slot },
            }
        })
        .collect();
    let ancestors = scan::ancestors_of(store, id);
    let ancestor_names = ancestors
        .iter()
        .map(|a| store.nodes[*a as usize].name.to_string())
        .collect();
    NodeResp {
        id,
        name: n.name.to_string(),
        path: scan::path_of(store, id),
        size: n.size,
        files: n.files,
        subdirs: n.subdirs,
        mtime: n.mtime,
        dir: n.is_dir,
        protected: scan::is_protected(store, id),
        parent_size: if id == 0 { n.size } else { store.nodes[n.parent as usize].size },
        total: store.nodes[0].size,
        generation: store.generation,
        ancestors,
        ancestor_names,
        children,
        more,
    }
}

fn handle(app: &Arc<App>, mut req: tiny_http::Request) {
    let url = req.url().to_string();
    let path = url.split_once('?').map(|(p, _)| p).unwrap_or(&url).to_string();
    let method = req.method().clone();

    let mut body = String::new();
    if method == Method::Post {
        let token_ok = req
            .headers()
            .iter()
            .any(|h| h.field.equiv("X-Diskhoji-Token") && h.value.as_str() == app.token);
        if !token_ok {
            let _ = req.respond(err_response(403, "missing or bad X-Diskhoji-Token"));
            return;
        }
        let _ = req.as_reader().take(1 << 20).read_to_string(&mut body);
    }

    let resp = route(app, &method, &path, &url, &body);
    match resp {
        Ok(r) => {
            let _ = req.respond(r);
        }
        Err((code, msg)) => {
            let _ = req.respond(err_response(code, &msg));
        }
    }
}

type Reply = Result<Response<std::io::Cursor<Vec<u8>>>, (u16, String)>;

fn route(app: &Arc<App>, method: &Method, path: &str, url: &str, body: &str) -> Reply {
    match (method, path) {
        (Method::Get, "/") => Ok(Response::from_data(
            UI.replace("__DK_TOKEN__", &app.token)
                .replace(
                    "__DK_ELEVATED__",
                    if elevate::is_elevated() { "1" } else { "0" },
                )
                .into_bytes(),
        )
            .with_header(
                Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                    .unwrap(),
            )
            .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap())),

        (Method::Get, "/api/status") => {
            let store = app.store.read().unwrap();
            let scanning = app.prog.scanning.load(Ordering::SeqCst);
            let state = if scanning {
                "scanning"
            } else if store.is_some() {
                "ready"
            } else {
                "idle"
            };
            Ok(json_response(&StatusResp {
                state,
                files: app.prog.files.load(Ordering::Relaxed),
                dirs: app.prog.dirs.load(Ordering::Relaxed),
                bytes: app.prog.bytes.load(Ordering::Relaxed),
                errors: app.prog.errors.load(Ordering::Relaxed),
                skipped: app.prog.skipped.load(Ordering::Relaxed),
                current: app.prog.current.lock().unwrap().clone(),
                generation: store.as_ref().map(|s| s.generation).unwrap_or(0),
                root: store.as_ref().map(|s| s.root_path.clone()),
            }))
        }

        (Method::Get, "/api/roots") => Ok(json_response(&list_roots())),

        (Method::Post, "/api/scan") => {
            let r: ScanReq = serde_json::from_str(body).map_err(|_| (400, "bad json".to_string()))?;
            let target = if r.all || r.path == scan::ALL_VOLUMES {
                ScanTarget::AllVolumes
            } else {
                let p = PathBuf::from(&r.path);
                if !p.is_dir() {
                    return Err((400, format!("not a directory: {}", r.path)));
                }
                ScanTarget::Path(p)
            };
            if app.prog.scanning.load(Ordering::SeqCst) {
                return Err((409, "scan already running".to_string()));
            }
            start_scan(app.clone(), target);
            Ok(json_response(&serde_json::json!({ "ok": true })))
        }

        (Method::Get, "/api/options") => {
            let o = app.opts.read().unwrap().clone();
            Ok(json_response(&o))
        }

        (Method::Post, "/api/options") => {
            let r: OptsReq = serde_json::from_str(body).map_err(|_| (400, "bad json".to_string()))?;
            let o = ScanOptions {
                skip_hidden: r.skip_hidden,
                excludes: r.excludes.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            };
            save_opts(&o);
            *app.opts.write().unwrap() = o.clone();
            Ok(json_response(&o))
        }

        // Rescan one folder in place
        (Method::Post, "/api/rescan") => {
            let r: IdReq = serde_json::from_str(body).map_err(|_| (400, "bad json".to_string()))?;
            let gen = r.generation.ok_or((400, "generation required".to_string()))?;
            rescan_subtree(app.clone(), r.id, gen).map_err(|e| (409, e))?;
            Ok(json_response(&serde_json::json!({ "ok": true })))
        }

        (Method::Get, "/api/search") => {
            let q = url
                .split_once('?')
                .map(|(_, q)| q)
                .unwrap_or("")
                .split('&')
                .find_map(|p| p.strip_prefix("q="))
                .map(|v| url_decode(v))
                .unwrap_or_default();
            let under = query_u64(url, "under").unwrap_or(0) as u32;
            let limit = query_u64(url, "limit").unwrap_or(500).min(5000) as usize;
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            if under as usize >= s.nodes.len() {
                return Err((404, "no such node".to_string()));
            }
            let hits: Vec<SearchHit> = scan::search(s, under, &q, limit)
                .into_iter()
                .map(|id| {
                    let n = &s.nodes[id as usize];
                    SearchHit {
                        id,
                        name: n.name.to_string(),
                        path: scan::path_of(s, id),
                        size: n.size,
                        mtime: n.mtime,
                        dir: n.is_dir,
                        slot: if n.is_dir { 254 } else { s.exts[n.ext as usize].slot },
                    }
                })
                .collect();
            Ok(json_response(&serde_json::json!({ "generation": s.generation, "hits": hits })))
        }

        (Method::Get, "/api/find") => {
            let path = url
                .split_once('?')
                .map(|(_, q)| q)
                .unwrap_or("")
                .split('&')
                .find_map(|p| p.strip_prefix("path="))
                .map(|v| url_decode(v))
                .unwrap_or_default();
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            let id = scan::find_by_path(s, &path).ok_or((404, "not found".to_string()))?;
            Ok(json_response(&serde_json::json!({ "id": id, "generation": s.generation })))
        }

        (Method::Get, "/api/export") => {
            let id = query_u64(url, "id").unwrap_or(0) as u32;
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            if id as usize >= s.nodes.len() || !s.nodes[id as usize].alive {
                return Err((404, "no such node".to_string()));
            }
            let mut buf: Vec<u8> = Vec::new();
            scan::export_csv(s, id, &mut buf).map_err(|e| (500, e.to_string()))?;
            let fname = format!("diskhoji-{}.csv", safe_name(&scan::path_of(s, id)));
            Ok(Response::from_data(buf)
                .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/csv; charset=utf-8"[..]).unwrap())
                .with_header(
                    Header::from_bytes(
                        &b"Content-Disposition"[..],
                        format!("attachment; filename=\"{}\"", fname).as_bytes(),
                    )
                    .unwrap(),
                ))
        }

        (Method::Get, "/api/trash") => {
            let (count, supported) = trash_info();
            Ok(json_response(&serde_json::json!({ "count": count, "supported": supported })))
        }

        (Method::Post, "/api/trash/empty") => {
            let n = empty_trash().map_err(|e| (500, e))?;
            Ok(json_response(&serde_json::json!({ "ok": true, "count": n })))
        }

        (Method::Post, "/api/terminal") => {
            let r: IdReq = serde_json::from_str(body).map_err(|_| (400, "bad json".to_string()))?;
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            if r.id as usize >= s.nodes.len() {
                return Err((404, "no such node".to_string()));
            }
            let p = scan::path_of(s, r.id);
            drop(store);
            open_terminal(&p);
            Ok(json_response(&serde_json::json!({ "ok": true })))
        }

        (Method::Post, "/api/cancel") => {
            app.prog.cancel.store(true, Ordering::SeqCst);
            Ok(json_response(&serde_json::json!({ "ok": true })))
        }

        // Open the OS folder picker — same dialog the native app's Browse uses.
        // Safe here because --web only ever binds 127.0.0.1: the browser and
        // this server are the same machine, so the dialog appears to the same
        // person who clicked. Blocks one worker thread; the pool has 4, and a
        // second click while open gets a 409 instead of a second dialog.
        (Method::Post, "/api/pick") => {
            static PICKING: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if PICKING.swap(true, Ordering::SeqCst) {
                return Err((409, "picker already open".to_string()));
            }
            let picked = native::pick_folder_blocking();
            PICKING.store(false, Ordering::SeqCst);
            Ok(json_response(&serde_json::json!({ "path": picked })))
        }

        (Method::Get, "/api/summary") => {
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            let (disk_total, disk_free) = store_disk_usage(s);
            let mut ext_ids: Vec<usize> = (0..s.exts.len()).filter(|e| s.exts[*e].bytes > 0).collect();
            ext_ids.sort_unstable_by(|a, b| s.exts[*b].bytes.cmp(&s.exts[*a].bytes));
            ext_ids.truncate(40);
            let exts = ext_ids
                .iter()
                .map(|e| ExtResp {
                    ext: s.exts[*e].name.clone(),
                    desc: scan::ext_description(&s.exts[*e].name),
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
                    BigFileResp {
                        id: *id,
                        name: n.name.to_string(),
                        path: scan::path_of(s, *id),
                        size: n.size,
                        slot: s.exts[n.ext as usize].slot,
                    }
                })
                .collect();
            Ok(json_response(&SummaryResp {
                root: s.root_path.clone(),
                bytes: s.nodes[0].size,
                files: s.nodes[0].files as u64,
                dirs: s.dirs,
                errors: s.errors,
                elapsed_ms: s.elapsed_ms,
                generation: s.generation,
                disk_total,
                disk_free,
                skipped: s.skipped,
                multi: s.multi,
                is_volume: s.is_volume,
                volumes: s.volumes.clone(),
                exts,
                largest,
            }))
        }

        (Method::Get, p) if p.starts_with("/api/node/") => {
            let id: u32 = p[10..].parse().map_err(|_| (400, "bad id".to_string()))?;
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            if id as usize >= s.nodes.len() || !s.nodes[id as usize].alive {
                return Err((404, "no such node".to_string()));
            }
            let (key, desc) = parse_sort(url);
            Ok(json_response(&node_resp(s, id, key, desc)))
        }

        (Method::Get, "/api/treemap") => {
            let id = query_u64(url, "id").unwrap_or(0) as u32;
            let w = query_u64(url, "w").unwrap_or(800) as f64;
            let h = query_u64(url, "h").unwrap_or(600) as f64;
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            if id as usize >= s.nodes.len() || !s.nodes[id as usize].alive {
                return Err((404, "no such node".to_string()));
            }
            let extras = treemap_extras(
                s,
                id,
                query_u64(url, "free").unwrap_or(0) != 0,
                query_u64(url, "unknown").unwrap_or(0) != 0,
            );
            let (rects, dirs) =
                treemap::layout(s, id, w.clamp(10.0, 10_000.0), h.clamp(10.0, 10_000.0), &extras);
            Ok(json_response(&TreemapResp {
                generation: s.generation,
                id,
                size: s.nodes[id as usize].size,
                rects,
                dirs,
            }))
        }

        (Method::Post, "/api/delete") => {
            let r: IdReq = serde_json::from_str(body).map_err(|_| (400, "bad json".to_string()))?;
            let to_trash = r.mode.as_deref() != Some("permanent");
            // validate under a read lock; ids are arena indices reused each
            // scan, so the client must prove it's talking about this store
            let (target, is_dir, gen) = {
                let store = app.store.read().unwrap();
                let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
                if r.id as usize >= s.nodes.len() || !s.nodes[r.id as usize].alive {
                    return Err((404, "no such node".to_string()));
                }
                if scan::is_protected(s, r.id) {
                    return Err((400, "refusing to delete the scan root".to_string()));
                }
                match r.generation {
                    Some(g) if g != s.generation => {
                        return Err((409, "scan changed — refresh before deleting".to_string()));
                    }
                    None => {
                        return Err((400, "generation required".to_string()));
                    }
                    _ => {}
                }
                (scan::path_of(s, r.id), s.nodes[r.id as usize].is_dir, s.generation)
            };
            // filesystem work happens with no lock held
            remove_path(&target, is_dir, to_trash).map_err(|e| (500, format!("delete failed: {}", e)))?;
            // fix up the model only if it still describes the same generation
            let mut store = app.store.write().unwrap();
            let (freed, files) = match store.as_mut() {
                Some(s)
                    if s.generation == gen
                        && (r.id as usize) < s.nodes.len()
                        && s.nodes[r.id as usize].alive =>
                {
                    scan::remove_subtree(s, r.id)
                }
                _ => (0, 0),
            };
            Ok(json_response(&serde_json::json!({
                "ok": true, "freed": freed, "files": files, "path": target, "trashed": to_trash
            })))
        }

        (Method::Post, "/api/reveal") => {
            let r: IdReq = serde_json::from_str(body).map_err(|_| (400, "bad json".to_string()))?;
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            if r.id as usize >= s.nodes.len() {
                return Err((404, "no such node".to_string()));
            }
            let p = scan::path_of(s, r.id);
            drop(store);
            reveal_in_file_manager(&p);
            Ok(json_response(&serde_json::json!({ "ok": true })))
        }

        (Method::Post, "/api/open") => {
            let r: IdReq = serde_json::from_str(body).map_err(|_| (400, "bad json".to_string()))?;
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            if r.id as usize >= s.nodes.len() {
                return Err((404, "no such node".to_string()));
            }
            let p = scan::path_of(s, r.id);
            drop(store);
            open_with_default(&p);
            Ok(json_response(&serde_json::json!({ "ok": true })))
        }

        _ => Err((404, "not found".to_string())),
    }
}

fn main() {
    let mut port: u16 = 5717;
    let mut no_open = false;
    let mut web = false;
    let mut scan_path: Option<PathBuf> = None;
    let mut takeover: Option<u32> = None;
    let mut scan_all = false;
    let mut opts = load_opts();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--all" => scan_all = true,
            "--skip-hidden" => opts.skip_hidden = true,
            "--exclude" | "-x" => {
                if let Some(p) = args.next() {
                    opts.excludes.extend(ScanOptions::parse_excludes(&p));
                }
            }
            "--port" => {
                if let Some(p) = args.next().and_then(|v| v.parse().ok()) {
                    port = p;
                }
            }
            "--no-open" => no_open = true,
            "--web" => web = true,
            // handed to the elevated relaunch: retire the pre-elevation
            // instance so exactly one window survives the hand-off — but only
            // once THIS instance's window is actually up (see native::run),
            // so a failed elevated start never kills the working window
            "--takeover" => takeover = args.next().and_then(|v| v.parse::<u32>().ok()),
            "--help" | "-h" => {
                println!(
                    "diskhoji [PATH | --all] [--web] [--port N] [--no-open] [--skip-hidden] [--exclude PATTERN]\n\n\
                     Opens the native window and scans PATH (or pick a volume inside).\n\
                     --all          scan every local volume together (WinDirStat's \"all local drives\")\n\
                     --skip-hidden  leave out hidden files and folders\n\
                     --exclude P    leave out names matching P (repeatable; * and ? wildcards,\n\
                                    comma-separated list accepted, e.g. \"node_modules,*.iso\")\n\
                     --web          serve the dashboard on localhost for a browser instead."
                );
                return;
            }
            p => scan_path = Some(PathBuf::from(p)),
        }
    }

    rayon::ThreadPoolBuilder::new()
        .stack_size(8 * 1024 * 1024)
        .build_global()
        .ok();

    let app = Arc::new(App {
        store: RwLock::new(None),
        prog: Progress::new(),
        generation: std::sync::atomic::AtomicU64::new(0),
        token: gen_token(),
        opts: RwLock::new(opts),
        last_target: std::sync::Mutex::new(None),
    });

    let initial = if scan_all {
        Some(ScanTarget::AllVolumes)
    } else {
        scan_path.map(ScanTarget::Path)
    };

    if !web {
        native::run(app, initial, takeover);
        return;
    }
    if let Some(pid) = takeover {
        elevate::takeover(pid);
    }

    match initial {
        Some(ScanTarget::Path(p)) if !p.is_dir() => {
            eprintln!("warning: {} is not a directory, skipping initial scan", p.display());
        }
        Some(t) => start_scan(app.clone(), t),
        None => {}
    }

    let server = Server::http(("127.0.0.1", port))
        .or_else(|_| Server::http(("127.0.0.1", 0)))
        .expect("cannot bind to localhost");
    let addr = server.server_addr();
    let url = format!("http://{}", addr);
    println!("▦ diskhoji — {}", url);
    println!("  scanning stays on one filesystem; nothing leaves this machine.");
    if !elevate::is_elevated() {
        println!("  running unelevated — some system folders may be unreadable; relaunch with sudo (or an administrator shell) to measure everything.");
    }

    if !no_open {
        open_browser(&url);
    }

    let server = Arc::new(server);
    let mut handles = Vec::new();
    for _ in 0..4 {
        let server = server.clone();
        let app = app.clone();
        handles.push(std::thread::spawn(move || loop {
            match server.recv() {
                Ok(req) => handle(&app, req),
                Err(_) => break,
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}
