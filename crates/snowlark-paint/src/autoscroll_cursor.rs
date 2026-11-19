// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The cursors of middle-button autoscroll: a small ring with a dot at the origin of the scroll and an
//! arrow head toward the direction the page scrolls (none at the origin itself: arrows on all four
//! sides, or two where the page can scroll one way only is the marker's business, not the cursor's).
//! Drawn here with hard edges (every pixel opaque or clear, so no alpha convention can go wrong).

/// The side of the cursor bitmap, and its hot spot (the ring's center).
pub const SIZE: usize = 32;
pub const HOT: usize = 16;

const BLACK: [u8; 4] = [0, 0, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

/// A filled triangle `tip`, `a`, `b` containing the point (all in bitmap pixels).
fn in_triangle(p: (f32, f32), tip: (f32, f32), a: (f32, f32), b: (f32, f32)) -> bool {
    let sign = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| {
        (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1)
    };
    let d1 = sign(p, tip, a);
    let d2 = sign(p, a, b);
    let d3 = sign(p, b, tip);
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

/// The arrow head toward `(dx, dy)` (each -1, 0 or 1): its tip and the two corners of its base.
fn arrow(dx: i8, dy: i8, grow: f32) -> [(f32, f32); 3] {
    let c = HOT as f32 + 0.5;
    let (fx, fy) = (f32::from(dx), f32::from(dy));
    let len = (fx * fx + fy * fy).sqrt().max(1.0);
    let (ux, uy) = (fx / len, fy / len);
    // (perpendicular)
    let (px, py) = (-uy, ux);
    let near = 8.5 - grow;
    let far = 14.5 + grow;
    let half = 4.5 + grow;
    let tip = (c + ux * far, c + uy * far);
    let base = (c + ux * near, c + uy * near);
    [
        tip,
        (base.0 + px * half, base.1 + py * half),
        (base.0 - px * half, base.1 - py * half),
    ]
}

/// The directions that get an arrow head for the cursor `(dx, dy)`: the one it points to, or all
/// four sides at the origin.
fn heads(dx: i8, dy: i8) -> Vec<(i8, i8)> {
    if dx == 0 && dy == 0 {
        vec![(0, -1), (0, 1), (-1, 0), (1, 0)]
    } else {
        vec![(dx.signum(), dy.signum())]
    }
}

/// The cursor for scrolling toward `(dx, dy)`: `SIZE` by `SIZE` pixels, BGRA, rows top to bottom.
pub fn bitmap(dx: i8, dy: i8) -> Vec<u8> {
    let heads = heads(dx, dy);
    let c = HOT as f32 + 0.5;
    let mut out = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let p = (x as f32 + 0.5, y as f32 + 0.5);
            let r = ((p.0 - c).powi(2) + (p.1 - c).powi(2)).sqrt();
            let in_head = |grow: f32| {
                heads.iter().any(|&(hx, hy)| {
                    let [t, a, b] = arrow(hx, hy, grow);
                    in_triangle(p, t, a, b)
                })
            };
            // Outline first (the shape grown by a pixel), then the fill on top.
            let px = if r <= 2.0 {
                Some(BLACK)
            } else if r <= 5.0 {
                Some(WHITE)
            } else if r <= 6.5 || in_head(-1.2) {
                Some(BLACK)
            } else if in_head(0.0) {
                Some(WHITE)
            } else {
                None
            };
            if let Some(c) = px {
                let i = (y * SIZE + x) * 4;
                out[i..i + 4].copy_from_slice(&c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(b: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * SIZE + x) * 4;
        [b[i], b[i + 1], b[i + 2], b[i + 3]]
    }

    fn opaque(b: &[u8], x: usize, y: usize) -> bool {
        at(b, x, y)[3] == 255
    }

    #[test]
    fn the_ring_is_at_the_hot_spot_and_the_head_points_where_it_scrolls() {
        for (dx, dy) in [(0, 0), (0, -1), (0, 1), (-1, 0), (1, 0), (1, 1), (-1, -1)] {
            let b = bitmap(dx, dy);
            assert_eq!(b.len(), SIZE * SIZE * 4);
            // The dot at the center is black, the ring around it white.
            assert_eq!(at(&b, HOT, HOT), BLACK, "({dx},{dy})");
            assert_eq!(at(&b, HOT + 4, HOT), WHITE, "({dx},{dy})");
            // Every pixel is opaque or clear.
            assert!(b.chunks(4).all(|p| p[3] == 0 || p[3] == 255));
        }
        // Up: pixels above the ring, none below it; down is its mirror.
        let up = bitmap(0, -1);
        assert!(opaque(&up, HOT, HOT - 12));
        assert!(!opaque(&up, HOT, HOT + 12));
        let down = bitmap(0, 1);
        assert!(opaque(&down, HOT, HOT + 12));
        assert!(!opaque(&down, HOT, HOT - 12));
        // Left and right.
        assert!(opaque(&bitmap(-1, 0), HOT - 12, HOT) && !opaque(&bitmap(-1, 0), HOT + 12, HOT));
        assert!(opaque(&bitmap(1, 0), HOT + 12, HOT) && !opaque(&bitmap(1, 0), HOT - 12, HOT));
        // The origin has heads on all four sides.
        let o = bitmap(0, 0);
        for (x, y) in [
            (HOT, HOT - 12),
            (HOT, HOT + 12),
            (HOT - 12, HOT),
            (HOT + 12, HOT),
        ] {
            assert!(opaque(&o, x, y));
        }
        // The cursors differ by direction.
        assert_ne!(bitmap(0, -1), bitmap(0, 1));
        assert_ne!(bitmap(1, 1), bitmap(1, 0));
    }
}
