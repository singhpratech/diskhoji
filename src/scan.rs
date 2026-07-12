use rayon::prelude::*;
use std::collections::{BinaryHeap, HashMap, VecDeque};
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
        self.err_paths.lock().map(|mut v| v.clear()).ok();
        self.cancel.store(false, Ordering::Relaxed);
        self.current.lock().unwrap().clear();
    }
}

pub struct LocalNode {
    pub name: String,
    pub size: u64,
    pub files: u32,
    pub is_dir: bool,
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
}

pub struct ExtStat {
    pub name: String,
    pub bytes: u64,
    pub files: u64,
    pub slot: u8,
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
}

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

pub fn scan_root(path: &Path, prog: &Progress) -> Option<LocalNode> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return None;
    }
    let dev = dev_of(&meta);
    let mut root = scan_dir(path, dev, prog, 0)?;
    root.name = path.to_string_lossy().into_owned();
    Some(root)
}

fn scan_dir(path: &Path, dev: u64, prog: &Progress, depth: u32) -> Option<LocalNode> {
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

    let rd = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => {
            note_err(prog, path);
            return Some(LocalNode { name, size: 0, files: 0, is_dir: true, children: Vec::new() });
        }
    };

    let mut children: Vec<LocalNode> = Vec::new();
    let mut subdirs: Vec<std::path::PathBuf> = Vec::new();

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
        if ft.is_dir() {
            // stay on one filesystem: skip mount points (also skips /proc, /sys when scanning /)
            match ent.metadata() {
                Ok(md) if dev_of(&md) == dev => subdirs.push(ent.path()),
                Ok(_) => {}
                Err(_) => {
                    note_err(prog, &ent.path());
                }
            }
        } else {
            // DirEntry::metadata does not follow symlinks; a symlink counts as its own tiny size
            let size = ent.metadata().map(|m| m.len()).unwrap_or(0);
            prog.files.fetch_add(1, Ordering::Relaxed);
            prog.bytes.fetch_add(size, Ordering::Relaxed);
            children.push(LocalNode {
                name: ent.file_name().to_string_lossy().into_owned(),
                size,
                files: 1,
                is_dir: false,
                children: Vec::new(),
            });
        }
    }

    let scanned: Vec<LocalNode> = subdirs
        .into_par_iter()
        .filter_map(|p| scan_dir(&p, dev, prog, depth + 1))
        .collect();
    children.extend(scanned);

    if prog.cancel.load(Ordering::Relaxed) {
        return None;
    }

    let size: u64 = children.iter().map(|c| c.size).sum();
    let files: u32 = children.iter().map(|c| c.files).sum();
    children.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    Some(LocalNode { name, size, files, is_dir: true, children })
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

pub fn flatten(root: LocalNode, root_path: String, elapsed_ms: u64, generation: u64, errors: u64) -> Store {
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

    Store { nodes, exts, root_path, elapsed_ms, generation, largest, dirs: total_dirs, errors }
}

fn count_dirs(n: &LocalNode) -> u64 {
    1 + n.children.iter().filter(|c| c.is_dir).map(count_dirs).sum::<u64>()
}

pub fn path_of(store: &Store, id: u32) -> String {
    const SEP: char = std::path::MAIN_SEPARATOR;
    let mut parts: Vec<&str> = Vec::new();
    let mut cur = id;
    while cur != 0 {
        parts.push(&store.nodes[cur as usize].name);
        cur = store.nodes[cur as usize].parent;
    }
    let mut path = store.root_path.clone();
    if path.ends_with(SEP) {
        path.pop();
    }
    for p in parts.iter().rev() {
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

/// Mark a subtree dead and subtract its weight from ancestors and extension stats.
/// Returns (bytes_freed, files_removed).
pub fn remove_subtree(store: &mut Store, id: u32) -> (u64, u64) {
    let freed = store.nodes[id as usize].size;
    let files = store.nodes[id as usize].files as u64;

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
    }
    (freed, files)
}
