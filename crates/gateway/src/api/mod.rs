//! API 层：网关面（API Key）与管理面（管理员会话）。

pub mod admin;
pub mod auth;
pub mod images;
pub mod v1;

use std::sync::Arc;

use crate::state::AppState;

pub type SharedState = Arc<AppState>;

#[derive(Debug, serde::Serialize)]
pub struct HealthResp {
    pub ok: bool,
    pub version: &'static str,
    pub nodes: usize,
    pub models: usize,
    pub accounts: usize,
}
