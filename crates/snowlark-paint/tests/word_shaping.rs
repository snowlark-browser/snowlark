// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text shaped per word through a `TextCache` draws exactly the pixels of text shaped as whole runs:
//! Latin with kerned spaces, Greek, Cyrillic, Arabic, Devanagari and mixed lines, painted headless.

use snowlark_paint::*;

const LINES: &[&str] = &[
    "AVATAR Tom Yes \u{201C}You\u{201D} Wave To Your office affine fluffy waffle.",
    "na\u{EF}ve caf\u{E9} cafe\u{301} 12,345.67 $99 \u{20AC}5 \u{2014} dash \u{2026}",
    "\u{39A}\u{3B1}\u{3BB}\u{3B7}\u{3BC}\u{3AD}\u{3C1}\u{3B1} \u{3BA}\u{3CC}\u{3C3}\u{3BC}\u{3B5} \u{41F}\u{440}\u{438}\u{432}\u{435}\u{442} \u{43C}\u{438}\u{440}",
    "\u{645}\u{631}\u{62D}\u{628}\u{627} \u{628}\u{627}\u{644}\u{639}\u{627}\u{644}\u{645} \u{627}\u{644}\u{639}\u{631}\u{628}\u{64A}\u{629}",
    "\u{928}\u{92E}\u{938}\u{94D}\u{924}\u{947} \u{926}\u{941}\u{928}\u{93F}\u{92F}\u{93E} \u{915}\u{94D}\u{937}\u{924}\u{94D}\u{930}\u{93F}\u{92F}",
    "Mixed abc \u{645}\u{631}\u{62D}\u{628}\u{627} def 123 \u{928}\u{92E}\u{938}\u{94D}\u{924}\u{947} end.",
];

fn render(text: &TextEngine, shaped: &[ShapedText]) -> Pixmap {
    let mut b = DisplayListBuilder::new();
    for (i, s) in shaped.iter().enumerate() {
        s.paint(
            0..s.clusters.len(),
            Point::new(6.0, 30.0 + 34.0 * i as f32),
            Color::BLACK,
            &mut b,
        );
    }
    let list = b.build();
    let images = ImageStore::new();
    HeadlessRenderer::new()
        .unwrap()
        .render(
            &list,
            Sources {
                text,
                images: &images,
            },
            &Viewport {
                width: 900,
                height: 34 * LINES.len() as u32 + 20,
                scale: 1.0,
                background: Color::WHITE,
            },
        )
        .unwrap()
}

#[test]
fn word_shaped_text_draws_the_pixels_of_whole_runs() {
    let text = TextEngine::new().unwrap();
    for family in ["Segoe UI", "Arial", "Times New Roman"] {
        let list = text
            .font_list(&FontQuery {
                families: &[family],
                weight: 400,
                style: FontStyle::Normal,
                stretch: FontStretch::NORMAL,
            })
            .unwrap();
        let options = ShapeOptions {
            size: 22.0,
            ..Default::default()
        };
        let mut cache = TextCache::default();
        let mut whole = Vec::new();
        let mut words = Vec::new();
        for line in LINES {
            let mut s = ShapedText::default();
            text.shape(line, list, &options, &mut s).unwrap();
            whole.push(s);
            // Shaped twice through the cache: the second time from cached words.
            let mut w = ShapedText::default();
            for _ in 0..2 {
                text.shape_cached(line, list, &options, Some(&mut cache.words), &mut w)
                    .unwrap();
            }
            words.push(w);
        }
        assert!(cache.words.stats().hits > 0);
        let a = render(&text, &whole);
        let b = render(&text, &words);
        let inked = a.data.chunks(4).filter(|p| p[0] < 128).count();
        assert!(inked > 2000, "{family}: {inked}");
        // Which lines differ, and from where: a failure names the words to look at.
        let mut lines: Vec<(usize, u32)> = Vec::new();
        let mut differ = 0;
        for (i, (x, y)) in a.data.chunks(4).zip(b.data.chunks(4)).enumerate() {
            if x != y {
                differ += 1;
                let (px, py) = (i as u32 % a.width, i as u32 / a.width);
                // (line i's baseline is at 30 + 34 i)
                let line = (py.saturating_sub(8) / 34) as usize;
                match lines.iter_mut().find(|(l, _)| *l == line) {
                    Some((_, from)) => *from = (*from).min(px),
                    None => lines.push((line, px)),
                }
            }
        }
        let at: Vec<String> = lines
            .iter()
            .map(|&(l, px)| format!("line {l} {:?} from x {px}", LINES.get(l)))
            .collect();
        assert_eq!(differ, 0, "{family}: {differ} pixels differ: {at:?}");
    }
}
