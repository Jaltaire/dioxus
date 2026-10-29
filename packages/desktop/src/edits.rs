//! The internal edit queue facilitating native <-> webview communication.
//!
//! Originally, we used long-polling on the wry custom protocol to send edits to the webview.
//! Due to bugs in wry on android, we switched to a websocket connection that the webview connects to.
//! We use the sledgehammer crate to build batches of edits and send them through the websocket to
//! the webview.
//!
//! Using a websocket lets us send binary data to the webview quite efficiently and does encounter
//! many of the issues with regular request/response protocols. Note that the websocket max frame
//! size is quite large (9.22 exabytes), so we can have very large batches without issue.
//!
//! Using websockets does mean we need to handle security and content security policies ourselves.
//! The code here generates a random key that the webview must use to connect to the websocket.
//! We use the initialization script API to setup the websocket connection without leaking the key
//! to the webview itself in case there's untrusted content in the webview.
//!
//! Some operating systems (like iOS) will kill the websocket connection when the device goes to sleep.
//! If this happens, we will automatically switch to a new port and notify the webview of the new location
//! and key. The webview will then reconnect to the new port and continue receiving edits.

use dioxus_interpreter_js::MutationState;
use futures_channel::oneshot;
use futures_util::FutureExt;
use rand::{RngCore, SeedableRng};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::net::{TcpListener, TcpStream};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::time::Duration;
use std::{
    net::IpAddr,
    sync::{Arc, RwLock},
};
use tokio::sync::Notify;

/// A page served into a webview, numbered in the order the webview was
/// served its pages.
///
/// A page names itself in everything it sends: its report that it has
/// loaded, the connection it is sent its edits over, and every event. Edits
/// are written for one page at a time, the page that last reported in, and
/// nothing is taken from or sent to any other: a page that has been
/// replaced still holds the nodes of the tree it was drawn from, and the
/// page that replaces it holds none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct Page(u64);

impl std::fmt::Display for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for Page {
    type Err = std::num::ParseIntError;

    fn from_str(number: &str) -> Result<Self, Self::Err> {
        number.parse().map(Page)
    }
}

/// The header an event names the page it was raised in with.
pub(crate) const PAGE_HEADER: &str = "dioxus-page";

/// The code a page's edits connection is closed with once the page has
/// been replaced, which tells the page to stop asking for another. The
/// interpreter's native script holds the same number.
pub(crate) const PAGE_REPLACED: u16 = 4001;

/// How long a page told it has been replaced is given to answer the close
/// before its connection is dropped regardless.
const CLOSING_PATIENCE: Duration = Duration::from_secs(1);

/// What became of a page's report that it has loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// The page is the one edits are written for from here on, starting
    /// with its rebuild.
    Drawn,
    /// The page, or a later one, has reported in already, so the page is
    /// not drawn into again.
    Stale { newest: Page },
}

/// What became of a page's request for an edits connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// The page is the one being drawn into, and is sent what is waiting.
    Drawn,
    /// The page has not reported in yet. Its connection is kept for the
    /// moment it does.
    Held,
    /// The page has been replaced, and is refused.
    Replaced,
}

/// This handles communication between the requests that the webview makes and the interpreter.
#[derive(Clone)]
pub(crate) struct WryQueue {
    inner: Rc<RefCell<WryQueueInner>>,
    pages: WebviewPagesHandle,
}

impl WryQueue {
    pub(crate) fn with_mutation_state_mut<O: 'static>(
        &self,
        callback: impl FnOnce(&mut MutationState) -> O,
    ) -> O {
        let mut inner = self.inner.borrow_mut();
        callback(&mut inner.mutation_state)
    }

    /// Numbers the page about to be served.
    pub(crate) fn serve_page(&self) -> Page {
        self.pages.with(|pages| pages.serve())
    }

    /// Whether the page is the one edits are written for.
    pub(crate) fn is_drawn(&self, page: Page) -> bool {
        self.pages.with(|pages| pages.drawn == Some(page))
    }

    /// Forgets the connection this webview's page had, so that edits sent from
    /// here on are queued for the page that replaces it.
    ///
    /// Used when the process drawing the page is terminated by the platform.
    /// The page never closed its connection -- it did not get the chance -- so
    /// without this the edits rebuilding the new page are handed to a channel
    /// nothing is reading and the window stays empty.
    pub(crate) fn forget_connection(&self) {
        let mut inner = self.inner.borrow_mut();
        tracing::info!(
            "Webview {} forgets its edits connection and awaits a page.",
            inner.location.webview_id
        );
        // Until the new page reports in, anything the virtual dom renders is
        // a diff against the page that is gone. Sent on, it would be the first
        // thing the new page received: edits to nodes it has never had. The
        // page that is gone, and every page before it, is refused a
        // connection from here on, so a page that outlives its loss cannot
        // take edits meant for the one replacing it.
        self.pages.with(|pages| pages.lose());
        // The page owed an acknowledgement for the last edits it was sent, and
        // cannot give one now. Left in place it is waited on for good: the
        // virtual dom is held back until the edits in flight are flushed, so
        // nothing would ever be rendered into the page that replaces this one.
        inner.edits_in_progress = None;
        // Templates are sent to a page once and referred to by number ever
        // after. A page that has just been loaded knows none of them, so
        // without this the rebuild refers to templates the new page has never
        // been given and builds nothing at all -- which looks exactly like the
        // empty window this is all meant to cure.
        inner.mutation_state = MutationState::default();
    }

    pub(crate) fn send_edits(&self) {
        let mut myself = self.inner.borrow_mut();
        let webview_id = myself.location.webview_id;
        let serialized_edits = myself.mutation_state.export_memory();
        let length = serialized_edits.len();
        match self.pages.with(|pages| pages.write(serialized_edits)) {
            Some(receiver) => {
                tracing::trace!("Webview {webview_id} is sent {length} bytes of edits.");
                myself.edits_in_progress = Some(receiver);
            }
            None => tracing::debug!(
                "Webview {webview_id} rendered {length} bytes of edits with no page drawn into; they are dropped."
            ),
        }
    }

    /// A page has reported in.
    ///
    /// If it is newer than every page that reported in before it, it is the
    /// page drawn into from here on: whatever was rendered while it was
    /// awaited has been dropped, and the page knows no templates yet, so the
    /// state that numbers them starts over and the rebuild that follows sends
    /// every template the page needs. A page that has reported in already,
    /// or one older than a page that has, is left as it is, so no page is
    /// ever rebuilt into twice.
    pub(crate) fn page_arrived(&self, page: Page) -> Arrival {
        let arrival = self.pages.with(|pages| pages.report_in(page));
        if arrival == Arrival::Drawn {
            let mut myself = self.inner.borrow_mut();
            myself.edits_in_progress = None;
            myself.mutation_state = MutationState::default();
        }
        arrival
    }

    pub(crate) fn is_idle(&self) -> bool {
        if self.pages.with(|pages| pages.drawn.is_none()) {
            return false;
        }
        let mut inner = self.inner.borrow_mut();
        match inner.edits_in_progress.as_mut() {
            None => true,
            Some(receiver) => !matches!(receiver.try_recv(), Ok(None)),
        }
    }

    pub(crate) fn take_rendered(&self) -> Vec<u8> {
        self.inner.borrow_mut().mutation_state.export_memory()
    }

    /// Wait until all pending edits have been rendered in the webview
    pub(crate) fn poll_edits_flushed(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let mut self_mut = self.inner.borrow_mut();
        if let Some(receiver) = self_mut.edits_in_progress.as_mut() {
            receiver.poll_unpin(cx).map(|_| ())
        } else {
            std::task::Poll::Ready(())
        }
    }

    /// Check if there is a new location for the websocket edits server.
    pub(crate) fn poll_new_edits_location(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let mut self_mut = self.inner.borrow_mut();
        let poll = self_mut
            .server_location_changed_future
            .as_mut()
            .poll_unpin(cx);
        if poll.is_ready() {
            // If the future is ready, we need to reset it to wait for the next change
            self_mut.server_location_changed_future =
                owned_notify_future(self_mut.server_location_changed.clone());
        }
        poll
    }

    /// Get the websocket path that the webview should connect to in order to receive edits.
    /// The page appends its own number to it.
    pub(crate) fn edits_path(&self) -> String {
        let WebviewWebsocketLocation {
            webview_id, server, ..
        } = &self.inner.borrow().location;
        let server = server.lock().unwrap();
        let port = server.port;
        let key = &server.client_key;
        let key_hex = encode_key_string(key);
        format!("ws://127.0.0.1:{port}/{webview_id}/{key_hex}")
    }

    /// Get the key the client should expect from the server when connecting to the websocket.
    pub(crate) fn required_server_key(&self) -> String {
        let server = &self.inner.borrow().location.server;
        let server = server.lock().unwrap();
        encode_key_string(&server.server_key)
    }
}

pub(crate) struct WryQueueInner {
    location: WebviewWebsocketLocation,
    // If this webview is currently waiting for an edit to be flushed. We don't run the virtual dom while this is true to avoid running effects before the dom has been updated
    edits_in_progress: Option<oneshot::Receiver<()>>,
    // The socket may be killed by the OS while running. If it does, this channel will receive the new server location
    server_location_changed: Arc<Notify>,
    server_location_changed_future: Pin<Box<dyn Future<Output = ()>>>,
    mutation_state: MutationState,
}

/// The location of a webview websocket connection. This is used to identify the webview and the port it is connected to.
#[derive(Clone)]
pub(crate) struct WebviewWebsocketLocation {
    /// The id of the webview that this websocket is connected to
    webview_id: u32,
    server: Arc<Mutex<ServerLocation>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ServerLocation {
    /// The port the websocket is on
    port: u16,
    /// A key that every websocket connection that originates from this application will use to identify itself.
    /// We use this to make sure no external applications can connect to our websocket and receive UI updates.
    client_key: [u8; KEY_SIZE],
    /// The key that the server must respond with for the client to connect to the websocket
    server_key: [u8; KEY_SIZE],
}

/// Start a new server on an available port on localhost. Return the server location and the TCP listener that is bound to the port.
pub(crate) fn start_server() -> (ServerLocation, TcpListener) {
    let client_key = create_secure_key();
    let server_key = create_secure_key();
    let server = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0))
        .expect("Failed to bind local TCP listener for edit socket");
    let port = server.local_addr().unwrap().port();
    let location = ServerLocation {
        port,
        client_key,
        server_key,
    };
    (location, server)
}

/// The websocket listener that the webview will connect to in order to receive edits and send requests. There
/// is only one websocket listener per application even if there are multiple windows so we don't use all the
/// open ports.
#[derive(Clone)]
pub(crate) struct EditWebsocket {
    current_location: Arc<Mutex<ServerLocation>>,
    max_webview_id: Arc<AtomicU32>,
    connections: Arc<RwLock<HashMap<u32, WebviewPages>>>,
    server_location: Arc<Notify>,
}

impl EditWebsocket {
    pub(crate) fn start() -> Self {
        let connections = Arc::new(RwLock::new(HashMap::new()));

        let notify = Arc::new(Notify::new());
        let (location, server) = start_server();
        let current_location = Arc::new(Mutex::new(location));

        // Every accepted connection is numbered, so that the thread serving
        // one can tell whether the table still refers to it before writing
        // there.
        let next_connection = Arc::new(AtomicU64::new(0));
        let connections_ = connections.clone();
        let current_location_ = current_location.clone();
        let notify_ = notify.clone();
        std::thread::spawn(move || {
            Self::accept_loop(
                notify_,
                server,
                current_location_,
                connections_,
                next_connection,
            )
        });

        Self {
            connections,
            max_webview_id: Default::default(),
            current_location,
            server_location: notify,
        }
    }

    /// Accepts incoming websocket connections and handles them in a loop.
    ///
    /// New sockets are accepted and then put in to a new thread to handle the connection.
    /// This is implemented using traditional sync code to allow us to be independent of the async runtime.
    fn accept_loop(
        notify: Arc<Notify>,
        mut server: TcpListener,
        current_location: Arc<Mutex<ServerLocation>>,
        connections: Arc<RwLock<HashMap<u32, WebviewPages>>>,
        next_connection: Arc<AtomicU64>,
    ) {
        loop {
            // Accept connections until we hit an error
            while let Ok((stream, _)) = server.accept() {
                Self::handle_connection(
                    stream,
                    current_location.clone(),
                    connections.clone(),
                    next_connection.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                );
            }

            // Switch ports and reconnect on a different port if the server is killed by the OS. This
            // will happen if an IOS device goes to sleep
            //
            // For security, it is important that the keys are also regenerated when the server is restarted.
            // The client may try to reconnect to the old port that is now being used by an attacker who steals the client
            // key and uses it to read the edits from the new port.
            let (location, new_server) = start_server();
            notify.notify_waiters();
            *current_location.lock().unwrap() = location;
            server = new_server;
        }
    }

    fn handle_connection(
        stream: TcpStream,
        server_location: Arc<Mutex<ServerLocation>>,
        connections: Arc<RwLock<HashMap<u32, WebviewPages>>>,
        connection_number: u64,
    ) {
        use tungstenite::handshake::server::{Request, Response};

        let current_server_location = { *server_location.lock().unwrap() };
        let hex_encoded_client_key = encode_key_string(&current_server_location.client_key);
        let hex_encoded_server_key = encode_key_string(&current_server_location.server_key);
        let mut location = None;

        #[allow(clippy::result_large_err)]
        let on_request = |req: &Request, res| {
            // Try to parse the webview id, key, and page from the path
            let path = req.uri().path();

            // The path should have three parts `/webview_id/key/page`
            let mut segments = path.trim_matches('/').split('/');
            let webview_id = segments
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or_else(|| {
                    Response::builder()
                        .status(400)
                        .body(Some("Bad Request: Invalid webview ID".to_string()))
                        .unwrap()
                })?;
            let key = segments.next().ok_or_else(|| {
                Response::builder()
                    .status(400)
                    .body(Some("Bad Request: Missing key".to_string()))
                    .unwrap()
            })?;

            // Make sure the key matches the expected key.
            // VERY IMPORTANT: We cannot use normal string comparison here because it reveals information
            // about the key based on timing information. Instead we use a constant time comparison method.
            let key_matches: bool =
                subtle::ConstantTimeEq::ct_eq(hex_encoded_client_key.as_ref(), key.as_bytes())
                    .into();
            if !key_matches {
                return Err(Response::builder()
                    .status(403)
                    .body(Some("Forbidden: Invalid key".to_string()))
                    .unwrap());
            }

            let page = segments
                .next()
                .and_then(|s| s.parse::<Page>().ok())
                .ok_or_else(|| {
                    Response::builder()
                        .status(400)
                        .body(Some("Bad Request: Invalid page".to_string()))
                        .unwrap()
                })?;

            location = Some((
                WebviewWebsocketLocation {
                    webview_id,
                    server: server_location,
                },
                page,
            ));

            Ok(res)
        };

        // Accept the websocket connection while reading the path and setting the location
        let mut websocket = match tungstenite::accept_hdr(stream, on_request) {
            Ok(ws) => ws,
            Err(e) => {
                tracing::error!("Error accepting websocket connection: {}", e);
                return;
            }
        };

        let (location, page) = match location {
            Some(loc) => loc,
            None => {
                tracing::error!("WebSocket connection without a valid webview ID");
                return;
            }
        };

        // Handle the websocket connection in a separate thread
        let (edits_outgoing, edits_incoming_rx) = std::sync::mpsc::channel::<MsgPair>();

        let admission = connections
            .write()
            .unwrap()
            .get_mut(&location.webview_id)
            .map(|pages| {
                pages.connect(
                    page,
                    Connection {
                        edits_outgoing,
                        number: connection_number,
                    },
                )
            })
            .unwrap_or(Admission::Replaced);

        // Immediately send the key to authenticate the server
        if let Err(error) =
            websocket.send(tungstenite::Message::Text(hex_encoded_server_key.into()))
        {
            tracing::error!("Error authenticating the edits connection to the webview: {error}");
            if let Some(pages) = connections.write().unwrap().get_mut(&location.webview_id) {
                pages.disconnect(page, connection_number, None);
            }
            return;
        }

        match admission {
            Admission::Drawn => tracing::debug!(
                "Page {page} of webview {} opened edits connection {connection_number}.",
                location.webview_id
            ),
            Admission::Held => tracing::debug!(
                "Page {page} of webview {} opened edits connection {connection_number} before \
                 reporting in; it is kept for when the page does.",
                location.webview_id
            ),
            Admission::Replaced => {
                tracing::info!(
                    "Page {page} of webview {} asked for an edits connection after it was \
                     replaced, and is refused.",
                    location.webview_id
                );
                std::thread::spawn(move || tell_replaced(websocket));
                return;
            }
        }

        let connections_ = connections.clone();
        // Spawn a task to handle the websocket connection
        std::thread::spawn(move || {
            let mut queued_message = None;
            let mut closed = false;
            // Wait until there are edits ready to send
            'connection: while let Ok(msg) = edits_incoming_rx.recv() {
                let data = msg.edits.clone();
                queued_message = Some(msg);
                // Send the edits to the webview
                tracing::trace!(
                    "Edits connection {connection_number} of webview {} sends {} bytes.",
                    location.webview_id,
                    data.len()
                );
                if let Err(e) = websocket.send(tungstenite::Message::Binary(data.into())) {
                    tracing::error!("Error sending edits to webview: {}", e);
                    closed = true;
                    break 'connection;
                }

                // Wait for the webview to apply the edits
                while let Ok(ws_msg) = websocket.read() {
                    match ws_msg {
                        // We expect the webview to send a binary message when it has applied the edits
                        // This is a signal that we can continue processing
                        tungstenite::Message::Binary(_) => break,
                        // If the websocket closes, switch back to the pending state and
                        // re-queue the edits that haven't been acknowledged yet
                        tungstenite::Message::Close(_) => {
                            closed = true;
                            break 'connection;
                        }
                        _ => {}
                    }
                }

                let msg = queued_message.take().expect("Message should be set here");
                tracing::trace!(
                    "Edits connection {connection_number} of webview {} has its edits applied.",
                    location.webview_id
                );

                // Notify that the edits have been applied. Nobody is waiting
                // when the page these edits were for has gone while they were
                // in flight: its connection was forgotten, and with it the wait.
                if msg.response.send(()).is_err() {
                    tracing::debug!(
                        "Edits connection {connection_number} of webview {} had its edits \
                         acknowledged after the page they were for was gone.",
                        location.webview_id
                    );
                }
            }
            tracing::trace!("Webview {} closed the connection", location.webview_id);
            // A page whose process was terminated never closes its connection,
            // and the thread serving it learns of the loss only when the
            // sender it waits on is dropped -- which is the moment the page
            // is lost, or replaced by one that has reported in. Writing a
            // pending state then would wipe the new page's connection from the
            // table, and every edit from there on would be queued for a page
            // that never comes. So the table only takes back the edits this
            // connection owed while it still refers to this connection; edits
            // owed to a page that has been replaced describe a page that no
            // longer exists, and are dropped with it.
            let still_mine = connections_
                .write()
                .unwrap()
                .get_mut(&location.webview_id)
                .is_some_and(|pages| pages.disconnect(page, connection_number, queued_message));
            tracing::debug!(
                "Edits connection {connection_number} of page {page} of webview {} ended; the \
                 table {} refers to it.",
                location.webview_id,
                if still_mine { "still" } else { "no longer" }
            );
            if !still_mine && !closed {
                tell_replaced(websocket);
            }
        });
    }

    pub(crate) fn create_queue(&self) -> WryQueue {
        let webview_id = self
            .max_webview_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.connections
            .write()
            .unwrap()
            .insert(webview_id, WebviewPages::default());
        let server = self.current_location.clone();
        let server_location = self.server_location.clone();
        WryQueue {
            inner: Rc::new(RefCell::new(WryQueueInner {
                server_location_changed: server_location.clone(),
                server_location_changed_future: owned_notify_future(server_location),
                location: WebviewWebsocketLocation { webview_id, server },
                edits_in_progress: None,
                mutation_state: MutationState::default(),
            })),
            pages: WebviewPagesHandle {
                webview: webview_id,
                connections: self.connections.clone(),
            },
        }
    }
}

/// Closes the connection of a page that has been replaced, with the code
/// that tells the page not to ask for another.
fn tell_replaced(mut websocket: tungstenite::WebSocket<TcpStream>) {
    _ = websocket.get_ref().set_read_timeout(Some(CLOSING_PATIENCE));
    let replaced = tungstenite::protocol::CloseFrame {
        code: tungstenite::protocol::frame::coding::CloseCode::from(PAGE_REPLACED),
        reason: "The page has been replaced.".into(),
    };
    if websocket.close(Some(replaced)).is_err() {
        return;
    }
    while websocket.read().is_ok() {}
}

/// The pages of one webview, held where both the virtual dom and the threads
/// serving connections can reach them.
#[derive(Clone)]
struct WebviewPagesHandle {
    webview: u32,
    connections: Arc<RwLock<HashMap<u32, WebviewPages>>>,
}

impl WebviewPagesHandle {
    fn with<O>(&self, act: impl FnOnce(&mut WebviewPages) -> O) -> O {
        let mut connections = self.connections.write().unwrap();
        act(connections
            .get_mut(&self.webview)
            .expect("A webview's pages are recorded when its queue is created."))
    }
}

/// A webview's pages: the page edits are written for, the edits waiting for
/// it to connect, and the connection of every page that may be drawn into.
#[derive(Default)]
struct WebviewPages {
    /// How many pages have been served.
    served: u64,
    /// The page edits are written for: the newest page that has reported in,
    /// unless it has since been lost.
    drawn: Option<Page>,
    /// The newest page that has reported in, whether or not it has since
    /// been lost. No page at or before it is drawn into again.
    newest_reported: Option<Page>,
    /// Edits for the drawn page, waiting for it to connect.
    pending: VecDeque<MsgPair>,
    /// The connection of the drawn page, and of any newer page that has
    /// connected before reporting in.
    connections: BTreeMap<Page, Connection>,
}

struct Connection {
    edits_outgoing: std::sync::mpsc::Sender<MsgPair>,
    number: u64,
}

impl WebviewPages {
    fn serve(&mut self) -> Page {
        self.served += 1;
        Page(self.served)
    }

    fn is_newer_than_every_report(&self, page: Page) -> bool {
        self.newest_reported.is_none_or(|newest| page > newest)
    }

    fn report_in(&mut self, page: Page) -> Arrival {
        if let Some(newest) = self.newest_reported.filter(|newest| page <= *newest) {
            return Arrival::Stale { newest };
        }
        self.drawn = Some(page);
        self.newest_reported = Some(page);
        self.pending.clear();
        self.connections = self.connections.split_off(&page);
        Arrival::Drawn
    }

    fn lose(&mut self) {
        self.drawn = None;
        self.pending.clear();
        if let Some(newest) = self.newest_reported {
            self.connections = self.connections.split_off(&Page(newest.0 + 1));
        }
    }

    fn connect(&mut self, page: Page, connection: Connection) -> Admission {
        if self.drawn == Some(page) {
            while let Some(pair) = self.pending.pop_front() {
                if let Err(std::sync::mpsc::SendError(pair)) = connection.edits_outgoing.send(pair)
                {
                    self.pending.push_front(pair);
                    break;
                }
            }
            self.connections.insert(page, connection);
            Admission::Drawn
        } else if self.is_newer_than_every_report(page) {
            self.connections.insert(page, connection);
            Admission::Held
        } else {
            Admission::Replaced
        }
    }

    /// Hands edits to the drawn page, or keeps them until it connects, and
    /// returns the wait for the page to apply them. With no page drawn into,
    /// the edits describe nothing on screen and are dropped.
    fn write(&mut self, edits: Vec<u8>) -> Option<oneshot::Receiver<()>> {
        let page = self.drawn?;
        let (response, applied) = oneshot::channel();
        self.deliver(MsgPair {
            page,
            edits,
            response,
        });
        Some(applied)
    }

    fn deliver(&mut self, pair: MsgPair) {
        if self.drawn != Some(pair.page) {
            return;
        }
        let pair = match self.connections.get(&pair.page) {
            Some(connection) => match connection.edits_outgoing.send(pair) {
                Ok(()) => return,
                Err(std::sync::mpsc::SendError(pair)) => pair,
            },
            None => pair,
        };
        self.pending.push_back(pair);
    }

    /// A connection's thread has ended. If the table still refers to it, the
    /// page is left to connect again and the edits it owed are sent first
    /// when it does; otherwise the connection was replaced, and what it owed
    /// is dropped with it.
    fn disconnect(&mut self, page: Page, number: u64, owed: Option<MsgPair>) -> bool {
        if !self
            .connections
            .get(&page)
            .is_some_and(|connection| connection.number == number)
        {
            return false;
        }
        self.connections.remove(&page);
        if let Some(owed) = owed.filter(|owed| self.drawn == Some(owed.page)) {
            self.pending.push_front(owed);
        }
        true
    }
}

struct MsgPair {
    page: Page,
    edits: Vec<u8>,
    response: oneshot::Sender<()>,
}

const KEY_SIZE: usize = 256;
type EncodedKey = [u8; KEY_SIZE];

/// Base64 encode the key to a string to be used in the websocket URL.
fn encode_key_string(key: &EncodedKey) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE, key)
}

/// Create a secure key for the websocket connection.
/// Returns the key as a byte array and a hex-encoded string representation of the key.
fn create_secure_key() -> EncodedKey {
    // Helper function to assert that the RNG is a CryptoRng - make sure we use a secure RNG
    fn assert_crypto_random<R: rand::CryptoRng>(val: R) -> R {
        val
    }

    let mut secure_rng = assert_crypto_random(rand::rngs::StdRng::from_os_rng());
    let mut expected_key: EncodedKey = [0u8; KEY_SIZE];
    secure_rng.fill_bytes(&mut expected_key);
    expected_key
}

#[cfg(test)]
mod page_tests {
    use super::*;
    use std::sync::mpsc::{Receiver, TryRecvError, channel};

    fn connection(number: u64) -> (Connection, Receiver<MsgPair>) {
        let (edits_outgoing, incoming) = channel();
        (
            Connection {
                edits_outgoing,
                number,
            },
            incoming,
        )
    }

    fn heard(incoming: &Receiver<MsgPair>) -> Vec<Vec<u8>> {
        incoming.try_iter().map(|pair| pair.edits).collect()
    }

    fn owed(page: Page, edits: &[u8]) -> MsgPair {
        MsgPair {
            page,
            edits: edits.to_vec(),
            response: oneshot::channel().0,
        }
    }

    #[test]
    fn pages_are_numbered_from_one_in_the_order_they_are_served() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.serve(), Page(1));
        assert_eq!(pages.serve(), Page(2));
        assert_eq!(pages.serve(), Page(3));
        assert!(Page(2) < Page(3));
        assert_eq!(Page(3).to_string(), "3");
        assert_eq!("3".parse::<Page>(), Ok(Page(3)));
        assert!("".parse::<Page>().is_err());
        assert!("three".parse::<Page>().is_err());
        assert!("-3".parse::<Page>().is_err());
    }

    #[test]
    fn only_a_page_newer_than_every_report_is_drawn_into() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(2)), Arrival::Drawn);
        assert_eq!(pages.drawn, Some(Page(2)));
        assert_eq!(
            pages.report_in(Page(2)),
            Arrival::Stale { newest: Page(2) },
            "A page that has reported in is not rebuilt into a second time."
        );
        assert_eq!(
            pages.report_in(Page(1)),
            Arrival::Stale { newest: Page(2) },
            "A page served before one that has reported in is not drawn into."
        );
        assert_eq!(pages.drawn, Some(Page(2)));
        assert_eq!(pages.report_in(Page(3)), Arrival::Drawn);
        assert_eq!(pages.drawn, Some(Page(3)));
    }

    #[test]
    fn a_lost_page_is_never_drawn_into_again_but_the_page_after_it_is() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        pages.lose();
        assert_eq!(pages.drawn, None);
        assert_eq!(pages.report_in(Page(1)), Arrival::Stale { newest: Page(1) });
        assert_eq!(pages.drawn, None);
        assert_eq!(pages.report_in(Page(2)), Arrival::Drawn);
        assert_eq!(pages.drawn, Some(Page(2)));
    }

    #[test]
    fn edits_written_with_no_page_drawn_into_are_dropped() {
        let mut pages = WebviewPages::default();
        assert!(pages.write(b"before any page".to_vec()).is_none());
        assert!(pages.pending.is_empty());

        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        assert!(pages.write(b"for the page".to_vec()).is_some());
        pages.lose();
        assert!(
            pages.pending.is_empty(),
            "What waited for the lost page was dropped with it."
        );
        assert!(pages.write(b"after the loss".to_vec()).is_none());
        assert!(pages.pending.is_empty());
    }

    #[test]
    fn edits_for_the_drawn_page_wait_for_its_connection_and_arrive_in_order() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        assert!(pages.write(b"rebuild".to_vec()).is_some());
        assert!(pages.write(b"diff".to_vec()).is_some());
        assert_eq!(pages.pending.len(), 2);

        let (page_one, incoming) = connection(0);
        assert_eq!(pages.connect(Page(1), page_one), Admission::Drawn);
        assert!(pages.pending.is_empty());
        assert_eq!(heard(&incoming), [b"rebuild".to_vec(), b"diff".to_vec()]);

        assert!(pages.write(b"later".to_vec()).is_some());
        assert!(pages.pending.is_empty());
        assert_eq!(heard(&incoming), [b"later".to_vec()]);
    }

    #[test]
    fn a_page_that_connects_before_reporting_in_is_held_and_sent_nothing_until_it_does() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        let (page_one, first) = connection(0);
        assert_eq!(pages.connect(Page(1), page_one), Admission::Drawn);
        let (page_two, second) = connection(1);
        assert_eq!(pages.connect(Page(2), page_two), Admission::Held);

        assert!(pages.write(b"for page one".to_vec()).is_some());
        assert_eq!(heard(&first), [b"for page one".to_vec()]);
        assert!(heard(&second).is_empty());

        assert_eq!(pages.report_in(Page(2)), Arrival::Drawn);
        assert_eq!(
            first.try_recv().err(),
            Some(TryRecvError::Disconnected),
            "The replaced page's connection is let go once its replacement reports in."
        );
        assert!(pages.write(b"for page two".to_vec()).is_some());
        assert_eq!(heard(&second), [b"for page two".to_vec()]);
    }

    #[test]
    fn edits_waiting_for_a_page_are_never_handed_to_the_page_that_replaces_it() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        assert!(pages.write(b"for page one".to_vec()).is_some());
        assert_eq!(pages.pending.len(), 1);

        assert_eq!(pages.report_in(Page(2)), Arrival::Drawn);
        let (page_two, incoming) = connection(0);
        assert_eq!(pages.connect(Page(2), page_two), Admission::Drawn);
        assert!(pages.write(b"rebuild".to_vec()).is_some());
        assert_eq!(heard(&incoming), [b"rebuild".to_vec()]);
    }

    #[test]
    fn a_page_connecting_before_any_page_has_reported_in_is_held() {
        let mut pages = WebviewPages::default();
        let (page_one, incoming) = connection(0);
        assert_eq!(pages.connect(Page(1), page_one), Admission::Held);
        assert!(pages.write(b"before the page".to_vec()).is_none());
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        assert!(pages.write(b"rebuild".to_vec()).is_some());
        assert_eq!(heard(&incoming), [b"rebuild".to_vec()]);
    }

    #[test]
    fn a_page_that_has_been_replaced_is_refused_a_connection() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        assert_eq!(pages.report_in(Page(2)), Arrival::Drawn);
        let (page_one, incoming) = connection(0);
        assert_eq!(pages.connect(Page(1), page_one), Admission::Replaced);
        assert!(!pages.connections.contains_key(&Page(1)));
        assert!(pages.write(b"for page two".to_vec()).is_some());
        assert!(
            heard(&incoming).is_empty(),
            "The replaced page was handed what was meant for its replacement."
        );
        assert_eq!(pages.pending.len(), 1);
    }

    #[test]
    fn a_lost_page_and_every_page_before_it_are_refused_and_the_next_is_held() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(2)), Arrival::Drawn);
        pages.lose();
        let (page_one, _) = connection(0);
        assert_eq!(pages.connect(Page(1), page_one), Admission::Replaced);
        let (page_two, _) = connection(1);
        assert_eq!(
            pages.connect(Page(2), page_two),
            Admission::Replaced,
            "The page that was lost cannot take the connection of the page replacing it."
        );
        let (page_three, _) = connection(2);
        assert_eq!(pages.connect(Page(3), page_three), Admission::Held);
    }

    #[test]
    fn losing_the_page_lets_its_connection_go_and_keeps_that_of_a_page_served_after_it() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        let (page_one, first) = connection(0);
        assert_eq!(pages.connect(Page(1), page_one), Admission::Drawn);
        let (page_two, second) = connection(1);
        assert_eq!(pages.connect(Page(2), page_two), Admission::Held);

        pages.lose();
        assert_eq!(first.try_recv().err(), Some(TryRecvError::Disconnected));
        assert_eq!(second.try_recv().err(), Some(TryRecvError::Empty));

        assert_eq!(pages.report_in(Page(2)), Arrival::Drawn);
        assert!(pages.write(b"rebuild".to_vec()).is_some());
        assert_eq!(heard(&second), [b"rebuild".to_vec()]);
    }

    #[test]
    fn losing_a_page_before_any_has_reported_in_keeps_every_connection() {
        let mut pages = WebviewPages::default();
        let (page_one, incoming) = connection(0);
        assert_eq!(pages.connect(Page(1), page_one), Admission::Held);
        pages.lose();
        assert_eq!(incoming.try_recv().err(), Some(TryRecvError::Empty));
        assert!(pages.connections.contains_key(&Page(1)));
    }

    #[test]
    fn edits_handed_to_a_connection_whose_thread_has_gone_wait_for_the_next() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        let (gone, incoming) = connection(0);
        assert_eq!(pages.connect(Page(1), gone), Admission::Drawn);
        drop(incoming);

        assert!(pages.write(b"rebuild".to_vec()).is_some());
        assert_eq!(pages.pending.len(), 1);

        let (again, incoming) = connection(1);
        assert_eq!(pages.connect(Page(1), again), Admission::Drawn);
        assert_eq!(heard(&incoming), [b"rebuild".to_vec()]);
    }

    #[test]
    fn edits_waiting_for_a_connection_whose_thread_has_gone_stay_waiting_in_order() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        assert!(pages.write(b"rebuild".to_vec()).is_some());
        assert!(pages.write(b"diff".to_vec()).is_some());
        let (gone, incoming) = connection(0);
        drop(incoming);
        assert_eq!(pages.connect(Page(1), gone), Admission::Drawn);
        assert_eq!(
            pages
                .pending
                .iter()
                .map(|pair| pair.edits.clone())
                .collect::<Vec<_>>(),
            [b"rebuild".to_vec(), b"diff".to_vec()]
        );
    }

    #[test]
    fn a_connection_that_ends_while_still_in_use_gives_back_what_it_owed_first() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        let (page_one, _incoming) = connection(7);
        assert_eq!(pages.connect(Page(1), page_one), Admission::Drawn);

        assert!(pages.disconnect(Page(1), 7, Some(owed(Page(1), b"owed"))));
        assert!(!pages.connections.contains_key(&Page(1)));
        assert!(pages.write(b"after".to_vec()).is_some());

        let (again, incoming) = connection(8);
        assert_eq!(pages.connect(Page(1), again), Admission::Drawn);
        assert_eq!(heard(&incoming), [b"owed".to_vec(), b"after".to_vec()]);
    }

    #[test]
    fn a_connection_already_replaced_gives_back_nothing() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        let (first, _first) = connection(7);
        assert_eq!(pages.connect(Page(1), first), Admission::Drawn);
        let (second, incoming) = connection(8);
        assert_eq!(pages.connect(Page(1), second), Admission::Drawn);

        assert!(!pages.disconnect(Page(1), 7, Some(owed(Page(1), b"owed"))));
        assert!(pages.pending.is_empty());
        assert!(
            pages.connections.contains_key(&Page(1)),
            "The connection that replaced it was wiped from the table."
        );
        assert!(pages.write(b"next".to_vec()).is_some());
        assert_eq!(heard(&incoming), [b"next".to_vec()]);
    }

    #[test]
    fn edits_owed_by_a_page_no_longer_drawn_into_are_dropped() {
        let mut pages = WebviewPages::default();
        let (held, _held) = connection(3);
        assert_eq!(pages.connect(Page(2), held), Admission::Held);
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);

        assert!(pages.disconnect(Page(2), 3, Some(owed(Page(1), b"owed"))));
        assert!(!pages.connections.contains_key(&Page(2)));
        assert_eq!(
            pages.pending.len(),
            1,
            "Edits owed to the drawn page are kept even when another page's connection gave them back."
        );

        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(2)), Arrival::Drawn);
        let (held, _held) = connection(4);
        assert_eq!(pages.connect(Page(3), held), Admission::Held);
        assert!(pages.disconnect(Page(3), 4, Some(owed(Page(1), b"owed"))));
        assert!(pages.pending.is_empty());
    }

    #[test]
    fn a_connection_that_ends_with_nothing_owed_leaves_nothing_waiting() {
        let mut pages = WebviewPages::default();
        assert_eq!(pages.report_in(Page(1)), Arrival::Drawn);
        let (page_one, _incoming) = connection(5);
        assert_eq!(pages.connect(Page(1), page_one), Admission::Drawn);
        assert!(pages.disconnect(Page(1), 5, None));
        assert!(pages.pending.is_empty());
        assert!(!pages.disconnect(Page(1), 5, None));
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    use tungstenite::protocol::frame::coding::CloseCode;

    const HUNG: Duration = Duration::from_secs(60);

    fn connect(
        server: &EditWebsocket,
        queue: &WryQueue,
        page: Page,
    ) -> tungstenite::WebSocket<TcpStream> {
        let stream = TcpStream::connect((
            IpAddr::from([127, 0, 0, 1]),
            server.current_location.lock().unwrap().port,
        ))
        .unwrap();
        stream.set_read_timeout(Some(HUNG)).unwrap();
        let (mut websocket, _) =
            tungstenite::client(format!("{}/{page}", queue.edits_path()), stream).unwrap();
        let greeting = websocket.read().unwrap();
        assert_eq!(greeting.into_text().unwrap(), queue.required_server_key());
        websocket
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Heard {
        Edits(Vec<u8>),
        Replaced,
    }

    fn hear(websocket: &mut tungstenite::WebSocket<TcpStream>) -> Heard {
        loop {
            match websocket.read() {
                Ok(tungstenite::Message::Binary(edits)) => return Heard::Edits(edits.to_vec()),
                Ok(tungstenite::Message::Close(Some(frame)))
                    if frame.code == CloseCode::from(PAGE_REPLACED) =>
                {
                    return Heard::Replaced;
                }
                Ok(tungstenite::Message::Close(frame)) => {
                    panic!("The page's connection was closed without being told why: {frame:?}")
                }
                Ok(_) => {}
                Err(error) => panic!("The page's connection ended without a word: {error}"),
            }
        }
    }

    fn acknowledge(websocket: &mut tungstenite::WebSocket<TcpStream>) {
        websocket
            .send(tungstenite::Message::Binary(Vec::new().into()))
            .unwrap();
    }

    fn write(queue: &WryQueue, edits: &[u8]) -> oneshot::Receiver<()> {
        queue
            .pages
            .with(|pages| pages.write(edits.to_vec()))
            .expect("A page is drawn into.")
    }

    fn applied(receiver: oneshot::Receiver<()>) -> Result<(), oneshot::Canceled> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(receiver)
    }

    #[derive(Debug, Clone, Copy)]
    enum AskedAgain {
        BeforeTheNewPageConnects,
        BeforeTheNewPageReportsIn,
        AfterTheNewPageReportsIn,
    }

    #[test]
    fn a_replaced_page_that_asks_again_is_refused_and_never_takes_the_new_pages_edits() {
        for asked_again in [
            AskedAgain::BeforeTheNewPageConnects,
            AskedAgain::BeforeTheNewPageReportsIn,
            AskedAgain::AfterTheNewPageReportsIn,
        ] {
            let server = EditWebsocket::start();
            let queue = server.create_queue();

            let first = queue.serve_page();
            let mut old = connect(&server, &queue, first);
            assert_eq!(queue.page_arrived(first), Arrival::Drawn);
            let first_rebuild = write(&queue, b"first rebuild");
            assert_eq!(hear(&mut old), Heard::Edits(b"first rebuild".to_vec()));
            acknowledge(&mut old);
            assert_eq!(applied(first_rebuild), Ok(()));

            queue.forget_connection();
            assert_eq!(
                hear(&mut old),
                Heard::Replaced,
                "{asked_again:?}: the lost page was not told it has been replaced."
            );

            let second = queue.serve_page();
            let mut refused = Vec::new();
            if let AskedAgain::BeforeTheNewPageConnects = asked_again {
                refused.push(connect(&server, &queue, first));
            }
            let mut new = connect(&server, &queue, second);
            if let AskedAgain::BeforeTheNewPageReportsIn = asked_again {
                refused.push(connect(&server, &queue, first));
            }
            assert_eq!(queue.page_arrived(second), Arrival::Drawn);
            let second_rebuild = write(&queue, b"second rebuild");
            if let AskedAgain::AfterTheNewPageReportsIn = asked_again {
                refused.push(connect(&server, &queue, first));
            }
            assert_eq!(queue.page_arrived(first), Arrival::Stale { newest: second });

            assert_eq!(
                hear(&mut new),
                Heard::Edits(b"second rebuild".to_vec()),
                "{asked_again:?}: the new page's first edits were not its rebuild."
            );
            acknowledge(&mut new);
            assert_eq!(applied(second_rebuild), Ok(()));
            let diff = write(&queue, b"diff");
            assert_eq!(hear(&mut new), Heard::Edits(b"diff".to_vec()));
            acknowledge(&mut new);
            assert_eq!(applied(diff), Ok(()));

            assert_eq!(refused.len(), 1);
            for mut old in refused {
                assert_eq!(
                    hear(&mut old),
                    Heard::Replaced,
                    "{asked_again:?}: the replaced page was handed edits."
                );
            }
        }
    }

    #[test]
    fn a_page_that_replaces_another_keeps_its_connection() {
        let server = EditWebsocket::start();
        let queue = server.create_queue();

        // The first page never closes its connection: its process was
        // terminated, or a second load of the page overtook it.
        let first = queue.serve_page();
        let mut old = connect(&server, &queue, first);
        assert_eq!(queue.page_arrived(first), Arrival::Drawn);
        let second = queue.serve_page();
        let mut new = connect(&server, &queue, second);
        assert_eq!(queue.page_arrived(second), Arrival::Drawn);

        // The first connection's thread has let go of the table by the time
        // it tells its page it was replaced.
        assert_eq!(hear(&mut old), Heard::Replaced);

        let rebuild = write(&queue, b"rebuild");
        assert_eq!(hear(&mut new), Heard::Edits(b"rebuild".to_vec()));
        acknowledge(&mut new);
        assert_eq!(
            applied(rebuild),
            Ok(()),
            "The new page's acknowledgement never reached the virtual dom."
        );
    }

    #[test]
    fn a_page_that_closes_its_own_connection_is_sent_what_it_owed_when_it_connects_again() {
        let server = EditWebsocket::start();
        let queue = server.create_queue();
        let page = queue.serve_page();
        assert_eq!(queue.page_arrived(page), Arrival::Drawn);

        let mut only = connect(&server, &queue, page);
        let _owed = write(&queue, b"owed");
        assert_eq!(hear(&mut only), Heard::Edits(b"owed".to_vec()));

        // The page closes before applying them, as one whose socket the system
        // ended does; the server has handed them back by the time the
        // connection is gone.
        only.close(None).unwrap();
        while only.read().is_ok() {}

        let mut again = connect(&server, &queue, page);
        assert_eq!(hear(&mut again), Heard::Edits(b"owed".to_vec()));
    }

    #[test]
    fn a_page_lost_before_its_replacement_has_connected_leaves_the_replacement_its_connection() {
        let server = EditWebsocket::start();
        let queue = server.create_queue();
        let first = queue.serve_page();
        let mut old = connect(&server, &queue, first);
        assert_eq!(queue.page_arrived(first), Arrival::Drawn);

        let second = queue.serve_page();
        let mut new = connect(&server, &queue, second);
        queue.forget_connection();
        assert_eq!(hear(&mut old), Heard::Replaced);

        assert_eq!(queue.page_arrived(second), Arrival::Drawn);
        let rebuild = write(&queue, b"rebuild");
        assert_eq!(hear(&mut new), Heard::Edits(b"rebuild".to_vec()));
        acknowledge(&mut new);
        assert_eq!(applied(rebuild), Ok(()));
    }

    #[test]
    fn a_connection_that_does_not_name_its_page_is_refused_at_the_handshake() {
        let server = EditWebsocket::start();
        let queue = server.create_queue();
        for path in [
            queue.edits_path(),
            format!("{}/", queue.edits_path()),
            format!("{}/page", queue.edits_path()),
            format!("{}/-1", queue.edits_path()),
        ] {
            let stream = TcpStream::connect((
                IpAddr::from([127, 0, 0, 1]),
                server.current_location.lock().unwrap().port,
            ))
            .unwrap();
            match tungstenite::client(path.as_str(), stream) {
                Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))) => {
                    assert_eq!(response.status(), 400, "{path}")
                }
                Err(error) => panic!("{path}: {error}"),
                Ok(_) => panic!("{path}: a connection naming no page was accepted."),
            }
        }
    }

    #[test]
    fn a_connection_for_a_webview_that_does_not_exist_is_refused() {
        let server = EditWebsocket::start();
        let queue = server.create_queue();
        let stream = TcpStream::connect((
            IpAddr::from([127, 0, 0, 1]),
            server.current_location.lock().unwrap().port,
        ))
        .unwrap();
        stream.set_read_timeout(Some(HUNG)).unwrap();
        let path = queue.edits_path().replacen("/0/", "/7/", 1);
        let (mut stranger, _) = tungstenite::client(format!("{path}/1"), stream).unwrap();
        assert_eq!(
            stranger.read().unwrap().into_text().unwrap(),
            queue.required_server_key()
        );
        assert_eq!(hear(&mut stranger), Heard::Replaced);
    }

    #[test]
    fn edits_rendered_while_no_page_is_drawn_into_are_dropped_and_the_rebuild_starts_the_templates_over()
     {
        const TEMPLATE: dioxus_core::Template = dioxus_core::Template::new(
            &[dioxus_core::TemplateNode::Text { text: "kept" }],
            &[],
            &[],
        );

        let server = EditWebsocket::start();
        let queue = server.create_queue();
        let first = queue.serve_page();
        assert_eq!(queue.page_arrived(first), Arrival::Drawn);

        // A first page had edits written for it.
        queue.with_mutation_state_mut(|state| {
            dioxus_core::WriteMutations::create_text_node(state, "gone", dioxus_core::ElementId(1))
        });
        queue.forget_connection();

        // The virtual dom renders while the page is awaited: a diff against the
        // page that is gone. Nothing of it may reach the table.
        queue.with_mutation_state_mut(|state| {
            dioxus_core::WriteMutations::load_template(
                state,
                TEMPLATE,
                0,
                dioxus_core::ElementId(2),
            )
        });
        queue.send_edits();
        assert!(queue.pages.with(|pages| pages.pending.is_empty()));
        assert!(queue.inner.borrow().edits_in_progress.is_none());

        // The new page reports in: the templates start over and the rebuild
        // is the first batch the page will receive.
        let second = queue.serve_page();
        assert_eq!(queue.page_arrived(second), Arrival::Drawn);
        let mut fresh = MutationState::default();
        assert_eq!(
            queue
                .with_mutation_state_mut(|state| state.export_memory())
                .len(),
            fresh.export_memory().len(),
            "The new page's edits state carries something of the old page's."
        );
        queue.with_mutation_state_mut(|state| {
            dioxus_core::WriteMutations::load_template(
                state,
                TEMPLATE,
                0,
                dioxus_core::ElementId(2),
            )
        });
        dioxus_core::WriteMutations::load_template(
            &mut fresh,
            TEMPLATE,
            0,
            dioxus_core::ElementId(2),
        );
        assert_eq!(
            queue
                .with_mutation_state_mut(|state| state.export_memory())
                .len(),
            fresh.export_memory().len(),
            "The new page was not sent a template rendered while it was awaited."
        );
        queue.send_edits();
        assert_eq!(
            queue.pages.with(|pages| pages
                .pending
                .iter()
                .map(|pair| pair.page)
                .collect::<Vec<_>>()),
            [second]
        );
        assert!(queue.inner.borrow().edits_in_progress.is_some());
    }

    #[test]
    fn a_page_that_reports_in_again_keeps_the_edits_state_of_its_first_report() {
        let server = EditWebsocket::start();
        let queue = server.create_queue();
        let page = queue.serve_page();
        assert_eq!(queue.page_arrived(page), Arrival::Drawn);
        queue.with_mutation_state_mut(|state| {
            dioxus_core::WriteMutations::create_text_node(
                state,
                "rebuild",
                dioxus_core::ElementId(1),
            )
        });
        queue.send_edits();
        assert!(queue.inner.borrow().edits_in_progress.is_some());
        queue.with_mutation_state_mut(|state| {
            dioxus_core::WriteMutations::create_text_node(state, "diff", dioxus_core::ElementId(2))
        });

        assert_eq!(queue.page_arrived(page), Arrival::Stale { newest: page });
        assert!(
            queue.inner.borrow().edits_in_progress.is_some(),
            "A second report let go of the wait for the page's rebuild."
        );
        assert!(
            queue
                .with_mutation_state_mut(|state| state.export_memory())
                .len()
                > MutationState::default().export_memory().len(),
            "A second report threw away edits rendered for the page."
        );
    }
}

#[test]
fn test_key_encoding_length() {
    let mut rand = rand::rngs::StdRng::from_os_rng();
    for _ in 0..100 {
        let mut key: EncodedKey = [0u8; KEY_SIZE];
        rand.fill_bytes(&mut key);
        let encoded = encode_key_string(&key);
        // The encoded key length should be the same regardless of the value of the key
        assert_eq!(encoded.len(), 344);
    }
}

// Take an Arc<Notify> and create a future that waits for the notify to be triggered.
fn owned_notify_future(notify: Arc<Notify>) -> Pin<Box<dyn Future<Output = ()>>> {
    let mut notify_owned = Box::pin(async move {
        let notified = notify.notified();

        // The future should be after this statement once it is polled bellow
        tokio::task::yield_now().await;
        notified.await;
    });

    // Start tracking notify before the output future is polled
    _ = (&mut notify_owned).now_or_never();
    notify_owned
}

#[cfg(test)]
mod in_frame_tests {
    use super::*;

    fn queue() -> WryQueue {
        EditWebsocket::start().create_queue()
    }

    fn drawing() -> WryQueue {
        let queue = queue();
        let page = queue.serve_page();
        assert_eq!(queue.page_arrived(page), Arrival::Drawn);
        queue
    }

    #[test]
    fn a_queue_with_no_page_drawn_into_is_busy() {
        assert!(!queue().is_idle());
    }

    #[test]
    fn a_queue_drawing_a_page_with_nothing_sent_is_idle() {
        assert!(drawing().is_idle());
    }

    #[test]
    fn edits_on_their_way_to_the_page_keep_the_queue_busy() {
        let queue = drawing();
        let (sender, receiver) = oneshot::channel();
        queue.inner.borrow_mut().edits_in_progress = Some(receiver);
        assert!(!queue.is_idle());
        assert!(!queue.is_idle(), "Asking twice does not settle anything.");
        sender.send(()).unwrap();
        assert!(queue.is_idle());
        assert!(queue.is_idle(), "Once applied, the edits stay applied.");
    }

    #[test]
    fn edits_whose_page_went_away_keep_nothing_busy() {
        let queue = drawing();
        let (sender, receiver) = oneshot::channel::<()>();
        queue.inner.borrow_mut().edits_in_progress = Some(receiver);
        drop(sender);
        assert!(queue.is_idle());
    }

    #[test]
    fn edits_sent_to_the_page_keep_the_queue_busy_until_it_answers() {
        let queue = drawing();
        queue.send_edits();
        assert!(!queue.is_idle());
    }

    #[test]
    fn edits_rendered_with_no_page_drawn_into_keep_nothing_waiting() {
        let queue = queue();
        queue.send_edits();
        assert!(queue.inner.borrow().edits_in_progress.is_none());
    }

    #[test]
    fn a_page_being_replaced_keeps_the_queue_busy_until_its_replacement_arrives() {
        let queue = queue();
        let first = queue.serve_page();
        assert_eq!(queue.page_arrived(first), Arrival::Drawn);
        queue.forget_connection();
        assert!(!queue.is_idle());
        assert_eq!(queue.page_arrived(first), Arrival::Stale { newest: first });
        assert!(
            !queue.is_idle(),
            "The lost page's late report brought it back."
        );
        let second = queue.serve_page();
        assert_eq!(queue.page_arrived(second), Arrival::Drawn);
        assert!(queue.is_idle());
    }

    #[test]
    fn only_the_page_drawn_into_is_drawn() {
        let queue = queue();
        let first = queue.serve_page();
        let second = queue.serve_page();
        assert!(!queue.is_drawn(first));
        assert_eq!(queue.page_arrived(first), Arrival::Drawn);
        assert!(queue.is_drawn(first));
        assert!(!queue.is_drawn(second));
        assert_eq!(queue.page_arrived(second), Arrival::Drawn);
        assert!(!queue.is_drawn(first));
        assert!(queue.is_drawn(second));
        queue.forget_connection();
        assert!(!queue.is_drawn(first));
        assert!(!queue.is_drawn(second));
    }

    #[test]
    fn what_was_rendered_is_taken_once() {
        let queue = queue();
        queue.with_mutation_state_mut(|state| {
            dioxus_core::WriteMutations::create_placeholder(state, dioxus_core::ElementId(1));
        });
        let first = queue.take_rendered();
        assert!(!first.is_empty());
        let second = queue.take_rendered();
        assert_ne!(first, second, "What was taken is not taken again.");
    }

    #[test]
    fn the_interpreter_names_its_page_where_the_server_reads_it() {
        let native = dioxus_interpreter_js::NATIVE_JS;
        assert!(native.contains(&format!("\"{PAGE_HEADER}\"")));
        assert!(native.contains(&PAGE_REPLACED.to_string()));
        assert!(native.contains("${editsPath}/${this.page}"));
    }
}
