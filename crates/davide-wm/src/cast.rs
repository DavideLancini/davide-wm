//! Screen casting: PipeWire video streams of the primary output or of one
//! window, fed after each frame the compositor draws.

use std::io;
use std::os::fd::RawFd;

use pipewire::{PipeWire, State as StreamState, Stream};
use render::{Renderer, Target};

use crate::server::{SKey, State};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Source {
    Output,
    Window(SKey),
}

struct Cast {
    id: u32,
    source: Source,
    stream: Stream,
    ready: bool,
    streaming: bool,
    /// Window-sized, for drawing a window alone.
    scratch: Option<Target>,
    /// Stream-sized, when the source does not match the stream size.
    scaled: Option<Target>,
}

pub enum Event {
    /// Consumers can connect to this node.
    Ready { id: u32, node: u32, size: (i32, i32) },
    Ended(u32),
}

#[derive(Default)]
pub struct Casts {
    /// Before `pw`: streams must go before the connection.
    list: Vec<Cast>,
    pw: Option<PipeWire>,
    next_id: u32,
    dead: Vec<Target>,
    /// A stream skipped a frame to respect its rate: draw again soon.
    pub catch_up: bool,
}

/// Keeps `slot` a target of `w`x`h`.
fn sized<'a>(slot: &'a mut Option<Target>, dead: &mut Vec<Target>, r: &mut Renderer, w: i32, h: i32) -> Option<&'a Target> {
    if slot.as_ref().is_some_and(|t| (t.width, t.height) != (w, h)) {
        dead.extend(slot.take());
    }
    if slot.is_none() {
        *slot = r.create_target(w, h, 0).ok();
    }
    slot.as_ref()
}

impl Casts {
    pub fn fd(&self) -> Option<RawFd> {
        self.pw.as_ref().map(|p| p.raw_fd())
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Some consumer is reading.
    pub fn streaming(&self) -> bool {
        self.list.iter().any(|c| c.streaming)
    }

    /// Some stream would take a frame now.
    pub fn due(&self) -> bool {
        self.list.iter().any(|c| c.streaming && c.stream.wants_frame())
    }

    /// Starts a stream of `width`x`height`; the PipeWire connection is made
    /// on first use. Returns the cast id.
    pub fn start(&mut self, source: Source, width: u32, height: u32) -> io::Result<u32> {
        if self.pw.is_none() {
            self.pw = Some(PipeWire::connect()?);
        }
        let stream = Stream::new(self.pw.as_ref().unwrap(), "davide-wm", width, height, config::CAST_FPS)?;
        self.next_id += 1;
        let id = self.next_id;
        log!("cast {id}: {source:?} at {width}x{height}");
        self.list.push(Cast { id, source, stream, ready: false, streaming: false, scratch: None, scaled: None });
        Ok(id)
    }

    pub fn stop(&mut self, id: u32) {
        if let Some(i) = self.list.iter().position(|c| c.id == id) {
            let c = self.list.remove(i);
            self.dead.extend(c.scratch.into_iter().chain(c.scaled));
            log!("cast {id} stopped");
        }
    }

    pub fn ids(&self) -> Vec<(u32, Source)> {
        self.list.iter().map(|c| (c.id, c.source)).collect()
    }

    /// Runs PipeWire's loop. `wake` turns true when a consumer started
    /// reading and needs a first frame.
    pub fn dispatch(&mut self, wake: &mut bool) -> Vec<Event> {
        let Some(pw) = &self.pw else { return Vec::new() };
        pw.dispatch();
        let mut events = Vec::new();
        let mut ended = Vec::new();
        for c in &mut self.list {
            if !c.ready {
                if let Some(node) = c.stream.node_id() {
                    c.ready = true;
                    let (w, h) = c.stream.size();
                    events.push(Event::Ready { id: c.id, node, size: (w as i32, h as i32) });
                }
            }
            let streaming = c.stream.state() == StreamState::Streaming;
            if streaming && !c.streaming {
                *wake = true;
            }
            c.streaming = streaming;
            if c.stream.state() == StreamState::Error {
                log!("cast {}: {}", c.id, c.stream.error().unwrap_or("stream error"));
                ended.push(c.id);
            }
        }
        for id in ended {
            self.stop(id);
            events.push(Event::Ended(id));
        }
        events
    }

    /// Feeds the streams after the primary output was drawn into `primary`.
    pub fn frames(&mut self, s: &mut State, r: &mut Renderer, primary: &Target) {
        for t in self.dead.drain(..) {
            r.destroy_target(t);
        }
        self.catch_up = false;
        for c in &mut self.list {
            if !c.streaming {
                continue;
            }
            if !c.stream.wants_frame() {
                self.catch_up = true;
                continue;
            }
            let (w, h) = c.stream.size();
            let (w, h) = (w as i32, h as i32);
            let src = match c.source {
                Source::Output => primary,
                Source::Window(key) => {
                    let Some((ww, wh)) = s.window_size(key) else { continue };
                    let Some(t) = sized(&mut c.scratch, &mut self.dead, r, ww, wh) else { continue };
                    s.render_window(r, key, t);
                    t
                }
            };
            let frame = if (src.width, src.height) == (w, h) {
                r.read_target(src)
            } else {
                let Some(t) = sized(&mut c.scaled, &mut self.dead, r, w, h) else { continue };
                r.blit(src, t);
                r.read_target(t)
            };
            c.stream.push(frame);
        }
    }
}
