//! Mono adapter: DirectPay one-time debits for collection, Disburse for seller
//! payouts. API facts are from docs.mono.co (checked 2026-10-23 and
//! 2026-10-25, see the Task 11 research); shapes Mono does not document are
//! marked **unconfirmed** where they are used.
//!
//! Rules this adapter keeps:
//! - **Data protection.** Request and response bodies carry names and bank
//!   account numbers: they are never logged or stored. Logs carry only the
//!   operation, the HTTP status and a fixed error class. The secret key lives
//!   in [`Secret`] (redacted `Debug`) and its header is marked sensitive.
//!   Data sent to Mono is minimised: the buyer's display name, an email only
//!   when the buyer has one, and for payouts the seller's bank code and account
//!   number with a platform-owned recipient email (never the seller's).
//! - **No retries inside the adapter.** `initiate` and `create_payout` are sent
//!   once. A failure after sending is [`ProviderError::Timeout`] (outcome
//!   unknown); only a connect failure, where nothing was sent, is
//!   [`ProviderError::Unavailable`]. The escrow engine looks a payout up with
//!   [`find_payout`](PaymentProvider::find_payout) before ever re-creating it.
//! - **Lenient parsing, conservative mapping.** Amounts may be numbers or
//!   numeric strings, `livemode`/`liveMode` are both read, unknown fields are
//!   ignored. An unknown status maps to `Pending`, never to a terminal state.
//! - **HTTP client.** rustls, `https_only`, no redirects, a connect timeout
//!   and a total timeout.

use std::{fmt, time::Duration};

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use reqwest::{Method, Url, redirect::Policy};
use serde_json::{Value, json};

use super::{
    InitiatePayment, InitiatedPayment, PaymentProvider, PaymentStatus, PayoutHandle, PayoutRequest,
    PayoutState, ProviderError, RefundHandle, RefundRequest, WebhookEvent, secret_eq,
};

pub const MONO_NAME: &str = "mono";
/// Default API base URL (`MONO_BASE_URL` overrides it).
pub const DEFAULT_BASE_URL: &str = "https://api.withmono.com";
/// Connect timeout of the HTTP client (research ruling 6).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Total request timeout of the HTTP client, connect to end of body (ruling 6).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

const SECRET_HEADER: &str = "mono-sec-key";
/// Largest response body read; anything longer is treated like a failed read.
pub const MAX_RESPONSE_BODY: usize = 512 * 1024;
const WEBHOOK_SECRET_HEADER: &str = "mono-webhook-secret";

/// Hosts Mono's hosted payment pages are served from (exact host or a
/// subdomain). Docs examples show `checkout.mono.co`; the API itself is on
/// `withmono.com`. **Unconfirmed** as a complete list (research Q12).
const PAYMENT_URL_HOSTS: &[&str] = &["mono.co", "withmono.com"];

/// DirectPay webhook events. Each one only triggers a verify; the verify
/// answer decides what happens (`failed` events may be retried at the bank).
const PAYMENT_EVENTS: &[&str] = &[
    "direct_debit.payment_successful",
    "direct_debit.payment_failed",
    "direct_debit.payment_cancelled",
    "direct_debit.payment_abandoned",
];
/// Disburse per-transaction events: the only ones that settle a payout,
/// matched by `data.reference` (the distribution reference, which we set equal
/// to the payout reference).
const PAYOUT_EVENTS: &[&str] = &[
    "event.disbursement.transaction.successful",
    "event.disbursement.transaction.failed",
];
/// Known events we deliberately ignore (logged by name). A batch `completed`
/// event can carry a failed distribution, so it never settles anything.
const IGNORED_EVENTS: &[&str] = &[
    "event.disbursement.completed",
    "event.disbursement.created",
    "event.disburse.account.created",
    "event.disburse.account.updated",
    "mono.events.account_connected",
];

/// A secret string (API key, webhook secret). `Debug` never prints it.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: &str) -> Self {
        Self(value.to_string())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// The unit Mono's `amount` fields are in, set per product by config
/// (`MONO_DIRECTPAY_AMOUNT_UNIT`, `MONO_DISBURSE_AMOUNT_UNIT`).
///
/// - DirectPay: documented as kobo ("the smallest denomination, such as a
///   Kobo", verify page). **To be confirmed by hand** with
///   `sandbox_smoke_test` (the hosted page must show ₦100.00 for 10,000).
///   Result: not yet run.
/// - Disburse: **undocumented**; no default, required in live mode. To be
///   confirmed with a minimum live payout to an owner account, checked against
///   the bank statement. Result: not yet run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountUnit {
    Kobo,
    Naira,
}

impl AmountUnit {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "kobo" => Some(Self::Kobo),
            "naira" => Some(Self::Naira),
            _ => None,
        }
    }
}

/// Our kobo amount in `unit`, for a request body. Checked: zero, negative and
/// (for naira) non-whole amounts are refused, never rounded.
fn to_mono_amount(kobo: i64, unit: AmountUnit) -> Result<i64, ProviderError> {
    if kobo <= 0 {
        return Err(rejected_locally("invalid_amount"));
    }
    match unit {
        AmountUnit::Kobo => Ok(kobo),
        AmountUnit::Naira if kobo % 100 == 0 => Ok(kobo / 100),
        AmountUnit::Naira => Err(rejected_locally("amount_not_whole_naira")),
    }
}

/// A Mono amount (JSON number or numeric string, in `unit`) as kobo. `None`
/// for anything that is not a plain non-negative decimal, has more precision
/// than a kobo, or overflows.
fn from_mono_amount(value: &Value, unit: AmountUnit) -> Option<i64> {
    let text = match value {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_string(),
        _ => return None,
    };
    let (whole, frac) = text.split_once('.').unwrap_or((&text, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole: i64 = whole.parse().ok()?;
    let (scale, places) = match unit {
        AmountUnit::Kobo => (1, 0),
        AmountUnit::Naira => (100, 2),
    };
    // Digits beyond a kobo must be zero.
    if frac.bytes().skip(places).any(|b| b != b'0') {
        return None;
    }
    let mut minor: i64 = 0;
    for i in 0..places {
        let digit = frac.as_bytes().get(i).map_or(0, |b| i64::from(b - b'0'));
        minor = minor * 10 + digit;
    }
    whole.checked_mul(scale)?.checked_add(minor)
}

/// Mono settings, from the environment (see `config.rs`).
#[derive(Debug, Clone)]
pub struct MonoConfig {
    pub secret_key: Secret,
    pub webhook_secret: Secret,
    /// `MONO_BASE_URL`, default [`DEFAULT_BASE_URL`].
    pub base_url: String,
    /// Live mode (`APP_ENV=production`). Webhooks from the other mode are ignored.
    pub live: bool,
    pub directpay_unit: AmountUnit,
    /// `None` (allowed outside live mode only): payouts are `NotSupported`.
    pub disburse_unit: Option<AmountUnit>,
    /// The Mono account id Disburse debits (an id such as
    /// `688a0b672f88771f77d05cf5`, not a bank account number).
    pub disburse_source_account: Option<String>,
    /// Platform-owned recipient email sent on every distribution instead of
    /// the seller's own (data minimisation).
    pub payout_recipient_email: Option<String>,
    pub connect_timeout: Duration,
    pub timeout: Duration,
    /// **Tests only** (a local mock server speaks plain http). `config.rs`
    /// always sets `false`, so production refuses non-https URLs.
    pub insecure_http_for_tests: bool,
}

/// Whether a call can have taken effect at Mono when it fails after sending.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Call {
    /// GET: no side effects; a failure after sending is still "not done".
    Read,
    /// POST: a failure after sending leaves the outcome unknown.
    Write,
}

/// A Mono answer: HTTP status plus the JSON body, when it parsed.
struct Answer {
    status: StatusCode,
    body: Option<Value>,
}

pub struct MonoProvider {
    cfg: MonoConfig,
    base: Url,
    client: reqwest::Client,
    key_header: HeaderValue,
}

impl fmt::Debug for MonoProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MonoProvider")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl MonoProvider {
    pub fn new(cfg: MonoConfig) -> anyhow::Result<Self> {
        let base = Url::parse(cfg.base_url.trim_end_matches('/'))
            .ok()
            .filter(|u| !u.cannot_be_a_base() && u.query().is_none())
            .context("MONO_BASE_URL is not a valid base URL")?;
        let mut key_header = HeaderValue::from_str(cfg.secret_key.expose())
            .map_err(|_| anyhow!("MONO_SECRET_KEY contains invalid characters"))?;
        key_header.set_sensitive(true);
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .connect_timeout(cfg.connect_timeout)
            .timeout(cfg.timeout)
            .redirect(Policy::none())
            .https_only(!cfg.insecure_http_for_tests)
            .build()
            .map_err(|e| anyhow!(e.without_url()))
            .context("build Mono HTTP client")?;
        Ok(Self {
            cfg,
            base,
            client,
            key_header,
        })
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }

    /// Sends one request (no retry) and reads the body. Never logs the body.
    async fn call(
        &self,
        op: &'static str,
        call: Call,
        method: Method,
        url: Url,
        body: Option<&Value>,
    ) -> Result<Answer, ProviderError> {
        let mut req = self
            .client
            .request(method, url)
            .header(SECRET_HEADER, self.key_header.clone())
            .header("accept", "application/json");
        if let Some(body) = body {
            req = req.json(body);
        }
        let mut res = req.send().await.map_err(|e| transport_error(op, call, e))?;
        let status = res.status();
        // Capped read: an oversize body is a failed read (outcome unknown for a write).
        let oversize = || {
            tracing::warn!(
                provider = MONO_NAME,
                op,
                cause = "body_too_large",
                "mono call failed"
            );
            match call {
                Call::Write => ProviderError::Timeout,
                Call::Read => ProviderError::Unavailable(anyhow!("mono body too large")),
            }
        };
        if res
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE_BODY as u64)
        {
            return Err(oversize());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|e| transport_error(op, call, e))?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BODY {
                return Err(oversize());
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = serde_json::from_slice::<Value>(&bytes).ok();
        Ok(Answer { status, body })
    }

    /// Payout config needed before any Disburse call.
    fn disburse(&self) -> Result<(AmountUnit, &str, &str), ProviderError> {
        match (
            self.cfg.disburse_unit,
            self.cfg.disburse_source_account.as_deref(),
            self.cfg.payout_recipient_email.as_deref(),
        ) {
            (Some(unit), Some(source), Some(email)) => Ok((unit, source, email)),
            _ => Err(ProviderError::NotSupported),
        }
    }

    async fn disbursement_state(
        &self,
        reference: &str,
        id: &str,
        unit: AmountUnit,
    ) -> Result<PayoutState, ProviderError> {
        const OP: &str = "payout_status";
        let url = self.url(&["v3", "payments", "disburse", "disbursements", id]);
        let answer = self.call(OP, Call::Read, Method::GET, url, None).await?;
        if !answer.status.is_success() {
            return Err(status_error(OP, Call::Read, &answer));
        }
        let data = answer
            .body
            .as_ref()
            .and_then(|b| b.get("data"))
            .filter(|d| d.is_object())
            .ok_or_else(|| unparseable(OP))?;
        // Unconfirmed: the field holding the distributions. The outcome comes
        // only from our distribution's own status: a batch can be "completed"
        // while its distribution failed (recorded webhook sample).
        let Some(dist) = ["distributions", "distribution", "transactions"]
            .iter()
            .filter_map(|k| data.get(*k).and_then(Value::as_array))
            .flatten()
            .find(|d| str_at(d, "reference") == Some(reference))
        else {
            tracing::info!(
                provider = MONO_NAME,
                op = OP,
                "distribution not in disbursement; pending"
            );
            return Ok(PayoutState::Pending);
        };
        let status = str_at(dist, "status")
            .unwrap_or_default()
            .to_ascii_lowercase();
        Ok(match status.as_str() {
            "successful" | "success" | "completed" => {
                let fee = [dist.get("fee"), data.get("total_fee")]
                    .into_iter()
                    .flatten()
                    .find(|v| !v.is_null());
                let fee_kobo = match fee {
                    None => 0,
                    Some(v) => from_mono_amount(v, unit).ok_or_else(|| unparseable(OP))?,
                };
                PayoutState::Successful { fee_kobo }
            }
            // Fixed text only: Mono's own message can echo the account.
            "failed" => PayoutState::Failed {
                reason: "mono_distribution_failed".into(),
            },
            "reversed" => PayoutState::Reversed {
                reason: "mono_distribution_reversed".into(),
            },
            _ => PayoutState::Pending,
        })
    }
}

/// A refusal decided here, before anything is sent.
fn rejected_locally(code: &str) -> ProviderError {
    ProviderError::Rejected {
        code: code.to_string(),
        message: String::new(),
    }
}

/// A 2xx body we cannot read: the outcome is unknown (trait contract).
fn unparseable(op: &'static str) -> ProviderError {
    tracing::warn!(
        provider = MONO_NAME,
        op,
        cause = "unparseable_body",
        "mono call failed"
    );
    ProviderError::Timeout
}

/// Maps a reqwest failure. Logs only the sanitised class, never the error
/// itself (it may hold the URL, which carries references).
fn transport_error(op: &'static str, call: Call, e: reqwest::Error) -> ProviderError {
    let e = e.without_url();
    // Checked first: a connect failure means nothing was sent.
    let (cause, err) = if e.is_connect() {
        (
            "connect",
            ProviderError::Unavailable(anyhow!("mono connect failed")),
        )
    } else if e.is_timeout() {
        ("timeout", ProviderError::Timeout)
    } else if call == Call::Write {
        ("transport_after_send", ProviderError::Timeout)
    } else {
        (
            "transport",
            ProviderError::Unavailable(anyhow!("mono request failed")),
        )
    };
    tracing::warn!(provider = MONO_NAME, op, cause, "mono call failed");
    err
}

/// Maps a non-2xx answer.
/// - 429: not processed, retry later -> Unavailable.
/// - other 4xx: refused -> Rejected (code `http_<status>`, sanitised message).
/// - 5xx / 3xx: a read -> Unavailable; a write may have taken effect -> Timeout.
fn status_error(op: &'static str, call: Call, answer: &Answer) -> ProviderError {
    let status = answer.status.as_u16();
    tracing::warn!(
        provider = MONO_NAME,
        op,
        status,
        cause = "http_status",
        "mono call failed"
    );
    if answer.status == StatusCode::TOO_MANY_REQUESTS {
        ProviderError::Unavailable(anyhow!("mono rate limited"))
    } else if answer.status.is_client_error() {
        ProviderError::Rejected {
            code: format!("http_{status}"),
            message: safe_message(answer.body.as_ref()),
        }
    } else if call == Call::Write {
        ProviderError::Timeout
    } else {
        ProviderError::Unavailable(anyhow!("mono answered {status}"))
    }
}

/// Mono's top-level `message`, kept for support only when it is short and
/// holds no run of 6+ digits (account numbers, BVNs, phone numbers).
fn safe_message(body: Option<&Value>) -> String {
    let Some(msg) = body.and_then(|b| str_at(b, "message")) else {
        return String::new();
    };
    let mut run = 0;
    let has_long_number = msg.chars().any(|c| {
        run = if c.is_ascii_digit() { run + 1 } else { 0 };
        run >= 6
    });
    if has_long_number || msg.chars().count() > 160 {
        "[message withheld]".into()
    } else {
        msg.to_string()
    }
}

/// A refused create that may hide an earlier, successful attempt.
fn is_possible_duplicate(answer: &Answer) -> bool {
    if matches!(answer.status.as_u16(), 408 | 409 | 425) {
        return true;
    }
    answer.status.is_client_error()
        && answer
            .body
            .as_ref()
            .and_then(|b| str_at(b, "message"))
            .map(str::to_ascii_lowercase)
            .is_some_and(|m| m.contains("duplicate") || m.contains("already exist"))
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// An id that may be a JSON string or number (docs show both).
fn id_at(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn bool_at(v: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_bool))
}

fn mentions_not_found(v: Option<&Value>) -> bool {
    v.and_then(|v| str_at(v, "message"))
        .is_some_and(|m| m.to_ascii_lowercase().contains("not found"))
}

/// Logged name of an event: only names we know, so a forged or odd event name
/// never reaches the logs verbatim.
fn loggable_event(event: &str) -> &'static str {
    PAYMENT_EVENTS
        .iter()
        .chain(PAYOUT_EVENTS)
        .chain(IGNORED_EVENTS)
        .find(|e| **e == event)
        .copied()
        .unwrap_or("unknown")
}

#[async_trait]
impl PaymentProvider for MonoProvider {
    fn name(&self) -> &'static str {
        MONO_NAME
    }

    fn methods(&self) -> &'static [&'static str] {
        &["mono_directpay"]
    }

    fn payment_url_hosts(&self) -> &'static [&'static str] {
        PAYMENT_URL_HOSTS
    }

    /// `POST /v2/payments/initiate`. Customer: name, plus email only when the
    /// buyer has one (no phone, address or BVN is ever sent).
    async fn initiate(&self, req: InitiatePayment) -> Result<InitiatedPayment, ProviderError> {
        const OP: &str = "initiate";
        let amount = to_mono_amount(req.amount_kobo, self.cfg.directpay_unit)?;
        let mut customer = json!({ "name": req.customer_name });
        if let Some(email) = req.customer_email.filter(|e| !e.trim().is_empty()) {
            customer["email"] = json!(email);
        }
        let body = json!({
            "amount": amount,
            "type": "onetime-debit",
            "method": "account",
            "description": req.description,
            "reference": req.reference,
            "redirect_url": req.redirect_url,
            "customer": customer,
        });
        let url = self.url(&["v2", "payments", "initiate"]);
        let answer = self
            .call(OP, Call::Write, Method::POST, url, Some(&body))
            .await?;
        if !answer.status.is_success() {
            return Err(status_error(OP, Call::Write, &answer));
        }
        let data = answer.body.as_ref().and_then(|b| b.get("data"));
        let (Some(id), Some(url)) = (
            data.and_then(|d| id_at(d, "id")),
            data.and_then(|d| str_at(d, "mono_url")),
        ) else {
            return Err(unparseable(OP));
        };
        Ok(InitiatedPayment {
            provider_payment_id: id,
            payment_url: url.to_string(),
        })
    }

    /// `GET /v2/payments/verify/{reference}`.
    ///
    /// - 404, or `failed` with a "not found" message (Mono's answer for an
    ///   unfinished payment, ruling 1) -> Pending.
    /// - `data.status` `successful` -> Successful; `flagged` /
    ///   `held_settlement` (as status or as boolean fields) -> Successful with
    ///   `held: true`, which blocks release until the owner reviews it.
    /// - `data.status` `failed` without that message -> Failed.
    /// - No data, or any other status -> Pending (checkout expiry bounds the wait).
    async fn verify_payment(&self, reference: &str) -> Result<PaymentStatus, ProviderError> {
        const OP: &str = "verify";
        let url = self.url(&["v2", "payments", "verify", reference]);
        let answer = self.call(OP, Call::Read, Method::GET, url, None).await?;
        if answer.status == StatusCode::NOT_FOUND
            || (answer.status.is_client_error() && mentions_not_found(answer.body.as_ref()))
        {
            return Ok(PaymentStatus::Pending);
        }
        if !answer.status.is_success() {
            return Err(status_error(OP, Call::Read, &answer));
        }
        let body = answer.body.as_ref().ok_or_else(|| unparseable(OP))?;
        let not_found = mentions_not_found(Some(body));
        let Some(data) = body.get("data").filter(|d| d.is_object()) else {
            return Ok(PaymentStatus::Pending);
        };
        // A payment from the other mode (sandbox vs live) is never acted on.
        if bool_at(data, &["livemode", "liveMode"]).is_some_and(|l| l != self.cfg.live) {
            tracing::warn!(
                provider = MONO_NAME,
                op = OP,
                cause = "livemode_mismatch",
                "mono payment from the other mode; pending"
            );
            return Ok(PaymentStatus::Pending);
        }
        let status = str_at(data, "status")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let flagged = bool_at(data, &["flagged"]).unwrap_or(false)
            || bool_at(data, &["held_settlement"]).unwrap_or(false);
        match status.as_str() {
            "successful" | "flagged" | "held_settlement" => {
                let unit = self.cfg.directpay_unit;
                let amount_kobo = data
                    .get("amount")
                    .and_then(|v| from_mono_amount(v, unit))
                    .ok_or_else(|| unparseable(OP))?;
                let fee_kobo = match data.get("fee").filter(|v| !v.is_null()) {
                    None => 0,
                    Some(v) => from_mono_amount(v, unit).ok_or_else(|| unparseable(OP))?,
                };
                Ok(PaymentStatus::Successful {
                    amount_kobo,
                    fee_kobo,
                    // Passed through; settlement refuses anything but NGN.
                    currency: str_at(data, "currency").unwrap_or_default().to_string(),
                    held: flagged || status != "successful",
                    reference: str_at(data, "reference").unwrap_or_default().to_string(),
                })
            }
            "failed" if !not_found => Ok(PaymentStatus::Failed),
            _ => Ok(PaymentStatus::Pending),
        }
    }

    /// `POST /v3/payments/disburse/disbursements`, one distribution whose
    /// reference equals the batch reference (our payout reference), so either
    /// webhook reference maps back to the payout. Sent once, never retried.
    async fn create_payout(&self, req: PayoutRequest) -> Result<PayoutHandle, ProviderError> {
        const OP: &str = "create_payout";
        let (unit, source, email) = self.disburse()?;
        let amount = to_mono_amount(req.amount_kobo, unit)?;
        let body = json!({
            "reference": req.reference,
            "source": "mandate",
            "account": source,
            "type": "instant",
            "total_amount": amount,
            "description": req.narration,
            "distribution": [{
                "reference": req.reference,
                "recipient_email": email,
                "account": {
                    "account_number": req.account_number,
                    "bank_code": req.bank_code,
                },
                "amount": amount,
                "narration": req.narration,
            }],
        });
        let url = self.url(&["v3", "payments", "disburse", "disbursements"]);
        let answer = self
            .call(OP, Call::Write, Method::POST, url, Some(&body))
            .await?;
        if !answer.status.is_success() {
            // REVISIT(Q5): a duplicate reference may mean an earlier attempt
            // went through. Until Mono documents its answer, 408/409/425 and
            // any "duplicate" / "already exist" refusal are an unknown outcome
            // (looked up, never re-created), not Rejected.
            if is_possible_duplicate(&answer) {
                tracing::warn!(
                    provider = MONO_NAME,
                    op = OP,
                    status = answer.status.as_u16(),
                    cause = "possible_duplicate",
                    "mono call failed"
                );
                return Err(ProviderError::Timeout);
            }
            return Err(status_error(OP, Call::Write, &answer));
        }
        let id = answer
            .body
            .as_ref()
            .and_then(|b| b.get("data"))
            .and_then(|d| id_at(d, "id"))
            .ok_or_else(|| unparseable(OP))?;
        Ok(PayoutHandle {
            provider_payout_id: id,
        })
    }

    /// `GET /v3/payments/disburse/disbursements/{id}` (the id is looked up by
    /// reference first when we never got one). The state comes from our
    /// distribution's status: `successful` -> Successful (fee from the
    /// distribution, else the batch `total_fee`, else 0), `failed` -> Failed,
    /// `reversed` -> Reversed, anything else (or no distribution) -> Pending.
    async fn payout_status(
        &self,
        reference: &str,
        provider_payout_id: Option<&str>,
    ) -> Result<PayoutState, ProviderError> {
        let (unit, _, _) = self.disburse()?;
        let id = match provider_payout_id {
            Some(id) => id.to_string(),
            // find_payout never answers None today (fail closed); the arm
            // stays for when the lookup is confirmed.
            None => match self.find_payout(reference).await? {
                Some(handle) => handle.provider_payout_id,
                None => return Ok(PayoutState::Pending),
            },
        };
        self.disbursement_state(reference, &id, unit).await
    }

    /// "Fetch All Disbursements" filtered by reference:
    /// `GET /v3/payments/disburse/disbursements?reference={reference}`.
    ///
    /// **Unconfirmed** (research Q6): whether the list accepts a `reference`
    /// filter, and where the array sits (`data`, or `data.disbursements` /
    /// `data.items`), and whether in-flight disbursements are listed. So only a
    /// match on the page is conclusive (found). An empty list or a page of
    /// other disbursements is an error (`Timeout`, outcome unknown): this
    /// adapter never reports "absent", because that would let the engine
    /// create the payout a second time (double payout).
    async fn find_payout(&self, reference: &str) -> Result<Option<PayoutHandle>, ProviderError> {
        const OP: &str = "find_payout";
        self.disburse()?;
        let mut url = self.url(&["v3", "payments", "disburse", "disbursements"]);
        url.query_pairs_mut().append_pair("reference", reference);
        let answer = self.call(OP, Call::Read, Method::GET, url, None).await?;
        if !answer.status.is_success() {
            return Err(status_error(OP, Call::Read, &answer));
        }
        let data = answer.body.as_ref().and_then(|b| b.get("data"));
        let items = data
            .and_then(Value::as_array)
            .or_else(|| {
                ["disbursements", "items"]
                    .iter()
                    .find_map(|k| data?.get(*k)?.as_array())
            })
            .ok_or_else(|| unparseable(OP))?;
        if let Some(item) = items
            .iter()
            .find(|i| str_at(i, "reference") == Some(reference))
        {
            return id_at(item, "id")
                .map(|id| {
                    Some(PayoutHandle {
                        provider_payout_id: id,
                    })
                })
                .ok_or_else(|| unparseable(OP));
        }
        // REVISIT(Q5,Q6): fail closed. Until Mono confirms that `?reference=`
        // filters the list and that in-flight disbursements are listed, an
        // empty list is not proof of absence: returning None would let the
        // engine create the payout a second time. The engine keeps the payout
        // `unknown` and sends it to manual after its lookup cap.
        tracing::warn!(
            provider = MONO_NAME,
            op = OP,
            cause = if items.is_empty() {
                "empty_list"
            } else {
                "inconclusive_page"
            },
            "mono call failed"
        );
        Err(ProviderError::Timeout)
    }

    /// Not supported: Mono has a v2 "Refund Payment" endpoint keyed by the
    /// payment reference (https://mono.co/blog/introducing-mono-apis-v2), and
    /// dashboard refunds (full or partial, ₦15 each), but the API request
    /// shape, partial-refund support and idempotency are undocumented
    /// (research Q7). Until confirmed, every refund (full or partial) goes to
    /// the owner's manual flow (Task 10) and is done on the Mono dashboard.
    /// Wire `POST` to the v2 refund endpoint here once Mono confirms it.
    async fn refund(&self, _req: RefundRequest) -> Result<RefundHandle, ProviderError> {
        Err(ProviderError::NotSupported)
    }

    /// Not supported: no account-name lookup endpoint confirmed for this
    /// integration; payout accounts stay pending for owner review.
    async fn resolve_account(
        &self,
        _bank_code: &str,
        _account_number: &str,
    ) -> Result<String, ProviderError> {
        Err(ProviderError::NotSupported)
    }

    /// Checks `mono-webhook-secret` in constant time before reading the body,
    /// then maps by exact event name (never a prefix). The body is never logged
    /// or kept: Disburse events carry account numbers.
    fn verify_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<WebhookEvent, ProviderError> {
        let given = headers
            .get(WEBHOOK_SECRET_HEADER)
            .map(HeaderValue::as_bytes)
            .unwrap_or_default();
        let expected = self.cfg.webhook_secret.expose().as_bytes();
        if given.is_empty() || !secret_eq(given, expected) {
            return Err(ProviderError::InvalidSignature);
        }
        let malformed = || rejected_locally("malformed_webhook");
        let v: Value = serde_json::from_slice(body).map_err(|_| malformed())?;
        let event = str_at(&v, "event").ok_or_else(malformed)?;
        let data = v.get("data").unwrap_or(&Value::Null);
        let object = data.get("object").unwrap_or(&Value::Null);

        let is_payment = PAYMENT_EVENTS.contains(&event);
        let is_payout = PAYOUT_EVENTS.contains(&event);
        let reference = if is_payment {
            str_at(object, "reference")
        } else if is_payout {
            str_at(data, "reference")
        } else {
            None
        }
        .filter(|r| !r.trim().is_empty());

        let event_id = match id_at(&v, "event_id") {
            Some(id) => id,
            // Unconfirmed whether every event carries `event_id`; a stable
            // synthetic id keeps payment and payout events deduplicated per
            // event name and reference instead of dropping them.
            None => match (is_payment || is_payout, reference) {
                (true, Some(r)) => format!("{event}:{r}"),
                _ => return Err(malformed()),
            },
        };
        let ignored = |why: &'static str| {
            tracing::info!(
                provider = MONO_NAME,
                event = loggable_event(event),
                why,
                "mono webhook ignored"
            );
            Ok(WebhookEvent::Ignored {
                event_id: event_id.clone(),
            })
        };

        let livemode = [object, data, &v]
            .iter()
            .find_map(|o| bool_at(o, &["livemode", "liveMode"]));
        if livemode.is_some_and(|l| l != self.cfg.live) {
            return ignored("livemode_mismatch");
        }
        match (is_payment, is_payout, reference) {
            (true, _, Some(r)) => Ok(WebhookEvent::Payment {
                event_id: event_id.clone(),
                reference: r.to_string(),
            }),
            (_, true, Some(r)) => Ok(WebhookEvent::Payout {
                event_id: event_id.clone(),
                reference: r.to_string(),
            }),
            (true, _, None) | (_, true, None) => ignored("no_reference"),
            _ => ignored("not_handled"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_mono_amount_is_checked() {
        assert_eq!(to_mono_amount(10_000, AmountUnit::Kobo).unwrap(), 10_000);
        assert_eq!(to_mono_amount(10_000, AmountUnit::Naira).unwrap(), 100);
        for (kobo, unit) in [
            (0, AmountUnit::Kobo),
            (-1, AmountUnit::Kobo),
            (10_001, AmountUnit::Naira),
        ] {
            assert!(matches!(
                to_mono_amount(kobo, unit),
                Err(ProviderError::Rejected { .. })
            ));
        }
    }

    #[test]
    fn from_mono_amount_parses_numbers_and_strings_exactly() {
        let k = AmountUnit::Kobo;
        let n = AmountUnit::Naira;
        assert_eq!(from_mono_amount(&json!(20000), k), Some(20000));
        assert_eq!(from_mono_amount(&json!("20000"), k), Some(20000));
        assert_eq!(from_mono_amount(&json!(" 20000 "), k), Some(20000));
        assert_eq!(from_mono_amount(&json!("20000.00"), k), Some(20000));
        assert_eq!(from_mono_amount(&json!(200), n), Some(20000));
        assert_eq!(from_mono_amount(&json!("200.5"), n), Some(20050));
        assert_eq!(from_mono_amount(&json!(200.05), n), Some(20005));
        assert_eq!(from_mono_amount(&json!("200.050"), n), Some(20005));
        for bad in [
            json!("200.5"),
            json!(-1),
            json!("1e3"),
            json!(""),
            json!("."),
            json!(null),
            json!(true),
            json!("99999999999999999999"),
        ] {
            assert_eq!(from_mono_amount(&bad, k), None, "{bad}");
        }
        assert_eq!(from_mono_amount(&json!("1.005"), n), None);
        assert_eq!(from_mono_amount(&json!(i64::MAX), n), None);
    }

    #[test]
    fn safe_message_withholds_numbers_and_long_text() {
        let m = |s: &str| safe_message(Some(&json!({ "message": s })));
        assert_eq!(m("Invalid bank code"), "Invalid bank code");
        assert_eq!(m("Account 0123456789 invalid"), "[message withheld]");
        assert_eq!(m(&"x".repeat(200)), "[message withheld]");
        assert_eq!(safe_message(None), "");
    }

    #[test]
    fn amount_unit_parses() {
        assert_eq!(AmountUnit::parse(" KOBO "), Some(AmountUnit::Kobo));
        assert_eq!(AmountUnit::parse("naira"), Some(AmountUnit::Naira));
        assert_eq!(AmountUnit::parse("ngn"), None);
    }
}
