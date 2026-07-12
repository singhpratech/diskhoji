use rayon::prelude::*;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

pub const EXT_DIR: u16 = u16::MAX;

pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub skipped: AtomicU64,
    pub err_paths: Mutex<Vec<String>>,
    pub scanning: AtomicBool,
    pub cancel: AtomicBool,
    pub current: Mutex<String>,
}

impl Progress {
    pub fn new() -> Self {
        Progress {
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            err_paths: Mutex::new(Vec::new()),
            scanning: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            current: Mutex::new(String::new()),
        }
    }
    pub fn reset(&self) {
        self.files.store(0, Ordering::Relaxed);
        self.dirs.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        self.skipped.store(0, Ordering::Relaxed);
        self.err_paths.lock().map(|mut v| v.clear()).ok();
        self.cancel.store(false, Ordering::Relaxed);
        self.current.lock().unwrap().clear();
    }
}

/// User-chosen scan filters. Persisted as JSON in the config dir so the
/// native window and `--web` share one set.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ScanOptions {
    /// skip dot-files on Unix / FILE_ATTRIBUTE_HIDDEN entries on Windows
    #[serde(default)]
    pub skip_hidden: bool,
    /// name patterns to leave out: `node_modules`, `*.iso`, `Cache*` — matched
    /// against each entry's file name, `*` and `?` wildcards, case-insensitive
    #[serde(default)]
    pub excludes: Vec<String>,
}

impl ScanOptions {
    pub fn parse_excludes(text: &str) -> Vec<String> {
        text.split(|c: char| c == ',' || c == '\n' || c == ';')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }
    pub fn is_active(&self) -> bool {
        self.skip_hidden || !self.excludes.is_empty()
    }
}

/// Case-insensitive glob with `*` (any run) and `?` (one char).
pub fn glob_match(pat: &str, name: &str) -> bool {
    fn rec(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => {
                let rest = &p[1..];
                if rest.is_empty() {
                    return true;
                }
                (0..=n.len()).any(|i| rec(rest, &n[i..]))
            }
            Some('?') => !n.is_empty() && rec(&p[1..], &n[1..]),
            Some(c) => {
                !n.is_empty()
                    && c.to_lowercase().eq(n[0].to_lowercase())
                    && rec(&p[1..], &n[1..])
            }
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    rec(&p, &n)
}

struct Filter<'a> {
    opts: &'a ScanOptions,
}

impl<'a> Filter<'a> {
    fn excluded(&self, name: &str, md: Option<&fs::Metadata>) -> bool {
        if self.opts.skip_hidden && is_hidden(name, md) {
            return true;
        }
        self.opts.excludes.iter().any(|p| glob_match(p, name))
    }
}

#[cfg(windows)]
fn is_hidden(name: &str, md: Option<&fs::Metadata>) -> bool {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2;
    md.map(|m| m.file_attributes() & HIDDEN != 0).unwrap_or(false) || name.starts_with('.')
}
#[cfg(not(windows))]
fn is_hidden(name: &str, _md: Option<&fs::Metadata>) -> bool {
    name.starts_with('.')
}

pub struct LocalNode {
    pub name: String,
    pub size: u64,
    pub files: u32,
    pub is_dir: bool,
    /// last modification, seconds since the Unix epoch; for a directory the
    /// newest change anywhere inside it (WinDirStat's "last change")
    pub mtime: i64,
    /// directories anywhere inside (0 for a file)
    pub subdirs: u32,
    pub children: Vec<LocalNode>,
}

pub struct Node {
    pub name: Box<str>,
    pub size: u64,
    pub parent: u32,
    pub first_child: u32,
    pub child_count: u32,
    pub files: u32,
    pub ext: u16,
    pub is_dir: bool,
    pub alive: bool,
    pub mtime: i64,
    pub subdirs: u32,
}

pub struct ExtStat {
    pub name: String,
    pub bytes: u64,
    pub files: u64,
    pub slot: u8,
}

/// One mounted volume inside a scan (exactly one for a normal scan; one per
/// drive for "all volumes").
#[derive(Clone, serde::Serialize)]
pub struct VolInfo {
    pub path: String,
    pub total: u64,
    pub free: u64,
    /// node id that holds this volume's tree (0 for a single-root scan)
    pub node: u32,
}

pub struct Store {
    pub nodes: Vec<Node>,
    pub exts: Vec<ExtStat>,
    pub root_path: String,
    pub elapsed_ms: u64,
    pub generation: u64,
    pub largest: Vec<u32>,
    pub dirs: u64,
    pub errors: u64,
    pub skipped: u64,
    /// true when the root is a synthetic "all volumes" node whose children
    /// are absolute mount paths
    pub multi: bool,
    pub volumes: Vec<VolInfo>,
    /// the scan root is itself a mount point, so disk used/free describe
    /// exactly this tree and free/unknown blocks make sense in the map
    pub is_volume: bool,
}

pub const ALL_VOLUMES: &str = "All volumes";

fn note_err(prog: &Progress, path: &Path) {
    prog.errors.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut v) = prog.err_paths.lock() {
        if v.len() < 50 {
            v.push(path.display().to_string());
        }
    }
}

#[cfg(unix)]
fn dev_of(md: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.dev()
}
#[cfg(not(unix))]
fn dev_of(_md: &fs::Metadata) -> u64 {
    0
}

fn mtime_of(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => Some(d.as_secs() as i64),
            Err(e) => Some(-(e.duration().as_secs() as i64)),
        })
        .unwrap_or(0)
}

/// Is `path` the top of its filesystem (a mount point / drive root)?
pub fn is_mount_point(path: &Path) -> bool {
    #[cfg(unix)]
    {
        let Ok(md) = fs::metadata(path) else { return false };
        match path.parent() {
            None => true,
            Some(p) => match fs::metadata(p) {
                Ok(pm) => dev_of(&pm) != dev_of(&md) || p == path,
                Err(_) => false,
            },
        }
    }
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        let s = s.trim_end_matches(['\\', '/']);
        s.len() == 2 && s.ends_with(':')
    }
}
/// Hardlinked files share one copy of their bytes on disk; only the first
/// sighting of a (volume, file-id) pair gets counted, the rest count as 0.
struct SeenLinks(Mutex<HashSet<(u64, u128)>>);

impl SeenLinks {
    fn new() -> Self {
        SeenLinks(Mutex::new(HashSet::new()))
    }
    /// true if this is the first sighting of the key (count it)
    fn first(&self, key: (u64, u128)) -> bool {
        self.0.lock().map(|mut s| s.insert(key)).unwrap_or(true)
    }
}

/// Bytes a file actually occupies on disk — sparse, compressed, and cloud
/// placeholder files count what they really use, and every set of hardlinks
/// is counted once. This is what makes the totals agree with the drive's
/// used/free numbers instead of overshooting them.
#[cfg(unix)]
fn file_size_on_disk(_ent: &fs::DirEntry, md: &fs::Metadata, seen: &SeenLinks) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if md.nlink() > 1 && !seen.first((md.dev(), md.ino() as u128)) {
        return 0;
    }
    md.blocks().saturating_mul(512)
}

#[cfg(windows)]
fn file_size_on_disk(ent: &fs::DirEntry, md: &fs::Metadata, seen: &SeenLinks) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, SetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileIdInfo, FileStandardInfo, GetCompressedFileSizeW,
        GetFileInformationByHandleEx, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, OPEN_EXISTING,
    };
    let path = ent.path();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        // Attribute-only opens bypass sharing locks, so even pagefile.sys opens.
        // OPEN_REPARSE_POINT keeps OneDrive placeholders dehydrated.
        let h = CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        );
        if h != INVALID_HANDLE_VALUE {
            let mut info: FILE_STANDARD_INFO = std::mem::zeroed();
            let ok = GetFileInformationByHandleEx(
                h,
                FileStandardInfo,
                &mut info as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            );
            if ok != 0 {
                let alloc = info.AllocationSize.max(0) as u64;
                if info.NumberOfLinks > 1 {
                    let mut id: FILE_ID_INFO = std::mem::zeroed();
                    let ok2 = GetFileInformationByHandleEx(
                        h,
                        FileIdInfo,
                        &mut id as *mut _ as *mut core::ffi::c_void,
                        std::mem::size_of::<FILE_ID_INFO>() as u32,
                    );
                    CloseHandle(h);
                    if ok2 != 0
                        && !seen.first((
                            id.VolumeSerialNumber,
                            u128::from_le_bytes(id.FileId.Identifier),
                        ))
                    {
                        return 0;
                    }
                    return alloc;
                }
                CloseHandle(h);
                return alloc;
            }
            CloseHandle(h);
        }
        // Couldn't open — GetCompressedFileSizeW works by path and still
        // reports compressed/sparse storage; last resort is the logical size.
        let mut hi: u32 = 0;
        SetLastError(0);
        let lo = GetCompressedFileSizeW(wide.as_ptr(), &mut hi);
        if lo != u32::MAX || GetLastError() == 0 {
            return (hi as u64) << 32 | lo as u64;
        }
    }
    md.len()
}

pub struct ScanResult {
    pub root: LocalNode,
    pub multi: bool,
    pub is_volume: bool,
    /// (mount path, total, free) per volume, in child order for multi scans
    pub volumes: Vec<(String, u64, u64)>,
}

pub fn scan_root(path: &Path, prog: &Progress, opts: &ScanOptions) -> Option<ScanResult> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return None;
    }
    let dev = dev_of(&meta);
    let seen = SeenLinks::new();
    let filter = Filter { opts };
    let mut root = scan_dir(path, dev, prog, &seen, &filter, 0)?;
    root.name = path.to_string_lossy().into_owned();
    root.mtime = root.mtime.max(mtime_of(&meta));
    let p = path.to_string_lossy().into_owned();
    let (total, free) = crate::disk_usage(&p);
    Some(ScanResult {
        root,
        multi: false,
        is_volume: is_mount_point(path),
        volumes: vec![(p, total, free)],
    })
}

/// Scan several mount points into one synthetic root ("All volumes"); each
/// child is one volume, named by its absolute mount path.
pub fn scan_all(
    roots: &[(String, u64, u64)],
    prog: &Progress,
    opts: &ScanOptions,
) -> Option<ScanResult> {
    let filter = Filter { opts };
    let mut children = Vec::new();
    let mut vols = Vec::new();
    for (p, total, free) in roots {
        if prog.cancel.load(Ordering::Relaxed) {
            return None;
        }
        let path = Path::new(p);
        let Ok(meta) = fs::metadata(path) else { continue };
        let seen = SeenLinks::new();
        let Some(mut n) = scan_dir(path, dev_of(&meta), prog, &seen, &filter, 1) else {
            return None;
        };
        n.name = p.clone();
        n.mtime = n.mtime.max(mtime_of(&meta));
        children.push(n);
        vols.push((p.clone(), *total, *free));
    }
    if children.is_empty() {
        return None;
    }
    let size = children.iter().map(|c| c.size).sum();
    let files = children.iter().map(|c| c.files).sum();
    let mtime = children.iter().map(|c| c.mtime).max().unwrap_or(0);
    let subdirs: u32 = children.iter().map(|c| 1 + c.subdirs).sum();
    // keep volumes in the order the user sees them (children sorted by size,
    // volumes follow so node ids line up after flatten)
    let mut order: Vec<usize> = (0..children.len()).collect();
    order.sort_by(|a, b| children[*b].size.cmp(&children[*a].size));
    let mut sorted_children = Vec::new();
    let mut sorted_vols = Vec::new();
    let mut children: Vec<Option<LocalNode>> = children.into_iter().map(Some).collect();
    for i in order {
        sorted_children.push(children[i].take().unwrap());
        sorted_vols.push(vols[i].clone());
    }
    Some(ScanResult {
        root: LocalNode {
            name: ALL_VOLUMES.to_string(),
            size,
            files,
            is_dir: true,
            mtime,
            subdirs,
            children: sorted_children,
        },
        multi: true,
        is_volume: false,
        volumes: sorted_vols,
    })
}

fn scan_dir(
    path: &Path,
    dev: u64,
    prog: &Progress,
    seen: &SeenLinks,
    filter: &Filter,
    depth: u32,
) -> Option<LocalNode> {
    if prog.cancel.load(Ordering::Relaxed) {
        return None;
    }
    let dcount = prog.dirs.fetch_add(1, Ordering::Relaxed);
    if depth <= 4 || dcount % 512 == 0 {
        if let Ok(mut cur) = prog.current.try_lock() {
            *cur = path.to_string_lossy().into_owned();
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let own_mtime = fs::symlink_metadata(path).map(|m| mtime_of(&m)).unwrap_or(0);

    let rd = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => {
            note_err(prog, path);
            return Some(LocalNode {
                name,
                size: 0,
                files: 0,
                is_dir: true,
                mtime: own_mtime,
                subdirs: 0,
                children: Vec::new(),
            });
        }
    };

    let mut children: Vec<LocalNode> = Vec::new();
    let mut subdirs: Vec<std::path::PathBuf> = Vec::new();
    let active = filter.opts.is_active();

    for ent in rd {
        let ent = match ent {
            Ok(e) => e,
            Err(_) => {
                note_err(prog, path);
                continue;
            }
        };
        let ft = match ent.file_type() {
            Ok(f) => f,
            Err(_) => {
                note_err(prog, &ent.path());
                continue;
            }
        };
        let fname = ent.file_name().to_string_lossy().into_owned();
        if ft.is_dir() {
            // stay on one filesystem: skip mount points (also skips /proc, /sys when scanning /)
            match ent.metadata() {
                Ok(md) if dev_of(&md) == dev => {
                    if active && filter.excluded(&fname, Some(&md)) {
                        prog.skipped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    subdirs.push(ent.path())
                }
                Ok(_) => {}
                Err(_) => {
                    note_err(prog, &ent.path());
                }
            }
        } else {
            // DirEntry::metadata does not follow symlinks; a symlink counts as its own tiny size
            let md = ent.metadata().ok();
            if active && filter.excluded(&fname, md.as_ref()) {
                prog.skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let (size, mtime) = match &md {
                Some(md) => (file_size_on_disk(&ent, md, seen), mtime_of(md)),
                None => (0, 0),
            };
            prog.files.fetch_add(1, Ordering::Relaxed);
            prog.bytes.fetch_add(size, Ordering::Relaxed);
            children.push(LocalNode {
                name: fname,
                size,
                files: 1,
                is_dir: false,
                mtime,
                subdirs: 0,
                children: Vec::new(),
            });
        }
    }

    let scanned: Vec<LocalNode> = subdirs
        .into_par_iter()
        .filter_map(|p| scan_dir(&p, dev, prog, seen, filter, depth + 1))
        .collect();
    children.extend(scanned);

    if prog.cancel.load(Ordering::Relaxed) {
        return None;
    }

    let size: u64 = children.iter().map(|c| c.size).sum();
    let files: u32 = children.iter().map(|c| c.files).sum();
    let mtime = children.iter().map(|c| c.mtime).max().unwrap_or(0).max(own_mtime);
    let subdirs: u32 = children.iter().filter(|c| c.is_dir).map(|c| 1 + c.subdirs).sum();
    children.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    Some(LocalNode { name, size, files, is_dir: true, mtime, subdirs, children })
}

fn ext_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(pos) if pos > 0 && pos + 1 < name.len() => {
            let ext = &name[pos + 1..];
            if ext.len() <= 12 && !ext.contains(char::is_whitespace) {
                ext
            } else {
                ""
            }
        }
        _ => "",
    }
}

pub struct FlattenMeta {
    pub root_path: String,
    pub elapsed_ms: u64,
    pub generation: u64,
    pub errors: u64,
    pub skipped: u64,
    pub multi: bool,
    pub is_volume: bool,
    pub volumes: Vec<(String, u64, u64)>,
}

pub fn flatten(root: LocalNode, meta: FlattenMeta) -> Store {
    let total_dirs = count_dirs(&root);
    let mut nodes: Vec<Node> = Vec::new();
    let mut ext_map: HashMap<String, u16> = HashMap::new();
    let mut exts: Vec<ExtStat> = Vec::new();
    // min-heap of (size, id) keeping the N largest files
    let mut heap: BinaryHeap<std::cmp::Reverse<(u64, u32)>> = BinaryHeap::new();
    const TOP_N: usize = 15;

    nodes.push(Node {
        name: root.name.into_boxed_str(),
        size: root.size,
        parent: 0,
        first_child: 0,
        child_count: 0,
        files: root.files,
        ext: EXT_DIR,
        is_dir: true,
        alive: true,
        mtime: root.mtime,
        subdirs: root.subdirs,
    });

    let mut queue: VecDeque<(u32, Vec<LocalNode>)> = VecDeque::new();
    queue.push_back((0, root.children));

    while let Some((pid, children)) = queue.pop_front() {
        let first = nodes.len() as u32;
        nodes[pid as usize].first_child = first;
        nodes[pid as usize].child_count = children.len() as u32;
        let mut pending: Vec<(u32, Vec<LocalNode>)> = Vec::new();
        for c in children {
            let id = nodes.len() as u32;
            let ext = if c.is_dir {
                EXT_DIR
            } else {
                let e = ext_of(&c.name).to_ascii_lowercase();
                let next = exts.len() as u16;
                let eid = *ext_map.entry(e.clone()).or_insert_with(|| {
                    exts.push(ExtStat { name: e, bytes: 0, files: 0, slot: 255 });
                    next
                });
                exts[eid as usize].bytes += c.size;
                exts[eid as usize].files += 1;
                eid
            };
            if !c.is_dir {
                if heap.len() < TOP_N {
                    heap.push(std::cmp::Reverse((c.size, id)));
                } else if let Some(&std::cmp::Reverse((min_sz, _))) = heap.peek() {
                    if c.size > min_sz {
                        heap.pop();
                        heap.push(std::cmp::Reverse((c.size, id)));
                    }
                }
            }
            nodes.push(Node {
                name: c.name.into_boxed_str(),
                size: c.size,
                parent: pid,
                first_child: 0,
                child_count: 0,
                files: c.files,
                ext,
                is_dir: c.is_dir,
                alive: true,
                mtime: c.mtime,
                subdirs: c.subdirs,
            });
            if !c.children.is_empty() {
                pending.push((id, c.children));
            }
        }
        for p in pending {
            queue.push_back(p);
        }
    }

    // top-8 extensions by bytes get the categorical slots, in fixed order
    let mut order: Vec<u16> = (0..exts.len() as u16).collect();
    order.sort_unstable_by(|a, b| exts[*b as usize].bytes.cmp(&exts[*a as usize].bytes));
    for (slot, eid) in order.iter().take(8).enumerate() {
        exts[*eid as usize].slot = slot as u8;
    }

    // into_sorted_vec on Reverse<_> already yields largest-first
    let largest: Vec<u32> = heap.into_sorted_vec().into_iter().map(|r| r.0 .1).collect();

    // volume → node id: single scan = root; multi = the depth-1 child whose
    // name is the mount path
    let volumes = meta
        .volumes
        .iter()
        .map(|(p, total, free)| {
            let node = if meta.multi {
                let r = &nodes[0];
                (r.first_child..r.first_child + r.child_count)
                    .find(|c| nodes[*c as usize].name.as_ref() == p.as_str())
                    .unwrap_or(0)
            } else {
                0
            };
            VolInfo { path: p.clone(), total: *total, free: *free, node }
        })
        .collect();

    Store {
        nodes,
        exts,
        root_path: meta.root_path,
        elapsed_ms: meta.elapsed_ms,
        generation: meta.generation,
        largest,
        dirs: total_dirs,
        errors: meta.errors,
        skipped: meta.skipped,
        multi: meta.multi,
        volumes,
        is_volume: meta.is_volume,
    }
}

fn count_dirs(n: &LocalNode) -> u64 {
    1 + n.children.iter().filter(|c| c.is_dir).map(count_dirs).sum::<u64>()
}

/// The volume a node belongs to, if the node IS a volume root (so used/free
/// blocks can be drawn around its tree).
pub fn volume_of(store: &Store, id: u32) -> Option<&VolInfo> {
    if store.multi {
        store.volumes.iter().find(|v| v.node == id && id != 0)
    } else if id == 0 && store.is_volume {
        store.volumes.first()
    } else {
        None
    }
}

pub fn path_of(store: &Store, id: u32) -> String {
    const SEP: char = std::path::MAIN_SEPARATOR;
    let mut parts: Vec<&str> = Vec::new();
    let mut cur = id;
    while cur != 0 {
        parts.push(&store.nodes[cur as usize].name);
        cur = store.nodes[cur as usize].parent;
    }
    parts.reverse();
    if store.multi {
        // depth-1 names are absolute mount paths
        let Some(first) = parts.first() else { return store.root_path.clone() };
        let mut path = first.to_string();
        if path.ends_with(SEP) {
            path.pop();
        }
        for p in &parts[1..] {
            path.push(SEP);
            path.push_str(p);
        }
        if path.is_empty() {
            path.push(SEP);
        }
        return path;
    }
    let mut path = store.root_path.clone();
    if path.ends_with(SEP) {
        path.pop();
    }
    for p in parts.iter() {
        path.push(SEP);
        path.push_str(p);
    }
    if path.is_empty() {
        path.push(SEP);
    }
    path
}

pub fn ancestors_of(store: &Store, id: u32) -> Vec<u32> {
    let mut anc = Vec::new();
    let mut cur = id;
    while cur != 0 {
        cur = store.nodes[cur as usize].parent;
        anc.push(cur);
    }
    anc.reverse();
    anc
}

/// Things that must never be deleted through the model: the scan root, and
/// in an all-volumes scan the volume roots themselves.
pub fn is_protected(store: &Store, id: u32) -> bool {
    id == 0 || (store.multi && store.nodes[id as usize].parent == 0)
}

/// Mark a subtree dead and subtract its weight from ancestors and extension stats.
/// Returns (bytes_freed, files_removed).
pub fn remove_subtree(store: &mut Store, id: u32) -> (u64, u64) {
    let freed = store.nodes[id as usize].size;
    let files = store.nodes[id as usize].files as u64;
    let gone_dirs = {
        let n = &store.nodes[id as usize];
        if n.is_dir { n.subdirs + 1 } else { 0 }
    };

    let mut stack = vec![id];
    while let Some(n) = stack.pop() {
        let (first, count, ext, is_dir, size, alive) = {
            let nd = &store.nodes[n as usize];
            (nd.first_child, nd.child_count, nd.ext, nd.is_dir, nd.size, nd.alive)
        };
        if !alive {
            continue;
        }
        store.nodes[n as usize].alive = false;
        if is_dir {
            store.dirs = store.dirs.saturating_sub(1);
            for c in first..first + count {
                stack.push(c);
            }
        } else {
            let e = &mut store.exts[ext as usize];
            e.bytes = e.bytes.saturating_sub(size);
            e.files = e.files.saturating_sub(1);
        }
    }

    let mut cur = id;
    while cur != 0 {
        cur = store.nodes[cur as usize].parent;
        let nd = &mut store.nodes[cur as usize];
        nd.size = nd.size.saturating_sub(freed);
        nd.files = nd.files.saturating_sub(files as u32);
        nd.subdirs = nd.subdirs.saturating_sub(gone_dirs);
    }
    (freed, files)
}

/// Rebuild the live part of a store as a LocalNode tree (dead nodes dropped).
fn to_local(store: &Store, id: u32) -> LocalNode {
    let n = &store.nodes[id as usize];
    let children = (n.first_child..n.first_child + n.child_count)
        .filter(|c| store.nodes[*c as usize].alive)
        .map(|c| to_local(store, c))
        .collect();
    LocalNode {
        name: n.name.to_string(),
        size: n.size,
        files: n.files,
        is_dir: n.is_dir,
        mtime: n.mtime,
        subdirs: n.subdirs,
        children,
    }
}

/// Replace one directory's subtree with a fresh scan of it and re-flatten, so
/// only that folder is rescanned. Returns the new store and the id of the
/// refreshed node in it.
pub fn replace_subtree(
    store: &Store,
    id: u32,
    mut fresh: LocalNode,
    generation: u64,
    elapsed_ms: u64,
    errors: u64,
    skipped: u64,
) -> (Store, u32) {
    let anc = ancestors_of(store, id);
    // path of names from root to target, used to find it again after flatten
    let mut trail: Vec<String> = anc
        .iter()
        .skip(1)
        .map(|a| store.nodes[*a as usize].name.to_string())
        .collect();
    if id != 0 {
        trail.push(store.nodes[id as usize].name.to_string());
    }
    fresh.name = store.nodes[id as usize].name.to_string();
    let mut root = if trail.is_empty() {
        fresh
    } else {
        let mut root = to_local(store, 0);
        // walk down the trail and swap in the fresh subtree
        let mut cur = &mut root;
        for (i, name) in trail.iter().enumerate() {
            let pos = cur.children.iter().position(|c| &c.name == name);
            let Some(pos) = pos else { break };
            if i + 1 == trail.len() {
                cur.children[pos] = fresh;
                break;
            }
            cur = &mut cur.children[pos];
        }
        root
    };
    fix_totals(&mut root);
    let volumes: Vec<(String, u64, u64)> = store
        .volumes
        .iter()
        .map(|v| {
            let (t, f) = crate::disk_usage(&v.path);
            (v.path.clone(), t, f)
        })
        .collect();
    let new = flatten(
        root,
        FlattenMeta {
            root_path: store.root_path.clone(),
            elapsed_ms,
            generation,
            errors,
            skipped,
            multi: store.multi,
            is_volume: store.is_volume,
            volumes,
        },
    );
    // locate the refreshed node by name trail
    let mut nid = 0u32;
    for name in &trail {
        let n = &new.nodes[nid as usize];
        let found = (n.first_child..n.first_child + n.child_count)
            .find(|c| new.nodes[*c as usize].name.as_ref() == name.as_str());
        match found {
            Some(c) => nid = c,
            None => break,
        }
    }
    (new, nid)
}

fn fix_totals(n: &mut LocalNode) -> (u64, u32, i64, u32) {
    if !n.is_dir {
        return (n.size, n.files, n.mtime, 0);
    }
    let mut size = 0;
    let mut files = 0;
    let mut mt = n.mtime;
    let mut sd = 0;
    for c in n.children.iter_mut() {
        let (s, f, m, d) = fix_totals(c);
        size += s;
        files += f;
        mt = mt.max(m);
        if c.is_dir {
            sd += 1 + d;
        }
    }
    n.children.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    n.size = size;
    n.files = files;
    n.mtime = mt;
    n.subdirs = sd;
    (size, files, mt, sd)
}

/// Resolve an absolute path back to a live node id (deepest match).
pub fn find_by_path(store: &Store, path: &str) -> Option<u32> {
    const SEP: char = std::path::MAIN_SEPARATOR;
    let path = path.trim_end_matches(SEP);
    let mut best = None;
    let mut stack = vec![0u32];
    while let Some(id) = stack.pop() {
        let p = path_of(store, id);
        let p = p.trim_end_matches(SEP);
        if p == path {
            return Some(id);
        }
        let is_prefix = path.starts_with(p)
            && (p.is_empty() || path[p.len()..].starts_with(SEP) || (store.multi && id == 0));
        if !is_prefix {
            continue;
        }
        best = Some(id);
        let n = &store.nodes[id as usize];
        for c in n.first_child..n.first_child + n.child_count {
            let k = &store.nodes[c as usize];
            if !k.alive {
                continue;
            }
            if k.is_dir {
                stack.push(c);
            } else if path_of(store, c) == path {
                return Some(c);
            }
        }
    }
    best
}

/// Case-insensitive substring search over live names under `under`;
/// results are largest-first, capped at `limit`.
pub fn search(store: &Store, under: u32, query: &str, limit: usize) -> Vec<u32> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let glob = q.contains('*') || q.contains('?');
    let mut hits: Vec<u32> = Vec::new();
    let mut stack = vec![under];
    while let Some(id) = stack.pop() {
        let n = &store.nodes[id as usize];
        if !n.alive {
            continue;
        }
        if id != under {
            let name = n.name.to_lowercase();
            let m = if glob { glob_match(&q, &name) } else { name.contains(&q) };
            if m {
                hits.push(id);
            }
        }
        if n.is_dir {
            for c in n.first_child..n.first_child + n.child_count {
                stack.push(c);
            }
        }
    }
    hits.sort_unstable_by(|a, b| store.nodes[*b as usize].size.cmp(&store.nodes[*a as usize].size));
    hits.truncate(limit);
    hits
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn fmt_date(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    // civil-from-days (Howard Hinnant), UTC
    let days = secs.div_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Write the live tree under `under` as CSV (path, kind, size on disk,
/// files, modified) into `out`, depth-first.
pub fn export_csv<W: std::io::Write>(store: &Store, under: u32, out: &mut W) -> std::io::Result<u64> {
    writeln!(out, "path,type,bytes,files,modified")?;
    let mut rows = 0u64;
    let mut stack = vec![under];
    while let Some(id) = stack.pop() {
        let n = &store.nodes[id as usize];
        if !n.alive {
            continue;
        }
        writeln!(
            out,
            "{},{},{},{},{}",
            csv_field(&path_of(store, id)),
            if n.is_dir { "folder" } else { "file" },
            n.size,
            n.files,
            fmt_date(n.mtime)
        )?;
        rows += 1;
        if n.is_dir {
            // push in reverse so the largest child is written first
            for c in (n.first_child..n.first_child + n.child_count).rev() {
                stack.push(c);
            }
        }
    }
    Ok(rows)
}

/// Human description for common extensions (WinDirStat shows the registry
/// file-type name; this is the portable equivalent).
pub fn ext_description(ext: &str) -> &'static str {
    match ext {
        "" => "no extension",
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "tif" | "tiff" | "svg" | "ico" | "raw" | "cr2" | "nef" | "dng" => "image",
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" | "wmv" | "flv" | "ts" | "mts" => "video",
        "mp3" | "flac" | "wav" | "aac" | "m4a" | "ogg" | "opus" | "wma" | "aiff" => "audio",
        "pdf" => "PDF document",
        "doc" | "docx" | "odt" | "rtf" | "pages" => "document",
        "xls" | "xlsx" | "ods" | "csv" | "numbers" => "spreadsheet",
        "ppt" | "pptx" | "odp" | "key" => "presentation",
        "txt" | "md" | "log" | "nfo" => "text",
        "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "zst" | "tgz" | "lz4" => "archive",
        "iso" | "img" | "dmg" | "vhd" | "vhdx" | "vmdk" | "qcow2" | "vdi" => "disk image",
        "exe" | "msi" | "dll" | "sys" | "com" => "Windows program",
        "so" | "a" | "o" | "ko" => "shared library / object",
        "app" | "dylib" | "pkg" => "macOS program",
        "deb" | "rpm" | "appimage" | "flatpak" | "snap" => "Linux package",
        "apk" | "ipa" => "mobile app",
        "js" | "jsx" | "tsx" | "mjs" | "cjs" => "JavaScript / TypeScript",
        "py" | "pyc" | "pyo" => "Python",
        "rs" | "rlib" | "rmeta" => "Rust",
        "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" => "C / C++",
        "java" | "class" | "jar" | "kt" => "Java / Kotlin",
        "go" => "Go",
        "rb" => "Ruby",
        "php" => "PHP",
        "cs" => "C#",
        "swift" => "Swift",
        "html" | "htm" | "css" | "scss" => "web page",
        "json" | "xml" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "plist" => "config / data",
        "sql" | "db" | "sqlite" | "sqlite3" | "mdb" | "accdb" => "database",
        "bin" | "dat" | "pak" | "cache" | "idx" | "pack" => "binary data",
        "bak" | "old" | "tmp" | "temp" | "swp" | "part" | "crdownload" => "backup / temporary",
        "ttf" | "otf" | "woff" | "woff2" => "font",
        "epub" | "mobi" | "azw3" => "e-book",
        "psd" | "ai" | "xcf" | "sketch" | "fig" | "blend" => "design file",
        "torrent" => "torrent",
        "lnk" | "url" | "desktop" => "shortcut",
        "sav" | "vpk" | "wad" | "unitypackage" => "game data",
        "lock" => "lock file",
        "map" => "source map",
        "wasm" => "WebAssembly",
        "node" => "Node.js addon",
        "jsonl" | "parquet" | "arrow" | "h5" | "hdf5" | "npy" | "npz" | "safetensors" | "gguf" | "ckpt" | "pt" | "pth" | "onnx" => "dataset / model",
        _ => "",
    }
}
