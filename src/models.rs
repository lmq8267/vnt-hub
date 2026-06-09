use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub access_token: String,
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateRoomRequest {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct Room {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct Device {
    pub id: String,
    pub room_id: String,
    pub group_id: Option<String>,
    pub display_name: String,
    pub client_version: Option<String>,
    pub console_public_ip: Option<String>,
    pub network_ip: Option<String>,
    pub network_node_id: Option<String>,
    pub last_seen: Option<i64>,
    pub status: String,
    pub vnts_status: Option<String>,
    pub vnts_error: Option<String>,
    pub vnts_updated_at: Option<i64>,
    pub config_version: i64,
    pub up_stream: i64,
    pub down_stream: i64,
    pub traffic_updated_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct PublicConfig {
    pub allow_register: bool,
    pub console_version: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDeviceRequest {
    pub display_name: Option<String>,
    pub group_id: Option<String>,
    pub network_ip: Option<String>,
    pub network_node_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
    pub config: VntClientConfig,
}

#[derive(Debug, Deserialize)]
pub struct UpdateGroupRequest {
    pub name: Option<String>,
    pub config: Option<VntClientConfig>,
}

#[derive(Debug, Serialize)]
pub struct Group {
    pub id: String,
    pub room_id: String,
    pub name: String,
    pub config_version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<VntClientConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VntClientConfig {
    pub tap: bool,
    pub token: String,
    pub device_id: String,
    pub name: String,
    pub server_address: String,
    pub stun_server: Vec<String>,
    pub dns: Vec<String>,
    pub in_ips: Vec<String>,
    pub out_ips: Vec<String>,
    pub password: Option<String>,
    pub mtu: Option<u32>,
    pub tcp: bool,
    pub ip: Option<String>,
    pub use_channel: String,
    pub no_proxy: bool,
    pub server_encrypt: bool,
    pub cipher_model: Option<String>,
    pub finger: bool,
    pub punch_model: String,
    pub ports: Option<Vec<u16>>,
    pub cmd: bool,
    pub first_latency: bool,
    pub device_name: Option<String>,
    pub packet_loss: Option<f64>,
    pub packet_delay: u32,
    pub mapping: Vec<String>,
    pub compressor: Option<String>,
    pub vnt_mapping: Vec<String>,
    pub disable_stats: bool,
    pub allow_wire_guard: bool,
    pub local_dev: Option<String>,
    pub disable_relay: bool,
    pub hook: Option<String>,
}

impl Default for VntClientConfig {
    fn default() -> Self {
        Self {
            tap: false,
            token: String::new(),
            device_id: String::new(),
            name: String::new(),
            server_address: "vnt.wherewego.top:29872".into(),
            stun_server: vec![
                "stun.miwifi.com:3478".into(),
                "stun.chat.bilibili.com:3478".into(),
                "stun.hitv.com:3478".into(),
                "stun.cdnbye.com:3478".into(),
            ],
            dns: vec![],
            in_ips: vec![],
            out_ips: vec![],
            password: None,
            mtu: None,
            tcp: false,
            ip: None,
            use_channel: "all".into(),
            no_proxy: false,
            server_encrypt: false,
            cipher_model: None,
            finger: false,
            punch_model: "all".into(),
            ports: None,
            cmd: false,
            first_latency: false,
            device_name: None,
            packet_loss: None,
            packet_delay: 0,
            mapping: vec![],
            compressor: None,
            vnt_mapping: vec![],
            disable_stats: false,
            allow_wire_guard: false,
            local_dev: None,
            disable_relay: false,
            hook: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    Hello {
        room_id: String,
        device_id: Option<String>,
        device_token: Option<String>,
        device_name: String,
        protocol_version: u32,
        #[serde(default)]
        client_version: Option<String>,
    },
    ConfigState {
        config_version: u32,
        group_id: Option<String>,
        config_name: Option<String>,
        config: VntClientConfig,
        running: bool,
    },
    EventReport {
        event_type: String,
        payload: serde_json::Value,
        timestamp: u64,
    },
    ConfigAck {
        config_version: u32,
    },
    Heartbeat {
        timestamp: u64,
    },
    Disconnect {
        timestamp: u64,
    },
    TrafficStats {
        up_stream: u64,
        down_stream: u64,
        timestamp: u64,
    },
}

#[derive(Debug, Serialize)]
pub struct EventItem {
    pub id: String,
    pub device_id: String,
    pub event_type: String,
    pub payload: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BackupEnvelope {
    pub format_version: u32,
    pub exported_at: i64,
    pub source_mode: String,
    #[serde(default = "default_backup_scope")]
    pub scope: String,
    pub data: BackupData,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct BackupData {
    pub users: Vec<serde_json::Value>,
    pub rooms: Vec<serde_json::Value>,
    pub groups: Vec<serde_json::Value>,
    pub devices: Vec<serde_json::Value>,
    pub events: Vec<serde_json::Value>,
    pub config_pushes: Vec<serde_json::Value>,
    pub system_config: Vec<serde_json::Value>,
}

fn default_backup_scope() -> String {
    "full".into()
}

#[derive(Debug, Serialize)]
pub struct BackupImportReport {
    pub ok: bool,
    pub mode: String,
    pub source_mode: String,
    pub scope: String,
    pub tables: Vec<BackupImportTableReport>,
}

#[derive(Debug, Serialize)]
pub struct BackupImportTableReport {
    pub table: String,
    pub attempted: u64,
    pub imported: u64,
    pub skipped: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    HelloAck {
        device_id: String,
        device_token: Option<String>,
        status: String,
        console_version: String,
    },
    ConfigPush {
        version: u32,
        encrypted_config: String,
        nonce: String,
    },
    Kick {
        reason: String,
    },
    Heartbeat {
        timestamp: u64,
    },
}
