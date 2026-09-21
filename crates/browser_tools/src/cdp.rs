use anyhow::{Context, Result, anyhow};
use async_tungstenite::client_async;
use async_tungstenite::tungstenite::{Error as TungsteniteError, Message};
use futures::channel::oneshot;
use futures::{Sink, SinkExt, Stream, StreamExt};
use gpui::BackgroundExecutor;
use serde_json::{Value, json};
use smol::channel::{Receiver, Sender, unbounded};
use smol::net::TcpStream;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use url::Url;

/// An error returned by the Chrome DevTools Protocol client.
#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    #[error("CDP error {code}: {message}")]
    Protocol { code: i64, message: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Maximum number of events buffered per session before the oldest drop.
const MAX_BUFFERED_EVENTS_PER_SESSION: usize = 500;

/// A command queued for delivery over the browser-level websocket.
struct Outgoing {
    request: String,
}

/// Events fanned out by flattened session id; browser-level events use `None`.
#[derive(Default)]
struct EventBuffer {
    browser: VecDeque<Value>,
    by_session: HashMap<String, VecDeque<Value>>,
}

impl EventBuffer {
    fn push(&mut self, event: Value) {
        let queue = match event.get("sessionId").and_then(Value::as_str) {
            Some(session_id) => self.by_session.entry(session_id.to_string()).or_default(),
            None => &mut self.browser,
        };
        if queue.len() >= MAX_BUFFERED_EVENTS_PER_SESSION {
            queue.pop_front();
        }
        queue.push_back(event);
    }

    fn recent_events(&self) -> Vec<Value> {
        let mut events = self.browser.iter().cloned().collect::<Vec<_>>();
        for queue in self.by_session.values() {
            events.extend(queue.iter().cloned());
        }
        events
    }

    fn clear(&mut self) {
        self.browser.clear();
        self.by_session.clear();
    }
}

/// A CDP client over a single browser-level websocket connection.
///
/// Commands are sent serially; a pair of background tasks owns the socket and
/// continuously drains incoming messages, routing responses back to the
/// awaiting command and fanning events out by `sessionId`. Capture is
/// continuous rather than a side effect of the next command.
pub struct CdpClient {
    outgoing: Sender<Outgoing>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    next_id: Arc<AtomicU64>,
    events: Arc<Mutex<EventBuffer>>,
}

impl CdpClient {
    /// Connect to a CDP websocket endpoint (the browser-level endpoint).
    pub async fn connect(endpoint: &str, executor: BackgroundExecutor) -> Result<Self> {
        let url = Url::parse(endpoint).context("invalid CDP websocket url")?;
        let host = url.host_str().context("CDP websocket url has no host")?;
        let port = url
            .port_or_known_default()
            .context("CDP websocket url has no port")?;
        let stream = TcpStream::connect((host, port)).await?;
        let (socket, _) = client_async(endpoint, stream).await?;
        let (sink, stream) = socket.split();

        let (outgoing, incoming) = unbounded::<Outgoing>();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let events = Arc::new(Mutex::new(EventBuffer::default()));

        executor.spawn(write_loop(sink, incoming)).detach();
        executor
            .spawn(read_loop(stream, pending.clone(), events.clone()))
            .detach();

        Ok(Self {
            outgoing,
            pending,
            next_id: Arc::new(AtomicU64::new(0)),
            events,
        })
    }

    /// Send a browser-level command (not scoped to a flattened target session).
    pub async fn send_command(&self, method: &str, params: Value) -> Result<Value> {
        self.send_command_with_session(None, method, params).await
    }

    /// Send a command scoped to a flattened target session, or to the browser
    /// when `session_id` is `None`.
    pub async fn send_command_with_session(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = session_id {
            request["sessionId"] = json!(session_id);
        }

        let (responder, response) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, responder);
        if self
            .outgoing
            .send(Outgoing {
                request: request.to_string(),
            })
            .await
            .is_err()
        {
            self.pending.lock().unwrap().remove(&id);
            return Err(anyhow!("failed to send CDP command: connection closed"));
        }
        response
            .await
            .map_err(|_| anyhow!("CDP connection closed while awaiting response"))?
    }

    /// Borrow the most recent buffered events without draining them.
    pub fn recent_events(&self) -> Vec<Value> {
        self.events.lock().unwrap().recent_events()
    }

    /// Drop all buffered events so a cleared capture is not re-ingested on the
    /// next store sync.
    pub fn clear_events(&self) {
        self.events.lock().unwrap().clear();
    }
}

async fn write_loop<S>(mut sink: S, incoming: Receiver<Outgoing>)
where
    S: Sink<Message, Error = TungsteniteError> + Unpin + Send + 'static,
{
    while let Ok(command) = incoming.recv().await {
        if sink
            .send(Message::Text(command.request.into()))
            .await
            .is_err()
        {
            break;
        }
    }
    let _ = sink.close().await;
}

async fn read_loop<S>(
    mut stream: S,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    events: Arc<Mutex<EventBuffer>>,
) where
    S: Stream<Item = Result<Message, TungsteniteError>> + Unpin + Send + 'static,
{
    while let Some(message) = stream.next().await {
        let Ok(message) = message else { break };
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(id) = value.get("id").and_then(Value::as_u64) {
            let responder = pending.lock().unwrap().remove(&id);
            if let Some(responder) = responder {
                let result = if let Some(error) = value.get("error") {
                    let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown CDP error")
                        .to_string();
                    Err(CdpError::Protocol { code, message }.into())
                } else {
                    Ok(value.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = responder.send(result);
            }
        } else if value.get("method").is_some() {
            events.lock().unwrap().push(value);
        }
    }

    let mut pending = pending.lock().unwrap();
    for (_, responder) in pending.drain() {
        let _ = responder.send(Err(anyhow!("CDP connection closed")));
    }
}
