// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL's link, both ends in this process over a real pipe and rings (the browser's ends are
//! loopback ones), served by a backend that records what it runs.

use std::sync::atomic::AtomicUsize;
use std::thread;

use snowlark_gl::Kind;
use snowlark_ipc::{loopback, BrowserEnd};

use super::*;

const WAIT: Duration = Duration::from_secs(20);

/// What the recording backend ran, by context.
#[derive(Default)]
struct Log {
    commands: Vec<(ContextId, Command)>,
    contexts: Vec<ContextId>,
}

/// A backend that keeps what it is sent; a query for `Error` answers how many commands its context
/// had, a present answers a buffer of the context's size filled with its id.
struct Recorder {
    log: Arc<Mutex<Log>>,
    sizes: HashMap<ContextId, (u32, u32)>,
}

impl Backend for Recorder {
    fn create_context(
        &mut self,
        id: ContextId,
        attributes: &Attributes,
        width: u32,
        height: u32,
    ) -> Result<Created, String> {
        self.log.lock().unwrap().contexts.push(id);
        self.sizes.insert(id, (width, height));
        Ok(Created {
            attributes: *attributes,
            limits: Default::default(),
            extensions: vec!["OES_x".into()],
            requestable: Vec::new(),
        })
    }
    fn resize(&mut self, id: ContextId, width: u32, height: u32) {
        self.sizes.insert(id, (width, height));
    }
    fn destroy(&mut self, id: ContextId) {
        self.sizes.remove(&id);
    }
    fn submit(&mut self, batch: Vec<(ContextId, Command)>) {
        self.log.lock().unwrap().commands.extend(batch);
    }
    fn query(&mut self, id: ContextId, query: &Query) -> Reply {
        match query {
            Query::Error => {
                let log = self.log.lock().unwrap();
                Reply::Int(log.commands.iter().filter(|(c, _)| *c == id).count() as i32)
            }
            _ => Reply::None,
        }
    }
    fn present(&mut self, id: ContextId) -> Option<Readback> {
        let &(width, height) = self.sizes.get(&id)?;
        Some(Readback {
            width,
            height,
            rgba: vec![id as u8; (width * height * 4) as usize],
        })
    }
}

/// A loopback pair, kept while its link lives.
type Ends = (BrowserEnd<(), ()>, snowlark_ipc::ChildEnd<(), ()>);

/// The browser's part: makes a link each time a renderer asks, and serves the GPU process's end of
/// it on a thread with a [`Recorder`].
struct Browser {
    ports: Mutex<Vec<Ends>>,
    log: Arc<Mutex<Log>>,
    links: Arc<AtomicUsize>,
    refused: Arc<Mutex<Vec<String>>>,
    /// The GPU ends served so far, to end them.
    servers: Arc<Mutex<Vec<GlServer>>>,
}

impl Browser {
    fn new() -> Arc<Browser> {
        Arc::new(Browser {
            ports: Mutex::new(Vec::new()),
            log: Arc::default(),
            links: Arc::default(),
            refused: Arc::default(),
            servers: Arc::default(),
        })
    }

    /// Ends every link served so far, as a GPU process that goes away.
    fn end_links(&self) {
        for s in self.servers.lock().unwrap().drain(..) {
            s.close();
        }
    }
}

struct Establish(Arc<Browser>);

impl EstablishLink for Establish {
    fn establish(&self) -> Option<LinkEnd> {
        let b = &self.0;
        let (renderer, rc) = loopback::pair::<(), ()>(0).ok()?;
        let (gpu, gc) = loopback::pair::<(), ()>(0).ok()?;
        let [mine, theirs] = renderer.link(&gpu, [RING_UP, RING_DOWN]).ok()?;
        b.ports.lock().unwrap().extend([(renderer, rc), (gpu, gc)]);
        b.links.fetch_add(1, Ordering::Relaxed);
        let (log, refused, servers) = (b.log.clone(), b.refused.clone(), b.servers.clone());
        thread::spawn(move || {
            let link: GlServer = snowlark_ipc::connect_link(theirs, WAIT).unwrap();
            servers.lock().unwrap().push(link.clone());
            serve(
                link,
                move || {
                    Ok(Box::new(Recorder {
                        log: log.clone(),
                        sizes: HashMap::new(),
                    }) as Box<dyn Backend>)
                },
                |why| refused.lock().unwrap().push(why.to_owned()),
            );
        });
        Some(mine)
    }
}

fn channel(b: &Arc<Browser>) -> Arc<WebGlChannel> {
    WebGlChannel::new(Arc::new(Establish(b.clone())))
}

fn make(gl: &mut RemoteGl, id: ContextId, w: u32, h: u32) -> Created {
    gl.create_context(id, &Attributes::default(), w, h).unwrap()
}

#[test]
fn commands_reach_the_gpu_process_in_order_before_the_query_after_them() {
    let b = Browser::new();
    let ch = channel(&b);
    let mut gl = ch.backend().unwrap();
    let made = make(&mut gl, 1, 4, 4);
    assert_eq!(made.extensions, vec!["OES_x".to_owned()]);
    gl.submit(vec![
        (1, Command::Create(Kind::Buffer, 1)),
        (1, Command::ClearColor([0.0, 1.0, 0.0, 1.0])),
        (1, Command::Clear(0x4000)),
    ]);
    gl.submit(vec![(1, Command::Flush)]);
    // (the query runs after every command sent before it)
    assert_eq!(gl.query(1, &Query::Error), Reply::Int(4));
    let log = b.log.lock().unwrap();
    let cmds: Vec<&Command> = log.commands.iter().map(|(_, c)| c).collect();
    assert_eq!(cmds[1], &Command::ClearColor([0.0, 1.0, 0.0, 1.0]));
    assert_eq!(cmds[3], &Command::Flush);
}

#[test]
fn a_readback_larger_than_the_ring_arrives_whole() {
    let b = Browser::new();
    let ch = channel(&b);
    let mut gl = ch.backend().unwrap();
    // 2048 × 1536 RGBA: 12 MiB, more than the ring down holds
    make(&mut gl, 3, 2048, 1536);
    let rb = gl.present(3).unwrap();
    assert!(rb.rgba.len() as u64 > RING_DOWN);
    assert_eq!((rb.width, rb.height), (2048, 1536));
    let id = b.log.lock().unwrap().contexts[0] as u8;
    assert!(rb.rgba.iter().all(|&v| v == id));
    // and an upload larger than the ring up
    let big = vec![7u8; (RING_UP as usize) * 3 / 2];
    gl.submit(vec![(
        3,
        Command::BufferData(0x8892, big.len() as i64, Some(big), 0x88E4),
    )]);
    assert_eq!(gl.query(3, &Query::Error), Reply::Int(1));
}

#[test]
fn two_pages_on_one_link_keep_their_own_contexts() {
    let b = Browser::new();
    let ch = channel(&b);
    let mut one = ch.backend().unwrap();
    let mut two = ch.backend().unwrap();
    // (both number their first context 1)
    make(&mut one, 1, 1, 1);
    make(&mut two, 1, 1, 1);
    one.submit(vec![(1, Command::Flush), (1, Command::Flush)]);
    two.submit(vec![(1, Command::Flush)]);
    assert_eq!(one.query(1, &Query::Error), Reply::Int(2));
    assert_eq!(two.query(1, &Query::Error), Reply::Int(1));
    assert_eq!(b.links.load(Ordering::Relaxed), 1, "one link per renderer");
    let contexts = b.log.lock().unwrap().contexts.clone();
    assert_eq!(contexts.len(), 2);
    assert_ne!(contexts[0], contexts[1]);
}

#[test]
fn a_gpu_process_gone_resets_the_contexts_and_a_new_context_links_again() {
    let b = Browser::new();
    let ch = channel(&b);
    let mut gl = ch.backend().unwrap();
    make(&mut gl, 1, 2, 2);
    assert_eq!(gl.query(1, &Query::ResetStatus), Reply::None);
    b.end_links();
    // WebGL 1.0 §5.15.2: the context reads as reset, and is lost
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if gl.query(1, &Query::ResetStatus) == Reply::Int(UNKNOWN_CONTEXT_RESET) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never reset");
    }
    assert!(gl.present(1).is_none());
    // a new context asks the browser for a new link
    make(&mut gl, 2, 2, 2);
    assert_eq!(b.links.load(Ordering::Relaxed), 2);
    gl.submit(vec![(2, Command::Flush)]);
    assert_eq!(gl.query(2, &Query::Error), Reply::Int(1));
    // (and the old context stays lost)
    assert_eq!(gl.query(1, &Query::ResetStatus), Reply::None);
}

#[test]
fn a_drawing_buffer_too_large_is_refused_and_the_link_lives() {
    let b = Browser::new();
    let ch = channel(&b);
    let mut gl = ch.backend().unwrap();
    assert!(gl
        .create_context(1, &Attributes::default(), MAX_SIDE + 1, 1)
        .is_err());
    make(&mut gl, 2, MAX_SIDE, 1);
    assert!(b.refused.lock().unwrap().is_empty());
}

#[test]
fn a_request_sent_as_the_other_kind_ends_the_link() {
    let b = Browser::new();
    let ch = channel(&b);
    let link = ch.linked().unwrap();
    // a present that does not wait for its answer: the ends would no longer agree
    let bytes = wire::encode(&Request::Present(1)).unwrap();
    link.link.send_bulk(&bytes, GlUp::Send, BYTES_WAIT).unwrap();
    let deadline = std::time::Instant::now() + WAIT;
    while b.refused.lock().unwrap().is_empty() {
        assert!(std::time::Instant::now() < deadline, "not refused");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(b.refused.lock().unwrap()[0].contains("other kind"));
}

#[test]
fn a_section_on_the_link_is_refused() {
    let p = policy();
    assert!(GlUp::Ask(Bulk::Shared { id: 1, len: 4 })
        .validate(&p)
        .is_err());
    assert!(GlUp::Send(Bulk::Given {
        handle: 1,
        size: 4,
        len: 4
    })
    .validate(&p)
    .is_err());
    assert!(GlUp::Send(Bulk::Ring {
        pos: 0,
        len: MAX_REQUEST + 1
    })
    .validate(&p)
    .is_err());
    assert!(GlUp::Send(Bulk::Ring { pos: 0, len: 4 })
        .validate(&p)
        .is_ok());
}
