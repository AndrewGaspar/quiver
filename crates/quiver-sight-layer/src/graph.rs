//! The live frame-time graph (SIGHT-HUD §2.8 step 3): the last `span_ms` of presented frame
//! intervals from the layer's own present timestamps, drawn into each graph rectangle the
//! HUD image's descriptor names. Pure CPU code: fixed-size history, fixed-size output, no
//! allocation, bounded work (≤ [`MAX_POINTS`] points per graph whatever the frame rate).

use quiver_perf::layout::HudGraph;

/// Present timestamps kept: 10 s at 400 fps. Faster games get a shorter graph span.
pub const HISTORY: usize = 4096;
/// Points per graph after merging frames that fall into the same pixel column (keeping the
/// slowest, so hitches always show).
pub const MAX_POINTS: usize = 1024;
/// Vertices per graph: a fill trapezoid and a line quad (6 each) per segment.
pub const VERTS_PER_GRAPH: usize = (MAX_POINTS - 1) * 12;

/// One graph vertex: swapchain pixels and a premultiplied RGBA8 colour (bytes R, G, B, A).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub color: u32,
}

/// A ring of present entry times (CLOCK_MONOTONIC_RAW ns) of the swapchain the HUD draws on.
pub struct History {
    t: [i64; HISTORY],
    next: usize,
    len: usize,
}

impl Default for History {
    fn default() -> Self {
        History::new()
    }
}

impl History {
    pub const fn new() -> History {
        History { t: [0; HISTORY], next: 0, len: 0 }
    }

    /// Adds a present time. Times going backwards (a clock glitch, presents from two threads
    /// racing) are dropped so intervals stay non-negative.
    pub fn push(&mut self, t: i64) {
        if self.len > 0 && t < self.get(0) {
            return;
        }
        self.t[self.next] = t;
        self.next = (self.next + 1) % HISTORY;
        self.len = (self.len + 1).min(HISTORY);
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `back`-th newest time (0 = newest). `back < len`.
    fn get(&self, back: usize) -> i64 {
        self.t[(self.next + HISTORY - 1 - back) % HISTORY]
    }
}

/// White at 90%, for a descriptor that leaves `line` zero.
const DEFAULT_LINE: u32 = u32::from_le_bytes([230, 230, 230, 230]);

/// A stable vertical range for an auto-scaled graph: the first "nice" frame time above the
/// window's slowest frame plus 15%.
fn auto_max(slowest_ms: f32, min_ms: f32) -> f32 {
    const NICE: [f32; 10] = [10.0, 20.0, 33.4, 50.0, 66.7, 100.0, 200.0, 500.0, 1000.0, 2000.0];
    let want = slowest_ms * 1.15;
    NICE.iter().copied().find(|&m| m >= want && m > min_ms).unwrap_or(want.max(min_ms + 1.0))
}

/// Builds the triangles for graph `g` (a rectangle in image pixels, the image's top-left at
/// `origin` in swapchain pixels) into `out`, and returns how many vertices it wrote.
/// `thickness` is the line width in pixels (the image's scale).
pub fn build(hist: &History, g: &HudGraph, origin: (i32, i32), thickness: f32, out: &mut [Vertex]) -> usize {
    if hist.len() < 2 || g.w <= 0 || g.h <= 0 || g.span_ms == 0 {
        return 0;
    }
    let gx = origin.0 as f32 + g.x as f32;
    let gy = origin.1 as f32 + g.y as f32;
    let (gw, gh) = (g.w as f32, g.h as f32);
    let span = g.span_ms as i64 * 1_000_000;
    let cols = (g.w as usize).clamp(1, MAX_POINTS - 1);

    // Newest to oldest: (x, frame time ms), one point per pixel column (the slowest frame).
    let mut pts = [(0f32, 0f32); MAX_POINTS];
    let mut n = 0;
    let mut cur_col = usize::MAX;
    let mut slowest = 0f32;
    let newest = hist.get(0);
    for back in 0..hist.len() - 1 {
        let t1 = hist.get(back);
        let age = newest - t1;
        if age > span {
            break;
        }
        let ft = (t1 - hist.get(back + 1)) as f32 / 1e6;
        let frac = age as f32 / span as f32;
        let col = ((frac * cols as f32) as usize).min(cols - 1);
        if col != cur_col {
            if n == pts.len() {
                break;
            }
            pts[n] = (gx + gw - frac * gw, ft);
            n += 1;
            cur_col = col;
        } else if ft > pts[n - 1].1 {
            pts[n - 1].1 = ft;
        }
        slowest = slowest.max(ft);
    }
    if n < 2 {
        return 0;
    }

    // Ranges beyond 10 s are nonsense (and min + 1 == min for huge floats): clamp.
    let min = g.min_ms.clamp(0.0, 10_000.0);
    let max = if g.max_ms > min { g.max_ms.min(20_000.0) } else { auto_max(slowest.min(10_000.0), min) };
    if max <= min || !max.is_finite() {
        return 0;
    }
    let bottom = gy + gh;
    let y = |ft: f32| bottom - ((ft - min) / (max - min)).clamp(0.0, 1.0) * gh;
    let line = if g.line == 0 { DEFAULT_LINE } else { g.line };
    let level = |ft: f32| {
        if g.bad_above_ms != 0 && ft > g.bad_above_ms as f32 && g.bad != 0 {
            g.bad
        } else if g.warn_above_ms != 0 && ft > g.warn_above_ms as f32 && g.warn != 0 {
            g.warn
        } else {
            line
        }
    };

    let mut w = 0;
    let mut emit = |v: [([f32; 2], u32); 6]| {
        if w + 6 <= out.len() {
            for (pos, color) in v {
                out[w] = Vertex { pos, color };
                w += 1;
            }
        }
    };
    if g.fill != 0 {
        for i in 0..n - 1 {
            let (x0, y0) = (pts[i + 1].0, y(pts[i + 1].1));
            let (x1, y1) = (pts[i].0, y(pts[i].1));
            let c = g.fill;
            emit([([x0, y0], c), ([x1, y1], c), ([x1, bottom], c), ([x0, y0], c), ([x1, bottom], c), ([x0, bottom], c)]);
        }
    }
    let half = thickness.max(1.0) * 0.5;
    for i in 0..n - 1 {
        let (x0, y0) = (pts[i + 1].0, y(pts[i + 1].1));
        let (x1, y1) = (pts[i].0, y(pts[i].1));
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-3 {
            continue;
        }
        // Extend each segment by half the width along its direction so joins don't gap.
        let (ux, uy) = (dx / len * half, dy / len * half);
        let (nx, ny) = (-uy, ux);
        let (ax, ay, bx, by) = (x0 - ux, y0 - uy, x1 + ux, y1 + uy);
        let c = level(pts[i].1.max(pts[i + 1].1));
        let p = |x: f32, y: f32| [x, y];
        emit([
            (p(ax + nx, ay + ny), c),
            (p(ax - nx, ay - ny), c),
            (p(bx + nx, by + ny), c),
            (p(bx + nx, by + ny), c),
            (p(ax - nx, ay - ny), c),
            (p(bx - nx, by - ny), c),
        ]);
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(w: i32, h: i32) -> HudGraph {
        HudGraph { x: 10, y: 20, w, h, span_ms: 3000, series: 1, ..HudGraph::default() }
    }

    fn history(fps: f64, secs: f64) -> History {
        let mut h = History::new();
        let n = (fps * secs) as usize;
        for i in 0..n {
            h.push(1_000_000_000 + (i as f64 * 1e9 / fps) as i64);
        }
        h
    }

    #[test]
    fn history_ring() {
        let mut h = History::new();
        for i in 0..(HISTORY as i64 + 10) {
            h.push(i);
        }
        assert_eq!(h.len(), HISTORY);
        assert_eq!(h.get(0), HISTORY as i64 + 9);
        assert_eq!(h.get(HISTORY - 1), 10);
        h.push(5); // backwards: dropped
        assert_eq!(h.get(0), HISTORY as i64 + 9);
        h.clear();
        assert!(h.is_empty());
    }

    #[test]
    fn empty_and_degenerate() {
        let mut out = vec![Vertex::default(); VERTS_PER_GRAPH];
        assert_eq!(build(&History::new(), &graph(200, 50), (0, 0), 1.0, &mut out), 0);
        let h = history(60.0, 1.0);
        assert_eq!(build(&h, &graph(0, 50), (0, 0), 1.0, &mut out), 0);
        let mut one = History::new();
        one.push(1);
        assert_eq!(build(&one, &graph(200, 50), (0, 0), 1.0, &mut out), 0);
    }

    #[test]
    fn inside_the_rectangle_and_bounded() {
        let mut out = vec![Vertex::default(); VERTS_PER_GRAPH];
        for (fps, w) in [(30.0, 300), (144.0, 268), (1000.0, 2000), (5000.0, 64)] {
            let h = history(fps, 4.0);
            let g = graph(w, 40);
            let n = build(&h, &g, (100, 200), 2.0, &mut out);
            assert!(n > 0 && n.is_multiple_of(6) && n <= VERTS_PER_GRAPH, "{fps} {w}: {n}");
            for v in &out[..n] {
                // Within the rect, allowing the line's half width.
                assert!(v.pos[0] >= 110.0 - 2.0 && v.pos[0] <= 110.0 + w as f32 + 2.0, "{:?}", v.pos);
                assert!(v.pos[1] >= 220.0 - 2.0 && v.pos[1] <= 260.0 + 2.0, "{:?}", v.pos);
            }
            // Points are one per column at most.
            assert!(n / 6 < (w as usize).min(MAX_POINTS));
        }
    }

    #[test]
    fn small_output_buffer_never_overruns() {
        let h = history(144.0, 4.0);
        let mut out = vec![Vertex::default(); 13];
        let n = build(&h, &graph(300, 40), (0, 0), 1.0, &mut out);
        assert_eq!(n, 12);
    }

    #[test]
    fn hitch_is_kept_and_coloured() {
        // 1000 fps into 100 columns: many frames per column; one 50 ms hitch must survive.
        let mut h = History::new();
        let mut t = 1_000_000_000i64;
        for i in 0..2500 {
            t += if i == 1500 { 50_000_000 } else { 1_000_000 };
            h.push(t);
        }
        let bad = u32::from_le_bytes([200, 0, 0, 255]);
        let g = HudGraph { bad_above_ms: 30, bad, max_ms: 60.0, ..graph(100, 60) };
        let mut out = vec![Vertex::default(); VERTS_PER_GRAPH];
        let n = build(&h, &g, (0, 0), 1.0, &mut out);
        assert!(out[..n].iter().any(|v| v.color == bad));
        // The hitch reaches 50/60 of the height: y ≈ 20 + 60 - 50.
        let top = out[..n].iter().map(|v| v.pos[1]).fold(f32::MAX, f32::min);
        assert!((top - 30.0).abs() < 1.5, "{top}");
    }

    #[test]
    fn absurd_ranges_draw_nothing_or_finite() {
        let h = history(60.0, 4.0);
        let mut out = vec![Vertex::default(); VERTS_PER_GRAPH];
        for (min, max) in [(1e30, 0.0), (1e30, 2e30), (0.0, 1e30), (5.0, 0.0)] {
            let g = HudGraph { min_ms: min, max_ms: max, ..graph(200, 50) };
            let n = build(&h, &g, (0, 0), 1.0, &mut out);
            assert!(out[..n].iter().all(|v| v.pos[0].is_finite() && v.pos[1].is_finite()), "{min} {max}");
        }
    }

    #[test]
    fn fill_and_auto_range() {
        let h = history(60.0, 4.0);
        let g = HudGraph { fill: u32::from_le_bytes([10, 10, 10, 40]), ..graph(200, 50) };
        let mut out = vec![Vertex::default(); VERTS_PER_GRAPH];
        let n = build(&h, &g, (0, 0), 1.0, &mut out);
        // Fill + line, one segment per frame in the 3 s span.
        assert_eq!(n % 12, 0);
        assert!(n / 12 >= 170 && n / 12 <= 180, "{}", n / 12);
        assert_eq!(auto_max(16.7, 0.0), 20.0);
        assert_eq!(auto_max(16.7, 25.0), 33.4);
        assert_eq!(auto_max(5000.0, 0.0), 5750.0);
    }
}
