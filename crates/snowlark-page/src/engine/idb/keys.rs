// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IndexedDB §7: converting values to keys and keys to values, evaluating key paths and injecting
//! keys; key ranges (§2.9) and `IDBKeyRange` (§4.7); `IDBRecord` (§4.8); `DOMStringList` (HTML
//! §2.6.? "DOMStringList"), in which IndexedDB returns names.

use snowlark_bindings::generated::{DOMStringListImpl, IDBKeyRangeImpl, IDBRecordImpl};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle, JsValue};
use snowlark_idb::{Key, KeyPath, KeyRange};

use super::{IdbObject, RecordData};
use crate::engine::{Engine, Object};

/// The result of converting a value to a key (§7.4).
pub(crate) enum Conv {
    Key(Key),
    InvalidValue,
    InvalidType,
}

impl Conv {
    pub(crate) fn key(self) -> Option<Key> {
        match self {
            Conv::Key(k) => Some(k),
            _ => None,
        }
    }
}

/// A "DataError" DOMException.
pub(crate) fn data_error(message: &'static str) -> Error {
    Error::dom(ExceptionName::DataError, message)
}

fn utf16(scope: &v8::PinScope<'_, '_>, s: v8::Local<v8::String>) -> Vec<u16> {
    let mut buf = vec![0u16; s.length()];
    s.write_v2(scope, 0, &mut buf, v8::WriteFlags::empty());
    buf
}

/// One step of "convert a value to a key": a value that is not an array converts on its own; an
/// array is opened, and its entries convert after it.
enum Step<'s> {
    Done(Conv),
    Array(v8::Local<'s, v8::Object>, u32),
}

/// An array being converted: its entries so far.
struct ArrayFrame<'s> {
    array: v8::Local<'s, v8::Object>,
    len: u32,
    next: u32,
    keys: Vec<Key>,
}

/// §7.4 "convert a value to a key". `Err(())`: an exception is pending. `seen` holds the arrays
/// being converted around `input`. The specification's recursion over nested arrays runs here over
/// a list on the heap, so a deep key needs no deeper native stack.
pub(crate) fn value_to_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<'s, v8::Value>,
    seen: &mut Vec<v8::Local<'s, v8::Value>>,
) -> Result<Conv, ()> {
    let base = seen.len();
    let r = convert_nested(scope, input, seen);
    seen.truncate(base);
    r
}

fn convert_nested<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<'s, v8::Value>,
    seen: &mut Vec<v8::Local<'s, v8::Value>>,
) -> Result<Conv, ()> {
    let mut open: Vec<ArrayFrame<'s>> = Vec::new();
    let mut step = convert_step(scope, input, seen)?;
    loop {
        let mut result = match step {
            Step::Done(c) => Some(c),
            Step::Array(array, len) => {
                seen.push(array.into());
                open.push(ArrayFrame {
                    array,
                    len,
                    next: 0,
                    keys: Vec::new(),
                });
                None
            }
        };
        // Hand each finished key to the array around it, until an entry is left to convert.
        step = loop {
            let Some(top) = open.last_mut() else {
                // (the outermost value is converted: `result` is set)
                return Ok(result.unwrap_or(Conv::InvalidValue));
            };
            match result.take() {
                Some(Conv::Key(k)) => top.keys.push(k),
                Some(_) => return Ok(Conv::InvalidValue),
                None => {}
            }
            if top.next < top.len {
                let i = top.next;
                top.next += 1;
                let array = top.array;
                let idx = v8::Integer::new_from_unsigned(scope, i);
                let idx_name = idx.to_string(scope).ok_or(())?;
                if !array.has_own_property(scope, idx_name.into()).ok_or(())? {
                    return Ok(Conv::InvalidValue);
                }
                let entry = array.get_index(scope, i).ok_or(())?;
                break convert_step(scope, entry, seen)?;
            }
            let keys = open.pop().map(|f| f.keys).unwrap_or_default();
            seen.pop();
            result = Some(Conv::Key(Key::Array(keys)));
        };
    }
}

/// Converts a value that is not an array, or opens an array (steps 1 to 6 of the array case).
fn convert_step<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<'s, v8::Value>,
    seen: &[v8::Local<'s, v8::Value>],
) -> Result<Step<'s>, ()> {
    if seen.iter().any(|s| s.strict_equals(input)) {
        return Ok(Step::Done(Conv::InvalidValue));
    }
    if !input.is_array() {
        return Ok(Step::Done(leaf_to_key(scope, input)?));
    }
    // (an array nested deeper than keys may cross between processes is an invalid value, a
    // "DataError", rather than a key that overflows a thread's stack: the specification sets no
    // depth, and engines refuse very deep keys)
    if seen.len() >= snowlark_idb::wire::WIRE_KEY_DEPTH as usize {
        return Ok(Step::Done(Conv::InvalidValue));
    }
    let arr: v8::Local<v8::Object> = input.try_into().map_err(|_| ())?;
    let len_key = rt::name(scope, "length");
    let len = arr.get(scope, len_key.into()).ok_or(())?;
    let len = len
        .integer_value(scope)
        .ok_or(())?
        .clamp(0, u32::MAX as i64) as u32;
    Ok(Step::Array(arr, len))
}

/// "Convert a value to a key" for every value but an array.
fn leaf_to_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<'s, v8::Value>,
) -> Result<Conv, ()> {
    if input.is_number() {
        let n = input.number_value(scope).unwrap_or(f64::NAN);
        return Ok(if n.is_nan() {
            Conv::InvalidValue
        } else {
            Conv::Key(Key::Number(n))
        });
    }
    if input.is_date() {
        let d: v8::Local<v8::Date> = input.try_into().map_err(|_| ())?;
        let ms = d.value_of();
        return Ok(if ms.is_nan() {
            Conv::InvalidValue
        } else {
            Conv::Key(Key::Date(ms))
        });
    }
    if input.is_string() {
        let s: v8::Local<v8::String> = input.try_into().map_err(|_| ())?;
        return Ok(Conv::Key(Key::String(utf16(scope, s))));
    }
    if input.is_shared_array_buffer() {
        return Ok(Conv::InvalidType);
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(input) {
        if ab.was_detached() {
            return Ok(Conv::InvalidValue);
        }
        let mut out = vec![0u8; ab.byte_length()];
        if !out.is_empty() {
            // (copied through a view: no unsafe access to the backing store)
            let view = v8::Uint8Array::new(scope, ab, 0, out.len()).ok_or(())?;
            view.copy_contents(&mut out);
        }
        return Ok(Conv::Key(Key::Binary(out)));
    }
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(input) {
        let Some(buffer) = view.buffer(scope) else {
            return Ok(Conv::InvalidValue);
        };
        let b: v8::Local<v8::Value> = buffer.into();
        if b.is_shared_array_buffer() {
            return Ok(Conv::InvalidType);
        }
        if buffer.was_detached() {
            return Ok(Conv::InvalidValue);
        }
        let mut out = vec![0u8; view.byte_length()];
        view.copy_contents(&mut out);
        return Ok(Conv::Key(Key::Binary(out)));
    }
    Ok(Conv::InvalidType)
}

/// §7.4 "convert a value to a multiEntry key".
pub(crate) fn value_to_multi_entry_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<'s, v8::Value>,
) -> Result<Conv, ()> {
    if !input.is_array() {
        return value_to_key(scope, input, &mut Vec::new());
    }
    let arr: v8::Local<v8::Object> = input.try_into().map_err(|_| ())?;
    let len_key = rt::name(scope, "length");
    let len = arr.get(scope, len_key.into()).ok_or(())?;
    let len = len.integer_value(scope).ok_or(())?.max(0) as u64;
    let mut keys: Vec<Key> = Vec::new();
    for i in 0..len {
        v8::tc_scope!(let tc, scope);
        let Some(entry) = arr.get_index(tc, i as u32) else {
            tc.reset();
            continue;
        };
        let mut seen = vec![input];
        match value_to_key(tc, entry, &mut seen) {
            Ok(Conv::Key(k)) => {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            Ok(_) => {}
            Err(()) => tc.reset(),
        }
    }
    Ok(Conv::Key(Key::Array(keys)))
}

/// §7.3 "convert a key to a value". Nested arrays are converted from a list on the heap, as in
/// [`value_to_key`].
pub(crate) fn key_to_value<'s>(
    scope: &v8::PinScope<'s, '_>,
    key: &Key,
) -> Option<v8::Local<'s, v8::Value>> {
    /// An array key being converted: its keys, the next one to convert, and the values so far.
    struct Open<'k, 's> {
        items: &'k [Key],
        next: usize,
        values: Vec<v8::Local<'s, v8::Value>>,
    }
    let mut open: Vec<Open<'_, 's>> = Vec::new();
    let mut current = key;
    loop {
        let mut value: Option<v8::Local<'s, v8::Value>> = match current {
            Key::Array(items) => {
                open.push(Open {
                    items,
                    next: 0,
                    values: Vec::with_capacity(items.len()),
                });
                None
            }
            Key::Number(n) => Some(v8::Number::new(scope, *n).into()),
            Key::Date(ms) => Some(v8::Date::new(scope, *ms)?.into()),
            Key::String(s) => {
                Some(v8::String::new_from_two_byte(scope, s, v8::NewStringType::Normal)?.into())
            }
            Key::Binary(b) => {
                let store = v8::ArrayBuffer::new_backing_store_from_vec(b.clone()).make_shared();
                Some(v8::ArrayBuffer::with_backing_store(scope, &store).into())
            }
        };
        current = loop {
            let Some(top) = open.last_mut() else {
                return value;
            };
            if let Some(v) = value.take() {
                top.values.push(v);
            }
            // (the key's own slice, not a borrow of `open`)
            let items = top.items;
            if let Some(k) = items.get(top.next) {
                top.next += 1;
                break k;
            }
            let done = open.pop()?;
            value = Some(v8::Array::new_with_elements(scope, &done.values).into());
        };
    }
}

impl Engine {
    /// Converts a value to a key; "invalid value" and "invalid type" both throw a "DataError".
    pub(crate) fn js_to_key(&self, cx: &mut Cx, v: &JsValue) -> rt::Result<Key> {
        self.js_to_key_conv(cx, v)?
            .key()
            .ok_or_else(|| data_error("The parameter is not a valid key"))
    }

    /// Converts a value to a key.
    pub(crate) fn js_to_key_conv(&self, cx: &mut Cx, v: &JsValue) -> rt::Result<Conv> {
        let scope = cx.scope();
        let v = v8::Local::new(scope, v.global());
        value_to_key(scope, v, &mut Vec::new()).map_err(|()| Error::Pending)
    }

    /// A key as a JS value of the current realm.
    pub(crate) fn key_to_js(&self, cx: &mut Cx, key: &Key) -> JsValue {
        let scope = cx.scope();
        match key_to_value(scope, key) {
            Some(v) => JsValue::new(scope, v),
            None => rt::undefined_value(scope),
        }
    }

    /// §2.9 "convert a value to a key range" (`null_disallowed`: null and undefined throw).
    pub(crate) fn js_to_range(
        &self,
        cx: &mut Cx,
        v: Option<&JsValue>,
        null_disallowed: bool,
    ) -> rt::Result<KeyRange> {
        let Some(v) = v else {
            return if null_disallowed {
                Err(data_error("No key or key range specified"))
            } else {
                Ok(KeyRange::unbounded())
            };
        };
        if let Some(r) = self.key_range_of(cx, v) {
            return Ok(r);
        }
        let is_null = {
            let scope = cx.scope();
            v8::Local::new(scope, v.global()).is_null_or_undefined()
        };
        if is_null {
            return if null_disallowed {
                Err(data_error("No key or key range specified"))
            } else {
                Ok(KeyRange::unbounded())
            };
        }
        Ok(KeyRange::only(self.js_to_key(cx, v)?))
    }

    /// The key range of an `IDBKeyRange` value.
    pub(crate) fn key_range_of(&self, cx: &mut Cx, v: &JsValue) -> Option<KeyRange> {
        use snowlark_bindings::generated::ids;
        let h = {
            let scope = cx.scope();
            let v = v8::Local::new(scope, v.global());
            rt::unwrap_value(v, ids::IDB_KEY_RANGE, ids::IDB_KEY_RANGE_END)?
        };
        match &*self.obj(h) {
            Object::Idb(o) => match &**o {
                IdbObject::KeyRange { range, .. } => Some(range.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// §2.9 "is a potentially valid key range".
    pub(crate) fn potentially_valid_range(&self, cx: &mut Cx, v: &JsValue) -> rt::Result<bool> {
        if self.key_range_of(cx, v).is_some() {
            return Ok(true);
        }
        Ok(!matches!(self.js_to_key_conv(cx, v)?, Conv::InvalidType))
    }

    // ----- key paths (§7.1, §7.2) ------------------------------------------------------------------

    /// §7.1 "evaluate a key path on a value": `Ok(None)` is failure.
    pub(crate) fn evaluate_key_path<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
        path: &KeyPath,
    ) -> Result<Option<v8::Local<'s, v8::Value>>, ()> {
        match path {
            KeyPath::Sequence(items) => {
                let result = v8::Array::new(scope, 0);
                for (i, item) in items.iter().enumerate() {
                    let Some(v) = self.evaluate_key_path_string(scope, value, item)? else {
                        return Ok(None);
                    };
                    result.set_index(scope, i as u32, v).ok_or(())?;
                }
                Ok(Some(result.into()))
            }
            KeyPath::String(s) => self.evaluate_key_path_string(scope, value, s),
        }
    }

    fn evaluate_key_path_string<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        mut value: v8::Local<'s, v8::Value>,
        path: &str,
    ) -> Result<Option<v8::Local<'s, v8::Value>>, ()> {
        use snowlark_bindings::generated::ids;
        if path.is_empty() {
            return Ok(Some(value));
        }
        for id in path.split('.') {
            if id == "length" && value.is_string() {
                let s: v8::Local<v8::String> = value.try_into().map_err(|_| ())?;
                value = v8::Number::new(scope, s.length() as f64).into();
                continue;
            }
            if id == "length" && value.is_array() {
                let o: v8::Local<v8::Object> = value.try_into().map_err(|_| ())?;
                let k = rt::name(scope, "length");
                let l = o.get(scope, k.into()).ok_or(())?;
                let n = l.integer_value(scope).ok_or(())?.max(0);
                value = v8::Number::new(scope, n as f64).into();
                continue;
            }
            if let Ok(o) = v8::Local::<v8::Object>::try_from(value) {
                if let Some(blob) = rt::unwrap(&o, ids::BLOB, ids::BLOB_END) {
                    let file = self.blob_file(blob);
                    let special: Option<v8::Local<v8::Value>> = match (id, &file) {
                        ("size", _) => Some(
                            v8::Number::new(scope, self.blob_contents(blob).0.len() as f64).into(),
                        ),
                        ("type", _) => Some(
                            rt::str(scope, &self.blob_contents(blob).1)
                                .ok_or(())?
                                .into(),
                        ),
                        ("name", Some((name, _))) => Some(rt::str(scope, name).ok_or(())?.into()),
                        ("lastModified", Some((_, m))) => {
                            Some(v8::Number::new(scope, *m as f64).into())
                        }
                        _ => None,
                    };
                    if let Some(v) = special {
                        value = v;
                        continue;
                    }
                }
            }
            let Ok(o) = v8::Local::<v8::Object>::try_from(value) else {
                return Ok(None);
            };
            let k = rt::str(scope, id).ok_or(())?;
            if !o.has_own_property(scope, k.into()).ok_or(())? {
                return Ok(None);
            }
            value = o.get(scope, k.into()).ok_or(())?;
            if value.is_undefined() {
                return Ok(None);
            }
        }
        Ok(Some(value))
    }

    /// §7.1 "extract a key from a value using a key path": `Ok(None)` is failure, `Ok(Some(Conv))`
    /// a key or invalid; `Err(())` an exception (pending).
    pub(crate) fn extract_key<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
        path: &KeyPath,
        multi_entry: bool,
    ) -> Result<Option<Conv>, ()> {
        let Some(r) = self.evaluate_key_path(scope, value, path)? else {
            return Ok(None);
        };
        let c = if multi_entry {
            value_to_multi_entry_key(scope, r)?
        } else {
            value_to_key(scope, r, &mut Vec::new())?
        };
        Ok(Some(c))
    }

    /// §7.2 "check that a key could be injected into a value".
    pub(crate) fn could_inject<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        mut value: v8::Local<'s, v8::Value>,
        path: &str,
    ) -> bool {
        let ids: Vec<&str> = path.split('.').collect();
        for id in &ids[..ids.len().saturating_sub(1)] {
            let Ok(o) = v8::Local::<v8::Object>::try_from(value) else {
                return false;
            };
            let Some(k) = rt::str(scope, id) else {
                return false;
            };
            if !o.has_own_property(scope, k.into()).unwrap_or(false) {
                return true;
            }
            match o.get(scope, k.into()) {
                Some(v) => value = v,
                None => return false,
            }
        }
        value.is_object()
    }

    /// §7.2 "inject a key into a value using a key path".
    pub(crate) fn inject_key<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        mut value: v8::Local<'s, v8::Value>,
        key: &Key,
        path: &str,
    ) -> Option<()> {
        let ids: Vec<&str> = path.split('.').collect();
        let (last, rest) = ids.split_last()?;
        for id in rest {
            let o: v8::Local<v8::Object> = value.try_into().ok()?;
            let k = rt::str(scope, id)?;
            if !o.has_own_property(scope, k.into())? {
                let new = v8::Object::new(scope);
                o.create_data_property(scope, k.into(), new.into())?;
            }
            value = o.get(scope, k.into())?;
        }
        let o: v8::Local<v8::Object> = value.try_into().ok()?;
        let k = rt::str(scope, last)?;
        let kv = key_to_value(scope, key)?;
        o.create_data_property(scope, k.into(), kv)?;
        Some(())
    }

    /// A key path as its JS value: a string, or an array of strings.
    pub(crate) fn key_path_to_js(&self, cx: &mut Cx, path: Option<&KeyPath>) -> JsValue {
        let scope = cx.scope();
        let v: v8::Local<v8::Value> = match path {
            None => v8::null(scope).into(),
            Some(KeyPath::String(s)) => match rt::str(scope, s) {
                Some(s) => s.into(),
                None => v8::null(scope).into(),
            },
            Some(KeyPath::Sequence(items)) => {
                let vals: Vec<v8::Local<v8::Value>> = items
                    .iter()
                    .filter_map(|s| rt::str(scope, s).map(Into::into))
                    .collect();
                v8::Array::new_with_elements(scope, &vals).into()
            }
        };
        JsValue::new(scope, v)
    }

    /// A new `DOMStringList` of `names` sorted by code units (§2 "create a sorted name list").
    pub(crate) fn sorted_name_list(&self, mut names: Vec<String>) -> Handle {
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        self.alloc(Object::StringList(names, false))
    }

    fn key_range_data(&self, h: Handle) -> KeyRange {
        self.idb(h, |o| match o {
            IdbObject::KeyRange { range, .. } => range.clone(),
            _ => KeyRange::unbounded(),
        })
    }

    fn new_key_range(&self, cx: &mut Cx, range: KeyRange) -> Handle {
        let global = self.current_global(cx).unwrap_or(Handle::new(1));
        self.idb_alloc(IdbObject::KeyRange { global, range })
    }

    /// A new `IDBRecord`.
    pub(crate) fn new_record(
        &self,
        global: Handle,
        key: Key,
        primary_key: Key,
        value: JsValue,
    ) -> Handle {
        self.idb_alloc(IdbObject::Record(RecordData {
            global,
            key,
            primary_key,
            value,
            key_js: None,
            primary_key_js: None,
        }))
    }
}

impl DOMStringListImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::StringList(l, _) => l.len() as u32,
            _ => 0,
        }
    }
    fn item(&self, this: Handle, index: u32) -> Option<DomStr<'_>> {
        match &*self.obj(this) {
            Object::StringList(l, _) => l.get(index as usize).map(|s| DomStr::Owned(s.clone())),
            _ => None,
        }
    }
    fn contains(&self, _cx: &mut Cx, this: Handle, string: &str) -> rt::Result<bool> {
        Ok(match &*self.obj(this) {
            Object::StringList(l, _) => l.iter().any(|s| s == string),
            _ => false,
        })
    }
}

impl IDBKeyRangeImpl for Engine {
    fn lower(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        Ok(match self.key_range_data(this).lower {
            Some(k) => self.key_to_js(cx, &k),
            None => rt::undefined_value(cx.scope()),
        })
    }
    fn upper(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        Ok(match self.key_range_data(this).upper {
            Some(k) => self.key_to_js(cx, &k),
            None => rt::undefined_value(cx.scope()),
        })
    }
    fn lower_open(&self, this: Handle) -> bool {
        self.key_range_data(this).lower_open
    }
    fn upper_open(&self, this: Handle) -> bool {
        self.key_range_data(this).upper_open
    }
    /// §4.7 `only(value)`.
    fn static_only(&self, cx: &mut Cx, value: JsValue) -> rt::Result<Handle> {
        let key = self.js_to_key(cx, &value)?;
        Ok(self.new_key_range(cx, KeyRange::only(key)))
    }
    /// §4.7 `lowerBound(lower, open)`.
    fn static_lower_bound(&self, cx: &mut Cx, lower: JsValue, open: bool) -> rt::Result<Handle> {
        let key = self.js_to_key(cx, &lower)?;
        Ok(self.new_key_range(
            cx,
            KeyRange {
                lower: Some(key),
                upper: None,
                lower_open: open,
                upper_open: true,
            },
        ))
    }
    /// §4.7 `upperBound(upper, open)`.
    fn static_upper_bound(&self, cx: &mut Cx, upper: JsValue, open: bool) -> rt::Result<Handle> {
        let key = self.js_to_key(cx, &upper)?;
        Ok(self.new_key_range(
            cx,
            KeyRange {
                lower: None,
                upper: Some(key),
                lower_open: true,
                upper_open: open,
            },
        ))
    }
    /// §4.7 `bound(lower, upper, lowerOpen, upperOpen)`.
    fn static_bound(
        &self,
        cx: &mut Cx,
        lower: JsValue,
        upper: JsValue,
        lower_open: bool,
        upper_open: bool,
    ) -> rt::Result<Handle> {
        let lo = self.js_to_key(cx, &lower)?;
        let hi = self.js_to_key(cx, &upper)?;
        if lo > hi {
            return Err(data_error("The lower key is greater than the upper key"));
        }
        if lo == hi && (lower_open || upper_open) {
            return Err(data_error("The bounds are equal and one of them is open"));
        }
        Ok(self.new_key_range(
            cx,
            KeyRange {
                lower: Some(lo),
                upper: Some(hi),
                lower_open,
                upper_open,
            },
        ))
    }
    /// §4.7 `includes(key)`.
    fn includes(&self, cx: &mut Cx, this: Handle, key: JsValue) -> rt::Result<bool> {
        let k = self.js_to_key(cx, &key)?;
        Ok(self.key_range_data(this).contains(&k))
    }
}

impl Engine {
    fn record_d<R>(&self, h: Handle, f: impl FnOnce(&mut RecordData) -> R) -> R {
        self.idb_mut(h, |o| match o {
            IdbObject::Record(r) => f(r),
            _ => panic!("not an IDBRecord"),
        })
    }
}

impl IDBRecordImpl for Engine {
    fn key(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        if let Some(v) = self.record_d(this, |r| r.key_js.clone()) {
            return Ok(v);
        }
        let k = self.record_d(this, |r| r.key.clone());
        let v = self.key_to_js(cx, &k);
        self.record_d(this, |r| r.key_js = Some(v.clone()));
        Ok(v)
    }
    fn primary_key(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        if let Some(v) = self.record_d(this, |r| r.primary_key_js.clone()) {
            return Ok(v);
        }
        let k = self.record_d(this, |r| r.primary_key.clone());
        let v = self.key_to_js(cx, &k);
        self.record_d(this, |r| r.primary_key_js = Some(v.clone()));
        Ok(v)
    }
    fn value(&self, _cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        Ok(self.record_d(this, |r| r.value.clone()))
    }
}
