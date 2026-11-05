//! Internal HTTP/1.1 protocol between `rynas-web` and `rynasd` over a Unix socket.
//!
//! Not a public API: `rynasd` trusts its peer (checked with `SO_PEERCRED`), the session and
//! CSRF checks happen in `rynas-web`. Errors use [`rynas_api_types::ApiError`] bodies with the
//! codes of [`rynas_api_types::error_codes`] plus "bad_request" and "internal".
//!
//! | Method | Path | Body | Answer |
//! |---|---|---|---|
//! | GET  | `/v1/health` | | `{"version": ..}` |
//! | POST | `/v1/authenticate` | [`AuthenticateRequest`] | [`crate::Identity`] or 401 |
//! | GET  | `/v1/users/{user}/groups` | | `Vec<String>` or 404 |
//! | GET  | `/v1/totp/{user}` | | [`crate::TotpStatus`] |
//! | POST | `/v1/totp/{user}/enroll` | [`EnrollRequest`] | `TotpEnrollment` |
//! | POST | `/v1/totp/{user}/confirm` | `TotpCode` | `TotpConfirmed` |
//! | POST | `/v1/totp/{user}/disable` | `TotpCode` | 204 |
//! | POST | `/v1/totp/{user}/verify` | `TotpCode` | 204 |
//!
//! Configuration and tasks: the same paths as the public API (`/api/v1/...` becomes
//! `/v1/...`, see `docs/phase1/plan.md`), proxied by `rynas-web` with the session user in
//! the [`USER_HEADER`] header: `config/pending`, `config/apply`, `config/discard`,
//! `config/revisions[/{n}[/restore]]`, `config/drift[/{key}/import|overwrite]`,
//! `system/hostname`, `tasks[/stream|/{id}[/log|/cancel]]`.
//! | GET  | `/v1/storage/disks` | | `Vec<storage::DiskSummary>` |
//! | GET  | `/v1/storage/disks/{name}` | | `storage::DiskDetail` or 404 |
//! | GET  | `/v1/storage/arrays` | | `Vec<storage::MdArray>` |
//! | GET  | `/v1/storage/filesystems` | | `Vec<storage::BtrfsFilesystem>` |
//! | GET  | `/v1/storage/tree` | | `storage::StorageTree` |
//! | POST | `/v1/storage/plan/{volume,grow,replace,scrub}` | `storage::*PlanRequest` | `storage::StoragePlan` |
//!
//! Storage types live in `rynas_api_types::storage`; the plans have no side effects.

use serde::{Deserialize, Serialize};

pub const DEFAULT_SOCKET: &str = "/run/rynas/rynasd.sock";

/// Header with the name of the logged-in user on proxied configuration and task requests.
pub const USER_HEADER: &str = "x-rynas-user";

/// Header with the local address (`ip:port`) the session's TCP connection arrived on, added
/// by `rynas-web` to the network endpoints: rynasd derives the management interface from it
/// and checks that a network confirmation came over the new path.
pub const LOCAL_ADDR_HEADER: &str = "x-rynas-local-addr";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthenticateRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    /// Account label in the otpauth URI, e.g. "alice@nas".
    pub account: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub version: String,
}
