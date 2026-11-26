// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The text engine on Linux, before it has one: [`TextEngine`] with the Windows engine's methods,
//! so the crates above it build, and no fonts. [`TextEngine::new`] fails, and an engine that cannot
//! be made has methods that never run (the compiler proves it: the type holds an uninhabited
//! value). The engine on fontconfig, a shaper and glyph outlines replaces this (docs/linux.md,
//! stage 3).

use crate::display_list::FontId;
use crate::text_cache::WordCache;
use crate::web_font::FontLoadError;
pub use snowlark_graphics::text::*;
use snowlark_graphics::{Error, Result};

/// The error of the text engine's calls.
pub type TextEngineError = Error;

/// A text engine with no fonts: see the module documentation.
pub struct TextEngine {
    pub(crate) never: std::convert::Infallible,
}

impl TextEngine {
    /// Fails: there is no text engine on this platform yet.
    pub fn new() -> Result<TextEngine> {
        Err(Error::Unsupported)
    }

    /// The system font database.
    pub fn font_database(&self) -> std::sync::Arc<fontdb::Database> {
        match self.never {}
    }

    /// Changes when `font` comes to stand for another face.
    pub fn font_revision(&self, _font: FontId) -> Option<u32> {
        match self.never {}
    }

    /// Where a glyph's outline crosses the band from `top` to `bottom` (`text-decoration-skip-ink`).
    pub fn glyph_intercept(
        &self,
        _font: FontId,
        _size: f32,
        _glyph: u16,
        _top: f32,
        _bottom: f32,
    ) -> Option<(f32, f32)> {
        match self.never {}
    }

    pub fn family_name(&self, _font: FontId) -> Option<String> {
        match self.never {}
    }

    pub fn weight(&self, _font: FontId) -> Option<u16> {
        match self.never {}
    }

    /// Registers a web font from memory; returns its family name.
    pub fn add_font_data(&self, _data: &[u8]) -> std::result::Result<String, FontLoadError> {
        match self.never {}
    }

    /// Registers a font file of the system (`local()`); returns its family name.
    pub fn add_local_font(&self, _data: &[u8]) -> Result<String> {
        match self.never {}
    }

    pub fn has_family(&self, _name: &str) -> bool {
        match self.never {}
    }

    pub fn font_list(&self, _query: &FontQuery) -> Result<FontListId> {
        match self.never {}
    }

    pub fn primary_font(&self, _list: FontListId) -> FontId {
        match self.never {}
    }

    pub fn metrics(&self, _font: FontId, _size: f32) -> FontMetrics {
        match self.never {}
    }

    pub fn space_advance(&self, _font: FontId, _size: f32) -> f32 {
        match self.never {}
    }

    pub fn measure(&self, _text: &str, _list: FontListId, _size: f32) -> Result<f32> {
        match self.never {}
    }

    pub fn measure_upright(&self, _text: &str, _list: FontListId, _size: f32) -> Result<f32> {
        match self.never {}
    }

    pub fn shape(
        &self,
        _text: &str,
        _list: FontListId,
        _options: &ShapeOptions,
        _out: &mut ShapedText,
    ) -> Result<()> {
        match self.never {}
    }

    pub fn shape_cached(
        &self,
        _text: &str,
        _list: FontListId,
        _options: &ShapeOptions,
        _cache: Option<&mut WordCache>,
        _out: &mut ShapedText,
    ) -> Result<()> {
        match self.never {}
    }
}
