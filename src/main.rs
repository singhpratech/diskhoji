#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod native;
mod scan;
mod treemap;
mod voyage;

use scan::{Progress, Store};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
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
    dir: bool,
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
    bytes: u64,
    files: u64,
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
    path: String,
}

#[derive(Deserialize)]
struct IdReq {
    id: u32,
    #[serde(default)]
    generation: Option<u64>,
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

fn start_scan(app: Arc<App>, path: PathBuf) {
    app.prog.reset();
    app.prog.scanning.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let result = scan::scan_root(&path, &app.prog);
        let elapsed = t0.elapsed().as_millis() as u64;
        if let Some(root) = result {
            let generation = app.generation.fetch_add(1, Ordering::SeqCst) + 1;
            let errors = app.prog.errors.load(Ordering::Relaxed);
            let store = scan::flatten(
                root,
                path.to_string_lossy().into_owned(),
                elapsed,
                generation,
                errors,
            );
            *app.store.write().unwrap() = Some(store);
        }
        app.prog.scanning.store(false, Ordering::SeqCst);
    });
}

fn node_resp(store: &Store, id: u32) -> NodeResp {
    let n = &store.nodes[id as usize];
    let first = n.first_child;
    let count = n.child_count;
    let mut kids: Vec<u32> = (first..first + count)
        .filter(|c| {
            let k = &store.nodes[*c as usize];
            k.alive
        })
        .collect();
    kids.sort_unstable_by(|a, b| {
        store.nodes[*b as usize].size.cmp(&store.nodes[*a as usize].size)
    });
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
        dir: n.is_dir,
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
            UI.replace("__DK_TOKEN__", &app.token).into_bytes(),
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
                current: app.prog.current.lock().unwrap().clone(),
                generation: store.as_ref().map(|s| s.generation).unwrap_or(0),
                root: store.as_ref().map(|s| s.root_path.clone()),
            }))
        }

        (Method::Get, "/api/roots") => Ok(json_response(&list_roots())),

        (Method::Post, "/api/scan") => {
            let r: ScanReq = serde_json::from_str(body).map_err(|_| (400, "bad json".to_string()))?;
            let p = PathBuf::from(&r.path);
            if !p.is_dir() {
                return Err((400, format!("not a directory: {}", r.path)));
            }
            if app.prog.scanning.load(Ordering::SeqCst) {
                return Err((409, "scan already running".to_string()));
            }
            start_scan(app.clone(), p);
            Ok(json_response(&serde_json::json!({ "ok": true })))
        }

        (Method::Post, "/api/cancel") => {
            app.prog.cancel.store(true, Ordering::SeqCst);
            Ok(json_response(&serde_json::json!({ "ok": true })))
        }

        (Method::Get, "/api/summary") => {
            let store = app.store.read().unwrap();
            let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
            let (disk_total, disk_free) = disk_usage(&s.root_path);
            let mut ext_ids: Vec<usize> = (0..s.exts.len()).filter(|e| s.exts[*e].bytes > 0).collect();
            ext_ids.sort_unstable_by(|a, b| s.exts[*b].bytes.cmp(&s.exts[*a].bytes));
            ext_ids.truncate(14);
            let exts = ext_ids
                .iter()
                .map(|e| ExtResp {
                    ext: s.exts[*e].name.clone(),
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
            Ok(json_response(&node_resp(s, id)))
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
            let (rects, dirs) = treemap::layout(s, id, w.clamp(10.0, 10_000.0), h.clamp(10.0, 10_000.0));
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
            if r.id == 0 {
                return Err((400, "refusing to delete the scan root".to_string()));
            }
            // validate under a read lock; ids are arena indices reused each
            // scan, so the client must prove it's talking about this store
            let (target, is_dir, gen) = {
                let store = app.store.read().unwrap();
                let s = store.as_ref().ok_or((404, "no scan yet".to_string()))?;
                if r.id as usize >= s.nodes.len() || !s.nodes[r.id as usize].alive {
                    return Err((404, "no such node".to_string()));
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
            let meta = std::fs::symlink_metadata(&target)
                .map_err(|e| (500, format!("cannot stat {}: {}", target, e)))?;
            let result = if is_dir && meta.is_dir() {
                std::fs::remove_dir_all(&target)
            } else if !is_dir && !meta.is_dir() {
                std::fs::remove_file(&target)
            } else {
                return Err((409, "path changed type on disk — refusing".to_string()));
            };
            result.map_err(|e| (500, format!("delete failed: {}", e)))?;
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
                "ok": true, "freed": freed, "files": files, "path": target
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
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                if let Some(p) = args.next().and_then(|v| v.parse().ok()) {
                    port = p;
                }
            }
            "--no-open" => no_open = true,
            "--web" => web = true,
            "--help" | "-h" => {
                println!("diskhoji [PATH] [--web] [--port N] [--no-open]\n\nOpens the native window and scans PATH (or pick a volume inside).\n--web serves the dashboard on localhost for a browser instead.");
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
    });

    if !web {
        native::run(app, scan_path);
        return;
    }

    if let Some(p) = scan_path {
        if p.is_dir() {
            start_scan(app.clone(), p);
        } else {
            eprintln!("warning: {} is not a directory, skipping initial scan", p.display());
        }
    }

    let server = Server::http(("127.0.0.1", port))
        .or_else(|_| Server::http(("127.0.0.1", 0)))
        .expect("cannot bind to localhost");
    let addr = server.server_addr();
    let url = format!("http://{}", addr);
    println!("▦ diskhoji — {}", url);
    println!("  scanning stays on one filesystem; nothing leaves this machine.");

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
