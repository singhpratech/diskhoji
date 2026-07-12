use crate::scan::Store;
use serde::Serialize;

pub const SLOT_SMALL: u8 = 253; // merged run of sub-pixel siblings
pub const SLOT_DIRAGG: u8 = 254; // directory too small to open

#[derive(Serialize)]
pub struct Rect {
    pub i: u32, // node id (for merged blocks: the containing directory)
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub s: u8, // color slot 0-7, or SLOT_* sentinel / 255 = "other" ext
    pub d: u8, // 0 file, 1 dir-aggregate, 2 merged-small-items
    pub n: String,
    pub z: u64, // bytes
}

#[derive(Serialize)]
pub struct DirRect {
    pub i: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub p: u16, // depth
    pub n: String,
    pub z: u64,
}

const MIN_AREA: f64 = 0.30; // px^2 — children below this merge into one block
const MAX_RECTS: usize = 40_000;
const MAX_DEPTH: u16 = 64;

struct Work {
    area: f64,
    id: u32,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    depth: u16,
}
impl PartialEq for Work {
    fn eq(&self, other: &Self) -> bool {
        self.area == other.area
    }
}
impl Eq for Work {}
impl PartialOrd for Work {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Work {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.area.total_cmp(&other.area)
    }
}

/// Largest-rect-first traversal: when the rect budget runs out, only the
/// smallest on-screen directories stay aggregated, never a visible region.
pub fn layout(store: &Store, root: u32, w: f64, h: f64) -> (Vec<Rect>, Vec<DirRect>) {
    let mut leafs = Vec::new();
    let mut dirs = Vec::new();
    if store.nodes[root as usize].size == 0 {
        return (leafs, dirs);
    }
    let mut heap: std::collections::BinaryHeap<Work> = std::collections::BinaryHeap::new();
    heap.push(Work { area: w * h, id: root, x: 0.0, y: 0.0, w, h, depth: 0 });

    while let Some(wk) = heap.pop() {
        if leafs.len() >= MAX_RECTS && wk.id != root {
            let k = &store.nodes[wk.id as usize];
            leafs.push(Rect {
                i: wk.id,
                x: r1(wk.x),
                y: r1(wk.y),
                w: r1(wk.w),
                h: r1(wk.h),
                s: SLOT_DIRAGG,
                d: 1,
                n: k.name.to_string(),
                z: k.size,
            });
            continue;
        }
        expand(store, &wk, &mut leafs, &mut dirs, &mut heap);
    }
    (leafs, dirs)
}

fn r1(v: f64) -> f32 {
    ((v * 10.0).round() / 10.0) as f32
}

fn expand(
    store: &Store,
    wk: &Work,
    leafs: &mut Vec<Rect>,
    dirs: &mut Vec<DirRect>,
    heap: &mut std::collections::BinaryHeap<Work>,
) {
    let (id, x, y, w, h, depth) = (wk.id, wk.x, wk.y, wk.w, wk.h, wk.depth);
    let node = &store.nodes[id as usize];
    let mut kids: Vec<(u32, f64)> = (node.first_child..node.first_child + node.child_count)
        .filter_map(|c| {
            let k = &store.nodes[c as usize];
            (k.alive && k.size > 0).then(|| (c, k.size as f64))
        })
        .collect();
    if kids.is_empty() {
        return;
    }
    // deletions can leave the stored order stale
    kids.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let total: f64 = kids.iter().map(|k| k.1).sum();
    let scale = (w * h) / total;

    // merge the sub-pixel tail into one block so area stays honest
    let mut items: Vec<(Option<u32>, f64, u32)> = Vec::new();
    let mut i = 0;
    while i < kids.len() {
        if kids[i].1 * scale < MIN_AREA && i + 1 < kids.len() {
            let tail_size: f64 = kids[i..].iter().map(|k| k.1).sum();
            items.push((None, tail_size, (kids.len() - i) as u32));
            break;
        }
        items.push((Some(kids[i].0), kids[i].1, 0));
        i += 1;
    }

    let sizes: Vec<f64> = items.iter().map(|it| it.1).collect();
    let placed = squarify(&sizes, x, y, w, h);

    for ((opt, sz, cnt), r) in items.into_iter().zip(placed) {
        if r.2 < 0.5 || r.3 < 0.5 {
            continue;
        }
        match opt {
            None => leafs.push(Rect {
                i: id,
                x: r1(r.0),
                y: r1(r.1),
                w: r1(r.2),
                h: r1(r.3),
                s: SLOT_SMALL,
                d: 2,
                n: format!("{} small items", cnt),
                z: sz as u64,
            }),
            Some(cid) => {
                let k = &store.nodes[cid as usize];
                if k.is_dir {
                    if r.2 >= 9.0 && r.3 >= 9.0 && depth < MAX_DEPTH {
                        dirs.push(DirRect {
                            i: cid,
                            x: r1(r.0),
                            y: r1(r.1),
                            w: r1(r.2),
                            h: r1(r.3),
                            p: depth,
                            n: k.name.to_string(),
                            z: k.size,
                        });
                        heap.push(Work {
                            area: r.2 * r.3,
                            id: cid,
                            x: r.0,
                            y: r.1,
                            w: r.2,
                            h: r.3,
                            depth: depth + 1,
                        });
                    } else {
                        leafs.push(Rect {
                            i: cid,
                            x: r1(r.0),
                            y: r1(r.1),
                            w: r1(r.2),
                            h: r1(r.3),
                            s: SLOT_DIRAGG,
                            d: 1,
                            n: k.name.to_string(),
                            z: k.size,
                        });
                    }
                } else {
                    leafs.push(Rect {
                        i: cid,
                        x: r1(r.0),
                        y: r1(r.1),
                        w: r1(r.2),
                        h: r1(r.3),
                        s: store.exts[k.ext as usize].slot,
                        d: 0,
                        n: k.name.to_string(),
                        z: k.size,
                    });
                }
            }
        }
    }
}

/// Squarified treemap (Bruls, Huizing, van Wijk). Input sizes must be descending.
/// Returns one (x, y, w, h) per input.
fn squarify(sizes: &[f64], x0: f64, y0: f64, w0: f64, h0: f64) -> Vec<(f64, f64, f64, f64)> {
    let n = sizes.len();
    let mut out = vec![(0.0, 0.0, 0.0, 0.0); n];
    let total: f64 = sizes.iter().sum();
    if total <= 0.0 || w0 <= 0.0 || h0 <= 0.0 {
        return out;
    }
    let scale = (w0 * h0) / total;
    let (mut x, mut y, mut w, mut h) = (x0, y0, w0, h0);
    let mut i = 0;

    while i < n {
        if w < 0.01 || h < 0.01 {
            break;
        }
        let vertical = w < h; // portrait container: slice a horizontal strip across the top
        let l = if vertical { w } else { h };

        // grow the row while the worst aspect ratio keeps improving
        let mut sum = 0.0;
        let mut amin = f64::MAX;
        let mut amax = 0.0_f64;
        let mut worst_prev = f64::MAX;
        let mut j = i;
        while j < n {
            let a = (sizes[j] * scale).max(1e-9);
            let ns = sum + a;
            let namin = amin.min(a);
            let namax = amax.max(a);
            let worst = ((l * l * namax) / (ns * ns)).max((ns * ns) / (l * l * namin));
            if worst <= worst_prev {
                sum = ns;
                amin = namin;
                amax = namax;
                worst_prev = worst;
                j += 1;
            } else {
                break;
            }
        }

        let t = sum / l; // strip thickness
        let mut off = 0.0;
        for k in i..j {
            let len = (sizes[k] * scale).max(1e-9) / t;
            out[k] = if vertical {
                (x + off, y, len, t)
            } else {
                (x, y + off, t, len)
            };
            off += len;
        }
        if vertical {
            y += t;
            h -= t;
        } else {
            x += t;
            w -= t;
        }
        i = j;
    }
    out
}
