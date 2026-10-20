pub mod capabilities;
pub mod close;
pub mod interrupt;
pub mod launch;
pub mod list;
pub mod read;
pub mod run;
pub mod send;
pub mod wait;

use rmcp::model::CallToolResult;
use serde_json::{Value, json};

pub fn failure(code: &str, message: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({"code": code, "message": message}))
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
