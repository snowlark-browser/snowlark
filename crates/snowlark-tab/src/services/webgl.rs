// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How a renderer gets its link to the GPU process for WebGL ([`crate::webgl_link`]): it asks the
//! browser, which makes a link between the two, gives the GPU process its end and answers with the
//! renderer's. Asked again after a link ends (the GPU process was started again), up to
//! [`MAX_LINKS`] times in the renderer's life.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use snowlark_ipc::{Error, LinkEnd, Replier};

use super::{answer, ask, BrowserPort, RendererPort};
use crate::webgl_link::EstablishLink;

/// How many links one renderer may ask for in its life.
pub const MAX_LINKS: u32 = 64;

/// Renderer to browser.
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// A link to the GPU process for the renderer's WebGL contexts (a call): its end, or none when
    /// there is no GPU process (the renderer then has no WebGL).
    Establish,
}

/// The renderer's end.
pub struct RemoteWebGl {
    port: RendererPort,
}

impl RemoteWebGl {
    pub fn new(port: RendererPort) -> RemoteWebGl {
        RemoteWebGl { port }
    }
}

impl EstablishLink for RemoteWebGl {
    fn establish(&self) -> Option<LinkEnd> {
        let end: Option<LinkEnd> = ask(&self.port, &super::Up::WebGl(Up::Establish))?;
        end
    }
}

/// Where the browser makes a renderer's link to the GPU process.
pub trait WebGlLinks: Send + Sync {
    /// A link between the renderer at `renderer` and the GPU process, once one is ready (a while
    /// at most), whose end the GPU process is given: the renderer's end. `None` without a GPU
    /// process.
    fn link(&self, renderer: &BrowserPort) -> Option<LinkEnd>;
}

/// The browser's end for one renderer.
pub struct WebGlHost {
    port: BrowserPort,
    links: Option<Arc<dyn WebGlLinks>>,
    asked: AtomicU32,
}

impl WebGlHost {
    pub fn new(port: BrowserPort, links: Option<Arc<dyn WebGlLinks>>) -> WebGlHost {
        WebGlHost {
            port,
            links,
            asked: AtomicU32::new(0),
        }
    }

    pub fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        match message {
            Up::Establish => {
                let n = self.asked.fetch_add(1, Ordering::Relaxed);
                let end = match &self.links {
                    // (past the cap the renderer has no more WebGL: an honest one asks again only
                    // after the GPU process ended)
                    Some(l) if n < MAX_LINKS => l.link(&self.port),
                    _ => None,
                };
                answer(&self.port, replier, &end)
            }
        }
    }
}
