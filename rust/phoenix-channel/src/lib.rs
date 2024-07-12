mod heartbeat;
mod login_url;

use std::collections::{HashSet, VecDeque};
use std::mem;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::{fmt, future, marker::PhantomData};

use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;
use base64::Engine;
use futures::future::BoxFuture;
use futures::{FutureExt, SinkExt, StreamExt};
use heartbeat::{Heartbeat, MissedLastHeartbeat};
use rand_core::{OsRng, RngCore};
use secrecy::{ExposeSecret as _, Secret};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use socket_factory::SocketFactory;
use std::task::{Context, Poll, Waker};
use tokio::net::TcpStream;
use tokio_tungstenite::client_async_tls;
use tokio_tungstenite::tungstenite::handshake::client::Response;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::{
    tungstenite::{handshake::client::Request, Message},
    MaybeTlsStream, WebSocketStream,
};
use url::{Host, Url};

pub use login_url::{LoginUrl, LoginUrlError};

pub struct PhoenixChannel<TInitReq, TInboundMsg, TOutboundRes> {
    state: State,
    waker: Option<Waker>,
    pending_messages: VecDeque<String>,
    next_request_id: Arc<AtomicU64>,
    socket_factory: Arc<dyn SocketFactory<tokio::net::TcpSocket>>,

    heartbeat: Heartbeat,

    _phantom: PhantomData<(TInboundMsg, TOutboundRes)>,

    pending_join_requests: HashSet<OutboundRequestId>,

    // Stored here to allow re-connecting.
    url: Secret<LoginUrl>,
    user_agent: String,
    reconnect_backoff: ExponentialBackoff,

    login: &'static str,
    init_req: TInitReq,
}

enum State {
    Connected(WebSocketStream<MaybeTlsStream<TcpStream>>),
    Connecting(
        BoxFuture<'static, Result<WebSocketStream<MaybeTlsStream<TcpStream>>, InternalError>>,
    ),
    Closing(WebSocketStream<MaybeTlsStream<TcpStream>>),
    Closed,
}

async fn connect_websocket(
    request: Request,
    socket: tokio::net::TcpStream,
) -> Result<
    (WebSocketStream<MaybeTlsStream<TcpStream>>, Response),
    tokio_tungstenite::tungstenite::Error,
> {
    client_async_tls(request, socket).await
}

async fn make_socket(
    url: &Url,
    socket_factory: &dyn SocketFactory<tokio::net::TcpSocket>,
) -> Result<TcpStream, InternalError> {
    let port = url
        .port_or_known_default()
        .expect("scheme to be http, https, ws or wss");
    let addrs: Vec<SocketAddr> = match url.host().ok_or(InternalError::InvalidUrl)? {
        Host::Domain(n) => tokio::net::lookup_host((n, port))
            .await
            .map_err(|_| InternalError::InvalidUrl)?
            .collect(),
        Host::Ipv6(ip) => {
            vec![(ip, port).into()]
        }
        Host::Ipv4(ip) => {
            vec![(ip, port).into()]
        }
    };

    let mut last_error = None;
    for addr in addrs {
        let Ok(socket) = socket_factory(&addr) else {
            continue;
        };

        match socket.connect(addr).await {
            Ok(socket) => return Ok(socket),
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    let Some(err) = last_error else {
        return Err(InternalError::InvalidUrl);
    };

    Err(InternalError::SocketConnection(err))
}

impl State {
    fn connect(
        url: Secret<LoginUrl>,
        user_agent: String,
        socket_factory: Arc<dyn SocketFactory<tokio::net::TcpSocket>>,
    ) -> Self {
        Self::Connecting(Box::pin(async move {
            let socket = make_socket(url.expose_secret().inner(), &*socket_factory).await?;

            let (stream, _) = connect_websocket(make_request(url, user_agent), socket)
                .await
                .map_err(InternalError::WebSocket)?;

            Ok(stream)
        }))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("client error: {0}")]
    Client(StatusCode),
    #[error("token expired")]
    TokenExpired,
    #[error("max retries reached")]
    MaxRetriesReached,
    #[error("login failed: {0}")]
    LoginFailed(ErrorReply),
}

impl Error {
    pub fn is_authentication_error(&self) -> bool {
        match self {
            Error::Client(s) => s == &StatusCode::UNAUTHORIZED || s == &StatusCode::FORBIDDEN,
            Error::TokenExpired => true,
            Error::MaxRetriesReached => false,
            Error::LoginFailed(_) => false,
        }
    }
}

enum InternalError {
    WebSocket(tokio_tungstenite::tungstenite::Error),
    Serde(serde_json::Error),
    MissedHeartbeat,
    CloseMessage,
    StreamClosed,
    InvalidUrl,
    SocketConnection(std::io::Error),
}

impl fmt::Display for InternalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InternalError::WebSocket(tokio_tungstenite::tungstenite::Error::Http(http)) => {
                let status = http.status();
                let body = http
                    .body()
                    .as_deref()
                    .map(String::from_utf8_lossy)
                    .unwrap_or_default();

                write!(f, "http error: {status} - {body}")
            }
            InternalError::WebSocket(e) => write!(f, "websocket connection failed: {e}"),
            InternalError::Serde(e) => write!(f, "failed to deserialize message: {e}"),
            InternalError::MissedHeartbeat => write!(f, "portal did not respond to our heartbeat"),
            InternalError::CloseMessage => write!(f, "portal closed the websocket connection"),
            InternalError::StreamClosed => write!(f, "websocket stream was closed"),
            InternalError::InvalidUrl => write!(f, "failed to resolve url"),
            InternalError::SocketConnection(e) => write!(f, "failed to connect socket: {e}"),
        }
    }
}

/// A strict-monotonically increasing ID for outbound requests.
#[derive(Debug, PartialEq, Eq, Hash, Deserialize, Serialize, PartialOrd, Ord)]
pub struct OutboundRequestId(u64);

impl OutboundRequestId {
    // Should only be used for unit-testing.
    pub fn for_test(id: u64) -> Self {
        Self(id)
    }

    /// Internal function to make a copy.
    ///
    /// Not exposed publicly because these IDs are meant to be unique.
    pub(crate) fn copy(&self) -> Self {
        Self(self.0)
    }
}

impl fmt::Display for OutboundRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OutReq-{}", self.0)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Cannot close websocket while we are connecting")]
pub struct Connecting;

impl<TInitReq, TInboundMsg, TOutboundRes> PhoenixChannel<TInitReq, TInboundMsg, TOutboundRes>
where
    TInitReq: Serialize + Clone,
    TInboundMsg: DeserializeOwned,
    TOutboundRes: DeserializeOwned,
{
    /// Creates a new [PhoenixChannel] to the given endpoint.
    ///
    /// This version of the function accept a Protect struct that will be used to protect the socket and prevent routing loops.
    ///
    /// The provided URL must contain a host.
    /// Additionally, you must already provide any query parameters required for authentication.
    pub fn connect(
        url: Secret<LoginUrl>,
        user_agent: String,
        login: &'static str,
        init_req: TInitReq,
        reconnect_backoff: ExponentialBackoff,
        socket_factory: Arc<dyn SocketFactory<tokio::net::TcpSocket>>,
    ) -> Self {
        let next_request_id = Arc::new(AtomicU64::new(0));

        tracing::debug!(host = %url.expose_secret().host(), %user_agent, "Connecting to portal");

        Self {
            reconnect_backoff,
            url: url.clone(),
            user_agent: user_agent.clone(),
            state: State::connect(url, user_agent, socket_factory.clone()),
            socket_factory,
            waker: None,
            pending_messages: Default::default(),
            _phantom: PhantomData,
            heartbeat: Heartbeat::new(
                heartbeat::INTERVAL,
                heartbeat::TIMEOUT,
                next_request_id.clone(),
            ),
            next_request_id,
            pending_join_requests: Default::default(),
            login,
            init_req,
        }
    }

    /// Join the provided room.
    ///
    /// If successful, a [`Event::JoinedRoom`] event will be emitted.
    pub fn join(&mut self, topic: impl Into<String>, payload: impl Serialize) {
        let (request_id, msg) = self.make_message(topic, EgressControlMessage::PhxJoin(payload));
        self.pending_messages.push_front(msg); // Must send the join message before all others.

        self.pending_join_requests.insert(request_id);
    }

    /// Send a message to a topic.
    pub fn send(&mut self, topic: impl Into<String>, message: impl Serialize) -> OutboundRequestId {
        let (id, msg) = self.make_message(topic, message);
        self.pending_messages.push_back(msg);

        id
    }

    /// Reconnects to the portal.
    pub fn reconnect(&mut self) {
        // 1. Reset the backoff.
        self.reconnect_backoff.reset();

        // 2. Set state to `Connecting` without a timer.
        let url = self.url.clone();
        let user_agent = self.user_agent.clone();
        self.state = State::connect(url, user_agent, self.socket_factory.clone());

        // 3. In case we were already re-connecting, we need to wake the suspended task.
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    /// Initiate a graceful close of the connection.
    pub fn close(&mut self) -> Result<(), Connecting> {
        tracing::info!("Closing connection to portal");

        match mem::replace(&mut self.state, State::Closed) {
            State::Connecting(_) => return Err(Connecting),
            State::Closing(stream) | State::Connected(stream) => {
                self.state = State::Closing(stream);
            }
            State::Closed => {}
        }

        Ok(())
    }

    pub fn poll(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<Event<TInboundMsg, TOutboundRes>, Error>> {
        loop {
            // First, check if we are connected.
            let stream = match &mut self.state {
                State::Closed => return Poll::Ready(Ok(Event::Closed)),
                State::Closing(stream) => match stream.poll_close_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        tracing::info!("Closed websocket connection to portal");

                        self.state = State::Closed;

                        return Poll::Ready(Ok(Event::Closed));
                    }
                    Poll::Ready(Err(e)) => {
                        tracing::warn!("Error while closing websocket: {e}");

                        return Poll::Ready(Ok(Event::Closed));
                    }
                    Poll::Pending => return Poll::Pending,
                },
                State::Connected(stream) => stream,
                State::Connecting(future) => match future.poll_unpin(cx) {
                    Poll::Ready(Ok(stream)) => {
                        self.reconnect_backoff.reset();
                        self.heartbeat.reset();
                        self.state = State::Connected(stream);

                        let host = self.url.expose_secret().host();

                        tracing::info!(%host, "Connected to portal");
                        self.join(self.login, self.init_req.clone());

                        continue;
                    }
                    Poll::Ready(Err(InternalError::WebSocket(
                        tokio_tungstenite::tungstenite::Error::Http(r),
                    ))) if r.status().is_client_error() => {
                        return Poll::Ready(Err(Error::Client(r.status())));
                    }
                    Poll::Ready(Err(e)) => {
                        let Some(backoff) = self.reconnect_backoff.next_backoff() else {
                            tracing::warn!("Reconnect backoff expired");
                            return Poll::Ready(Err(Error::MaxRetriesReached));
                        };

                        let secret_url = self.url.clone();
                        let user_agent = self.user_agent.clone();

                        tracing::debug!(?backoff, max_elapsed_time = ?self.reconnect_backoff.max_elapsed_time, "Reconnecting to portal on transient client error: {e}");

                        self.state =
                            State::connect(secret_url, user_agent, self.socket_factory.clone());

                        continue;
                    }
                    Poll::Pending => {
                        // Save a waker in case we want to reset the `Connecting` state while we are waiting.
                        self.waker = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                },
            };

            // Priority 1: Keep local buffers small and send pending messages.
            match stream.poll_ready_unpin(cx) {
                Poll::Ready(Ok(())) => {
                    if let Some(message) = self.pending_messages.pop_front() {
                        match stream.start_send_unpin(Message::Text(message.clone())) {
                            Ok(()) => {
                                tracing::trace!(target: "wire::api::send", %message);

                                match stream.poll_flush_unpin(cx) {
                                    Poll::Ready(Ok(())) => {
                                        tracing::trace!("Flushed websocket");
                                    }
                                    Poll::Ready(Err(e)) => {
                                        self.reconnect_on_transient_error(
                                            InternalError::WebSocket(e),
                                        );
                                        continue;
                                    }
                                    Poll::Pending => {}
                                }
                            }
                            Err(e) => {
                                self.pending_messages.push_front(message);
                                self.reconnect_on_transient_error(InternalError::WebSocket(e));
                            }
                        }
                        continue;
                    }
                }
                Poll::Ready(Err(e)) => {
                    self.reconnect_on_transient_error(InternalError::WebSocket(e));
                    continue;
                }
                Poll::Pending => {}
            }

            // Priority 2: Handle incoming messages.
            match stream.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(message))) => {
                    let Ok(message) = message.into_text() else {
                        tracing::warn!("Received non-text message from portal");
                        continue;
                    };

                    tracing::trace!(target: "wire::api::recv", %message);

                    let message = match serde_json::from_str::<
                        PhoenixMessage<TInboundMsg, TOutboundRes>,
                    >(&message)
                    {
                        Ok(m) => m,
                        Err(e) if e.is_io() || e.is_eof() => {
                            self.reconnect_on_transient_error(InternalError::Serde(e));
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!("Failed to deserialize message: {e}");
                            continue;
                        }
                    };

                    match (message.payload, message.reference) {
                        (Payload::Message(msg), _) => {
                            return Poll::Ready(Ok(Event::InboundMessage {
                                topic: message.topic,
                                msg,
                            }))
                        }
                        (Payload::Reply(_), None) => {
                            tracing::warn!("Discarding reply because server omitted reference");
                            continue;
                        }
                        (Payload::Reply(Reply::Error { reason }), Some(req_id)) => {
                            if message.topic == self.login
                                && self.pending_join_requests.contains(&req_id)
                            {
                                return Poll::Ready(Err(Error::LoginFailed(reason)));
                            }

                            return Poll::Ready(Ok(Event::ErrorResponse {
                                topic: message.topic,
                                req_id,
                                res: reason,
                            }));
                        }
                        (Payload::Reply(Reply::Ok(OkReply::Message(reply))), Some(req_id)) => {
                            if self.pending_join_requests.remove(&req_id) {
                                tracing::info!("Joined {} room on portal", message.topic);

                                // For `phx_join` requests, `reply` is empty so we can safely ignore it.
                                return Poll::Ready(Ok(Event::JoinedRoom {
                                    topic: message.topic,
                                }));
                            }

                            return Poll::Ready(Ok(Event::SuccessResponse {
                                topic: message.topic,
                                req_id,
                                res: reply,
                            }));
                        }
                        (Payload::Reply(Reply::Ok(OkReply::NoMessage(Empty {}))), Some(req_id)) => {
                            if self.heartbeat.maybe_handle_reply(req_id.copy()) {
                                continue;
                            }

                            tracing::trace!("Received empty reply for request {req_id:?}");

                            continue;
                        }
                        (Payload::Error(Empty {}), reference) => {
                            tracing::debug!(
                                ?reference,
                                topic = &message.topic,
                                "Received empty error response"
                            );
                            continue;
                        }
                        (Payload::Close(Empty {}), _) => {
                            self.reconnect_on_transient_error(InternalError::CloseMessage);
                            continue;
                        }
                        (
                            Payload::Disconnect {
                                reason: DisconnectReason::TokenExpired,
                            },
                            _,
                        ) => {
                            return Poll::Ready(Err(Error::TokenExpired));
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    self.reconnect_on_transient_error(InternalError::WebSocket(e));
                    continue;
                }
                Poll::Ready(None) => {
                    self.reconnect_on_transient_error(InternalError::StreamClosed);
                    continue;
                }
                Poll::Pending => {}
            }

            // Priority 3: Handle heartbeats.
            match self.heartbeat.poll(cx) {
                Poll::Ready(Ok(id)) => {
                    self.pending_messages.push_back(serialize_msg(
                        "phoenix",
                        EgressControlMessage::<()>::Heartbeat(Empty {}),
                        id.copy(),
                    ));

                    return Poll::Ready(Ok(Event::HeartbeatSent));
                }
                Poll::Ready(Err(MissedLastHeartbeat {})) => {
                    self.reconnect_on_transient_error(InternalError::MissedHeartbeat);
                    continue;
                }
                Poll::Pending => {}
            }

            return Poll::Pending;
        }
    }

    /// Sets the channels state to [`State::Connecting`] with the given error.
    ///
    /// The [`PhoenixChannel::poll`] function will handle the reconnect if appropriate for the given error.
    fn reconnect_on_transient_error(&mut self, e: InternalError) {
        self.state = State::Connecting(future::ready(Err(e)).boxed())
    }

    fn make_message(
        &mut self,
        topic: impl Into<String>,
        payload: impl Serialize,
    ) -> (OutboundRequestId, String) {
        let request_id = self.fetch_add_request_id();

        // We don't care about the reply type when serializing
        let msg = serialize_msg(topic, payload, request_id.copy());

        (request_id, msg)
    }

    fn fetch_add_request_id(&mut self) -> OutboundRequestId {
        let next_id = self
            .next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        OutboundRequestId(next_id)
    }
}

#[derive(Debug)]
pub enum Event<TInboundMsg, TOutboundRes> {
    SuccessResponse {
        topic: String,
        req_id: OutboundRequestId,
        /// The response received for an outbound request.
        res: TOutboundRes,
    },
    ErrorResponse {
        topic: String,
        req_id: OutboundRequestId,
        res: ErrorReply,
    },
    JoinedRoom {
        topic: String,
    },
    HeartbeatSent,
    /// The server sent us a message, most likely this is a broadcast to all connected clients.
    InboundMessage {
        topic: String,
        msg: TInboundMsg,
    },
    /// The connection was closed successfully.
    Closed,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct PhoenixMessage<T, R> {
    // TODO: we should use a newtype pattern for topics
    topic: String,
    #[serde(flatten)]
    payload: Payload<T, R>,
    #[serde(rename = "ref")]
    reference: Option<OutboundRequestId>,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
#[serde(tag = "event", content = "payload")]
enum Payload<T, R> {
    #[serde(rename = "phx_reply")]
    Reply(Reply<R>),
    #[serde(rename = "phx_error")]
    Error(Empty),
    #[serde(rename = "phx_close")]
    Close(Empty),
    #[serde(rename = "disconnect")]
    Disconnect { reason: DisconnectReason },
    #[serde(untagged)]
    Message(T),
}

// Awful hack to get serde_json to generate an empty "{}" instead of using "null"
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case", tag = "status", content = "response")]
enum Reply<T> {
    Ok(OkReply<T>),
    Error { reason: ErrorReply },
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(untagged)]
enum OkReply<T> {
    Message(T),
    NoMessage(Empty),
}

// TODO: I think this should also be a type-parameter.
/// This represents the info we have about the error
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorReply {
    #[serde(rename = "unmatched topic")]
    UnmatchedTopic,
    NotFound,
    InvalidVersion,
    Offline,
    Disabled,
    #[serde(other)]
    Other,
}

impl fmt::Display for ErrorReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorReply::UnmatchedTopic => write!(f, "unmatched topic"),
            ErrorReply::NotFound => write!(f, "not found"),
            ErrorReply::InvalidVersion => write!(f, "invalid version"),
            ErrorReply::Offline => write!(f, "offline"),
            ErrorReply::Disabled => write!(f, "disabled"),
            ErrorReply::Other => write!(f, "other"),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    TokenExpired,
}

impl<T, R> PhoenixMessage<T, R> {
    pub fn new_message(
        topic: impl Into<String>,
        payload: T,
        reference: Option<OutboundRequestId>,
    ) -> Self {
        Self {
            topic: topic.into(),
            payload: Payload::Message(payload),
            reference,
        }
    }

    pub fn new_ok_reply(
        topic: impl Into<String>,
        payload: R,
        reference: Option<OutboundRequestId>,
    ) -> Self {
        Self {
            topic: topic.into(),
            payload: Payload::Reply(Reply::Ok(OkReply::Message(payload))),
            reference,
        }
    }

    #[cfg(test)]
    fn new_err_reply(
        topic: impl Into<String>,
        reason: ErrorReply,
        reference: Option<OutboundRequestId>,
    ) -> Self {
        Self {
            topic: topic.into(),
            payload: Payload::Reply(Reply::Error { reason }),
            reference,
        }
    }
}

// This is basically the same as tungstenite does but we add some new headers (namely user-agent)
fn make_request(url: Secret<LoginUrl>, user_agent: String) -> Request {
    use secrecy::ExposeSecret as _;

    let mut r = [0u8; 16];
    OsRng.fill_bytes(&mut r);
    let key = base64::engine::general_purpose::STANDARD.encode(r);

    Request::builder()
        .method("GET")
        .header("Host", url.expose_secret().host())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", key)
        .header("User-Agent", user_agent)
        .uri(url.expose_secret().inner().as_str())
        .body(())
        .expect("building static request always works")
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case", tag = "event", content = "payload")]
enum EgressControlMessage<T> {
    PhxJoin(T),
    Heartbeat(Empty),
}

fn serialize_msg(
    topic: impl Into<String>,
    payload: impl Serialize,
    request_id: OutboundRequestId,
) -> String {
    serde_json::to_string(&PhoenixMessage::<_, ()>::new_message(
        topic,
        payload,
        Some(request_id),
    ))
    .expect("we should always be able to serialize a join topic message")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, PartialEq, Debug)]
    #[serde(rename_all = "snake_case", tag = "event", content = "payload")] // This line makes it all work.
    enum Msg {
        Shout { hello: String },
    }

    #[test]
    fn can_deserialize_inbound_message() {
        let msg = r#"{
            "topic": "room:lobby",
            "ref": null,
            "payload": {
                "hello": "world"
            },
            "join_ref": null,
            "event": "shout"
        }"#;

        let msg = serde_json::from_str::<PhoenixMessage<Msg, ()>>(msg).unwrap();

        assert_eq!(msg.topic, "room:lobby");
        assert_eq!(msg.reference, None);
        assert_eq!(
            msg.payload,
            Payload::Message(Msg::Shout {
                hello: "world".to_owned()
            })
        );
    }

    #[test]
    fn unmatched_topic_reply() {
        let actual_reply = r#"
            {
               "event": "phx_reply",
               "ref": "12",
               "topic": "client",
               "payload":{
                  "status": "error",
                  "response":{
                     "reason": "unmatched topic"
                  }
               }
            }
        "#;
        let actual_reply: Payload<(), ()> = serde_json::from_str(actual_reply).unwrap();
        let expected_reply = Payload::<(), ()>::Reply(Reply::Error {
            reason: ErrorReply::UnmatchedTopic,
        });
        assert_eq!(actual_reply, expected_reply);
    }

    #[test]
    fn phx_close() {
        let actual_reply = r#"
        {
          "event": "phx_close",
          "ref": null,
          "topic": "client",
          "payload": {}
        }
        "#;
        let actual_reply: Payload<(), ()> = serde_json::from_str(actual_reply).unwrap();
        let expected_reply = Payload::<(), ()>::Close(Empty {});
        assert_eq!(actual_reply, expected_reply);
    }

    #[test]
    fn token_expired() {
        let actual_reply = r#"
        {
          "event": "disconnect",
          "ref": null,
          "topic": "client",
          "payload": { "reason": "token_expired" }
        }
        "#;
        let actual_reply: Payload<(), ()> = serde_json::from_str(actual_reply).unwrap();
        let expected_reply = Payload::<(), ()>::Disconnect {
            reason: DisconnectReason::TokenExpired,
        };
        assert_eq!(actual_reply, expected_reply);
    }

    #[test]
    fn not_found() {
        let actual_reply = r#"
        {
            "event": "phx_reply",
            "ref": null,
            "topic": "client",
            "payload": {
                "status": "error",
                "response": {
                    "reason": "not_found"
                }
            }
        }
        "#;
        let actual_reply: Payload<(), ()> = serde_json::from_str(actual_reply).unwrap();
        let expected_reply = Payload::<(), ()>::Reply(Reply::Error {
            reason: ErrorReply::NotFound,
        });
        assert_eq!(actual_reply, expected_reply);
    }

    #[test]
    fn unexpected_error_reply() {
        let actual_reply = r#"
            {
               "event": "phx_reply",
               "ref": "12",
               "topic": "client",
               "payload": {
                  "status": "error",
                  "response": {
                     "reason": "bad reply"
                  }
               }
            }
        "#;
        let actual_reply: Payload<(), ()> = serde_json::from_str(actual_reply).unwrap();
        let expected_reply = Payload::<(), ()>::Reply(Reply::Error {
            reason: ErrorReply::Other,
        });
        assert_eq!(actual_reply, expected_reply);
    }

    #[test]
    fn invalid_version_reply() {
        let actual_reply = r#"
            {
                "event": "phx_reply",
                "ref": "12",
                "topic": "client",
                "payload":{
                    "status": "error",
                    "response":{
                        "reason": "invalid_version"
                    }
                }
            }
        "#;
        let actual_reply: Payload<(), ()> = serde_json::from_str(actual_reply).unwrap();
        let expected_reply = Payload::<(), ()>::Reply(Reply::Error {
            reason: ErrorReply::InvalidVersion,
        });
        assert_eq!(actual_reply, expected_reply);
    }

    #[test]
    fn disabled_err_reply() {
        let json = r#"{"event":"phx_reply","ref":null,"topic":"client","payload":{"status":"error","response":{"reason": "disabled"}}}"#;

        let actual = serde_json::from_str::<PhoenixMessage<(), ()>>(json).unwrap();
        let expected = PhoenixMessage::new_err_reply("client", ErrorReply::Disabled, None);

        assert_eq!(actual, expected)
    }
}
