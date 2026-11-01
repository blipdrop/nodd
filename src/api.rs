use reqwest::blocking::Client;
use reqwest::Method;
use serde_json::Value;
use std::fmt;
use std::time::Duration;

pub const DEFAULT_API: &str = "https://api.nodd.blipdrop.net";
const MAX_ATTEMPTS: u32 = 4;
const MAX_WAIT_SECONDS: u64 = 30;

#[derive(Debug)]
pub enum ApiError {
    /// 届いたか分からない失敗。
    Network(String),
    Status { status: u16, code: String, message: String, retry_after: Option<u64> },
}

impl ApiError {
    /// 月間上限のように待っても戻らないものは再送しない。
    pub fn is_transient(&self) -> bool {
        match self {
            ApiError::Network(_) => true,
            ApiError::Status { status, code, .. } => {
                *status >= 500 || (*status == 429 && code == "rate_limited")
            }
        }
    }

    fn wait(&self, attempt: u32) -> Duration {
        let hinted = match self {
            ApiError::Status { retry_after: Some(seconds), .. } => *seconds,
            _ => 1 << attempt.min(4),
        };
        Duration::from_secs(hinted.min(MAX_WAIT_SECONDS))
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Network(message) => write!(f, "Cannot reach the server: {message}"),
            ApiError::Status { status, code, message, .. } => write!(f, "{message} ({status} {code})"),
        }
    }
}

pub struct Reply {
    pub body: Value,
    pub retry_after: Option<u64>,
}

pub struct Api {
    http: Client,
    base: String,
}

/// キーをBearerで送るので、平文の接続は手元の検証だけに許す。
pub fn normalize_base(raw: &str) -> Result<String, String> {
    let url = url::Url::parse(raw).map_err(|e| format!("Invalid API URL: {e}"))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    if url.scheme() != "https" && !local {
        return Err("API URL must use https".to_string());
    }
    Ok(raw.trim_end_matches('/').to_string())
}

/// パスへ埋める値。IDとアラートキーに使う文字だけを通す。
pub fn segment(value: &str) -> Result<&str, String> {
    let valid = !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c));
    if valid {
        Ok(value)
    } else {
        Err(format!("{value:?} contains invalid characters"))
    }
}

impl Api {
    pub fn new(base: String) -> Result<Self, String> {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("nodd-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { http, base })
    }

    pub fn call(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
        idempotency_key: Option<&str>,
    ) -> Result<Reply, ApiError> {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().map_err(|e| ApiError::Network(e.to_string()))?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        let text = response.text().map_err(|e| ApiError::Network(e.to_string()))?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(Reply { body, retry_after });
        }
        let field = |name: &str| body[name].as_str().map(str::to_string);
        Err(ApiError::Status {
            status,
            code: field("code").unwrap_or_default(),
            message: field("detail").or_else(|| field("title")).unwrap_or_else(|| "Request failed".to_string()),
            retry_after,
        })
    }

    /// 一時的な失敗だけ送り直す。POSTはIdempotency-Keyを付けて二重送信を防ぐ。
    pub fn call_retrying(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
        idempotency_key: Option<&str>,
    ) -> Result<Reply, ApiError> {
        let mut attempt = 1;
        loop {
            match self.call(method.clone(), path, bearer, body, idempotency_key) {
                Err(error) if error.is_transient() && attempt < MAX_ATTEMPTS => {
                    std::thread::sleep(error.wait(attempt));
                    attempt += 1;
                }
                result => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(status: u16, code: &str) -> ApiError {
        ApiError::Status { status, code: code.into(), message: String::new(), retry_after: None }
    }

    #[test]
    fn only_passing_failures_are_retried() {
        assert!(ApiError::Network("x".into()).is_transient());
        assert!(status(503, "maintenance").is_transient());
        assert!(status(429, "rate_limited").is_transient());
        assert!(!status(429, "monthly_limit").is_transient());
        assert!(!status(409, "open_actions_exceeded").is_transient());
        assert!(!status(401, "invalid_key").is_transient());
    }

    #[test]
    fn retry_after_is_capped() {
        let error = ApiError::Status { status: 429, code: "rate_limited".into(), message: String::new(), retry_after: Some(300) };
        assert_eq!(error.wait(1), Duration::from_secs(MAX_WAIT_SECONDS));
    }

    #[test]
    fn plain_http_is_only_for_local_use() {
        assert!(normalize_base("http://example.com").is_err());
        assert_eq!(normalize_base("http://localhost:3000/").unwrap(), "http://localhost:3000");
        assert_eq!(normalize_base("https://a.example/dev/").unwrap(), "https://a.example/dev");
    }

    #[test]
    fn path_values_cannot_escape_the_route() {
        assert!(segment("msg_01J").is_ok());
        assert!(segment("disk.full:web-1").is_ok());
        assert!(segment("../keys").is_err());
        assert!(segment("..").is_err());
        assert!(segment("a?b").is_err());
        assert!(segment("").is_err());
    }
}
