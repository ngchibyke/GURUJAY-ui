//! A programmable [`PaymentProvider`] test double. **Test and development
//! only:** production (`main.rs`) never registers it. It is compiled into the
//! library (not `cfg(test)`) because the integration tests in `tests/` use it.
//!
//! Defaults: payments and payouts stay `Pending`, `find_payout` finds nothing,
//! `refund` and `resolve_account` are `NotSupported`. Test code overrides each
//! outcome with the `set_*` / `fail_*` methods and reads call counts with
//! [`FakeProvider::calls`].

use std::{collections::HashMap, sync::Mutex};

use async_trait::async_trait;
use axum::http::HeaderMap;
use serde::Deserialize;

use super::{
    InitiatePayment, InitiatedPayment, PaymentProvider, PaymentStatus, PayoutHandle, PayoutRequest,
    PayoutState, ProviderError, RefundHandle, RefundRequest, WebhookEvent, secret_eq,
};

pub const FAKE_NAME: &str = "fake";
pub const FAKE_WEBHOOK_SECRET: &str = "test-secret";
pub const FAKE_SIGNATURE_HEADER: &str = "x-fake-signature";

/// `ProviderError` holds an `anyhow::Error`, which is not `Clone`; a stored
/// outcome is re-created on every call.
fn copy_err(e: &ProviderError) -> ProviderError {
    match e {
        ProviderError::NotSupported => ProviderError::NotSupported,
        ProviderError::Rejected { code, message } => ProviderError::Rejected {
            code: code.clone(),
            message: message.clone(),
        },
        ProviderError::Unavailable(_) => {
            ProviderError::Unavailable(anyhow::anyhow!("fake provider unavailable"))
        }
        ProviderError::Timeout => ProviderError::Timeout,
        ProviderError::InvalidSignature => ProviderError::InvalidSignature,
    }
}

fn copy_result<T: Clone>(r: &Result<T, ProviderError>) -> Result<T, ProviderError> {
    match r {
        Ok(v) => Ok(v.clone()),
        Err(e) => Err(copy_err(e)),
    }
}

#[derive(Default)]
pub struct FakeProvider {
    payments: Mutex<HashMap<String, PaymentStatus>>,
    initiate_fails: Mutex<bool>,
    initiate_timeout_after_success: Mutex<bool>,
    payment_url: Mutex<Option<String>>,
    payout_timeout_after_success: Mutex<bool>,
    verify_unavailable: Mutex<usize>,
    payout_failure: Mutex<Option<ProviderError>>,
    payout_states: Mutex<HashMap<String, PayoutState>>,
    found_payouts: Mutex<HashMap<String, Option<String>>>,
    refund_supported: Mutex<bool>,
    lookup: Mutex<Option<Result<String, ProviderError>>>,
    calls: Mutex<HashMap<&'static str, usize>>,
    initiated: Mutex<Vec<InitiatePayment>>,
    payouts: Mutex<Vec<PayoutRequest>>,
    refunds: Mutex<Vec<RefundRequest>>,
}

impl FakeProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, name: &'static str) {
        *self.calls.lock().unwrap().entry(name).or_default() += 1;
    }

    /// How many times the trait method `name` (e.g. "create_payout") was called.
    pub fn calls(&self, name: &str) -> usize {
        self.calls.lock().unwrap().get(name).copied().unwrap_or(0)
    }

    /// The status `verify_payment(reference)` returns from now on.
    pub fn set_payment(&self, reference: &str, status: PaymentStatus) {
        self.payments
            .lock()
            .unwrap()
            .insert(reference.to_string(), status);
    }

    /// Every later `initiate` call returns `Unavailable`.
    pub fn fail_initiate(&self) {
        *self.initiate_fails.lock().unwrap() = true;
    }

    /// The next `initiate` call takes effect (is recorded) but returns `Timeout`.
    pub fn timeout_after_success_initiate(&self) {
        *self.initiate_timeout_after_success.lock().unwrap() = true;
    }

    /// The `payment_url` that `initiate` returns from now on (`None`: the default
    /// `https://pay.fake.test/<reference>`). Lets tests return an untrusted URL.
    pub fn set_payment_url(&self, url: Option<&str>) {
        *self.payment_url.lock().unwrap() = url.map(str::to_string);
    }

    /// The next `create_payout` call takes effect but returns `Timeout`: the
    /// payout is recorded and a later `find_payout(reference)` finds it.
    pub fn timeout_after_success_payout(&self) {
        *self.payout_timeout_after_success.lock().unwrap() = true;
    }

    /// The next `n` `verify_payment` calls return `Unavailable`.
    pub fn verify_unavailable_n(&self, n: usize) {
        *self.verify_unavailable.lock().unwrap() = n;
    }

    /// Every later `create_payout` call returns this error, until
    /// [`clear_payout_failure`](Self::clear_payout_failure).
    pub fn fail_payout_with(&self, err: ProviderError) {
        *self.payout_failure.lock().unwrap() = Some(err);
    }

    pub fn clear_payout_failure(&self) {
        *self.payout_failure.lock().unwrap() = None;
    }

    /// The state `payout_status(reference, _)` returns from now on.
    pub fn set_payout_state(&self, reference: &str, state: PayoutState) {
        self.payout_states
            .lock()
            .unwrap()
            .insert(reference.to_string(), state);
    }

    /// What `find_payout(reference)` returns: `Some(id)` → found with that id.
    pub fn set_found_payout(&self, reference: &str, provider_payout_id: Option<&str>) {
        self.found_payouts.lock().unwrap().insert(
            reference.to_string(),
            provider_payout_id.map(str::to_string),
        );
    }

    /// When true, `refund` succeeds with id `fakerf_<payment_reference>`.
    pub fn set_refund_supported(&self, supported: bool) {
        *self.refund_supported.lock().unwrap() = supported;
    }

    /// What `resolve_account` returns from now on.
    pub fn set_lookup(&self, result: Result<String, ProviderError>) {
        *self.lookup.lock().unwrap() = Some(result);
    }

    /// Requests passed to `initiate`, in order.
    pub fn initiated(&self) -> Vec<InitiatePayment> {
        self.initiated.lock().unwrap().clone()
    }

    /// Requests passed to `create_payout`, in order (including failed ones).
    pub fn payouts(&self) -> Vec<PayoutRequest> {
        self.payouts.lock().unwrap().clone()
    }

    /// Requests passed to `refund`, in order.
    pub fn refunds(&self) -> Vec<RefundRequest> {
        self.refunds.lock().unwrap().clone()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum FakeKind {
    Payment,
    Payout,
    Other,
}

#[derive(Deserialize)]
struct FakeWebhook {
    event_id: String,
    kind: FakeKind,
    reference: Option<String>,
}

#[async_trait]
impl PaymentProvider for FakeProvider {
    fn name(&self) -> &'static str {
        FAKE_NAME
    }

    fn methods(&self) -> &'static [&'static str] {
        &["mono_directpay"]
    }

    fn payment_url_hosts(&self) -> &'static [&'static str] {
        &["pay.fake.test"]
    }

    async fn initiate(&self, req: InitiatePayment) -> Result<InitiatedPayment, ProviderError> {
        self.record("initiate");
        let reference = req.reference.clone();
        self.initiated.lock().unwrap().push(req);
        if *self.initiate_fails.lock().unwrap() {
            return Err(ProviderError::Unavailable(anyhow::anyhow!(
                "fake initiate failure"
            )));
        }
        if std::mem::take(&mut *self.initiate_timeout_after_success.lock().unwrap()) {
            return Err(ProviderError::Timeout);
        }
        Ok(InitiatedPayment {
            provider_payment_id: format!("fake_{reference}"),
            payment_url: self
                .payment_url
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| format!("https://pay.fake.test/{reference}")),
        })
    }

    async fn verify_payment(&self, reference: &str) -> Result<PaymentStatus, ProviderError> {
        self.record("verify_payment");
        {
            let mut left = self.verify_unavailable.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(ProviderError::Unavailable(anyhow::anyhow!(
                    "fake verify failure"
                )));
            }
        }
        Ok(self
            .payments
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .unwrap_or(PaymentStatus::Pending))
    }

    async fn create_payout(&self, req: PayoutRequest) -> Result<PayoutHandle, ProviderError> {
        self.record("create_payout");
        let reference = req.reference.clone();
        self.payouts.lock().unwrap().push(req);
        if let Some(e) = self.payout_failure.lock().unwrap().as_ref() {
            return Err(copy_err(e));
        }
        if std::mem::take(&mut *self.payout_timeout_after_success.lock().unwrap()) {
            self.set_found_payout(&reference, Some(&format!("fakepo_{reference}")));
            return Err(ProviderError::Timeout);
        }
        Ok(PayoutHandle {
            provider_payout_id: format!("fakepo_{reference}"),
        })
    }

    async fn payout_status(
        &self,
        reference: &str,
        _provider_payout_id: Option<&str>,
    ) -> Result<PayoutState, ProviderError> {
        self.record("payout_status");
        Ok(self
            .payout_states
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .unwrap_or(PayoutState::Pending))
    }

    async fn find_payout(&self, reference: &str) -> Result<Option<PayoutHandle>, ProviderError> {
        self.record("find_payout");
        Ok(self
            .found_payouts
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .flatten()
            .map(|provider_payout_id| PayoutHandle { provider_payout_id }))
    }

    async fn refund(&self, req: RefundRequest) -> Result<RefundHandle, ProviderError> {
        self.record("refund");
        let reference = req.payment_reference.clone();
        self.refunds.lock().unwrap().push(req);
        if !*self.refund_supported.lock().unwrap() {
            return Err(ProviderError::NotSupported);
        }
        Ok(RefundHandle {
            provider_refund_id: format!("fakerf_{reference}"),
        })
    }

    async fn resolve_account(
        &self,
        _bank_code: &str,
        _account_number: &str,
    ) -> Result<String, ProviderError> {
        self.record("resolve_account");
        match self.lookup.lock().unwrap().as_ref() {
            Some(r) => copy_result(r),
            None => Err(ProviderError::NotSupported),
        }
    }

    /// Checks the `x-fake-signature` header against "test-secret" in constant
    /// time, then parses `{"event_id","kind":"payment"|"payout"|"other","reference"}`.
    fn verify_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<WebhookEvent, ProviderError> {
        self.record("verify_webhook");
        let given = headers
            .get(FAKE_SIGNATURE_HEADER)
            .map(|v| v.as_bytes())
            .unwrap_or_default();
        if !secret_eq(given, FAKE_WEBHOOK_SECRET.as_bytes()) {
            return Err(ProviderError::InvalidSignature);
        }
        let malformed = || ProviderError::Rejected {
            code: "malformed_webhook".into(),
            message: "webhook body could not be parsed".into(),
        };
        let hook: FakeWebhook = serde_json::from_slice(body).map_err(|_| malformed())?;
        Ok(match hook.kind {
            FakeKind::Payment => WebhookEvent::Payment {
                event_id: hook.event_id,
                reference: hook.reference.ok_or_else(malformed)?,
            },
            FakeKind::Payout => WebhookEvent::Payout {
                event_id: hook.event_id,
                reference: hook.reference.ok_or_else(malformed)?,
            },
            FakeKind::Other => WebhookEvent::Ignored {
                event_id: hook.event_id,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    fn signed(secret: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            FAKE_SIGNATURE_HEADER,
            HeaderValue::from_str(secret).unwrap(),
        );
        h
    }

    fn body(v: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn webhook_with_right_secret_parses_events() {
        let fake = FakeProvider::new();
        let h = signed(FAKE_WEBHOOK_SECRET);
        let ev = fake
            .verify_webhook(
                &h,
                &body(json!({"event_id":"e1","kind":"payment","reference":"CHK1"})),
            )
            .unwrap();
        assert_eq!(
            ev,
            WebhookEvent::Payment {
                event_id: "e1".into(),
                reference: "CHK1".into()
            }
        );
        let ev = fake
            .verify_webhook(
                &h,
                &body(json!({"event_id":"e2","kind":"payout","reference":"PO1"})),
            )
            .unwrap();
        assert_eq!(
            ev,
            WebhookEvent::Payout {
                event_id: "e2".into(),
                reference: "PO1".into()
            }
        );
        let ev = fake
            .verify_webhook(&h, &body(json!({"event_id":"e3","kind":"other"})))
            .unwrap();
        assert_eq!(
            ev,
            WebhookEvent::Ignored {
                event_id: "e3".into()
            }
        );
        assert_eq!(fake.calls("verify_webhook"), 3);
    }

    #[test]
    fn webhook_with_wrong_or_missing_secret_is_rejected() {
        let fake = FakeProvider::new();
        let b = body(json!({"event_id":"e1","kind":"payment","reference":"CHK1"}));
        assert!(matches!(
            fake.verify_webhook(&signed("wrong-secret"), &b),
            Err(ProviderError::InvalidSignature)
        ));
        assert!(matches!(
            fake.verify_webhook(&HeaderMap::new(), &b),
            Err(ProviderError::InvalidSignature)
        ));
    }

    #[test]
    fn malformed_webhook_body_is_rejected() {
        let fake = FakeProvider::new();
        let h = signed(FAKE_WEBHOOK_SECRET);
        assert!(matches!(
            fake.verify_webhook(&h, b"not json"),
            Err(ProviderError::Rejected { .. })
        ));
        assert!(matches!(
            fake.verify_webhook(&h, &body(json!({"event_id":"e1","kind":"payment"}))),
            Err(ProviderError::Rejected { .. })
        ));
    }

    #[tokio::test]
    async fn defaults_and_overrides() {
        let fake = FakeProvider::new();
        assert_eq!(
            fake.verify_payment("R").await.unwrap(),
            PaymentStatus::Pending
        );
        fake.verify_unavailable_n(1);
        assert!(matches!(
            fake.verify_payment("R").await,
            Err(ProviderError::Unavailable(_))
        ));
        let paid = PaymentStatus::Successful {
            amount_kobo: 100,
            fee_kobo: 1,
            currency: "NGN".into(),
            held: false,
        };
        fake.set_payment("R", paid.clone());
        assert_eq!(fake.verify_payment("R").await.unwrap(), paid);
        assert_eq!(fake.calls("verify_payment"), 3);

        let req = PayoutRequest {
            reference: "PO1".into(),
            amount_kobo: 5,
            bank_code: "058".into(),
            account_number: "0123456789".into(),
            narration: "n".into(),
        };
        assert_eq!(
            fake.create_payout(req.clone())
                .await
                .unwrap()
                .provider_payout_id,
            "fakepo_PO1"
        );
        fake.fail_payout_with(ProviderError::Timeout);
        assert!(matches!(
            fake.create_payout(req.clone()).await,
            Err(ProviderError::Timeout)
        ));
        assert_eq!(fake.payouts().len(), 2);
        fake.clear_payout_failure();
        fake.timeout_after_success_payout();
        let mut req2 = req.clone();
        req2.reference = "PO2".into();
        assert!(matches!(
            fake.create_payout(req2.clone()).await,
            Err(ProviderError::Timeout)
        ));
        assert_eq!(
            fake.find_payout("PO2")
                .await
                .unwrap()
                .unwrap()
                .provider_payout_id,
            "fakepo_PO2"
        );
        // One-shot: the next call succeeds.
        assert!(fake.create_payout(req2).await.is_ok());
        assert_eq!(fake.calls("create_payout"), 4);
        assert_eq!(fake.find_payout("PO1").await.unwrap(), None);
        fake.set_found_payout("PO1", Some("x9"));
        assert_eq!(
            fake.find_payout("PO1")
                .await
                .unwrap()
                .unwrap()
                .provider_payout_id,
            "x9"
        );
        assert_eq!(
            fake.payout_status("PO1", None).await.unwrap(),
            PayoutState::Pending
        );

        let refund = RefundRequest {
            payment_reference: "R".into(),
            amount_kobo: 100,
            reason: "r".into(),
        };
        assert!(matches!(
            fake.refund(refund.clone()).await,
            Err(ProviderError::NotSupported)
        ));
        fake.set_refund_supported(true);
        assert_eq!(
            fake.refund(refund).await.unwrap().provider_refund_id,
            "fakerf_R"
        );
        assert!(matches!(
            fake.resolve_account("058", "0123456789").await,
            Err(ProviderError::NotSupported)
        ));
        fake.set_lookup(Ok("ADA OBI".into()));
        assert_eq!(
            fake.resolve_account("058", "0123456789").await.unwrap(),
            "ADA OBI"
        );

        let init = fake
            .initiate(InitiatePayment {
                reference: "CHK1".into(),
                amount_kobo: 100,
                description: "d".into(),
                customer_name: "n".into(),
                customer_email: None,
                redirect_url: "https://x.test".into(),
            })
            .await
            .unwrap();
        assert_eq!(init.provider_payment_id, "fake_CHK1");
        assert_eq!(init.payment_url, "https://pay.fake.test/CHK1");
        fake.timeout_after_success_initiate();
        assert!(matches!(
            fake.initiate(fake.initiated()[0].clone()).await,
            Err(ProviderError::Timeout)
        ));
        assert_eq!(fake.initiated().len(), 2);
        fake.fail_initiate();
        assert!(matches!(
            fake.initiate(fake.initiated()[0].clone()).await,
            Err(ProviderError::Unavailable(_))
        ));
        assert_eq!(fake.calls("initiate"), 3);
    }
}
