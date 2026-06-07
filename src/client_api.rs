use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::db;
use crate::models::{ClientMessage, ServerMessage};
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
) -> Result<impl IntoResponse, StatusCode> {
    Ok(ws.on_upgrade(move |socket| handle_socket(state, socket)))
}

async fn handle_socket(state: AppState, socket: WebSocket) {
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
    let ClientMessage::Hello {
        room_id,
        device_id,
        device_token,
        device_name,
        protocol_version,
    } = hello
    else {
        return;
    };
    if protocol_version != 1 {
        let _ = sender
            .send(Message::Text(
                serde_json::to_string(&ServerMessage::Kick {
                    reason: "unsupported protocol version".into(),
                })
                .unwrap_or_default(),
            ))
            .await;
        return;
    }
    match state.db.room_exists(&room_id).await {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            log::warn!("client room auth failed");
            return;
        }
    }
    let (id, new_token, status) = match state
        .db
        .upsert_device_hello(
            &room_id,
            device_id.as_deref(),
            device_token.as_deref(),
            &device_name,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            log::warn!("client device auth failed {:?}", e);
            return;
        }
    };
    let ack = ServerMessage::HelloAck {
        device_id: id.clone(),
        device_token: new_token,
        status: status.clone(),
    };
    if sender
        .send(Message::Text(
            serde_json::to_string(&ack).unwrap_or_default(),
        ))
        .await
        .is_err()
    {
        return;
    }
    if status == "kicked" {
        return;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    state.clients.write().await.insert(id.clone(), tx);

    let send_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
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

    while let Some(item) = receiver.next().await {
        let Ok(Message::Text(text)) = item else {
            continue;
        };
        let message: ClientMessage = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("client message parse failed {:?}", e);
                continue;
            }
        };
        if let Err(e) = handle_client_message(&state, &id, message).await {
            log::warn!("client message handle failed {:?}", e);
        }
    }
    state.clients.write().await.remove(&id);
    let _ = send_task.abort();
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
            sqlx::query("UPDATE devices SET config_version = ?1, last_seen = ?2 WHERE id = ?3")
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
        ClientMessage::Heartbeat { timestamp: _ } => {
            sqlx::query("UPDATE devices SET last_seen = ?1 WHERE id = ?2")
                .bind(db::now())
                .bind(device_id)
                .execute(&state.db.pool)
                .await?;
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
