// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §6.11.3 the drag data store's script interfaces: `DataTransfer`, its
//! `DataTransferItemList` and `DataTransferItem`s, as script makes them (`new DataTransfer()`,
//! which pages and tests use to give a file input files through its `files` setter, read/write),
//! as the clipboard events make them (read-only for a paste) and as the drag-and-drop events make
//! them ([`crate::dnd`]: the mode of the drag data store for the event, disabled once the event
//! has been dispatched).

use snowlark_bindings::generated::{
    DataTransferImpl, DataTransferItemImpl, DataTransferItemListImpl, FunctionStringCallback,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle};

use super::{exception, Engine, Object};

/// An item of a drag data store: Plain Unicode string or File, with its type string.
#[derive(Clone, Debug)]
pub enum ItemData {
    Text(String),
    File(Handle),
}

#[derive(Clone, Debug)]
pub struct Item {
    pub data: ItemData,
    pub type_: String,
    /// The item's DataTransferItem object, once script asked for it.
    pub object: Option<Handle>,
}

/// A DataTransfer: its drag data store, effects, and its [SameObject] item list and FileList.
#[derive(Debug, Default)]
pub struct DataTransferData {
    pub items: Vec<Item>,
    pub drop_effect: String,
    pub effect_allowed: String,
    pub item_list: Option<Handle>,
    pub files: Option<Handle>,
    /// The drag data store mode (HTML §6.11.2), or `Disabled` once the DataTransfer is no longer
    /// associated with a drag data store.
    pub mode: StoreMode,
}

/// HTML §6.11.2 the drag data store mode, and the disabled mode of a DataTransfer whose
/// association with its store was broken (§6.11.4 "fire a DND event", the last step).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StoreMode {
    /// `dragstart`, and a DataTransfer made by script: data can be added.
    #[default]
    ReadWrite,
    /// `drop` (and a paste): the data can be read, not changed.
    ReadOnly,
    /// The other drag-and-drop events: the formats and kinds can be enumerated, the data cannot
    /// be read.
    Protected,
    /// No store: empty to script.
    Disabled,
}

impl DataTransferData {
    /// The handles it keeps alive (for the wrapper GC).
    pub fn gc_refs(&self, out: &mut Vec<Handle>) {
        for i in &self.items {
            if let ItemData::File(f) = i.data {
                out.push(f);
            }
            out.extend(i.object);
        }
        out.extend(self.item_list);
        out.extend(self.files);
    }
}

/// HTML §6.11.3: a format's normalization for getData/setData/clearData: ASCII lowercase, "text"
/// as "text/plain" and "url" as "text/uri-list". (The engines also strip ASCII whitespace around
/// it, as the drag and drop tests expect.)
fn normalize_format(format: &str) -> String {
    let f = format
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_ascii_lowercase();
    match f.as_str() {
        "text" => "text/plain".into(),
        "url" => "text/uri-list".into(),
        // (a type with parameters is its type: "text/plain;charset=utf-8")
        _ if f.starts_with("text/plain;") => "text/plain".into(),
        _ if f.starts_with("text/uri-list;") => "text/uri-list".into(),
        _ => f,
    }
}

impl Engine {
    fn dt<R>(&self, h: Handle, f: impl FnOnce(&mut DataTransferData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::DataTransfer(d) => f(d),
            _ => panic!("not a DataTransfer"),
        }
    }

    /// The DataTransfer an item list or item belongs to, and the item's place (by identity).
    fn dt_of_list(&self, h: Handle) -> Handle {
        match &*self.obj(h) {
            Object::DataTransferItemList(dt) => *dt,
            _ => panic!("not a DataTransferItemList"),
        }
    }

    fn dt_item(&self, h: Handle) -> Option<Item> {
        let dt = match &*self.obj(h) {
            Object::DataTransferItem(dt) => *dt,
            _ => panic!("not a DataTransferItem"),
        };
        self.dt(dt, |d| {
            if d.mode == StoreMode::Disabled {
                return None;
            }
            d.items.iter().find(|i| i.object == Some(h)).cloned()
        })
    }

    fn dt_mode(&self, h: Handle) -> StoreMode {
        self.dt(h, |d| d.mode)
    }

    /// The mode of an item's DataTransfer.
    fn dt_item_mode(&self, h: Handle) -> StoreMode {
        let dt = match &*self.obj(h) {
            Object::DataTransferItem(dt) => *dt,
            _ => return StoreMode::Disabled,
        };
        self.dt_mode(dt)
    }
}

impl DataTransferImpl for Engine {
    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        Ok(self.alloc(Object::DataTransfer(Box::new(DataTransferData {
            drop_effect: "none".into(),
            effect_allowed: "none".into(),
            ..Default::default()
        }))))
    }
    fn drop_effect(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.dt(this, |d| d.drop_effect.clone()))
    }
    fn set_drop_effect(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if matches!(value, "none" | "copy" | "link" | "move") {
            self.dt(this, |d| d.drop_effect = value.to_owned());
        }
        Ok(())
    }
    fn effect_allowed(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.dt(this, |d| d.effect_allowed.clone()))
    }
    fn set_effect_allowed(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        const VALUES: &[&str] = &[
            "none",
            "copy",
            "copyLink",
            "copyMove",
            "link",
            "linkMove",
            "move",
            "all",
            "uninitialized",
        ];
        // (only while the store is in read/write mode: during `dragstart`)
        if VALUES.contains(&value) && self.dt_mode(this) == StoreMode::ReadWrite {
            self.dt(this, |d| d.effect_allowed = value.to_owned());
        }
        Ok(())
    }
    fn items(&self, this: Handle) -> Handle {
        if let Some(h) = self.dt(this, |d| d.item_list) {
            return h;
        }
        let h = self.alloc(Object::DataTransferItemList(this));
        self.dt(this, |d| d.item_list = Some(h));
        h
    }
    /// The formats of the string items, in order, then "Files" if there is a file item.
    fn types(&self, this: Handle) -> Vec<String> {
        self.dt(this, |d| {
            if d.mode == StoreMode::Disabled {
                return Vec::new();
            }
            let mut out: Vec<String> = d
                .items
                .iter()
                .filter(|i| matches!(i.data, ItemData::Text(_)))
                .map(|i| i.type_.clone())
                .collect();
            if d.items.iter().any(|i| matches!(i.data, ItemData::File(_))) {
                out.push("Files".into());
            }
            out
        })
    }
    /// A FileList of the store's files ([SameObject]: the same list, brought up to date); empty
    /// without a store or in protected mode.
    fn files(&self, this: Handle) -> Handle {
        let files: Vec<Handle> = self.dt(this, |d| {
            if matches!(d.mode, StoreMode::Disabled | StoreMode::Protected) {
                return Vec::new();
            }
            d.items
                .iter()
                .filter_map(|i| match i.data {
                    ItemData::File(f) => Some(f),
                    ItemData::Text(_) => None,
                })
                .collect()
        });
        match self.dt(this, |d| d.files) {
            Some(h) => {
                *self.obj_mut(h) = Object::FileList(files);
                h
            }
            None => {
                let h = self.alloc(Object::FileList(files));
                self.dt(this, |d| d.files = Some(h));
                h
            }
        }
    }
    fn set_drag_image(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _image: Handle,
        _x: i32,
        _y: i32,
    ) -> rt::Result<()> {
        // (the drag feedback is the platform's: there is no drag image to replace)
        Ok(())
    }
    fn get_data(&self, _cx: &mut Cx, this: Handle, format: &str) -> rt::Result<DomStr<'_>> {
        if matches!(
            self.dt_mode(this),
            StoreMode::Disabled | StoreMode::Protected
        ) {
            return Ok(DomStr::Borrowed(""));
        }
        // ("url" asks for the first URL of the list; "text/uri-list" for the list as it is)
        let url = format
            .trim_matches(|c: char| c.is_ascii_whitespace())
            .eq_ignore_ascii_case("url");
        let format = normalize_format(format);
        let data = self.dt(this, |d| {
            d.items.iter().find_map(|i| match &i.data {
                ItemData::Text(s) if i.type_ == format => Some(s.clone()),
                _ => None,
            })
        });
        let data = data.unwrap_or_default();
        // ("url": the first URL of the list, comment lines skipped)
        if url {
            let first = data
                .split(['\r', '\n'])
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .unwrap_or("")
                .to_owned();
            return Ok(DomStr::Owned(first));
        }
        Ok(DomStr::Owned(data))
    }
    fn set_data(&self, _cx: &mut Cx, this: Handle, format: &str, data: &str) -> rt::Result<()> {
        let format = normalize_format(format);
        self.dt(this, |d| {
            if d.mode != StoreMode::ReadWrite {
                return;
            }
            d.items
                .retain(|i| !(matches!(i.data, ItemData::Text(_)) && i.type_ == format));
            d.items.push(Item {
                data: ItemData::Text(data.to_owned()),
                type_: format,
                object: None,
            });
        });
        Ok(())
    }
    fn clear_data(&self, _cx: &mut Cx, this: Handle, format: Option<&str>) -> rt::Result<()> {
        let format = format.map(normalize_format);
        self.dt(this, |d| {
            if d.mode != StoreMode::ReadWrite {
                return;
            }
            d.items.retain(|i| {
                !matches!(i.data, ItemData::Text(_))
                    || format.as_ref().is_some_and(|f| *f != i.type_)
            })
        });
        Ok(())
    }
}

impl DataTransferItemListImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        let dt = self.dt_of_list(this);
        self.dt(dt, |d| match d.mode {
            StoreMode::Disabled => 0,
            _ => d.items.len() as u32,
        })
    }
    fn indexed_getter(&self, this: Handle, index: u32) -> Option<Handle> {
        let dt = self.dt_of_list(this);
        if self.dt_mode(dt) == StoreMode::Disabled {
            return None;
        }
        let existing = self.dt(dt, |d| d.items.get(index as usize).map(|i| i.object))?;
        if let Some(h) = existing {
            return Some(h);
        }
        let h = self.alloc(Object::DataTransferItem(dt));
        self.dt(dt, |d| d.items[index as usize].object = Some(h));
        Some(h)
    }
    /// `add(data, type)`: a string item; NotSupportedError when one of that type is there already.
    fn add_data_type(
        &self,
        _cx: &mut Cx,
        this: Handle,
        data: &str,
        type_: &str,
    ) -> rt::Result<Option<Handle>> {
        let dt = self.dt_of_list(this);
        if self.dt_mode(dt) != StoreMode::ReadWrite {
            return Ok(None);
        }
        let type_ = type_.to_ascii_lowercase();
        let taken = self.dt(dt, |d| {
            d.items
                .iter()
                .any(|i| matches!(i.data, ItemData::Text(_)) && i.type_ == type_)
        });
        if taken {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "The store already has a string item of that type",
            ));
        }
        let index = self.dt(dt, |d| {
            d.items.push(Item {
                data: ItemData::Text(data.to_owned()),
                type_,
                object: None,
            });
            d.items.len() as u32 - 1
        });
        Ok(self.indexed_getter(this, index))
    }
    /// `add(file)`: a File item, its type the File's, lowercased.
    fn add_data(&self, _cx: &mut Cx, this: Handle, data: Handle) -> rt::Result<Option<Handle>> {
        let dt = self.dt_of_list(this);
        if self.dt_mode(dt) != StoreMode::ReadWrite {
            return Ok(None);
        }
        let type_ = self.blob_contents(data).1.to_ascii_lowercase();
        let index = self.dt(dt, |d| {
            d.items.push(Item {
                data: ItemData::File(data),
                type_,
                object: None,
            });
            d.items.len() as u32 - 1
        });
        Ok(self.indexed_getter(this, index))
    }
    fn remove(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<()> {
        let dt = self.dt_of_list(this);
        if self.dt_mode(dt) != StoreMode::ReadWrite {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The drag data store is not in read/write mode",
            ));
        }
        self.dt(dt, |d| {
            if (index as usize) < d.items.len() {
                d.items.remove(index as usize);
            }
        });
        Ok(())
    }
    fn clear(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let dt = self.dt_of_list(this);
        self.dt(dt, |d| {
            if d.mode == StoreMode::ReadWrite {
                d.items.clear()
            }
        });
        Ok(())
    }
}

impl DataTransferItemImpl for Engine {
    /// "string" or "file"; the empty string once the item left its store.
    fn kind(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(match self.dt_item(this).map(|i| i.data) {
            Some(ItemData::Text(_)) => "string",
            Some(ItemData::File(_)) => "file",
            None => "",
        })
    }
    fn type_(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.dt_item(this).map(|i| i.type_).unwrap_or_default())
    }
    /// A string item's data, given to the callback in a task (only in read/write and read-only
    /// mode).
    fn get_as_string(
        &self,
        _cx: &mut Cx,
        this: Handle,
        callback: Option<FunctionStringCallback>,
    ) -> rt::Result<()> {
        let (
            Some(cb),
            Some(Item {
                data: ItemData::Text(s),
                ..
            }),
        ) = (callback, self.dt_item(this))
        else {
            return Ok(());
        };
        if self.dt_item_mode(this) == StoreMode::Protected {
            return Ok(());
        }
        self.queue_task(
            crate::event_loop::TaskSource::DomManipulation,
            move |_, cx| {
                let _ = cb.invoke(cx, None, &s);
            },
        );
        Ok(())
    }
    fn get_as_file(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        if self.dt_item_mode(this) == StoreMode::Protected {
            return Ok(None);
        }
        Ok(match self.dt_item(this).map(|i| i.data) {
            Some(ItemData::File(f)) => Some(f),
            _ => None,
        })
    }
}
