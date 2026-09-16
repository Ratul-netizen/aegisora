//! API errors, as RFC 7807 `problem+json`.
//!
//! SPEC conventions: one error type per crate boundary, and the API maps them onto
//! problem+json. The mapping is deliberately lossy in one direction — several distinct
//! internal failures become the same response — and that is the point rather than an
//! oversight.
//!
//! # What a client is allowed to learn
//!
//! A tenant the caller has no role on returns **404**, not 403. So does a resource in
//! another tenant. Confirming that something exists but is forbidden tells an MSP
//! customer that another customer exists, and at what ID — which is exactly the
//! inventory leak `uops_core::Error::TenantMismatch` is written to avoid. The one place
//! 403 is correct is a tenant the caller *can* see but lacks the role for: they already
//! know it exists.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// An error on its way to a client.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    /// No session, or one that is no longer valid.
    #[error("authentication required")]
    Unauthenticated,

    /// Authenticated, allowed to see this tenant, not allowed to do this.
    #[error("{0}")]
    Forbidden(&'static str),

    /// Anything the caller may not know exists: another tenant, another tenant's
    /// resource, a tenant they hold no role on.
    #[error("not found")]
    NotFound,

    #[error("{0}")]
    BadRequest(String),

    /// The deployment has not configured something this route needs. A 503 rather than
    /// a 500: nothing is broken, a capability is switched off, and the message says
    /// which variable turns it on.
    #[error("{0}")]
    Unavailable(&'static str),

    /// Everything below the API. Its message is logged, not returned.
    #[error(transparent)]
    Internal(#[from] uops_core::Error),
}

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            Self::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "authentication required".to_owned(),
            ),
            Self::Forbidden(why) => (StatusCode::FORBIDDEN, "forbidden", (*why).to_owned()),
            Self::NotFound => (StatusCode::NOT_FOUND, "not-found", "not found".to_owned()),
            Self::BadRequest(detail) => (StatusCode::BAD_REQUEST, "invalid-input", detail.clone()),
            Self::Unavailable(why) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "not-configured",
                (*why).to_owned(),
            ),
            Self::Internal(e) => {
                let status = StatusCode::from_u16(e.status_code())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                // A 500's detail is deliberately generic: the real message can name a
                // table, a constraint or a connection string, and none of that belongs
                // in a response. It is logged instead.
                let detail = if status.is_server_error() {
                    "internal error".to_owned()
                } else {
                    e.to_string()
                };
                (status, e.problem_type(), detail)
            }
        }
    }
}

/// RFC 7807.
#[derive(Debug, Serialize)]
struct Problem {
    /// A slug rather than a URL for now; the URL prefix belongs to documentation that
    /// does not exist yet, and a link to nothing is worse than a stable identifier.
    r#type: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, kind, detail) = self.parts();

        if status.is_server_error() {
            // The only place the real message goes. Not to the client.
            eprintln!("api error: {self}");
        }

        let body = Problem {
            r#type: kind,
            title: status.canonical_reason().unwrap_or("Error"),
            status: status.as_u16(),
            detail,
        };

        (status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use uops_core::Error as CoreError;

    fn status_of(e: &ApiError) -> u16 {
        e.parts().0.as_u16()
    }

    #[test]
    fn a_tenant_you_cannot_see_is_indistinguishable_from_one_that_does_not_exist() {
        // The rule the whole error model is built around. A 403 here would confirm to
        // one MSP customer that another customer exists, and at what ID.
        assert_eq!(status_of(&ApiError::NotFound), 404);
        assert_eq!(status_of(&CoreError::TenantMismatch.into()), 404);
        assert_eq!(
            ApiError::from(CoreError::TenantMismatch).parts().1,
            ApiError::NotFound.parts().1,
            "the problem type must match too, or the slug is the oracle"
        );
    }

    #[test]
    fn forbidden_is_for_something_the_caller_already_knows_exists() {
        // A viewer on a tenant they genuinely hold a role on. Hiding this as a 404
        // would be actively unhelpful: they can see the tenant, they simply may not do
        // this, and telling them so is what lets them ask for the right role.
        assert_eq!(status_of(&ApiError::Forbidden("operator required")), 403);
    }

    #[test]
    fn an_internal_failure_does_not_describe_itself_to_the_client() {
        // The message can name a table, a constraint, or a connection string.
        let leaky =
            CoreError::Storage("connection to postgres://uops:hunter2@db:5432 refused".into());
        let (status, _, detail) = ApiError::from(leaky).parts();

        assert_eq!(status.as_u16(), 500);
        assert_eq!(detail, "internal error");
        assert!(!detail.contains("hunter2"));
    }

    #[test]
    fn a_client_error_does_explain_itself() {
        // A 400 the caller can act on is worth the words; hiding it just produces a
        // support ticket.
        let (status, kind, detail) =
            ApiError::BadRequest("limit must be at least 1".into()).parts();
        assert_eq!(status.as_u16(), 400);
        assert_eq!(kind, "invalid-input");
        assert!(detail.contains("limit"));
    }
}
