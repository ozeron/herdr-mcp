use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::herdr::HerdrError;
use crate::server::Inner;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct UseParams {
    /// A session name from `herdr_sessions`: `default`, a named session, or `custom`.
    pub name: String,
}

pub async fn list(inner: &Inner) -> Result<Value, HerdrError> {
    Ok(json!({"sessions": inner.sessions.list().await}))
}

pub async fn use_session(inner: &Inner, params: UseParams) -> Result<Value, HerdrError> {
    let (previous, active) = inner.sessions.switch(&params.name).await?;
    Ok(json!({
        "active": active.name,
        "socket": active.socket_path,
        "previous": previous.name,
    }))
}
