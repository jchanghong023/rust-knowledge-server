use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        rejection::{JsonRejection, QueryRejection},
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, oneshot},
    time::{self, Instant, MissedTickBehavior},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const MAX_FRAME: usize = 16 * 1024 * 1024;
const QUEUE_CAPACITY: usize = 16;
const PING_INTERVAL: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const PING: &[u8] = b"collab2";

#[derive(Clone)]
struct Relay {
    rooms: Arc<Mutex<Rooms>>,
    jobs: TaskTracker,
    cancellation: CancellationToken,
}

#[derive(Default)]
struct Rooms {
    by_id: HashMap<String, Room>,
    aliases: HashMap<String, JoinInfo>,
}

struct Room {
    host: Output,
    guests: HashMap<u32, Output>,
    next_peer: u32,
    aliases: Vec<String>,
}

struct Output {
    sender: mpsc::Sender<Message>,
    stop: CancellationToken,
}

impl Output {
    fn send(&self, message: Message) {
        if self.sender.try_send(message).is_err() {
            self.stop.cancel();
        }
    }

    fn is_live(&self) -> bool {
        !self.stop.is_cancelled() && !self.sender.is_closed()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JoinInfo {
    room_id: String,
    secret: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    room_number: String,
    room_id: String,
    secret: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lookup {
    room_number: String,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Host,
    Guest,
}

#[derive(Deserialize)]
struct RoleQuery {
    role: Role,
}

pub(crate) fn router(jobs: TaskTracker, cancellation: CancellationToken) -> Router {
    Router::new()
        .route("/api/collab2/rooms", post(register))
        .route("/api/collab2/lookup", post(lookup))
        .route_layer(middleware::map_response(|mut response: Response| async {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }))
        .route("/r/{roomId}", get(upgrade))
        .with_state(Relay {
            rooms: Arc::default(),
            jobs,
            cancellation,
        })
}

fn valid_id(value: &str, min: usize) -> bool {
    (min..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_number(value: &str) -> bool {
    valid_id(value, 1) && !matches!(value, "join" | "stop")
}

async fn register(
    State(relay): State<Relay>,
    body: Result<Json<Registration>, JsonRejection>,
) -> StatusCode {
    let Ok(Json(Registration {
        room_number,
        room_id,
        secret,
    })) = body
    else {
        return StatusCode::BAD_REQUEST;
    };
    if !valid_number(&room_number)
        || !valid_id(&room_id, 10)
        || secret.len() != 64
        || !matches!(URL_SAFE_NO_PAD.decode_slice(&secret, &mut [0; 48]), Ok(48))
    {
        return StatusCode::BAD_REQUEST;
    }

    let mut state = relay.rooms.lock().unwrap();
    let Rooms { by_id, aliases } = &mut *state;
    let Some(room) = by_id.get_mut(&room_id).filter(|room| room.host.is_live()) else {
        return StatusCode::NOT_FOUND;
    };
    if aliases.contains_key(&room_number) {
        return StatusCode::CONFLICT;
    }
    room.aliases.push(room_number.clone());
    aliases.insert(room_number, JoinInfo { room_id, secret });
    StatusCode::CREATED
}

async fn lookup(State(relay): State<Relay>, body: Result<Json<Lookup>, JsonRejection>) -> Response {
    let Ok(Json(Lookup { room_number })) = body else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !valid_number(&room_number) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let state = relay.rooms.lock().unwrap();
    let Some(info) = state.aliases.get(&room_number).filter(|info| {
        state
            .by_id
            .get(&info.room_id)
            .is_some_and(|room| room.host.is_live())
    }) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(info).into_response()
}

async fn upgrade(
    State(relay): State<Relay>,
    Path(room_id): Path<String>,
    role: Result<Query<RoleQuery>, QueryRejection>,
    ws: WebSocketUpgrade,
) -> Response {
    let Ok(Query(RoleQuery { role })) = role else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !valid_id(&room_id, 10) || relay.cancellation.is_cancelled() {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Track the pending handoff as well as the connection. On failure Axum drops
    // the sender; on shutdown the tracked receiver exits even before upgrade.
    let (sender, receiver) = oneshot::channel();
    let connection = relay.clone();
    relay.jobs.spawn(async move {
        tokio::select! {
            _ = connection.cancellation.cancelled() => {}
            result = receiver => {
                if let Ok(socket) = result {
                    serve_socket(&connection, room_id, role, socket).await;
                }
            }
        }
    });
    ws.max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| async move {
            let _ = sender.send(socket);
        })
}

fn admit(
    state: &mut Rooms,
    room_id: &str,
    role: Role,
    out: Output,
) -> Result<u32, (u16, &'static str)> {
    match role {
        Role::Host => {
            if state.by_id.contains_key(room_id) {
                return Err((4009, "a host is already connected for this room"));
            }
            out.send(Message::Text(r#"{"t":"host-ready"}"#.into()));
            state.by_id.insert(
                room_id.to_owned(),
                Room {
                    host: out,
                    guests: HashMap::new(),
                    next_peer: 1,
                    aliases: Vec::new(),
                },
            );
            Ok(0)
        }
        Role::Guest => {
            let room = state
                .by_id
                .get_mut(room_id)
                .filter(|room| room.host.is_live())
                .ok_or((4004, "no such room"))?;
            let peer = room.next_peer;
            room.next_peer = peer.checked_add(1).ok_or((1011, "peer IDs exhausted"))?;
            room.guests.insert(peer, out);
            room.host.send(peer_event("peer-joined", peer));
            Ok(peer)
        }
    }
}

fn peer_event(event: &str, peer: u32) -> Message {
    Message::Text(format!(r#"{{"t":"{event}","peer":{peer}}}"#).into())
}

fn forward(
    relay: &Relay,
    room_id: &str,
    peer: u32,
    identity: &mpsc::WeakSender<Message>,
    frame: Bytes,
) {
    let Some(prefix) = frame.first_chunk::<4>() else {
        return;
    };
    if peer == 0 {
        let target = u32::from_be_bytes(*prefix);
        let state = relay.rooms.lock().unwrap();
        let Some(room) = state.by_id.get(room_id) else {
            return;
        };
        if target == 0 {
            for guest in room.guests.values() {
                guest.send(Message::Binary(frame.clone()));
            }
        } else if let Some(guest) = room.guests.get(&target) {
            guest.send(Message::Binary(frame));
        }
    } else {
        let mut frame = frame
            .try_into_mut()
            .unwrap_or_else(|shared| shared.as_ref().into());
        frame[..4].copy_from_slice(&peer.to_be_bytes());
        let state = relay.rooms.lock().unwrap();
        if let Some(room) = state.by_id.get(room_id) {
            // A closing guest must not forward into a new host's room with the
            // same raw ID and reassigned peer ID.
            if room.guests.get(&peer).is_some_and(|out| {
                identity
                    .upgrade()
                    .is_some_and(|own| own.same_channel(&out.sender))
            }) {
                room.host.send(Message::Binary(frame.freeze()));
            }
        }
    }
}

async fn serve_socket(relay: &Relay, room_id: String, role: Role, mut socket: WebSocket) {
    let (sender, mut inbox) = mpsc::channel(QUEUE_CAPACITY);
    let identity = sender.downgrade();
    let stop = CancellationToken::new();
    let admitted = {
        let mut state = relay.rooms.lock().unwrap();
        if relay.cancellation.is_cancelled() {
            return;
        }
        admit(
            &mut state,
            &room_id,
            role,
            Output {
                sender,
                stop: stop.clone(),
            },
        )
    };
    let peer = match admitted {
        Ok(peer) => peer,
        Err((code, reason)) => {
            write(&mut socket, close(code, reason)).await;
            return;
        }
    };

    let mut heartbeat = time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut awaiting_pong = false;
    let mut reply_to_close = false;
    let terminal = loop {
        tokio::select! {
            _ = stop.cancelled() => break None,
            _ = relay.cancellation.cancelled(), if peer == 0 => {
                break Some(close(1001, "relay shutting down"));
            }
            _ = heartbeat.tick() => {
                if awaiting_pong || !write(&mut socket, Message::Ping(PING.into())).await {
                    break None;
                }
                awaiting_pong = true;
            }
            queued = inbox.recv() => {
                let Some(message) = queued else {
                    // Only room removal drops an admitted guest's sender.
                    break Some(close(4001, "room closed"));
                };
                if !write(&mut socket, message).await {
                    break None;
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Binary(frame))) => {
                    forward(relay, &room_id, peer, &identity, frame);
                }
                Some(Ok(Message::Pong(payload))) if payload.as_ref() == PING => awaiting_pong = false,
                Some(Ok(Message::Close(_))) => {
                    reply_to_close = true;
                    break None;
                }
                Some(Err(_)) | None => break None,
                Some(Ok(_)) => {}
            }
        }
    };

    {
        let mut state = relay.rooms.lock().unwrap();
        if peer == 0 {
            if let Some(room) = state.by_id.remove(&room_id) {
                for alias in room.aliases {
                    state.aliases.remove(&alias);
                }
            }
        } else if let Some(room) = state.by_id.get_mut(&room_id)
            && room.guests.get(&peer).is_some_and(|out| {
                identity
                    .upgrade()
                    .is_some_and(|own| own.same_channel(&out.sender))
            })
        {
            room.guests.remove(&peer);
            room.host.send(peer_event("peer-left", peer));
        }
    }
    if reply_to_close {
        // Tungstenite queues the reply when it reads the peer's Close frame.
        let _ = time::timeout(WRITE_TIMEOUT, socket.flush()).await;
    } else if let Some(message) = terminal
        && (peer == 0 || write(&mut socket, Message::Text(r#"{"t":"room-closed"}"#.into())).await)
    {
        write(&mut socket, message).await;
    }
}

fn close(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

async fn write(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        time::timeout(WRITE_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}
