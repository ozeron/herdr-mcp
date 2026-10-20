use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::client::LineReader;
use super::error::{ApiErrorCode, HerdrError};
use super::types::EventEnvelope;

#[derive(Debug)]
pub enum SubscriptionEvent {
    Event(EventEnvelope),
    EventsLost,
    Closed,
    Error(HerdrError),
}

#[derive(Debug)]
pub struct Subscription {
    rx: mpsc::UnboundedReceiver<SubscriptionEvent>,
    task: JoinHandle<()>,
}

impl Subscription {
    pub(crate) fn spawn(reader: LineReader) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(drain(reader, tx));
        Self { rx, task }
    }

    pub async fn next(&mut self) -> Option<SubscriptionEvent> {
        self.rx.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn drain(mut reader: LineReader, tx: mpsc::UnboundedSender<SubscriptionEvent>) {
    loop {
        let line = match reader.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                let _ = tx.send(SubscriptionEvent::Closed);
                return;
            }
            Err(err) => {
                let _ = tx.send(SubscriptionEvent::Error(err));
                return;
            }
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                let _ = tx.send(SubscriptionEvent::Error(HerdrError::protocol(format!(
                    "invalid json in event stream: {err}"
                ))));
                continue;
            }
        };
        if let Some(error) = value.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let event = if ApiErrorCode::parse(code) == ApiErrorCode::EventsLost {
                SubscriptionEvent::EventsLost
            } else {
                SubscriptionEvent::Error(HerdrError::Api {
                    code: ApiErrorCode::parse(code),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                })
            };
            let _ = tx.send(event);
            return;
        }
        let event = match serde_json::from_value::<EventEnvelope>(value) {
            Ok(envelope) => SubscriptionEvent::Event(envelope),
            Err(err) => SubscriptionEvent::Error(HerdrError::incompatible(format!(
                "cannot decode event: {err}"
            ))),
        };
        if tx.send(event).is_err() {
            return;
        }
    }
}
