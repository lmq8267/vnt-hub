use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::async_trait;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use base64::{engine::general_purpose, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub username: String,
    pub role: String,
    pub exp: usize,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: String,
    pub username: String,
    pub role: String,
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("{:?}", e))?
        .to_string())
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

pub fn jwt_secret() -> String {
    std::env::var("VNT_HUB_JWT_SECRET").unwrap_or_else(|_| "vnt-hub-dev-jwt-secret".into())
}

pub fn sign(user_id: &str, username: &str, role: &str) -> anyhow::Result<String> {
    let exp = (chrono::Utc::now().timestamp() + 24 * 3600) as usize;
    let claims = Claims {
        sub: user_id.into(),
        username: username.into(),
        role: role.into(),
        exp,
    };
    let header = general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let signing_input = format!("{}.{}", header, payload);
    let signature = general_purpose::URL_SAFE_NO_PAD.encode(hmac_sha256(
        jwt_secret().as_bytes(),
        signing_input.as_bytes(),
    ));
    Ok(format!("{}.{}", signing_input, signature))
}

#[async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let token = auth
            .strip_prefix("Bearer ")
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let claims = verify_token(token).map_err(|_| StatusCode::UNAUTHORIZED)?;
        Ok(AuthUser {
            id: claims.sub,
            username: claims.username,
            role: claims.role,
        })
    }
}

fn verify_token(token: &str) -> anyhow::Result<Claims> {
    let mut parts = token.split('.');
    let header = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("jwt header missing"))?;
    let payload = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("jwt payload missing"))?;
    let signature = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("jwt signature missing"))?;
    if parts.next().is_some() {
        anyhow::bail!("invalid jwt parts");
    }

    let header_value: serde_json::Value =
        serde_json::from_slice(&general_purpose::URL_SAFE_NO_PAD.decode(header)?)?;
    if header_value.get("alg").and_then(|v| v.as_str()) != Some("HS256") {
        anyhow::bail!("unsupported jwt alg");
    }

    let signing_input = format!("{}.{}", header, payload);
    let expected = hmac_sha256(jwt_secret().as_bytes(), signing_input.as_bytes());
    let actual = general_purpose::URL_SAFE_NO_PAD.decode(signature)?;
    if !constant_time_eq(&expected, &actual) {
        anyhow::bail!("invalid jwt signature");
    }

    let claims: Claims =
        serde_json::from_slice(&general_purpose::URL_SAFE_NO_PAD.decode(payload)?)?;
    let now = chrono::Utc::now().timestamp() as usize;
    if claims.exp < now {
        anyhow::bail!("jwt expired");
    }
    Ok(claims)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut normalized_key = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        normalized_key[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized_key[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= normalized_key[i];
        opad[i] ^= normalized_key[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    outer.finalize().into()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b) {
        diff |= left ^ right;
    }
    diff == 0
}
