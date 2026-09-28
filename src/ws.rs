//! `/ws/rfq` and `/ws/sign` (contract §1): challenge → hello → welcome, then a reader
//! loop that hands replies to their waiting requests, and a writer task that owns the
//! socket's send half and pings it. Upgrades take an unauthenticated-socket slot and
//! cap message size; frames over 16 KiB before the hello are refused unparsed (§1.2).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::auth::{self, AuthError, CLOSE_BAD_HELLO, CLOSE_REPLACED, Expected};
use crate::conn::{Conn, ConnInfo, Outbound};
use crate::http::json_response;
use crate::market::health_poller;
use crate::preauth::{PreauthPermit, Refused};
use crate::protocol::{Channel, error_body, now_ms, random_challenge};
use crate::ratelimit::{client_key, is_local};
use crate::state::App;

/// Close code for a connection that went silent (§1.3 names none).
pub const CLOSE_IDLE: u16 = 4008;
/// §1.2: a frame this large before authentication is refused unparsed (a hello is
/// well under 1 KiB).
pub const MAX_PREAUTH_FRAME_BYTES: usize = 16 * 1024;
const WRITER_GRACE: Duration = Duration::from_secs(1);

type Peer = Option<Extension<ConnectInfo<SocketAddr>>>;

pub async fn ws_rfq(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    peer: Peer,
    ws: WebSocketUpgrade,
) -> Response {
    upgrade(app, Channel::Rfq, &headers, peer, ws)
}

pub async fn ws_sign(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    peer: Peer,
    ws: WebSocketUpgrade,
) -> Response {
    upgrade(app, Channel::Sign, &headers, peer, ws)
}

/// Takes an unauthenticated-socket slot (else 503 without upgrading) and upgrades with
/// the message cap; the slot travels with the socket until its hello is verified. A
/// desk on this machine (the house desk) takes no slot, so a flood through the public
/// tunnel cannot keep it from reconnecting.
fn upgrade(
    app: Arc<App>,
    channel: Channel,
    headers: &HeaderMap,
    peer: Peer,
    ws: WebSocketUpgrade,
) -> Response {
    let peer = peer.map(|Extension(ConnectInfo(addr))| addr);
    let permit = if app.config.rate_limits.exempt_local_ws && is_local(headers, peer) {
        None
    } else {
        let client = client_key(headers, peer);
        match app.preauth.admit(&client) {
            Ok(p) => Some(p),
            Err(why) => {
                let reason = match why {
                    Refused::Global => "too many unauthenticated connections",
                    Refused::PerIp => "too many unauthenticated connections from this client",
                };
                tracing::info!(%client, channel = channel.as_str(), "refusing upgrade: {reason}");
                return json_response(503, &error_body(reason));
            }
        }
    };
    let cap = app.config.ws_max_message_bytes;
    ws.max_message_size(cap)
        .max_frame_size(cap)
        .on_upgrade(move |socket| handle_socket(app, channel, socket, permit))
}

async fn handle_socket(
    app: Arc<App>,
    channel: Channel,
    socket: WebSocket,
    permit: Option<PreauthPermit>,
) {
    let (sink, mut stream) = socket.split();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_loop(sink, out_rx, app.config.ping_interval));

    let challenge = match random_challenge() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("no randomness for a challenge: {e}");
            let _ = out_tx.send(Outbound::Close(1011, "internal error".into()));
            return finish(writer).await;
        }
    };
    let _ = out_tx.send(text(
        &json!({ "type": "challenge", "challenge": challenge, "server_time": now_ms() }),
    ));

    let expected = Expected {
        channel,
        challenge: &challenge,
        allowlist: &app.config.allowlist,
        program_id: &app.config.program_id,
        usdc_mint: &app.config.usdc_mint,
    };
    let verdict =
        match tokio::time::timeout(app.config.hello_timeout, read_hello(&mut stream)).await {
            Err(_) => Err(AuthError {
                code: CLOSE_BAD_HELLO,
                reason: "no valid hello within the time limit".into(),
            }),
            Ok(Hello::Closed) => return finish_with(out_tx, writer).await, // peer went away
            Ok(Hello::TooLarge) => Err(AuthError {
                code: CLOSE_BAD_HELLO,
                reason: "hello too large".into(),
            }),
            Ok(Hello::Frame(hello)) => auth::verify_hello(&hello, &expected),
        };
    // Verified or refused: either way this socket no longer counts as unauthenticated.
    drop(permit);
    let authed = match verdict {
        Ok(a) => a,
        Err(e) => {
            tracing::info!(
                channel = channel.as_str(),
                code = e.code,
                "rejecting hello: {}",
                e.reason
            );
            let _ = out_tx.send(text(&json!({ "type": "error", "reason": e.reason })));
            let _ = out_tx.send(Outbound::Close(e.code, e.reason));
            return finish_with(out_tx, writer).await;
        }
    };

    let conn = Arc::new(Conn::new(
        ConnInfo {
            id: app.registry.next_conn_id(),
            channel,
            mm_pubkey: authed.mm_pubkey.clone(),
            instance_id: authed.instance_id.clone(),
            name: authed.name.clone(),
            authenticated_at_ms: now_ms(),
        },
        out_tx,
    ));
    conn.send(&json!({
        "type": "welcome", "channel": channel.as_str(), "mm_pubkey": authed.mm_pubkey,
        "name": authed.name, "instance_id": authed.instance_id,
    }));
    if let Some(old) = app.registry.register(conn.clone()) {
        tracing::info!(mm = %conn.name, channel = channel.as_str(), "replacing connection");
        old.close(CLOSE_REPLACED, "replaced");
    }
    tracing::info!(mm = %conn.name, instance = %conn.instance_id, channel = channel.as_str(), "market maker connected");
    if channel == Channel::Rfq {
        tokio::spawn(health_poller(app.clone(), conn.clone()));
    }

    read_loop(&app, &conn, &mut stream).await;

    app.registry.unregister(&conn);
    conn.close(1000, "closing");
    tracing::info!(mm = %conn.name, instance = %conn.instance_id, channel = channel.as_str(), "market maker disconnected");
    finish(writer).await;
}

fn text(v: &Value) -> Outbound {
    Outbound::Text(v.to_string())
}

enum Hello {
    Frame(Value),
    /// A frame over [`MAX_PREAUTH_FRAME_BYTES`], refused without parsing.
    TooLarge,
    Closed,
}

/// Waits for the first `hello` frame; smaller frames before it are ignored.
async fn read_hello(stream: &mut SplitStream<WebSocket>) -> Hello {
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(t) if t.len() > MAX_PREAUTH_FRAME_BYTES => return Hello::TooLarge,
            Message::Binary(b) if b.len() > MAX_PREAUTH_FRAME_BYTES => return Hello::TooLarge,
            Message::Text(t) => {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str())
                    && v.get("type").and_then(Value::as_str) == Some("hello")
                {
                    return Hello::Frame(v);
                }
            }
            Message::Close(_) => return Hello::Closed,
            _ => {}
        }
    }
    Hello::Closed
}

async fn read_loop(app: &App, conn: &Conn, stream: &mut SplitStream<WebSocket>) {
    loop {
        tokio::select! {
            _ = conn.closed() => return, // replaced or shutting down; the writer sends the close
            next = tokio::time::timeout(app.config.idle_timeout, stream.next()) => match next {
                Err(_) => {
                    tracing::info!(mm = %conn.name, channel = conn.channel.as_str(), "closing idle connection");
                    conn.close(CLOSE_IDLE, "idle timeout");
                    return;
                }
                Ok(None) | Ok(Some(Ok(Message::Close(_)))) => return,
                Ok(Some(Err(e))) => {
                    // Includes a message over the size cap (§1.2).
                    tracing::info!(mm = %conn.name, channel = conn.channel.as_str(), "closing connection after a read error: {e}");
                    return;
                }
                Ok(Some(Ok(Message::Text(t)))) => on_text(conn, t.as_str()),
                Ok(Some(Ok(_))) => {} // ping, pong, binary: liveness only
            },
        }
    }
}

/// Routes a reply to the request waiting on its id.
fn on_text(conn: &Conn, raw: &str) {
    let Ok(frame) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    let key_field = match (conn.channel, frame.get("type").and_then(Value::as_str)) {
        (Channel::Rfq, Some("rfq_response" | "rfq_decline")) => "rfq_id",
        (Channel::Rfq, Some("market_response"))
        | (Channel::Sign, Some("sign_response" | "sign_rejection")) => "request_id",
        _ => return,
    };
    let Some(key) = frame
        .get(key_field)
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    if !conn.resolve(&key, frame) {
        tracing::debug!(mm = %conn.name, %key, "late or unknown reply");
    }
}

type Sink = SplitSink<WebSocket, Message>;

async fn write_loop(
    mut sink: Sink,
    mut rx: mpsc::UnboundedReceiver<Outbound>,
    ping_every: Duration,
) {
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + ping_every, ping_every);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Outbound::Text(t)) => {
                    if sink.send(Message::Text(t.into())).await.is_err() {
                        return;
                    }
                }
                Some(Outbound::Close(code, reason)) => {
                    let _ = sink.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
                    let _ = sink.flush().await;
                    return;
                }
                None => return,
            },
            _ = ping.tick() => {
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn finish_with(out_tx: mpsc::UnboundedSender<Outbound>, writer: JoinHandle<()>) {
    drop(out_tx);
    finish(writer).await;
}

/// Lets the writer flush its close frame, but never hangs on a dead peer.
async fn finish(mut writer: JoinHandle<()>) {
    if tokio::time::timeout(WRITER_GRACE, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
}
