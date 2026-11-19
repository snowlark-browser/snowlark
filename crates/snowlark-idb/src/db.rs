// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A database (IndexedDB §2.1) with its object stores (§2.2) and indexes (§2.6): the records in key
//! order, the index records in (index key, primary key) order, the key generators (§2.11).
//!
//! Every change is a [`Change`] applied with [`Database::apply`], which returns the [`Undo`] that
//! reverts it: a transaction keeps its changes (to write when it commits) and their undo entries (to
//! roll back when it aborts). The disk log stores changes and replays them with the same `apply`.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::key::{encode_key, encode_key_path, encode_str, Key, KeyPath, KeyRange, Reader};

/// A record's value: the bytes of StructuredSerializeForStorage.
pub type Value = Arc<[u8]>;

/// The largest key a key generator makes (§2.11: 2^53).
pub const GENERATOR_MAX: u64 = 1 << 53;

/// An object store record (§2.2): its value, and its keys in each index (the index records that
/// reference it).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    #[serde(with = "crate::wire::value")]
    pub value: Value,
    pub index_keys: Vec<(u64, Vec<Key>)>,
}

/// An index (§2.6). `ready` is false between `createIndex()` and the index's asynchronous population
/// in its upgrade transaction: storing records skips an index that is not ready.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    pub id: u64,
    pub name: String,
    pub key_path: KeyPath,
    pub unique: bool,
    pub multi_entry: bool,
    pub ready: bool,
    /// Index key → the primary keys of the records with that key.
    pub entries: BTreeMap<Key, BTreeSet<Key>>,
}

/// An object store (§2.2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Store {
    pub id: u64,
    pub name: String,
    pub key_path: Option<KeyPath>,
    pub auto_increment: bool,
    /// The key generator's current number (§2.11), from 1.
    pub current: u64,
    pub records: BTreeMap<Key, Record>,
    pub indexes: BTreeMap<u64, Index>,
}

/// A database (§2.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Database {
    pub name: String,
    pub version: u64,
    pub stores: BTreeMap<u64, Store>,
    /// The next object store or index id (never reused, so stale ids never alias).
    pub next_id: u64,
}

/// One change to a database.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Change {
    Version(u64),
    CreateStore {
        id: u64,
        name: String,
        key_path: Option<KeyPath>,
        auto_increment: bool,
    },
    DeleteStore(u64),
    RenameStore(u64, String),
    CreateIndex {
        store: u64,
        id: u64,
        name: String,
        key_path: KeyPath,
        unique: bool,
        multi_entry: bool,
    },
    /// An index's population: each record's keys in it (records not listed have none).
    PopulateIndex {
        store: u64,
        index: u64,
        keys: Vec<(Key, Vec<Key>)>,
    },
    DeleteIndex(u64, u64),
    RenameIndex(u64, u64, String),
    /// Stores a record (replacing one with the same key).
    Put {
        store: u64,
        key: Key,
        record: Record,
    },
    DeleteRange(u64, KeyRange),
    Clear(u64),
    Generator(u64, u64),
}

/// What reverts one change.
#[derive(Debug)]
pub enum Undo {
    Version(u64),
    RemoveStore(u64),
    RestoreStore(Box<Store>),
    RenameStore(u64, String),
    RemoveIndex(u64, u64),
    RestoreIndex(u64, Box<Index>),
    /// Restores the records' index keys for an index before its population.
    Unpopulate(u64, u64, Vec<Key>),
    RenameIndex(u64, u64, String),
    /// Puts back the records that were at these keys (`None`: there was none).
    Records(u64, Vec<(Key, Option<Record>)>),
    Generator(u64, u64),
    Nothing,
}

/// A record found by a read: (key, primary key, value). For an object store, the key is the primary
/// key.
pub type Found = (Key, Key, Value);

/// The direction of a cursor or of `getAll()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Next,
    NextUnique,
    Prev,
    PrevUnique,
}

impl Direction {
    pub fn forward(self) -> bool {
        matches!(self, Direction::Next | Direction::NextUnique)
    }
    pub fn unique(self) -> bool {
        matches!(self, Direction::NextUnique | Direction::PrevUnique)
    }
}

/// Where a cursor iterates: an object store or one of its indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Store(u64),
    Index(u64, u64),
}

/// The position and target of one cursor iteration step (§6.7).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Seek {
    pub position: Option<Key>,
    pub object_store_position: Option<Key>,
    pub key: Option<Key>,
    pub primary_key: Option<Key>,
}

impl Index {
    fn insert_entry(&mut self, k: Key, pk: Key) {
        self.entries.entry(k).or_default().insert(pk);
    }

    fn remove_entry(&mut self, k: &Key, pk: &Key) {
        if let Some(set) = self.entries.get_mut(k) {
            set.remove(pk);
            if set.is_empty() {
                self.entries.remove(k);
            }
        }
    }

    /// Whether a record other than `pk` has index key `k`.
    pub fn has_other(&self, k: &Key, pk: &Key) -> bool {
        self.entries
            .get(k)
            .is_some_and(|set| set.iter().any(|p| p != pk))
    }
}

impl Store {
    fn remove_record(&mut self, key: &Key) -> Option<Record> {
        let rec = self.records.remove(key)?;
        for (index, keys) in &rec.index_keys {
            if let Some(ix) = self.indexes.get_mut(index) {
                for k in keys {
                    ix.remove_entry(k, key);
                }
            }
        }
        Some(rec)
    }

    fn insert_record(&mut self, key: Key, rec: Record) {
        for (index, keys) in &rec.index_keys {
            if let Some(ix) = self.indexes.get_mut(index) {
                for k in keys {
                    ix.insert_entry(k.clone(), key.clone());
                }
            }
        }
        self.records.insert(key, rec);
    }

    /// The records whose keys are in `range`, in key order.
    pub fn range<'a>(
        &'a self,
        range: &KeyRange,
    ) -> Box<dyn DoubleEndedIterator<Item = (&'a Key, &'a Record)> + 'a> {
        match range.bounds() {
            Some(b) => Box::new(self.records.range::<Key, _>(b)),
            None => Box::new(std::iter::empty()),
        }
    }

    /// §2.11 "generate a key": the next key, or `None` past 2^53 (a "ConstraintError").
    pub fn generate_key(&self) -> Option<u64> {
        (self.current <= GENERATOR_MAX).then_some(self.current)
    }

    /// §2.11 "possibly update the key generator": the new current number for an explicit `key`, if
    /// it changes.
    pub fn updated_generator(&self, key: &Key) -> Option<u64> {
        let Key::Number(n) = key else {
            return None;
        };
        let value = n.min(GENERATOR_MAX as f64).floor();
        if value >= self.current as f64 {
            Some(value as u64 + 1)
        } else {
            None
        }
    }
}

impl Database {
    /// A new database with no object stores, at version 0.
    pub fn new(name: &str) -> Database {
        Database {
            name: name.to_owned(),
            version: 0,
            stores: BTreeMap::new(),
            next_id: 1,
        }
    }

    pub fn store(&self, id: u64) -> Option<&Store> {
        self.stores.get(&id)
    }

    pub fn store_mut(&mut self, id: u64) -> Option<&mut Store> {
        self.stores.get_mut(&id)
    }

    pub fn index(&self, store: u64, index: u64) -> Option<&Index> {
        self.stores.get(&store)?.indexes.get(&index)
    }

    /// The object store named `name`.
    pub fn store_named(&self, name: &str) -> Option<&Store> {
        self.stores.values().find(|s| s.name == name)
    }

    /// A fresh object store or index id.
    pub fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The schema alone: the stores and indexes without records or index entries. What a page
    /// keeps of a database it has a connection to.
    pub fn schema(&self) -> Database {
        Database {
            name: self.name.clone(),
            version: self.version,
            next_id: self.next_id,
            stores: self
                .stores
                .iter()
                .map(|(&id, s)| {
                    let indexes = s
                        .indexes
                        .iter()
                        .map(|(&i, ix)| {
                            let ix = Index {
                                id: ix.id,
                                name: ix.name.clone(),
                                key_path: ix.key_path.clone(),
                                unique: ix.unique,
                                multi_entry: ix.multi_entry,
                                ready: ix.ready,
                                entries: BTreeMap::new(),
                            };
                            (i, ix)
                        })
                        .collect();
                    let store = Store {
                        id: s.id,
                        name: s.name.clone(),
                        key_path: s.key_path.clone(),
                        auto_increment: s.auto_increment,
                        current: s.current,
                        records: BTreeMap::new(),
                        indexes,
                    };
                    (id, store)
                })
                .collect(),
        }
    }

    /// Applies what a change does to the schema (to a copy from [`Database::schema`]); record
    /// changes leave it as it is, but an index's population makes it ready.
    pub fn apply_schema(&mut self, change: &Change) {
        match change {
            Change::Version(_)
            | Change::CreateStore { .. }
            | Change::DeleteStore(_)
            | Change::RenameStore(..)
            | Change::CreateIndex { .. }
            | Change::DeleteIndex(..)
            | Change::RenameIndex(..) => {
                self.apply(change.clone());
            }
            Change::PopulateIndex { store, index, .. } => {
                if let Some(ix) = self
                    .stores
                    .get_mut(store)
                    .and_then(|s| s.indexes.get_mut(index))
                {
                    ix.ready = true;
                }
            }
            _ => {}
        }
    }

    /// The ids a schema change creates (an object store's or an index's).
    pub fn created_id(change: &Change) -> Option<u64> {
        match change {
            Change::CreateStore { id, .. } => Some(*id),
            Change::CreateIndex { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// Applies a change; returns what reverts it.
    pub fn apply(&mut self, change: Change) -> Undo {
        match change {
            Change::Version(v) => Undo::Version(std::mem::replace(&mut self.version, v)),
            Change::CreateStore {
                id,
                name,
                key_path,
                auto_increment,
            } => {
                self.next_id = self.next_id.max(id + 1);
                self.stores.insert(
                    id,
                    Store {
                        id,
                        name,
                        key_path,
                        auto_increment,
                        current: 1,
                        records: BTreeMap::new(),
                        indexes: BTreeMap::new(),
                    },
                );
                Undo::RemoveStore(id)
            }
            Change::DeleteStore(id) => match self.stores.remove(&id) {
                Some(s) => Undo::RestoreStore(Box::new(s)),
                None => Undo::Nothing,
            },
            Change::RenameStore(id, name) => match self.stores.get_mut(&id) {
                Some(s) => Undo::RenameStore(id, std::mem::replace(&mut s.name, name)),
                None => Undo::Nothing,
            },
            Change::CreateIndex {
                store,
                id,
                name,
                key_path,
                unique,
                multi_entry,
            } => {
                self.next_id = self.next_id.max(id + 1);
                let Some(s) = self.stores.get_mut(&store) else {
                    return Undo::Nothing;
                };
                s.indexes.insert(
                    id,
                    Index {
                        id,
                        name,
                        key_path,
                        unique,
                        multi_entry,
                        ready: false,
                        entries: BTreeMap::new(),
                    },
                );
                Undo::RemoveIndex(store, id)
            }
            Change::PopulateIndex { store, index, keys } => {
                let Some(s) = self.stores.get_mut(&store) else {
                    return Undo::Nothing;
                };
                let mut touched = Vec::with_capacity(keys.len());
                for (pk, ks) in keys {
                    let Some(rec) = s.records.get_mut(&pk) else {
                        continue;
                    };
                    rec.index_keys.retain(|(i, _)| *i != index);
                    if let Some(ix) = s.indexes.get_mut(&index) {
                        for k in &ks {
                            ix.insert_entry(k.clone(), pk.clone());
                        }
                    }
                    rec.index_keys.push((index, ks));
                    touched.push(pk);
                }
                if let Some(ix) = s.indexes.get_mut(&index) {
                    ix.ready = true;
                }
                Undo::Unpopulate(store, index, touched)
            }
            Change::DeleteIndex(store, id) => {
                match self
                    .stores
                    .get_mut(&store)
                    .and_then(|s| s.indexes.remove(&id))
                {
                    Some(ix) => Undo::RestoreIndex(store, Box::new(ix)),
                    None => Undo::Nothing,
                }
            }
            Change::RenameIndex(store, id, name) => {
                match self
                    .stores
                    .get_mut(&store)
                    .and_then(|s| s.indexes.get_mut(&id))
                {
                    Some(ix) => Undo::RenameIndex(store, id, std::mem::replace(&mut ix.name, name)),
                    None => Undo::Nothing,
                }
            }
            Change::Put { store, key, record } => {
                let Some(s) = self.stores.get_mut(&store) else {
                    return Undo::Nothing;
                };
                let old = s.remove_record(&key);
                s.insert_record(key.clone(), record);
                Undo::Records(store, vec![(key, old)])
            }
            Change::DeleteRange(store, range) => {
                let Some(s) = self.stores.get_mut(&store) else {
                    return Undo::Nothing;
                };
                let keys: Vec<Key> = match range.single() {
                    Some(k) => s
                        .records
                        .contains_key(k)
                        .then(|| k.clone())
                        .into_iter()
                        .collect(),
                    None => s.range(&range).map(|(k, _)| k.clone()).collect(),
                };
                let removed = keys
                    .into_iter()
                    .map(|k| {
                        let r = s.remove_record(&k);
                        (k, r)
                    })
                    .collect();
                Undo::Records(store, removed)
            }
            Change::Clear(store) => {
                let Some(s) = self.stores.get_mut(&store) else {
                    return Undo::Nothing;
                };
                let records = std::mem::take(&mut s.records);
                for ix in s.indexes.values_mut() {
                    ix.entries.clear();
                }
                Undo::Records(
                    store,
                    records.into_iter().map(|(k, r)| (k, Some(r))).collect(),
                )
            }
            Change::Generator(store, current) => match self.stores.get_mut(&store) {
                Some(s) => Undo::Generator(store, std::mem::replace(&mut s.current, current)),
                None => Undo::Nothing,
            },
        }
    }

    /// Reverts one change.
    pub fn revert(&mut self, undo: Undo) {
        match undo {
            Undo::Version(v) => self.version = v,
            Undo::RemoveStore(id) => {
                self.stores.remove(&id);
            }
            Undo::RestoreStore(s) => {
                self.stores.insert(s.id, *s);
            }
            Undo::RenameStore(id, name) => {
                if let Some(s) = self.stores.get_mut(&id) {
                    s.name = name;
                }
            }
            Undo::RemoveIndex(store, id) => {
                if let Some(s) = self.stores.get_mut(&store) {
                    s.indexes.remove(&id);
                    for rec in s.records.values_mut() {
                        rec.index_keys.retain(|(i, _)| *i != id);
                    }
                }
            }
            Undo::RestoreIndex(store, ix) => {
                if let Some(s) = self.stores.get_mut(&store) {
                    s.indexes.insert(ix.id, *ix);
                }
            }
            Undo::Unpopulate(store, index, keys) => {
                if let Some(s) = self.stores.get_mut(&store) {
                    for pk in keys {
                        if let Some(rec) = s.records.get_mut(&pk) {
                            rec.index_keys.retain(|(i, _)| *i != index);
                        }
                    }
                    if let Some(ix) = s.indexes.get_mut(&index) {
                        ix.entries.clear();
                        ix.ready = false;
                    }
                }
            }
            Undo::RenameIndex(store, id, name) => {
                if let Some(ix) = self
                    .stores
                    .get_mut(&store)
                    .and_then(|s| s.indexes.get_mut(&id))
                {
                    ix.name = name;
                }
            }
            Undo::Records(store, records) => {
                if let Some(s) = self.stores.get_mut(&store) {
                    for (key, old) in records.into_iter().rev() {
                        s.remove_record(&key);
                        if let Some(r) = old {
                            s.insert_record(key, r);
                        }
                    }
                }
            }
            Undo::Generator(store, current) => {
                if let Some(s) = self.stores.get_mut(&store) {
                    s.current = current;
                }
            }
            Undo::Nothing => {}
        }
    }

    // ----- reads ---------------------------------------------------------------------------------------

    /// The number of records of a store or an index whose keys are in `range` (§6.5).
    pub fn count(&self, source: Source, range: &KeyRange) -> u64 {
        match source {
            Source::Store(s) => self.stores.get(&s).map_or(0, |s| match range.single() {
                Some(k) => s.records.contains_key(k) as u64,
                None => s.range(range).count() as u64,
            }),
            Source::Index(s, i) => self.index(s, i).map_or(0, |ix| match range.bounds() {
                Some(b) => ix
                    .entries
                    .range::<Key, _>(b)
                    .map(|(_, set)| set.len() as u64)
                    .sum(),
                None => 0,
            }),
        }
    }

    /// The records of a store or an index in `range` in `direction`, at most `count` (0: all)
    /// (§6.2, §6.3 "retrieve multiple items").
    pub fn get_all(
        &self,
        source: Source,
        range: &KeyRange,
        direction: Direction,
        count: u32,
    ) -> Vec<Found> {
        let limit = if count == 0 {
            usize::MAX
        } else {
            count as usize
        };
        let mut out = Vec::new();
        match source {
            Source::Store(s) => {
                let Some(s) = self.stores.get(&s) else {
                    return out;
                };
                let it = s.range(range);
                let push = |out: &mut Vec<Found>, (k, r): (&Key, &Record)| {
                    out.push((k.clone(), k.clone(), r.value.clone()))
                };
                if direction.forward() {
                    for e in it.take(limit) {
                        push(&mut out, e);
                    }
                } else {
                    for e in it.rev().take(limit) {
                        push(&mut out, e);
                    }
                }
            }
            Source::Index(si, ii) => {
                let (Some(s), Some(ix)) = (self.stores.get(&si), self.index(si, ii)) else {
                    return out;
                };
                let Some(b) = range.bounds() else {
                    return out;
                };
                let entries = ix.entries.range::<Key, _>(b);
                let value = |pk: &Key| s.records.get(pk).map(|r| r.value.clone());
                let push = |out: &mut Vec<Found>, k: &Key, pk: &Key| {
                    if let Some(v) = value(pk) {
                        out.push((k.clone(), pk.clone(), v));
                    }
                    out.len() >= limit
                };
                match direction {
                    Direction::Next => {
                        'outer: for (k, set) in entries {
                            for pk in set {
                                if push(&mut out, k, pk) {
                                    break 'outer;
                                }
                            }
                        }
                    }
                    Direction::Prev => {
                        'outer: for (k, set) in entries.rev() {
                            for pk in set.iter().rev() {
                                if push(&mut out, k, pk) {
                                    break 'outer;
                                }
                            }
                        }
                    }
                    Direction::NextUnique => {
                        for (k, set) in entries {
                            if let Some(pk) = set.iter().next() {
                                if push(&mut out, k, pk) {
                                    break;
                                }
                            }
                        }
                    }
                    Direction::PrevUnique => {
                        for (k, set) in entries.rev() {
                            if let Some(pk) = set.iter().next() {
                                if push(&mut out, k, pk) {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// The first record of a store or an index in `range`.
    pub fn first(&self, source: Source, range: &KeyRange) -> Option<Found> {
        if let (Source::Store(s), Some(k)) = (source, range.single()) {
            let rec = self.stores.get(&s)?.records.get(k)?;
            return Some((k.clone(), k.clone(), rec.value.clone()));
        }
        self.get_all(source, range, Direction::Next, 1).pop()
    }

    /// §6.7 "iterate a cursor": `count` steps (at least one) from `seek`, each from the record the
    /// last one found; the last record, or `None` once a step finds none.
    pub fn cursor_advance(
        &self,
        source: Source,
        direction: Direction,
        range: &KeyRange,
        mut seek: Seek,
        count: u32,
    ) -> Option<Found> {
        let mut found = None;
        for _ in 0..count.max(1) {
            let f = self.cursor_step(source, direction, range, &seek)?;
            seek.position = Some(f.0.clone());
            if matches!(source, Source::Index(..)) {
                seek.object_store_position = Some(f.1.clone());
            }
            found = Some(f);
        }
        found
    }

    /// §6.7 "iterate a cursor", one step: the record found from `seek` in `direction` within
    /// `range`.
    pub fn cursor_step(
        &self,
        source: Source,
        direction: Direction,
        range: &KeyRange,
        seek: &Seek,
    ) -> Option<Found> {
        match source {
            Source::Store(s) => {
                let s = self.stores.get(&s)?;
                let b = range.bounds()?;
                let (lo, hi) = (b.0.cloned(), b.1.cloned());
                if direction.forward() {
                    let mut lo = lo;
                    if let Some(k) = &seek.key {
                        lo = max_lower(lo, Bound::Included(k.clone()));
                    }
                    if let Some(p) = &seek.position {
                        lo = max_lower(lo, Bound::Excluded(p.clone()));
                    }
                    let (k, r) = range_iter(&s.records, lo, hi)?.next()?;
                    Some((k.clone(), k.clone(), r.value.clone()))
                } else {
                    let mut hi = hi;
                    if let Some(k) = &seek.key {
                        hi = min_upper(hi, Bound::Included(k.clone()));
                    }
                    if let Some(p) = &seek.position {
                        hi = min_upper(hi, Bound::Excluded(p.clone()));
                    }
                    let (k, r) = range_iter(&s.records, lo, hi)?.next_back()?;
                    Some((k.clone(), k.clone(), r.value.clone()))
                }
            }
            Source::Index(si, ii) => {
                let s = self.stores.get(&si)?;
                let ix = s.indexes.get(&ii)?;
                let b = range.bounds()?;
                let (lo, hi) = (b.0.cloned(), b.1.cloned());
                let value = |pk: &Key| s.records.get(pk).map(|r| r.value.clone());
                match direction {
                    Direction::Next | Direction::NextUnique => {
                        // The lowest (key, primary key) above every lower bound: the range's, the
                        // key's and primary key's, the position's.
                        let mut lo = PairBound::from_key_lower(lo);
                        if let Some(k) = &seek.key {
                            let b = match &seek.primary_key {
                                Some(pk) => PairBound::At(k.clone(), Bound::Included(pk.clone())),
                                None => PairBound::from_key_lower(Bound::Included(k.clone())),
                            };
                            lo = lo.max(b);
                        }
                        if let Some(p) = &seek.position {
                            let b = match (&seek.object_store_position, direction) {
                                (Some(op), Direction::Next) => {
                                    PairBound::At(p.clone(), Bound::Excluded(op.clone()))
                                }
                                _ => PairBound::from_key_lower(Bound::Excluded(p.clone())),
                            };
                            lo = lo.max(b);
                        }
                        let (k, pk) = lo.first_in(&ix.entries, &hi)?;
                        let pk = if direction == Direction::NextUnique {
                            ix.entries.get(&k)?.iter().next()?.clone()
                        } else {
                            pk
                        };
                        let v = value(&pk)?;
                        Some((k, pk, v))
                    }
                    Direction::Prev => {
                        let mut hi = PairBound::from_key_upper(hi);
                        if let Some(k) = &seek.key {
                            let b = match &seek.primary_key {
                                Some(pk) => PairBound::At(k.clone(), Bound::Included(pk.clone())),
                                None => PairBound::from_key_upper(Bound::Included(k.clone())),
                            };
                            hi = hi.min(b);
                        }
                        if let Some(p) = &seek.position {
                            let b = match &seek.object_store_position {
                                Some(op) => PairBound::At(p.clone(), Bound::Excluded(op.clone())),
                                None => PairBound::from_key_upper(Bound::Excluded(p.clone())),
                            };
                            hi = hi.min(b);
                        }
                        let (k, pk) = hi.last_in(&ix.entries, &lo)?;
                        let v = value(&pk)?;
                        Some((k, pk, v))
                    }
                    Direction::PrevUnique => {
                        let mut hi = hi;
                        if let Some(k) = &seek.key {
                            hi = min_upper(hi, Bound::Included(k.clone()));
                        }
                        if let Some(p) = &seek.position {
                            hi = min_upper(hi, Bound::Excluded(p.clone()));
                        }
                        let (k, set) = range_iter(&ix.entries, lo, hi)?.next_back()?;
                        let pk = set.iter().next()?.clone();
                        let v = value(&pk)?;
                        Some((k.clone(), pk, v))
                    }
                }
            }
        }
    }
}

/// `map.range(lo..hi)`, or `None` when the bounds are empty or crossed.
fn range_iter<V>(
    map: &BTreeMap<Key, V>,
    lo: Bound<Key>,
    hi: Bound<Key>,
) -> Option<std::collections::btree_map::Range<'_, Key, V>> {
    if let (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) =
        (&lo, &hi)
    {
        match a.cmp(b) {
            std::cmp::Ordering::Greater => return None,
            std::cmp::Ordering::Equal
                if matches!(lo, Bound::Excluded(_)) || matches!(hi, Bound::Excluded(_)) =>
            {
                return None
            }
            _ => {}
        }
    }
    Some(map.range((lo, hi)))
}

/// The tighter of two lower bounds.
fn max_lower(a: Bound<Key>, b: Bound<Key>) -> Bound<Key> {
    match (&a, &b) {
        (Bound::Unbounded, _) => b,
        (_, Bound::Unbounded) => a,
        (Bound::Included(x) | Bound::Excluded(x), Bound::Included(y) | Bound::Excluded(y)) => {
            match x.cmp(y) {
                std::cmp::Ordering::Less => b,
                std::cmp::Ordering::Greater => a,
                std::cmp::Ordering::Equal => {
                    if matches!(a, Bound::Excluded(_)) {
                        a
                    } else {
                        b
                    }
                }
            }
        }
    }
}

/// The tighter of two upper bounds.
fn min_upper(a: Bound<Key>, b: Bound<Key>) -> Bound<Key> {
    match (&a, &b) {
        (Bound::Unbounded, _) => b,
        (_, Bound::Unbounded) => a,
        (Bound::Included(x) | Bound::Excluded(x), Bound::Included(y) | Bound::Excluded(y)) => {
            match x.cmp(y) {
                std::cmp::Ordering::Less => a,
                std::cmp::Ordering::Greater => b,
                std::cmp::Ordering::Equal => {
                    if matches!(a, Bound::Excluded(_)) {
                        a
                    } else {
                        b
                    }
                }
            }
        }
    }
}

/// A bound on (index key, primary key) pairs.
#[derive(Clone, Debug)]
enum PairBound {
    /// No bound.
    None,
    /// Every pair with index key below (lower bound) or above (upper bound) this key is out; pairs
    /// with this key are in when the flag is true.
    Key(Key, bool),
    /// At this index key, the bound on the primary key.
    At(Key, Bound<Key>),
}

impl PairBound {
    fn from_key_lower(b: Bound<Key>) -> PairBound {
        match b {
            Bound::Unbounded => PairBound::None,
            Bound::Included(k) => PairBound::Key(k, true),
            Bound::Excluded(k) => PairBound::Key(k, false),
        }
    }

    fn from_key_upper(b: Bound<Key>) -> PairBound {
        PairBound::from_key_lower(b)
    }

    fn key(&self) -> Option<&Key> {
        match self {
            PairBound::None => None,
            PairBound::Key(k, _) | PairBound::At(k, _) => Some(k),
        }
    }

    /// Whether (k, pk) satisfies this as a lower bound.
    fn admits_lower(&self, k: &Key, pk: &Key) -> bool {
        match self {
            PairBound::None => true,
            PairBound::Key(b, incl) => k > b || (*incl && k == b),
            PairBound::At(b, pb) => {
                k > b
                    || (k == b
                        && match pb {
                            Bound::Unbounded => true,
                            Bound::Included(x) => pk >= x,
                            Bound::Excluded(x) => pk > x,
                        })
            }
        }
    }

    /// Whether (k, pk) satisfies this as an upper bound.
    fn admits_upper(&self, k: &Key, pk: &Key) -> bool {
        match self {
            PairBound::None => true,
            PairBound::Key(b, incl) => k < b || (*incl && k == b),
            PairBound::At(b, pb) => {
                k < b
                    || (k == b
                        && match pb {
                            Bound::Unbounded => true,
                            Bound::Included(x) => pk <= x,
                            Bound::Excluded(x) => pk < x,
                        })
            }
        }
    }

    /// The tighter of two lower bounds (the one that admits less): compared by where they start.
    fn max(self, other: PairBound) -> PairBound {
        let (Some(a), Some(b)) = (self.key(), other.key()) else {
            return if matches!(self, PairBound::None) {
                other
            } else {
                self
            };
        };
        match a.cmp(b) {
            std::cmp::Ordering::Less => other,
            std::cmp::Ordering::Greater => self,
            std::cmp::Ordering::Equal => {
                // At the same key: excluding the key beats any primary key bound, which beats
                // including it.
                match (&self, &other) {
                    (PairBound::Key(_, false), _) => self,
                    (_, PairBound::Key(_, false)) => other,
                    (PairBound::Key(_, true), _) => other,
                    (_, PairBound::Key(_, true)) => self,
                    (PairBound::At(_, x), PairBound::At(_, y)) => {
                        if bound_above(x, y) {
                            self
                        } else {
                            other
                        }
                    }
                    _ => self,
                }
            }
        }
    }

    /// The tighter of two upper bounds.
    fn min(self, other: PairBound) -> PairBound {
        let (Some(a), Some(b)) = (self.key(), other.key()) else {
            return if matches!(self, PairBound::None) {
                other
            } else {
                self
            };
        };
        match a.cmp(b) {
            std::cmp::Ordering::Less => self,
            std::cmp::Ordering::Greater => other,
            std::cmp::Ordering::Equal => match (&self, &other) {
                (PairBound::Key(_, false), _) => self,
                (_, PairBound::Key(_, false)) => other,
                (PairBound::Key(_, true), _) => other,
                (_, PairBound::Key(_, true)) => self,
                (PairBound::At(_, x), PairBound::At(_, y)) => {
                    if bound_above(x, y) {
                        other
                    } else {
                        self
                    }
                }
                _ => self,
            },
        }
    }

    /// The first pair at or above this lower bound with its key within `hi`.
    fn first_in(
        &self,
        entries: &BTreeMap<Key, BTreeSet<Key>>,
        hi: &Bound<Key>,
    ) -> Option<(Key, Key)> {
        let start = match self.key() {
            Some(k) => Bound::Included(k.clone()),
            None => Bound::Unbounded,
        };
        let iter = range_iter(entries, start, hi.clone())?;
        for (k, set) in iter {
            for pk in set {
                if self.admits_lower(k, pk) {
                    return Some((k.clone(), pk.clone()));
                }
                if !matches!(self, PairBound::At(..)) || Some(k) != self.key() {
                    break;
                }
            }
        }
        None
    }

    /// The last pair at or below this upper bound with its key within `lo`.
    fn last_in(
        &self,
        entries: &BTreeMap<Key, BTreeSet<Key>>,
        lo: &Bound<Key>,
    ) -> Option<(Key, Key)> {
        let end = match self.key() {
            Some(k) => Bound::Included(k.clone()),
            None => Bound::Unbounded,
        };
        let iter = range_iter(entries, lo.clone(), end)?;
        for (k, set) in iter.rev() {
            for pk in set.iter().rev() {
                if self.admits_upper(k, pk) {
                    return Some((k.clone(), pk.clone()));
                }
                if !matches!(self, PairBound::At(..)) || Some(k) != self.key() {
                    break;
                }
            }
        }
        None
    }
}

/// Whether bound `x` on a primary key starts above bound `y` (admits less as a lower bound).
fn bound_above(x: &Bound<Key>, y: &Bound<Key>) -> bool {
    match (x, y) {
        (Bound::Unbounded, _) => false,
        (_, Bound::Unbounded) => true,
        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
            match a.cmp(b) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => matches!(x, Bound::Excluded(_)),
            }
        }
    }
}

// ----- encoding changes ------------------------------------------------------------------------------

const C_VERSION: u8 = 1;
const C_CREATE_STORE: u8 = 2;
const C_DELETE_STORE: u8 = 3;
const C_RENAME_STORE: u8 = 4;
const C_CREATE_INDEX: u8 = 5;
const C_POPULATE_INDEX: u8 = 6;
const C_DELETE_INDEX: u8 = 7;
const C_RENAME_INDEX: u8 = 8;
const C_PUT: u8 = 9;
const C_DELETE_RANGE: u8 = 10;
const C_CLEAR: u8 = 11;
const C_GENERATOR: u8 = 12;

fn encode_range(r: &KeyRange, out: &mut Vec<u8>) {
    let flags = (r.lower.is_some() as u8)
        | ((r.upper.is_some() as u8) << 1)
        | ((r.lower_open as u8) << 2)
        | ((r.upper_open as u8) << 3);
    out.push(flags);
    if let Some(k) = &r.lower {
        encode_key(k, out);
    }
    if let Some(k) = &r.upper {
        encode_key(k, out);
    }
}

fn encode_index_keys(keys: &[(u64, Vec<Key>)], out: &mut Vec<u8>) {
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for (i, ks) in keys {
        out.extend_from_slice(&i.to_le_bytes());
        out.extend_from_slice(&(ks.len() as u32).to_le_bytes());
        for k in ks {
            encode_key(k, out);
        }
    }
}

/// Appends a change's encoding.
pub fn encode_change(c: &Change, out: &mut Vec<u8>) {
    let u64le = |out: &mut Vec<u8>, v: u64| out.extend_from_slice(&v.to_le_bytes());
    match c {
        Change::Version(v) => {
            out.push(C_VERSION);
            u64le(out, *v);
        }
        Change::CreateStore {
            id,
            name,
            key_path,
            auto_increment,
        } => {
            out.push(C_CREATE_STORE);
            u64le(out, *id);
            encode_str(name, out);
            encode_key_path(key_path.as_ref(), out);
            out.push(*auto_increment as u8);
        }
        Change::DeleteStore(id) => {
            out.push(C_DELETE_STORE);
            u64le(out, *id);
        }
        Change::RenameStore(id, name) => {
            out.push(C_RENAME_STORE);
            u64le(out, *id);
            encode_str(name, out);
        }
        Change::CreateIndex {
            store,
            id,
            name,
            key_path,
            unique,
            multi_entry,
        } => {
            out.push(C_CREATE_INDEX);
            u64le(out, *store);
            u64le(out, *id);
            encode_str(name, out);
            encode_key_path(Some(key_path), out);
            out.push(*unique as u8 | ((*multi_entry as u8) << 1));
        }
        Change::PopulateIndex { store, index, keys } => {
            out.push(C_POPULATE_INDEX);
            u64le(out, *store);
            u64le(out, *index);
            out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
            for (pk, ks) in keys {
                encode_key(pk, out);
                out.extend_from_slice(&(ks.len() as u32).to_le_bytes());
                for k in ks {
                    encode_key(k, out);
                }
            }
        }
        Change::DeleteIndex(store, id) => {
            out.push(C_DELETE_INDEX);
            u64le(out, *store);
            u64le(out, *id);
        }
        Change::RenameIndex(store, id, name) => {
            out.push(C_RENAME_INDEX);
            u64le(out, *store);
            u64le(out, *id);
            encode_str(name, out);
        }
        Change::Put { store, key, record } => {
            out.push(C_PUT);
            u64le(out, *store);
            encode_key(key, out);
            out.extend_from_slice(&(record.value.len() as u32).to_le_bytes());
            out.extend_from_slice(&record.value);
            encode_index_keys(&record.index_keys, out);
        }
        Change::DeleteRange(store, range) => {
            out.push(C_DELETE_RANGE);
            u64le(out, *store);
            encode_range(range, out);
        }
        Change::Clear(store) => {
            out.push(C_CLEAR);
            u64le(out, *store);
        }
        Change::Generator(store, current) => {
            out.push(C_GENERATOR);
            u64le(out, *store);
            u64le(out, *current);
        }
    }
}

impl Reader<'_> {
    fn range(&mut self) -> Option<KeyRange> {
        let flags = self.u8()?;
        let lower = if flags & 1 != 0 {
            Some(self.key()?)
        } else {
            None
        };
        let upper = if flags & 2 != 0 {
            Some(self.key()?)
        } else {
            None
        };
        Some(KeyRange {
            lower,
            upper,
            lower_open: flags & 4 != 0,
            upper_open: flags & 8 != 0,
        })
    }

    fn keys(&mut self) -> Option<Vec<Key>> {
        let n = self.u32()? as usize;
        let mut v = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            v.push(self.key()?);
        }
        Some(v)
    }

    /// Reads one change.
    pub fn change(&mut self) -> Option<Change> {
        Some(match self.u8()? {
            C_VERSION => Change::Version(self.u64()?),
            C_CREATE_STORE => Change::CreateStore {
                id: self.u64()?,
                name: self.string()?,
                key_path: self.key_path()?,
                auto_increment: self.u8()? != 0,
            },
            C_DELETE_STORE => Change::DeleteStore(self.u64()?),
            C_RENAME_STORE => Change::RenameStore(self.u64()?, self.string()?),
            C_CREATE_INDEX => {
                let store = self.u64()?;
                let id = self.u64()?;
                let name = self.string()?;
                let key_path = self.key_path()??;
                let flags = self.u8()?;
                Change::CreateIndex {
                    store,
                    id,
                    name,
                    key_path,
                    unique: flags & 1 != 0,
                    multi_entry: flags & 2 != 0,
                }
            }
            C_POPULATE_INDEX => {
                let store = self.u64()?;
                let index = self.u64()?;
                let n = self.u32()? as usize;
                let mut keys = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let pk = self.key()?;
                    keys.push((pk, self.keys()?));
                }
                Change::PopulateIndex { store, index, keys }
            }
            C_DELETE_INDEX => Change::DeleteIndex(self.u64()?, self.u64()?),
            C_RENAME_INDEX => Change::RenameIndex(self.u64()?, self.u64()?, self.string()?),
            C_PUT => {
                let store = self.u64()?;
                let key = self.key()?;
                let value: Value = Arc::from(self.bytes()?);
                let n = self.u32()? as usize;
                let mut index_keys = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    let i = self.u64()?;
                    index_keys.push((i, self.keys()?));
                }
                Change::Put {
                    store,
                    key,
                    record: Record { value, index_keys },
                }
            }
            C_DELETE_RANGE => Change::DeleteRange(self.u64()?, self.range()?),
            C_CLEAR => Change::Clear(self.u64()?),
            C_GENERATOR => Change::Generator(self.u64()?, self.u64()?),
            _ => return None,
        })
    }
}

impl Database {
    /// The changes that rebuild this database from nothing (a compacted log).
    pub fn snapshot(&self) -> Vec<Change> {
        let mut out = vec![Change::Version(self.version)];
        for s in self.stores.values() {
            out.push(Change::CreateStore {
                id: s.id,
                name: s.name.clone(),
                key_path: s.key_path.clone(),
                auto_increment: s.auto_increment,
            });
            if s.current != 1 {
                out.push(Change::Generator(s.id, s.current));
            }
            for ix in s.indexes.values() {
                out.push(Change::CreateIndex {
                    store: s.id,
                    id: ix.id,
                    name: ix.name.clone(),
                    key_path: ix.key_path.clone(),
                    unique: ix.unique,
                    multi_entry: ix.multi_entry,
                });
            }
            for (k, r) in &s.records {
                out.push(Change::Put {
                    store: s.id,
                    key: k.clone(),
                    record: r.clone(),
                });
            }
            // (the records carried their index keys; an index that was ready is ready again)
            for ix in s.indexes.values().filter(|ix| ix.ready) {
                out.push(Change::PopulateIndex {
                    store: s.id,
                    index: ix.id,
                    keys: Vec::new(),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(x: f64) -> Key {
        Key::Number(x)
    }

    fn val(b: &[u8]) -> Value {
        Arc::from(b)
    }

    fn db_with_index() -> Database {
        let mut db = Database::new("d");
        db.apply(Change::CreateStore {
            id: 1,
            name: "s".into(),
            key_path: None,
            auto_increment: false,
        });
        db.apply(Change::CreateIndex {
            store: 1,
            id: 2,
            name: "i".into(),
            key_path: KeyPath::String("x".into()),
            unique: false,
            multi_entry: false,
        });
        db.apply(Change::PopulateIndex {
            store: 1,
            index: 2,
            keys: vec![],
        });
        // index key → primary keys: a → {1, 2}, b → {3}, c → {4, 5}
        for (pk, ik) in [(1.0, "a"), (2.0, "a"), (3.0, "b"), (4.0, "c"), (5.0, "c")] {
            db.apply(Change::Put {
                store: 1,
                key: n(pk),
                record: Record {
                    value: val(&[pk as u8]),
                    index_keys: vec![(2, vec![Key::string(ik)])],
                },
            });
        }
        db
    }

    fn walk(db: &Database, dir: Direction) -> Vec<(String, f64)> {
        let src = Source::Index(1, 2);
        let mut seek = Seek::default();
        let mut out = Vec::new();
        while let Some((k, pk, _)) = db.cursor_step(src, dir, &KeyRange::unbounded(), &seek) {
            let Key::String(s) = &k else { panic!() };
            let Key::Number(p) = pk else { panic!() };
            out.push((String::from_utf16(s).unwrap(), p));
            seek.position = Some(k);
            seek.object_store_position = Some(pk);
        }
        out
    }

    #[test]
    fn index_cursor_directions() {
        let db = db_with_index();
        let pairs = |v: &[(&str, f64)]| -> Vec<(String, f64)> {
            v.iter().map(|(a, b)| (a.to_string(), *b)).collect()
        };
        assert_eq!(
            walk(&db, Direction::Next),
            pairs(&[("a", 1.0), ("a", 2.0), ("b", 3.0), ("c", 4.0), ("c", 5.0)])
        );
        assert_eq!(
            walk(&db, Direction::Prev),
            pairs(&[("c", 5.0), ("c", 4.0), ("b", 3.0), ("a", 2.0), ("a", 1.0)])
        );
        assert_eq!(
            walk(&db, Direction::NextUnique),
            pairs(&[("a", 1.0), ("b", 3.0), ("c", 4.0)])
        );
        assert_eq!(
            walk(&db, Direction::PrevUnique),
            pairs(&[("c", 4.0), ("b", 3.0), ("a", 1.0)])
        );
    }

    #[test]
    fn continue_primary_key() {
        let db = db_with_index();
        let seek = Seek {
            position: Some(Key::string("a")),
            object_store_position: Some(n(1.0)),
            key: Some(Key::string("c")),
            primary_key: Some(n(5.0)),
        };
        let (k, pk, _) = db
            .cursor_step(
                Source::Index(1, 2),
                Direction::Next,
                &KeyRange::unbounded(),
                &seek,
            )
            .unwrap();
        assert_eq!((k, pk), (Key::string("c"), n(5.0)));
        let seek = Seek {
            key: Some(Key::string("b")),
            primary_key: Some(n(4.0)),
            ..Seek::default()
        };
        let (k, pk, _) = db
            .cursor_step(
                Source::Index(1, 2),
                Direction::Next,
                &KeyRange::unbounded(),
                &seek,
            )
            .unwrap();
        assert_eq!((k, pk), (Key::string("c"), n(4.0)));
    }

    #[test]
    fn undo_restores_records_and_indexes() {
        let mut db = db_with_index();
        let mut undo = vec![db.apply(Change::DeleteRange(
            1,
            KeyRange {
                lower: Some(n(2.0)),
                upper: Some(n(4.0)),
                ..KeyRange::default()
            },
        ))];
        undo.push(db.apply(Change::Put {
            store: 1,
            key: n(1.0),
            record: Record {
                value: val(b"z"),
                index_keys: vec![(2, vec![Key::string("z")])],
            },
        }));
        undo.push(db.apply(Change::Clear(1)));
        assert_eq!(db.count(Source::Store(1), &KeyRange::unbounded()), 0);
        for u in undo.into_iter().rev() {
            db.revert(u);
        }
        assert_eq!(walk(&db, Direction::Next).len(), 5);
        assert_eq!(db.count(Source::Index(1, 2), &KeyRange::unbounded()), 5);
        let (_, _, v) = db.first(Source::Store(1), &KeyRange::only(n(1.0))).unwrap();
        assert_eq!(&*v, &[1]);
    }

    #[test]
    fn get_all_and_count() {
        let db = db_with_index();
        let src = Source::Store(1);
        let r = KeyRange::unbounded();
        let keys = |v: Vec<Found>| -> Vec<Key> { v.into_iter().map(|f| f.0).collect() };
        assert_eq!(
            keys(db.get_all(src, &r, Direction::Prev, 2)),
            vec![n(5.0), n(4.0)]
        );
        assert_eq!(
            keys(db.get_all(Source::Index(1, 2), &r, Direction::PrevUnique, 0)),
            vec![Key::string("c"), Key::string("b"), Key::string("a")]
        );
        assert_eq!(
            db.count(Source::Index(1, 2), &KeyRange::only(Key::string("c"))),
            2
        );
    }

    #[test]
    fn generator() {
        let mut db = db_with_index();
        let s = db.store(1).unwrap();
        assert_eq!(s.generate_key(), Some(1));
        assert_eq!(s.updated_generator(&n(6.5)), Some(7));
        assert_eq!(s.updated_generator(&n(-3.0)), None);
        assert_eq!(s.updated_generator(&n(1e300)), Some(GENERATOR_MAX + 1));
        db.apply(Change::Generator(1, GENERATOR_MAX + 1));
        assert_eq!(db.store(1).unwrap().generate_key(), None);
    }

    #[test]
    fn changes_round_trip_and_snapshot() {
        let db = db_with_index();
        let mut buf = Vec::new();
        for c in db.snapshot() {
            encode_change(&c, &mut buf);
        }
        let mut r = Reader::new(&buf);
        let mut copy = Database::new("d");
        while !r.at_end() {
            copy.apply(r.change().unwrap());
        }
        assert_eq!(walk(&copy, Direction::Next), walk(&db, Direction::Next));
        assert!(copy.index(1, 2).unwrap().ready);
    }
}
