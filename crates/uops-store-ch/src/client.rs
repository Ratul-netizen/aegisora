//! The `ClickHouse` HTTP client.
//!
//! One POST per statement, SQL as the body, parameters as `param_<name>` in the query
//! string — the same protocol `uops-ch-migrate` speaks, in its async form. That crate
//! runs once at deploy time and can block a thread; this one runs on every request and
//! cannot.
//!
//! Built on `hyper` because `axum` already pulls exactly these crates: no new licences,
//! no extra download, nothing added to the SBOM. No TLS, like every other client here —
//! `ClickHouse` is reached over a private network and a proxy terminates TLS where a
//! deployment needs it.

use std::fmt::Write as _;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::error::{Error, Result};

/// Where the server is and who to be.
#[derive(Clone, Debug)]
pub struct ChConfig {
    pub url: String,
    pub database: String,
    pub user: String,
    pub password: String,
    /// Killed server-side. A telemetry query that never returns holds a connection
    /// until somebody notices, and the UI has given up long before that.
    pub max_execution_time: Duration,
}

impl Default for ChConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8123".into(),
            database: "uops".into(),
            user: "default".into(),
            password: String::new(),
            max_execution_time: Duration::from_secs(30),
        }
    }
}

impl ChConfig {
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            url: std::env::var("CLICKHOUSE_URL").unwrap_or(d.url),
            database: std::env::var("CLICKHOUSE_DB").unwrap_or(d.database),
            user: std::env::var("CLICKHOUSE_USER").unwrap_or(d.user),
            password: std::env::var("CLICKHOUSE_PASSWORD").unwrap_or(d.password),
            ..d
        }
    }
}

/// What the server reports about the work a statement did.
///
/// Read from `X-ClickHouse-Summary`. Rows read is the number that matters and the one
/// W1 was written around: latency tells you a query was slow, rows read tells you
/// *why* — whether it pruned or scanned the tenant. It also becomes the `row_count` on
/// the access-log row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub rows_read: u64,
    pub bytes_read: u64,
    pub rows_returned: u64,
}

/// One statement's result.
#[derive(Clone, Debug)]
pub struct Raw {
    pub body: String,
    pub summary: Summary,
}

/// An async `ClickHouse` client.
#[derive(Clone)]
pub struct ChClient {
    http: Client<HttpConnector, Full<Bytes>>,
    config: ChConfig,
}

impl std::fmt::Debug for ChClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the password. Same reasoning as PgStore and HttpExecutor.
        f.debug_struct("ChClient")
            .field("url", &self.config.url)
            .field("database", &self.config.database)
            .field("user", &self.config.user)
            .finish_non_exhaustive()
    }
}

impl ChClient {
    #[must_use]
    pub fn new(config: ChConfig) -> Self {
        Self {
            http: Client::builder(TokioExecutor::new()).build_http(),
            config,
        }
    }

    #[must_use]
    pub const fn config(&self) -> &ChConfig {
        &self.config
    }

    /// Send one statement with bound parameters.
    ///
    /// `params` are `ClickHouse` query parameters — `{name:Type}` in the SQL,
    /// `param_name` in the query string. Nothing is ever formatted into the statement;
    /// that is the whole point of `uops-query` emitting placeholders.
    pub async fn run(&self, sql: &str, params: &[(&str, String)]) -> Result<Raw> {
        let mut url = format!(
            "{}/?database={}&user={}&password={}&max_execution_time={}",
            self.config.url.trim_end_matches('/'),
            encode(&self.config.database),
            encode(&self.config.user),
            encode(&self.config.password),
            self.config.max_execution_time.as_secs(),
        );
        for (name, value) in params {
            let _ = write!(url, "&param_{}={}", encode(name), encode(value));
        }

        let request = Request::builder()
            .method("POST")
            .uri(&url)
            .body(Full::new(Bytes::from(sql.to_owned())))
            .map_err(|e| Error::Request(e.to_string()))?;

        let response = self
            .http
            .request(request)
            .await
            .map_err(|e| Error::Unreachable {
                url: self.config.url.clone(),
                detail: e.to_string(),
            })?;

        let status = response.status();
        let summary = summary_of(response.headers());

        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::Protocol(e.to_string()))?
            .to_bytes();
        let body = String::from_utf8_lossy(&body).into_owned();

        if !status.is_success() {
            // ClickHouse error bodies are several lines of stack; the first says what
            // is wrong, and the rest is noise in a log line.
            let first = body.lines().next().unwrap_or(&body).trim().to_owned();
            return Err(Error::Server {
                status: status.as_u16(),
                message: first,
            });
        }

        Ok(Raw { body, summary })
    }
}

fn summary_of(headers: &hyper::HeaderMap) -> Summary {
    let Some(raw) = headers
        .get("x-clickhouse-summary")
        .and_then(|v| v.to_str().ok())
    else {
        return Summary::default();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Summary::default();
    };

    // The values arrive as strings, which is a ClickHouse quirk rather than a mistake:
    // they can exceed what JSON numbers represent exactly.
    let number = |key: &str| -> u64 {
        parsed
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };

    Summary {
        rows_read: number("read_rows"),
        bytes_read: number("read_bytes"),
        rows_returned: number("result_rows"),
    }
}

/// Percent-encode a query-string value.
///
/// Hand-written for the same reason as the cookie parser: this escapes a value into a
/// query string and nothing more. Everything outside the unreserved set is encoded,
/// which is stricter than necessary and cannot be wrong.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_that_would_end_the_query_string_is_encoded() {
        // Parameters carry user input — a hostname from a syslog message, a search
        // term. One containing & or = would otherwise become extra query-string
        // arguments, which is the URL's version of SQL injection.
        assert_eq!(encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(
            encode("2026-09-01 00:00:00.000"),
            "2026-09-01%2000%3A00%3A00.000"
        );
        assert_eq!(encode("plain-value_1.0~"), "plain-value_1.0~");
    }

    #[test]
    fn a_summary_header_is_parsed_and_a_missing_one_is_not_fatal() {
        // Rows read is what W1 was written around: latency says a query was slow, rows
        // read says whether it pruned or scanned the whole tenant.
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            "x-clickhouse-summary",
            r#"{"read_rows":"16380","read_bytes":"1048576","result_rows":"100"}"#
                .parse()
                .unwrap(),
        );
        let summary = summary_of(&headers);
        assert_eq!(summary.rows_read, 16_380);
        assert_eq!(summary.bytes_read, 1_048_576);
        assert_eq!(summary.rows_returned, 100);

        // An older server, or a statement that reports nothing, must not fail the query.
        assert_eq!(summary_of(&hyper::HeaderMap::new()), Summary::default());
    }

    #[test]
    fn a_malformed_summary_is_ignored_rather_than_fatal() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-clickhouse-summary", "not json".parse().unwrap());
        assert_eq!(summary_of(&headers), Summary::default());
    }

    #[test]
    fn debug_output_cannot_leak_the_password() {
        let client = ChClient::new(ChConfig {
            password: "hunter2".into(),
            ..ChConfig::default()
        });
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("localhost"), "{rendered}");
    }
}
