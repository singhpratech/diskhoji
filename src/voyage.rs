//! The Seeker's Voyage — the little side-scroller from diskhoji.org, ported
//! natively so it ships inside the app itself. Fully offline, no dependencies.
//!
//! Physics are a faithful transplant of the Chrome offline dino runner
//! (chromium's runner.js), scaled uniformly from its 600px world to our
//! 1040px canvas (K = 1040/600): initial speed 6K, max 13K, acceleration
//! 0.001K/frame, gravity 0.6K, jump velocity -(10K + speed/10), variable
//! jump height (releasing the key early cuts the jump at DROP_VELOCITY),
//! fast-drop on ↓, and speed-proportional obstacle gaps.
//!
//! The scene is a fixed night seascape (dusk sky, pasha moon, tiled sea), so
//! its palette is intentionally NOT theme-driven — night looks the same in
//! light and dark mode, exactly like the game on the website.

use eframe::egui::epaint::{Mesh, PathShape, Vertex, WHITE_UV};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke};

pub const W: f32 = 1040.0;
pub const H: f32 = 320.0;
const SEA: f32 = H - 74.0;
const BOATX: f32 = 150.0;

// Chrome-dino MECHANICS (constant acceleration, speed-scaled jumps, variable
// jump height, dive, speed-proportional gaps) at the game's own proven pacing —
// a straight scale-up of the dino world felt frantic on this wide canvas.
const BASE_SPEED: f32 = 3.2; // gentle launch, like the original tune
const MAX_SPEED: f32 = 8.5;
const ACCEL: f32 = 0.00076; // per frame — reaches MAX in ~2 min, dino-style ramp
const GRAVITY: f32 = 0.62; // per frame²
const JUMP_V: f32 = 12.0; // vy = -(JUMP_V + speed/12): slightly higher at speed
const DROP_V: f32 = 6.0; // jump-cut clamp: release early → short hop
const MIN_JUMP: f32 = 42.0; // px of height before a cut is allowed
const SPEED_DROP_V: f32 = 1.0; // ↓ kills upward motion, then falls at 3×
const REEF_GAP_BASE: f32 = 430.0; // min gap = reef.w × speed + this (dino getGap shape)

const GOLD: Color32 = Color32::from_rgb(0xE5, 0xB9, 0x6B);
const HULL: Color32 = Color32::from_rgb(0x13, 0x1A, 0x28);
const SEA_FILL: Color32 = Color32::from_rgb(0x0D, 0x15, 0x24);
const REEF_FILL: Color32 = Color32::from_rgb(0xE6, 0x67, 0x67);
const REEF_EDGE: Color32 = Color32::from_rgb(0x8F, 0x30, 0x30);
const MAST: Color32 = Color32::from_rgb(0x7A, 0x87, 0x9E);
const SAIL: Color32 = Color32::from_rgb(0x9A, 0xA5, 0xB8);

/// Same generator as the site: s = 1664525*s + 1013904223 (mod 2^32).
struct Lcg(u32);
impl Lcg {
    fn new(seed: u32) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 as f64 / 4_294_967_296.0) as f32
    }
}

struct Star {
    x: f32,
    y: f32,
    r: f32,
    p: f32,
    s: f32,
}
struct Tile {
    x: f32,
    d: f32,
    w: f32,
    c: f32,
}
struct Reef {
    x: f32,
    w: f32,
    h: f32,
}
struct Orb {
    x: f32,
    y: f32,
    dy: f32,
    got: bool,
}

pub struct Voyage {
    rnd: Lcg,
    stars: Vec<Star>,
    tiles: Vec<Tile>,
    pub running: bool,
    over: bool,
    t: f32,
    dist: f32,
    pub bytes: u32,
    speed: f32,
    vy: f32,
    boat_y: f32,
    jumping: bool,
    reached_min: bool,
    speed_drop: bool,
    reefs: Vec<Reef>,
    orbs: Vec<Orb>,
    next_reef: f32,
    next_orb: f32,
    pub best: u32,
    /// Overlay card shown when not running: (title, subtitle, button label).
    overlay: (String, String, String),
}

impl Voyage {
    pub fn new(best: u32) -> Self {
        let mut rnd = Lcg::new(108);
        let stars = (0..42)
            .map(|_| Star {
                x: rnd.next() * W,
                y: rnd.next() * (SEA - 110.0),
                r: 0.7 + rnd.next() * 1.4,
                p: rnd.next() * std::f32::consts::TAU,
                s: 0.4 + rnd.next() * 1.2,
            })
            .collect();
        let tiles = (0..90)
            .map(|_| Tile {
                x: rnd.next() * W,
                d: 6.0 + rnd.next() * 46.0,
                w: 10.0 + rnd.next() * 20.0,
                c: rnd.next(),
            })
            .collect();
        Self {
            rnd,
            stars,
            tiles,
            running: false,
            over: false,
            t: 40.0,
            dist: 0.0,
            bytes: 0,
            speed: BASE_SPEED,
            vy: 0.0,
            boat_y: SEA,
            jumping: false,
            reached_min: false,
            speed_drop: false,
            reefs: Vec::new(),
            orbs: Vec::new(),
            next_reef: 560.0,
            next_orb: 880.0,
            best,
            overlay: (
                "The Seeker's Voyage".into(),
                "Sail east under the pasha moon. Leap the corrupt reefs, gather amber bytes.".into(),
                // no ⛵ here: U+26F5 is missing from the bundled fallback font
                "Set sail".into(),
            ),
        }
    }

    pub fn fathoms(&self) -> u32 {
        (self.dist / 10.0) as u32
    }

    fn reset(&mut self) {
        self.over = false;
        self.t = 0.0;
        self.dist = 0.0;
        self.bytes = 0;
        self.speed = BASE_SPEED;
        self.vy = 0.0;
        self.boat_y = SEA;
        self.jumping = false;
        self.reached_min = false;
        self.speed_drop = false;
        self.reefs.clear();
        self.orbs.clear();
        self.next_reef = 560.0;
        self.next_orb = 880.0;
    }

    fn start(&mut self) {
        if self.running && !self.over {
            return;
        }
        if self.over || self.dist == 0.0 {
            self.reset();
        }
        self.running = true;
    }

    /// Dino startJump: velocity scales slightly with current speed.
    fn jump(&mut self) {
        if !self.running || self.over {
            return;
        }
        if !self.jumping {
            self.jumping = true;
            self.vy = -(JUMP_V + self.speed / 12.0);
            self.reached_min = false;
            self.speed_drop = false;
        }
    }

    /// Dino endJump: releasing the key early shortens the hop.
    fn end_jump(&mut self) {
        if self.jumping && self.reached_min && self.vy < -DROP_V {
            self.vy = -DROP_V;
        }
    }

    /// Dino setSpeedDrop: ↓ kills upward motion and falls at 3×.
    fn set_speed_drop(&mut self) {
        if self.jumping {
            self.speed_drop = true;
            self.vy = SPEED_DROP_V;
        }
    }

    /// Pause without wrecking (window closed / app hidden).
    pub fn anchor(&mut self) {
        if self.running && !self.over {
            self.running = false;
            self.overlay = (
                "At anchor".into(),
                "The watch paused while you were away.".into(),
                "Resume — sail on".into(),
            );
        }
    }

    fn wreck(&mut self) -> bool {
        self.over = true;
        self.running = false;
        let f = self.fathoms();
        let mut best_changed = false;
        if f > self.best {
            self.best = f;
            best_changed = true;
        }
        let sub = if self.bytes > 0 {
            format!(
                "With {} amber bytes aboard. The sea keeps honest ledgers.",
                self.bytes
            )
        } else {
            "The sea keeps honest ledgers.".into()
        };
        self.overlay = (format!("Wrecked at {} fathoms", f), sub, "Sail again".into());
        best_changed
    }

    fn wave_y(&self, x: f32) -> f32 {
        SEA + 4.0 * ((x + self.t * self.speed) * 0.018).sin()
            + 2.0 * ((x + self.t * self.speed) * 0.043).sin()
    }

    /// Advance one frame. `dt_secs` is real elapsed time; normalised to
    /// 60 fps units exactly like the dino runner. Returns true if `best` changed.
    fn step(&mut self, dt_secs: f32) -> bool {
        let mut dt = dt_secs * 1000.0 / 16.667;
        if !(dt > 0.0) {
            dt = 1.0;
        }
        dt = dt.min(2.5);

        self.t += dt;
        self.dist += self.speed * 0.28 * dt;
        // dino-style: constant acceleration every frame up to MAX_SPEED.
        self.speed = (self.speed + ACCEL * dt).min(MAX_SPEED);

        if self.jumping {
            self.vy += GRAVITY * dt;
            let fall = if self.speed_drop { 3.0 } else { 1.0 };
            self.boat_y += self.vy * dt * fall;
            if SEA - self.boat_y >= MIN_JUMP {
                self.reached_min = true;
            }
            if self.boat_y >= SEA {
                self.boat_y = SEA;
                self.jumping = false;
                self.vy = 0.0;
                self.speed_drop = false;
            }
        }
        let step = self.speed * dt;
        self.next_reef -= step;
        if self.next_reef <= 0.0 {
            let w = 22.0 + self.rnd.next() * 18.0;
            let h = 28.0 + self.rnd.next() * 24.0;
            self.reefs.push(Reef { x: W + 40.0, w, h });
            // dino getGap shape: minGap = width×speed + base, maxGap = 1.5×minGap
            // — gaps stretch as the world speeds up, so speed never feels unfair.
            let min_gap = w * self.speed + REEF_GAP_BASE;
            self.next_reef = min_gap * (1.0 + self.rnd.next() * 0.5);
        }
        self.next_orb -= step;
        if self.next_orb <= 0.0 {
            let y = SEA - 98.0 - self.rnd.next() * 36.0;
            self.orbs.push(Orb { x: W + 40.0, y, dy: 0.0, got: false });
            self.next_orb = 520.0 + self.rnd.next() * 940.0;
        }
        for r in &mut self.reefs {
            r.x -= step;
        }
        for o in &mut self.orbs {
            o.x -= step;
        }
        self.reefs.retain(|r| r.x > -80.0);
        self.orbs.retain(|o| o.x > -40.0);

        // collisions (boat box) — identical to the web build
        let (bx1, bx2) = (BOATX - 28.0, BOATX + 34.0);
        let (by1, by2) = (self.boat_y - 30.0, self.boat_y + 6.0);
        let mut wrecked = false;
        for r in &self.reefs {
            let ry = self.wave_y(r.x);
            if bx2 > r.x + 3.0 && bx1 < r.x + r.w - 3.0 && by2 > ry - r.h + 6.0 {
                wrecked = true;
                break;
            }
        }
        if wrecked {
            return self.wreck();
        }
        for o in &mut self.orbs {
            if o.x > bx1 && o.x < bx2 && o.dy > by1 - 10.0 && o.dy < by2 + 6.0 && !o.got {
                o.got = true;
                o.x = -999.0;
                self.bytes += 8;
            }
        }
        false
    }

    /// Draw + input + physics for one frame inside `rect` (any size; the
    /// 1040×320 logical scene is scaled to fit). Returns true if `best` changed.
    pub fn frame(&mut self, ui: &mut egui::Ui, rect: Rect) -> bool {
        let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        let (jump_press, jump_release, drop_press, drop_release) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::ArrowUp),
                i.key_released(egui::Key::Space) || i.key_released(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_released(egui::Key::ArrowDown),
            )
        });
        if resp.drag_started() || resp.clicked() || jump_press {
            if !self.running {
                self.start();
            } else {
                self.jump();
            }
        }
        if resp.drag_stopped() || jump_release {
            self.end_jump();
        }
        if drop_press {
            self.set_speed_drop();
        }
        if drop_release {
            self.speed_drop = false;
        }

        let mut best_changed = false;
        if self.running {
            let dt = ui.input(|i| i.stable_dt).min(0.1);
            best_changed = self.step(dt);
        } else {
            // ambient preview: the sea keeps sailing behind the overlay card
            self.t += 0.5;
        }
        ui.ctx().request_repaint();

        self.paint(ui, rect);
        best_changed
    }

    // ---------- painting ----------

    fn paint(&self, ui: &mut egui::Ui, rect: Rect) {
        let s = rect.width() / W;
        let o = rect.min;
        let p = |x: f32, y: f32| Pos2::new(o.x + x * s, o.y + y * s);
        let painter = ui.painter_at(rect);

        // dusk sky: vertical gradient, two banded quads with vertex colors
        let sky = [
            (0.0, Color32::from_rgb(0x08, 0x0B, 0x11)),
            (SEA * 0.72, Color32::from_rgb(0x0C, 0x12, 0x20)),
            (SEA, Color32::from_rgb(0x25, 0x1B, 0x27)),
        ];
        let mut mesh = Mesh::default();
        for w in sky.windows(2) {
            let (y0, c0) = w[0];
            let (y1, c1) = w[1];
            let base = mesh.vertices.len() as u32;
            for (x, y, c) in [(0.0, y0, c0), (W, y0, c0), (W, y1, c1), (0.0, y1, c1)] {
                mesh.vertices.push(Vertex { pos: p(x, y), uv: WHITE_UV, color: c });
            }
            mesh.indices
                .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        painter.add(mesh);

        // star spies (little diamonds, twinkling)
        for st in &self.stars {
            let tw = 0.55 + 0.45 * (self.t * 0.02 * st.s + st.p).sin();
            let a = (255.0 * 0.35 * tw) as u8;
            let c = Color32::from_rgba_unmultiplied(0xE5, 0xB9, 0x6B, a);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    p(st.x, st.y - st.r * 2.0),
                    p(st.x + st.r * 0.7, st.y),
                    p(st.x, st.y + st.r * 2.0),
                    p(st.x - st.r * 0.7, st.y),
                ],
                c,
                Stroke::NONE,
            ));
        }

        // pasha moon: glow halo, gold ring, rope spokes, knot
        let (mx, my, mr) = (W - 190.0, 84.0, 44.0);
        let mc = p(mx, my);
        painter.circle_stroke(
            mc,
            mr * s,
            Stroke::new(16.0 * s, Color32::from_rgba_unmultiplied(0xE5, 0xB9, 0x6B, 40)),
        );
        painter.circle_stroke(mc, mr * s, Stroke::new(7.0 * s, GOLD));
        let dark = Color32::from_rgba_unmultiplied(0x0A, 0x0D, 0x12, 204);
        let mut a = 0.0_f32;
        while a < std::f32::consts::TAU {
            painter.line_segment(
                [
                    p(mx + a.cos() * (mr - 4.0), my + a.sin() * (mr - 4.0)),
                    p(
                        mx + (a + 0.18).cos() * (mr + 4.0),
                        my + (a + 0.18).sin() * (mr + 4.0),
                    ),
                ],
                Stroke::new(2.4 * s, dark),
            );
            a += std::f32::consts::PI / 9.0;
        }
        painter.circle_filled(p(mx, my + mr), 6.5 * s, GOLD);
        // moon glade on the water
        painter.rect_filled(
            Rect::from_min_max(p(mx - 30.0, SEA), p(mx + 30.0, H)),
            0.0,
            Color32::from_rgba_unmultiplied(0xE5, 0xB9, 0x6B, 20),
        );

        // sea: vertical strips under the wave line (artifact-free concave fill)
        let mut x = 0.0;
        while x <= W {
            let y = self.wave_y(x);
            painter.rect_filled(Rect::from_min_max(p(x, y), p(x + 8.0, H)), 0.0, SEA_FILL);
            x += 8.0;
        }
        // wave crest
        let mut crest = Vec::with_capacity((W as usize / 8) + 1);
        let mut x = 0.0;
        while x <= W {
            crest.push(p(x, self.wave_y(x)));
            x += 8.0;
        }
        painter.add(egui::Shape::line(
            crest,
            Stroke::new(2.0 * s, Color32::from_rgba_unmultiplied(0x39, 0x87, 0xE5, 191)),
        ));
        // drifting data tiles in the water
        for tl in &self.tiles {
            let mut tx = tl.x - (self.t * self.speed * 0.6) % (W + 80.0);
            if tx < -60.0 {
                tx += W + 80.0;
            }
            let y = self.wave_y(tx) + tl.d;
            if y > H - 6.0 {
                continue;
            }
            let c = if tl.c < 0.12 {
                Color32::from_rgba_unmultiplied(0xE5, 0xB9, 0x6B, 128)
            } else if tl.c < 0.5 {
                Color32::from_rgba_unmultiplied(0x1E, 0x5A, 0x9E, 140)
            } else {
                Color32::from_rgba_unmultiplied(0x19, 0x9E, 0x70, 102)
            };
            painter.rect_filled(Rect::from_min_max(p(tx, y), p(tx + tl.w, y + 5.0)), 0.0, c);
        }

        // reefs (drawn as a fan mesh: the zigzag top makes the polygon concave)
        for r in &self.reefs {
            let y = self.wave_y(r.x);
            let pts = [
                p(r.x, y + 6.0),
                p(r.x + r.w * 0.18, y - r.h * 0.72),
                p(r.x + r.w * 0.44, y - r.h * 0.42),
                p(r.x + r.w * 0.62, y - r.h),
                p(r.x + r.w * 0.86, y - r.h * 0.5),
                p(r.x + r.w, y + 6.0),
            ];
            let mut m = Mesh::default();
            for pt in pts {
                m.vertices.push(Vertex { pos: pt, uv: WHITE_UV, color: REEF_FILL });
            }
            for i in 1..pts.len() as u32 - 1 {
                m.indices.extend([0, i, i + 1]);
            }
            painter.add(m);
            painter.add(egui::Shape::Path(PathShape::closed_line(
                pts.to_vec(),
                Stroke::new(1.6 * s, REEF_EDGE),
            )));
        }

        // amber orbs (rotated squares with a soft glow)
        for ob in &self.orbs {
            if ob.got {
                continue;
            }
            let y = ob.y + 3.0 * (self.t * 0.07 + ob.x * 0.05).sin();
            let c = p(ob.x, y);
            painter.circle_filled(
                c,
                12.0 * s,
                Color32::from_rgba_unmultiplied(0xE5, 0xB9, 0x6B, 46),
            );
            let d = 7.0 * std::f32::consts::SQRT_2 * s; // half-diagonal of the 14px square
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(c.x, c.y - d),
                    Pos2::new(c.x + d, c.y),
                    Pos2::new(c.x, c.y + d),
                    Pos2::new(c.x - d, c.y),
                ],
                GOLD,
                Stroke::NONE,
            ));
        }

        self.paint_boat(&painter, &p, s);

        // overlay card when not sailing
        if !self.running {
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(110));
            let (title, sub, btn) = &self.overlay;
            painter.text(
                p(W / 2.0, H / 2.0 - 34.0),
                Align2::CENTER_CENTER,
                title,
                FontId::proportional(26.0 * s.max(0.5)),
                GOLD,
            );
            painter.text(
                p(W / 2.0, H / 2.0 + 2.0),
                Align2::CENTER_CENTER,
                sub,
                FontId::proportional(14.0 * s.max(0.5)),
                Color32::from_rgb(0xC9, 0xD2, 0xE0),
            );
            painter.text(
                p(W / 2.0, H / 2.0 + 34.0),
                Align2::CENTER_CENTER,
                format!("{}  —  Space, ↑, or click", btn),
                FontId::proportional(15.0 * s.max(0.5)),
                Color32::WHITE,
            );
        }
    }

    fn paint_boat(&self, painter: &egui::Painter, p: &dyn Fn(f32, f32) -> Pos2, s: f32) {
        let bob = if self.jumping {
            0.0
        } else {
            2.4 * (self.t * 0.09).sin()
        };
        let (bx, by) = (BOATX, self.boat_y + bob);
        let q = |x: f32, y: f32| p(bx + x, by + y);
        // quadratic bezier sampler
        let quad = |p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), out: &mut Vec<Pos2>| {
            for i in 1..=6 {
                let t = i as f32 / 6.0;
                let u = 1.0 - t;
                let x = u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0;
                let y = u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1;
                out.push(q(x, y));
            }
        };
        // hull
        let mut hull = vec![q(-44.0, -8.0)];
        quad((-44.0, -8.0), (-30.0, 12.0), (0.0, 13.0), &mut hull);
        quad((0.0, 13.0), (34.0, 12.0), (46.0, -10.0), &mut hull);
        hull.push(q(38.0, -8.0));
        hull.push(q(-38.0, -8.0));
        painter.add(egui::Shape::convex_polygon(hull.clone(), HULL, Stroke::NONE));
        painter.add(egui::Shape::Path(PathShape::closed_line(
            hull,
            Stroke::new(2.0 * s, GOLD),
        )));
        // makara prow
        let mut prow = vec![q(46.0, -10.0)];
        quad((46.0, -10.0), (58.0, -18.0), (54.0, -30.0), &mut prow);
        quad((54.0, -30.0), (50.0, -24.0), (44.0, -24.0), &mut prow);
        painter.add(egui::Shape::convex_polygon(prow, GOLD, Stroke::NONE));
        // mast + lateen sail
        painter.line_segment([q(-2.0, -8.0), q(-2.0, -74.0)], Stroke::new(2.4 * s, MAST));
        painter.line_segment([q(-34.0, -18.0), q(30.0, -80.0)], Stroke::new(2.4 * s, MAST));
        painter.add(egui::Shape::convex_polygon(
            vec![q(-30.0, -20.0), q(28.0, -76.0), q(24.0, -22.0)],
            SAIL,
            Stroke::NONE,
        ));
        // lantern glow + body
        painter.circle_filled(
            q(40.0, -16.0),
            14.0 * s,
            Color32::from_rgba_unmultiplied(0xE5, 0xB9, 0x6B, 36),
        );
        painter.circle_filled(
            q(40.0, -16.0),
            7.0 * s,
            Color32::from_rgba_unmultiplied(0xE5, 0xB9, 0x6B, 90),
        );
        painter.rect_filled(
            Rect::from_min_max(q(38.0, -19.0), q(43.0, -12.0)),
            1.0,
            GOLD,
        );
    }
}
