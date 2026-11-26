// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Canvas bitmaps drawn on a canvas thread, on Linux: none yet. [`RemoteCanvas::new`] fails, so a
//! canvas falls back to `snowlark-render`'s CPU bitmap, and the types of a canvas that cannot be made
//! have methods that never run (they hold an uninhabited value). They keep the Windows half's
//! methods so the crates above build.

use std::sync::Arc;

use snowlark_graphics::{Error, Result};

use crate::canvas::{CanvasClip, CanvasPaint, DrawParams, FillShape, PathData, Rgba, StrokeParams};
use crate::geom::{Point, Rect};
use crate::gpu::Driver;
use crate::image::DecodedImage;
use crate::text::{ShapedText, TextEngine};

/// Called on the canvas thread once a read's pixels are sent ([`RemoteCanvas::read_later`]): the
/// page thread's wake-up.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Makes the canvas thread's device ahead of the first canvas: nothing to make here.
pub fn prepare(_driver: Driver) {}

/// Whether [`prepare`] has made the device: never here.
pub fn is_prepared(_driver: Driver) -> bool {
    false
}

/// The decoded images kept uploaded on the canvas thread (count, bytes): none here.
pub fn kept_images(_driver: Driver) -> (usize, usize) {
    (0, 0)
}

/// An image on the canvas thread (`drawImage` sources, patterns).
#[derive(Clone)]
pub struct RemoteImage {
    pub width: u32,
    pub height: u32,
    #[allow(dead_code)] // (what makes the type uninhabited)
    never: std::convert::Infallible,
}

/// The pixels of a [`RemoteCanvas::read_later`], premultiplied BGRA.
pub struct PendingRead {
    pub width: u32,
    pub height: u32,
    never: std::convert::Infallible,
}

impl PendingRead {
    pub fn try_take(&self) -> Option<Result<Vec<u8>>> {
        match self.never {}
    }

    pub fn wait(self) -> Result<Vec<u8>> {
        match self.never {}
    }
}

/// A canvas bitmap drawn on the canvas thread: see the module documentation.
pub struct RemoteCanvas {
    never: std::convert::Infallible,
}

impl RemoteCanvas {
    /// Fails: there is no GPU canvas on this platform yet.
    pub fn new(_width: u32, _height: u32, _driver: Driver) -> Result<RemoteCanvas> {
        Err(Error::Unsupported)
    }

    pub fn size(&self) -> (u32, u32) {
        match self.never {}
    }

    pub fn fill(
        &mut self,
        _shape: FillShape,
        _paint: &CanvasPaint<RemoteImage>,
        _p: &DrawParams,
    ) -> Result<()> {
        match self.never {}
    }

    pub fn stroke(
        &mut self,
        _path: &PathData,
        _stroke: &StrokeParams,
        _paint: &CanvasPaint<RemoteImage>,
        _p: &DrawParams,
    ) -> Result<()> {
        match self.never {}
    }

    pub fn stroke_circle(
        &mut self,
        _center: Point,
        _radius: f32,
        _width: f32,
        _color: Rgba,
        _p: &DrawParams,
    ) -> Result<()> {
        match self.never {}
    }

    pub fn clear(&mut self, _shape: FillShape, _clip: Option<&Arc<CanvasClip>>) -> Result<()> {
        match self.never {}
    }

    pub fn draw_image(
        &mut self,
        _image: &RemoteImage,
        _src: Rect,
        _dst: Rect,
        _p: &DrawParams,
    ) -> Result<()> {
        match self.never {}
    }

    pub fn draw_frame(
        &mut self,
        _image: &Arc<DecodedImage>,
        _src: Rect,
        _dst: Rect,
        _p: &DrawParams,
    ) -> Result<()> {
        match self.never {}
    }

    pub fn draw_text(
        &mut self,
        _text: &Arc<TextEngine>,
        _shaped: &Arc<ShapedText>,
        _origin: Point,
        _paint: &CanvasPaint<RemoteImage>,
        _stroke: Option<&StrokeParams>,
        _p: &DrawParams,
    ) -> Result<()> {
        match self.never {}
    }

    pub fn draw_device_image(&mut self, _image: &RemoteImage, _p: &DrawParams) -> Result<()> {
        match self.never {}
    }

    pub fn image_from_bgra(&self, _width: u32, _height: u32, _bgra: &[u8]) -> Result<RemoteImage> {
        match self.never {}
    }

    pub fn snapshot(&mut self) -> RemoteImage {
        match self.never {}
    }

    pub fn can_draw(&self, _image: &RemoteImage) -> bool {
        match self.never {}
    }

    pub fn read(&mut self, _x: i32, _y: i32, _w: u32, _h: u32) -> Result<Vec<u8>> {
        match self.never {}
    }

    pub fn read_later(&mut self, _then: Option<Wake>) -> PendingRead {
        match self.never {}
    }

    pub fn start_read_all(&mut self) -> Result<()> {
        match self.never {}
    }

    pub fn write(&mut self, _x: i32, _y: i32, _w: u32, _h: u32, _bgra: &[u8]) -> Result<()> {
        match self.never {}
    }

    pub fn flush(&mut self) -> Result<()> {
        match self.never {}
    }
}
