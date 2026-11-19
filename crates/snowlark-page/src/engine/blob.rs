// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The File API (W3C File API): `Blob`, `File`, `FileList`, `FileReader`, and the blob URL store behind
//! `URL.createObjectURL` (snowlark-net's `BlobStore`, which `fetch` resolves `blob:` URLs against).

use std::rc::Rc;

use snowlark_bindings::generated::{
    ArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataViewOrBlobOrString as BlobPart,
    BlobImpl, BlobPropertyBag, EndingType, FileImpl, FileListImpl, FilePropertyBag, FileReaderImpl,
    StringOrArrayBuffer,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle, JsObject, JsPromise};
use snowlark_bindings::InterfaceId;

use super::encoding::buffer_bytes;
use super::events::EventExt;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// A Blob's (or File's) data.
pub struct BlobData {
    pub bytes: Rc<Vec<u8>>,
    /// The type, lowercased ASCII, or empty.
    pub type_: String,
    /// For a File: its name and last modified time (milliseconds since the epoch).
    pub file: Option<(String, i64)>,
}

/// A FileReader's state (File API §6).
#[derive(Default)]
pub struct ReaderData {
    pub state: u16,
    pub result: Option<ReaderResult>,
    pub error: Option<Handle>,
    /// Bumped by `abort()` and each read, so a queued completion of an older read does nothing.
    pub generation: u32,
}

pub enum ReaderResult {
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Clone, Copy)]
enum ReadAs {
    ArrayBuffer,
    BinaryString,
    Text,
    DataUrl,
}

/// A type string as the Blob constructor keeps it: lowercased, or empty when it has a character outside
/// U+0020..U+007E.
fn normalize_type(t: &str) -> String {
    if t.chars().all(|c| ('\u{20}'..='\u{7E}').contains(&c)) {
        t.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// "Convert line endings to native" (LF here: the platform convention the spec allows).
fn native_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A new ArrayBuffer holding `bytes`.
pub(crate) fn array_buffer(cx: &mut Cx, bytes: Vec<u8>) -> JsObject {
    let scope = cx.scope();
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
    JsObject::new(scope, ab.into())
}

/// A new Uint8Array holding `bytes`.
pub(crate) fn uint8_array(cx: &mut Cx, bytes: Vec<u8>) -> rt::Result<JsObject> {
    let len = bytes.len();
    let scope = cx.scope();
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
    let arr = v8::Uint8Array::new(scope, ab, 0, len).ok_or(rt::Error::Pending)?;
    Ok(JsObject::new(scope, arr.into()))
}

impl Engine {
    pub(crate) fn new_blob(&self, bytes: Vec<u8>, type_: &str) -> Handle {
        self.alloc(Object::Blob(BlobData {
            bytes: Rc::new(bytes),
            type_: normalize_type(type_),
            file: None,
        }))
    }

    /// A Blob whose type is `type_` as given, not normalized as the constructor's is (Fetch's
    /// `blob()` gives the body's serialized MIME type, whose parameter values keep their case).
    pub(crate) fn new_blob_with_exact_type(&self, bytes: Vec<u8>, type_: &str) -> Handle {
        self.alloc(Object::Blob(BlobData {
            bytes: Rc::new(bytes),
            type_: type_.to_owned(),
            file: None,
        }))
    }

    pub(crate) fn new_file(
        &self,
        bytes: Vec<u8>,
        type_: &str,
        name: &str,
        modified: i64,
    ) -> Handle {
        self.alloc(Object::Blob(BlobData {
            bytes: Rc::new(bytes),
            type_: normalize_type(type_),
            file: Some((name.to_owned(), modified)),
        }))
    }

    /// A blob's bytes and type.
    pub(crate) fn blob_contents(&self, h: Handle) -> (Rc<Vec<u8>>, String) {
        match &*self.obj(h) {
            Object::Blob(b) => (b.bytes.clone(), b.type_.clone()),
            _ => (Rc::new(Vec::new()), String::new()),
        }
    }

    /// A File's name and last modification date (none for a Blob that is not a File).
    pub(crate) fn blob_file(&self, h: Handle) -> Option<(String, i64)> {
        match &*self.obj(h) {
            Object::Blob(b) => b.file.clone(),
            _ => None,
        }
    }

    pub(crate) fn blob_file_name(&self, h: Handle) -> Option<String> {
        match &*self.obj(h) {
            Object::Blob(b) => b.file.as_ref().map(|(n, _)| n.clone()),
            _ => None,
        }
    }

    /// "Process blob parts".
    fn blob_parts(&self, cx: &mut Cx, parts: Vec<BlobPart>, endings: EndingType) -> Vec<u8> {
        let mut out = Vec::new();
        for p in parts {
            match p {
                BlobPart::String(s) => {
                    let s = if endings == EndingType::Native {
                        native_line_endings(&s)
                    } else {
                        s
                    };
                    out.extend_from_slice(s.as_bytes());
                }
                BlobPart::Blob(b) => out.extend_from_slice(&self.blob_contents(b).0),
                BlobPart::ArrayBuffer(o)
                | BlobPart::Int8Array(o)
                | BlobPart::Int16Array(o)
                | BlobPart::Int32Array(o)
                | BlobPart::Uint8Array(o)
                | BlobPart::Uint16Array(o)
                | BlobPart::Uint32Array(o)
                | BlobPart::Uint8ClampedArray(o)
                | BlobPart::BigInt64Array(o)
                | BlobPart::BigUint64Array(o)
                | BlobPart::Float16Array(o)
                | BlobPart::Float32Array(o)
                | BlobPart::Float64Array(o)
                | BlobPart::DataView(o) => out.extend_from_slice(&buffer_bytes(cx, &o)),
            }
        }
        out
    }

    fn reader(&self, h: Handle) -> std::cell::RefMut<'_, ReaderData> {
        std::cell::RefMut::map(self.obj_mut(h), |o| match o {
            Object::FileReader(r) => &mut **r,
            _ => panic!("not a FileReader"),
        })
    }

    /// File API §6.4.1 "read operation": queues the events and the result.
    fn read_blob(
        &self,
        reader: Handle,
        blob: Handle,
        as_: ReadAs,
        encoding: Option<String>,
    ) -> rt::Result<()> {
        let generation = {
            let mut r = self.reader(reader);
            if r.state == 1 {
                return Err(exception(
                    rt::ExceptionName::InvalidStateError,
                    "The FileReader is already reading",
                ));
            }
            r.state = 1;
            r.result = None;
            r.error = None;
            r.generation += 1;
            r.generation
        };
        let (bytes, type_) = self.blob_contents(blob);
        self.queue_task(TaskSource::Networking, move |e, cx| {
            if e.reader(reader).generation != generation {
                return;
            }
            e.fire_progress(cx, reader, "loadstart", 0, bytes.len() as u64, true);
            if e.reader(reader).generation != generation {
                return;
            }
            let result = match as_ {
                ReadAs::ArrayBuffer => ReaderResult::Bytes(bytes.to_vec()),
                ReadAs::BinaryString => {
                    ReaderResult::Text(bytes.iter().map(|&b| b as char).collect())
                }
                ReadAs::Text => {
                    // The encoding: the argument, the blob's type's charset, UTF-8; a BOM wins.
                    let label = encoding.clone().or_else(|| {
                        snowlark_net::mime::MimeType::parse(&type_)
                            .and_then(|m| m.parameter("charset").map(str::to_owned))
                    });
                    let enc = label
                        .and_then(|l| encoding_rs::Encoding::for_label(l.trim().as_bytes()))
                        .unwrap_or(encoding_rs::UTF_8);
                    let (text, _, _) = enc.decode(&bytes);
                    ReaderResult::Text(text.into_owned())
                }
                ReadAs::DataUrl => {
                    let t = if type_.is_empty() {
                        "application/octet-stream"
                    } else {
                        type_.as_str()
                    };
                    ReaderResult::Text(format!(
                        "data:{t};base64,{}",
                        super::window::base64_encode(&bytes)
                    ))
                }
            };
            {
                let mut r = e.reader(reader);
                r.state = 2;
                r.result = Some(result);
            }
            let n = bytes.len() as u64;
            e.fire_progress(cx, reader, "progress", n, n, true);
            e.fire_progress(cx, reader, "load", n, n, true);
            if e.reader(reader).state == 2 && e.reader(reader).generation == generation {
                e.fire_progress(cx, reader, "loadend", n, n, true);
            }
        });
        Ok(())
    }

    /// Fires a ProgressEvent at `target`.
    pub(crate) fn fire_progress(
        &self,
        cx: &mut Cx,
        target: Handle,
        type_: &str,
        loaded: u64,
        total: u64,
        length_computable: bool,
    ) {
        let ev = self.trusted_event(
            InterfaceId::ProgressEvent,
            type_,
            false,
            false,
            EventExt::Progress {
                length_computable,
                loaded: loaded as f64,
                total: total as f64,
            },
        );
        self.dispatch(cx, ev, target, false);
    }

    /// `URL.createObjectURL(blob)`.
    pub(crate) fn create_object_url(&self, cx: &mut Cx, blob: Handle) -> rt::Result<String> {
        let (bytes, type_) = self.blob_contents(blob);
        let doc = self.current_document(cx)?;
        let origin = self.net_origin_of_document(doc);
        let net = self.hooks.borrow().net.clone();
        Ok(match net {
            Some(net) => net.register_blob(
                &origin,
                snowlark_net::blob::BlobData::new(bytes.to_vec().into(), &type_),
            ),
            None => {
                // No network: a URL nothing resolves.
                format!("blob:{}/{}", origin.ascii_serialization(), uuid_like())
            }
        })
    }

    /// The blob URL entry `url` names, held for a request of it
    /// ([`snowlark_net::Request::blob_url_entry`]): URL Standard §4.4 resolves a `blob:` URL as it is
    /// parsed, so a fetch reads the blob the URL named then, whatever `revokeObjectURL` did since.
    /// `None` for any other URL, or one that names no blob.
    pub(crate) fn hold_blob_url(&self, url: &url::Url) -> Option<u64> {
        if url.scheme() != "blob" {
            return None;
        }
        self.hooks
            .borrow()
            .net
            .as_ref()
            .and_then(|net| net.hold_blob_url(url))
    }

    pub(crate) fn revoke_object_url(&self, cx: &mut Cx, url: &str) {
        let Ok(u) = url::Url::parse(url) else {
            return;
        };
        let Ok(doc) = self.current_document(cx) else {
            return;
        };
        let origin = self.net_origin_of_document(doc);
        if let Some(net) = self.hooks.borrow().net.as_ref() {
            net.revoke_blob(&u, &origin);
        }
    }

    /// The origin of a document as snowlark-net sees it (`about:` documents take their creator's).
    pub(crate) fn net_origin_of_document(&self, doc: snowlark_dom::NodeId) -> url::Origin {
        if let Some(o) = self.sandboxed_origin(doc) {
            return o;
        }
        // (an `about:` document's is its creator's, as of when it was made)
        if let Some(i) = self.frames.borrow().inherited_origins.get(&doc) {
            return i.net_origin.clone();
        }
        let url = self.effective_document_url(doc);
        snowlark_net::origin::origin_of(&url, None)
    }

    /// The URL that stands for a document's origin and referrer: its own, or for `about:blank` and
    /// `about:srcdoc` documents the one of the document they took their origin from when they were
    /// made (the initiator of the navigation to them, else the nearest ancestor document that is not
    /// `about:`). It is never the URL of what that ancestor navigated to since, or a navigation's
    /// initiator would get requests made with another origin.
    pub(crate) fn effective_document_url(&self, doc: snowlark_dom::NodeId) -> url::Url {
        let mut d = Some(doc);
        while let Some(cur) = d {
            if let Ok(u) = url::Url::parse(self.dom().document_url(cur)) {
                if u.scheme() != "about" {
                    return u;
                }
            }
            if let Some(i) = self.frames.borrow().inherited_origins.get(&cur) {
                return i.url.clone();
            }
            d = self.parent_document(cur);
        }
        // (no ancestor to take it from: an opaque origin, not the top-level document's)
        url::Url::parse("about:blank").expect("about:blank")
    }
}

fn uuid_like() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        n as u32,
        (n >> 32) as u16,
        (n >> 48) as u16 & 0xfff,
        (n >> 60) as u16 & 0xfff,
        n as u64 & 0xffff_ffff_ffff
    )
}

impl BlobImpl for Engine {
    /// File API §3.3.4 `stream()`: the blob's bytes as a byte stream.
    fn stream(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let bytes = self.blob_contents(this).0.to_vec();
        Ok(self.create_readable_stream(
            cx,
            Rc::new(super::streams::readable::BytesSource {
                bytes: std::cell::RefCell::new(Some(bytes)),
            }),
            0.0,
            true,
        ))
    }
    /// The blob's UTF-8 text as a stream of strings.
    fn text_stream(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let bytes = self.blob_contents(this).0;
        let (text, _) = encoding_rs::UTF_8.decode_with_bom_removal(&bytes);
        Ok(self.create_readable_stream(
            cx,
            Rc::new(super::streams::readable::TextSource {
                text: std::cell::RefCell::new(Some(text.into_owned())),
            }),
            1.0,
            false,
        ))
    }
    fn size(&self, this: Handle) -> u64 {
        self.blob_contents(this).0.len() as u64
    }
    fn type_(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.blob_contents(this).1)
    }
    /// `slice(start, end, contentType)`: relative positions clamp to the size.
    fn slice(
        &self,
        _cx: &mut Cx,
        this: Handle,
        start: Option<i64>,
        end: Option<i64>,
        content_type: Option<&str>,
    ) -> rt::Result<Handle> {
        let (bytes, _) = self.blob_contents(this);
        let size = bytes.len() as i64;
        let clamp = |v: i64| {
            if v < 0 {
                (size + v).max(0)
            } else {
                v.min(size)
            }
        };
        let from = clamp(start.unwrap_or(0));
        let to = clamp(end.unwrap_or(size));
        let span = (to - from).max(0);
        let slice = bytes[from as usize..(from + span) as usize].to_vec();
        Ok(self.new_blob(slice, content_type.unwrap_or("")))
    }
    fn text(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (bytes, _) = self.blob_contents(this);
        let (text, _) = encoding_rs::UTF_8.decode_with_bom_removal(&bytes);
        p.resolve(cx, DomStr::Owned(text.into_owned()));
        Ok(p)
    }
    fn array_buffer(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (bytes, _) = self.blob_contents(this);
        let ab = array_buffer(cx, bytes.to_vec());
        p.resolve(cx, ab);
        Ok(p)
    }
    fn bytes(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (bytes, _) = self.blob_contents(this);
        let arr = uint8_array(cx, bytes.to_vec())?;
        p.resolve(cx, arr);
        Ok(p)
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        blob_parts: Option<Vec<BlobPart>>,
        options: BlobPropertyBag,
    ) -> rt::Result<Handle> {
        let bytes = self.blob_parts(cx, blob_parts.unwrap_or_default(), options.endings);
        Ok(self.new_blob(bytes, &options.type_))
    }
}

impl FileImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.blob_file_name(this).unwrap_or_default())
    }
    fn last_modified(&self, this: Handle) -> i64 {
        match &*self.obj(this) {
            Object::Blob(b) => b.file.as_ref().map(|(_, m)| *m).unwrap_or(0),
            _ => 0,
        }
    }
    fn webkit_relative_path(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        file_bits: Vec<BlobPart>,
        file_name: &str,
        options: FilePropertyBag,
    ) -> rt::Result<Handle> {
        let bytes = self.blob_parts(cx, file_bits, options.endings);
        let modified = options.last_modified.unwrap_or_else(now_ms);
        Ok(self.new_file(bytes, &options.type_, file_name, modified))
    }
}

impl FileListImpl for Engine {
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        match &*self.obj(this) {
            Object::FileList(l) => l.get(index as usize).copied(),
            _ => None,
        }
    }
    fn length(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::FileList(l) => l.len() as u32,
            _ => 0,
        }
    }
}

impl FileReaderImpl for Engine {
    fn ready_state(&self, this: Handle) -> u16 {
        self.reader(this).state
    }
    fn result(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<StringOrArrayBuffer>> {
        let r = match self.reader(this).result.as_ref() {
            None => None,
            Some(ReaderResult::Text(t)) => Some(Err(t.clone())),
            Some(ReaderResult::Bytes(b)) => Some(Ok(b.clone())),
        };
        Ok(match r {
            None => None,
            Some(Err(t)) => Some(StringOrArrayBuffer::String(t)),
            Some(Ok(b)) => Some(StringOrArrayBuffer::ArrayBuffer(array_buffer(cx, b))),
        })
    }
    fn error(&self, this: Handle) -> Option<Handle> {
        self.reader(this).error
    }
    fn read_as_array_buffer(&self, _cx: &mut Cx, this: Handle, blob: Handle) -> rt::Result<()> {
        self.read_blob(this, blob, ReadAs::ArrayBuffer, None)
    }
    fn read_as_binary_string(&self, _cx: &mut Cx, this: Handle, blob: Handle) -> rt::Result<()> {
        self.read_blob(this, blob, ReadAs::BinaryString, None)
    }
    fn read_as_text(
        &self,
        _cx: &mut Cx,
        this: Handle,
        blob: Handle,
        encoding: Option<&str>,
    ) -> rt::Result<()> {
        self.read_blob(this, blob, ReadAs::Text, encoding.map(str::to_owned))
    }
    fn read_as_data_url(&self, _cx: &mut Cx, this: Handle, blob: Handle) -> rt::Result<()> {
        self.read_blob(this, blob, ReadAs::DataUrl, None)
    }
    fn abort(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let reading = {
            let mut r = self.reader(this);
            if r.state == 0 || r.state == 2 {
                r.result = None;
                return Ok(());
            }
            r.state = 2;
            r.result = None;
            r.generation += 1;
            true
        };
        if reading {
            self.fire_progress(cx, this, "abort", 0, 0, false);
            if self.reader(this).state != 1 {
                self.fire_progress(cx, this, "loadend", 0, 0, false);
            }
        }
        Ok(())
    }
    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        Ok(self.alloc(Object::FileReader(Box::default())))
    }
}
