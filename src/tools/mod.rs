pub mod capabilities;
pub mod close;
pub mod list;
pub mod read;
pub mod run;
pub mod wait;

use rmcp::model::CallToolResult;
use serde_json::{Value, json};

pub const NOT_IMPLEMENTED: &str = "not_implemented";

pub fn failure(code: &str, message: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({"code": code, "message": message}))
}

pub fn success(value: Value) -> CallToolResult {
    CallToolResult::structured(value)
}
