// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IndexedDB §4.5 `IDBObjectStore` and §4.6 `IDBIndex`, and the database operations of §6 they
//! place: storing a record (§6.1), retrieving (§6.2, §6.3), deleting (§6.4), counting (§6.5),
//! clearing (§6.6), an index's population.

use std::sync::Arc;

use snowlark_bindings::generated::{
    IDBCursorDirection, IDBGetAllOptions, IDBIndexImpl, IDBIndexParameters, IDBObjectStoreImpl,
    StringOrStringSequence,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle, JsValue};
use snowlark_idb::{
    Answer, Change, Direction, Fill, Key, KeyPath, KeyRange, Mode, Record, Request, Source,
};

use super::factory::key_path_of;
use super::keys::{data_error, Conv};
use super::transaction::inactive;
use super::{
    counted, found, found_all, generated, IdbObject, IndexHandle, OpError, Step, TxnState,
};
use crate::engine::{exception, Engine};

/// What `getAll()`, `getAllKeys()` and `getAllRecords()` return (§5.12).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Key,
    Value,
    Record,
}

/// The object store of a store or index.
fn store_of(source: Source) -> u64 {
    match source {
        Source::Store(s) | Source::Index(s, _) => s,
    }
}

pub(crate) fn direction_of(d: IDBCursorDirection) -> Direction {
    match d {
        IDBCursorDirection::Next => Direction::Next,
        IDBCursorDirection::Nextunique => Direction::NextUnique,
        IDBCursorDirection::Prev => Direction::Prev,
        IDBCursorDirection::Prevunique => Direction::PrevUnique,
    }
}

fn deleted() -> Error {
    exception(
        ExceptionName::InvalidStateError,
        "The object store or index has been deleted",
    )
}

fn read_only() -> Error {
    exception(ExceptionName::ReadOnlyError, "The transaction is read-only")
}

/// What a source handle (an object store handle or an index handle) reads: its object store or
/// index, and the handle its transaction is reached through (the store handle of an index).
#[derive(Clone, Copy)]
pub(crate) struct Src {
    pub txn: Handle,
    pub source: Source,
}

impl Engine {
    /// The source of an `IDBObjectStore` or `IDBIndex` handle.
    pub(crate) fn src(&self, h: Handle) -> Src {
        self.idb(h, |o| match o {
            IdbObject::Store(s) => Src {
                txn: s.txn,
                source: Source::Store(s.id),
            },
            IdbObject::Index(i) => Src {
                txn: i.store,
                source: Source::Index(i.store_id, i.id),
            },
            _ => panic!("not a source"),
        })
    }

    /// The transaction of a source handle.
    pub(crate) fn src_txn(&self, h: Handle) -> Handle {
        let s = self.src(h);
        match s.source {
            Source::Store(_) => s.txn,
            Source::Index(..) => self.store_h(s.txn, |st| st.txn),
        }
    }

    /// Whether a source (or, for an index, its object store) has been deleted.
    pub(crate) fn source_deleted(&self, t: Handle, source: Source) -> bool {
        self.txn_schema(t, |db| match source {
            Source::Store(s) => db.store(s).is_none(),
            Source::Index(s, i) => db.index(s, i).is_none(),
        })
    }

    /// Whether the object store or index of a handle has been deleted. An index is deleted when
    /// `deleteIndex()` removed it from its store handle's index set (its destruction in the
    /// database is a request that may not have run yet).
    pub(crate) fn handle_deleted(&self, h: Handle) -> bool {
        let src = self.src(h);
        let t = self.src_txn(h);
        if self.source_deleted(t, src.source) {
            return true;
        }
        match src.source {
            Source::Index(_, i) => self.store_h(src.txn, |s| {
                s.deleted || !s.indexes.iter().any(|(x, _)| *x == i)
            }),
            Source::Store(_) => self.store_h(h, |s| s.deleted),
        }
    }

    /// The checks the request-making methods share: deleted, then inactive.
    fn check_source(&self, h: Handle) -> rt::Result<(Handle, Source)> {
        let src = self.src(h);
        let t = self.src_txn(h);
        if self.handle_deleted(h) {
            return Err(deleted());
        }
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        Ok((t, src.source))
    }

    /// §5.11 "clone a value in targetRealm during transaction": the serialization (with the
    /// transaction inactive) and a deserialized copy.
    pub(crate) fn clone_during(
        &self,
        cx: &mut Cx,
        t: Handle,
        value: &JsValue,
    ) -> rt::Result<(Vec<u8>, JsValue)> {
        self.txn(t, |t| t.state = TxnState::Inactive);
        // (StructuredSerializeForStorage)
        let bytes = self.serializing_for_storage(|| self.serialize_to_bytes(cx, value));
        self.txn(t, |t| t.state = TxnState::Active);
        let bytes = bytes?;
        let clone = self.deserialize_value(cx, &bytes).map_err(|()| {
            exception(
                ExceptionName::DataCloneError,
                "The value could not be cloned",
            )
        })?;
        Ok((bytes, clone))
    }

    /// A stored value, deserialized in the current realm.
    pub(crate) fn value_of(&self, cx: &mut Cx, bytes: &[u8]) -> Result<JsValue, OpError> {
        self.deserialize_value(cx, bytes).map_err(|()| {
            (
                ExceptionName::NotReadableError,
                "The value could not be read",
            )
        })
    }

    /// The key path a record of `store` read in transaction `t` may lack its key at: the store's,
    /// if it has a key generator and in-line keys ([`Engine::record_value`]).
    pub(crate) fn injected_at(&self, t: Handle, store: u64) -> Option<KeyPath> {
        self.txn_schema(t, |d| {
            let s = d.store(store)?;
            match &s.key_path {
                Some(p @ KeyPath::String(_)) if s.auto_increment => Some(p.clone()),
                _ => None,
            }
        })
    }

    /// A record's value, deserialized in the current realm. A key its store's key generator made is
    /// not in the stored value (the backend generates it after the value is serialized): with
    /// `inject` the store's key path, `key` is injected where the value has nothing at the path.
    /// This is the value §6.1 "store a record into an object store" stores, which injects the key
    /// before serializing: the key was generated exactly when the path yielded nothing, and the
    /// injected property comes last either way.
    pub(crate) fn record_value(
        &self,
        cx: &mut Cx,
        bytes: &[u8],
        key: &Key,
        inject: Option<&KeyPath>,
    ) -> Result<JsValue, OpError> {
        let v = self.value_of(cx, bytes)?;
        if let Some(path @ KeyPath::String(p)) = inject {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let lv = v8::Local::new(tc, v.global());
            if let Ok(None) = self.evaluate_key_path(tc, lv, path) {
                let _ = self.inject_key(tc, lv, key, p);
            }
            tc.reset();
        }
        Ok(v)
    }

    /// §4.5 "add or put".
    fn add_or_put(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: JsValue,
        key: Option<JsValue>,
        no_overwrite: bool,
    ) -> rt::Result<Handle> {
        let (t, store_id, key_path, auto_increment) = self.store_h(this, |s| {
            (s.txn, s.id, s.key_path.clone(), s.auto_increment)
        });
        if self.handle_deleted(this) {
            return Err(deleted());
        }
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        if self.txn(t, |t| t.mode) == Mode::ReadOnly {
            return Err(read_only());
        }
        let key = key.filter(|k| !k.is_undefined(cx));
        if key_path.is_some() && key.is_some() {
            return Err(data_error(
                "The object store uses in-line keys and the key parameter was provided",
            ));
        }
        if key_path.is_none() && !auto_increment && key.is_none() {
            return Err(data_error(
                "The object store uses out-of-line keys and has no key generator and the key parameter was not provided",
            ));
        }
        let mut key = match key {
            Some(k) => Some(self.js_to_key(cx, &k)?),
            None => None,
        };
        let (bytes, clone) = self.clone_during(cx, t, &value)?;
        if let Some(path) = &key_path {
            let kpk = {
                let scope = cx.scope();
                let v = v8::Local::new(scope, clone.global());
                self.extract_key(scope, v, path, false)
                    .map_err(|()| Error::Pending)?
            };
            match kpk {
                Some(Conv::Key(k)) => key = Some(k),
                Some(_) => {
                    return Err(data_error(
                        "Evaluating the object store's key path yielded a value that is not a valid key",
                    ))
                }
                None => {
                    if !auto_increment {
                        return Err(data_error(
                            "Evaluating the object store's key path did not yield a value",
                        ));
                    }
                    let KeyPath::String(p) = path else {
                        return Err(data_error("A key cannot be injected"));
                    };
                    let ok = {
                        let scope = cx.scope();
                        let v = v8::Local::new(scope, clone.global());
                        self.could_inject(scope, v, p)
                    };
                    if !ok {
                        return Err(data_error(
                            "A generated key could not be inserted into the value",
                        ));
                    }
                }
            }
        }
        let op = Box::new(move |e: &Engine, cx: &mut Cx| {
            e.store_record(cx, t, store_id, bytes, Some(clone), key, no_overwrite)
        });
        Ok(self.place_request(this, t, op, None))
    }

    /// §6.1 "store a record into an object store": with no key in a store with a key generator, the
    /// backend generates one first (the next calls wait: an in-line key goes into the value).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_record(
        &self,
        cx: &mut Cx,
        t: Handle,
        store_id: u64,
        bytes: Vec<u8>,
        clone: Option<JsValue>,
        key: Option<Key>,
        no_overwrite: bool,
    ) -> Step {
        // (what the operation reads of the store's schema)
        let schema = self.txn_schema(t, |d| {
            d.store(store_id).map(|s| {
                let indexes: Vec<(u64, KeyPath, bool, bool)> = s
                    .indexes
                    .values()
                    .filter(|i| i.ready)
                    .map(|i| (i.id, i.key_path.clone(), i.unique, i.multi_entry))
                    .collect();
                (s.key_path.clone(), s.auto_increment, indexes)
            })
        });
        let Some((key_path, auto, indexes)) = schema else {
            return Step::Done(Err((
                ExceptionName::InvalidStateError,
                "The object store has been deleted",
            )));
        };
        match key {
            Some(k) => self.store_keyed(cx, store_id, bytes, clone, k, no_overwrite, &indexes),
            None if auto => {
                let inline = match key_path {
                    Some(KeyPath::String(p)) => Some(p),
                    _ => None,
                };
                self.store_generated(cx, store_id, bytes, clone, inline, no_overwrite, &indexes)
            }
            None => Step::Done(Err((ExceptionName::DataError, "No key"))),
        }
    }

    /// §6.1 with a key the store's key generator makes: the backend generates it, completes the
    /// index keys that depend on it, and stores the record, so nothing here waits for it (the next
    /// requests go at once). An in-line key (`inline`: the store's key path) stays out of the value
    /// and is injected when the record is read ([`Engine::record_value`]); an index key path that
    /// reads it goes to the backend as a [`Fill`].
    #[allow(clippy::too_many_arguments)]
    fn store_generated(
        &self,
        cx: &mut Cx,
        store_id: u64,
        bytes: Vec<u8>,
        mut clone: Option<JsValue>,
        inline: Option<String>,
        no_overwrite: bool,
        indexes: &[(u64, KeyPath, bool, bool)],
    ) -> Step {
        let mut index_keys: Vec<(u64, Vec<Key>)> = Vec::new();
        let mut fill: Vec<(u64, Fill)> = Vec::new();
        if !indexes.is_empty() {
            if clone.is_none() {
                match self.value_of(cx, &bytes) {
                    Ok(c) => clone = Some(c),
                    Err(err) => return Step::Done(Err(err)),
                }
            }
            let c = clone.clone().expect("a clone");
            for (id, path, _, multi) in indexes {
                let scope = cx.scope();
                v8::tc_scope!(let tc, scope);
                let v = v8::Local::new(tc, c.global());
                match self.extract_key(tc, v, path, *multi) {
                    Ok(Some(Conv::Key(k))) => {
                        let keys = match k {
                            Key::Array(sub) if *multi => sub,
                            k => vec![k],
                        };
                        index_keys.push((*id, keys));
                    }
                    Ok(None) => {
                        if let Some(f) = inline.as_deref().and_then(|p| self.fill(tc, v, path, p)) {
                            fill.push((*id, f));
                        }
                    }
                    _ => {}
                }
                tc.reset();
            }
        }
        let request = Request::Generate {
            store: store_id,
            record: Record {
                value: Arc::from(bytes),
                index_keys,
            },
            fill,
            add: no_overwrite,
        };
        Step::finish(request, move |e, cx, a| {
            let k = generated(a)?;
            Ok(e.key_to_js(cx, &Key::Number(k as f64)))
        })
    }

    /// An index key that the key the backend generates for a record completes: the index's key path
    /// `path` is the store's (`store_path`), or a sequence with it whose other items yield keys on
    /// `value` (which has nothing at `store_path`: its key is generated). `None`: the index has no
    /// key for the record.
    fn fill<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
        path: &KeyPath,
        store_path: &str,
    ) -> Option<Fill> {
        match path {
            KeyPath::String(p) => (p == store_path).then_some(Fill::Key),
            KeyPath::Sequence(items) => {
                if !items.iter().any(|i| i == store_path) {
                    return None;
                }
                let mut keys = Vec::with_capacity(items.len());
                for item in items {
                    if item == store_path {
                        keys.push(None);
                        continue;
                    }
                    let one = KeyPath::String(item.clone());
                    match self.extract_key(scope, value, &one, false) {
                        Ok(Some(Conv::Key(k))) => keys.push(Some(k)),
                        _ => return None,
                    }
                }
                Some(Fill::Array(keys))
            }
        }
    }

    /// The rest of §6.1 once the record has its key: its keys in each index (an index whose key
    /// path yields no valid key has none), then the write.
    #[allow(clippy::too_many_arguments)]
    fn store_keyed(
        &self,
        cx: &mut Cx,
        store_id: u64,
        bytes: Vec<u8>,
        mut clone: Option<JsValue>,
        key: Key,
        no_overwrite: bool,
        indexes: &[(u64, KeyPath, bool, bool)],
    ) -> Step {
        let mut index_keys: Vec<(u64, Vec<Key>)> = Vec::new();
        if !indexes.is_empty() {
            if clone.is_none() {
                match self.value_of(cx, &bytes) {
                    Ok(c) => clone = Some(c),
                    Err(err) => return Step::Done(Err(err)),
                }
            }
            let c = clone.clone().expect("a clone");
            for (id, path, _, multi) in indexes {
                let scope = cx.scope();
                v8::tc_scope!(let tc, scope);
                let v = v8::Local::new(tc, c.global());
                let r = self.extract_key(tc, v, path, *multi);
                tc.reset();
                if let Ok(Some(Conv::Key(k))) = r {
                    let keys = match k {
                        Key::Array(sub) if *multi => sub,
                        k => vec![k],
                    };
                    index_keys.push((*id, keys));
                }
            }
        }
        self.txn_put(
            Change::Put {
                store: store_id,
                key: key.clone(),
                record: Record {
                    value: Arc::from(bytes),
                    index_keys,
                },
            },
            no_overwrite,
            key,
        )
    }

    /// An index's population after `createIndex()` (§4.5): every record's keys in it, or a
    /// "ConstraintError" for a unique index the records violate. The next calls wait for it.
    fn populate_index(&self, cx: &mut Cx, t: Handle, store_id: u64, index_id: u64) -> Step {
        let index = self.txn_schema(t, |d| {
            d.index(store_id, index_id)
                .map(|ix| (ix.key_path.clone(), ix.unique, ix.multi_entry))
        });
        let Some((path, unique, multi)) = index else {
            return Step::Done(Ok(rt::undefined_value(cx.scope())));
        };
        let request = Request::GetAll {
            source: Source::Store(store_id),
            range: KeyRange::unbounded(),
            direction: Direction::Next,
            count: 0,
        };
        Step::then(request, move |e, cx, a| {
            Step::Done(e.index_records(cx, t, store_id, index_id, &path, unique, multi, a))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn index_records(
        &self,
        cx: &mut Cx,
        t: Handle,
        store_id: u64,
        index_id: u64,
        path: &KeyPath,
        unique: bool,
        multi: bool,
        answer: Answer,
    ) -> Result<JsValue, OpError> {
        let records: Vec<(Key, snowlark_idb::Value)> = found_all(answer)?
            .into_iter()
            .map(|(k, _, v)| (k, v))
            .collect();
        let mut keys: Vec<(Key, Vec<Key>)> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let inject = self.injected_at(t, store_id);
        for (pk, value) in records {
            let v = self.record_value(cx, &value, &pk, inject.as_ref())?;
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let lv = v8::Local::new(tc, v.global());
            let r = self.extract_key(tc, lv, path, multi);
            tc.reset();
            if let Ok(Some(Conv::Key(k))) = r {
                let ks = match k {
                    Key::Array(sub) if multi => sub,
                    k => vec![k],
                };
                if unique {
                    for k in &ks {
                        if !seen.insert(k.clone()) {
                            return Err((
                                ExceptionName::ConstraintError,
                                "The records violate the unique index",
                            ));
                        }
                    }
                }
                keys.push((pk, ks));
            }
        }
        self.txn_apply(
            t,
            Change::PopulateIndex {
                store: store_id,
                index: index_id,
                keys,
            },
        );
        Ok(rt::undefined_value(cx.scope()))
    }

    /// `get()` on a store or an index.
    fn get_request(
        &self,
        this: Handle,
        query: JsValue,
        cx: &mut Cx,
        key_only: bool,
    ) -> rt::Result<Handle> {
        let (t, source) = self.check_source(this)?;
        let range = self.js_to_range(cx, Some(&query), true)?;
        let op = Box::new(move |_: &Engine, _: &mut Cx| {
            Step::finish(
                Request::First { source, range },
                move |e, cx, a| match found(a)? {
                    None => Ok(rt::undefined_value(cx.scope())),
                    Some((_, pk, v)) => {
                        if key_only {
                            Ok(e.key_to_js(cx, &pk))
                        } else {
                            let inject = e.injected_at(t, store_of(source));
                            e.record_value(cx, &v, &pk, inject.as_ref())
                        }
                    }
                },
            )
        });
        Ok(self.place_request(this, t, op, None))
    }

    /// §5.12 "create a request to retrieve multiple items".
    fn get_all_request(
        &self,
        cx: &mut Cx,
        this: Handle,
        kind: Kind,
        query_or_options: Option<JsValue>,
        count: Option<u32>,
    ) -> rt::Result<Handle> {
        let (t, source) = self.check_source(this)?;
        let q = query_or_options.filter(|q| {
            let scope = cx.scope();
            !v8::Local::new(scope, q.global()).is_null_or_undefined()
        });
        let (range, direction, count) = match q {
            None => (KeyRange::unbounded(), Direction::Next, count),
            Some(q) if self.potentially_valid_range(cx, &q)? => (
                self.js_to_range(cx, Some(&q), false)?,
                Direction::Next,
                count,
            ),
            Some(q) => {
                let o = IDBGetAllOptions::from_value(cx, &q)?;
                (
                    self.js_to_range(cx, o.query.as_ref(), false)?,
                    direction_of(o.direction),
                    o.count,
                )
            }
        };
        Ok(self.place_get_all(t, this, source, kind, range, direction, count.unwrap_or(0)))
    }

    #[allow(clippy::too_many_arguments)]
    fn place_get_all(
        &self,
        t: Handle,
        this: Handle,
        source: Source,
        kind: Kind,
        range: KeyRange,
        direction: Direction,
        count: u32,
    ) -> Handle {
        let request = Request::GetAll {
            source,
            range,
            direction,
            count,
        };
        let finish = move |e: &Engine, cx: &mut Cx, a: Answer| {
            let found = found_all(a)?;
            let global = e.txn(t, |t| t.global);
            let inject = e.injected_at(t, store_of(source));
            let mut items = Vec::with_capacity(found.len());
            for (k, pk, v) in found {
                let item = match kind {
                    Kind::Key => e.key_to_js(cx, &pk),
                    Kind::Value => e.record_value(cx, &v, &pk, inject.as_ref())?,
                    Kind::Record => {
                        let value = e.record_value(cx, &v, &pk, inject.as_ref())?;
                        let r = e.new_record(global, k, pk, value);
                        e.handle_value(cx, r)
                    }
                };
                items.push(item);
            }
            let scope = cx.scope();
            let vals: Vec<v8::Local<v8::Value>> = items
                .iter()
                .map(|v| v8::Local::new(scope, v.global()))
                .collect();
            let arr = v8::Array::new_with_elements(scope, &vals);
            Ok(JsValue::new(scope, arr.into()))
        };
        let op = Box::new(move |_: &Engine, _: &mut Cx| Step::finish(request, finish));
        self.place_request(this, t, op, None)
    }

    fn get_all_records_request(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: IDBGetAllOptions,
    ) -> rt::Result<Handle> {
        let (t, source) = self.check_source(this)?;
        let range = self.js_to_range(cx, options.query.as_ref(), false)?;
        Ok(self.place_get_all(
            t,
            this,
            source,
            Kind::Record,
            range,
            direction_of(options.direction),
            options.count.unwrap_or(0),
        ))
    }

    fn count_request(
        &self,
        cx: &mut Cx,
        this: Handle,
        query: Option<JsValue>,
    ) -> rt::Result<Handle> {
        let (t, source) = self.check_source(this)?;
        let range = self.js_to_range(cx, query.as_ref(), false)?;
        let op = Box::new(move |_: &Engine, _: &mut Cx| {
            Step::finish(Request::Count { source, range }, |_, cx, a| {
                let n = counted(a)?;
                let scope = cx.scope();
                Ok(JsValue::new(scope, v8::Number::new(scope, n as f64).into()))
            })
        });
        Ok(self.place_request(this, t, op, None))
    }

    /// The index handle of a store handle for an index (one per index and store handle).
    fn index_handle(&self, store: Handle, index_id: u64) -> Option<Handle> {
        if let Some(h) = self.store_h(store, |s| {
            s.index_handles
                .iter()
                .find(|(i, _)| *i == index_id)
                .map(|(_, h)| *h)
        }) {
            return Some(h);
        }
        let (t, store_id, global) = self.store_h(store, |s| (s.txn, s.id, s.global));
        let handle = self.txn_schema(t, |d| {
            let ix = d.index(store_id, index_id)?;
            Some(IndexHandle {
                global,
                store,
                store_id,
                id: index_id,
                name: ix.name.clone(),
                key_path: ix.key_path.clone(),
                unique: ix.unique,
                multi_entry: ix.multi_entry,
                key_path_js: None,
                created: false,
            })
        })?;
        let h = self.idb_alloc(IdbObject::Index(handle));
        self.store_h(store, |s| s.index_handles.push((index_id, h)));
        Some(h)
    }
}

impl IDBObjectStoreImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.store_h(this, |s| s.name.clone()))
    }

    /// §4.5 the `name` setter.
    fn set_name(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let (t, id, current) = self.store_h(this, |s| (s.txn, s.id, s.name.clone()));
        if self.handle_deleted(this) {
            return Err(deleted());
        }
        if self.txn(t, |t| t.mode) != Mode::VersionChange {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Not in a version change transaction",
            ));
        }
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        if current == value {
            return Ok(());
        }
        let conn = self.txn(t, |t| t.conn);
        if self.conn(conn, |c| c.stores.iter().any(|(_, n)| n == value)) {
            return Err(exception(
                ExceptionName::ConstraintError,
                "An object store with that name exists",
            ));
        }
        self.txn_apply(t, Change::RenameStore(id, value.to_owned()));
        self.conn(conn, |c| {
            if let Some(s) = c.stores.iter_mut().find(|(s, _)| *s == id) {
                s.1 = value.to_owned();
            }
        });
        self.store_h(this, |s| s.name = value.to_owned());
        Ok(())
    }

    fn key_path(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        if let Some(v) = self.store_h(this, |s| s.key_path_js.clone()) {
            return Ok(v);
        }
        let path = self.store_h(this, |s| s.key_path.clone());
        let v = self.key_path_to_js(cx, path.as_ref());
        self.store_h(this, |s| s.key_path_js = Some(v.clone()));
        Ok(v)
    }

    fn index_names(&self, this: Handle) -> Handle {
        let names = self.store_h(this, |s| s.indexes.iter().map(|(_, n)| n.clone()).collect());
        self.sorted_name_list(names)
    }

    fn transaction(&self, this: Handle) -> Handle {
        self.store_h(this, |s| s.txn)
    }

    fn auto_increment(&self, this: Handle) -> bool {
        self.store_h(this, |s| s.auto_increment)
    }

    fn put(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: JsValue,
        key: Option<JsValue>,
    ) -> rt::Result<Handle> {
        self.add_or_put(cx, this, value, key, false)
    }

    fn add(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: JsValue,
        key: Option<JsValue>,
    ) -> rt::Result<Handle> {
        self.add_or_put(cx, this, value, key, true)
    }

    /// §4.5 `delete(query)`.
    fn delete(&self, cx: &mut Cx, this: Handle, query: JsValue) -> rt::Result<Handle> {
        let (t, source) = self.check_source(this)?;
        if self.txn(t, |t| t.mode) == Mode::ReadOnly {
            return Err(read_only());
        }
        let range = self.js_to_range(cx, Some(&query), true)?;
        let Source::Store(store_id) = source else {
            return Err(deleted());
        };
        let op = Box::new(move |e: &Engine, cx: &mut Cx| {
            e.txn_apply(t, Change::DeleteRange(store_id, range));
            Step::Done(Ok(rt::undefined_value(cx.scope())))
        });
        Ok(self.place_request(this, t, op, None))
    }

    /// §4.5 `clear()`.
    fn clear(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let (t, source) = self.check_source(this)?;
        if self.txn(t, |t| t.mode) == Mode::ReadOnly {
            return Err(read_only());
        }
        let Source::Store(store_id) = source else {
            return Err(deleted());
        };
        let op = Box::new(move |e: &Engine, cx: &mut Cx| {
            e.txn_apply(t, Change::Clear(store_id));
            Step::Done(Ok(rt::undefined_value(cx.scope())))
        });
        Ok(self.place_request(this, t, op, None))
    }

    fn get(&self, cx: &mut Cx, this: Handle, query: JsValue) -> rt::Result<Handle> {
        self.get_request(this, query, cx, false)
    }

    fn get_key(&self, cx: &mut Cx, this: Handle, query: JsValue) -> rt::Result<Handle> {
        self.get_request(this, query, cx, true)
    }

    fn get_all(
        &self,
        cx: &mut Cx,
        this: Handle,
        query_or_options: Option<JsValue>,
        count: Option<u32>,
    ) -> rt::Result<Handle> {
        self.get_all_request(cx, this, Kind::Value, query_or_options, count)
    }

    fn get_all_keys(
        &self,
        cx: &mut Cx,
        this: Handle,
        query_or_options: Option<JsValue>,
        count: Option<u32>,
    ) -> rt::Result<Handle> {
        self.get_all_request(cx, this, Kind::Key, query_or_options, count)
    }

    fn get_all_records(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: IDBGetAllOptions,
    ) -> rt::Result<Handle> {
        self.get_all_records_request(cx, this, options)
    }

    fn count(&self, cx: &mut Cx, this: Handle, query: Option<JsValue>) -> rt::Result<Handle> {
        self.count_request(cx, this, query)
    }

    fn open_cursor(
        &self,
        cx: &mut Cx,
        this: Handle,
        query: Option<JsValue>,
        direction: IDBCursorDirection,
    ) -> rt::Result<Handle> {
        self.open_cursor_request(cx, this, query, direction, false)
    }

    fn open_key_cursor(
        &self,
        cx: &mut Cx,
        this: Handle,
        query: Option<JsValue>,
        direction: IDBCursorDirection,
    ) -> rt::Result<Handle> {
        self.open_cursor_request(cx, this, query, direction, true)
    }

    /// §4.5 `index(name)`.
    fn index(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Handle> {
        let t = self.store_h(this, |s| s.txn);
        if self.handle_deleted(this) {
            return Err(deleted());
        }
        if self.txn_state(t) == TxnState::Finished {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The transaction has finished",
            ));
        }
        let Some(index_id) = self.store_h(this, |s| {
            s.indexes.iter().find(|(_, n)| n == name).map(|(i, _)| *i)
        }) else {
            return Err(exception(
                ExceptionName::NotFoundError,
                "No index has that name",
            ));
        };
        self.index_handle(this, index_id)
            .ok_or_else(|| exception(ExceptionName::NotFoundError, "No index has that name"))
    }

    /// §4.5 `createIndex(name, keyPath, options)`.
    fn create_index(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        key_path: StringOrStringSequence,
        options: IDBIndexParameters,
    ) -> rt::Result<Handle> {
        let (t, store_id, global) = self.store_h(this, |s| (s.txn, s.id, s.global));
        if self.txn(t, |t| t.mode) != Mode::VersionChange {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Not in a version change transaction",
            ));
        }
        if self.handle_deleted(this) {
            return Err(deleted());
        }
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        if self.store_h(this, |s| s.indexes.iter().any(|(_, n)| n == name)) {
            return Err(exception(
                ExceptionName::ConstraintError,
                "An index with that name exists",
            ));
        }
        let key_path = key_path_of(key_path);
        if !key_path.is_valid() {
            return Err(exception(
                ExceptionName::SyntaxError,
                "The key path is not valid",
            ));
        }
        if matches!(key_path, KeyPath::Sequence(_)) && options.multi_entry {
            return Err(exception(
                ExceptionName::InvalidAccessError,
                "A multiEntry index cannot have an array key path",
            ));
        }
        let conn = self.txn(t, |t| t.conn);
        let id = self.conn(conn, |c| c.schema.take_id());
        self.txn_apply(
            t,
            Change::CreateIndex {
                store: store_id,
                id,
                name: name.to_owned(),
                key_path: key_path.clone(),
                unique: options.unique,
                multi_entry: options.multi_entry,
            },
        );
        let h = self.idb_alloc(IdbObject::Index(IndexHandle {
            global,
            store: this,
            store_id,
            id,
            name: name.to_owned(),
            key_path,
            unique: options.unique,
            multi_entry: options.multi_entry,
            key_path_js: None,
            created: true,
        }));
        self.store_h(this, |s| {
            s.indexes.push((id, name.to_owned()));
            s.index_handles.push((id, h));
        });
        self.place_operation(
            t,
            Box::new(move |e: &Engine, cx: &mut Cx| e.populate_index(cx, t, store_id, id)),
        );
        Ok(h)
    }

    /// §4.5 `deleteIndex(name)`.
    fn delete_index(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<()> {
        let (t, store_id) = self.store_h(this, |s| (s.txn, s.id));
        if self.txn(t, |t| t.mode) != Mode::VersionChange {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Not in a version change transaction",
            ));
        }
        if self.handle_deleted(this) {
            return Err(deleted());
        }
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        let Some(index_id) = self.store_h(this, |s| {
            s.indexes.iter().find(|(_, n)| n == name).map(|(i, _)| *i)
        }) else {
            return Err(exception(
                ExceptionName::NotFoundError,
                "No index has that name",
            ));
        };
        self.store_h(this, |s| s.indexes.retain(|(i, _)| *i != index_id));
        // (the index's destruction is processed as an asynchronous request: the requests placed
        // before it still see the index)
        self.place_operation(
            t,
            Box::new(move |e: &Engine, cx: &mut Cx| {
                e.txn_apply(t, Change::DeleteIndex(store_id, index_id));
                Step::Done(Ok(rt::undefined_value(cx.scope())))
            }),
        );
        Ok(())
    }
}

impl IDBIndexImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.index_h(this, |i| i.name.clone()))
    }

    /// §4.6 the `name` setter.
    fn set_name(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let (store, store_id, id, current) =
            self.index_h(this, |i| (i.store, i.store_id, i.id, i.name.clone()));
        let t = self.store_h(store, |s| s.txn);
        if self.txn(t, |t| t.mode) != Mode::VersionChange {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Not in a version change transaction",
            ));
        }
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        if self.handle_deleted(this) {
            return Err(deleted());
        }
        if current == value {
            return Ok(());
        }
        if self.store_h(store, |s| s.indexes.iter().any(|(_, n)| n == value)) {
            return Err(exception(
                ExceptionName::ConstraintError,
                "An index with that name exists",
            ));
        }
        self.txn_apply(t, Change::RenameIndex(store_id, id, value.to_owned()));
        self.store_h(store, |s| {
            if let Some(e) = s.indexes.iter_mut().find(|(i, _)| *i == id) {
                e.1 = value.to_owned();
            }
        });
        self.index_h(this, |i| i.name = value.to_owned());
        Ok(())
    }

    fn object_store(&self, this: Handle) -> Handle {
        self.index_h(this, |i| i.store)
    }

    fn key_path(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        if let Some(v) = self.index_h(this, |i| i.key_path_js.clone()) {
            return Ok(v);
        }
        let path = self.index_h(this, |i| i.key_path.clone());
        let v = self.key_path_to_js(cx, Some(&path));
        self.index_h(this, |i| i.key_path_js = Some(v.clone()));
        Ok(v)
    }

    fn multi_entry(&self, this: Handle) -> bool {
        self.index_h(this, |i| i.multi_entry)
    }

    fn unique(&self, this: Handle) -> bool {
        self.index_h(this, |i| i.unique)
    }

    fn get(&self, cx: &mut Cx, this: Handle, query: JsValue) -> rt::Result<Handle> {
        self.get_request(this, query, cx, false)
    }

    fn get_key(&self, cx: &mut Cx, this: Handle, query: JsValue) -> rt::Result<Handle> {
        self.get_request(this, query, cx, true)
    }

    fn get_all(
        &self,
        cx: &mut Cx,
        this: Handle,
        query_or_options: Option<JsValue>,
        count: Option<u32>,
    ) -> rt::Result<Handle> {
        self.get_all_request(cx, this, Kind::Value, query_or_options, count)
    }

    fn get_all_keys(
        &self,
        cx: &mut Cx,
        this: Handle,
        query_or_options: Option<JsValue>,
        count: Option<u32>,
    ) -> rt::Result<Handle> {
        self.get_all_request(cx, this, Kind::Key, query_or_options, count)
    }

    fn get_all_records(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: IDBGetAllOptions,
    ) -> rt::Result<Handle> {
        self.get_all_records_request(cx, this, options)
    }

    fn count(&self, cx: &mut Cx, this: Handle, query: Option<JsValue>) -> rt::Result<Handle> {
        self.count_request(cx, this, query)
    }

    fn open_cursor(
        &self,
        cx: &mut Cx,
        this: Handle,
        query: Option<JsValue>,
        direction: IDBCursorDirection,
    ) -> rt::Result<Handle> {
        self.open_cursor_request(cx, this, query, direction, false)
    }

    fn open_key_cursor(
        &self,
        cx: &mut Cx,
        this: Handle,
        query: Option<JsValue>,
        direction: IDBCursorDirection,
    ) -> rt::Result<Handle> {
        self.open_cursor_request(cx, this, query, direction, true)
    }
}
