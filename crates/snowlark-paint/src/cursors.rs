// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CSS cursors Windows has no system cursor for, drawn here: white shapes with a black outline,
//! as the system arrow is, so they show on any page. Hard edges: every pixel opaque or clear, so no
//! alpha convention can go wrong.

use crate::input::Cursor;

/// The width and height of a cursor, in pixels.
pub const SIZE: usize = 32;

/// A cursor's pixels, ready for the window.
pub struct Drawn {
    /// BGRA, top row first.
    pub bgra: Vec<u8>,
    /// One bit a pixel, set where it is clear, in rows of `SIZE / 8` bytes.
    pub mask: Vec<u8>,
    /// The point that is the pointer's position.
    pub hot: (u32, u32),
}

impl Drawn {
    /// The cursor of `bgra`, whose pixels are all opaque or clear.
    fn new(bgra: Vec<u8>, hot: (u32, u32)) -> Drawn {
        let mut mask = vec![0; SIZE * SIZE / 8];
        for i in 0..SIZE * SIZE {
            if bgra[i * 4 + 3] == 0 {
                mask[i / 8] |= 0x80 >> (i % 8);
            }
        }
        Drawn { bgra, mask, hot }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Ink {
    Clear,
    White,
    Black,
}

struct Art([Ink; SIZE * SIZE]);

impl Art {
    fn new() -> Art {
        Art([Ink::Clear; SIZE * SIZE])
    }

    fn get(&self, x: i32, y: i32) -> Ink {
        let inside = (0..SIZE as i32).contains(&x) && (0..SIZE as i32).contains(&y);
        if inside {
            self.0[y as usize * SIZE + x as usize]
        } else {
            Ink::Clear
        }
    }

    /// Paints every pixel whose centre `inside` accepts.
    fn fill(&mut self, ink: Ink, inside: impl Fn(f32, f32) -> bool) {
        for y in 0..SIZE {
            for x in 0..SIZE {
                if inside(x as f32 + 0.5, y as f32 + 0.5) {
                    self.0[y * SIZE + x] = ink;
                }
            }
        }
    }

    /// Paints the pixels from `(x0, y0)` up to, not including, `(x1, y1)`.
    fn rect(&mut self, ink: Ink, x0: usize, y0: usize, x1: usize, y1: usize) {
        for y in y0..y1 {
            self.0[y * SIZE + x0..y * SIZE + x1].fill(ink);
        }
    }

    fn polygon(&mut self, ink: Ink, points: &[(f32, f32)]) {
        self.fill(ink, |x, y| {
            let mut inside = false;
            let mut last = points[points.len() - 1];
            for &p in points {
                if (p.1 > y) != (last.1 > y)
                    && x < (last.0 - p.0) * (y - p.1) / (last.1 - p.1) + p.0
                {
                    inside = !inside;
                }
                last = p;
            }
            inside
        });
    }

    /// Paints `#` of `rows` with `ink`, its first row and column at `(x, y)`.
    fn rows(&mut self, ink: Ink, x: usize, y: usize, rows: &[&str]) {
        for (j, row) in rows.iter().enumerate() {
            for (i, c) in row.bytes().enumerate() {
                if c == b'#' {
                    self.0[(y + j) * SIZE + x + i] = ink;
                }
            }
        }
    }

    /// Outlines what is white, in the clear pixels around it.
    fn outline(&mut self) {
        let before = Art(self.0);
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
                let near =
                    (-1..=1).any(|dy| (-1..=1).any(|dx| before.get(x + dx, y + dy) == Ink::White));
                if near && before.get(x, y) == Ink::Clear {
                    self.0[y as usize * SIZE + x as usize] = Ink::Black;
                }
            }
        }
    }

    fn finish(self, hot: (u32, u32)) -> Drawn {
        let mut bgra = Vec::with_capacity(SIZE * SIZE * 4);
        for ink in &self.0 {
            bgra.extend_from_slice(match ink {
                Ink::Clear => &[0, 0, 0, 0],
                Ink::White => &[255, 255, 255, 255],
                Ink::Black => &[0, 0, 0, 255],
            });
        }
        Drawn::new(bgra, hot)
    }
}

/// The cursors [`draw`] draws.
pub const DRAWN: [Cursor; 8] = [
    Cursor::VerticalText,
    Cursor::Alias,
    Cursor::Copy,
    Cursor::ContextMenu,
    Cursor::ZoomIn,
    Cursor::ZoomOut,
    Cursor::Grab,
    Cursor::Grabbing,
];

/// The arrow, its tip at `ARROW_HOT`.
const ARROW: [(f32, f32); 7] = [
    (1.0, 1.0),
    (1.0, 17.0),
    (5.0, 13.0),
    (8.0, 20.0),
    (10.5, 19.0),
    (7.5, 12.0),
    (12.5, 12.0),
];
const ARROW_HOT: (u32, u32) = (1, 1);

/// The arrow with a white box beside it, which `detail` marks.
fn arrow_with_box(detail: impl FnOnce(&mut Art)) -> Drawn {
    let mut a = Art::new();
    a.polygon(Ink::White, &ARROW);
    a.rect(Ink::White, 14, 15, 27, 28);
    a.outline();
    detail(&mut a);
    a.finish(ARROW_HOT)
}

/// A magnifying glass with a white lens, which `detail` marks.
fn magnifier(detail: impl FnOnce(&mut Art)) -> Drawn {
    let mut a = Art::new();
    a.fill(Ink::White, |x, y| {
        (x - 12.0) * (x - 12.0) + (y - 12.0) * (y - 12.0) <= 9.5 * 9.5
    });
    a.polygon(
        Ink::White,
        &[(18.0, 20.5), (20.5, 18.0), (28.0, 25.5), (25.5, 28.0)],
    );
    a.outline();
    detail(&mut a);
    a.finish((12, 12))
}

/// The cursors [`draw_system`] draws: those a desktop's cursor theme normally has.
pub const SYSTEM: [Cursor; 13] = [
    Cursor::Arrow,
    Cursor::Hand,
    Cursor::IBeam,
    Cursor::Wait,
    Cursor::Progress,
    Cursor::Crosshair,
    Cursor::Move,
    Cursor::NotAllowed,
    Cursor::ResizeEW,
    Cursor::ResizeNS,
    Cursor::ResizeNWSE,
    Cursor::ResizeNESW,
    Cursor::Help,
];

/// An hourglass between `top` and `bottom`, `half` wide on each side of `cx`.
fn hourglass(a: &mut Art, cx: f32, top: f32, bottom: f32, half: f32) {
    let mid = (top + bottom) / 2.0;
    let cap = (bottom - top) / 7.0;
    a.polygon(
        Ink::White,
        &[
            (cx - half, top),
            (cx + half, top),
            (cx + half, top + cap),
            (cx + 1.5, mid),
            (cx + half, bottom - cap),
            (cx + half, bottom),
            (cx - half, bottom),
            (cx - half, bottom - cap),
            (cx - 1.5, mid),
            (cx - half, top + cap),
        ],
    );
}

/// A two-headed arrow through the centre, along `angle` (degrees, clockwise from pointing
/// right).
fn double_arrow(angle: f32) -> Drawn {
    let (sin, cos) = angle.to_radians().sin_cos();
    let reach = if angle % 90.0 == 0.0 { 13.5 } else { 12.5 };
    let mut a = Art::new();
    a.fill(Ink::White, |x, y| {
        let (dx, dy) = (x - 16.0, y - 16.0);
        let along = (dx * cos + dy * sin).abs();
        let across = (-dx * sin + dy * cos).abs();
        let head = reach - 6.5;
        along <= reach && (across <= 1.5 || (along >= head && across <= reach - along))
    });
    a.outline();
    a.finish((16, 16))
}

/// What `cursor` looks like where the system has no image for it (a desktop with no cursor
/// theme), for those a system normally has ([`SYSTEM`]); the others are [`draw`]'s.
pub fn draw_system(cursor: Cursor) -> Option<Drawn> {
    Some(match cursor {
        Cursor::Arrow => {
            let mut a = Art::new();
            a.polygon(Ink::White, &ARROW);
            a.outline();
            a.finish(ARROW_HOT)
        }
        Cursor::Hand => {
            let mut a = Art::new();
            a.rows(
                Ink::White,
                7,
                3,
                &[
                    "....##.........",
                    "....##.........",
                    "....##.........",
                    "....##.........",
                    "....##.##......",
                    "....##.##.##...",
                    "....##.##.##.##",
                    "##..##.##.##.##",
                    "###.###########",
                    ".##############",
                    "..#############",
                    "..#############",
                    "...###########.",
                    "...###########.",
                    "....#########..",
                    "....#########..",
                ],
            );
            a.outline();
            a.finish((11, 3))
        }
        Cursor::IBeam => {
            let mut a = Art::new();
            a.rect(Ink::White, 15, 5, 17, 27);
            a.rect(Ink::White, 11, 5, 21, 7);
            a.rect(Ink::White, 11, 25, 21, 27);
            a.outline();
            a.finish((16, 16))
        }
        Cursor::Wait => {
            let mut a = Art::new();
            hourglass(&mut a, 16.0, 4.0, 28.0, 7.0);
            a.outline();
            a.finish((16, 16))
        }
        Cursor::Progress => {
            let mut a = Art::new();
            a.polygon(Ink::White, &ARROW);
            hourglass(&mut a, 21.0, 15.0, 29.0, 4.5);
            a.outline();
            a.finish(ARROW_HOT)
        }
        Cursor::Crosshair => {
            let mut a = Art::new();
            a.rect(Ink::White, 4, 15, 28, 17);
            a.rect(Ink::White, 15, 4, 17, 28);
            a.outline();
            a.finish((16, 16))
        }
        Cursor::Move => {
            let mut a = Art::new();
            a.rect(Ink::White, 8, 15, 24, 17);
            a.rect(Ink::White, 15, 8, 17, 24);
            a.polygon(Ink::White, &[(16.0, 2.0), (21.5, 8.0), (10.5, 8.0)]);
            a.polygon(Ink::White, &[(16.0, 30.0), (10.5, 24.0), (21.5, 24.0)]);
            a.polygon(Ink::White, &[(2.0, 16.0), (8.0, 10.5), (8.0, 21.5)]);
            a.polygon(Ink::White, &[(30.0, 16.0), (24.0, 21.5), (24.0, 10.5)]);
            a.outline();
            a.finish((16, 16))
        }
        Cursor::NotAllowed => {
            let mut a = Art::new();
            a.fill(Ink::White, |x, y| {
                let (dx, dy) = (x - 16.0, y - 16.0);
                let r2 = dx * dx + dy * dy;
                let ring = (7.5 * 7.5..=11.0 * 11.0).contains(&r2);
                // (the bar, from top left to bottom right, inside the ring)
                let bar = (dx - dy).abs() <= 2.5 && r2 <= 8.0 * 8.0;
                ring || bar
            });
            a.outline();
            a.finish((16, 16))
        }
        Cursor::ResizeEW => double_arrow(0.0),
        Cursor::ResizeNS => double_arrow(90.0),
        Cursor::ResizeNWSE => double_arrow(45.0),
        Cursor::ResizeNESW => double_arrow(135.0),
        Cursor::Help => arrow_with_box(|a| {
            a.rows(
                Ink::Black,
                18,
                17,
                &[
                    ".###.", "#...#", "....#", "...#.", "..#..", "..#..", ".....", "..#..",
                ],
            );
        }),
        _ => return None,
    })
}

/// What `cursor` looks like, for those Windows has no cursor for.
pub fn draw(cursor: Cursor) -> Option<Drawn> {
    Some(match cursor {
        Cursor::VerticalText => {
            let mut a = Art::new();
            a.rect(Ink::White, 7, 15, 25, 17);
            a.rect(Ink::White, 5, 11, 7, 21);
            a.rect(Ink::White, 25, 11, 27, 21);
            a.outline();
            a.finish((16, 16))
        }
        Cursor::Copy => arrow_with_box(|a| {
            a.rect(Ink::Black, 19, 17, 22, 26);
            a.rect(Ink::Black, 16, 20, 25, 23);
        }),
        Cursor::Alias => arrow_with_box(|a| {
            a.polygon(
                Ink::Black,
                &[(17.0, 24.0), (19.0, 26.0), (24.0, 21.0), (22.0, 19.0)],
            );
            a.polygon(Ink::Black, &[(18.0, 17.0), (25.0, 17.0), (25.0, 24.0)]);
        }),
        Cursor::ContextMenu => arrow_with_box(|a| {
            a.rect(Ink::Black, 17, 18, 24, 19);
            a.rect(Ink::Black, 17, 21, 24, 22);
            a.rect(Ink::Black, 17, 24, 24, 25);
        }),
        Cursor::ZoomIn => magnifier(|a| {
            a.rect(Ink::Black, 7, 11, 17, 13);
            a.rect(Ink::Black, 11, 7, 13, 17);
        }),
        Cursor::ZoomOut => magnifier(|a| {
            a.rect(Ink::Black, 7, 11, 17, 13);
        }),
        Cursor::Grab => {
            let mut a = Art::new();
            a.rows(
                Ink::White,
                7,
                5,
                &[
                    ".......##........",
                    "....##.##.##.....",
                    "....##.##.##.....",
                    "....##.##.##.##..",
                    "....##.##.##.##..",
                    "....##.##.##.##..",
                    "....##.##.##.##..",
                    "....###########..",
                    "##..###########..",
                    "###.###########..",
                    ".##############..",
                    "..#############..",
                    "..#############..",
                    "...###########...",
                    "....##########...",
                    ".....########....",
                    ".....########....",
                ],
            );
            a.outline();
            a.finish((15, 13))
        }
        Cursor::Grabbing => {
            let mut a = Art::new();
            a.rows(
                Ink::White,
                7,
                9,
                &[
                    "....##.##.##.##..",
                    "...############..",
                    "...#############.",
                    "..##############.",
                    ".###############.",
                    ".###############.",
                    "..##############.",
                    "...############..",
                    "....##########...",
                    ".....########....",
                    ".....########....",
                ],
            );
            a.outline();
            // (the fingers, folded)
            for x in [13, 16, 19] {
                a.rect(Ink::Black, x, 10, x + 1, 14);
            }
            a.finish((15, 14))
        }
        Cursor::AutoScroll(dx, dy) => {
            let hot = crate::autoscroll_cursor::HOT as u32;
            Drawn::new(crate::autoscroll_cursor::bitmap(dx, dy), (hot, hot))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(d: &Drawn, x: usize, y: usize) -> &[u8] {
        &d.bgra[(y * SIZE + x) * 4..][..4]
    }

    #[test]
    fn each_is_drawn_and_differs_from_the_others() {
        let all: Vec<Drawn> = DRAWN.iter().map(|&c| draw(c).unwrap()).collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.bgra, b.bgra);
            }
        }
    }

    #[test]
    fn system_cursors_are_not_drawn() {
        assert!(draw(Cursor::Arrow).is_none());
        assert!(draw(Cursor::Hidden).is_none());
    }

    /// Every cursor [`draw`] or [`draw_system`] draws.
    fn every_drawn() -> Vec<(Cursor, Drawn)> {
        let drawn = DRAWN.iter().map(|&c| (c, draw(c).unwrap()));
        let system = SYSTEM.iter().map(|&c| (c, draw_system(c).unwrap()));
        drawn.chain(system).collect()
    }

    #[test]
    fn the_system_set_is_drawn_and_differs_from_the_others() {
        let all = every_drawn();
        for (i, (c, a)) in all.iter().enumerate() {
            for (d, b) in &all[i + 1..] {
                assert_ne!(a.bgra, b.bgra, "{c:?} and {d:?}");
            }
        }
        assert!(draw_system(Cursor::Copy).is_none());
        assert!(draw_system(Cursor::Hidden).is_none());
    }

    /// Every pixel is opaque black, opaque white or clear, and the mask says which.
    #[test]
    fn pixels_are_hard_edged_and_the_mask_agrees() {
        for (c, d) in every_drawn() {
            assert_eq!(d.bgra.len(), SIZE * SIZE * 4);
            assert_eq!(d.mask.len(), SIZE * SIZE / 8);
            for i in 0..SIZE * SIZE {
                let p = &d.bgra[i * 4..i * 4 + 4];
                assert!(matches!(
                    p,
                    [0, 0, 0, 0] | [0, 0, 0, 255] | [255, 255, 255, 255]
                ));
                let clear = d.mask[i / 8] & (0x80 >> (i % 8)) != 0;
                assert_eq!(clear, p[3] == 0, "{c:?} at {i}");
            }
        }
    }

    /// A white shape with nothing white on a clear pixel's edge: it shows on any background.
    #[test]
    fn white_is_always_outlined() {
        for (c, d) in every_drawn() {
            let mut white = 0;
            for y in 0..SIZE {
                for x in 0..SIZE {
                    if px(&d, x, y)[0] != 255 {
                        continue;
                    }
                    white += 1;
                    assert!(x > 0 && y > 0 && x < SIZE - 1 && y < SIZE - 1, "{c:?}");
                    for (dx, dy) in [(0, 1), (2, 1), (1, 0), (1, 2)] {
                        assert_ne!(px(&d, x + dx - 1, y + dy - 1)[3], 0, "{c:?} at {x},{y}");
                    }
                }
            }
            assert!(white > 20, "{c:?}");
        }
    }

    /// Autoscroll's cursors come through here too, with a mask that says what is clear.
    #[test]
    fn autoscroll_cursors_are_drawn_with_a_mask() {
        assert_eq!(crate::autoscroll_cursor::SIZE, SIZE);
        let d = draw(Cursor::AutoScroll(1, -1)).unwrap();
        assert_eq!(d.hot, (16, 16));
        for i in 0..SIZE * SIZE {
            let clear = d.mask[i / 8] & (0x80 >> (i % 8)) != 0;
            assert_eq!(clear, d.bgra[i * 4 + 3] == 0, "at {i}");
        }
        assert!(d.mask.iter().any(|&b| b != 0xFF));
    }

    /// The hot spot is on the cursor, where the pointer seems to be.
    #[test]
    fn the_hot_spot_is_on_the_drawing() {
        for (c, d) in every_drawn() {
            let (x, y) = (d.hot.0 as usize, d.hot.1 as usize);
            assert_ne!(px(&d, x, y)[3], 0, "{c:?}");
        }
    }
}
