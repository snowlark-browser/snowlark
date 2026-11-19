// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Embedded content (HTML §4.8) and the proposal elements: images, canvas, dialogs, frames. Media
//! elements (`video`, `audio`, `track`) are [`crate::media`]'s.

use snowlark_bindings::generated::{
    HTMLCameraElementImpl, HTMLEmbedElementImpl, HTMLFencedFrameElementImpl, HTMLFrameElementImpl,
    HTMLIFrameElementImpl, HTMLImageElementImpl, HTMLInstallElementImpl,
    HTMLMediaStreamConstraints, HTMLMicrophoneElementImpl, HTMLModelElementImpl,
    HTMLObjectElementImpl, HTMLPortalElementImpl, HTMLUserMediaElementImpl,
    MediaTrackConstraintSet, PortalActivateOptions, StructuredSerializeOptions,
    TrustedHTMLOrString,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsPromise, JsValue};
use snowlark_dom::{local_name, ns, LocalName, NodeId};

use super::element::rejected;
use super::{exception, node_handle, Engine, Object};

impl Engine {
    pub(crate) fn dim_attr(&self, el: NodeId, name: &str, default: u32) -> u32 {
        self.dom()
            .attr(el, &ns!(), &LocalName::from(name))
            .and_then(rt::parse_non_negative_integer)
            .filter(|v| *v <= i32::MAX as i64)
            .map_or(default, |v| v as u32)
    }

    pub(crate) fn set_dim_attr(&self, cx: &mut Cx, el: NodeId, name: &str, v: u32, default: u32) {
        let v = if v > i32::MAX as u32 { default } else { v };
        self.set_attr(cx, el, name, &v.to_string());
    }
}

impl HTMLImageElementImpl for Engine {
    /// HTML §4.8.4.4 `width`: the rendered width if the image is being rendered, else its
    /// density-corrected natural width if available, else 0.
    fn width(&self, this: Handle) -> u32 {
        self.img_dimension(self.n(this), true)
    }
    fn height(&self, this: Handle) -> u32 {
        self.img_dimension(self.n(this), false)
    }
    fn natural_width(&self, this: Handle) -> u32 {
        self.img_natural_size(self.n(this))
            .map_or(0, |(w, _)| w.round() as u32)
    }
    fn natural_height(&self, this: Handle) -> u32 {
        self.img_natural_size(self.n(this))
            .map_or(0, |(_, h)| h.round() as u32)
    }
    fn complete(&self, this: Handle) -> bool {
        self.img_complete(self.n(this))
    }
    fn current_src(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.img_current_src(self.n(this)))
    }
    fn x(&self, this: Handle) -> i32 {
        self.layout_box(self.n(this)).x as i32
    }
    fn y(&self, this: Handle) -> i32 {
        self.layout_box(self.n(this)).y as i32
    }
    fn decode(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.img_decode(cx, self.n(this))
    }
    fn factory_image(
        &self,
        cx: &mut Cx,
        width: Option<u32>,
        height: Option<u32>,
    ) -> rt::Result<Handle> {
        let doc = self.current_document(cx)?;
        let img = self.dom_mut().create_html_element(doc, local_name!("img"));
        if let Some(w) = width {
            self.set_attr(cx, img, "width", &w.to_string());
        }
        if let Some(h) = height {
            self.set_attr(cx, img, "height", &h.to_string());
        }
        Ok(node_handle(img))
    }
}

impl Engine {
    /// An `img` element's `width` (or `height`) IDL attribute's value.
    fn img_dimension(&self, el: NodeId, width: bool) -> u32 {
        let rendered = self
            .with_layout(el, |r, _| {
                r.geometry(el).map(|g| {
                    if width {
                        g.content_box.width
                    } else {
                        g.content_box.height
                    }
                })
            })
            .flatten();
        if let Some(v) = rendered {
            return v.max(0.0).round() as u32;
        }
        // (not rendered: the dimension attribute, parsed as a non-negative integer)
        let attr = self
            .dom()
            .attr(
                el,
                &ns!(),
                &LocalName::from(if width { "width" } else { "height" }),
            )
            .and_then(rt::parse_non_negative_integer)
            .filter(|v| *v <= u32::MAX as i64);
        if let Some(v) = attr {
            return v as u32;
        }
        self.img_natural_size(el)
            .map_or(0, |(w, h)| (if width { w } else { h }).round() as u32)
    }
}

impl HTMLEmbedElementImpl for Engine {
    fn get_svg_document(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Option<Handle>> {
        Ok(None)
    }
}

impl HTMLFrameElementImpl for Engine {
    fn content_document(&self, this: Handle) -> Option<Handle> {
        Engine::content_document(self, self.n(this)).map(node_handle)
    }
    fn content_window(&self, this: Handle) -> Option<Handle> {
        Engine::content_window(self, self.n(this))
    }
}

impl HTMLIFrameElementImpl for Engine {
    fn srcdoc(&self, this: Handle) -> TrustedHTMLOrString {
        TrustedHTMLOrString::String(
            self.dom()
                .attr(self.n(this), &ns!(), &LocalName::from("srcdoc"))
                .unwrap_or("")
                .to_owned(),
        )
    }
    fn set_srcdoc(&self, cx: &mut Cx, this: Handle, value: TrustedHTMLOrString) -> rt::Result<()> {
        let v = self.tt_html(cx, value, self.n(this), "HTMLIFrameElement srcdoc")?;
        self.set_attr(cx, self.n(this), "srcdoc", &v);
        Ok(())
    }
    fn content_document(&self, this: Handle) -> Option<Handle> {
        Engine::content_document(self, self.n(this)).map(node_handle)
    }
    fn content_window(&self, this: Handle) -> Option<Handle> {
        Engine::content_window(self, self.n(this))
    }
    fn credentialless(&self, this: Handle) -> bool {
        self.attr_str(self.n(this), "credentialless").is_some()
    }
    fn set_credentialless(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        let el = self.n(this);
        if value {
            self.set_attr(cx, el, "credentialless", "");
        } else {
            self.remove_attr(cx, el, "credentialless");
        }
        Ok(())
    }
    fn connection_allowlist(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "connectionallowlist")
            .unwrap_or(DomStr::Borrowed(""))
    }
    fn set_connection_allowlist(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "connectionallowlist", value);
        Ok(())
    }
    fn csp(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "csp")
            .unwrap_or(DomStr::Borrowed(""))
    }
    fn set_csp(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "csp", value);
        Ok(())
    }
    fn private_token(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "privatetoken")
            .unwrap_or(DomStr::Borrowed(""))
    }
    fn set_private_token(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "privatetoken", value);
        Ok(())
    }
    fn get_svg_document(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Option<Handle>> {
        Ok(None)
    }
}

impl HTMLObjectElementImpl for Engine {
    fn form(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.form_owner_of(self.n(this)))
    }
    fn content_document(&self, this: Handle) -> Option<Handle> {
        Engine::content_document(self, self.n(this)).map(node_handle)
    }
    fn content_window(&self, this: Handle) -> Option<Handle> {
        Engine::content_window(self, self.n(this))
    }
    fn will_validate(&self, _this: Handle) -> bool {
        false
    }
    fn validity(&self, this: Handle) -> Handle {
        self.alloc(Object::ValidityState {
            element: self.n(this),
        })
    }
    // (an `object` is always barred from constraint validation: no message, always valid)
    fn validation_message(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn get_svg_document(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Option<Handle>> {
        Ok(None)
    }
    fn check_validity(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(true)
    }
    fn report_validity(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(true)
    }
    /// HTML "the constraint validation API": `setCustomValidity()` sets the element's custom
    /// validity error message, which `validity.customError` reports even for an element barred
    /// from constraint validation (so no `:invalid` state changes).
    fn set_custom_validity(&self, _cx: &mut Cx, this: Handle, error: &str) -> rt::Result<()> {
        self.ctl(self.n(this), |c| c.custom_validity = error.to_owned());
        Ok(())
    }
}

impl HTMLCameraElementImpl for Engine {
    fn error(&self, _this: Handle) -> Option<Handle> {
        None
    }
    fn set_constraints(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _c: MediaTrackConstraintSet,
    ) -> rt::Result<()> {
        Ok(())
    }
}

impl HTMLMicrophoneElementImpl for Engine {
    fn error(&self, _this: Handle) -> Option<Handle> {
        None
    }
    fn set_constraints(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _c: MediaTrackConstraintSet,
    ) -> rt::Result<()> {
        Ok(())
    }
}

impl HTMLUserMediaElementImpl for Engine {
    fn error(&self, _this: Handle) -> Option<Handle> {
        None
    }
    fn set_constraints(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _c: HTMLMediaStreamConstraints,
    ) -> rt::Result<()> {
        Ok(())
    }
}

impl HTMLFencedFrameElementImpl for Engine {
    fn width(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "width")
            .unwrap_or(DomStr::Borrowed(""))
    }
    fn set_width(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "width", value);
        Ok(())
    }
    fn height(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "height")
            .unwrap_or(DomStr::Borrowed(""))
    }
    fn set_height(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "height", value);
        Ok(())
    }
    fn sandbox(&self, this: Handle) -> Handle {
        self.alloc(Object::TokenList {
            element: self.n(this),
            attr: LocalName::from("sandbox"),
        })
    }
    fn allow(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "allow")
            .unwrap_or(DomStr::Borrowed(""))
    }
    fn set_allow(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "allow", value);
        Ok(())
    }
}

impl HTMLInstallElementImpl for Engine {
    fn is_valid(&self, _this: Handle) -> bool {
        false
    }
    fn invalid_reason(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
}

impl HTMLModelElementImpl for Engine {
    fn ready(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        rejected(
            cx,
            ExceptionName::NotSupportedError,
            "Models are not supported",
        )
    }
    fn environment_map(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "environmentmap")
            .unwrap_or(DomStr::Borrowed(""))
    }
    fn set_environment_map(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "environmentmap", value);
        Ok(())
    }
    fn environment_map_ready(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        rejected(
            cx,
            ExceptionName::NotSupportedError,
            "Models are not supported",
        )
    }
    /// With no model loaded: the origin, an empty extent and the identity transform.
    fn bounding_box_center(&self, _this: Handle) -> Handle {
        self.new_point(0.0, 0.0, 0.0, 1.0, true)
    }
    fn bounding_box_extents(&self, _this: Handle) -> Handle {
        self.new_point(0.0, 0.0, 0.0, 1.0, true)
    }
    fn entity_transform(&self, _this: Handle) -> Handle {
        self.new_matrix(super::matrix::Matrix::identity(), true)
    }
    fn set_entity_transform(&self, _cx: &mut Cx, _this: Handle, _value: Handle) -> rt::Result<()> {
        Ok(())
    }
}

impl HTMLPortalElementImpl for Engine {
    fn src(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "src")
            .unwrap_or(DomStr::Borrowed(""))
    }
    fn set_src(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "src", value);
        Ok(())
    }
    fn referrer_policy(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "referrerpolicy")
            .unwrap_or(DomStr::Borrowed(""))
    }
    fn set_referrer_policy(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "referrerpolicy", value);
        Ok(())
    }
    fn activate(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _options: PortalActivateOptions,
    ) -> rt::Result<JsPromise> {
        rejected(
            cx,
            ExceptionName::InvalidStateError,
            "Portals are not supported",
        )
    }
    fn post_message(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _message: JsValue,
        _options: StructuredSerializeOptions,
    ) -> rt::Result<()> {
        Err(exception(
            ExceptionName::InvalidStateError,
            "Portals are not supported",
        ))
    }
}
