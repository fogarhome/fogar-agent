//! Account REST bodies: login, `/auth/me`, scopes and the API error body.
//! CERTAIN (5.0, M22): gateway `docs/client-protocol.md` §0 «Iniciar sesión», «Permisos» and
//! `models/admin_schemas.py` at 290efa0. HTTP itself lives in an adapter.

use alloc::{borrow::Cow, string::String, vec::Vec};
use core::fmt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pairing::ClientType;

/// A device permission. CERTAIN (5.0, M22). `Other` is a scope this client does not know yet
/// (read leniently); it is never written: serializing it is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    #[serde(rename = "conversations:read")]
    ConversationsRead,
    /// Needed for `/ws/stream` (else close 4011). Includes `conversations:read`.
    #[serde(rename = "conversations:write")]
    ConversationsWrite,
    #[serde(rename = "home:read")]
    HomeRead,
    #[serde(rename = "home:admin")]
    HomeAdmin,
    #[serde(other, skip_serializing)]
    Other,
}

/// `POST /auth/login`. Absent options are not sent. `Debug` redacts the password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest<'a> {
    #[serde(borrow)]
    pub username: Cow<'a, str>,
    #[serde(borrow)]
    pub password: Cow<'a, str>,
    #[serde(borrow)]
    pub device_name: Cow<'a, str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_type: Option<ClientType>,
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<Cow<'a, str>>>,
    /// Without it the gateway grants `conversations:write`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<Scope>>,
}

impl fmt::Debug for LoginRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginRequest")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("device_name", &self.device_name)
            .field("client_type", &self.client_type)
            .field("capabilities", &self.capabilities)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// `201` of `POST /auth/login`. `token` arrives only here, once: persist it, never log it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct LoginResponse {
    pub client_id: String,
    pub token: String,
    #[serde(default)]
    pub scopes: Vec<Scope>,
    pub user: User,
    /// The doc always shows it; the gateway code sends `null` when the device has no home.
    #[serde(default)]
    pub home: Option<Home>,
}

impl fmt::Debug for LoginResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginResponse")
            .field("client_id", &self.client_id)
            .field("token", &"<redacted>")
            .field("scopes", &self.scopes)
            .field("user", &self.user)
            .field("home", &self.home)
            .finish()
    }
}

/// `role` stays a string (`owner`, `admin`, `member`; may grow). `Debug` hides `display_name`
/// (personal data).
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub role: String,
}

impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("display_name", &"<redacted>")
            .field("role", &self.role)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Home {
    pub id: String,
    pub name: String,
}

/// `GET /auth/me` (and the reply of `PATCH /auth/me`). `device` is not documented: ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Me {
    pub user: User,
    #[serde(default)]
    pub home: Option<Home>,
    /// The effective scopes of this device.
    #[serde(default)]
    pub scopes: Vec<Scope>,
    #[serde(default)]
    pub notify_client_id: Option<String>,
}

/// `PATCH /auth/me`: choose this person's notification device (`422 invalid_notify_device`).
/// `None` is sent as `null`: it clears the notification device (gateway `MeUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateMe<'a> {
    pub notify_client_id: Option<Cow<'a, str>>,
}

/// Any non-2xx JSON body: `{"detail": "<code>"}`, or FastAPI's 422 list.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ApiError {
    pub detail: Value,
}

impl ApiError {
    /// The error code when `detail` is a string (`missing_scope`, `invalid_credentials`…).
    pub fn code(&self) -> Option<&str> {
        self.detail.as_str()
    }
}
