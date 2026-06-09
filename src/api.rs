use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::auth::{self, AuthUser};
use crate::crypto;
use crate::db;
use crate::models::{
    BackupEnvelope, ChangePasswordRequest, CreateGroupRequest, CreateRoomRequest, LoginRequest,
    LoginResponse, PublicConfig, ServerMessage, UpdateDeviceRequest, UpdateGroupRequest,
};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/public/config", get(public_config))
        .route("/api/auth/login", post(login))
        .route("/api/auth/register", post(register))
        .route("/api/auth/change-password", post(change_password))
        .route("/api/auth/account", delete(delete_account))
        .route("/api/rooms", get(list_rooms).post(create_room))
        .route("/api/rooms/:room_id", delete(delete_room))
        .route("/api/rooms/:room_id/devices", get(list_devices))
        .route(
            "/api/rooms/:room_id/devices/:id",
            patch(update_device).delete(delete_device),
        )
        .route("/api/rooms/:room_id/devices/:id/kick", post(kick_device))
        .route("/api/rooms/:room_id/devices/:id/push", post(push_device))
        .route(
            "/api/rooms/:room_id/devices/:id/token",
            get(regenerate_device_token),
        )
        .route(
            "/api/rooms/:room_id/groups",
            get(list_groups).post(create_group),
        )
        .route(
            "/api/rooms/:room_id/groups/:id",
            put(update_group).delete(delete_group),
        )
        .route("/api/rooms/:room_id/groups/:id/push", post(push_group))
        .route("/api/rooms/:room_id/groups/:id/add", post(add_group_device))
        .route(
            "/api/rooms/:room_id/groups/:id/remove",
            post(remove_group_device),
        )
        .route("/api/events", get(list_events))
        .route("/api/events/stream", get(events_stream))
        .route("/api/backup/export", get(export_backup))
        .route("/api/backup/import", post(import_backup))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn public_config(State(state): State<AppState>) -> Json<PublicConfig> {
    Json(PublicConfig {
        allow_register: !state.args.disable_register,
        console_version: env!("CARGO_PKG_VERSION").into(),
    })
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, StatusCode> {
    let key = login_failure_key(&headers);
    let now = db::now();
    {
        let failures = state.login_failures.lock().await;
        if failures
            .get(&key)
            .map(|v| v.locked_until > now)
            .unwrap_or(false)
        {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    }
    let Some((id, password_hash, role)) = state
        .db
        .find_user(&req.username)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        record_login_failure(&state, &key).await;
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !auth::verify_password(&password_hash, &req.password) {
        record_login_failure(&state, &key).await;
        return Err(StatusCode::UNAUTHORIZED);
    }
    state.login_failures.lock().await.remove(&key);
    let access_token =
        auth::sign(&id, &req.username, &role).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(LoginResponse { access_token }))
}

async fn register(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    if state.args.disable_register {
        return Err(StatusCode::FORBIDDEN);
    }
    validate_username_password(&req.username, &req.password)?;
    let id = uuid::Uuid::new_v4().to_string();
    let hash = auth::hash_password(&req.password).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query("INSERT INTO users(id, username, password, role, created_at) VALUES(?1, ?2, ?3, 'user', ?4)")
        .bind(&id)
        .bind(&req.username)
        .bind(hash)
        .bind(db::now())
        .execute(&state.db.pool)
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(json!({ "id": id })))
}

async fn change_password(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let Some((_id, old_hash, _role)) = state
        .db
        .find_user(&user.username)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !auth::verify_password(&old_hash, &req.old_password) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    validate_password(&req.new_password)?;
    let hash =
        auth::hash_password(&req.new_password).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query("UPDATE users SET password = ?1 WHERE id = ?2")
        .bind(hash)
        .bind(&user.id)
        .execute(&state.db.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_account(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<impl IntoResponse, StatusCode> {
    if user.username == "admin" || user.role == "admin" {
        return Err(StatusCode::FORBIDDEN);
    }
    sqlx::query("DELETE FROM users WHERE id = ?1")
        .bind(&user.id)
        .execute(&state.db.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "ok": true })))
}

async fn list_rooms(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<impl IntoResponse, StatusCode> {
    let rooms = state
        .db
        .list_rooms(&user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rooms))
}

async fn create_room(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<CreateRoomRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let room = state
        .db
        .create_room(&user.id, &req.name)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(room))
}

async fn delete_room(
    State(state): State<AppState>,
    user: AuthUser,
    Path(room_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    state
        .db
        .delete_room(&room_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "ok": true })))
}

async fn list_devices(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(room_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &_user, &room_id).await?;
    let devices = state
        .db
        .list_devices(&room_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(devices))
}

async fn update_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, id)): Path<(String, String)>,
    Json(req): Json<UpdateDeviceRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    state
        .db
        .update_device(&room_id, &id, req)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    state
        .db
        .delete_device(&room_id, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.clients.write().await.remove(&id);
    Ok(Json(json!({ "ok": true })))
}

async fn regenerate_device_token(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    let token = state
        .db
        .regenerate_device_token(&room_id, &id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    state.clients.write().await.remove(&id);
    Ok(Json(json!({ "device_token": token })))
}

async fn list_groups(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(room_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &_user, &room_id).await?;
    let groups = state
        .db
        .list_groups(&room_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(groups))
}

async fn create_group(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(room_id): Path<String>,
    Json(req): Json<CreateGroupRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &_user, &room_id).await?;
    let group = state
        .db
        .create_group(&room_id, &req.name, &req.config)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(group))
}

async fn update_group(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, id)): Path<(String, String)>,
    Json(req): Json<UpdateGroupRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    let group = state
        .db
        .update_group(&room_id, &id, req)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Json(group))
}

async fn delete_group(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    state
        .db
        .delete_group(&room_id, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct DeviceIdRequest {
    device_id: String,
}

async fn add_group_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, id)): Path<(String, String)>,
    Json(req): Json<DeviceIdRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    state
        .db
        .set_device_group(&room_id, Some(&id), &req.device_id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Json(json!({ "ok": true })))
}

async fn remove_group_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, _id)): Path<(String, String)>,
    Json(req): Json<DeviceIdRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    state
        .db
        .set_device_group(&room_id, None, &req.device_id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Json(json!({ "ok": true })))
}

async fn kick_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    let result = sqlx::query("UPDATE devices SET status = 'kicked' WHERE id = ?1 AND room_id = ?2")
        .bind(&id)
        .bind(&room_id)
        .execute(&state.db.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Some(tx) = state.clients.write().await.remove(&id) {
        let _ = tx.send(crate::models::ServerMessage::Kick {
            reason: "kicked by console".into(),
        });
    }
    Ok(Json(json!({ "ok": true })))
}

async fn push_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    ensure_device_in_room(&state, &room_id, &id).await?;
    let pushed = push_config_to_device(&state, &id, None).await?;
    Ok(Json(json!({ "pushed": pushed })))
}

async fn push_group(
    State(state): State<AppState>,
    user: AuthUser,
    Path((room_id, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    ensure_room_access(&state, &user, &room_id).await?;
    ensure_group_in_room(&state, &room_id, &id).await?;
    let rows = sqlx::query("SELECT id FROM devices WHERE room_id = ?1 AND group_id = ?2")
        .bind(&room_id)
        .bind(&id)
        .fetch_all(&state.db.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut count = 0;
    for row in rows {
        use sqlx::Row;
        let device_id: String = row.get(0);
        if push_config_to_device(&state, &device_id, Some(&id)).await? {
            count += 1;
        }
    }
    Ok(Json(json!({ "pushed": count })))
}

pub(crate) async fn push_config_to_device(
    state: &AppState,
    device_id: &str,
    group_id: Option<&str>,
) -> Result<bool, StatusCode> {
    let Some((effective_group_id, version, config)) = state
        .db
        .device_config(device_id, group_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::NOT_FOUND);
    };
    let config_name: Option<String> = if let Some(group_id) = effective_group_id.as_deref() {
        sqlx::query_scalar("SELECT name FROM groups WHERE id = ?1")
            .bind(group_id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
        None
    };
    let payload = json!({
        "device_id": device_id,
        "group_id": effective_group_id.clone(),
        "config_name": config_name,
        "config_version": version,
        "config": config,
    });
    let device_token: String = sqlx::query_scalar("SELECT device_token FROM devices WHERE id = ?1")
        .bind(device_id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let push_version = version as u32;
    let master = crypto::device_push_key_material(device_id, &device_token);
    let (encrypted_config, nonce) = crypto::encrypt_parts(
        &master,
        &crypto::device_push_context(device_id, push_version),
        &payload.to_string(),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query(
        "INSERT INTO config_pushes(id, device_id, group_id, config_snapshot, pushed_at, acked)
         VALUES(?1, ?2, ?3, ?4, ?5, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(device_id)
    .bind(effective_group_id)
    .bind(&encrypted_config)
    .bind(db::now())
    .execute(&state.db.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Some(tx) = state.clients.read().await.get(device_id) {
        let _ = tx.send(ServerMessage::ConfigPush {
            version: push_version,
            encrypted_config,
            nonce,
        });
        Ok(true)
    } else {
        Ok(false)
    }
}

#[derive(Deserialize)]
struct EventQuery {
    device_id: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct ImportQuery {
    mode: Option<String>,
}

#[derive(Deserialize)]
struct BackupQuery {
    scope: Option<String>,
}

async fn list_events(
    State(state): State<AppState>,
    user: AuthUser,
    axum::extract::Query(query): axum::extract::Query<EventQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let events = if user.role == "admin" {
        state
            .db
            .list_events(
                query.device_id.as_deref(),
                query.event_type.as_deref(),
                limit,
            )
            .await
    } else {
        state
            .db
            .list_events_for_owner(
                &user.id,
                query.device_id.as_deref(),
                query.event_type.as_deref(),
                limit,
            )
            .await
    }
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(events))
}

async fn events_stream(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if user.role != "admin" {
        return Err(StatusCode::FORBIDDEN);
    }
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|item| {
        Some(Ok(Event::default().data(match item {
            Ok(v) => v,
            Err(e) => json!({ "error": e.to_string() }).to_string(),
        })))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn export_backup(
    State(state): State<AppState>,
    user: AuthUser,
    axum::extract::Query(query): axum::extract::Query<BackupQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let scope = query.scope.unwrap_or_else(|| {
        if user.role == "admin" {
            "full".into()
        } else {
            "user".into()
        }
    });
    let data = match scope.as_str() {
        "full" => {
            if user.role != "admin" {
                return Err(StatusCode::FORBIDDEN);
            }
            state.db.export_backup().await
        }
        "user" => state.db.export_user_backup(&user.id).await,
        _ => return Err(StatusCode::BAD_REQUEST),
    }
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(BackupEnvelope {
        format_version: 1,
        exported_at: db::now(),
        source_mode: "sqlite".into(),
        scope,
        data,
    }))
}

async fn import_backup(
    State(state): State<AppState>,
    user: AuthUser,
    axum::extract::Query(query): axum::extract::Query<ImportQuery>,
    Json(backup): Json<BackupEnvelope>,
) -> Result<impl IntoResponse, StatusCode> {
    if user.role != "admin" {
        return Err(StatusCode::FORBIDDEN);
    }
    if backup.format_version != 1 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let overwrite = query.mode.as_deref() == Some("overwrite");
    let report = state
        .db
        .import_backup(backup.data, overwrite, backup.source_mode, backup.scope)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(report))
}

async fn ensure_room_access(
    state: &AppState,
    user: &AuthUser,
    room_id: &str,
) -> Result<(), StatusCode> {
    if user.role == "admin" {
        if state
            .db
            .room_exists(room_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        {
            return Ok(());
        }
        return Err(StatusCode::NOT_FOUND);
    }
    if state
        .db
        .room_belongs_to(room_id, &user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

fn validate_username_password(username: &str, password: &str) -> Result<(), StatusCode> {
    let username_ok = username.len() >= 3
        && username.len() <= 32
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if !username_ok {
        return Err(StatusCode::BAD_REQUEST);
    }
    validate_password(password)
}

fn validate_password(password: &str) -> Result<(), StatusCode> {
    if password.len() < 6 || password.len() > 128 {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(())
}

async fn ensure_device_in_room(
    state: &AppState,
    room_id: &str,
    device_id: &str,
) -> Result<(), StatusCode> {
    let exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE id = ?1 AND room_id = ?2")
            .bind(device_id)
            .bind(room_id)
            .fetch_one(&state.db.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if exists > 0 {
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn ensure_group_in_room(
    state: &AppState,
    room_id: &str,
    group_id: &str,
) -> Result<(), StatusCode> {
    let exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM groups WHERE id = ?1 AND room_id = ?2")
            .bind(group_id)
            .bind(room_id)
            .fetch_one(&state.db.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if exists > 0 {
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

fn login_failure_key(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("local")
        .to_string()
}

async fn record_login_failure(state: &AppState, key: &str) {
    let now = db::now();
    let mut failures = state.login_failures.lock().await;
    let entry = failures
        .entry(key.to_string())
        .or_insert(crate::state::LoginFailure {
            count: 0,
            locked_until: 0,
        });
    if entry.locked_until <= now {
        entry.count += 1;
    }
    if entry.count >= 3 {
        entry.locked_until = now + 10 * 60;
        entry.count = 0;
    }
}
