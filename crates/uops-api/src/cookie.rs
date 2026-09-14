//! Cookies: reading one, and setting the two that matter.
//!
//! Hand-written rather than a cookie crate. Reading is one value out of a
//! `name=value; name=value` header, and setting is one formatted string — the parsing a
//! library adds is for jars and attribute round-tripping, neither of which a server
//! that issues exactly two cookies needs.
//!
//! # The two cookies, and why they have different flags
//!
//! | cookie | `HttpOnly` | why |
//! |---|---|---|
//! | `uops_session` | **yes** | a bearer credential; script must never be able to read it, or an XSS becomes an account takeover |
//! | `uops_csrf` | **no** | the double-submit token exists *to be read by script* and echoed in a header, which is the whole mechanism |
//!
//! Both are `SameSite=Lax` and `Secure`. `Lax` rather than `Strict` because `Strict`
//! breaks following a link into the app from anywhere — a page that appears logged out
//! when reached from a chat message is a support ticket, and `Lax` still blocks the
//! cross-site POST that CSRF actually needs.

use axum::http::HeaderValue;
use axum::http::request::Parts;

/// The session cookie. `HttpOnly`.
pub const SESSION_COOKIE: &str = "uops_session";
/// The CSRF token cookie. Readable by script, on purpose.
pub const CSRF_COOKIE: &str = "uops_csrf";

/// Read one cookie from the `Cookie` header.
#[must_use]
pub fn read(parts: &Parts, name: &str) -> Option<String> {
    let header = parts
        .headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;

    header.split(';').find_map(|pair| {
        let (found, value) = pair.split_once('=')?;
        (found.trim() == name).then(|| value.trim().to_owned())
    })
}

/// Whether to mark cookies `Secure`.
///
/// `Secure` always, except when a developer is on plain HTTP against localhost — where
/// the browser would silently discard the cookie and the whole app would appear broken
/// for a reason nothing logs. Off is a deliberate, named setting, never a default that
/// drifted into production.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Secure {
    Yes,
    /// Only for `http://localhost` during development.
    No,
}

impl Secure {
    const fn attribute(self) -> &'static str {
        match self {
            Self::Yes => "; Secure",
            Self::No => "",
        }
    }
}

/// Build a `Set-Cookie` value.
///
/// `max_age` is passed rather than derived so it can be made to match the session's own
/// expiry exactly. A cookie that outlives its session is a browser presenting a dead
/// token; a cookie that dies first logs the user out early. Both are avoidable by
/// passing the same number twice.
#[must_use]
pub fn set(name: &str, value: &str, max_age_secs: i64, http_only: bool, secure: Secure) -> String {
    format!(
        "{name}={value}; Path=/; Max-Age={max_age_secs}; SameSite=Lax{}{}",
        if http_only { "; HttpOnly" } else { "" },
        secure.attribute()
    )
}

/// Build a `Set-Cookie` that removes one.
///
/// The value is emptied as well as expired: a browser that ignores `Max-Age=0` for any
/// reason is then presenting an empty token rather than the real one.
#[must_use]
pub fn clear(name: &str, http_only: bool, secure: Secure) -> String {
    format!(
        "{name}=; Path=/; Max-Age=0; SameSite=Lax{}{}",
        if http_only { "; HttpOnly" } else { "" },
        secure.attribute()
    )
}

/// Parse a built cookie string into a header value.
pub(crate) fn header(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, header};

    fn parts_with(cookie: &str) -> Parts {
        Request::builder()
            .uri("/")
            .header(header::COOKIE, cookie)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    #[test]
    fn a_cookie_is_found_among_others_whatever_the_spacing() {
        let parts = parts_with("theme=dark; uops_session=abc123 ;uops_csrf=xyz");
        assert_eq!(read(&parts, SESSION_COOKIE).as_deref(), Some("abc123"));
        assert_eq!(read(&parts, CSRF_COOKIE).as_deref(), Some("xyz"));
        assert_eq!(read(&parts, "absent"), None);
    }

    #[test]
    fn a_prefix_is_not_a_match() {
        let parts = parts_with("uops_session_backup=abc");
        assert_eq!(read(&parts, SESSION_COOKIE), None);
    }

    #[test]
    fn the_session_cookie_is_http_only_and_the_csrf_cookie_is_not() {
        // The difference IS the mechanism. A script-readable session cookie turns any
        // XSS into an account takeover; a script-unreadable CSRF token cannot be
        // echoed into a header, which is the only thing it is for.
        let session = set(SESSION_COOKIE, "t", 3600, true, Secure::Yes);
        let csrf = set(CSRF_COOKIE, "c", 3600, false, Secure::Yes);

        assert!(session.contains("; HttpOnly"), "{session}");
        assert!(!csrf.contains("HttpOnly"), "{csrf}");
    }

    #[test]
    fn cookies_are_secure_and_same_site_lax_by_default() {
        let c = set(SESSION_COOKIE, "t", 3600, true, Secure::Yes);
        assert!(c.contains("; Secure"), "{c}");
        assert!(c.contains("; SameSite=Lax"), "{c}");
        assert!(c.contains("; Path=/"), "{c}");
    }

    #[test]
    fn insecure_is_available_but_has_to_be_asked_for() {
        // A developer on http://localhost needs this, and nobody else ever should.
        let c = set(SESSION_COOKIE, "t", 3600, true, Secure::No);
        assert!(!c.contains("Secure"), "{c}");
    }

    #[test]
    fn clearing_empties_the_value_as_well_as_expiring_it() {
        let c = clear(SESSION_COOKIE, true, Secure::Yes);
        assert!(c.starts_with("uops_session=;"), "{c}");
        assert!(c.contains("Max-Age=0"), "{c}");
        assert!(c.contains("HttpOnly"), "{c}");
    }
}
