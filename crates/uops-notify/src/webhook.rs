//! The webhook transport.
//!
//! A `POST` of one JSON document, with a bounded timeout and no retry. Each of those is a
//! decision:
//!
//! **No retry.** The engine re-evaluates every interval, and an alert that is still
//! firing is still firing — a transport that retried would be a second scheduler with its
//! own idea of how often, racing the first. What a failure produces instead is a row
//! saying so, with the status code in it.
//!
//! **A short timeout.** An endpoint that takes thirty seconds to answer is an endpoint
//! that will hold an evaluation slot for thirty seconds, and there are only sixteen. Five
//! seconds is longer than any healthy receiver and short enough that a hung one costs one
//! notification rather than the cycle.
//!
//! **Plain HTTP.** This workspace carries no TLS by decision — see the root `Cargo.toml`:
//! the rustls and native-tls trees carry licences outside the `cargo-deny` allow-list, and
//! every client here reaches its peer over a private network or through a proxy the
//! deployment already runs. A webhook to `https://hooks.example.com` therefore goes
//! through that egress proxy, exactly as syslog-over-TLS is terminated at one. An
//! `https://` URL is refused here rather than attempted and failed, so the reason appears
//! when the channel is configured rather than at 4am.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::Request;
use hyper::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::notification::Notification;

/// How long an endpoint has to answer.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// How much of a failing endpoint's answer is kept for the record.
const DETAIL_BYTES: usize = 300;

/// A configured webhook.
#[derive(Clone, Debug)]
pub struct Webhook {
    url: String,
    /// Extra headers, from the channel's config. A shared secret belongs in one of these,
    /// which is why the channel row says config is readable and points at the vault for
    /// anything that is not.
    headers: Vec<(String, String)>,
}

impl Webhook {
    /// Read a channel's `config`.
    ///
    /// # Errors
    ///
    /// When there is no `url`, when it is not `http://`, or when a header is not a pair of
    /// strings. All three are refused when the channel is written rather than when it is
    /// used, so the sentence arrives while somebody is still looking at the form.
    pub fn from_config(config: &serde_json::Value) -> Result<Self, String> {
        let url = config
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or("a webhook channel needs a url")?
            .trim()
            .to_owned();

        if url.starts_with("https://") {
            return Err(
                "https is not supported here: this server carries no TLS by design, so an \
                 https webhook goes through the egress proxy the deployment already runs \
                 — point this at that proxy over http://"
                    .to_owned(),
            );
        }
        if !url.starts_with("http://") {
            return Err(format!("a webhook url must start with http:// — got {url}"));
        }

        let mut headers = Vec::new();
        if let Some(map) = config.get("headers") {
            let object = map
                .as_object()
                .ok_or("a webhook's headers must be an object of name to value")?;
            for (name, value) in object {
                let value = value
                    .as_str()
                    .ok_or_else(|| format!("the header {name} must be a string"))?;
                // Validated here so a bad header is a refusal at configuration time and
                // not a delivery that silently drops it.
                HeaderName::try_from(name.as_str())
                    .map_err(|_| format!("{name} is not a header name"))?;
                HeaderValue::try_from(value)
                    .map_err(|_| format!("the value of {name} is not a header value"))?;
                headers.push((name.clone(), value.to_owned()));
            }
        }

        Ok(Self { url, headers })
    }

    /// Deliver one notification.
    ///
    /// # Errors
    ///
    /// A sentence for the record: the status code and what the endpoint said, or why it
    /// could not be reached. Never a type — the only consumer is a `detail` column that an
    /// operator reads.
    pub async fn deliver(&self, notification: &Notification) -> Result<(), String> {
        let body = serde_json::to_vec(&notification.payload())
            .map_err(|e| format!("the notification could not be encoded: {e}"))?;

        let mut request = Request::builder()
            .method("POST")
            .uri(&self.url)
            .header(CONTENT_TYPE, "application/json");

        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }

        let request = request
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| format!("the request could not be built: {e}"))?;

        let client: Client<HttpConnector, Full<Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();

        let response = tokio::time::timeout(TIMEOUT, client.request(request))
            .await
            .map_err(|_| format!("{} did not answer within {:?}", self.url, TIMEOUT))?
            .map_err(|e| format!("{} could not be reached: {e}", self.url))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        // The endpoint's own words, bounded: a receiver that returns a megabyte of HTML on
        // error would otherwise put a megabyte of HTML in a row somebody reads in a
        // terminal.
        let said = response
            .into_body()
            .collect()
            .await
            .map(|b| String::from_utf8_lossy(&b.to_bytes()).into_owned())
            .unwrap_or_default();
        let said: String = said.chars().take(DETAIL_BYTES).collect();

        Err(format!("{} answered {status}: {}", self.url, said.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_without_a_url_is_refused_when_it_is_written() {
        assert!(Webhook::from_config(&serde_json::json!({})).is_err());
    }

    #[test]
    fn an_https_webhook_says_what_to_do_instead() {
        // The failure this prevents is a channel that looks configured, is accepted, and
        // fails at 4am with a connection error nobody can interpret.
        let error =
            Webhook::from_config(&serde_json::json!({ "url": "https://hooks.example.com/x" }))
                .expect_err("https must be refused");
        assert!(error.contains("proxy"), "{error}");
    }

    #[test]
    fn a_url_that_is_not_http_at_all_is_refused() {
        for url in [
            "ftp://example.com",
            "example.com/hook",
            "javascript:alert(1)",
        ] {
            assert!(
                Webhook::from_config(&serde_json::json!({ "url": url })).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn headers_are_validated_where_somebody_can_still_fix_them() {
        // A header with a newline in its value is a request-splitting attempt or a typo,
        // and either way it must not become a delivery that silently drops it.
        assert!(
            Webhook::from_config(&serde_json::json!({
                "url": "http://example.com/hook",
                "headers": { "X-Token": "abc\r\nX-Injected: yes" }
            }))
            .is_err()
        );
        assert!(
            Webhook::from_config(&serde_json::json!({
                "url": "http://example.com/hook",
                "headers": { "Not A Header Name": "x" }
            }))
            .is_err()
        );
        assert!(
            Webhook::from_config(&serde_json::json!({
                "url": "http://example.com/hook",
                "headers": { "X-Token": 42 }
            }))
            .is_err(),
            "a header value has to be a string"
        );

        let ok = Webhook::from_config(&serde_json::json!({
            "url": "http://example.com/hook",
            "headers": { "X-Token": "abc" }
        }))
        .expect("a valid channel");
        assert_eq!(ok.headers.len(), 1);
    }
}
