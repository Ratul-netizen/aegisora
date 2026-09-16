//! Device credentials — the material the poller authenticates with.
//!
//! Until this existed the `credential` table could only be written by a test. A
//! deployment could create a device, point it at an address and watch the poller report
//! "no credential is assigned" for ever, because nothing in the product could put one
//! there.
//!
//! # Material goes in and never comes out
//!
//! There is no route that returns a credential's material, and there is not going to be
//! one. Not for an administrator, not for an export, not for a "reveal" button. The API
//! writes it into the vault and afterwards can say only that it exists, what kind it is
//! and what it is called.
//!
//! That is not caution for its own sake. The whole value of envelope encryption is that
//! the material has exactly one reader — the collector that needs it — and every
//! additional path to it is a path a compromised session, a screenshot or a support
//! transcript can take. An operator who has lost a community string types a new one; an
//! operator who can read the old one out of the UI has turned the UI into a credential
//! store.
//!
//! # Why admin and not operator
//!
//! Creating a credential is handing the poller something that opens a customer's network
//! equipment. Changing a resource's status is an operational act; adding a credential is
//! an administrative one, and the roles already draw that line.
//!
//! # Why the vault is optional here
//!
//! A deployment with no KEK is this product with one feature off, not a broken one — see
//! [`crate::AppState::vault`]. These routes answer 503 with a sentence naming the
//! variable, rather than 500-ing with whatever the vault said.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uops_core::{
    AuthProtocol, CredentialMaterial, CredentialRef, PrivProtocol, ResourceId, Role, Secret,
};
use uops_secrets::CredentialMeta;

use crate::csrf::CsrfChecked;
use crate::error::{ApiError, ApiResult};
use crate::extract::Caller;
use crate::state::AppState;

/// What a client may send. Deliberately not `CredentialMaterial`: that is an untagged
/// domain type with SSH and API-token variants this route does not accept, and widening
/// it later should be a deliberate act rather than a consequence of a `serde` derive.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NewMaterial {
    /// SNMP v1/v2c. A community string is a password sent in clear text on the wire;
    /// stored sealed anyway, because the database is not the wire.
    SnmpCommunity { community: String },
    /// SNMP v3 with authentication and privacy. The only v3 mode accepted — see
    /// `uops_snmp::credential::Strength` on why `noAuthNoPriv` is not a credential.
    SnmpV3 {
        username: String,
        auth: AuthProtocol,
        auth_key: String,
        privacy: PrivProtocol,
        priv_key: String,
    },
}

impl NewMaterial {
    fn into_material(self) -> CredentialMaterial {
        match self {
            Self::SnmpCommunity { community } => CredentialMaterial::SnmpCommunity(community),
            Self::SnmpV3 {
                username,
                auth,
                auth_key,
                privacy,
                priv_key,
            } => CredentialMaterial::SnmpV3 {
                username,
                auth,
                auth_key,
                privacy,
                priv_key,
            },
        }
    }
}

/// A request to store a credential.
#[derive(Deserialize)]
pub struct CreateCredential {
    /// What an operator calls it — `core-switches`, `branch-readonly`. This is what a
    /// collector resolves by name, so it is the stable half of the identity.
    pub name: String,
    #[serde(flatten)]
    pub material: NewMaterial,
    /// Rotating an existing credential rather than adding a new one. The id is reused
    /// and the version incremented, so every resource already pointing at it switches
    /// over without being touched.
    #[serde(default)]
    pub supersedes: Option<CredentialRef>,
}

/// What a client gets back. Never the material.
#[derive(Debug, Serialize)]
pub struct CredentialView {
    pub id: CredentialRef,
    pub name: String,
    /// `snmp_community`, `snmp_v3`. The kind is not the secret — a UI lists credentials
    /// by what they are without opening any of them, and "this tenant has an
    /// `SNMPv3` credential" tells an attacker nothing they could not guess.
    pub kind: String,
    pub version: u32,
    pub revoked: bool,
}

impl std::fmt::Debug for NewMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written, and this is the reason. A derived `Debug` on a type holding a
        // community string or a v3 passphrase puts that string into the first
        // `{:?}` anybody reaches for while debugging a request — a log line, a panic
        // message, a test failure. `uops_core::Secret` exists to make that impossible for
        // stored material; this is the same discipline at the edge where it arrives.
        //
        // The username is printed. It is not the secret, and knowing which v3 user a
        // request was for is most of what makes a bad request diagnosable.
        match self {
            Self::SnmpCommunity { .. } => f
                .debug_struct("SnmpCommunity")
                .field("community", &"<redacted>")
                .finish(),
            Self::SnmpV3 {
                username,
                auth,
                privacy,
                ..
            } => f
                .debug_struct("SnmpV3")
                .field("username", username)
                .field("auth", auth)
                .field("privacy", privacy)
                .field("auth_key", &"<redacted>")
                .field("priv_key", &"<redacted>")
                .finish(),
        }
    }
}

impl std::fmt::Debug for CreateCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The name and the rotation target are not secret; the material redacts itself.
        f.debug_struct("CreateCredential")
            .field("name", &self.name)
            .field("material", &self.material)
            .field("supersedes", &self.supersedes)
            .finish()
    }
}

/// `POST /api/v1/credentials`
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<CreateCredential>,
) -> ApiResult<(StatusCode, Json<CredentialView>)> {
    caller.require(Role::Admin)?;
    let vault = vault(&state)?;

    if body.name.trim().is_empty() {
        return Err(ApiError::from(uops_core::Error::Invalid(
            "a credential needs a name; it is what a collector resolves it by".into(),
        )));
    }

    let material = body.material.into_material();
    let kind = material.kind().to_owned();

    let mut meta = CredentialMeta::new(body.name.clone());
    meta.supersedes = body.supersedes;

    // `Secret::new` here and by value into `put`: the caller surrenders the plaintext at
    // this boundary and it is zeroized when `put` returns. The JSON body it was parsed
    // from is not, which is a real limit of accepting secrets over HTTP and is why the
    // body type above is as narrow as it is.
    let id = vault
        .put(caller.scope().tenant_id(), Secret::new(material), &meta)
        .map_err(sealed)?;

    // Nothing about the material, in either half. An audit row is read by people who are
    // not allowed to see what it describes.
    caller.audit().wrote(
        "credential.create",
        format!("credential:{id}"),
        None,
        Some(serde_json::json!({ "name": body.name, "kind": kind })),
    );

    let stored = vault
        .describe(caller.scope().tenant_id(), id)
        .map_err(sealed)?;

    Ok((
        StatusCode::CREATED,
        Json(CredentialView {
            id,
            name: stored.name,
            kind: stored.kind,
            version: stored.version,
            revoked: stored.revoked_at.is_some(),
        }),
    ))
}

/// `GET /api/v1/credentials`
///
/// Names, kinds and versions. There is no page cursor: a tenant has credentials in the
/// tens — one per device class, not one per device — and that is the whole point of
/// resolving them by name.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Vec<CredentialView>>> {
    caller.require(Role::Admin)?;
    let vault = vault(&state)?;

    let rows = vault.list(caller.scope().tenant_id()).map_err(sealed)?;
    caller.audit().read(
        "credential.list",
        Some(i64::try_from(rows.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(
        rows.into_iter()
            .map(|r| CredentialView {
                id: r.id,
                name: r.name,
                kind: r.kind,
                version: r.version,
                revoked: r.revoked_at.is_some(),
            })
            .collect(),
    ))
}

/// `DELETE /api/v1/credentials/{id}`
///
/// Stamped, not deleted. Telemetry and audit rows reference a credential by id, and a
/// row that vanished would leave them pointing at nothing — the same reasoning as a
/// decommissioned resource.
pub async fn revoke(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<CredentialRef>,
    _csrf: CsrfChecked,
) -> ApiResult<StatusCode> {
    caller.require(Role::Admin)?;
    let vault = vault(&state)?;

    vault
        .revoke(caller.scope().tenant_id(), id)
        .map_err(sealed)?;
    caller
        .audit()
        .wrote("credential.revoke", format!("credential:{id}"), None, None);

    Ok(StatusCode::NO_CONTENT)
}

/// What a client sends to point a device at a credential.
#[derive(Debug, Deserialize)]
pub struct Assignment {
    /// `null` detaches it, which stops the device being polled rather than breaking it.
    #[serde(default)]
    pub credential: Option<CredentialRef>,
}

/// `PUT /api/v1/resources/{id}/credential`
///
/// Operator rather than admin: choosing *which* stored credential a device uses is an
/// operational decision, and it reveals nothing — the id is already visible in the
/// listing above.
pub async fn assign(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceId>,
    _csrf: CsrfChecked,
    Json(body): Json<Assignment>,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    // The store checks that the credential is in this tenant. Without that, a resource
    // could be pointed at another customer's credential by id — and the poller would
    // then open it, because the poller trusts the column.
    state
        .store
        .assign_credential(caller.scope(), id, body.credential)
        .await?;

    caller.audit().wrote(
        "resource.credential",
        format!("resource:{id}"),
        None,
        Some(match body.credential {
            Some(c) => serde_json::json!({ "credential": c.to_string() }),
            None => serde_json::Value::Null,
        }),
    );

    Ok(StatusCode::NO_CONTENT)
}

/// The vault, or a 503 naming what is missing.
fn vault(state: &AppState) -> Result<&crate::state::Vault, ApiError> {
    state.vault.as_deref().ok_or(ApiError::Unavailable(
        "this deployment has no key-encryption key, so credentials cannot be stored; \
         set UOPS_KEK_FILE or UOPS_KEK_HEX and restart",
    ))
}

/// A vault error, as an API error.
///
/// Deliberately lossy. `uops_secrets::Error` distinguishes a missing row from a failed
/// decrypt from an unknown key, and those distinctions are for the operator reading the
/// server's log rather than for a client: telling a caller which of them happened is
/// telling them something about a credential they are not allowed to read.
fn sealed(e: uops_secrets::Error) -> ApiError {
    match e {
        uops_secrets::Error::NotFound => ApiError::from(uops_core::Error::NotFound {
            kind: "credential",
            id: String::new(),
        }),
        other => {
            // The detail reaches the server's log, not the response body — `Storage`
            // maps to a 500, whose detail is replaced with "internal error" on the way
            // out. See crate::error.
            eprintln!("credential operation failed: {other}");
            ApiError::from(uops_core::Error::Storage(other.to_string()))
        }
    }
}

/// One identifier, as a client sends or receives it.
#[derive(Debug, Serialize, Deserialize)]
pub struct IdentifierView {
    pub kind: uops_core::IdentifierKind,
    pub value: String,
    /// What recorded it. Only `manual` rows can be edited — see the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// `GET /api/v1/resources/{id}/identifiers`
///
/// Everything known about who this device is, whoever recorded it. A viewer may read
/// them: a serial number and a MAC are inventory, not secrets, and an operator
/// diagnosing a merged resource needs to see what it was merged on.
pub async fn identifiers(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceId>,
) -> ApiResult<Json<Vec<IdentifierView>>> {
    caller.require(Role::Viewer)?;

    let found = state.store.identifiers_for(caller.scope(), id).await?;
    caller.audit().read(
        "resource.identifiers",
        Some(i64::try_from(found.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(
        found
            .into_iter()
            .map(|i| IdentifierView {
                kind: i.kind,
                value: i.value,
                source: None,
            })
            .collect(),
    ))
}

/// `PUT /api/v1/resources/{id}/identifiers`
///
/// Replaces the identifiers a person typed and leaves the ones collectors found. One
/// verb gives add, change and remove — a route that only added would make a mistyped
/// `mgmt_ip` permanent, and a mistyped management address is a device polling somebody
/// else's equipment.
///
/// This is also the route that makes a device *pollable*: `uops_store_pg::pollable` reads
/// `resource_identifier` where `kind = 'mgmt_ip'`, deliberately, because an address is
/// something identity resolution matches on rather than a column on the device.
pub async fn set_identifiers(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceId>,
    _csrf: CsrfChecked,
    Json(body): Json<Vec<IdentifierView>>,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    let identifiers: Vec<uops_core::Identifier> = body
        .into_iter()
        .map(|i| uops_core::Identifier::new(i.kind, i.value))
        .collect();

    state
        .store
        .set_manual_identifiers(caller.scope(), id, &identifiers)
        .await?;

    caller.audit().wrote(
        "resource.identifiers",
        format!("resource:{id}"),
        None,
        // The kinds and how many, not the values. A management address is not a secret,
        // but an audit row that carried every identifier would be a second copy of the
        // inventory in a table nobody prunes.
        Some(serde_json::json!({
            "count": identifiers.len(),
            "kinds": identifiers
                .iter()
                .map(|i| i.kind.as_str())
                .collect::<Vec<_>>(),
        })),
    );

    Ok(StatusCode::NO_CONTENT)
}
