// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pixel codes: every layer writes what it is (its size, the frame number) into a row of black and
//! white blocks, so a capture of the window says which frames of which sizes DWM actually showed.

pub const BLOCK: u32 = 8;
/// Where a layer's code starts, in the layer (clear of the window's rounded corners).
pub const ORIGIN: (u32, u32) = (24, 24);
const SENTINEL: [bool; 4] = [true, false, true, false];
/// Bits: frame number, width, height.
pub const FRAME_BITS: u32 = 20;
pub const SIZE_BITS: u32 = 12;
pub const TOTAL: u32 = SENTINEL.len() as u32 + FRAME_BITS + 2 * SIZE_BITS;

/// The blocks of a code, in order (true is white).
pub fn bits(frame: u32, w: u32, h: u32) -> Vec<bool> {
    let mut v: Vec<bool> = SENTINEL.to_vec();
    for i in 0..FRAME_BITS {
        v.push(frame >> i & 1 == 1);
    }
    for i in 0..SIZE_BITS {
        v.push(w >> i & 1 == 1);
    }
    for i in 0..SIZE_BITS {
        v.push(h >> i & 1 == 1);
    }
    v
}

/// Reads a code at `at` in a BGRA image: (frame, width, height); `None` if the sentinel isn't there.
pub fn read(pixels: &[u8], pitch: usize, at: (u32, u32)) -> Option<(u32, u32, u32)> {
    let bit = |i: u32| {
        let x = (at.0 + i * BLOCK + BLOCK / 2) as usize;
        let y = (at.1 + BLOCK / 2) as usize;
        let p = y * pitch + x * 4;
        pixels.get(p + 1).is_some_and(|g| *g > 127)
    };
    let n = SENTINEL.len() as u32;
    for (i, s) in SENTINEL.iter().enumerate() {
        if bit(i as u32) != *s {
            return None;
        }
    }
    let field = |start: u32, len: u32| (0..len).fold(0u32, |v, i| v | (bit(start + i) as u32) << i);
    Some((
        field(n, FRAME_BITS),
        field(n + FRAME_BITS, SIZE_BITS),
        field(n + FRAME_BITS + SIZE_BITS, SIZE_BITS),
    ))
}
