// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The forgeries of a compromised renderer (process isolation M5 step 5, the evil-renderer suite;
//! `docs/process-isolation.md` §7): each role is one message a renderer locked to a site must not
//! send, and the browser must refuse it and serve nothing of it; all but three ([`is_ended`]) end
//! the process too. Test renderers play the roles (`renderer_process::tests`, `snowlark-shell`'s
//! tab tests); no renderer of the browser's calls this.

use std::sync::Mutex;

use snowlark_ipc::{frame, Bulk, Error};
use snowlark_net::wire::OriginTable;
use snowlark_net::{Client, Destination, Request, Url};

use crate::driver::{Disposition, InternalRequest};
use crate::renderer_process::TabUp;
use crate::services::{self, broadcast, code_cache, idb, net, storage, RendererPort, Up};
use crate::tab::{TabEvent, TabId, TabState};

/// Every role: what each sends is in [`forge`].
pub const ROLES: &[&str] = &[
    "fetch-as-another-site",
    "cookies-of-another-site",
    "storage-of-another-site",
    "fetch-of-a-file",
    "fetch-of-the-browsers-page",
    "settings",
    "opening-the-browsers-page",
    "a-document-of-another-site",
    "a-database-of-another-site",
    "a-channel-of-another-site",
    "the-code-cache-of-another-site",
    "a-blob-of-another-site",
    "an-oversized-frame",
    "garbage",
];

/// The roles that are whole messages, which the browser refuses by their content (`broke a rule`);
/// the rest are not even frames.
pub fn is_message(role: &str) -> bool {
    !matches!(role, "an-oversized-frame" | "garbage")
}

/// The roles that end the renderer. The three that do not are what an honest renderer sends too,
/// for a page that asks (a `file:` or `snowlark:` subresource, `window.open` of a settings address):
/// the fetch gets a network error, the open is dropped, and the renderer lives.
pub fn is_ended(role: &str) -> bool {
    !matches!(
        role,
        "fetch-of-a-file" | "fetch-of-the-browsers-page" | "opening-the-browsers-page"
    )
}

/// An id no honest renderer has reached.
const ID: u64 = 1 << 40;
/// The other site.
const OTHER: &str = "https://b.test";

/// Sends the forgery of `role` as tab `tab`'s renderer, whose documents are pages of `own` (of
/// the site it is locked to).
pub fn forge(role: &str, port: &RendererPort, own: &Url, tab: TabId) -> Result<(), Error> {
    let fetch = |url: &str| -> Result<Up, Error> {
        let request = Request::subresource(
            Url::parse(url).unwrap(),
            Destination::Image,
            std::sync::Arc::new(Client::for_url(own)),
        );
        let origins = Mutex::new(OriginTable::renderer());
        let request = net::pack_with(port, &origins, &request)?;
        Ok(Up::Net(net::Up::Fetch { id: ID, request }))
    };
    let event = |event: TabEvent| -> Result<Up, Error> {
        Ok(Up::Tab(TabUp::Event {
            tab,
            event: services::pack(port, &event)?,
        }))
    };
    let message = match role {
        "fetch-as-another-site" => {
            let request = Request::subresource(
                Url::parse("https://cdn.test/x").unwrap(),
                Destination::Image,
                std::sync::Arc::new(Client::for_url(&Url::parse(OTHER).unwrap())),
            );
            let origins = Mutex::new(OriginTable::renderer());
            let request = net::pack_with(port, &origins, &request)?;
            Up::Net(net::Up::Fetch { id: ID, request })
        }
        "cookies-of-another-site" => Up::Net(net::Up::DocumentCookie(
            snowlark_net::cookies::CookieContext {
                url: Url::parse(OTHER).unwrap(),
                http_api: false,
                same_site: true,
                top_level_navigation: false,
                safe_method: true,
            },
        )),
        "storage-of-another-site" => Up::Storage(storage::Up::Open {
            client: ID,
            origin: OTHER.into(),
        }),
        "fetch-of-a-file" => fetch("file:///C:/Windows/win.ini")?,
        "fetch-of-the-browsers-page" => fetch("snowlark:settings")?,
        "settings" => event(TabEvent::Internal(InternalRequest::Settings("a=b".into())))?,
        "opening-the-browsers-page" => event(TabEvent::Open(
            Url::parse("snowlark:settings").unwrap(),
            Disposition::NewForegroundTab,
        ))?,
        "a-document-of-another-site" => event(TabEvent::State(TabState {
            url: format!("{OTHER}/"),
            ..Default::default()
        }))?,
        "a-database-of-another-site" => Up::Idb(idb::Up(services::pack(
            port,
            &idb::Call::Open {
                loop_: ID,
                req: 1,
                key: OTHER.into(),
                name: "x".into(),
                version: None,
                upgrade: 1,
            },
        )?)),
        "a-channel-of-another-site" => Up::Broadcast(broadcast::Up::Subscribe {
            id: ID,
            key: OTHER.into(),
            name: "c".into(),
        }),
        "a-blob-of-another-site" => Up::Net(net::Up::HoldBlobUrl(
            Url::parse(&format!("{OTHER}/0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f"))
                .map(|u| Url::parse(&format!("blob:{u}")).unwrap())
                .unwrap(),
        )),
        "the-code-cache-of-another-site" => Up::CodeCache(code_cache::Up::Store {
            url: format!("{OTHER} https://cdn.test/s.js"),
            kind: 0,
            hash: [0; 32],
            data: Bulk::Inline(vec![1]),
        }),
        "an-oversized-frame" => {
            return port.write_raw(u32::MAX.to_le_bytes().to_vec());
        }
        "garbage" => {
            return port.write_raw(frame::encode(frame::Kind::Message, 0, &[200, 200])?);
        }
        other => panic!("no such role: {other}"),
    };
    port.send(&message)
}
