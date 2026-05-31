//! A minimal async LSP/JSON-RPC client over a child process's stdio.
//!
//! The client spawns a language server, writes requests and notifications to
//! its stdin, and runs a background task reading its stdout: responses are
//! routed back to the awaiting caller, and the server-to-client requests jdtls
//! issues during startup (`workspace/configuration`, `client/registerCapability`,
//! progress creation, …) are answered with sensible defaults.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, broadcast, oneshot};

use crate::error::{LspError, Result};
use crate::framing::{read_message, write_message};

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value>>>>>;

/// A server-to-client notification: its method and params.
pub type Notification = (String, Value);

/// A handle to a running language server.
pub struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    pending: Pending,
    child: Arc<Mutex<Child>>,
    notifications: broadcast::Sender<Notification>,
}

impl LspClient {
    /// Spawn `command` as a language server and start serving its I/O.
    pub fn spawn(mut command: Command) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| LspError::Spawn(e.to_string()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Spawn("no stdin pipe".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Spawn("no stdout pipe".into()))?;
        let stderr = child.stderr.take();

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (notifications, _) = broadcast::channel(256);

        // Reader: route responses, answer server-to-client requests, broadcast
        // notifications.
        {
            let stdin = Arc::clone(&stdin);
            let pending = Arc::clone(&pending);
            let notifications = notifications.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_message(&mut reader).await {
                        Ok(Some(msg)) => {
                            handle_incoming(msg, &stdin, &pending, &notifications).await
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "lsp read error");
                            break;
                        }
                    }
                }
                fail_pending(&pending).await;
            });
        }

        // Forward the server's stderr to tracing for diagnostics.
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "language_server", "{line}");
                }
            });
        }

        Ok(Self {
            stdin,
            next_id: AtomicI64::new(1),
            pending,
            child: Arc::new(Mutex::new(child)),
            notifications,
        })
    }

    /// Subscribe to server-to-client notifications. Subscribe before issuing
    /// requests that trigger the notifications you care about, since only
    /// notifications received after subscribing are delivered.
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.notifications.subscribe()
    }

    /// Send a request and await its typed result.
    pub async fn request<P, R>(&self, method: &str, params: P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write(&message).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        let value = rx.await.map_err(|_| LspError::Closed)??;
        Ok(serde_json::from_value(value)?)
    }

    /// Send a notification (no response expected).
    pub async fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<()> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write(&message).await
    }

    /// Politely shut the server down (`shutdown` + `exit`), then kill it.
    pub async fn shutdown(&self) -> Result<()> {
        let _: Result<Value> = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        Ok(())
    }

    async fn write(&self, message: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        write_message(&mut *stdin, message).await
    }
}

/// Route an incoming message: response, server-to-client request, or
/// notification.
async fn handle_incoming(
    msg: Value,
    stdin: &Arc<Mutex<ChildStdin>>,
    pending: &Pending,
    notifications: &broadcast::Sender<Notification>,
) {
    let is_response =
        msg.get("id").is_some() && (msg.get("result").is_some() || msg.get("error").is_some());

    if is_response {
        let Some(id) = msg.get("id").and_then(Value::as_i64) else {
            return;
        };
        let Some(tx) = pending.lock().await.remove(&id) else {
            return;
        };
        if let Some(err) = msg.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let _ = tx.send(Err(LspError::Response { code, message }));
        } else {
            let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
        }
        return;
    }

    // Server-to-client request: reply with a default, and broadcast it so
    // callers can observe requests like `workspace/applyEdit` (the only way some
    // servers return a refactoring's edit).
    if let (Some(method), Some(id)) = (
        msg.get("method").and_then(Value::as_str),
        msg.get("id").cloned(),
    ) {
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = default_server_response(method, msg.get("params"));
        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        {
            let mut stdin = stdin.lock().await;
            let _ = write_message(&mut *stdin, &reply).await;
        }
        let _ = notifications.send((method.to_string(), params));
        return;
    }

    // Otherwise it's a notification: broadcast it to any subscribers.
    if let Some(method) = msg.get("method").and_then(Value::as_str) {
        tracing::trace!(method, "lsp notification");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let _ = notifications.send((method.to_string(), params));
    }
}

/// The default reply to a server-to-client request we don't specifically model.
fn default_server_response(method: &str, params: Option<&Value>) -> Value {
    match method {
        // One config object per requested item; null means "use defaults".
        "workspace/configuration" => {
            let n = params
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            Value::Array(vec![Value::Null; n])
        }
        // Accept edits the server asks the client to apply.
        "workspace/applyEdit" => json!({ "applied": true }),
        // registerCapability, progress creation, etc. expect a null result.
        _ => Value::Null,
    }
}

/// Fail every outstanding request when the connection drops.
async fn fail_pending(pending: &Pending) {
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(LspError::Closed));
    }
}
