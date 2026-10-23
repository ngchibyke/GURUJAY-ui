//! The payment provider layer. Escrow logic talks only to [`PaymentProvider`];
//! concrete providers (Mono in production, [`fake::FakeProvider`] in tests)
//! are looked up through the [`ProviderRegistry`].
//!
//! Data protection: request types carry personal data (buyer name/email,
//! seller bank account numbers). Their `Debug` output redacts it, and
//! [`ProviderError`] never prints its inner detail, so none of it reaches logs.

pub mod fake;

use std::{collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use axum::http::HeaderMap;
use rand::{Rng, rngs::OsRng};
use sha2::{Digest, Sha256};

use crate::error::AppError;

/// Every payment method supported by a provider compiled into this build,
/// whether or not that provider is configured (has keys) at runtime. The
/// owner may enable any of these in settings before keys exist.
///
/// [`ProviderRegistry::register`] refuses a provider whose methods are not
/// listed here, which keeps this list in step with the providers.
pub const ALL_METHODS: &[&str] = &["mono_directpay"];

/// Length of the random part of a [`new_reference`].
const REFERENCE_RANDOM_LEN: usize = 20;
/// Mono caps references at 24 characters.
const REFERENCE_MAX_LEN: usize = 24;
const REFERENCE_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// A fresh provider reference: `prefix` followed by 20 characters drawn
/// uniformly from `A-Z0-9` with the OS RNG. Panics if the result would be
/// longer than 24 characters (a programming error: prefixes are constants).
///
/// Generate a reference once, persist it on its row before the first provider
/// call, and reuse it on every retry: it is the provider-side idempotency key.
/// Never call this inside a retry loop.
pub fn new_reference(prefix: &str) -> String {
    assert!(
        prefix.is_ascii() && prefix.len() + REFERENCE_RANDOM_LEN <= REFERENCE_MAX_LEN,
        "reference prefix too long"
    );
    let mut rng = OsRng;
    let mut out = String::with_capacity(prefix.len() + REFERENCE_RANDOM_LEN);
    out.push_str(prefix);
    for _ in 0..REFERENCE_RANDOM_LEN {
        out.push(char::from(
            REFERENCE_ALPHABET[rng.gen_range(0..REFERENCE_ALPHABET.len())],
        ));
    }
    out
}

/// Whether `url` may be given to a buyer as a payment page: `https`, no
/// userinfo, no explicit port, and a host equal to one of `hosts` or a
/// subdomain of one (case-insensitive). Guards against an open redirect from a
/// compromised or misconfigured provider response (CWE-601).
pub fn payment_url_allowed(url: &str, hosts: &[&str]) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains(['@', ':']) {
        return false;
    }
    let host = authority.trim_end_matches('.').to_ascii_lowercase();
    hosts.iter().any(|allowed| {
        let allowed = allowed.to_ascii_lowercase();
        host == allowed
            || host
                .strip_suffix(allowed.as_str())
                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
    })
}

/// Constant-time equality for shared secrets. Both sides are hashed first so
/// neither the content nor the length of the expected secret leaks through timing.
pub fn secret_eq(given: &[u8], expected: &[u8]) -> bool {
    let a = Sha256::digest(given);
    let b = Sha256::digest(expected);
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// A payment provider's failure. `Display`/`Debug` print only the kind, never
/// the inner detail, which may echo provider responses.
#[non_exhaustive]
pub enum ProviderError {
    /// The provider does not offer this operation (e.g. refunds via API).
    NotSupported,
    /// The provider received and refused the request. `code` is the provider's
    /// own error code (kept for support); neither field may carry secrets or
    /// account numbers.
    Rejected { code: String, message: String },
    /// The call definitely did not take effect (connect failure, 5xx before sending).
    Unavailable(anyhow::Error),
    /// The outcome is unknown: the request was sent (or may have been) and may
    /// have taken effect. This includes any failure after sending, such as an
    /// unparseable 200 body. Never re-create the operation and never fail over
    /// to another provider: look it up with `find_payout` / `verify_payment`
    /// and reuse the persisted reference.
    Timeout,
    /// A webhook's signature/secret did not match.
    InvalidSignature,
}

impl ProviderError {
    fn kind(&self) -> &'static str {
        match self {
            ProviderError::NotSupported => "not supported",
            ProviderError::Rejected { .. } => "rejected",
            ProviderError::Unavailable(_) => "unavailable",
            ProviderError::Timeout => "timed out",
            ProviderError::InvalidSignature => "invalid webhook signature",
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "payment provider: {}", self.kind())
    }
}

impl fmt::Debug for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::NotSupported => f.write_str("NotSupported"),
            ProviderError::Rejected { .. } => f.write_str("Rejected { .. }"),
            ProviderError::Unavailable(_) => f.write_str("Unavailable(..)"),
            ProviderError::Timeout => f.write_str("Timeout"),
            ProviderError::InvalidSignature => f.write_str("InvalidSignature"),
        }
    }
}

impl std::error::Error for ProviderError {}

impl From<ProviderError> for AppError {
    fn from(e: ProviderError) -> Self {
        tracing::warn!(kind = e.kind(), "payment provider error");
        match e {
            ProviderError::NotSupported => AppError::NotSupported,
            ProviderError::InvalidSignature => AppError::Unauthorized,
            ProviderError::Rejected { .. }
            | ProviderError::Unavailable(_)
            | ProviderError::Timeout => AppError::ProviderUnavailable,
        }
    }
}

#[derive(Clone)]
pub struct InitiatePayment {
    pub reference: String,
    pub amount_kobo: i64,
    pub description: String,
    pub customer_name: String,
    pub customer_email: Option<String>,
    pub redirect_url: String,
}

impl fmt::Debug for InitiatePayment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitiatePayment")
            .field("reference", &self.reference)
            .field("amount_kobo", &self.amount_kobo)
            .field("customer", &"[redacted]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitiatedPayment {
    pub provider_payment_id: String,
    pub payment_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PaymentStatus {
    Pending,
    Successful {
        amount_kobo: i64,
        fee_kobo: i64,
        currency: String,
        /// The provider flagged the payment or is holding its settlement.
        held: bool,
    },
    Failed,
}

#[derive(Clone)]
pub struct PayoutRequest {
    /// Our reference (≤ 24 chars, see [`new_reference`]); payouts are keyed by it.
    pub reference: String,
    pub amount_kobo: i64,
    pub bank_code: String,
    pub account_number: String,
    pub narration: String,
}

impl fmt::Debug for PayoutRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayoutRequest")
            .field("reference", &self.reference)
            .field("amount_kobo", &self.amount_kobo)
            .field("bank_code", &self.bank_code)
            .field("account_number", &"[redacted]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutHandle {
    pub provider_payout_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PayoutState {
    Pending,
    Successful {
        fee_kobo: i64,
    },
    Failed {
        reason: String,
    },
    /// The payout succeeded and was later reversed by the provider or bank.
    Reversed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundRequest {
    pub payment_reference: String,
    pub amount_kobo: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundHandle {
    pub provider_refund_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WebhookEvent {
    Payment {
        event_id: String,
        reference: String,
    },
    /// Matched by our payout reference: Mono's disburse webhooks carry no payout id.
    Payout {
        event_id: String,
        reference: String,
    },
    Ignored {
        event_id: String,
    },
}

#[async_trait]
pub trait PaymentProvider: Send + Sync {
    /// Stable provider name, e.g. "mono", "fake".
    fn name(&self) -> &'static str;
    /// Payment methods this provider collects with, e.g. `["mono_directpay"]`.
    /// Each must be listed in [`ALL_METHODS`].
    fn methods(&self) -> &'static [&'static str];
    /// Hosts the provider's hosted payment pages live on. A `payment_url` from
    /// [`initiate`](Self::initiate) is handed to the buyer only when
    /// [`payment_url_allowed`] accepts it against this list (exact host or a
    /// subdomain of one); anything else counts as an initiate failure.
    fn payment_url_hosts(&self) -> &'static [&'static str];
    async fn initiate(&self, req: InitiatePayment) -> Result<InitiatedPayment, ProviderError>;
    async fn verify_payment(&self, reference: &str) -> Result<PaymentStatus, ProviderError>;
    async fn create_payout(&self, req: PayoutRequest) -> Result<PayoutHandle, ProviderError>;
    async fn payout_status(
        &self,
        reference: &str,
        provider_payout_id: Option<&str>,
    ) -> Result<PayoutState, ProviderError>;
    /// Looks up a payout by our reference, e.g. after `create_payout` timed out.
    async fn find_payout(&self, reference: &str) -> Result<Option<PayoutHandle>, ProviderError>;
    /// Completes a payout that needs an OTP (Paystack's transfer flow).
    /// Providers without that step keep this default.
    async fn finalize_payout(&self, _reference: &str, _otp: &str) -> Result<(), ProviderError> {
        Err(ProviderError::NotSupported)
    }
    async fn refund(&self, req: RefundRequest) -> Result<RefundHandle, ProviderError>;
    /// Returns the account holder's name.
    async fn resolve_account(
        &self,
        bank_code: &str,
        account_number: &str,
    ) -> Result<String, ProviderError>;
    fn verify_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<WebhookEvent, ProviderError>;
}

/// The providers configured at runtime, by name and by payment method.
///
/// Use [`for_method`](Self::for_method) only to start a new payment. Persist
/// `provider.name()` on the checkout/order/payout and use
/// [`by_name`](Self::by_name) for every later verify, payout, refund or
/// webhook call, so an order never switches provider midway.
#[derive(Default)]
pub struct ProviderRegistry {
    by_name: HashMap<&'static str, Arc<dyn PaymentProvider>>,
    by_method: HashMap<&'static str, Arc<dyn PaymentProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a provider. Panics (startup programming error) on a duplicate
    /// name, a method already served by another provider, or a method missing
    /// from [`ALL_METHODS`].
    pub fn register(&mut self, provider: Arc<dyn PaymentProvider>) {
        let name = provider.name();
        assert!(
            !self.by_name.contains_key(name),
            "provider {name} already registered"
        );
        for &method in provider.methods() {
            assert!(
                ALL_METHODS.contains(&method),
                "method {method} missing from ALL_METHODS"
            );
            assert!(
                !self.by_method.contains_key(method),
                "method {method} already registered"
            );
            self.by_method.insert(method, provider.clone());
        }
        self.by_name.insert(name, provider);
    }

    pub fn for_method(&self, method: &str) -> Option<Arc<dyn PaymentProvider>> {
        self.by_method.get(method).cloned()
    }

    pub fn by_name(&self, name: &str) -> Option<Arc<dyn PaymentProvider>> {
        self.by_name.get(name).cloned()
    }

    /// Whether a provider for `method` is registered (configured) right now.
    pub fn is_registered_method(&self, method: &str) -> bool {
        self.by_method.contains_key(method)
    }

    /// Whether `method` belongs to a provider compiled into this build,
    /// configured or not. Settings validation uses this.
    pub fn is_supported_method(method: &str) -> bool {
        ALL_METHODS.contains(&method)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payments::fake::FakeProvider;
    use std::collections::HashSet;

    #[test]
    fn registry_looks_up_by_method_and_name() {
        let mut reg = ProviderRegistry::new();
        reg.register(Arc::new(FakeProvider::new()));
        assert_eq!(reg.for_method("mono_directpay").unwrap().name(), "fake");
        assert_eq!(reg.by_name("fake").unwrap().name(), "fake");
        // Records persist provider.name(); later calls go through by_name and
        // must reach the very same provider instance.
        assert!(Arc::ptr_eq(
            &reg.by_name("fake").unwrap(),
            &reg.for_method("mono_directpay").unwrap()
        ));
        assert!(reg.is_registered_method("mono_directpay"));
    }

    #[test]
    fn unknown_method_or_name_is_none() {
        let mut reg = ProviderRegistry::new();
        assert!(reg.for_method("mono_directpay").is_none());
        reg.register(Arc::new(FakeProvider::new()));
        assert!(reg.for_method("paystack_card").is_none());
        assert!(reg.by_name("mono").is_none());
        assert!(!reg.is_registered_method("paystack_card"));
    }

    #[test]
    fn supported_methods_do_not_need_a_registered_provider() {
        assert!(ProviderRegistry::is_supported_method("mono_directpay"));
        assert!(!ProviderRegistry::is_supported_method("paystack_card"));
        assert!(!ProviderRegistry::new().is_registered_method("mono_directpay"));
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn duplicate_registration_panics() {
        let mut reg = ProviderRegistry::new();
        reg.register(Arc::new(FakeProvider::new()));
        reg.register(Arc::new(FakeProvider::new()));
    }

    #[test]
    fn new_reference_has_prefix_length_and_alphabet() {
        let r = new_reference("PO");
        assert_eq!(r.len(), 22);
        assert!(r.starts_with("PO"));
        assert!(
            r[2..]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        );
        assert_eq!(new_reference("CHK4").len(), 24);
        assert_eq!(new_reference("").len(), 20);
    }

    #[test]
    fn new_reference_is_unique() {
        let refs: HashSet<String> = (0..1000).map(|_| new_reference("PO")).collect();
        assert_eq!(refs.len(), 1000);
    }

    #[test]
    #[should_panic]
    fn new_reference_rejects_long_prefix() {
        new_reference("TOOLONG");
    }

    #[test]
    fn provider_error_hides_detail() {
        let e = ProviderError::Unavailable(anyhow::anyhow!("sk_live_secret 0123456789"));
        assert!(!format!("{e} {e:?}").contains("secret"));
        let e = ProviderError::Rejected {
            code: "E1".into(),
            message: "acct 0123456789".into(),
        };
        assert!(!format!("{e} {e:?}").contains("0123456789"));
    }

    #[test]
    fn payout_request_debug_redacts_account_number() {
        let req = PayoutRequest {
            reference: "PO1".into(),
            amount_kobo: 1,
            bank_code: "058".into(),
            account_number: "0123456789".into(),
            narration: "n".into(),
        };
        assert!(!format!("{req:?}").contains("0123456789"));
    }

    #[test]
    fn payment_url_must_be_https_on_an_allowed_host() {
        let hosts = &["pay.fake.test", "withmono.com"];
        for ok in [
            "https://pay.fake.test/CK1",
            "https://PAY.fake.test/CK1?x=1",
            "https://checkout.withmono.com/abc",
            "https://withmono.com",
            "https://pay.fake.test./x",
        ] {
            assert!(payment_url_allowed(ok, hosts), "{ok}");
        }
        for bad in [
            "http://pay.fake.test/CK1",
            "https://pay.fake.test.evil.example/x",
            "https://evilwithmono.com/x",
            "https://user@pay.fake.test/x",
            "https://pay.fake.test@evil.example/x",
            "https://pay.fake.test:8443/x",
            "https:///x",
            "javascript:alert(1)",
            "https://pay.fake.test\\@evil.example/",
            "https://pay.fake.test /x",
            "",
        ] {
            assert!(!payment_url_allowed(bad, hosts), "{bad}");
        }
        assert!(!payment_url_allowed("https://pay.fake.test/x", &[]));
    }

    #[test]
    fn secret_eq_compares() {
        assert!(secret_eq(b"test-secret", b"test-secret"));
        assert!(!secret_eq(b"test-secreT", b"test-secret"));
        assert!(!secret_eq(b"", b"test-secret"));
    }
}
