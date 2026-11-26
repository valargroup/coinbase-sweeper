//! A blocking zcashd-style JSON-RPC client.
//!
//! Reads and submissions are separate on purpose. A read may be retried because it changes
//! nothing. A submission (`z_shieldcoinbase`, `z_sendmany`) is sent exactly once: when its answer
//! is lost, the caller must find out what the wallet did instead of sending again.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::config::Credentials;

/// zcashd's "wallet has insufficient funds" / "nothing to shield" code.
pub const RPC_WALLET_INSUFFICIENT_FUNDS: i64 = -6;
/// zcashd's "still loading" code during startup.
pub const RPC_IN_WARMUP: i64 = -28;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// HTTP 401 or 403: the RPC user or password is wrong.
    Auth(u16),
    /// The wallet answered with a JSON-RPC error.
    Rpc { code: i64, message: String },
    /// The request never reached the wallet (connection refused, unknown host).
    Unreachable(String),
    /// The request may have reached the wallet but no answer arrived (timeout, dropped
    /// connection). For a submission the outcome is unknown.
    NoAnswer(String),
    /// An HTTP 5xx with no JSON-RPC body: the wallet (or a proxy in front of it) is overloaded
    /// or restarting. For a submission the outcome is unknown.
    Busy(String),
    /// The wallet answered with something that is not a JSON-RPC response.
    Protocol(String),
}

impl RpcError {
    pub fn code(&self) -> Option<i64> {
        match self {
            RpcError::Rpc { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// True when the wallet is unreachable, silent, busy or still starting: a reason to skip
    /// the cycle and try again later, never to refuse.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            RpcError::Unreachable(_) | RpcError::NoAnswer(_) | RpcError::Busy(_)
        ) || self.code() == Some(RPC_IN_WARMUP)
    }

    /// For a submission: true when the wallet may have acted on it even though no usable answer
    /// came back. Such a call must be settled by looking at the wallet, never resent.
    pub fn outcome_unknown(&self) -> bool {
        matches!(
            self,
            RpcError::NoAnswer(_) | RpcError::Busy(_) | RpcError::Protocol(_)
        )
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::Auth(status) => write!(
                f,
                "the wallet refused the RPC credentials (HTTP {status}); check rpc.user, \
                 rpc.cookie_file or SWEEPER_RPC_PASSWORD"
            ),
            RpcError::Rpc { code, message } => write!(f, "wallet error {code}: {message}"),
            RpcError::Unreachable(e) => write!(f, "wallet RPC unreachable: {e}"),
            RpcError::NoAnswer(e) => write!(f, "no answer from the wallet RPC: {e}"),
            RpcError::Busy(e) => write!(f, "the wallet RPC is busy or restarting: {e}"),
            RpcError::Protocol(e) => write!(f, "unexpected wallet RPC response: {e}"),
        }
    }
}

impl std::error::Error for RpcError {}

pub struct Rpc {
    read_agent: ureq::Agent,
    submit_agent: ureq::Agent,
    url: String,
    auth_header: Mutex<String>,
    /// Re-read after a 401/403: zcashd writes a new cookie each time it starts.
    cookie_file: Option<PathBuf>,
    read_retries: u32,
    next_id: AtomicU64,
    calls: AtomicU64,
}

impl Rpc {
    pub fn new(
        url: &str,
        credentials: &Credentials,
        cookie_file: Option<PathBuf>,
        read_timeout: Duration,
        submit_timeout: Duration,
        read_retries: u32,
    ) -> Rpc {
        let agent = |timeout| {
            ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5).min(timeout))
                .timeout(timeout)
                .build()
        };
        Rpc {
            read_agent: agent(read_timeout),
            submit_agent: agent(submit_timeout),
            url: url.to_owned(),
            auth_header: Mutex::new(basic_auth(credentials)),
            cookie_file,
            read_retries,
            next_id: AtomicU64::new(1),
            calls: AtomicU64::new(0),
        }
    }

    /// How many HTTP requests this client has made, for the per-cycle cost log.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// A call that changes nothing in the wallet. Retried with capped backoff when the wallet
    /// cannot be reached or does not answer.
    pub fn read(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut attempt = 0;
        loop {
            match self.call(&self.read_agent, method, &params) {
                Err(e) if e.is_transient() && attempt < self.read_retries => {
                    let delay = Duration::from_millis(250 << attempt.min(4));
                    log::debug!("rpc retry method={method} attempt={attempt} err={e}");
                    std::thread::sleep(delay);
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// A call that may move funds. Sent once; never retried here.
    pub fn submit(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call(&self.submit_agent, method, &params)
    }

    fn call(&self, agent: &ureq::Agent, method: &str, params: &Value) -> Result<Value, RpcError> {
        match self.call_once(agent, method, params) {
            // The wallet refused the request before acting on it, so one retry with a freshly
            // read cookie is safe even for a submission.
            Err(RpcError::Auth(status)) if self.reload_cookie() => {
                log::info!("event=rpc_cookie_reloaded after_status={status}");
                self.call_once(agent, method, params)
            }
            other => other,
        }
    }

    fn reload_cookie(&self) -> bool {
        let Some(path) = &self.cookie_file else {
            return false;
        };
        match crate::config::read_cookie(path) {
            Ok(c) => {
                let fresh = basic_auth(&c);
                let mut header = self.auth_header.lock().expect("no panics while held");
                let changed = *header != fresh;
                *header = fresh;
                changed
            }
            Err(e) => {
                log::warn!("event=rpc_cookie_unreadable err={e:#}");
                false
            }
        }
    }

    fn call_once(
        &self,
        agent: &ureq::Agent,
        method: &str,
        params: &Value,
    ) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "1.0", "id": id, "method": method, "params": params});
        let auth = self
            .auth_header
            .lock()
            .expect("no panics while held")
            .clone();
        let response = agent
            .post(&self.url)
            .set("Authorization", &auth)
            .set("Content-Type", "application/json")
            .send_json(body);
        let text = match response {
            Ok(r) => r
                .into_string()
                .map_err(|e| RpcError::NoAnswer(format!("{method}: reading the body: {e}")))?,
            Err(ureq::Error::Status(status @ (401 | 403), _)) => {
                return Err(RpcError::Auth(status))
            }
            // zcashd and zecd answer a failed call with an HTTP error status and a JSON-RPC body.
            Err(ureq::Error::Status(status, r)) => {
                let text = r.into_string().unwrap_or_default();
                return Err(parse_error_body(&text).unwrap_or_else(|| {
                    let detail = format!("{method}: HTTP {status}: {}", snippet(&text));
                    if status >= 500 {
                        RpcError::Busy(detail)
                    } else {
                        RpcError::Protocol(detail)
                    }
                }));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(match t.kind() {
                    ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns => {
                        RpcError::Unreachable(format!("{method}: {t}"))
                    }
                    _ => RpcError::NoAnswer(format!("{method}: {t}")),
                })
            }
        };
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| RpcError::Protocol(format!("{method}: {e}: {}", snippet(&text))))?;
        if let Some(err) = error_of(&v) {
            return Err(err);
        }
        match v.get("result") {
            Some(result) => Ok(result.clone()),
            None => Err(RpcError::Protocol(format!(
                "{method}: no result field: {}",
                snippet(&text)
            ))),
        }
    }
}

fn basic_auth(c: &Credentials) -> String {
    let token =
        base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", c.user, c.password));
    format!("Basic {token}")
}

fn parse_error_body(text: &str) -> Option<RpcError> {
    error_of(&serde_json::from_str(text).ok()?)
}

fn error_of(v: &Value) -> Option<RpcError> {
    let e = v.get("error")?;
    if e.is_null() {
        return None;
    }
    Some(RpcError::Rpc {
        code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
        message: e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn snippet(text: &str) -> String {
    text.chars().take(200).collect()
}
