// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drags from other programs over the window (the window's OLE drop target): over the page they
//! go to the active tab as [`TabCommand::Drag`], whose answer ([`TabEvent::DragOperation`]) is the
//! operation the window tells the drag source. The files are read here, in the browser, when they
//! are dropped, and handed to the renderer with the drop: a page reads nothing from the disk.

use std::path::PathBuf;

use snowlark_page::dnd::{DragOperation, DroppedFile, ExternalDrag};
use snowlark_paint::drop_target::{DragData, DropEffect};
use snowlark_paint::window::{Modifiers, Window};
use snowlark_paint::Point;

use super::Browser;
use crate::driver::DragInput;
use crate::tab::TabCommand;

/// The most a dropped file may hold to be handed to the page (bytes): the contents cross to the
/// renderer in one message.
const MAX_FILE: u64 = 512 * 1024 * 1024;

/// A drag from another program over the window.
pub(super) struct IncomingDrag {
    paths: Vec<PathBuf>,
    /// What the page is told on entering (the files without their contents).
    data: ExternalDrag,
    /// The drag is over the page (the tab has it).
    in_page: bool,
}

/// `effectAllowed` for the operations the source allows.
fn effect_allowed(d: &DragData) -> &'static str {
    match (d.copy, d.moves, d.link) {
        (true, true, true) => "all",
        (true, true, false) => "copyMove",
        (true, false, true) => "copyLink",
        (false, true, true) => "linkMove",
        (true, false, false) => "copy",
        (false, true, false) => "move",
        (false, false, true) => "link",
        (false, false, false) => "none",
    }
}

/// A dropped file's name and type (its extension's; empty when unknown), contents to come.
fn file_entry(path: &std::path::Path) -> DroppedFile {
    let type_ = snowlark_net::file_mime_type(path);
    DroppedFile {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        type_: if type_ == "application/octet-stream" {
            String::new()
        } else {
            type_.to_owned()
        },
        bytes: Vec::new(),
        modified: 0,
    }
}

/// The files that are files (not folders), with their contents and times.
fn read_files(paths: &[PathBuf]) -> Vec<DroppedFile> {
    paths
        .iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(p).ok()?;
            if !meta.is_file() || meta.len() > MAX_FILE {
                return None;
            }
            let mut f = file_entry(p);
            f.bytes = std::fs::read(p).ok()?;
            f.modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            Some(f)
        })
        .collect()
}

impl Browser {
    /// The page's viewport point (CSS px) of a window point, when the page is there.
    fn drag_page_point(&self, dip: Point) -> Option<Point> {
        let r = self.page_rect();
        let y = dip.y - self.chrome_h();
        let inside = dip.x >= r.x && dip.x < r.x + r.width && y >= 0.0 && y < r.height;
        (inside && !self.in_scrollbar(dip) && !self.active_crashed()).then(|| self.page_point(dip))
    }

    pub(super) fn drag_enter(
        &mut self,
        window: &Window,
        dip: Point,
        data: DragData,
        mods: Modifiers,
    ) {
        let files: Vec<PathBuf> = data.files.iter().filter(|p| p.is_file()).cloned().collect();
        let drag = ExternalDrag {
            files: files.iter().map(|p| file_entry(p)).collect(),
            text: data.text.clone(),
            allowed: effect_allowed(&data).to_owned(),
            ..Default::default()
        };
        self.drag_in = Some(IncomingDrag {
            paths: files,
            data: drag,
            in_page: false,
        });
        window.set_drop_effect(DropEffect::None);
        self.drag_over(window, dip, mods);
    }

    pub(super) fn drag_over(&mut self, window: &Window, dip: Point, mods: Modifiers) {
        let at = self.drag_page_point(dip);
        let Some(d) = self.drag_in.as_mut() else {
            return;
        };
        match (at, d.in_page) {
            (Some(at), false) => {
                d.in_page = true;
                let data = Box::new(d.data.clone());
                self.send_active(TabCommand::Drag(DragInput::Enter { at, data, mods }));
            }
            (Some(at), true) => self.send_active(TabCommand::Drag(DragInput::Over { at, mods })),
            (None, true) => {
                d.in_page = false;
                self.send_active(TabCommand::Drag(DragInput::Leave));
                window.set_drop_effect(DropEffect::None);
            }
            (None, false) => {}
        }
    }

    pub(super) fn drag_leave(&mut self, window: &Window) {
        if self.drag_in.take().is_some_and(|d| d.in_page) {
            self.send_active(TabCommand::Drag(DragInput::Leave));
        }
        window.set_drop_effect(DropEffect::None);
    }

    pub(super) fn drag_drop(&mut self, window: &Window, dip: Point, mods: Modifiers) {
        let Some(d) = self.drag_in.take() else {
            return;
        };
        match self.drag_page_point(dip).filter(|_| d.in_page) {
            Some(at) => {
                let data = Box::new(ExternalDrag {
                    files: read_files(&d.paths),
                    ..d.data
                });
                self.send_active(TabCommand::Drag(DragInput::Drop { at, data, mods }));
            }
            None if d.in_page => self.send_active(TabCommand::Drag(DragInput::Leave)),
            None => {}
        }
        window.set_drop_effect(DropEffect::None);
    }

    /// The active tab's answer to the drag: what the window tells the source while the drag is
    /// over the page.
    pub(super) fn drag_operation(&mut self, window: &Window, op: DragOperation) {
        if !self.drag_in.as_ref().is_some_and(|d| d.in_page) {
            return;
        }
        window.set_drop_effect(match op {
            DragOperation::None => DropEffect::None,
            DragOperation::Copy => DropEffect::Copy,
            DragOperation::Move => DropEffect::Move,
            DragOperation::Link => DropEffect::Link,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sources_operations_are_an_effect_allowed() {
        let d = |copy, moves, link| DragData {
            copy,
            moves,
            link,
            ..Default::default()
        };
        assert_eq!(effect_allowed(&d(true, true, true)), "all");
        assert_eq!(effect_allowed(&d(true, false, true)), "copyLink");
        assert_eq!(effect_allowed(&d(false, false, false)), "none");
    }

    #[test]
    fn a_dropped_file_has_its_extensions_type() {
        let f = file_entry(std::path::Path::new("photo.PNG"));
        assert_eq!(
            (f.name.as_str(), f.type_.as_str()),
            ("photo.PNG", "image/png")
        );
        assert_eq!(file_entry(std::path::Path::new("x.unknownext")).type_, "");
    }
}
