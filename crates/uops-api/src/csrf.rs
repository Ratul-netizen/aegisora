//! CSRF double-submit — SPEC §M0.8.
//!
//! The attack: a page on another origin makes the browser send a request to this API,
//! and the browser helpfully attaches the session cookie. The defence is to require
//! something the attacker's page cannot read or set.
//!
//! Double-submit: at login, a random token goes into a **script-readable** cookie. The
//! app reads it and echoes it in a request header. An attacker's page can cause the
//! cookie to be *sent* — that is the whole problem — but the same-origin policy stops it
//! being *read*, so it cannot produce the header.
//!
//! # Two layers, not one
//!
//! `SameSite=Lax` already blocks the cross-site POST this protects against, and the
//! mandatory `X-Uops-Tenant` header blocks the simple-form case on its own. This layer
//! does not depend on either. `SameSite` is browser behaviour and has had gaps; a header
//! check is ours and can be tested. Defence in depth means the layers fail for different
//! reasons, not that there are several of them.
//!
//! # Why only mutations
//!
//! A cross-origin read is already prevented by the same-origin policy: the browser will
//! send the request, but the attacker's page cannot see the response. What CSRF achieves
//! is a *side effect*, so the check goes where side effects are.

use axum::extract::FromRequestParts;
use axum::http::{Method, request::Parts};
use uops_secrets::session;

use crate::cookie::{self, CSRF_COOKIE};
use crate::error::ApiError;

/// The header the app echoes the cookie in.
pub const CSRF_HEADER: &str = "x-uops-csrf";

/// Mint a token. Reuses the session-token generator: same requirement, same entropy.
pub fn issue() -> Result<String, ApiError> {
    let (token, _) = session::issue().map_err(|e| ApiError::Internal(e.into()))?;
    Ok(token.expose().to_owned())
}

/// Proof that a mutating request carried a matching CSRF token.
///
/// A handler takes one to say "this changes something". It holds nothing, because the
/// value is the check having happened.
#[derive(Clone, Copy, Debug)]
pub struct CsrfChecked;

/// Generic over the state because this check reads only the request: a cookie and a
/// header. Nothing about it needs a database, and saying so in the signature is what
/// lets it be tested without one.
impl<S: Send + Sync> FromRequestParts<S> for CsrfChecked {
    type Rejection = ApiError;

    // The trait method is async; this check reads two headers and awaits nothing. That
    // it needs no I/O is the point rather than an oversight.
    #[allow(clippy::unused_async_trait_impl)]
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Safe methods are not what CSRF achieves — see the module docs.
        if matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            return Ok(Self);
        }

        let cookie = cookie::read(parts, CSRF_COOKIE).ok_or(ApiError::Forbidden(
            "this request needs a CSRF token; sign in again",
        ))?;

        let header = parts
            .headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or(ApiError::Forbidden(
                "this request needs the CSRF token echoed in X-Uops-Csrf",
            ))?;

        if !constant_time_eq(cookie.as_bytes(), header.as_bytes()) {
            return Err(ApiError::Forbidden("the CSRF token did not match"));
        }
        Ok(Self)
    }
}

/// Compare without an early exit.
///
/// The token is not a long-lived secret and a timing attack on it is a stretch, but the
/// comparison costs the same either way and the habit is worth more than the argument.
/// The length is compared first and separately: lengths are not secret, and a fixed-size
/// loop over mismatched lengths would need padding to be meaningful anyway.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, header};

    fn parts(method: Method, cookie: Option<&str>, sent_header: Option<&str>) -> Parts {
        let mut builder = Request::builder().method(method).uri("/");
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, format!("{CSRF_COOKIE}={c}"));
        }
        if let Some(h) = sent_header {
            builder = builder.header(CSRF_HEADER, h);
        }
        builder.body(()).unwrap().into_parts().0
    }

    async fn check(parts: &mut Parts) -> Result<CsrfChecked, ApiError> {
        CsrfChecked::from_request_parts(parts, &()).await
    }

    #[tokio::test]
    async fn a_get_needs_no_token() {
        // A cross-origin read cannot see its own response, so there is no side effect
        // to protect. Requiring a token here would only break links into the app.
        let mut p = parts(Method::GET, None, None);
        assert!(check(&mut p).await.is_ok());
    }

    #[tokio::test]
    async fn a_post_without_the_header_is_refused() {
        // The attacker's case: the browser attaches the cookie, the attacker's page
        // cannot read it, so the header is missing.
        let mut p = parts(Method::POST, Some("token123"), None);
        let err = check(&mut p).await.unwrap_err();
        assert!(matches!(err, ApiError::Forbidden(_)), "{err}");
    }

    #[tokio::test]
    async fn a_post_with_a_wrong_header_is_refused() {
        let mut p = parts(Method::POST, Some("token123"), Some("guessed"));
        assert!(check(&mut p).await.is_err());
    }

    #[tokio::test]
    async fn a_post_with_no_cookie_at_all_is_refused() {
        // Not an accident worth being lenient about: the cookie is set at login, so its
        // absence means the session predates CSRF or something stripped it.
        let mut p = parts(Method::POST, None, Some("anything"));
        assert!(check(&mut p).await.is_err());
    }

    #[tokio::test]
    async fn a_matching_pair_passes() {
        let mut p = parts(Method::POST, Some("token123"), Some("token123"));
        assert!(check(&mut p).await.is_ok());
    }

    #[tokio::test]
    async fn every_mutating_method_is_checked() {
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            let mut p = parts(method.clone(), Some("t"), None);
            assert!(
                check(&mut p).await.is_err(),
                "{method} must require a CSRF token"
            );
        }
    }

    #[test]
    fn tokens_are_long_and_do_not_repeat() {
        let a = issue().unwrap();
        let b = issue().unwrap();
        assert_ne!(a, b);
        assert!(a.len() >= 32, "a guessable token defends nothing");
    }

    #[test]
    fn comparison_rejects_a_prefix() {
        // The mistake `starts_with` would make.
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
