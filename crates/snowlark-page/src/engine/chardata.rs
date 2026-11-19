// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `CharacterData`, `Text`, `CDATASection`, `Comment`, `ProcessingInstruction`, `DocumentType` and
//! `DocumentFragment`.

use snowlark_bindings::generated::{
    CDATASectionImpl, CharacterDataImpl, CommentImpl, DocumentFragmentImpl, DocumentTypeImpl,
    ProcessingInstructionImpl, TextImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};

use super::{node_handle, DomResultExt, Engine};

impl CharacterDataImpl for Engine {
    fn data(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        self.dom_str(|d| d.character_data(id).unwrap_or(""))
    }

    fn set_data(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.dom_mut().set_data(self.n(this), value);
        self.after_mutation(cx);
        Ok(())
    }

    fn length(&self, this: Handle) -> u32 {
        self.dom().length(self.n(this)) as u32
    }

    fn substring_data(
        &self,
        _cx: &mut Cx,
        this: Handle,
        offset: u32,
        count: u32,
    ) -> rt::Result<DomStr<'_>> {
        self.dom()
            .substring_data(self.n(this), offset as usize, count as usize)
            .js()
            .map(DomStr::Owned)
    }

    fn append_data(&self, cx: &mut Cx, this: Handle, data: &str) -> rt::Result<()> {
        self.dom_mut().append_data(self.n(this), data);
        self.after_mutation(cx);
        Ok(())
    }

    fn insert_data(&self, cx: &mut Cx, this: Handle, offset: u32, data: &str) -> rt::Result<()> {
        let r = self
            .dom_mut()
            .insert_data(self.n(this), offset as usize, data)
            .js();
        self.after_mutation(cx);
        r
    }

    fn delete_data(&self, cx: &mut Cx, this: Handle, offset: u32, count: u32) -> rt::Result<()> {
        let r = self
            .dom_mut()
            .delete_data(self.n(this), offset as usize, count as usize)
            .js();
        self.after_mutation(cx);
        r
    }

    fn replace_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        offset: u32,
        count: u32,
        data: &str,
    ) -> rt::Result<()> {
        let r = self
            .dom_mut()
            .replace_data(self.n(this), offset as usize, count as usize, data)
            .js();
        self.after_mutation(cx);
        r
    }
}

impl TextImpl for Engine {
    fn whole_text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.dom().whole_text(self.n(this)))
    }

    fn split_text(&self, cx: &mut Cx, this: Handle, offset: u32) -> rt::Result<Handle> {
        let r = self
            .dom_mut()
            .split_text(self.n(this), offset as usize)
            .js();
        self.after_mutation(cx);
        r.map(node_handle)
    }

    fn constructor(&self, cx: &mut Cx, data: &str) -> rt::Result<Handle> {
        let doc = self.current_document(cx)?;
        Ok(node_handle(self.dom_mut().create_text_node(doc, data)))
    }
}

impl CDATASectionImpl for Engine {}

impl CommentImpl for Engine {
    fn constructor(&self, cx: &mut Cx, data: &str) -> rt::Result<Handle> {
        let doc = self.current_document(cx)?;
        Ok(node_handle(self.dom_mut().create_comment(doc, data)))
    }
}

/// Processing instructions have no attributes in the DOM Standard; the attribute members of the
/// "processing instruction attributes" proposal see none and change nothing.
impl ProcessingInstructionImpl for Engine {
    fn target(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        self.dom_str(|d| d.pi_target(id).unwrap_or(""))
    }
    fn has_attributes(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(false)
    }
    fn get_attribute_names(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn get_attribute(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _name: &str,
    ) -> rt::Result<Option<DomStr<'_>>> {
        Ok(None)
    }
    fn set_attribute(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _name: &str,
        _value: &str,
    ) -> rt::Result<()> {
        Ok(())
    }
    fn remove_attribute(&self, _cx: &mut Cx, _this: Handle, _name: &str) -> rt::Result<()> {
        Ok(())
    }
    fn toggle_attribute(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _name: &str,
        _force: Option<bool>,
    ) -> rt::Result<bool> {
        Ok(false)
    }
    fn has_attribute(&self, _cx: &mut Cx, _this: Handle, _name: &str) -> rt::Result<bool> {
        Ok(false)
    }
    fn constructor(&self, cx: &mut Cx, target: &str, data: &str) -> rt::Result<Handle> {
        let doc = self.current_document(cx)?;
        self.dom_mut()
            .create_processing_instruction(doc, target, data)
            .js()
            .map(node_handle)
    }
}

impl DocumentTypeImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        self.dom_str(|d| d.doctype(id).map_or("", |t| t.0))
    }
    fn public_id(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        self.dom_str(|d| d.doctype(id).map_or("", |t| t.1))
    }
    fn system_id(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        self.dom_str(|d| d.doctype(id).map_or("", |t| t.2))
    }
}

impl DocumentFragmentImpl for Engine {
    fn constructor(&self, cx: &mut Cx) -> rt::Result<Handle> {
        let doc = self.current_document(cx)?;
        Ok(node_handle(self.dom_mut().create_document_fragment(doc)))
    }
}
