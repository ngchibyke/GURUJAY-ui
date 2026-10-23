use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Postgres, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{audit, error::AppError, limits, money::FeeSchedule};

/// Payment methods the owner may enable. Task 5 replaces this with the provider registry.
const KNOWN_METHODS: &[&str] = &["mono_directpay"];
/// Payout providers the owner may select.
const KNOWN_PAYOUT_PROVIDERS: &[&str] = &["mono"];

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Settings {
    pub platform_cut_bps: i32,
    pub fee_small_kobo: i64,
    pub fee_large_kobo: i64,
    pub fee_threshold_kobo: i64,
    pub stamp_duty_kobo: i64,
    pub stamp_duty_threshold_kobo: i64,
    pub enabled_methods: Vec<String>,
    pub payout_provider: String,
    pub checkout_ttl_minutes: i32,
    pub ship_deadline_hours: i32,
    pub release_after_hours: i32,
    pub backstop_days: i32,
    pub payout_settlement_buffer_hours: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl Settings {
    pub fn fee_schedule(&self) -> FeeSchedule {
        FeeSchedule {
            small_fee: self.fee_small_kobo,
            large_fee: self.fee_large_kobo,
            threshold: self.fee_threshold_kobo,
            stamp_duty_kobo: self.stamp_duty_kobo,
            stamp_duty_threshold: self.stamp_duty_threshold_kobo,
        }
    }
}

const SELECT: &str = "SELECT platform_cut_bps, fee_small_kobo, fee_large_kobo, \
    fee_threshold_kobo, stamp_duty_kobo, stamp_duty_threshold_kobo, enabled_methods, \
    payout_provider, checkout_ttl_minutes, ship_deadline_hours, release_after_hours, \
    backstop_days, payout_settlement_buffer_hours, updated_at \
    FROM platform_settings WHERE id";

pub async fn load(db: &PgPool) -> Result<Settings, AppError> {
    let s = sqlx::query_as::<_, Settings>(SELECT)
        .fetch_one(db)
        .await
        .context("load platform_settings")?;
    Ok(s)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub platform_cut_bps: Option<i32>,
    pub fee_small_kobo: Option<i64>,
    pub fee_large_kobo: Option<i64>,
    pub fee_threshold_kobo: Option<i64>,
    pub stamp_duty_kobo: Option<i64>,
    pub stamp_duty_threshold_kobo: Option<i64>,
    pub enabled_methods: Option<Vec<String>>,
    pub payout_provider: Option<String>,
    pub checkout_ttl_minutes: Option<i32>,
    pub ship_deadline_hours: Option<i32>,
    pub release_after_hours: Option<i32>,
    pub backstop_days: Option<i32>,
    pub payout_settlement_buffer_hours: Option<i32>,
}

fn in_range<T: PartialOrd>(field: &str, v: Option<T>, lo: T, hi: T) -> Result<(), AppError> {
    match v {
        Some(v) if v < lo || v > hi => {
            Err(AppError::Validation(format!("{field} is out of range")))
        }
        _ => Ok(()),
    }
}

impl SettingsPatch {
    fn validate(&self) -> Result<(), AppError> {
        let max = limits::MAX_PRICE_KOBO;
        in_range("platform_cut_bps", self.platform_cut_bps, 0, 3000)?;
        in_range("fee_small_kobo", self.fee_small_kobo, 0, max)?;
        in_range("fee_large_kobo", self.fee_large_kobo, 0, max)?;
        in_range("fee_threshold_kobo", self.fee_threshold_kobo, 0, i64::MAX)?;
        in_range("stamp_duty_kobo", self.stamp_duty_kobo, 0, max)?;
        in_range(
            "stamp_duty_threshold_kobo",
            self.stamp_duty_threshold_kobo,
            0,
            i64::MAX,
        )?;
        in_range("checkout_ttl_minutes", self.checkout_ttl_minutes, 5, 120)?;
        in_range("ship_deadline_hours", self.ship_deadline_hours, 1, 168)?;
        in_range("release_after_hours", self.release_after_hours, 1, 720)?;
        in_range("backstop_days", self.backstop_days, 1, 90)?;
        in_range(
            "payout_settlement_buffer_hours",
            self.payout_settlement_buffer_hours,
            0,
            72,
        )?;
        if let Some(p) = &self.payout_provider
            && !KNOWN_PAYOUT_PROVIDERS.contains(&p.as_str())
        {
            return Err(AppError::Unprocessable(
                "unknown_payout_provider",
                "payout provider is not supported".into(),
            ));
        }
        if let Some(methods) = &self.enabled_methods {
            if methods.is_empty() {
                return Err(AppError::Validation(
                    "enabled_methods must not be empty".into(),
                ));
            }
            for (i, m) in methods.iter().enumerate() {
                if !KNOWN_METHODS.contains(&m.as_str()) {
                    return Err(AppError::Unprocessable(
                        "unknown_payment_method",
                        "a payment method is not registered".into(),
                    ));
                }
                if methods[..i].contains(m) {
                    return Err(AppError::Validation(
                        "enabled_methods must not contain duplicates".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub async fn update(db: &PgPool, actor: Uuid, patch: SettingsPatch) -> Result<Settings, AppError> {
    patch.validate()?;

    // Lock the row so concurrent PATCHes serialise (last write wins, no lost partial updates),
    // then validate the merged result against the values being replaced.
    let mut tx = db.begin().await.context("begin")?;
    let old = sqlx::query_as::<_, Settings>(&format!("{SELECT} FOR UPDATE"))
        .fetch_one(&mut *tx)
        .await
        .context("lock platform_settings")?;
    let backstop = patch.backstop_days.unwrap_or(old.backstop_days);
    let release = patch.release_after_hours.unwrap_or(old.release_after_hours);
    if i64::from(backstop) * 24 <= i64::from(release) {
        return Err(AppError::Unprocessable(
            "invalid_settings",
            "backstop_days must outlast release_after_hours".into(),
        ));
    }

    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE platform_settings SET ");
    let mut changed: Vec<&'static str> = Vec::new();
    {
        let mut sep = qb.separated(", ");
        macro_rules! set {
            ($($f:ident),* $(,)?) => {$(
                if let Some(v) = patch.$f {
                    sep.push(concat!(stringify!($f), " = ")).push_bind_unseparated(v);
                    changed.push(stringify!($f));
                }
            )*};
        }
        set!(
            platform_cut_bps,
            fee_small_kobo,
            fee_large_kobo,
            fee_threshold_kobo,
            stamp_duty_kobo,
            stamp_duty_threshold_kobo,
            enabled_methods,
            payout_provider,
            checkout_ttl_minutes,
            ship_deadline_hours,
            release_after_hours,
            backstop_days,
            payout_settlement_buffer_hours,
        );
    }
    if changed.is_empty() {
        return Err(AppError::Validation(
            "at least one setting is required".into(),
        ));
    }
    qb.push(", updated_at = now() WHERE id");

    qb.build()
        .execute(&mut *tx)
        .await
        .context("update platform_settings")?;
    let s = sqlx::query_as::<_, Settings>(SELECT)
        .fetch_one(&mut *tx)
        .await
        .context("reload platform_settings")?;
    // Fees and timers are commercial configuration, not personal data: old/new values are audited.
    let (old_v, new_v) = (
        serde_json::to_value(&old).context("serialise old settings")?,
        serde_json::to_value(&s).context("serialise new settings")?,
    );
    let detail: serde_json::Map<String, serde_json::Value> = changed
        .iter()
        .map(|f| (f.to_string(), json!({ "old": old_v[*f], "new": new_v[*f] })))
        .collect();
    audit::record(
        &mut tx,
        Some(actor),
        "settings.updated",
        "platform_settings",
        None,
        serde_json::Value::Object(detail),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(s)
}
