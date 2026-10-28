//! Bank holidays (Lagos dates) that the payout settlement wait skips. Owner
//! edits, audited; Support may read. A change affects payouts released from
//! then on: a payout's `send_after` is fixed at release.

use anyhow::Context;
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;
use time::{Date, Month};
use uuid::Uuid;

use crate::{audit, error::AppError};

#[derive(Debug, Serialize)]
pub struct Holiday {
    pub day: String,
    pub name: String,
}

/// `YYYY-MM-DD`, a real calendar date.
pub fn parse_day(s: &str) -> Result<Date, AppError> {
    let bad = || AppError::Validation("day must be a date as YYYY-MM-DD".into());
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return Err(bad());
    }
    let num = |r: std::ops::Range<usize>| {
        s.get(r)
            .filter(|p| p.bytes().all(|c| c.is_ascii_digit()))
            .and_then(|p| p.parse::<i32>().ok())
            .ok_or_else(bad)
    };
    let month = u8::try_from(num(5..7)?).map_err(|_| bad())?;
    let day = u8::try_from(num(8..10)?).map_err(|_| bad())?;
    let month = Month::try_from(month).map_err(|_| bad())?;
    Date::from_calendar_date(num(0..4)?, month, day).map_err(|_| bad())
}

fn format_day(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

pub async fn list(db: &PgPool) -> Result<Vec<Holiday>, AppError> {
    let rows: Vec<(Date, String)> =
        sqlx::query_as("SELECT day, name FROM bank_holidays ORDER BY day LIMIT 1000")
            .fetch_all(db)
            .await
            .context("list bank holidays")?;
    Ok(rows
        .into_iter()
        .map(|(day, name)| Holiday {
            day: format_day(day),
            name,
        })
        .collect())
}

/// Adds or renames the holiday on `day`.
pub async fn set(db: &PgPool, actor: Uuid, day: Date, name: &str) -> Result<Holiday, AppError> {
    let mut tx = db.begin().await.context("begin")?;
    let old: Option<String> =
        sqlx::query_scalar("SELECT name FROM bank_holidays WHERE day = $1 FOR UPDATE")
            .bind(day)
            .fetch_optional(&mut *tx)
            .await
            .context("lock bank holiday")?;
    sqlx::query(
        "INSERT INTO bank_holidays (day, name) VALUES ($1, $2) \
         ON CONFLICT (day) DO UPDATE SET name = EXCLUDED.name",
    )
    .bind(day)
    .bind(name)
    .execute(&mut *tx)
    .await
    .context("upsert bank holiday")?;
    audit::record(
        &mut tx,
        Some(actor),
        "admin.bank_holiday_set",
        "bank_holiday",
        None,
        json!({ "day": format_day(day), "name": name, "old_name": old }),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(Holiday {
        day: format_day(day),
        name: name.to_string(),
    })
}

pub async fn remove(db: &PgPool, actor: Uuid, day: Date) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    let name: String =
        sqlx::query_scalar("DELETE FROM bank_holidays WHERE day = $1 RETURNING name")
            .bind(day)
            .fetch_optional(&mut *tx)
            .await
            .context("delete bank holiday")?
            .ok_or(AppError::NotFound)?;
    audit::record(
        &mut tx,
        Some(actor),
        "admin.bank_holiday_removed",
        "bank_holiday",
        None,
        json!({ "day": format_day(day), "name": name }),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_round_trip_and_bad_dates_fail() {
        let d = parse_day("2026-12-25").unwrap();
        assert_eq!(format_day(d), "2026-12-25");
        for bad in [
            "2026-13-01",
            "2026-02-30",
            "26-12-25",
            "2026/12/25",
            "2026-1a-01",
            "",
        ] {
            assert!(parse_day(bad).is_err(), "{bad}");
        }
    }
}
