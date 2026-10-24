//! The double-entry ledger (`ledger_entries`, append-only).
//!
//! Sign convention: a **positive amount is a debit** to that account, a
//! negative amount a credit. Money flowing into the platform's books is
//! positive on the internal account and negative on the external one, e.g. a
//! payment posts `buyer_funds -T`, `escrow +T`.
//!
//! Every write goes through [`post`], which stamps one `txn_id` on a set of
//! entries that sums to zero. The database re-checks the zero sum per
//! `txn_id` at commit (a deferred constraint trigger), and rejects UPDATE,
//! DELETE and TRUNCATE. Balances are derived with `SUM(amount_kobo)`.

use std::collections::HashMap;

use anyhow::Context;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "ledger_account", rename_all = "snake_case")]
pub enum Account {
    /// External: the buyer's money outside the platform.
    BuyerFunds,
    /// Held for an order until release or refund.
    Escrow,
    PlatformRevenue,
    ProviderFees,
    SellerPayable,
    SellerPaid,
    /// Money returned to buyers.
    Refunds,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub order_id: Option<Uuid>,
    pub checkout_id: Option<Uuid>,
    pub account: Account,
    /// Signed kobo; positive = debit. Zero-amount entries are skipped.
    pub amount_kobo: i64,
    /// What caused it, e.g. "payment", "refund".
    pub kind: &'static str,
    /// Our provider reference (checkout, refund or payout); never personal data.
    pub reference: Option<String>,
}

/// Inserts `entries` as one balanced transaction (a fresh `txn_id`) inside the
/// caller's DB transaction. Fails, inserting nothing, when the set is empty,
/// has no non-zero entry, or does not sum to zero overall and per `order_id`
/// (entries with an order id); those are programming errors.
pub async fn post(conn: &mut PgConnection, entries: &[Entry]) -> Result<(), AppError> {
    let entries: Vec<&Entry> = entries.iter().filter(|e| e.amount_kobo != 0).collect();
    if entries.is_empty() {
        return Err(anyhow::anyhow!("ledger post with no entries").into());
    }
    let sum: i128 = entries.iter().map(|e| i128::from(e.amount_kobo)).sum();
    if sum != 0 {
        return Err(anyhow::anyhow!("ledger post does not balance").into());
    }
    // Every order's own entries balance too (global constraint: per order, zero).
    let mut per_order: HashMap<Uuid, i128> = HashMap::new();
    for e in &entries {
        if let Some(order_id) = e.order_id {
            *per_order.entry(order_id).or_default() += i128::from(e.amount_kobo);
        }
    }
    if per_order.values().any(|&s| s != 0) {
        return Err(anyhow::anyhow!("ledger post does not balance per order").into());
    }
    let txn_id = Uuid::new_v4();
    for e in entries {
        sqlx::query(
            "INSERT INTO ledger_entries \
                (txn_id, order_id, checkout_id, account, amount_kobo, kind, ref) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(txn_id)
        .bind(e.order_id)
        .bind(e.checkout_id)
        .bind(e.account)
        .bind(e.amount_kobo)
        .bind(e.kind)
        .bind(&e.reference)
        .execute(&mut *conn)
        .await
        .context("insert ledger entry")?;
    }
    Ok(())
}
