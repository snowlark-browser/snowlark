// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The PDF viewer's rasterizer on Linux: none yet (PDFium is stage 9 of docs/linux.md), so the
//! engine starts with HTML's "PDF viewer supported" false and PDF documents download instead.

/// Whether this system renders PDF files.
pub const AVAILABLE: bool = false;

/// One rendered page.
pub struct PdfPage {
    /// The page's size in CSS px.
    pub width: f32,
    pub height: f32,
    /// The page as a PNG, `scale` device pixels per CSS px.
    pub png: Vec<u8>,
}

/// A PDF file's pages, rendered.
pub struct RenderedPdf {
    pub page_count: usize,
    /// The first `max_pages` pages.
    pub pages: Vec<PdfPage>,
}

/// Always an error here: there is no PDF renderer.
pub fn render(_bytes: &[u8], _scale: f32, _max_pages: usize) -> Result<RenderedPdf, String> {
    Err("no PDF renderer on this system".into())
}
