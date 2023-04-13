
use std::{net::SocketAddr, collections::HashMap, sync::Arc};

use axum::{
    routing::get,
    Router,
    extract::{WebSocketUpgrade, Path, ws::{Message, WebSocket}, State},
    response::Response,
};

use futures::{StreamExt, stream::SplitStream, SinkExt};
use serde::{Serialize, Deserialize};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone)]
struct Rooms(Arc<Mutex<HashMap<String, Room>>>);

impl Rooms {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    async fn connect(&self, room: &str, tx: mpsc::Sender<Message>) -> Uuid {
        let mut rooms = self.0.lock().await;
        let room = rooms.entry(room.to_string()).or_insert(Room::new());

        let id = Uuid::new_v4();
        room.connections.push(Connection { id, tx });
        id
    }

    async fn send(&self, room: &str, sender_id: Uuid, msg: Message) {
        if let Some(room) = self.0.lock().await.get(room) {
            for conn in room.connections.iter().filter(|v| v.id != sender_id) {
                conn.tx.send(msg.clone()).await.expect("could not send message");
            }
        }
    }

    async fn send_to(&self, room: &str, target_id: Uuid, msg: Message) {
        let tx = self.0.lock().await.get(room).and_then(|room| room.connections.iter().find(|v| v.id == target_id).map(|v| v.tx.clone()));

        if let Some(tx) = tx { tx.send(msg).await.expect("could not send message") }
    }

    async fn send_json(&self, room: &str, sender_id: Uuid, msg: &impl Serialize) {
        self.send(
            room,
            sender_id,
            Message::Text(
                serde_json::to_string(msg).expect("could not serialize json message"),
            ),
        ).await
    }

    async fn send_json_to(&self, room: &str, target_id: Uuid, msg: &impl Serialize) {
        self.send_to(
            room,
            target_id,
            Message::Text(
                serde_json::to_string(msg).expect("could not serialize json message"),
            ),
        ).await;
    }

    async fn disconnect(&self, room: &str, id: Uuid) {
        let mut rooms = self.0.lock().await;
        if let Some(room) = rooms.get_mut(room) {
            room.connections.retain(|v| v.id != id)
        }
    }
}

struct Room {
    connections: Vec<Connection>,
}

impl Room {
    fn new() -> Self { Self { connections: Vec::new() } }
}

struct Connection {
    id: Uuid,
    tx: mpsc::Sender<Message>,
}

#[derive(Serialize)]
#[serde(tag = "ty")]
enum SocketMessage {
    Join { id: Uuid, username: String },
    JoinSelf { id: Uuid },
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Path((room_id, username)): Path<(String, String)>,
    State(rooms): State<Rooms>,
) -> Response {
    ws.on_upgrade(|sock| async move {

        tracing::info!("got upgraded websocket (room: {room_id})");
        let (mut sock_tx, sock_rx) = sock.split();
        let (tx, mut rx) = mpsc::channel::<Message>(8);

        let id = rooms.connect(&room_id, tx).await;
        tracing::info!("connection {id} to room {room_id}");

        rooms.send_json_to(&room_id, id, &SocketMessage::JoinSelf { id }).await;
        rooms.send_json(&room_id, id, &SocketMessage::Join { id, username }).await;

        tokio::spawn(handle_socket(sock_rx, room_id, id, rooms));

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                sock_tx.send(msg).await.expect("could not send message via socket");
            }
        });
    })
}

async fn handle_socket(mut sock: SplitStream<WebSocket>, room_id: String, id: Uuid, rooms: Rooms) {
    while let Some(msg) = sock.next().await {
        // tracing::info!("message ({id})");
        match msg {
            Ok(msg) => {
                match msg {
                    Message::Text(msg) => {
                        #[derive(Deserialize)]
                        struct Message {
                            target: Option<Uuid>,

                            #[serde(default)] #[serde(flatten)]
                            rest: Option<HashMap<String, serde_json::Value>>,
                        }

                        #[derive(Serialize, Debug)]
                        struct MsgOut {
                            source: Uuid,
                            #[serde(flatten)]
                            data: Option<HashMap<String, serde_json::Value>>,
                        }

                        let Ok(message) = serde_json::from_str::<Message>(&msg) else { continue };
                        let msg = MsgOut {
                            source: id,
                            data: message.rest,
                        };
                        // tracing::info!("message ({id}): {msg:?}");
                        if let Some(target) = message.target {
                            rooms.send_json_to(&room_id, target, &msg).await;
                        } else {
                            rooms.send_json(&room_id, id, &msg).await;
                        }
                    },
                    Message::Binary(msg) => tracing::info!("got binary message ({} bytes): ignoring", msg.len()),
                    Message::Close(close) => { tracing::info!("got close message {close:?} from {id}"); break },
                    m => { tracing::info!("got other message: {m:?}") },
                }
            },
            Err(err) => { tracing::error!("web socket error: {err}"); break },
        }
    }

    tracing::info!("disconnecting from the room");
    rooms.disconnect(&room_id, id).await;
}

#[tokio::main]
async fn main() {
    let server_port = 3000;

    tracing_subscriber::fmt()
        .with_env_filter("none,server=trace")
    .init();

    let app = Router::new()
        .route("/ws/:room_id/:user", get(ws_handler))
        .with_state(Rooms::new());

    let addr = SocketAddr::from(([0, 0, 0, 0], server_port));
    tracing::info!("listening on {addr}");
    axum::Server::bind(&addr)
        .serve(app.into_make_service()).await
    .expect("got a server error");
}

