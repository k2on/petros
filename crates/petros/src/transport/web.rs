//! The browser half of the transport: the same `Link` shape over a `WebSocket`
//! from the DOM.
//!
//! Like its sibling, this is deliberately thin and knows nothing about the
//! engine. It exists so a page can talk to the very same server the terminal
//! examples talk to.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use serde::de::DeserializeOwned;
use serde::Serialize;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{js_sys, MessageEvent, WebSocket};

use crate::{decode, encode, ClientMsg, Error, Result, ServerMsg};

/// Shared between the socket's callbacks and the caller.
struct Inbox<M> {
    messages: VecDeque<ServerMsg<M>>,
    open: bool,
    closed: bool,
}

/// A connection to an Petros server. Drop it to disconnect.
///
/// The browser gives us callbacks rather than a blocking read, so frames land
/// in a queue and [`try_recv`](Link::try_recv) drains it — which is exactly the
/// shape the sans-io client wants anyway.
pub struct Link<M> {
    socket: WebSocket,
    inbox: Rc<RefCell<Inbox<M>>>,
    /// Frames written before the socket finished opening. A `WebSocket` refuses
    /// sends until then, and the first thing a client says is its `Hello`.
    backlog: RefCell<Vec<Vec<u8>>>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(JsValue)>,
    _on_open: Closure<dyn FnMut(JsValue)>,
}

impl<M: Serialize + DeserializeOwned + 'static> Link<M> {
    /// Connect to `ws://host:port`.
    pub fn connect(url: &str) -> Result<Self> {
        let socket = WebSocket::new(url).map_err(js_err)?;
        socket.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let inbox = Rc::new(RefCell::new(Inbox {
            messages: VecDeque::new(),
            open: false,
            closed: false,
        }));

        let on_message = {
            let inbox = inbox.clone();
            Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
                let Ok(buffer) = e.data().dyn_into::<js_sys::ArrayBuffer>() else {
                    return;
                };
                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                if let Ok(msg) = decode::<ServerMsg<M>>(&bytes) {
                    inbox.borrow_mut().messages.push_back(msg);
                }
            })
        };
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let on_close = {
            let inbox = inbox.clone();
            Closure::<dyn FnMut(JsValue)>::new(move |_| {
                inbox.borrow_mut().closed = true;
            })
        };
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        socket.set_onerror(Some(on_close.as_ref().unchecked_ref()));

        let on_open = {
            let inbox = inbox.clone();
            Closure::<dyn FnMut(JsValue)>::new(move |_| {
                inbox.borrow_mut().open = true;
            })
        };
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));

        Ok(Link {
            socket,
            inbox,
            backlog: RefCell::new(Vec::new()),
            _on_message: on_message,
            _on_close: on_close,
            _on_open: on_open,
        })
    }

    /// Queue a message. Returns false once the link is gone.
    pub fn send(&self, msg: ClientMsg<M>) -> bool {
        let Ok(bytes) = encode(&msg) else {
            return false;
        };
        if !self.is_alive() {
            return false;
        }
        if self.inbox.borrow().open {
            self.flush();
            self.socket.send_with_u8_array(&bytes).is_ok()
        } else {
            // Not open yet. Hold it rather than dropping it: the very first
            // thing a client sends is the Hello that starts the sync.
            self.backlog.borrow_mut().push(bytes);
            true
        }
    }

    /// Take one message the server sent, if any. Never blocks.
    pub fn try_recv(&self) -> Option<ServerMsg<M>> {
        if self.inbox.borrow().open {
            self.flush();
        }
        self.inbox.borrow_mut().messages.pop_front()
    }

    pub fn is_alive(&self) -> bool {
        !self.inbox.borrow().closed
    }

    fn flush(&self) {
        for bytes in self.backlog.borrow_mut().drain(..) {
            let _ = self.socket.send_with_u8_array(&bytes);
        }
    }
}

impl<M> Drop for Link<M> {
    fn drop(&mut self) {
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        self.socket.set_onerror(None);
        self.socket.set_onopen(None);
        let _ = self.socket.close();
    }
}

impl<M> std::fmt::Debug for Link<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link").finish_non_exhaustive()
    }
}

fn js_err(e: JsValue) -> Error {
    Error::Transport(format!("{e:?}"))
}
