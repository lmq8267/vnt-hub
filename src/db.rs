use anyhow::Context;
use base64::{engine::general_purpose, Engine};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Column, Row, SqlitePool};
use std::str::FromStr;
use uuid::Uuid;

use crate::auth;
use crate::crypto;
use crate::models::{
    BackupData, BackupImportReport, BackupImportTableReport, Device, EventItem, Group, Room,
    UpdateDeviceRequest, UpdateGroupRequest, VntClientConfig,
};

#[derive(Clone)]
pub struct Database {
    pub pool: SqlitePool,
}

impl Database {
    pub async fn connect(path: &str) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    pub async fn init(&self) -> anyhow::Result<()> {
        for sql in SCHEMA.split(";\n") {
            let sql = sql.trim();
            if !sql.is_empty() {
                sqlx::query(sql).execute(&self.pool).await?;
            }
        }
        self.ensure_device_traffic_columns().await?;
        Ok(())
    }

    async fn ensure_device_traffic_columns(&self) -> anyhow::Result<()> {
        let columns = sqlx::query("PRAGMA table_info(devices)")
            .fetch_all(&self.pool)
            .await?;
        let names = columns
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect::<std::collections::HashSet<_>>();
        for (name, definition) in [
            ("up_stream", "INTEGER DEFAULT 0"),
            ("down_stream", "INTEGER DEFAULT 0"),
            ("traffic_updated_at", "INTEGER"),
        ] {
            if !names.contains(name) {
                sqlx::query(&format!(
                    "ALTER TABLE devices ADD COLUMN {} {}",
                    name, definition
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }

    pub async fn ensure_admin(&self) -> anyhow::Result<()> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE username = 'admin'")
            .fetch_one(&self.pool)
            .await?;
        if count == 0 {
            let now = now();
            let password = auth::hash_password("admin")?;
            sqlx::query(
                "INSERT INTO users(id, username, password, role, created_at) VALUES(?1, 'admin', ?2, 'admin', ?3)",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(password)
            .bind(now)
            .execute(&self.pool)
            .await?;
            log::warn!("默认 admin/admin 已创建，请登录后立即修改密码");
        }
        Ok(())
    }

    pub async fn ensure_tls_cert(&self) -> anyhow::Result<()> {
        if self.system_get("tls_cert").await?.is_some()
            && self.system_get("tls_key").await?.is_some()
        {
            return Ok(());
        }
        let CertifiedKey { cert, key_pair } = generate_simple_self_signed(vec![
            "localhost".into(),
            "127.0.0.1".into(),
            "0.0.0.0".into(),
            "::1".into(),
        ])?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();
        self.system_set("tls_cert", &general_purpose::STANDARD.encode(cert_pem))
            .await?;
        self.system_set("tls_key", &general_purpose::STANDARD.encode(key_pem))
            .await?;
        Ok(())
    }

    pub async fn tls_pem(&self) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let cert = self
            .system_get("tls_cert")
            .await?
            .context("tls_cert missing")?;
        let key = self
            .system_get("tls_key")
            .await?
            .context("tls_key missing")?;
        Ok((
            general_purpose::STANDARD.decode(cert)?,
            general_purpose::STANDARD.decode(key)?,
        ))
    }

    pub async fn system_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM system_config WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    pub async fn system_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO system_config(key, value, updated_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn find_user(
        &self,
        username: &str,
    ) -> anyhow::Result<Option<(String, String, String)>> {
        let row = sqlx::query("SELECT id, password, role FROM users WHERE username = ?1")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| (r.get(0), r.get(1), r.get(2))))
    }

    pub async fn create_room(&self, owner_id: &str, name: &str) -> anyhow::Result<Room> {
        for _ in 0..5 {
            let id = crypto::room_id();
            let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE id = ?1")
                .bind(&id)
                .fetch_one(&self.pool)
                .await?;
            if exists != 0 {
                continue;
            }
            let now = now();
            sqlx::query("INSERT INTO rooms(id, name, owner_id, created_at) VALUES(?1, ?2, ?3, ?4)")
                .bind(&id)
                .bind(name)
                .bind(owner_id)
                .bind(now)
                .execute(&self.pool)
                .await?;
            return Ok(Room {
                id,
                name: name.into(),
                owner_id: owner_id.into(),
                created_at: now,
            });
        }
        anyhow::bail!("room id conflict");
    }

    pub async fn list_rooms(&self, owner_id: &str) -> anyhow::Result<Vec<Room>> {
        let rows = sqlx::query("SELECT id, name, owner_id, created_at FROM rooms WHERE owner_id = ?1 ORDER BY created_at DESC")
            .bind(owner_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| Room {
                id: r.get(0),
                name: r.get(1),
                owner_id: r.get(2),
                created_at: r.get(3),
            })
            .collect())
    }

    pub async fn room_exists(&self, room_id: &str) -> anyhow::Result<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE id = ?1")
            .bind(room_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count > 0)
    }

    pub async fn room_belongs_to(&self, room_id: &str, user_id: &str) -> anyhow::Result<bool> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE id = ?1 AND owner_id = ?2")
                .bind(room_id)
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(count > 0)
    }

    pub async fn delete_room(&self, room_id: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM config_pushes WHERE device_id IN (SELECT id FROM devices WHERE room_id = ?1)")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM events WHERE device_id IN (SELECT id FROM devices WHERE room_id = ?1)",
        )
        .bind(room_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM devices WHERE room_id = ?1")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM groups WHERE room_id = ?1")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM rooms WHERE id = ?1")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn upsert_device_hello(
        &self,
        room_id: &str,
        device_id: Option<&str>,
        device_token: Option<&str>,
        device_name: &str,
    ) -> anyhow::Result<(String, Option<String>, String)> {
        if let (Some(id), Some(token)) = (device_id, device_token) {
            let row = sqlx::query(
                "SELECT device_token, status FROM devices WHERE id = ?1 AND room_id = ?2",
            )
            .bind(id)
            .bind(room_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(row) = row else {
                anyhow::bail!("device auth failed");
            };
            let saved: String = row.get(0);
            if saved != token {
                anyhow::bail!("device auth failed");
            }
            let status: String = row.get(1);
            sqlx::query("UPDATE devices SET display_name = ?1, last_seen = ?2, status = CASE WHEN status = 'kicked' THEN status ELSE 'online' END WHERE id = ?3")
                .bind(device_name)
                .bind(now())
                .bind(id)
                .execute(&self.pool)
                .await?;
            return Ok((id.into(), None, status));
        }

        let id = Uuid::new_v4().to_string();
        let token = crypto::random_base64(32);
        let now = now();
        sqlx::query(
            "INSERT INTO devices(id, room_id, display_name, device_token, last_seen, status, config_version, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, 'pending', 0, ?5)",
        )
        .bind(&id)
        .bind(room_id)
        .bind(device_name)
        .bind(&token)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok((id, Some(token), "pending".into()))
    }

    pub async fn list_devices(&self, room_id: &str) -> anyhow::Result<Vec<Device>> {
        let rows = sqlx::query(
            "SELECT id, room_id, group_id, display_name, network_ip, network_node_id, last_seen, status, config_version,
                    COALESCE(up_stream, 0), COALESCE(down_stream, 0), traffic_updated_at, created_at
             FROM devices WHERE room_id = ?1 ORDER BY created_at DESC",
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(device_from_row).collect())
    }

    pub async fn update_device(
        &self,
        room_id: &str,
        device_id: &str,
        req: UpdateDeviceRequest,
    ) -> anyhow::Result<()> {
        let current = sqlx::query(
            "SELECT display_name, group_id, network_ip, network_node_id FROM devices WHERE id = ?1 AND room_id = ?2",
        )
        .bind(device_id)
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = current else {
            anyhow::bail!("device not found");
        };
        let display_name: String = req.display_name.unwrap_or_else(|| row.get(0));
        let group_id: Option<String> = req.group_id.or_else(|| row.get(1));
        let network_ip: Option<String> = req.network_ip.or_else(|| row.get(2));
        let network_node_id: Option<String> = req.network_node_id.or_else(|| row.get(3));
        sqlx::query(
            "UPDATE devices SET display_name = ?1, group_id = ?2, network_ip = ?3, network_node_id = ?4 WHERE id = ?5 AND room_id = ?6",
        )
        .bind(display_name)
        .bind(group_id)
        .bind(network_ip)
        .bind(network_node_id)
        .bind(device_id)
        .bind(room_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_device(&self, room_id: &str, device_id: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM config_pushes WHERE device_id = ?1")
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM events WHERE device_id = ?1")
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM devices WHERE id = ?1 AND room_id = ?2")
            .bind(device_id)
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn regenerate_device_token(
        &self,
        room_id: &str,
        device_id: &str,
    ) -> anyhow::Result<String> {
        let token = crypto::random_base64(32);
        sqlx::query("UPDATE devices SET device_token = ?1 WHERE id = ?2 AND room_id = ?3")
            .bind(&token)
            .bind(device_id)
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        Ok(token)
    }

    pub async fn create_group(
        &self,
        room_id: &str,
        name: &str,
        config: &VntClientConfig,
    ) -> anyhow::Result<Group> {
        let id = Uuid::new_v4().to_string();
        let now = now();
        let master = crypto::master_key();
        let encrypted_token =
            crypto::encrypt_to_base64(&master, &format!("group:{}:token", id), &config.token)?;
        let encrypted_server = crypto::encrypt_to_base64(
            &master,
            &format!("group:{}:server", id),
            &config.server_address,
        )?;
        let encrypted_password = encrypt_optional_field(
            &master,
            &format!("group:{}:password", id),
            config.password.as_deref(),
        )?;
        let extra_config = encrypted_extra_config(&master, &id, config)?;
        sqlx::query(
            "INSERT INTO groups(id, room_id, name, encrypted_token, encrypted_server, encrypted_password, extra_config, config_version, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)",
        )
        .bind(&id)
        .bind(room_id)
        .bind(name)
        .bind(encrypted_token)
        .bind(encrypted_server)
        .bind(encrypted_password)
        .bind(extra_config)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(Group {
            id,
            room_id: room_id.into(),
            name: name.into(),
            config_version: 1,
            created_at: now,
            updated_at: now,
            config: Some(config.clone()),
        })
    }

    pub async fn list_groups(&self, room_id: &str) -> anyhow::Result<Vec<Group>> {
        let rows = sqlx::query(
            "SELECT id, room_id, name, config_version, created_at, updated_at, encrypted_token, encrypted_server, encrypted_password, extra_config
             FROM groups WHERE room_id = ?1 ORDER BY created_at DESC",
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await?;
        let mut groups = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.get(0);
            let config =
                self.decrypt_group_config(&id, row.get(6), row.get(7), row.get(8), row.get(9))?;
            groups.push(Group {
                id,
                room_id: row.get(1),
                name: row.get(2),
                config_version: row.get(3),
                created_at: row.get(4),
                updated_at: row.get(5),
                config,
            });
        }
        Ok(groups)
    }

    pub async fn update_group(
        &self,
        room_id: &str,
        group_id: &str,
        req: UpdateGroupRequest,
    ) -> anyhow::Result<Group> {
        let row = sqlx::query(
            "SELECT name, config_version, created_at, encrypted_token, encrypted_server, encrypted_password, extra_config FROM groups WHERE id = ?1 AND room_id = ?2",
        )
        .bind(group_id)
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            anyhow::bail!("group not found");
        };
        let name: String = req.name.unwrap_or_else(|| row.get(0));
        let version: i64 = row.get::<i64, _>(1) + 1;
        let created_at: i64 = row.get(2);
        let current_config =
            self.decrypt_group_config(group_id, row.get(3), row.get(4), row.get(5), row.get(6))?;
        let now = now();
        let master = crypto::master_key();
        let encrypted_token = req
            .config
            .as_ref()
            .map(|v| {
                crypto::encrypt_to_base64(&master, &format!("group:{}:token", group_id), &v.token)
            })
            .transpose()?;
        let encrypted_server = req
            .config
            .as_ref()
            .map(|v| {
                crypto::encrypt_to_base64(
                    &master,
                    &format!("group:{}:server", group_id),
                    &v.server_address,
                )
            })
            .transpose()?;
        let encrypted_password = if let Some(config) = req.config.as_ref() {
            encrypt_optional_field(
                &master,
                &format!("group:{}:password", group_id),
                config.password.as_deref(),
            )?
        } else {
            row.get(5)
        };
        let extra_config = req
            .config
            .as_ref()
            .map(|v| encrypted_extra_config(&master, group_id, v))
            .transpose()?;
        sqlx::query(
            "UPDATE groups SET
             name = ?1,
             encrypted_token = COALESCE(?2, encrypted_token),
             encrypted_server = COALESCE(?3, encrypted_server),
             encrypted_password = ?4,
             extra_config = COALESCE(?5, extra_config),
             config_version = ?6,
             updated_at = ?7
             WHERE id = ?8 AND room_id = ?9",
        )
        .bind(&name)
        .bind(encrypted_token)
        .bind(encrypted_server)
        .bind(encrypted_password)
        .bind(extra_config)
        .bind(version)
        .bind(now)
        .bind(group_id)
        .bind(room_id)
        .execute(&self.pool)
        .await?;
        Ok(Group {
            id: group_id.into(),
            room_id: room_id.into(),
            name,
            config_version: version,
            created_at,
            updated_at: now,
            config: req.config.or(current_config),
        })
    }

    fn decrypt_group_config(
        &self,
        group_id: &str,
        encrypted_token: Option<String>,
        encrypted_server: Option<String>,
        encrypted_password: Option<String>,
        extra_config: Option<String>,
    ) -> anyhow::Result<Option<VntClientConfig>> {
        let Some(extra_config) = extra_config else {
            return Ok(None);
        };
        let master = crypto::master_key();
        let extra_json = crypto::decrypt_from_base64(
            &master,
            &format!("group:{}:config", group_id),
            &extra_config,
        )?;
        let mut config = serde_json::from_str::<VntClientConfig>(&extra_json)?;
        if let Some(v) = encrypted_token {
            config.token =
                crypto::decrypt_from_base64(&master, &format!("group:{}:token", group_id), &v)?;
        }
        if let Some(v) = encrypted_server {
            config.server_address =
                crypto::decrypt_from_base64(&master, &format!("group:{}:server", group_id), &v)?;
        }
        if let Some(v) = encrypted_password {
            config.password = Some(crypto::decrypt_from_base64(
                &master,
                &format!("group:{}:password", group_id),
                &v,
            )?);
        }
        Ok(Some(config))
    }

    pub async fn device_config(
        &self,
        device_id: &str,
        override_group_id: Option<&str>,
    ) -> anyhow::Result<Option<(Option<String>, i64, VntClientConfig)>> {
        let row = sqlx::query(
            "SELECT d.id, d.group_id, d.display_name, d.network_ip, d.network_node_id,
                    d.config_version, g.config_version, g.encrypted_token, g.encrypted_server, g.encrypted_password, g.extra_config
             FROM devices d LEFT JOIN groups g ON d.group_id = g.id
             WHERE d.id = ?1",
        )
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };

        let mut effective_group_id: Option<String> = override_group_id
            .map(str::to_string)
            .or_else(|| row.get::<Option<String>, _>(1));
        let mut group_version: Option<i64> = row.get(6);
        let mut encrypted_token: Option<String> = row.get(7);
        let mut encrypted_server: Option<String> = row.get(8);
        let mut encrypted_password: Option<String> = row.get(9);
        let mut extra_config: Option<String> = row.get(10);

        if let Some(group_id) = override_group_id {
            let group_row =
                sqlx::query("SELECT config_version, encrypted_token, encrypted_server, encrypted_password, extra_config FROM groups WHERE id = ?1")
                    .bind(group_id)
                    .fetch_optional(&self.pool)
                    .await?;
            let Some(group_row) = group_row else {
                anyhow::bail!("group not found");
            };
            group_version = Some(group_row.get(0));
            encrypted_token = group_row.get(1);
            encrypted_server = group_row.get(2);
            encrypted_password = group_row.get(3);
            extra_config = group_row.get(4);
            effective_group_id = Some(group_id.to_string());
        }

        let version = group_version.unwrap_or_else(|| row.get::<i64, _>(5) + 1);
        let mut config = if let Some(group_id) = effective_group_id.as_deref() {
            self.decrypt_group_config(
                group_id,
                encrypted_token,
                encrypted_server,
                encrypted_password,
                extra_config,
            )?
            .unwrap_or_default()
        } else {
            VntClientConfig::default()
        };

        let display_name: String = row.get(2);
        let network_ip: Option<String> = row.get(3);
        let network_node_id: Option<String> = row.get(4);
        config.device_id = network_node_id.unwrap_or_else(|| device_id.to_string());
        config.name = display_name;
        if let Some(ip) = network_ip {
            config.ip = Some(ip);
        }

        Ok(Some((effective_group_id, version, config)))
    }

    pub async fn delete_group(&self, room_id: &str, group_id: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE devices SET group_id = NULL WHERE room_id = ?1 AND group_id = ?2")
            .bind(room_id)
            .bind(group_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM groups WHERE id = ?1 AND room_id = ?2")
            .bind(group_id)
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_device_group(
        &self,
        room_id: &str,
        group_id: Option<&str>,
        device_id: &str,
    ) -> anyhow::Result<()> {
        if let Some(group_id) = group_id {
            let exists: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM groups WHERE id = ?1 AND room_id = ?2")
                    .bind(group_id)
                    .bind(room_id)
                    .fetch_one(&self.pool)
                    .await?;
            if exists == 0 {
                anyhow::bail!("group not found");
            }
        }
        let result = sqlx::query("UPDATE devices SET group_id = ?1 WHERE id = ?2 AND room_id = ?3")
            .bind(group_id)
            .bind(device_id)
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!("device not found");
        }
        Ok(())
    }

    pub async fn update_device_traffic(
        &self,
        device_id: &str,
        up_stream: u64,
        down_stream: u64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE devices
             SET up_stream = ?1, down_stream = ?2, traffic_updated_at = ?3, last_seen = ?3
             WHERE id = ?4",
        )
        .bind(up_stream as i64)
        .bind(down_stream as i64)
        .bind(now())
        .bind(device_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_events(
        &self,
        device_id: Option<&str>,
        event_type: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<EventItem>> {
        let rows = match (device_id, event_type) {
            (Some(device_id), Some(event_type)) => {
                sqlx::query("SELECT id, device_id, event_type, payload, created_at FROM events WHERE device_id = ?1 AND event_type = ?2 ORDER BY created_at DESC LIMIT ?3")
                    .bind(device_id)
                    .bind(event_type)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
            (Some(device_id), None) => {
                sqlx::query("SELECT id, device_id, event_type, payload, created_at FROM events WHERE device_id = ?1 ORDER BY created_at DESC LIMIT ?2")
                    .bind(device_id)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
            (None, Some(event_type)) => {
                sqlx::query("SELECT id, device_id, event_type, payload, created_at FROM events WHERE event_type = ?1 ORDER BY created_at DESC LIMIT ?2")
                    .bind(event_type)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
            (None, None) => {
                sqlx::query("SELECT id, device_id, event_type, payload, created_at FROM events ORDER BY created_at DESC LIMIT ?1")
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        Ok(rows
            .into_iter()
            .map(|r| EventItem {
                id: r.get(0),
                device_id: r.get(1),
                event_type: r.get(2),
                payload: r.get(3),
                created_at: r.get(4),
            })
            .collect())
    }

    pub async fn list_events_for_owner(
        &self,
        owner_id: &str,
        device_id: Option<&str>,
        event_type: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<EventItem>> {
        let rows = match (device_id, event_type) {
            (Some(device_id), Some(event_type)) => {
                sqlx::query("SELECT e.id, e.device_id, e.event_type, e.payload, e.created_at FROM events e JOIN devices d ON e.device_id = d.id JOIN rooms r ON d.room_id = r.id WHERE r.owner_id = ?1 AND e.device_id = ?2 AND e.event_type = ?3 ORDER BY e.created_at DESC LIMIT ?4")
                    .bind(owner_id)
                    .bind(device_id)
                    .bind(event_type)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
            (Some(device_id), None) => {
                sqlx::query("SELECT e.id, e.device_id, e.event_type, e.payload, e.created_at FROM events e JOIN devices d ON e.device_id = d.id JOIN rooms r ON d.room_id = r.id WHERE r.owner_id = ?1 AND e.device_id = ?2 ORDER BY e.created_at DESC LIMIT ?3")
                    .bind(owner_id)
                    .bind(device_id)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
            (None, Some(event_type)) => {
                sqlx::query("SELECT e.id, e.device_id, e.event_type, e.payload, e.created_at FROM events e JOIN devices d ON e.device_id = d.id JOIN rooms r ON d.room_id = r.id WHERE r.owner_id = ?1 AND e.event_type = ?2 ORDER BY e.created_at DESC LIMIT ?3")
                    .bind(owner_id)
                    .bind(event_type)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
            (None, None) => {
                sqlx::query("SELECT e.id, e.device_id, e.event_type, e.payload, e.created_at FROM events e JOIN devices d ON e.device_id = d.id JOIN rooms r ON d.room_id = r.id WHERE r.owner_id = ?1 ORDER BY e.created_at DESC LIMIT ?2")
                    .bind(owner_id)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        Ok(rows
            .into_iter()
            .map(|r| EventItem {
                id: r.get(0),
                device_id: r.get(1),
                event_type: r.get(2),
                payload: r.get(3),
                created_at: r.get(4),
            })
            .collect())
    }

    pub async fn export_backup(&self) -> anyhow::Result<BackupData> {
        Ok(BackupData {
            users: self.export_table("users").await?,
            rooms: self.export_table("rooms").await?,
            groups: self.export_table("groups").await?,
            devices: self.export_table("devices").await?,
            events: self.export_table("events").await?,
            config_pushes: self.export_table("config_pushes").await?,
            system_config: self.export_table("system_config").await?,
        })
    }

    pub async fn export_user_backup(&self, user_id: &str) -> anyhow::Result<BackupData> {
        Ok(BackupData {
            users: self
                .export_query("SELECT * FROM users WHERE id = ?1", &[user_id])
                .await?,
            rooms: self
                .export_query("SELECT * FROM rooms WHERE owner_id = ?1", &[user_id])
                .await?,
            groups: self
                .export_query(
                    "SELECT g.* FROM groups g JOIN rooms r ON g.room_id = r.id WHERE r.owner_id = ?1",
                    &[user_id],
                )
                .await?,
            devices: self
                .export_query(
                    "SELECT d.* FROM devices d JOIN rooms r ON d.room_id = r.id WHERE r.owner_id = ?1",
                    &[user_id],
                )
                .await?,
            events: self
                .export_query(
                    "SELECT e.* FROM events e JOIN devices d ON e.device_id = d.id JOIN rooms r ON d.room_id = r.id WHERE r.owner_id = ?1",
                    &[user_id],
                )
                .await?,
            config_pushes: self
                .export_query(
                    "SELECT c.* FROM config_pushes c JOIN devices d ON c.device_id = d.id JOIN rooms r ON d.room_id = r.id WHERE r.owner_id = ?1",
                    &[user_id],
                )
                .await?,
            system_config: Vec::new(),
        })
    }

    async fn export_table(&self, table: &str) -> anyhow::Result<Vec<serde_json::Value>> {
        let sql = format!("SELECT * FROM {}", table);
        self.export_query(&sql, &[]).await
    }

    async fn export_query(
        &self,
        sql: &str,
        params: &[&str],
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut query = sqlx::query(sql);
        for param in params {
            query = query.bind(*param);
        }
        let rows = query.fetch_all(&self.pool).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut map = serde_json::Map::new();
            for column in row.columns() {
                let name = column.name();
                let value = if let Ok(v) = row.try_get::<String, _>(name) {
                    serde_json::Value::String(v)
                } else if let Ok(v) = row.try_get::<i64, _>(name) {
                    serde_json::Value::Number(v.into())
                } else {
                    serde_json::Value::Null
                };
                map.insert(name.to_string(), value);
            }
            out.push(serde_json::Value::Object(map));
        }
        Ok(out)
    }

    pub async fn import_backup(
        &self,
        data: BackupData,
        overwrite: bool,
        source_mode: String,
        scope: String,
    ) -> anyhow::Result<BackupImportReport> {
        let mut tx = self.pool.begin().await?;
        if overwrite {
            for table in [
                "config_pushes",
                "events",
                "devices",
                "groups",
                "rooms",
                "sessions",
                "users",
                "system_config",
            ] {
                let sql = format!("DELETE FROM {}", table);
                sqlx::query(&sql).execute(&mut *tx).await?;
            }
        }
        let mut tables = Vec::new();
        tables.push(
            import_rows(
                &mut tx,
                "users",
                &["id", "username", "password", "role", "created_at"],
                data.users,
                !overwrite,
                overwrite,
            )
            .await?,
        );
        tables.push(
            import_rows(
                &mut tx,
                "rooms",
                &["id", "name", "owner_id", "created_at"],
                data.rooms,
                false,
                overwrite,
            )
            .await?,
        );
        tables.push(
            import_rows(
                &mut tx,
                "groups",
                &[
                    "id",
                    "room_id",
                    "name",
                    "encrypted_token",
                    "encrypted_server",
                    "encrypted_password",
                    "extra_config",
                    "config_version",
                    "created_at",
                    "updated_at",
                ],
                data.groups,
                false,
                overwrite,
            )
            .await?,
        );
        tables.push(
            import_rows(
                &mut tx,
                "devices",
                &[
                    "id",
                    "room_id",
                    "group_id",
                    "display_name",
                    "network_ip",
                    "network_node_id",
                    "device_token",
                    "last_seen",
                    "status",
                    "config_version",
                    "up_stream",
                    "down_stream",
                    "traffic_updated_at",
                    "created_at",
                ],
                data.devices,
                false,
                overwrite,
            )
            .await?,
        );
        tables.push(
            import_rows(
                &mut tx,
                "events",
                &["id", "device_id", "event_type", "payload", "created_at"],
                data.events,
                false,
                overwrite,
            )
            .await?,
        );
        tables.push(
            import_rows(
                &mut tx,
                "config_pushes",
                &[
                    "id",
                    "device_id",
                    "group_id",
                    "config_snapshot",
                    "pushed_at",
                    "acked",
                ],
                data.config_pushes,
                false,
                overwrite,
            )
            .await?,
        );
        tables.push(
            import_rows(
                &mut tx,
                "system_config",
                &["key", "value", "updated_at"],
                data.system_config,
                false,
                overwrite,
            )
            .await?,
        );
        tx.commit().await?;
        Ok(BackupImportReport {
            ok: true,
            mode: if overwrite { "overwrite" } else { "merge" }.into(),
            source_mode,
            scope,
            tables,
        })
    }
}

async fn import_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    columns: &[&str],
    rows: Vec<serde_json::Value>,
    skip_admin_user: bool,
    overwrite: bool,
) -> anyhow::Result<BackupImportTableReport> {
    let attempted = rows.len() as u64;
    let mut imported = 0;
    let mut skipped = 0;
    for row in rows {
        let Some(obj) = row.as_object() else {
            skipped += 1;
            continue;
        };
        if skip_admin_user && table == "users" {
            if obj
                .get("username")
                .and_then(|v| v.as_str())
                .map(|v| v == "admin")
                .unwrap_or(false)
            {
                skipped += 1;
                continue;
            }
        }
        let placeholders = (1..=columns.len())
            .map(|i| format!("?{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let insert_mode = if overwrite {
            "INSERT OR REPLACE"
        } else {
            "INSERT OR IGNORE"
        };
        let sql = format!(
            "{} INTO {}({}) VALUES({})",
            insert_mode,
            table,
            columns.join(", "),
            placeholders
        );
        let mut query = sqlx::query(&sql);
        for column in columns {
            let value = obj.get(*column).unwrap_or(&serde_json::Value::Null);
            if let Some(v) = value.as_i64() {
                query = query.bind(v);
            } else if let Some(v) = value.as_u64() {
                query = query.bind(v as i64);
            } else if let Some(v) = value.as_str() {
                query = query.bind(v.to_string());
            } else if value.is_null() {
                query = query.bind(Option::<String>::None);
            } else {
                query = query.bind(value.to_string());
            }
        }
        let result = query.execute(&mut **tx).await?;
        if result.rows_affected() > 0 {
            imported += 1;
        } else {
            skipped += 1;
        }
    }
    Ok(BackupImportTableReport {
        table: table.into(),
        attempted,
        imported,
        skipped,
    })
}

fn encrypted_extra_config(
    master: &str,
    group_id: &str,
    config: &VntClientConfig,
) -> anyhow::Result<String> {
    let mut extra = config.clone();
    extra.token.clear();
    extra.server_address.clear();
    extra.password = None;
    let json = serde_json::to_string(&extra)?;
    crypto::encrypt_to_base64(master, &format!("group:{}:config", group_id), &json)
}

fn encrypt_optional_field(
    master: &str,
    context: &str,
    value: Option<&str>,
) -> anyhow::Result<Option<String>> {
    value
        .map(|v| crypto::encrypt_to_base64(master, context, v))
        .transpose()
}

fn device_from_row(r: sqlx::sqlite::SqliteRow) -> Device {
    Device {
        id: r.get(0),
        room_id: r.get(1),
        group_id: r.get(2),
        display_name: r.get(3),
        network_ip: r.get(4),
        network_node_id: r.get(5),
        last_seen: r.get(6),
        status: r.get(7),
        config_version: r.get(8),
        up_stream: r.get(9),
        down_stream: r.get(10),
        traffic_updated_at: r.get(11),
        created_at: r.get(12),
    }
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    username TEXT UNIQUE NOT NULL,
    password TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'user',
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS rooms (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(owner_id) REFERENCES users(id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_rooms_id ON rooms(id);
CREATE TABLE IF NOT EXISTS groups (
    id TEXT PRIMARY KEY,
    room_id TEXT NOT NULL,
    name TEXT NOT NULL,
    encrypted_token TEXT,
    encrypted_server TEXT,
    encrypted_password TEXT,
    extra_config TEXT,
    config_version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY(room_id) REFERENCES rooms(id)
);
CREATE TABLE IF NOT EXISTS devices (
    id TEXT PRIMARY KEY,
    room_id TEXT NOT NULL,
    group_id TEXT,
    display_name TEXT NOT NULL,
    network_ip TEXT,
    network_node_id TEXT,
    device_token TEXT NOT NULL,
    last_seen INTEGER,
    status TEXT NOT NULL DEFAULT 'pending',
    config_version INTEGER NOT NULL DEFAULT 0,
    up_stream INTEGER DEFAULT 0,
    down_stream INTEGER DEFAULT 0,
    traffic_updated_at INTEGER,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(room_id) REFERENCES rooms(id),
    FOREIGN KEY(group_id) REFERENCES groups(id)
);
CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(device_id) REFERENCES devices(id)
);
CREATE TABLE IF NOT EXISTS config_pushes (
    id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL,
    group_id TEXT,
    config_snapshot TEXT NOT NULL,
    pushed_at INTEGER NOT NULL,
    acked INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(device_id) REFERENCES devices(id)
);
CREATE TABLE IF NOT EXISTS sessions (
    token TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    FOREIGN KEY(user_id) REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS system_config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
"#;
