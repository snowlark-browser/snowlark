// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Notifications API (notifications.spec.whatwg.org) without a notification platform: the
//! permission is "denied" (as `navigator.permissions` reports for "notifications"), so a
//! `Notification` is made, validated and kept, and fires `error` instead of being shown.

use snowlark_bindings::generated::{
    NotificationAction, NotificationDirection, NotificationImpl, NotificationOptions,
    NotificationPermission, NotificationPermissionCallback,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, Handle, JsPromise, JsValue};

use super::Engine;
use super::Object;
use crate::event_loop::TaskSource;

/// A notification (§2): what the constructor recorded.
pub struct NotificationData {
    pub title: String,
    pub dir: NotificationDirection,
    pub lang: String,
    pub body: String,
    pub navigate: String,
    pub tag: String,
    pub image: String,
    pub icon: String,
    pub badge: String,
    pub timestamp: u64,
    pub renotify: bool,
    pub silent: Option<bool>,
    pub require_interaction: bool,
    /// The deserialized data (`[SameObject] data`).
    pub data: JsValue,
    pub actions: Vec<NotificationAction>,
}

/// A permission's IDL string.
fn permission_name(p: NotificationPermission) -> &'static str {
    match p {
        NotificationPermission::Default => "default",
        NotificationPermission::Denied => "denied",
        NotificationPermission::Granted => "granted",
    }
}

/// The maximum number of actions a notification shows (§2: user agent defined); Chrome's.
const MAX_ACTIONS: u32 = 2;

impl Engine {
    /// The notification platform's permission state for this environment: nothing can be shown.
    fn notification_permission(&self) -> NotificationPermission {
        NotificationPermission::Denied
    }

    fn notification<R>(&self, h: Handle, f: impl FnOnce(&NotificationData) -> R) -> Option<R> {
        match &*self.obj(h) {
            Object::Notification(n) => Some(f(n)),
            _ => None,
        }
    }

    fn notification_str(&self, h: Handle, f: impl FnOnce(&NotificationData) -> &str) -> DomStr<'_> {
        DomStr::Owned(
            self.notification(h, |n| f(n).to_owned())
                .unwrap_or_default(),
        )
    }

    /// §2.5 "create a notification with a settings object".
    fn create_notification(
        &self,
        cx: &mut Cx,
        title: &str,
        options: NotificationOptions,
    ) -> rt::Result<Handle> {
        // (step: "If options["renotify"] is true and options["tag"] is the empty string, throw")
        if options.renotify && options.tag.is_empty() {
            return Err(Error::Type(
                "A notification that renotifies needs a tag".into(),
            ));
        }
        let data = match &options.data {
            Some(v) => self.structured_clone_value(cx, v)?,
            None => {
                let scope = cx.scope();
                let null: v8::Local<v8::Value> = v8::null(scope).into();
                JsValue::new(scope, null)
            }
        };
        // URLs parse against the API base URL; one that does not parse is left out.
        let base = self.current_base_url(cx)?;
        let parse = |u: &Option<String>| {
            u.as_deref()
                .and_then(|u| base.join(u).ok())
                .map(String::from)
                .unwrap_or_default()
        };
        let timestamp = options.timestamp.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        });
        let actions = options
            .actions
            .into_iter()
            .take(MAX_ACTIONS as usize)
            .map(|a| NotificationAction {
                navigate: a
                    .navigate
                    .as_ref()
                    .and_then(|u| base.join(u).ok())
                    .map(String::from),
                icon: a
                    .icon
                    .as_ref()
                    .and_then(|u| base.join(u).ok())
                    .map(String::from),
                ..a
            })
            .collect();
        Ok(self.alloc(Object::Notification(Box::new(NotificationData {
            title: title.to_owned(),
            dir: options.dir,
            lang: options.lang,
            body: options.body,
            navigate: parse(&options.navigate),
            tag: options.tag,
            image: parse(&options.image),
            icon: parse(&options.icon),
            badge: parse(&options.badge),
            timestamp,
            renotify: options.renotify,
            silent: options.silent,
            require_interaction: options.require_interaction,
            data,
            actions,
        }))))
    }
}

impl NotificationImpl for Engine {
    /// §2.6 `new Notification(title, options)`.
    fn constructor(
        &self,
        cx: &mut Cx,
        title: &str,
        options: NotificationOptions,
    ) -> rt::Result<Handle> {
        if !options.actions.is_empty() {
            return Err(Error::Type(
                "Actions are only supported for persistent notifications".into(),
            ));
        }
        let h = self.create_notification(cx, title, options)?;
        // "the notification show steps": without permission, an `error` event (in a task).
        if self.notification_permission() != NotificationPermission::Granted {
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                e.fire_event(cx, h, "error", false, false);
            });
        }
        Ok(h)
    }
    fn static_permission(&self) -> NotificationPermission {
        self.notification_permission()
    }
    /// §3 `requestPermission()`: no prompt, the state as it is; the deprecated callback is called
    /// with it before the promise resolves (in a task).
    fn static_request_permission(
        &self,
        cx: &mut Cx,
        deprecated_callback: Option<NotificationPermissionCallback>,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let state = self.notification_permission();
        let window = self.current_global(cx);
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if let Some(callback) = deprecated_callback {
                let arg = {
                    let scope = cx.scope();
                    let s = v8::String::new(scope, permission_name(state)).map(|s| s.into());
                    s.map(|s: v8::Local<v8::Value>| v8::Global::new(scope, s))
                };
                if let Some(arg) = arg {
                    e.call_reporting(cx, &callback.0, window, &[arg]);
                }
            }
            resolve.resolve(cx, state);
        });
        Ok(p)
    }
    fn static_max_actions(&self) -> u32 {
        MAX_ACTIONS
    }
    fn title(&self, this: Handle) -> DomStr<'_> {
        self.notification_str(this, |n| &n.title)
    }
    fn dir(&self, this: Handle) -> NotificationDirection {
        self.notification(this, |n| n.dir)
            .unwrap_or(NotificationDirection::Auto)
    }
    fn lang(&self, this: Handle) -> DomStr<'_> {
        self.notification_str(this, |n| &n.lang)
    }
    fn body(&self, this: Handle) -> DomStr<'_> {
        self.notification_str(this, |n| &n.body)
    }
    fn navigate(&self, this: Handle) -> DomStr<'_> {
        self.notification_str(this, |n| &n.navigate)
    }
    fn tag(&self, this: Handle) -> DomStr<'_> {
        self.notification_str(this, |n| &n.tag)
    }
    fn image(&self, this: Handle) -> DomStr<'_> {
        self.notification_str(this, |n| &n.image)
    }
    fn icon(&self, this: Handle) -> DomStr<'_> {
        self.notification_str(this, |n| &n.icon)
    }
    fn badge(&self, this: Handle) -> DomStr<'_> {
        self.notification_str(this, |n| &n.badge)
    }
    fn timestamp(&self, this: Handle) -> u64 {
        self.notification(this, |n| n.timestamp).unwrap_or(0)
    }
    fn renotify(&self, this: Handle) -> bool {
        self.notification(this, |n| n.renotify).unwrap_or(false)
    }
    fn silent(&self, this: Handle) -> Option<bool> {
        self.notification(this, |n| n.silent).flatten()
    }
    fn require_interaction(&self, this: Handle) -> bool {
        self.notification(this, |n| n.require_interaction)
            .unwrap_or(false)
    }
    fn data(&self, _cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        self.notification(this, |n| n.data.clone())
            .ok_or(Error::Pending)
    }
    fn actions(&self, this: Handle) -> Vec<NotificationAction> {
        self.notification(this, |n| n.actions.clone())
            .unwrap_or_default()
    }
    /// §2.8 "close steps": a notification never shown is in no list of notifications: nothing.
    fn close(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
}
