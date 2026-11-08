//! Trust roots for signed registry content (design 2026-11-08 §2).
//!
//! Compiled-in roots cover Scenery's own origins. Any other origin has no
//! trust until the user runs `scenery trust pin`, which stores the key in the
//! `trusted_keys` table. **Only the CLI writes that table; the daemon never
//! does.** (Tests also pin through [`pin_key`] to get a trust root for a mock
//! registry.)

use crate::signed::TrustedKey;
use crate::state::AppState;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

/// Debug-only escape hatch for the client test bed and local development:
/// accept unsigned definitions. Ignored in release builds.
pub const INSECURE_UNSIGNED_ENV: &str = "SCENERY_INSECURE_UNSIGNED";

/// A kid and its raw 32-byte Ed25519 public key.
pub type RootKey = (&'static str, [u8; 32]);

/// A placeholder is all zeroes: a small-order point that `verify_strict`
/// rejects, and that [`compiled_keys`] skips entirely.
pub const PLACEHOLDER_KEY: [u8; 32] = [0u8; 32];

// TODO(P0): replace with real pubkeys. The KMS keys do not exist yet; these
// are placeholders and never verify anything. Export each key with
// `aws kms get-public-key` (DER SPKI, raw key = last 32 bytes) and paste the
// 32 bytes here. Rotation: the standby `b` key is compiled in ahead of time.
/// Compiled-in trust roots: `(origin, keys)`.
pub const TRUST_ROOTS: &[(&str, &[RootKey])] = &[
    (
        "https://app.scenery.dev",
        &[
            ("scn-prod-a", PLACEHOLDER_KEY), // TODO(P0): replace with real pubkeys
            ("scn-prod-b", PLACEHOLDER_KEY), // TODO(P0): replace with real pubkeys
        ],
    ),
    (
        "https://dev.scenery.dev",
        &[
            ("scn-dev-a", PLACEHOLDER_KEY), // TODO(P0): replace with real pubkeys
            ("scn-dev-b", PLACEHOLDER_KEY), // TODO(P0): replace with real pubkeys
        ],
    ),
    (
        "https://demo.scenery.dev",
        &[
            ("scn-demo-a", PLACEHOLDER_KEY), // TODO(P0): replace with real pubkeys
            ("scn-demo-b", PLACEHOLDER_KEY), // TODO(P0): replace with real pubkeys
        ],
    ),
];

/// True when `SCENERY_INSECURE_UNSIGNED=1` is set in a debug build.
pub fn insecure_unsigned() -> bool {
    cfg!(debug_assertions) && std::env::var(INSECURE_UNSIGNED_ENV).as_deref() == Ok("1")
}

/// Compiled-in keys for `origin`, skipping placeholders.
pub fn compiled_keys(origin: &str) -> Vec<TrustedKey> {
    TRUST_ROOTS
        .iter()
        .filter(|(o, _)| *o == origin)
        .flat_map(|(_, keys)| keys.iter())
        .filter(|(_, k)| *k != PLACEHOLDER_KEY)
        .map(|(kid, k)| TrustedKey {
            kid: (*kid).to_string(),
            pubkey: *k,
        })
        .collect()
}

/// True when `origin` is one of the compiled-in origins (even if only
/// placeholder keys are present so far).
pub fn is_compiled_origin(origin: &str) -> bool {
    TRUST_ROOTS.iter().any(|(o, _)| *o == origin)
}

fn open(state: &AppState) -> Result<Connection> {
    Connection::open(&state.db_path).context("failed to open state DB")
}

/// Pinned keys for `origin` from `trusted_keys`.
pub fn pinned_keys(state: &AppState, origin: &str) -> Result<Vec<TrustedKey>> {
    let conn = open(state)?;
    let mut stmt = conn
        .prepare("SELECT kid, pubkey FROM trusted_keys WHERE origin = ?1")
        .context("failed to prepare trusted_keys query")?;
    let rows = stmt
        .query_map([origin], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .context("failed to read trusted_keys")?;
    let mut out = Vec::new();
    for row in rows {
        let (kid, raw) = row.context("bad trusted_keys row")?;
        if let Ok(pubkey) = <[u8; 32]>::try_from(raw.as_slice()) {
            out.push(TrustedKey { kid, pubkey });
        }
    }
    Ok(out)
}

/// All keys trusted for `origin`: compiled-in plus pinned.
pub fn keys_for_origin(state: &AppState, origin: &str) -> Vec<TrustedKey> {
    let mut keys = compiled_keys(origin);
    match pinned_keys(state, origin) {
        Ok(p) => keys.extend(p),
        Err(e) => tracing::warn!(error = %e, "trust: could not read pinned keys"),
    }
    keys
}

/// Whether signed mode is possible for `origin` at all.
pub fn has_trust_root(state: &AppState, origin: &str) -> bool {
    !keys_for_origin(state, origin).is_empty()
}

/// Store a pinned key. CLI-only in production code paths.
pub fn pin_key(state: &AppState, origin: &str, kid: &str, pubkey: &[u8; 32]) -> Result<()> {
    let conn = open(state)?;
    conn.execute(
        "INSERT INTO trusted_keys (origin, kid, pubkey, pinned_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(origin, kid) DO UPDATE SET pubkey = excluded.pubkey, pinned_at = excluded.pinned_at",
        params![origin, kid, pubkey.as_slice(), chrono::Utc::now().to_rfc3339()],
    )
    .context("failed to pin key")?;
    Ok(())
}

/// Remove every pinned key for `origin`. Returns how many were removed.
pub fn reset_origin(state: &AppState, origin: &str) -> Result<usize> {
    let conn = open(state)?;
    conn.execute("DELETE FROM trusted_keys WHERE origin = ?1", [origin])
        .context("failed to reset pinned keys")
}

/// Lowercase hex SHA-256 of the raw public key: what `trust pin --fingerprint`
/// compares against.
pub fn fingerprint(pubkey: &[u8; 32]) -> String {
    hex::encode(Sha256::digest(pubkey))
}

/// One `keys[]` entry of `GET /api/runner/signing-keys`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SigningKeyInfo {
    pub kid: String,
    pub alg: String,
    /// base64url (no padding), 32 bytes.
    pub public_key: String,
    #[serde(default)]
    pub status: String,
}

/// `GET /api/runner/signing-keys` response.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SigningKeysResponse {
    pub iss: String,
    pub keys: Vec<SigningKeyInfo>,
}

/// Pick the key whose fingerprint equals `fingerprint_hex` out of a
/// `/signing-keys` response and decode it. The response is never trusted on
/// its own: the user supplied the fingerprint out of band, and the response's
/// `iss` must equal the origin being pinned.
pub fn select_key_by_fingerprint(
    resp: &SigningKeysResponse,
    origin: &str,
    fingerprint_hex: &str,
) -> Result<(String, [u8; 32])> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    anyhow::ensure!(
        resp.iss == origin,
        "signing-keys response is for '{}', not '{origin}'",
        resp.iss
    );
    let want = fingerprint_hex.trim().to_ascii_lowercase();
    for k in &resp.keys {
        if k.alg != "ed25519" {
            continue;
        }
        let Ok(raw) = URL_SAFE_NO_PAD.decode(k.public_key.as_bytes()) else {
            continue;
        };
        let Ok(arr) = <[u8; 32]>::try_from(raw.as_slice()) else {
            continue;
        };
        if fingerprint(&arr) == want {
            return Ok((k.kid.clone(), arr));
        }
    }
    anyhow::bail!("no key served by the registry matches fingerprint {want}")
}

#[cfg(test)]
#[path = "trust_tests.rs"]
mod tests;
