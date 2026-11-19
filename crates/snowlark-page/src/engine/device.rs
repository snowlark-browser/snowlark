// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What `navigator` says about the device and the user agent's built-in viewers: HTML §8.9.1.6 "PDF
//! viewing support" (`plugins`, `mimeTypes`, `pdfViewerEnabled`), Device Memory (`deviceMemory`),
//! Battery Status (`getBattery()`) and the Tracking Preference Expression's `doNotTrack`.

use snowlark_bindings::generated::{
    BatteryManagerImpl, MimeTypeArrayImpl, MimeTypeImpl, NavigatorDeviceMemoryImpl,
    NavigatorPluginsImpl, PluginArrayImpl, PluginImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle, JsPromise};

use snowlark_bindings::rt::ExceptionName;

use super::policy::Feature;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// HTML §8.9.1.6: the PDF viewer plugin names, in order.
const PDF_PLUGINS: [&str; 5] = [
    "PDF Viewer",
    "Chrome PDF Viewer",
    "Chromium PDF Viewer",
    "Microsoft Edge PDF Viewer",
    "WebKit built-in PDF",
];

/// HTML §8.9.1.6: the PDF viewer MIME types, in order.
const PDF_MIME_TYPES: [&str; 2] = ["application/pdf", "text/pdf"];

/// The `description` of every PDF viewer plugin and MIME type object.
const PDF_DESCRIPTION: &str = "Portable Document Format";

/// Device Memory §5: the upper bound of the reported value (Chrome's), in GiB. The engine does not
/// read the machine's memory (no safe API outside the crates allowed `unsafe`): it reports the
/// bound, which a machine with 8 GiB or more reports anyway.
const DEVICE_MEMORY_GIB: f64 = 8.0;

impl Engine {
    /// HTML "PDF viewer supported": whether the embedder shows PDFs inline when navigated to them.
    pub(crate) fn pdf_viewer_supported(&self) -> bool {
        self.embedder.borrow().pdf_viewer
    }

    /// Says whether PDFs show inline (HTML §8.9.1.6 "PDF viewer supported"): the engine's PDF viewer,
    /// on unless the embedder turns it off.
    pub fn set_pdf_viewer_supported(&self, supported: bool) {
        self.embedder.borrow_mut().pdf_viewer = supported;
    }

    /// A window's PDF viewer plugin object `index` (HTML's "PDF viewer plugin objects").
    fn pdf_plugin(&self, window: Handle, index: usize) -> Handle {
        const KEYS: [&str; 5] = ["plugin0", "plugin1", "plugin2", "plugin3", "plugin4"];
        self.window_singleton(window, KEYS[index], || Object::Plugin { window, index })
    }

    /// A window's PDF viewer MIME type object `index` (HTML's "PDF viewer mime type objects").
    fn pdf_mime_type(&self, window: Handle, index: usize) -> Handle {
        const KEYS: [&str; 2] = ["mimetype0", "mimetype1"];
        self.window_singleton(window, KEYS[index], || Object::MimeType { window, index })
    }

    fn plugin_window(&self, h: Handle) -> (Handle, usize) {
        match &*self.obj(h) {
            Object::Plugin { window, index }
            | Object::MimeType { window, index }
            | Object::PluginArray { window, index }
            | Object::MimeTypeArray { window, index } => (*window, *index),
            _ => (h, 0),
        }
    }

    /// The window of a Navigator (or the Navigator itself when it has none).
    pub(crate) fn navigator_window(&self, navigator: Handle) -> Handle {
        let f = self.frames.borrow();
        f.singletons
            .iter()
            .find_map(|((global, _), h)| (*h == navigator).then_some(*global))
            .unwrap_or(navigator)
    }

    fn plugin_count(&self) -> usize {
        if self.pdf_viewer_supported() {
            PDF_PLUGINS.len()
        } else {
            0
        }
    }

    fn mime_type_count(&self) -> usize {
        if self.pdf_viewer_supported() {
            PDF_MIME_TYPES.len()
        } else {
            0
        }
    }
}

impl NavigatorPluginsImpl for Engine {
    /// HTML §8.9.1.6: the PluginArray (one per window).
    fn plugins(&self, this: Handle) -> Handle {
        let window = self.navigator_window(this);
        self.window_singleton(window, "plugins", || Object::PluginArray {
            window,
            index: 0,
        })
    }
    fn mime_types(&self, this: Handle) -> Handle {
        let window = self.navigator_window(this);
        self.window_singleton(window, "mimeTypes", || Object::MimeTypeArray {
            window,
            index: 0,
        })
    }
    fn pdf_viewer_enabled(&self, _this: Handle) -> bool {
        self.pdf_viewer_supported()
    }
    fn java_enabled(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(false)
    }
}

impl PluginArrayImpl for Engine {
    /// "does nothing".
    fn refresh(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn length(&self, _this: Handle) -> u32 {
        self.plugin_count() as u32
    }
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        let (window, _) = self.plugin_window(this);
        ((index as usize) < self.plugin_count()).then(|| self.pdf_plugin(window, index as usize))
    }
    fn named_item(&self, this: Handle, name: &str) -> Option<Handle> {
        let (window, _) = self.plugin_window(this);
        let i = PDF_PLUGINS[..self.plugin_count()]
            .iter()
            .position(|p| *p == name)?;
        Some(self.pdf_plugin(window, i))
    }
    fn supported_property_names(&self, _this: Handle) -> Vec<String> {
        PDF_PLUGINS[..self.plugin_count()]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }
}

impl MimeTypeArrayImpl for Engine {
    fn length(&self, _this: Handle) -> u32 {
        self.mime_type_count() as u32
    }
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        let (window, _) = self.plugin_window(this);
        ((index as usize) < self.mime_type_count())
            .then(|| self.pdf_mime_type(window, index as usize))
    }
    fn named_item(&self, this: Handle, name: &str) -> Option<Handle> {
        let (window, _) = self.plugin_window(this);
        let i = PDF_MIME_TYPES[..self.mime_type_count()]
            .iter()
            .position(|t| *t == name)?;
        Some(self.pdf_mime_type(window, i))
    }
    fn supported_property_names(&self, _this: Handle) -> Vec<String> {
        PDF_MIME_TYPES[..self.mime_type_count()]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }
}

impl PluginImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(PDF_PLUGINS[self.plugin_window(this).1])
    }
    fn description(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(PDF_DESCRIPTION)
    }
    fn filename(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("internal-pdf-viewer")
    }
    /// Each PDF viewer plugin supports every PDF viewer MIME type.
    fn length(&self, _this: Handle) -> u32 {
        PDF_MIME_TYPES.len() as u32
    }
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        let (window, _) = self.plugin_window(this);
        ((index as usize) < PDF_MIME_TYPES.len())
            .then(|| self.pdf_mime_type(window, index as usize))
    }
    fn named_item(&self, this: Handle, name: &str) -> Option<Handle> {
        let (window, _) = self.plugin_window(this);
        let i = PDF_MIME_TYPES.iter().position(|t| *t == name)?;
        Some(self.pdf_mime_type(window, i))
    }
    fn supported_property_names(&self, _this: Handle) -> Vec<String> {
        PDF_MIME_TYPES.iter().map(|s| (*s).to_owned()).collect()
    }
}

impl MimeTypeImpl for Engine {
    fn type_(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(PDF_MIME_TYPES[self.plugin_window(this).1])
    }
    fn description(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(PDF_DESCRIPTION)
    }
    fn suffixes(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("pdf")
    }
    /// The "PDF Viewer" plugin object.
    fn enabled_plugin(&self, this: Handle) -> Handle {
        let (window, _) = self.plugin_window(this);
        self.pdf_plugin(window, 0)
    }
}

impl NavigatorDeviceMemoryImpl for Engine {
    fn device_memory(&self, _this: Handle) -> f64 {
        DEVICE_MEMORY_GIB
    }
}

impl Engine {
    /// The Tracking Preference Expression's `doNotTrack`: null, the user has expressed no preference.
    pub(crate) fn do_not_track(&self) -> Option<DomStr<'_>> {
        None
    }

    /// Battery Status §4 `getBattery()`: the navigator's battery promise, made once and resolved
    /// (in a task) with its BatteryManager; rejected with NotAllowedError when the document may not
    /// use the "battery" feature (a cross-origin frame: its default allowlist is 'self').
    pub(crate) fn get_battery(&self, cx: &mut Cx, navigator: Handle) -> rt::Result<JsPromise> {
        let window = self.navigator_window(navigator);
        let manager = self.window_singleton(window, "battery", || Object::BatteryManager {
            promise: None,
        });
        if let Object::BatteryManager {
            promise: Some(p), ..
        } = &*self.obj(manager)
        {
            return Ok(p.clone());
        }
        let allowed = self
            .document_of_window(window)
            .is_some_and(|doc| self.allowed_to_use(doc, Feature::Battery));
        if !allowed {
            let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
            p.reject(
                cx,
                exception(
                    ExceptionName::NotAllowedError,
                    "The \"battery\" feature is not allowed in this document",
                ),
            );
            return Ok(p);
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if let Object::BatteryManager { promise, .. } = &mut *self.obj_mut(manager) {
            *promise = Some(p.clone());
        }
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            resolve.resolve(cx, manager);
        });
        Ok(p)
    }
}

/// Battery Status §5: with no battery information, a fully charged battery that is charging.
impl BatteryManagerImpl for Engine {
    fn charging(&self, _this: Handle) -> bool {
        true
    }
    fn charging_time(&self, _this: Handle) -> f64 {
        0.0
    }
    fn discharging_time(&self, _this: Handle) -> f64 {
        f64::INFINITY
    }
    fn level(&self, _this: Handle) -> f64 {
        1.0
    }
}
