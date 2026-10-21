pub mod capabilities;
pub mod close;
pub mod interrupt;
pub mod launch;
pub mod list;
pub mod mail;
pub mod read;
pub mod run;
pub mod send;
pub mod wait;

use rmcp::RoleServer;
use rmcp::model::{CallToolResult, ProgressNotificationParam};
use rmcp::service::RequestContext;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use std::time::Duration;

use crate::wait::Progress;

/// How long a wait, readiness check or idle poll runs when the caller gives no `timeout_ms`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The asked timeout (or `default`) limited to the wait `cap`, with a note when it was cut.
pub fn clamp_timeout(
    asked_ms: Option<u64>,
    cap: Duration,
    default: Duration,
) -> (Duration, Option<String>) {
    let asked = asked_ms.map_or(default, Duration::from_millis);
    let note = (asked > cap).then(|| {
        format!(
            "timeout_ms capped at the wait cap of {} ms",
            cap.as_millis()
        )
    });
    (asked.min(cap), note)
}

/// A sink that forwards wait ticks as `notify_progress`, or `None` when the client sent no
/// progress token.
pub fn progress_sink(ctx: &RequestContext<RoleServer>) -> Option<UnboundedSender<Progress>> {
    let token = ctx.meta.get_progress_token()?;
    let (tx, mut rx) = unbounded_channel::<Progress>();
    let peer = ctx.peer.clone();
    tokio::spawn(async move {
        while let Some(tick) = rx.recv().await {
            let note = ProgressNotificationParam::new(token.clone(), tick.elapsed.as_secs_f64())
                .with_total(tick.cap.as_secs_f64())
                .with_message("still waiting");
            let _ = peer.notify_progress(note).await;
        }
    });
    Some(tx)
}

pub fn failure_with(code: &str, message: &str, details: Option<&Value>) -> CallToolResult {
    let mut body = json!({"code": code, "message": message});
    if let (Some(Value::Object(extra)), Some(fields)) = (details, body.as_object_mut()) {
        for (key, value) in extra {
            fields.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    CallToolResult::structured_error(body)
}

pub fn success(value: Value) -> CallToolResult {
    CallToolResult::structured(value)
}
