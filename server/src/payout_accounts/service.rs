//! Seller payout accounts and store activation.
//!
//! Data protection: the account number and the account holder's name are encrypted
//! at rest; only the last four digits are stored in the clear (data minimisation: they let
//! the seller recognise the account without exposing it). Neither value is logged or audited.
//! A resolved name is shown to the seller only once the account is verified (otherwise the
//! endpoint would be an account-holder-name lookup oracle).
//!
//! Lock order: stores row first, then the payout_accounts row. No network call or password
//! hash is computed while a transaction is open.
//!
//! Rate limit: counted from `audit_log` rows (`payout_account.updated` and
//! `payout_account.reauth_failed`, per store, rolling 24h); no extra table is needed.
//!
//! Provider outcomes on a lookup: `Ok(name)` verifies when the name matches the seller;
//! `NotSupported`, `Unavailable`, `Timeout` (and an unregistered provider) leave the account
//! pending for owner review; `Rejected` (bad account or bank) is a 422 `account_not_found`
//! and nothing is saved.

use std::collections::HashSet;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgConnection, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState, audit,
    auth::password::{blocking, verify_password},
    error::AppError,
    payments::ProviderError,
    settings,
};

/// PUT attempts (accepted, or failed re-authentication) allowed per store in a rolling day.
pub const MAX_PUTS_PER_DAY: i64 = 5;

const REDACTED: &str = "[redacted]";

#[derive(Deserialize)]
pub struct SetPayoutAccount {
    pub bank_code: String,
    pub account_number: String,
    /// Required when the store already has a payout account (re-authentication).
    #[serde(default)]
    pub password: Option<String>,
}

impl std::fmt::Debug for SetPayoutAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetPayoutAccount")
            .field("bank_code", &self.bank_code)
            .field("account_number", &REDACTED)
            .field("password", &REDACTED)
            .finish()
    }
}

#[derive(Deserialize)]
pub struct VerifyPayoutAccount {
    pub account_name: String,
}

impl std::fmt::Debug for VerifyPayoutAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyPayoutAccount")
            .field("account_name", &REDACTED)
            .finish()
    }
}

#[derive(Serialize)]
pub struct PayoutAccountView {
    pub bank_code: String,
    pub last4: String,
    pub account_name: Option<String>,
    pub status: String,
    pub pending_reason: Option<String>,
    pub verified_by: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub changed_at: OffsetDateTime,
}

impl std::fmt::Debug for PayoutAccountView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayoutAccountView")
            .field("bank_code", &self.bank_code)
            .field("last4", &self.last4)
            .field(
                "account_name",
                &self.account_name.as_ref().map(|_| REDACTED),
            )
            .field("status", &self.status)
            .finish()
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    bank_code: String,
    account_last4: String,
    account_name_enc: Option<Vec<u8>>,
    status: String,
    pending_reason: Option<String>,
    verified_by: Option<String>,
    changed_at: OffsetDateTime,
}

const COLUMNS: &str = "bank_code, account_last4, account_name_enc, status::text AS status, \
                       pending_reason, verified_by, changed_at";

impl Row {
    /// `admin`: admins also see a stored name that is not verified yet (for review).
    fn view(self, state: &AppState, admin: bool) -> Result<PayoutAccountView, AppError> {
        let show = admin || self.status == "verified";
        let account_name = match (&self.account_name_enc, show) {
            (Some(enc), true) => Some(state.cipher.decrypt(enc)?),
            _ => None,
        };
        Ok(PayoutAccountView {
            bank_code: self.bank_code,
            last4: self.account_last4,
            account_name,
            status: self.status,
            pending_reason: self.pending_reason,
            verified_by: self.verified_by,
            changed_at: self.changed_at,
        })
    }
}

fn validate(input: &SetPayoutAccount) -> Result<(), AppError> {
    let bank = &input.bank_code;
    if !(3..=6).contains(&bank.len()) || !bank.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AppError::Validation(
            "bank_code must be 3 to 6 digits".into(),
        ));
    }
    let n = &input.account_number;
    if n.len() != 10 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AppError::Validation(
            "account_number must be exactly 10 digits".into(),
        ));
    }
    Ok(())
}

/// CBN NUBAN check digit: weights 3,7,3,3,7,3,3,7,3,3,7,3 over the 3-digit bank code
/// followed by the first nine account digits; check = (10 - sum mod 10) mod 10.
/// False for malformed input. Callers apply it to 3-digit bank codes only, and treat a
/// failure as "needs review" rather than invalid (some accounts do not follow it).
pub fn nuban_check_ok(bank_code: &str, account_number: &str) -> bool {
    const WEIGHTS: [u32; 12] = [3, 7, 3, 3, 7, 3, 3, 7, 3, 3, 7, 3];
    if bank_code.len() != 3 || account_number.len() != 10 {
        return false;
    }
    let digits: Option<Vec<u32>> = bank_code
        .chars()
        .chain(account_number.chars().take(9))
        .map(|c| c.to_digit(10))
        .collect();
    let Some(digits) = digits else { return false };
    let Some(check) = account_number.chars().last().and_then(|c| c.to_digit(10)) else {
        return false;
    };
    let sum: u32 = digits.iter().zip(WEIGHTS).map(|(d, w)| d * w).sum();
    (10 - sum % 10) % 10 == check
}

fn name_tokens(name: &str) -> HashSet<String> {
    name.to_uppercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(str::to_string)
        .collect()
}

/// Whether the account holder's name plausibly belongs to the registered user: at least two
/// of the user's name tokens appear in the account name, or all of them when the user has
/// fewer than two. A user with no usable tokens never matches.
pub fn names_match(registered: &str, account_name: &str) -> bool {
    let mine = name_tokens(registered);
    if mine.is_empty() {
        return false;
    }
    let common = mine.intersection(&name_tokens(account_name)).count();
    common >= mine.len().min(2)
}

/// Locks a live store row (owned by `owner` when given); anything else is NotFound.
async fn lock_store(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    owner: Option<Uuid>,
) -> Result<String, AppError> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status::text FROM stores \
         WHERE id = $1 AND deleted_at IS NULL AND ($2::uuid IS NULL OR owner_id = $2) \
         FOR NO KEY UPDATE",
    )
    .bind(id)
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await
    .context("lock store")?;
    status.ok_or(AppError::NotFound)
}

/// Active <-> pending follows the account; a suspended store is never touched.
async fn sync_store_status(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    current: &str,
    verified: bool,
) -> Result<(), AppError> {
    let next = match (current, verified) {
        ("pending", true) => "active",
        ("active", false) => "pending",
        _ => return Ok(()),
    };
    sqlx::query("UPDATE stores SET status = $2::store_status WHERE id = $1")
        .bind(id)
        .bind(next)
        .execute(&mut **tx)
        .await
        .context("update store status")?;
    Ok(())
}

async fn recent_attempts(conn: &mut PgConnection, store_id: Uuid) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM audit_log \
         WHERE subject_type = 'store' AND subject_id = $1 \
           AND action IN ('payout_account.updated', 'payout_account.reauth_failed') \
           AND at > now() - interval '24 hours'",
    )
    .bind(store_id)
    .fetch_one(conn)
    .await
    .context("count payout attempts")?)
}

struct Decision {
    verified: bool,
    /// Provider-resolved name (kept encrypted, also on a mismatch, for owner review).
    name: Option<String>,
    reason: Option<&'static str>,
}

impl Decision {
    fn pending(reason: &'static str, name: Option<String>) -> Self {
        Self {
            verified: false,
            name,
            reason: Some(reason),
        }
    }
}

/// All network I/O happens here, before any transaction.
async fn decide(
    state: &AppState,
    owner: Uuid,
    input: &SetPayoutAccount,
) -> Result<Decision, AppError> {
    if input.bank_code.len() == 3 && !nuban_check_ok(&input.bank_code, &input.account_number) {
        return Ok(Decision::pending("checksum_failed", None));
    }
    let settings = settings::service::load(&state.db).await?;
    let Some(provider) = state.providers.by_name(&settings.payout_provider) else {
        return Ok(Decision::pending("lookup_unsupported", None));
    };
    match provider
        .resolve_account(&input.bank_code, &input.account_number)
        .await
    {
        Ok(name) => {
            let registered: String =
                sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
                    .bind(owner)
                    .fetch_one(&state.db)
                    .await
                    .context("load display name")?;
            if names_match(&registered, &name) {
                Ok(Decision {
                    verified: true,
                    name: Some(name),
                    reason: None,
                })
            } else {
                Ok(Decision::pending("name_mismatch", Some(name)))
            }
        }
        Err(ProviderError::NotSupported) => Ok(Decision::pending("lookup_unsupported", None)),
        Err(ProviderError::Rejected { .. }) => Err(AppError::Unprocessable(
            "account_not_found",
            "That bank account could not be found. Check the bank and account number".into(),
        )),
        Err(ProviderError::Unavailable(_))
        | Err(ProviderError::Timeout)
        | Err(ProviderError::InvalidSignature) => Ok(Decision::pending("lookup_unavailable", None)),
    }
}

pub async fn set_account(
    state: &AppState,
    owner: Uuid,
    store_id: Uuid,
    input: SetPayoutAccount,
) -> Result<PayoutAccountView, AppError> {
    validate(&input)?;

    // Ownership and pre-checks; no transaction or lock is held.
    let owned: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM stores WHERE id = $1 AND owner_id = $2 AND deleted_at IS NULL",
    )
    .bind(store_id)
    .bind(owner)
    .fetch_optional(&state.db)
    .await
    .context("check store")?;
    owned.ok_or(AppError::NotFound)?;
    let mut conn = state.db.acquire().await.context("acquire")?;
    if recent_attempts(&mut conn, store_id).await? >= MAX_PUTS_PER_DAY {
        return Err(AppError::TooManyRequests("too_many_attempts"));
    }
    let existing: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM payout_accounts WHERE store_id = $1)")
            .bind(store_id)
            .fetch_one(&mut *conn)
            .await
            .context("check existing account")?;
    if existing {
        // Changing a payout account is the account-takeover step: re-authenticate.
        let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(owner)
            .fetch_one(&mut *conn)
            .await
            .context("load password hash")?;
        let given = input.password.clone().unwrap_or_default();
        if !blocking(move || verify_password(&given, &hash)).await? {
            audit::record(
                &mut conn,
                Some(owner),
                "payout_account.reauth_failed",
                "store",
                Some(store_id),
                json!({}),
            )
            .await?;
            return Err(AppError::InvalidCredentials);
        }
    }
    drop(conn);

    let decision = decide(state, owner, &input).await?;

    let last4 = input.account_number[6..].to_string();
    let number_enc = state.cipher.encrypt(&input.account_number);
    let name_enc = decision.name.as_deref().map(|n| state.cipher.encrypt(n));

    let mut tx = state.db.begin().await.context("begin")?;
    let store_status = lock_store(&mut tx, store_id, Some(owner)).await?;
    if recent_attempts(&mut tx, store_id).await? >= MAX_PUTS_PER_DAY {
        return Err(AppError::TooManyRequests("too_many_attempts"));
    }
    let row: Row = sqlx::query_as(&format!(
        "INSERT INTO payout_accounts \
           (store_id, bank_code, account_number_enc, account_last4, account_name_enc, status, \
            pending_reason, verified_by, verified_at, changed_at, previous_account_existed) \
         VALUES ($1, $2, $3, $4, $5, $6::payout_account_status, $7, \
                 CASE WHEN $8 THEN 'provider' END, CASE WHEN $8 THEN now() END, now(), false) \
         ON CONFLICT (store_id) DO UPDATE SET \
           bank_code = EXCLUDED.bank_code, \
           account_number_enc = EXCLUDED.account_number_enc, \
           account_last4 = EXCLUDED.account_last4, \
           account_name_enc = EXCLUDED.account_name_enc, \
           status = EXCLUDED.status, pending_reason = EXCLUDED.pending_reason, \
           verified_by = EXCLUDED.verified_by, verified_at = EXCLUDED.verified_at, \
           changed_at = now(), previous_account_existed = true, updated_at = now() \
         RETURNING {COLUMNS}"
    ))
    .bind(store_id)
    .bind(&input.bank_code)
    .bind(&number_enc)
    .bind(&last4)
    .bind(&name_enc)
    .bind(if decision.verified {
        "verified"
    } else {
        "pending"
    })
    .bind(decision.reason)
    .bind(decision.verified)
    .fetch_one(&mut *tx)
    .await
    .context("upsert payout account")?;
    sync_store_status(&mut tx, store_id, &store_status, decision.verified).await?;
    audit::record(
        &mut tx,
        Some(owner),
        "payout_account.updated",
        "store",
        Some(store_id),
        json!({ "store_id": store_id, "last4": last4 }),
    )
    .await?;
    if decision.verified {
        audit::record(
            &mut tx,
            Some(owner),
            "payout_account.verified",
            "store",
            Some(store_id),
            json!({ "verified_by": "provider" }),
        )
        .await?;
    }
    tx.commit().await.context("commit")?;
    row.view(state, false)
}

pub async fn get_for_owner(
    state: &AppState,
    owner: Uuid,
    store_id: Uuid,
) -> Result<PayoutAccountView, AppError> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {} FROM payout_accounts pa JOIN stores s ON s.id = pa.store_id \
         WHERE pa.store_id = $1 AND s.owner_id = $2 AND s.deleted_at IS NULL",
        COLUMNS
            .split(", ")
            .map(|c| format!("pa.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .bind(store_id)
    .bind(owner)
    .fetch_optional(&state.db)
    .await
    .context("get payout account")?;
    row.ok_or(AppError::NotFound)?.view(state, false)
}

/// Owner/support read. Reading the holder's name is audited (ids only).
pub async fn get_for_admin(
    state: &AppState,
    actor: Uuid,
    store_id: Uuid,
) -> Result<PayoutAccountView, AppError> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM payout_accounts WHERE store_id = $1"
    ))
    .bind(store_id)
    .fetch_optional(&state.db)
    .await
    .context("get payout account")?;
    let row = row.ok_or(AppError::NotFound)?;
    let mut conn = state.db.acquire().await.context("acquire")?;
    audit::record(
        &mut conn,
        Some(actor),
        "payout_account.viewed",
        "store",
        Some(store_id),
        json!({}),
    )
    .await?;
    row.view(state, true)
}

/// Owner manual verification, for accounts the provider could not verify.
pub async fn owner_verify(
    state: &AppState,
    actor: Uuid,
    store_id: Uuid,
    input: VerifyPayoutAccount,
) -> Result<PayoutAccountView, AppError> {
    let name = input.account_name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(AppError::Validation(
            "account_name must be 1 to 100 characters".into(),
        ));
    }
    let name_enc = state.cipher.encrypt(name);
    let mut tx = state.db.begin().await.context("begin")?;
    let store_status = lock_store(&mut tx, store_id, None).await?;
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT store_id FROM payout_accounts WHERE store_id = $1 FOR UPDATE")
            .bind(store_id)
            .fetch_optional(&mut *tx)
            .await
            .context("lock payout account")?;
    exists.ok_or(AppError::NotFound)?;
    let row: Row = sqlx::query_as(&format!(
        "UPDATE payout_accounts SET status = 'verified', verified_by = 'owner', \
                verified_at = now(), account_name_enc = $2, pending_reason = NULL, \
                updated_at = now() \
         WHERE store_id = $1 RETURNING {COLUMNS}"
    ))
    .bind(store_id)
    .bind(&name_enc)
    .fetch_one(&mut *tx)
    .await
    .context("verify payout account")?;
    sync_store_status(&mut tx, store_id, &store_status, true).await?;
    audit::record(
        &mut tx,
        Some(actor),
        "payout_account.verified",
        "store",
        Some(store_id),
        json!({ "verified_by": "owner" }),
    )
    .await?;
    tx.commit().await.context("commit")?;
    row.view(state, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nuban_cbn_example_vector() {
        // CBN worked example: bank 011, serial 000001457, check digit 9.
        assert!(nuban_check_ok("011", "0000014579"));
        assert!(!nuban_check_ok("011", "0000014578"));
        assert!(!nuban_check_ok("011", "000001457"));
        assert!(!nuban_check_ok("0110", "0000014579"));
        assert!(!nuban_check_ok("01a", "0000014579"));
        // Check digit 0 when the sum is a multiple of ten.
        assert!(nuban_check_ok("058", "0123456785"));
    }

    #[test]
    fn name_matching_is_token_based() {
        assert!(names_match("Ada Obi", "OBI ADA CHINWE"));
        assert!(names_match("Ada", "ADA OBI"));
        assert!(names_match("ada-obi", "Obi, Ada"));
        assert!(!names_match("Ada Obi", "CHIDI OKAFOR"));
        // One shared token is not enough when the user has two.
        assert!(!names_match("Ada Obi", "ADA OKAFOR"));
        // Single-letter tokens are ignored; nothing usable never matches.
        assert!(!names_match("A B", "A B"));
        assert!(!names_match("", "ADA OBI"));
    }
}
