//! Owner admin endpoints (Task 10): the backstop queue, owner release, refund
//! and hold, manual money overrides (mark-paid, mark-refunded), payout
//! re-queue after a reversal, clearing a provider flag, reconciliation lists
//! and bank holidays.
//!
//! **Roles.** Every mutation is Owner-only; Support may read the backstop
//! queue, the reconciliation list and the bank holidays. The role is checked
//! by the [`Owner`]/[`Staff`] extractors, before the body is parsed, so a
//! non-owner gets 403 whatever it sends.
//!
//! **Idempotency.** Every money action is a state-conditional transition under
//! the row locks (global lock order): a retry gets 409 naming the current
//! state and never acts twice. Amounts and destinations always come from the
//! stored rows, never from the request.
//!
//! **Audit and personal data.** Every mutation writes an `admin.*` audit row
//! with the actor in its own transaction; every list read writes
//! `admin.viewed`. Lists show ids, amounts, statuses and the last four digits
//! of an account only: never an account number, never buyer data. Support
//! never sees owner-written free text (hold reasons, notes). Free-text notes
//! are refused if they hold a run of 10+ digits (an account number), spaces,
//! `-` and `.` ignored; a provider reference of exactly ten digits is refused.
//! Nothing here logs personal data.
//!
//! **Double payment.** Mark-paid on a payout the provider may have paid asks
//! the provider first (see [`money::mark_paid`]).

pub mod holidays;
pub mod money;
pub mod orders;
pub mod routes;
pub mod views;

use axum::{extract::FromRequestParts, http::request::Parts};
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthUser, Role},
    error::AppError,
};

pub(crate) const LOCK_TIMEOUT: &str = "SET LOCAL lock_timeout = '5s'";

/// The signed-in Owner (403 for anyone else).
pub struct Owner(pub AuthUser);

/// The signed-in Owner or Support (403 for anyone else).
pub struct Staff(pub AuthUser);

impl FromRequestParts<AppState> for Owner {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        if user.role != Role::Owner {
            return Err(AppError::Forbidden("forbidden"));
        }
        Ok(Owner(user))
    }
}

impl FromRequestParts<AppState> for Staff {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        user.require_role(Role::Support)?;
        Ok(Staff(user))
    }
}

impl Staff {
    pub fn is_owner(&self) -> bool {
        self.0.role == Role::Owner
    }
}

pub(crate) fn parse_id(key: &str) -> Result<Uuid, AppError> {
    key.parse().map_err(|_| AppError::NotFound)
}

/// Whether `c[i..]` starts with `n` ASCII digits.
fn digits_at(c: &[char], i: usize, n: usize) -> bool {
    c.len() >= i + n && c[i..i + n].iter().all(char::is_ascii_digit)
}

/// The length of an ISO date (`2026-10-28`) or a clock time (`9:05`,
/// `14.30`) starting at `c[i]`, if one does. Neither may continue into more
/// digits; a time may not continue into `<separator><digit>` either, so a
/// dotted number ("12.34.56.7890") is never read as a time.
fn date_or_time_at(c: &[char], i: usize) -> Option<usize> {
    if i > 0 && c[i - 1].is_ascii_digit() {
        return None;
    }
    let ends = |end: usize| !c.get(end).is_some_and(char::is_ascii_digit);
    if digits_at(c, i, 4)
        && c.get(i + 4) == Some(&'-')
        && digits_at(c, i + 5, 2)
        && c.get(i + 7) == Some(&'-')
        && digits_at(c, i + 8, 2)
        && ends(i + 10)
    {
        return Some(10);
    }
    for hour in [1, 2] {
        let end = i + hour + 3;
        if digits_at(c, i, hour)
            && matches!(c.get(i + hour), Some(':' | '.'))
            && digits_at(c, i + hour + 1, 2)
            && ends(end)
            && !(matches!(c.get(end), Some(' ' | '-' | '.' | ':'))
                && c.get(end + 1).is_some_and(char::is_ascii_digit))
        {
            return Some(hour + 3);
        }
    }
    None
}

/// Longest run of ASCII digits in `s`, ignoring the separators people write
/// inside an account number (spaces, `-`, `.`): "0123 456 785" is one run.
/// ISO dates and clock times are not digits for this count (item (5)): "on
/// 2026-10-28 at 14:30" has no long run.
fn longest_digit_run(s: &str) -> usize {
    let c: Vec<char> = s.chars().collect();
    let mut masked = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        if let Some(n) = date_or_time_at(&c, i) {
            masked.push('_');
            i += n;
        } else {
            masked.push(c[i]);
            i += 1;
        }
    }
    masked
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '.'))
        .collect::<String>()
        .split(|c: char| !c.is_ascii_digit())
        .map(str::len)
        .max()
        .unwrap_or(0)
}

/// A required free-text field: trimmed, 1..=`max` characters, no control
/// characters, and no run of 10 or more digits (a NUBAN account number must
/// never reach the audit log).
pub(crate) fn note(field: &str, value: Option<&str>, max: usize) -> Result<String, AppError> {
    let v = value.unwrap_or("").trim();
    if v.is_empty() || v.chars().count() > max {
        return Err(AppError::Validation(format!(
            "{field} must be 1 to {max} characters"
        )));
    }
    if v.chars().any(char::is_control) {
        return Err(AppError::Validation(format!(
            "{field} must not contain control characters"
        )));
    }
    if longest_digit_run(v) >= 10 {
        return Err(AppError::Validation(format!(
            "{field} must not contain an account number"
        )));
    }
    Ok(v.to_string())
}

/// The bank or provider transaction reference the owner checked: 1..=100 of
/// `[A-Za-z0-9_./:-]` (matches the column CHECK). Exactly ten digits is
/// refused: that is a NUBAN account number, not a transaction reference.
pub(crate) fn provider_reference(value: Option<&str>) -> Result<String, AppError> {
    let v = value.unwrap_or("").trim();
    let ok = (1..=100).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b));
    if !ok {
        return Err(AppError::Validation(
            "provider_reference must be 1 to 100 letters, digits or _ . / : -".into(),
        ));
    }
    if v.len() == 10 && v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AppError::Validation(
            "provider_reference must not be an account number".into(),
        ));
    }
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_refuse_account_numbers_and_blanks() {
        assert_eq!(note("note", Some("  paid  "), 500).unwrap(), "paid");
        assert!(note("note", None, 500).is_err());
        assert!(note("note", Some("   "), 500).is_err());
        assert!(note("note", Some("to 0123456785"), 500).is_err());
        assert!(note("note", Some("order 123456789 ok"), 500).is_ok());
        // Separators inside the number do not hide it.
        assert!(note("note", Some("to 0123 456 785"), 500).is_err());
        assert!(note("note", Some("to 012-345-6785"), 500).is_err());
        assert!(note("note", Some("to 012.345.6785"), 500).is_err());
        assert!(note("note", Some("order 12345, ref 67890"), 500).is_ok());
        assert!(note("note", Some("a\u{0}b"), 500).is_err());
    }

    /// Item (5): dates and times are not account numbers; numbers still are.
    #[test]
    fn notes_allow_dates_and_times_but_not_account_numbers() {
        for ok in [
            "paid on 2026-10-28 at 14:30",
            "paid 2026-10-28 10:30:15",
            "transfer at 9.05 on 2026-10-28",
            "between 10:30-11:00 on 2026-10-28",
            "2026-10-28 2026-10-29",
        ] {
            assert!(note("note", Some(ok), 500).is_ok(), "{ok}");
        }
        for bad in [
            "0123 456 7890",
            "0123-456-7890",
            "to 0123456785 on 2026-10-28",
            "2026-10-28 0123456785",
            "at 14:30 0123 456 785",
            "12.34.56.7890",
            "2026-10-280123456785",
        ] {
            assert!(note("note", Some(bad), 500).is_err(), "{bad}");
        }
        assert!(note("note", Some(&"x".repeat(501)), 500).is_err());
    }

    #[test]
    fn provider_references_are_plain() {
        assert_eq!(provider_reference(Some(" NIP-1/2 ")).unwrap(), "NIP-1/2");
        assert!(provider_reference(Some("")).is_err());
        assert!(provider_reference(Some("has space")).is_err());
        assert!(provider_reference(Some(&"A".repeat(101))).is_err());
        // A bare NUBAN is not a reference.
        assert!(provider_reference(Some("0123456785")).is_err());
        assert!(provider_reference(Some("012345678")).is_ok());
        assert!(provider_reference(Some("NIP0123456785")).is_ok());

        // NIP session ids are long digit strings: allowed here, not in notes.
        assert!(provider_reference(Some("000013261025123456789012345678")).is_ok());
    }
}
