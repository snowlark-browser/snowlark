// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IndexedDB §4.9 `IDBCursor` and `IDBCursorWithValue`, opened by `openCursor()` and
//! `openKeyCursor()`, and §6.7 "iterate a cursor".

use snowlark_bindings::generated::{
    IDBCursorDirection, IDBCursorImpl, IDBCursorWithValueImpl, IDBObjectStoreOrIDBIndex,
};
use snowlark_bindings::rt::{self, Cx, Error, ExceptionName, Handle, JsValue};
use snowlark_idb::{Change, Direction, Found, Key, KeyRange, Mode, Request, Seek, Source};

use super::keys::{data_error, Conv};
use super::store::direction_of;
use super::transaction::inactive;
use super::{found, CursorData, IdbObject, OpError, Step, TxnState};
use crate::engine::{exception, Engine};

fn not_got_value() -> Error {
    exception(
        ExceptionName::InvalidStateError,
        "The cursor is being iterated or has iterated past its end",
    )
}

impl Engine {
    /// §4.5 / §4.6 `openCursor()` and `openKeyCursor()`.
    pub(crate) fn open_cursor_request(
        &self,
        cx: &mut Cx,
        this: Handle,
        query: Option<JsValue>,
        direction: IDBCursorDirection,
        key_only: bool,
    ) -> rt::Result<Handle> {
        let src = self.src(this);
        let t = self.src_txn(this);
        if self.handle_deleted(this) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The object store or index has been deleted",
            ));
        }
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        let range = self.js_to_range(cx, query.as_ref(), false)?;
        let (store_id, index_id) = match src.source {
            Source::Store(s) => (s, None),
            Source::Index(s, i) => (s, Some(i)),
        };
        let global = self.txn(t, |t| t.global);
        let cursor = self.idb_alloc(IdbObject::Cursor(CursorData {
            global,
            request: this,
            source: this,
            txn: t,
            store_id,
            index_id,
            direction: direction_of(direction),
            range,
            position: None,
            object_store_position: None,
            key: None,
            key_js: None,
            primary_key_js: None,
            value: None,
            got_value: false,
            key_only,
        }));
        let op = Box::new(move |e: &Engine, _: &mut Cx| e.iterate_cursor(cursor, None, None, 1));
        let request = self.place_request(this, t, op, None);
        self.cursor_d(cursor, |c| c.request = request);
        Ok(request)
    }

    /// §6.7 "iterate a cursor": the backend steps the cursor from its position; the cursor takes
    /// what it found when the request's event is fired.
    fn iterate_cursor(
        &self,
        cursor: Handle,
        key: Option<Key>,
        primary_key: Option<Key>,
        count: u32,
    ) -> Step {
        let (t, store_id, index_id, direction, range, position, os_position, key_only) = self
            .cursor_d(cursor, |c| {
                (
                    c.txn,
                    c.store_id,
                    c.index_id,
                    c.direction,
                    c.range.clone(),
                    c.position.clone(),
                    c.object_store_position.clone(),
                    c.key_only,
                )
            });
        let source = match index_id {
            Some(i) => Source::Index(store_id, i),
            None => Source::Store(store_id),
        };
        let seek = Seek {
            position,
            object_store_position: os_position,
            key,
            primary_key,
        };
        let _ = t;
        let request = Request::CursorAdvance {
            source,
            direction,
            range,
            seek,
            count,
        };
        Step::finish(request, move |e, cx, a| {
            e.cursor_found(cx, cursor, index_id.is_some(), key_only, found(a)?)
        })
    }

    /// The end of §6.7 "iterate a cursor": the cursor takes the record found, or none.
    fn cursor_found(
        &self,
        cx: &mut Cx,
        cursor: Handle,
        index: bool,
        key_only: bool,
        found: Option<Found>,
    ) -> Result<JsValue, OpError> {
        let index_id = index.then_some(());
        let Some((k, pk, v)) = found else {
            self.cursor_d(cursor, |c| {
                c.key = None;
                c.key_js = None;
                c.primary_key_js = None;
                if index_id.is_some() {
                    c.object_store_position = None;
                }
                if !c.key_only {
                    c.value = None;
                }
            });
            let scope = cx.scope();
            return Ok(JsValue::new(scope, v8::null(scope).into()));
        };
        let value = if key_only {
            None
        } else {
            let (t, store) = self.cursor_d(cursor, |c| (c.txn, c.store_id));
            let inject = self.injected_at(t, store);
            Some(self.record_value(cx, &v, &pk, inject.as_ref())?)
        };
        self.cursor_d(cursor, |c| {
            c.position = Some(k.clone());
            if index_id.is_some() {
                c.object_store_position = Some(pk);
            }
            c.key = Some(k);
            c.key_js = None;
            c.primary_key_js = None;
            if !c.key_only {
                c.value = value;
            }
            c.got_value = true;
        });
        Ok(self.handle_value(cx, cursor))
    }

    /// The cursor's effective key: its object store position for an index, its position for a
    /// store.
    fn effective_key(&self, cursor: Handle) -> Option<Key> {
        self.cursor_d(cursor, |c| {
            if c.index_id.is_some() {
                c.object_store_position.clone()
            } else {
                c.position.clone()
            }
        })
    }

    fn cursor_source(&self, cursor: Handle) -> (Handle, Source) {
        self.cursor_d(cursor, |c| {
            (
                c.txn,
                match c.index_id {
                    Some(i) => Source::Index(c.store_id, i),
                    None => Source::Store(c.store_id),
                },
            )
        })
    }

    /// Whether the cursor's source or effective object store has been deleted.
    fn cursor_deleted(&self, cursor: Handle) -> bool {
        let (t, source) = self.cursor_source(cursor);
        let store_gone = match source {
            Source::Index(s, _) => self.source_deleted(t, Source::Store(s)),
            Source::Store(_) => false,
        };
        let handle = self.cursor_d(cursor, |c| c.source);
        store_gone || self.handle_deleted(handle)
    }

    /// The steps `advance()`, `continue()` and `continuePrimaryKey()` end with.
    fn iterate_again(
        &self,
        cursor: Handle,
        key: Option<Key>,
        primary_key: Option<Key>,
        count: u32,
    ) {
        let (t, request, source) = self.cursor_d(cursor, |c| {
            c.got_value = false;
            (c.txn, c.request, c.source)
        });
        self.req(request, |r| r.done = false);
        let op = Box::new(move |e: &Engine, _: &mut Cx| {
            e.iterate_cursor(cursor, key, primary_key, count)
        });
        self.place_request(source, t, op, Some(request));
    }
}

impl IDBCursorImpl for Engine {
    fn source(&self, this: Handle) -> IDBObjectStoreOrIDBIndex {
        let (source, is_index) = self.cursor_d(this, |c| (c.source, c.index_id.is_some()));
        if is_index {
            IDBObjectStoreOrIDBIndex::IDBIndex(source)
        } else {
            IDBObjectStoreOrIDBIndex::IDBObjectStore(source)
        }
    }

    fn direction(&self, this: Handle) -> IDBCursorDirection {
        match self.cursor_d(this, |c| c.direction) {
            Direction::Next => IDBCursorDirection::Next,
            Direction::NextUnique => IDBCursorDirection::Nextunique,
            Direction::Prev => IDBCursorDirection::Prev,
            Direction::PrevUnique => IDBCursorDirection::Prevunique,
        }
    }

    fn key(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        if let Some(v) = self.cursor_d(this, |c| c.key_js.clone()) {
            return Ok(v);
        }
        let Some(k) = self.cursor_d(this, |c| c.key.clone()) else {
            return Ok(rt::undefined_value(cx.scope()));
        };
        let v = self.key_to_js(cx, &k);
        self.cursor_d(this, |c| c.key_js = Some(v.clone()));
        Ok(v)
    }

    fn primary_key(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        if let Some(v) = self.cursor_d(this, |c| c.primary_key_js.clone()) {
            return Ok(v);
        }
        let has_key = self.cursor_d(this, |c| c.key.is_some());
        let Some(k) = self.effective_key(this).filter(|_| has_key) else {
            return Ok(rt::undefined_value(cx.scope()));
        };
        let v = self.key_to_js(cx, &k);
        self.cursor_d(this, |c| c.primary_key_js = Some(v.clone()));
        Ok(v)
    }

    fn request(&self, this: Handle) -> Handle {
        self.cursor_d(this, |c| c.request)
    }

    /// §4.9 `advance(count)`.
    fn advance(&self, _cx: &mut Cx, this: Handle, count: u32) -> rt::Result<()> {
        if count == 0 {
            return Err(Error::Type("The count must not be 0".into()));
        }
        let t = self.cursor_d(this, |c| c.txn);
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        if self.cursor_deleted(this) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The cursor's source has been deleted",
            ));
        }
        if !self.cursor_d(this, |c| c.got_value) {
            return Err(not_got_value());
        }
        self.iterate_again(this, None, None, count);
        Ok(())
    }

    /// §4.9 `continue(key)`.
    fn continue_(&self, cx: &mut Cx, this: Handle, key: Option<JsValue>) -> rt::Result<()> {
        let t = self.cursor_d(this, |c| c.txn);
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        if self.cursor_deleted(this) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The cursor's source has been deleted",
            ));
        }
        if !self.cursor_d(this, |c| c.got_value) {
            return Err(not_got_value());
        }
        let key = key.filter(|k| !k.is_undefined(cx));
        let key = match key {
            Some(k) => {
                let k = self.js_to_key(cx, &k)?;
                let (direction, position) =
                    self.cursor_d(this, |c| (c.direction, c.position.clone()));
                if let Some(p) = &position {
                    if direction.forward() && &k <= p {
                        return Err(data_error("The key is not after the cursor's position"));
                    }
                    if !direction.forward() && &k >= p {
                        return Err(data_error("The key is not before the cursor's position"));
                    }
                }
                Some(k)
            }
            None => None,
        };
        self.iterate_again(this, key, None, 1);
        Ok(())
    }

    /// §4.9 `continuePrimaryKey(key, primaryKey)`.
    fn continue_primary_key(
        &self,
        cx: &mut Cx,
        this: Handle,
        key: JsValue,
        primary_key: JsValue,
    ) -> rt::Result<()> {
        let t = self.cursor_d(this, |c| c.txn);
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        if self.cursor_deleted(this) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The cursor's source has been deleted",
            ));
        }
        let (is_index, direction) = self.cursor_d(this, |c| (c.index_id.is_some(), c.direction));
        if !is_index {
            return Err(exception(
                ExceptionName::InvalidAccessError,
                "The cursor's source is not an index",
            ));
        }
        if !matches!(direction, Direction::Next | Direction::Prev) {
            return Err(exception(
                ExceptionName::InvalidAccessError,
                "The cursor's direction is not next or prev",
            ));
        }
        if !self.cursor_d(this, |c| c.got_value) {
            return Err(not_got_value());
        }
        let k = self.js_to_key(cx, &key)?;
        let pk = self.js_to_key(cx, &primary_key)?;
        let (position, os_position) = self.cursor_d(this, |c| {
            (c.position.clone(), c.object_store_position.clone())
        });
        if let (Some(p), Some(op)) = (&position, &os_position) {
            let forward = direction == Direction::Next;
            if forward && &k < p || !forward && &k > p {
                return Err(data_error(
                    "The key is on the wrong side of the cursor's position",
                ));
            }
            if &k == p && (forward && &pk <= op || !forward && &pk >= op) {
                return Err(data_error(
                    "The primary key is on the wrong side of the cursor's object store position",
                ));
            }
        }
        self.iterate_again(this, Some(k), Some(pk), 1);
        Ok(())
    }

    /// §4.9 `update(value)`.
    fn update(&self, cx: &mut Cx, this: Handle, value: JsValue) -> rt::Result<Handle> {
        let (t, store_id, key_only, got_value) =
            self.cursor_d(this, |c| (c.txn, c.store_id, c.key_only, c.got_value));
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        if self.txn(t, |t| t.mode) == Mode::ReadOnly {
            return Err(exception(
                ExceptionName::ReadOnlyError,
                "The transaction is read-only",
            ));
        }
        if self.cursor_deleted(this) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The cursor's source has been deleted",
            ));
        }
        if !got_value {
            return Err(not_got_value());
        }
        if key_only {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The cursor is a key cursor",
            ));
        }
        let effective = self
            .effective_key(this)
            .expect("a cursor with a value has a key");
        let (bytes, clone) = self.clone_during(cx, t, &value)?;
        let key_path = self.txn_schema(t, |d| d.store(store_id).and_then(|s| s.key_path.clone()));
        if let Some(path) = key_path {
            let kpk = {
                let scope = cx.scope();
                let v = v8::Local::new(scope, clone.global());
                self.extract_key(scope, v, &path, false)
                    .map_err(|()| Error::Pending)?
            };
            match kpk {
                Some(Conv::Key(k)) if k == effective => {}
                _ => {
                    return Err(data_error(
                        "The effective object store uses in-line keys and the key would change",
                    ))
                }
            }
        }
        let op = Box::new(move |e: &Engine, cx: &mut Cx| {
            e.store_record(cx, t, store_id, bytes, Some(clone), Some(effective), false)
        });
        Ok(self.place_request(this, t, op, None))
    }

    /// §4.9 `delete()`.
    fn delete(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let (t, store_id, key_only, got_value) =
            self.cursor_d(this, |c| (c.txn, c.store_id, c.key_only, c.got_value));
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        if self.txn(t, |t| t.mode) == Mode::ReadOnly {
            return Err(exception(
                ExceptionName::ReadOnlyError,
                "The transaction is read-only",
            ));
        }
        if self.cursor_deleted(this) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The cursor's source has been deleted",
            ));
        }
        if !got_value {
            return Err(not_got_value());
        }
        if key_only {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The cursor is a key cursor",
            ));
        }
        let effective = self
            .effective_key(this)
            .expect("a cursor with a value has a key");
        let op = Box::new(move |e: &Engine, cx: &mut Cx| {
            e.txn_apply(t, Change::DeleteRange(store_id, KeyRange::only(effective)));
            Step::Done(Ok(rt::undefined_value(cx.scope())))
        });
        Ok(self.place_request(this, t, op, None))
    }
}

impl IDBCursorWithValueImpl for Engine {
    fn value(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        Ok(self
            .cursor_d(this, |c| c.value.clone())
            .unwrap_or_else(|| rt::undefined_value(cx.scope())))
    }
}
