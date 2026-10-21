mod client;
mod error;
mod events;
mod session;
mod types;

pub use client::{Client, MAX_LINE_BYTES, MAX_SOCKET_PATH_BYTES, MIN_HERDR_VERSION, Outcome};
pub use error::{ApiErrorCode, HerdrError};
pub use events::{Subscription, SubscriptionEvent};
pub use session::{
    DEFAULT_SESSION_NAME, SESSION_ENV, SOCKET_PATH_ENV, SessionEnv, SessionError,
    resolve_socket_path, session_socket,
};
pub use types::*;
