#![allow(dead_code)]
use std::io;
use std::fmt;
use std::time;
use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicUsize};
use futures::{self, Future};
use mio;

use sync::oneshot;
use collections::RemovableHeap;

pub type RequestSender = std_mpsc::Sender<Request>;
pub type RequestReceiver = std_mpsc::Receiver<Request>;

pub const DEFAULT_EVENTS_CAPACITY: usize = 128;

struct MioEvents(mio::Events);
impl fmt::Debug for MioEvents {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "MioEvents(_)")
    }
}

#[derive(Debug)]
pub struct Registrant {
    is_first: bool,
    evented: BoxEvented,
    read_waitings: Vec<oneshot::Sender<()>>,
    write_waitings: Vec<oneshot::Sender<()>>,
}
impl Registrant {
    pub fn new(evented: BoxEvented) -> Self {
        Registrant {
            is_first: false,
            evented: evented,
            read_waitings: Vec::new(),
            write_waitings: Vec::new(),
        }
    }
    pub fn mio_interest(&self) -> mio::Ready {
        (if self.read_waitings.is_empty() {
            mio::Ready::none()
        } else {
            mio::Ready::readable()
        }) |
        (if self.write_waitings.is_empty() {
            mio::Ready::none()
        } else {
            mio::Ready::writable()
        })
    }
}

#[derive(Debug)]
pub struct Poller {
    poll: mio::Poll,
    events: MioEvents,
    request_tx: RequestSender,
    request_rx: RequestReceiver,
    next_token: usize,
    registrants: HashMap<mio::Token, Registrant>,
    timeout_queue: RemovableHeap<()>,
}
impl Poller {
    pub fn new() -> io::Result<Self> {
        Self::with_capacity(DEFAULT_EVENTS_CAPACITY)
    }
    pub fn with_capacity(capacity: usize) -> io::Result<Self> {
        let poll = mio::Poll::new()?;
        let (tx, rx) = std_mpsc::channel();
        Ok(Poller {
            poll: poll,
            events: MioEvents(mio::Events::with_capacity(capacity)),
            request_tx: tx,
            request_rx: rx,
            next_token: 0,
            registrants: HashMap::new(),
            timeout_queue: RemovableHeap::new(),
        })
    }
    pub fn registrant_count(&self) -> usize {
        self.registrants.len()
    }
    pub fn handle(&self) -> PollerHandle {
        PollerHandle {
            request_tx: self.request_tx.clone(),
            is_alive: true,
        }
    }
    pub fn poll(&mut self, timeout: Option<time::Duration>) -> io::Result<()> {
        let mut did_something = false;

        // Request
        match self.request_rx.try_recv() {
            Err(std_mpsc::TryRecvError::Empty) => {}
            Err(std_mpsc::TryRecvError::Disconnected) => unreachable!(),
            Ok(r) => {
                did_something = true;
                self.handle_request(r)?;
            }
        }

        // Timeout
        // TODO

        // I/O event
        let timeout = if did_something {
            Some(time::Duration::from_millis(0))
        } else if self.timeout_queue.len() > 0 {
            // TODO: min(timeout, timeout_queue.front() - now())
            timeout
        } else {
            timeout
        };
        let _ = self.poll.poll(&mut self.events.0, timeout)?;
        for e in self.events.0.iter() {
            let r = assert_some!(self.registrants.get_mut(&e.token()));
            if e.kind().is_readable() {
                for _ in r.read_waitings.drain(..).map(|tx| tx.send(())) {}
            }
            if e.kind().is_writable() {
                for _ in r.write_waitings.drain(..).map(|tx| tx.send(())) {}
            }
            Self::mio_register(&self.poll, e.token(), r)?;
        }

        Ok(())
    }
    fn handle_request(&mut self, request: Request) -> io::Result<()> {
        match request {
            Request::Register(evented, reply) => {
                let token = self.next_token();
                self.registrants.insert(token, Registrant::new(evented));
                let _ = reply.send(EventedHandle::new(self.request_tx.clone(), token));
            }
            Request::Deregister(token) => {
                let r = assert_some!(self.registrants.remove(&token));
                if !r.is_first {
                    self.poll.deregister(&*r.evented.0)?;
                }
            }
            Request::Monitor(token, interest, notifier) => {
                let r = assert_some!(self.registrants.get_mut(&token));
                match interest {
                    Interest::Read => r.read_waitings.push(notifier),
                    Interest::Write => r.write_waitings.push(notifier),
                }
                if r.read_waitings.len() == 1 || r.write_waitings.len() == 1 {
                    Self::mio_register(&self.poll, token, r)?;
                }
            }
            _ => unimplemented!(),
        }
        Ok(())
    }
    fn mio_register(poll: &mio::Poll, token: mio::Token, r: &mut Registrant) -> io::Result<()> {
        let interest = r.mio_interest();
        if interest != mio::Ready::none() {
            let options = mio::PollOpt::edge() | mio::PollOpt::oneshot();
            if r.is_first {
                r.is_first = false;
                poll.register(&*r.evented.0, token, interest, options)?;
            } else {
                poll.reregister(&*r.evented.0, token, interest, options)?;
            }
        }
        Ok(())
    }
    fn next_token(&mut self) -> mio::Token {
        loop {
            let token = self.next_token;
            self.next_token = token.wrapping_add(1);
            if self.registrants.contains_key(&mio::Token(token)) {
                continue;
            }
            return mio::Token(token);
        }
    }
}

#[derive(Debug, Clone)]
pub struct PollerHandle {
    request_tx: RequestSender,
    is_alive: bool,
}
impl PollerHandle {
    pub fn is_alive(&self) -> bool {
        self.is_alive
    }
    // TODO: name
    pub fn register<E>(&mut self, evented: E) -> Register
        where E: mio::Evented + Send + 'static
    {
        let evented = BoxEvented(Box::new(evented));
        let (tx, rx) = oneshot::channel();
        if self.request_tx.send(Request::Register(evented, tx)).is_err() {
            self.is_alive = false;
        }
        Register { rx: rx }
    }
}

#[derive(Debug)]
pub struct Register {
    rx: oneshot::Receiver<EventedHandle>,
}
impl Future for Register {
    type Item = EventedHandle;
    type Error = std_mpsc::RecvError;
    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        self.rx.poll()
    }
}

// TODO: name
#[derive(Debug)]
pub struct EventedHandle {
    token: mio::Token,
    request_tx: RequestSender,
    shared_count: Arc<AtomicUsize>,
}
impl EventedHandle {
    pub fn new(request_tx: RequestSender, token: mio::Token) -> Self {
        EventedHandle {
            token: token,
            request_tx: request_tx,
            shared_count: Arc::new(AtomicUsize::new(1)),
        }
    }
    pub fn monitor(&self, interest: Interest) -> Monitor {
        let (tx, rx) = oneshot::channel();
        let _ = self.request_tx.send(Request::Monitor(self.token, interest, tx));
        Monitor(rx)
    }
}
impl Clone for EventedHandle {
    fn clone(&self) -> Self {
        self.shared_count.fetch_add(1, atomic::Ordering::SeqCst);
        EventedHandle {
            token: self.token.clone(),
            request_tx: self.request_tx.clone(),
            shared_count: self.shared_count.clone(),
        }
    }
}
impl Drop for EventedHandle {
    fn drop(&mut self) {
        if 1 == self.shared_count.fetch_sub(1, atomic::Ordering::SeqCst) {
            let _ = self.request_tx.send(Request::Deregister(self.token));
        }
    }
}

#[derive(Debug)]
pub struct Monitor(oneshot::Receiver<()>);
impl Future for Monitor {
    type Item = ();
    type Error = std_mpsc::RecvError;
    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

#[derive(Debug)]
pub enum Interest {
    Read,
    Write,
}

pub struct BoxEvented(Box<mio::Evented + Send + 'static>);
impl fmt::Debug for BoxEvented {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "BoxEvented(_)")
    }
}

#[derive(Debug)]
pub enum Request {
    Register(BoxEvented, oneshot::Sender<EventedHandle>),
    Deregister(mio::Token),
    Monitor(mio::Token, Interest, oneshot::Sender<()>),
    SetTimeout,
    CancelTimeout,
}
