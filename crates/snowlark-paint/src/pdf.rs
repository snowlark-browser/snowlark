// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The PDF viewer's rasterizer: a PDF file's pages as PNG images, through the Windows PDF renderer
//! (`Windows.Data.Pdf`, part of Windows since 8.1). The page side builds the viewer document from
//! these (HTML's "inline content that doesn't have a DOM", `snowlark-page`'s loader).

use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream};

/// Whether this system renders PDF files.
pub const AVAILABLE: bool = true;

/// One rendered page.
pub struct PdfPage {
    /// The page's size in CSS px (the renderer's device-independent pixels, 96 per inch).
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

/// Renders the first `max_pages` pages of the PDF file `bytes`, `scale` device pixels per CSS px.
/// An error for a file the renderer cannot open (not a PDF, damaged, password protected).
pub fn render(bytes: &[u8], scale: f32, max_pages: usize) -> Result<RenderedPdf, String> {
    crate::com::ensure();
    let err = |e: windows_core::Error| e.message();
    let input = InMemoryRandomAccessStream::new().map_err(err)?;
    let writer = DataWriter::CreateDataWriter(&input).map_err(err)?;
    writer.WriteBytes(bytes).map_err(err)?;
    writer.StoreAsync().map_err(err)?.join().map_err(err)?;
    writer.DetachStream().map_err(err)?;
    input.Seek(0).map_err(err)?;
    let doc = PdfDocument::LoadFromStreamAsync(&input)
        .map_err(err)?
        .join()
        .map_err(err)?;
    if doc.IsPasswordProtected().unwrap_or(false) {
        return Err("password protected".into());
    }
    let page_count = doc.PageCount().map_err(err)? as usize;
    let mut pages = Vec::new();
    for i in 0..page_count.min(max_pages) {
        let page = doc.GetPage(i as u32).map_err(err)?;
        // (the size of the page as shown: its crop box, rotated)
        let size = page.Size().map_err(err)?;
        let options = PdfPageRenderOptions::new().map_err(err)?;
        options
            .SetDestinationWidth(((size.Width * scale).round() as u32).max(1))
            .map_err(err)?;
        options
            .SetDestinationHeight(((size.Height * scale).round() as u32).max(1))
            .map_err(err)?;
        let output = InMemoryRandomAccessStream::new().map_err(err)?;
        page.RenderWithOptionsToStreamAsync(&output, &options)
            .map_err(err)?
            .join()
            .map_err(err)?;
        let len = output.Size().map_err(err)? as u32;
        let reader =
            DataReader::CreateDataReader(&output.GetInputStreamAt(0).map_err(err)?).map_err(err)?;
        reader.LoadAsync(len).map_err(err)?.join().map_err(err)?;
        let mut png = vec![0u8; len as usize];
        reader.ReadBytes(&mut png).map_err(err)?;
        pages.push(PdfPage {
            width: size.Width,
            height: size.Height,
            png,
        });
    }
    Ok(RenderedPdf { page_count, pages })
}
