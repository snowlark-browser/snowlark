// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The PDF viewer's rasterizer (`snowlark_paint::pdf`).

use snowlark_paint::pdf;

/// A PDF file of pages (width, height in points), the first with a red square 72 pt (1 in) wide at
/// its top left corner (one inch in from the left, one in from the top).
pub fn small_pdf(pages: &[(u32, u32)]) -> Vec<u8> {
    let n = pages.len();
    let mut objects: Vec<String> = Vec::new();
    // 1: catalog, 2: pages, then per page a page object and a content stream.
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {n} >>",
        kids.join(" ")
    ));
    for (i, &(w, h)) in pages.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents {} 0 R /Resources << >> >>",
            4 + 2 * i
        ));
        let content = if i == 0 {
            format!("1 0 0 rg 72 {} 72 72 re f", h - 144)
        } else {
            String::new()
        };
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len() + 1
        ));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[test]
fn pages_render_as_pngs_of_their_size() {
    // US Letter (612 x 792 pt) and A5 landscape (595 x 420 pt): 816 x 1056 and 793.3 x 560 CSS px.
    let file = small_pdf(&[(612, 792), (595, 420)]);
    let r = pdf::render(&file, 1.0, 10).expect("renders");
    assert_eq!(r.page_count, 2);
    assert_eq!(r.pages.len(), 2);
    assert_eq!((r.pages[0].width, r.pages[0].height), (816.0, 1056.0));
    assert!((r.pages[1].width - 793.33).abs() < 0.1 && (r.pages[1].height - 560.0).abs() < 0.1);
    let img = snowlark_paint::image::decode(&r.pages[0].png).expect("a PNG");
    let f = &img.frames[0];
    assert_eq!((f.width, f.height), (816, 1056));
    // The square: an inch (96 px) in from the left and the top, an inch wide; white around it.
    let px = |x: u32, y: u32| {
        let i = ((y * f.width + x) * 4) as usize;
        (f.pixels[i], f.pixels[i + 1], f.pixels[i + 2])
    };
    let (b, g, r_) = px(144, 144);
    assert!(r_ > 200 && g < 60 && b < 60, "red inside: {:?}", (r_, g, b));
    let (b, g, r_) = px(48, 48);
    assert!(
        r_ > 240 && g > 240 && b > 240,
        "white outside: {:?}",
        (r_, g, b)
    );
    // A scale renders more device pixels; `max_pages` stops early but counts them all.
    let r = pdf::render(&file, 2.0, 1).unwrap();
    assert_eq!((r.page_count, r.pages.len()), (2, 1));
    let f = &snowlark_paint::image::decode(&r.pages[0].png)
        .unwrap()
        .frames[0];
    assert_eq!((f.width, f.height), (1632, 2112));
}

#[test]
fn what_is_not_a_pdf_is_an_error() {
    assert!(pdf::render(b"<html>not a pdf</html>", 1.0, 10).is_err());
}
