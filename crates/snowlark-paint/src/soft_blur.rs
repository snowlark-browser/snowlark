// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A Gaussian blur on the CPU, for the parts of a raster the GPU cannot blur right: an 8-bit coverage
//! mask blurred in place with three successive box blurs, the approximation Filter Effects 1 §9.7
//! (`feGaussianBlur`) allows for a standard deviation of 2 or more device pixels; under that, an exact
//! Gaussian kernel.

/// Blurs the `width` × `height` coverage `mask` (one byte a pixel, rows packed) with a Gaussian of
/// standard deviation `sigma` device pixels, treating everything outside it as 0.
pub fn blur_mask(mask: &mut [u8], width: usize, height: usize, sigma: f32) {
    debug_assert_eq!(mask.len(), width * height);
    if width == 0 || height == 0 || sigma.is_nan() || sigma <= 0.0 {
        return;
    }
    let mut line = Vec::new();
    let mut out = Vec::new();
    let passes = Passes::for_sigma(sigma);
    for y in 0..height {
        line.clear();
        line.extend_from_slice(&mask[y * width..(y + 1) * width]);
        passes.run(&mut line, &mut out);
        mask[y * width..(y + 1) * width].copy_from_slice(&line);
    }
    for x in 0..width {
        line.clear();
        line.extend((0..height).map(|y| mask[y * width + x]));
        passes.run(&mut line, &mut out);
        for (y, v) in line.iter().enumerate() {
            mask[y * width + x] = *v;
        }
    }
}

/// How one line is blurred.
enum Passes {
    /// Three box blurs: (size, the shift of each box's center from the pixel's: -1, 0 or +1 half a
    /// pixel, as `2 * offset` rounds).
    Boxes([(usize, isize); 3]),
    /// An exact kernel, from its center out.
    Kernel(Vec<f32>),
}

impl Passes {
    fn for_sigma(sigma: f32) -> Passes {
        if sigma >= 2.0 {
            // d = floor(s * 3 * sqrt(2 * pi) / 4 + 0.5) (Filter Effects 1 §9.7).
            let d =
                (sigma * 3.0 * (2.0 * std::f32::consts::PI).sqrt() / 4.0 + 0.5).floor() as usize;
            let d = d.max(1);
            if d % 2 == 1 {
                // Three boxes of size d centered on the output pixel.
                Passes::Boxes([(d, 0), (d, 0), (d, 0)])
            } else {
                // Two boxes of size d, the first centered on the boundary to the left of the output
                // pixel and the second on the one to its right, then one of size d + 1 centered.
                Passes::Boxes([(d, -1), (d, 1), (d + 1, 0)])
            }
        } else {
            let radius = (sigma * 3.0).ceil() as usize;
            let mut k: Vec<f32> = (0..=radius)
                .map(|i| (-((i * i) as f32) / (2.0 * sigma * sigma)).exp())
                .collect();
            let sum: f32 = k[0] + 2.0 * k[1..].iter().sum::<f32>();
            for v in &mut k {
                *v /= sum;
            }
            Passes::Kernel(k)
        }
    }

    fn run(&self, line: &mut [u8], out: &mut Vec<u8>) {
        match self {
            Passes::Boxes(boxes) => {
                for &(size, shift) in boxes {
                    box_pass(line, out, size, shift);
                    line.copy_from_slice(out);
                }
            }
            Passes::Kernel(k) => {
                out.clear();
                let n = line.len() as isize;
                for i in 0..n {
                    let mut acc = 0.0f32;
                    for (j, w) in k.iter().enumerate() {
                        let j = j as isize;
                        let at = |p: isize| {
                            if (0..n).contains(&p) {
                                line[p as usize] as f32
                            } else {
                                0.0
                            }
                        };
                        acc += w * if j == 0 { at(i) } else { at(i - j) + at(i + j) };
                    }
                    out.push(acc.round().clamp(0.0, 255.0) as u8);
                }
                line.copy_from_slice(out);
            }
        }
    }
}

/// One box blur of `size` pixels over `line` into `out`. The box over output pixel `i` covers
/// `i - size / 2 ..` `size` pixels for an odd size; for an even size, `shift` -1 puts its center on
/// the boundary left of `i` (it covers `i - size / 2 .. i + size / 2 - 1`), +1 on the one right of it.
fn box_pass(line: &[u8], out: &mut Vec<u8>, size: usize, shift: isize) {
    out.clear();
    let n = line.len() as isize;
    let size_i = size as isize;
    // (an even box centered right of the pixel starts one later)
    let later = isize::from(size.is_multiple_of(2) && shift > 0);
    let start_of = |i: isize| -> isize { i - size_i / 2 + later };
    let at = |p: isize| -> u32 {
        if (0..n).contains(&p) {
            line[p as usize] as u32
        } else {
            0
        }
    };
    let mut start = start_of(0);
    let mut sum: u32 = (start..start + size_i).map(at).sum();
    let half = size as u32 / 2;
    for i in 0..n {
        if i > 0 {
            sum -= at(start);
            start += 1;
            sum += at(start + size_i - 1);
        }
        out.push(((sum + half) / size as u32).min(255) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1-D Gaussian over a step from 255 to 0: the exact value at distance `x` from the edge.
    fn step(x: f32, sigma: f32) -> f32 {
        // 255 * (1 - Phi(x / sigma)), Phi by the error function's Abramowitz-Stegun approximation.
        let z = x / (sigma * std::f32::consts::SQRT_2);
        let t = 1.0 / (1.0 + 0.3275911 * z.abs());
        let y = 1.0
            - (((((1.061_405_4 * t - 1.453_152) * t) + 1.421_413_8) * t - 0.284_496_74) * t
                + 0.254_829_6)
                * t
                * (-z * z).exp();
        let erf = if z >= 0.0 { y } else { -y };
        255.0 * 0.5 * (1.0 - erf)
    }

    #[test]
    fn a_blurred_edge_follows_the_gaussian() {
        for sigma in [1.0f32, 3.0, 6.0, 7.5, 12.0] {
            // Opaque on the left half, the edge at x = 100; the middle row is far enough from the
            // top and bottom (which fade too) to see the edge alone.
            let (w, h) = (200usize, 81usize);
            let mut mask: Vec<u8> = (0..w * h)
                .map(|i| if i % w < 100 { 255 } else { 0 })
                .collect();
            let wide = (sigma * 3.0) as usize + 2;
            assert!(100 + wide < w && wide < 100 && wide < h / 2);
            blur_mask(&mut mask, w, h, sigma);
            let row = &mask[h / 2 * w..(h / 2 + 1) * w];
            for x in [90usize, 96, 99, 100, 103, 108] {
                let expected = step(x as f32 + 0.5 - 100.0, sigma);
                let got = row[x] as f32;
                assert!(
                    (got - expected).abs() <= 8.0,
                    "sigma {sigma}: x {x} is {got}, expected {expected}"
                );
            }
            // Far from the edge, the content and the clear part are untouched.
            assert_eq!(row[100 - wide - 1], 255, "sigma {sigma}");
            assert_eq!(row[100 + wide + 1], 0, "sigma {sigma}");
        }
    }

    #[test]
    fn a_blurred_mask_keeps_its_total() {
        let (w, h) = (80usize, 80usize);
        let mut mask = vec![0u8; w * h];
        for y in 30..50 {
            for x in 30..50 {
                mask[y * w + x] = 200;
            }
        }
        let before: u32 = mask.iter().map(|&v| v as u32).sum();
        blur_mask(&mut mask, w, h, 4.0);
        let after: u32 = mask.iter().map(|&v| v as u32).sum();
        assert!((after as f32 / before as f32 - 1.0).abs() < 0.03);
        // Symmetric about the square's center (39.5, 39.5).
        assert!((mask[39 * w + 25] as i32 - mask[40 * w + 54] as i32).abs() <= 2);
        assert_eq!(mask[0], 0);
    }
}
