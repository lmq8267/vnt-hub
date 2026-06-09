use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Extension, Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use sqlx::Row;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{self, Duration};
use uuid::Uuid;

use crate::db;
use crate::http_server::{PrefixedStream, RawTcpHandler};
use crate::models::{ClientMessage, ServerMessage, VntClientConfig};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(health))
        .route("/client/ws", get(ws_handler))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "service": "vnt-hub-console" }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(peer): Extension<SocketAddr>,
) -> Result<impl IntoResponse, StatusCode> {
    Ok(ws.on_upgrade(move |socket| handle_socket(state, socket, peer)))
}

async fn handle_socket(state: AppState, socket: WebSocket, peer: SocketAddr) {
    let (mut sender, mut receiver) = socket.split();
    let Some(Ok(Message::Text(first))) = receiver.next().await else {
        return;
    };
    let hello: ClientMessage = match serde_json::from_str(&first) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("client hello parse failed {:?}", e);
            return;
        }
    };
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<ClientMessage>();

    let send_task = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            let text = match serde_json::to_string(&message) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("server message encode failed {:?}", e);
                    continue;
                }
            };
            if sender.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });
    let recv_task = tokio::spawn(async move {
        while let Some(item) = receiver.next().await {
            let Ok(Message::Text(text)) = item else {
                continue;
            };
            match serde_json::from_str::<ClientMessage>(&text) {
                Ok(message) => {
                    if in_tx.send(message).is_err() {
                        break;
                    }
                }
                Err(e) => log::warn!("client message parse failed {:?}", e),
            }
        }
    });
    handle_client_session(state, peer, hello, out_tx, in_rx).await;
    let _ = send_task.abort();
    let _ = recv_task.abort();
}

pub fn raw_tcp_handler(state: AppState) -> RawTcpHandler {
    Arc::new(move |stream, peer| {
        let state = state.clone();
        Box::pin(async move {
            if let Err(e) = handle_raw_tcp(state, stream, peer).await {
                log::warn!("raw tcp client {} failed {:?}", peer, e);
            }
        })
    })
}

async fn handle_raw_tcp(
    state: AppState,
    stream: PrefixedStream,
    peer: SocketAddr,
) -> anyhow::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let Some(first) = lines.next_line().await? else {
        return Ok(());
    };
    let hello = serde_json::from_str::<ClientMessage>(&first)?;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<ClientMessage>();

    let send_task = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            let text = match serde_json::to_string(&message) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("raw tcp server message encode failed {:?}", e);
                    continue;
                }
            };
            if writer.write_all(text.as_bytes()).await.is_err() {
                break;
            }
            if writer.write_all(b"\n").await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });
    let recv_task = tokio::spawn(async move {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match serde_json::from_str::<ClientMessage>(&line) {
                    Ok(message) => {
                        if in_tx.send(message).is_err() {
                            break;
                        }
                    }
                    Err(e) => log::warn!("raw tcp client message parse failed {:?}", e),
                },
                Ok(None) => break,
                Err(e) => {
                    log::warn!("raw tcp client read failed {:?}", e);
                    break;
                }
            }
        }
    });
    handle_client_session(state, peer, hello, out_tx, in_rx).await;
    let _ = send_task.abort();
    let _ = recv_task.abort();
    Ok(())
}

pub async fn serve_udp(state: AppState, addr: SocketAddr) -> anyhow::Result<()> {
    let socket = Arc::new(UdpSocket::bind(addr).await?);
    log::info!("vnt-hub udp client console listen {}", addr);
    let peers = Arc::new(RwLock::new(HashMap::<SocketAddr, String>::new()));
    let mut buf = vec![0u8; 65_535];
    loop {
        let (len, peer) = socket.recv_from(&mut buf).await?;
        let text = match std::str::from_utf8(&buf[..len]) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("udp client {} invalid utf8 {:?}", peer, e);
                continue;
            }
        };
        let message = match serde_json::from_str::<ClientMessage>(text) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("udp client {} message parse failed {:?}", peer, e);
                continue;
            }
        };
        if matches!(message, ClientMessage::Hello { .. }) {
            handle_udp_hello(state.clone(), socket.clone(), peers.clone(), peer, message).await;
            continue;
        }
        let Some(device_id) = peers.read().await.get(&peer).cloned() else {
            log::warn!("udp client {} message before hello", peer);
            continue;
        };
        let is_disconnect = matches!(message, ClientMessage::Disconnect { .. });
        if let Err(e) = handle_client_message(&state, &device_id, message).await {
            log::warn!("udp client {} message handle failed {:?}", peer, e);
        }
        if is_disconnect {
            peers.write().await.remove(&peer);
        }
    }
}

async fn handle_udp_hello(
    state: AppState,
    socket: Arc<UdpSocket>,
    peers: Arc<RwLock<HashMap<SocketAddr, String>>>,
    peer: SocketAddr,
    hello: ClientMessage,
) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let send_socket = socket.clone();
    let send_task = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            let text = match serde_json::to_string(&message) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("udp server message encode failed {:?}", e);
                    continue;
                }
            };
            if let Err(e) = send_socket.send_to(text.as_bytes(), peer).await {
                log::warn!("udp send to {} failed {:?}", peer, e);
                break;
            }
        }
    });
    let Some((id, status)) = handle_client_hello(&state, peer, hello, &out_tx).await else {
        drop(out_tx);
        let _ = time::timeout(Duration::from_secs(1), send_task).await;
        return;
    };
    drop(out_tx);
    let _ = time::timeout(Duration::from_secs(1), send_task).await;
    if status == "kicked" {
        return;
    }

    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let client_socket = socket.clone();
    tokio::spawn(async move {
        while let Some(message) = client_rx.recv().await {
            let text = match serde_json::to_string(&message) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("udp server message encode failed {:?}", e);
                    continue;
                }
            };
            if let Err(e) = client_socket.send_to(text.as_bytes(), peer).await {
                log::warn!("udp send to {} failed {:?}", peer, e);
                break;
            }
        }
    });
    peers.write().await.insert(peer, id.clone());
    state.clients.write().await.insert(id.clone(), client_tx);
    if let Err(e) = push_current_config_if_needed(&state, &id).await {
        log::warn!("initial udp config sync failed {:?}", e);
    }
}

async fn handle_client_session(
    state: AppState,
    peer: SocketAddr,
    hello: ClientMessage,
    outbound: mpsc::UnboundedSender<ServerMessage>,
    mut inbound: mpsc::UnboundedReceiver<ClientMessage>,
) {
    let Some((id, status)) = handle_client_hello(&state, peer, hello, &outbound).await else {
        return;
    };
    if status == "kicked" {
        return;
    }
    state.clients.write().await.insert(id.clone(), outbound);

    if let Err(e) = push_current_config_if_needed(&state, &id).await {
        log::warn!("initial config sync failed {:?}", e);
    }

    while let Some(message) = inbound.recv().await {
        if let Err(e) = handle_client_message(&state, &id, message).await {
            log::warn!("client message handle failed {:?}", e);
        }
    }
    state.clients.write().await.remove(&id);
    mark_device_offline(&state, &id).await;
}

async fn handle_client_hello(
    state: &AppState,
    peer: SocketAddr,
    hello: ClientMessage,
    outbound: &mpsc::UnboundedSender<ServerMessage>,
) -> Option<(String, String)> {
    let ClientMessage::Hello {
        room_id,
        device_id,
        device_token,
        device_name,
        protocol_version,
        client_version,
    } = hello
    else {
        return None;
    };
    if protocol_version != 1 {
        let _ = outbound.send(ServerMessage::Kick {
            reason: "unsupported protocol version".into(),
        });
        return None;
    }
    match state.db.room_exists(&room_id).await {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            log::warn!("client room auth failed");
            return None;
        }
    }
    let console_public_ip = peer.ip().to_string();
    let (id, new_token, status) = match state
        .db
        .upsert_device_hello(
            &room_id,
            device_id.as_deref(),
            device_token.as_deref(),
            &device_name,
            client_version.as_deref(),
            Some(&console_public_ip),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            log::warn!("client device auth failed {:?}", e);
            return None;
        }
    };
    let _ = outbound.send(ServerMessage::HelloAck {
        device_id: id.clone(),
        device_token: new_token,
        status: status.clone(),
        console_version: env!("CARGO_PKG_VERSION").into(),
    });
    Some((id, status))
}

async fn mark_device_offline(state: &AppState, id: &str) {
    let _ = sqlx::query("UPDATE devices SET status = CASE WHEN status = 'kicked' THEN status ELSE 'offline' END WHERE id = ?1")
        .bind(&id)
        .execute(&state.db.pool)
        .await;
}

async fn handle_client_message(
    state: &AppState,
    device_id: &str,
    message: ClientMessage,
) -> anyhow::Result<()> {
    match message {
        ClientMessage::EventReport {
            event_type,
            payload,
            timestamp: _,
        } => {
            if let Some((up_stream, down_stream)) = traffic_from_payload(&event_type, &payload) {
                state
                    .db
                    .update_device_traffic(device_id, up_stream, down_stream)
                    .await?;
            }
            if let Some((status, error)) = vnts_status_from_payload(&event_type, &payload) {
                state
                    .db
                    .update_device_vnts_status(device_id, &status, error.as_deref())
                    .await?;
            }
            let event_id = Uuid::new_v4().to_string();
            let created_at = db::now();
            sqlx::query(
                "INSERT INTO events(id, device_id, event_type, payload, created_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            )
            .bind(&event_id)
            .bind(device_id)
            .bind(&event_type)
            .bind(payload.to_string())
            .bind(created_at)
            .execute(&state.db.pool)
            .await?;
            let _ = state.events.send(
                serde_json::json!({
                    "id": event_id,
                    "device_id": device_id,
                    "event_type": event_type,
                    "payload": payload,
                    "created_at": created_at,
                })
                .to_string(),
            );
        }
        ClientMessage::ConfigAck { config_version } => {
            sqlx::query(
                "UPDATE devices
                 SET config_version = ?1,
                     last_seen = ?2,
                     status = CASE WHEN status = 'kicked' THEN status ELSE 'online' END
                 WHERE id = ?3",
            )
            .bind(config_version as i64)
            .bind(db::now())
            .bind(device_id)
            .execute(&state.db.pool)
            .await?;
            sqlx::query(
                "UPDATE config_pushes
                 SET acked = 1
                 WHERE id = (
                     SELECT id FROM config_pushes
                     WHERE device_id = ?1 AND acked = 0
                     ORDER BY pushed_at DESC
                     LIMIT 1
                 )",
            )
            .bind(device_id)
            .execute(&state.db.pool)
            .await?;
        }
        ClientMessage::ConfigState {
            config_version,
            group_id,
            config_name,
            config,
            running,
        } => {
            sync_reported_config_state(
                state,
                device_id,
                config_version,
                group_id,
                config_name,
                config,
                running,
            )
            .await?;
        }
        ClientMessage::Heartbeat { timestamp: _ } => {
            sqlx::query("UPDATE devices SET last_seen = ?1 WHERE id = ?2")
                .bind(db::now())
                .bind(device_id)
                .execute(&state.db.pool)
                .await?;
        }
        ClientMessage::Disconnect { timestamp: _ } => {
            state.clients.write().await.remove(device_id);
            mark_device_offline(state, device_id).await;
        }
        ClientMessage::TrafficStats {
            up_stream,
            down_stream,
            timestamp: _,
        } => {
            state
                .db
                .update_device_traffic(device_id, up_stream, down_stream)
                .await?;
        }
        ClientMessage::Hello { .. } => {}
    }
    Ok(())
}

async fn push_current_config_if_needed(state: &AppState, device_id: &str) -> anyhow::Result<()> {
    let Some(row) = sqlx::query("SELECT group_id, config_version FROM devices WHERE id = ?1")
        .bind(device_id)
        .fetch_optional(&state.db.pool)
        .await?
    else {
        return Ok(());
    };
    let group_id: Option<String> = row.get(0);
    if group_id.is_none() {
        return Ok(());
    }
    let device_version: i64 = row.get(1);
    let Some((_group_id, config_version, _config)) =
        state.db.device_config(device_id, None).await?
    else {
        return Ok(());
    };
    if config_version > device_version {
        push_device_config(state, device_id).await?;
    }
    Ok(())
}

async fn sync_reported_config_state(
    state: &AppState,
    device_id: &str,
    config_version: u32,
    group_id: Option<String>,
    config_name: Option<String>,
    config: VntClientConfig,
    running: bool,
) -> anyhow::Result<()> {
    if running {
        sqlx::query(
            "UPDATE devices
             SET config_version = ?1,
                 last_seen = ?2,
                 status = CASE WHEN status = 'kicked' THEN status ELSE 'online' END
             WHERE id = ?3",
        )
        .bind(config_version as i64)
        .bind(db::now())
        .bind(device_id)
        .execute(&state.db.pool)
        .await?;
    }

    let Some(row) = sqlx::query("SELECT room_id, group_id FROM devices WHERE id = ?1")
        .bind(device_id)
        .fetch_optional(&state.db.pool)
        .await?
    else {
        return Ok(());
    };
    let room_id: String = row.get(0);
    let current_group_id: Option<String> = row.get(1);

    if current_group_id.is_some() {
        push_if_console_config_differs(state, device_id, config_version, &config).await?;
        return Ok(());
    }

    if let Some(reported_group_id) = group_id.as_deref().filter(|v| !v.trim().is_empty()) {
        let exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM groups WHERE id = ?1 AND room_id = ?2")
                .bind(reported_group_id)
                .bind(&room_id)
                .fetch_one(&state.db.pool)
                .await?;
        if exists > 0 {
            state
                .db
                .set_device_group(&room_id, Some(reported_group_id), device_id)
                .await?;
            push_if_console_config_differs(state, device_id, config_version, &config).await?;
            return Ok(());
        }
    }

    let name = config_name
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "客户端缓存配置".into());
    let group = state
        .db
        .create_group_with_version(&room_id, &name, &config, config_version as i64)
        .await?;
    state
        .db
        .set_device_group(&room_id, Some(&group.id), device_id)
        .await?;
    sqlx::query("UPDATE devices SET config_version = ?1 WHERE id = ?2")
        .bind(config_version as i64)
        .bind(device_id)
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

async fn push_if_console_config_differs(
    state: &AppState,
    device_id: &str,
    reported_version: u32,
    _reported_config: &VntClientConfig,
) -> anyhow::Result<()> {
    let Some((_group_id, console_version, _console_config)) =
        state.db.device_config(device_id, None).await?
    else {
        return Ok(());
    };
    if console_version as u32 != reported_version {
        push_device_config(state, device_id).await?;
    }
    Ok(())
}

async fn push_device_config(state: &AppState, device_id: &str) -> anyhow::Result<bool> {
    crate::api::push_config_to_device(state, device_id, None)
        .await
        .map_err(|status| anyhow::anyhow!("push config failed: {}", status))
}

fn traffic_from_payload(event_type: &str, payload: &serde_json::Value) -> Option<(u64, u64)> {
    if !matches!(
        event_type,
        "traffic_stats" | "traffic" | "status" | "client_status"
    ) {
        return None;
    }
    let up = payload_u64(
        payload,
        &["up_stream", "up_bytes", "upload", "tx", "tx_bytes"],
    )?;
    let down = payload_u64(
        payload,
        &["down_stream", "down_bytes", "download", "rx", "rx_bytes"],
    )?;
    Some((up, down))
}

fn vnts_status_from_payload(
    event_type: &str,
    payload: &serde_json::Value,
) -> Option<(String, Option<String>)> {
    if event_type != "vnts_status" {
        return None;
    }
    let status = payload.get("status")?.as_str()?.trim();
    if status.is_empty() {
        return None;
    }
    let error = payload
        .get("error")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    Some((status.to_string(), error))
}

fn payload_u64(payload: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        payload.get(*key).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|v| u64::try_from(v).ok()))
                .or_else(|| value.as_str().and_then(|v| v.parse::<u64>().ok()))
        })
    })
}
