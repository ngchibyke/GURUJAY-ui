//! MonoProvider against a local mock server (wiremock). No real network.
//!
//! Fixtures follow the sample payloads recorded in the Task 11 research
//! (docs.mono.co, checked 2026-10-25). Shapes Mono does not document are
//! marked "unconfirmed" in `src/payments/mono.rs` and in the fixtures here.

use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use server::payments::{
    InitiatePayment, PaymentProvider, PaymentStatus, PayoutHandle, PayoutRequest, PayoutState,
    ProviderError, RefundRequest, WebhookEvent,
    mono::{AmountUnit, MonoConfig, MonoProvider, Secret},
    payment_url_allowed,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, method, path, query_param},
};

const SECRET_KEY: &str = "test_sk_9f8e7d6c5b4a3210";
const WEBHOOK_SECRET: &str = "whsec_mono_test_51f2";
const SOURCE_ACCOUNT: &str = "688a0b672f88771f77d05cf5";
const RECIPIENT_EMAIL: &str = "payouts@platform.test";
const ACCOUNT_NUMBER: &str = "0987654321";

fn config(base_url: &str) -> MonoConfig {
    MonoConfig {
        secret_key: Secret::new(SECRET_KEY),
        webhook_secret: Secret::new(WEBHOOK_SECRET),
        base_url: base_url.to_string(),
        live: false,
        directpay_unit: AmountUnit::Kobo,
        disburse_unit: Some(AmountUnit::Kobo),
        disburse_source_account: Some(SOURCE_ACCOUNT.into()),
        payout_recipient_email: Some(RECIPIENT_EMAIL.into()),
        connect_timeout: Duration::from_secs(5),
        timeout: Duration::from_secs(15),
        insecure_http_for_tests: true,
    }
}

fn provider(server: &MockServer) -> MonoProvider {
    MonoProvider::new(config(&server.uri())).expect("build provider")
}

fn initiate_req() -> InitiatePayment {
    InitiatePayment {
        reference: "CHK1ABCDEFGHIJKLMNOPQRST".into(),
        amount_kobo: 1_050_000,
        description: "Marketplace order CHK1ABCDEFGHIJKLMNOPQRST".into(),
        customer_name: "Ada Obi".into(),
        customer_email: Some("ada@example.test".into()),
        redirect_url: "https://shop.example/checkout/return".into(),
    }
}

fn payout_req() -> PayoutRequest {
    PayoutRequest {
        reference: "PO12345678901234567890".into(),
        amount_kobo: 945_000,
        bank_code: "058".into(),
        account_number: ACCOUNT_NUMBER.into(),
        narration: "Marketplace payout".into(),
    }
}

fn initiate_response() -> Value {
    // Recorded sample (initiate API reference), trimmed.
    json!({
        "status": "successful",
        "message": "Payment Initiated",
        "timestamp": "2026-10-25T10:00:00.000Z",
        "data": {
            "id": "txreq_9c3a",
            "mono_url": "https://checkout.mono.co/txreq_9c3a",
            "type": "onetime-debit",
            "method": "account",
            "amount": 1050000,
            "description": "Marketplace order CHK1ABCDEFGHIJKLMNOPQRST",
            "reference": "CHK1ABCDEFGHIJKLMNOPQRST",
            "liveMode": false,
            "created_at": "2026-10-25T10:00:00.000Z"
        }
    })
}

fn verify_body(status: &str, extra: Value) -> Value {
    // Recorded verify sample: amount and fee as strings, `liveMode` casing,
    // and the payer's account object (never logged or stored by us).
    let mut data = json!({
        "id": "txd_77",
        "channel": "account",
        "fee": "15000",
        "type": "onetime-debit",
        "status": status,
        "amount": "1050000",
        "currency": "NGN",
        "description": "Marketplace order",
        "reference": "CHK1ABCDEFGHIJKLMNOPQRST",
        "liveMode": false,
        "account": {
            "id": "acc_1",
            "name": "ADA OBI",
            "account_number": ACCOUNT_NUMBER,
            "bank": { "name": "GTBank", "code": "058" }
        },
        "created_at": "2026-10-25T10:00:00.000Z"
    });
    if let (Some(d), Some(e)) = (data.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            d.insert(k.clone(), v.clone());
        }
    }
    json!({ "status": "successful", "message": "Payment retrieved", "data": data })
}

async fn mount_verify(server: &MockServer, status: u16, body: Value) {
    Mock::given(method("GET"))
        .and(path("/v2/payments/verify/CHK1ABCDEFGHIJKLMNOPQRST"))
        .and(header("mono-sec-key", SECRET_KEY))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------- initiate

#[tokio::test]
async fn initiate_sends_expected_body_and_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/payments/initiate"))
        .and(header("mono-sec-key", SECRET_KEY))
        .and(body_json(json!({
            "amount": 1050000,
            "type": "onetime-debit",
            "method": "account",
            "description": "Marketplace order CHK1ABCDEFGHIJKLMNOPQRST",
            "reference": "CHK1ABCDEFGHIJKLMNOPQRST",
            "redirect_url": "https://shop.example/checkout/return",
            "customer": { "name": "Ada Obi", "email": "ada@example.test" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(initiate_response()))
        .expect(1)
        .mount(&server)
        .await;
    let mono = provider(&server);
    let out = mono.initiate(initiate_req()).await.unwrap();
    assert_eq!(out.provider_payment_id, "txreq_9c3a");
    assert_eq!(out.payment_url, "https://checkout.mono.co/txreq_9c3a");
    assert!(payment_url_allowed(
        &out.payment_url,
        mono.payment_url_hosts()
    ));
}

#[tokio::test]
async fn initiate_sends_no_email_when_the_buyer_has_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/payments/initiate"))
        .and(body_json(json!({
            "amount": 1050000,
            "type": "onetime-debit",
            "method": "account",
            "description": "Marketplace order CHK1ABCDEFGHIJKLMNOPQRST",
            "reference": "CHK1ABCDEFGHIJKLMNOPQRST",
            "redirect_url": "https://shop.example/checkout/return",
            "customer": { "name": "Ada Obi" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(initiate_response()))
        .expect(1)
        .mount(&server)
        .await;
    let mut req = initiate_req();
    req.customer_email = None;
    provider(&server).initiate(req).await.unwrap();
}

#[tokio::test]
async fn naira_unit_divides_and_refuses_fractional_naira() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/payments/initiate"))
        .and(body_json(json!({
            "amount": 10500,
            "type": "onetime-debit",
            "method": "account",
            "description": "Marketplace order CHK1ABCDEFGHIJKLMNOPQRST",
            "reference": "CHK1ABCDEFGHIJKLMNOPQRST",
            "redirect_url": "https://shop.example/checkout/return",
            "customer": { "name": "Ada Obi", "email": "ada@example.test" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(initiate_response()))
        .expect(1)
        .mount(&server)
        .await;
    let mut cfg = config(&server.uri());
    cfg.directpay_unit = AmountUnit::Naira;
    let mono = MonoProvider::new(cfg).unwrap();
    mono.initiate(initiate_req()).await.unwrap();

    // 1,050,050 kobo is ₦10,500.50: never rounded, never sent.
    let mut req = initiate_req();
    req.amount_kobo = 1_050_050;
    assert!(matches!(
        mono.initiate(req).await,
        Err(ProviderError::Rejected { .. })
    ));
}

#[tokio::test]
async fn initiate_without_mono_url_is_an_unknown_outcome() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/payments/initiate"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"status":"successful","data":{}})),
        )
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server).initiate(initiate_req()).await,
        Err(ProviderError::Timeout)
    ));
}

// ------------------------------------------------------------------ verify

#[tokio::test]
async fn verify_maps_successful_with_amount_and_fee() {
    let server = MockServer::start().await;
    mount_verify(&server, 200, verify_body("successful", json!({}))).await;
    let status = provider(&server)
        .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
        .await
        .unwrap();
    assert_eq!(
        status,
        PaymentStatus::Successful {
            amount_kobo: 1_050_000,
            fee_kobo: 15_000,
            currency: "NGN".into(),
            held: false,
            reference: "CHK1ABCDEFGHIJKLMNOPQRST".into(),
        }
    );
}

#[tokio::test]
async fn verify_accepts_numeric_amounts_and_naira_unit() {
    let server = MockServer::start().await;
    mount_verify(
        &server,
        200,
        verify_body("successful", json!({"amount": 10500, "fee": 150.5})),
    )
    .await;
    let mut cfg = config(&server.uri());
    cfg.directpay_unit = AmountUnit::Naira;
    let status = MonoProvider::new(cfg)
        .unwrap()
        .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
        .await
        .unwrap();
    let PaymentStatus::Successful {
        amount_kobo,
        fee_kobo,
        ..
    } = status
    else {
        panic!("expected successful")
    };
    assert_eq!((amount_kobo, fee_kobo), (1_050_000, 15_050));
}

#[tokio::test]
async fn verify_404_is_pending() {
    let server = MockServer::start().await;
    mount_verify(
        &server,
        404,
        json!({"status":"failed","message":"Payment not found","data":null}),
    )
    .await;
    assert_eq!(
        provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap(),
        PaymentStatus::Pending
    );
}

#[tokio::test]
async fn verify_failed_with_payment_not_found_is_pending() {
    // Recorded sample (one-time integration guide): an unfinished payment.
    let server = MockServer::start().await;
    mount_verify(
        &server,
        200,
        json!({
            "status": "failed",
            "message": "Invalid reference, payment not found",
            "timestamp": "2026-10-25T10:00:00.000Z",
            "data": null
        }),
    )
    .await;
    let mono = provider(&server);
    assert_eq!(
        mono.verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap(),
        PaymentStatus::Pending
    );

    // Same message with a data object that says failed: still not terminal.
    let server = MockServer::start().await;
    let mut body = verify_body("failed", json!({}));
    body["message"] = json!("Payment NOT FOUND for this reference");
    mount_verify(&server, 200, body).await;
    assert_eq!(
        provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap(),
        PaymentStatus::Pending
    );
}

#[tokio::test]
async fn verify_400_not_found_is_pending() {
    let server = MockServer::start().await;
    mount_verify(
        &server,
        400,
        json!({"status": "failed", "message": "Invalid reference, payment not found", "data": null}),
    )
    .await;
    assert_eq!(
        provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap(),
        PaymentStatus::Pending
    );
}

#[tokio::test]
async fn verify_livemode_mismatch_is_pending() {
    // A sandbox payment seen by a live provider, and the reverse.
    let server = MockServer::start().await;
    mount_verify(
        &server,
        200,
        verify_body("successful", json!({"liveMode": false})),
    )
    .await;
    let mut live = config(&server.uri());
    live.live = true;
    assert_eq!(
        MonoProvider::new(live)
            .unwrap()
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap(),
        PaymentStatus::Pending
    );
    let server = MockServer::start().await;
    let mut body = verify_body("successful", json!({}));
    let data = body["data"].as_object_mut().unwrap();
    data.remove("liveMode");
    data.insert("livemode".into(), json!(true));
    mount_verify(&server, 200, body).await;
    assert_eq!(
        provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap(),
        PaymentStatus::Pending
    );
}

#[tokio::test]
async fn verify_explicit_failed_is_failed() {
    let server = MockServer::start().await;
    let mut body = verify_body("failed", json!({}));
    body["message"] = json!("Payment retrieved");
    mount_verify(&server, 200, body).await;
    assert_eq!(
        provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap(),
        PaymentStatus::Failed
    );
}

#[tokio::test]
async fn verify_flagged_or_held_settlement_is_held() {
    for extra in [
        json!({"flagged": true, "flag_reasons": ["velocity"]}),
        json!({"held_settlement": true}),
    ] {
        let server = MockServer::start().await;
        mount_verify(&server, 200, verify_body("successful", extra.clone())).await;
        let status = provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap();
        assert!(
            matches!(status, PaymentStatus::Successful { held: true, .. }),
            "{extra}"
        );
    }
    for st in ["flagged", "held_settlement"] {
        let server = MockServer::start().await;
        mount_verify(&server, 200, verify_body(st, json!({}))).await;
        let status = provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap();
        assert!(
            matches!(status, PaymentStatus::Successful { held: true, .. }),
            "{st}"
        );
    }
}

#[tokio::test]
async fn verify_unknown_or_processing_status_is_pending() {
    for st in ["processing", "pending", "something_new", ""] {
        let server = MockServer::start().await;
        mount_verify(&server, 200, verify_body(st, json!({}))).await;
        assert_eq!(
            provider(&server)
                .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
                .await
                .unwrap(),
            PaymentStatus::Pending,
            "{st}"
        );
    }
}

#[tokio::test]
async fn verify_unparseable_success_is_an_unknown_outcome() {
    for body in [
        verify_body("successful", json!({"amount": "ten naira"})),
        verify_body("successful", json!({"amount": -5})),
        verify_body("successful", json!({"fee": "1.5e3"})),
    ] {
        let server = MockServer::start().await;
        mount_verify(&server, 200, body).await;
        assert!(matches!(
            provider(&server)
                .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
                .await,
            Err(ProviderError::Timeout)
        ));
    }
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>oops</html>"))
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await,
        Err(ProviderError::Timeout)
    ));
}

#[tokio::test]
async fn http_errors_map_to_provider_errors() {
    type Check = fn(&ProviderError) -> bool;
    let cases: [(u16, Check); 4] = [
        (400, |e| matches!(e, ProviderError::Rejected { .. })),
        (401, |e| matches!(e, ProviderError::Rejected { .. })),
        (429, |e| matches!(e, ProviderError::Unavailable(_))),
        (503, |e| matches!(e, ProviderError::Unavailable(_))),
    ];
    for (code, check) in cases {
        let server = MockServer::start().await;
        mount_verify(
            &server,
            code,
            json!({"status":"failed","message":"Bad request"}),
        )
        .await;
        let err = provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .unwrap_err();
        assert!(check(&err), "{code}: {err:?}");
    }
}

#[tokio::test]
async fn rejected_message_never_carries_account_numbers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/payments/disburse/disbursements"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "status": "failed",
            "message": "Account 0987654321 could not be credited"
        })))
        .mount(&server)
        .await;
    let Err(ProviderError::Rejected { code, message }) =
        provider(&server).create_payout(payout_req()).await
    else {
        panic!("expected rejected")
    };
    assert_eq!(code, "http_400");
    assert!(!message.contains(ACCOUNT_NUMBER), "{message}");
}

#[tokio::test]
async fn connect_failure_is_unavailable() {
    // A port nothing listens on: the request is never sent.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let mono = MonoProvider::new(config(&base)).unwrap();
    assert!(matches!(
        mono.verify_payment("CHK1ABCDEFGHIJKLMNOPQRST").await,
        Err(ProviderError::Unavailable(_))
    ));
    assert!(matches!(
        mono.create_payout(payout_req()).await,
        Err(ProviderError::Unavailable(_))
    ));
}

#[tokio::test]
async fn client_refuses_plain_http_without_the_test_toggle() {
    let server = MockServer::start().await;
    let mut cfg = config(&server.uri());
    cfg.insecure_http_for_tests = false;
    let mono = MonoProvider::new(cfg).unwrap();
    assert!(
        mono.verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .is_err()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/payments/verify/CHK1ABCDEFGHIJKLMNOPQRST"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/elsewhere"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/elsewhere"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(verify_body("successful", json!({}))),
        )
        .expect(0)
        .mount(&server)
        .await;
    assert!(
        provider(&server)
            .verify_payment("CHK1ABCDEFGHIJKLMNOPQRST")
            .await
            .is_err()
    );
}

// ------------------------------------------------------------------ payouts

#[tokio::test]
async fn create_payout_body_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/payments/disburse/disbursements"))
        .and(header("mono-sec-key", SECRET_KEY))
        .and(body_json(json!({
            "reference": "PO12345678901234567890",
            "source": "mandate",
            "account": SOURCE_ACCOUNT,
            "type": "instant",
            "total_amount": 945000,
            "description": "Marketplace payout",
            "distribution": [{
                "reference": "PO12345678901234567890",
                "recipient_email": RECIPIENT_EMAIL,
                "account": { "account_number": ACCOUNT_NUMBER, "bank_code": "058" },
                "amount": 945000,
                "narration": "Marketplace payout"
            }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "successful",
            "message": "Disbursement created",
            "data": { "id": "6745ab12cd", "reference": "PO12345678901234567890" }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let handle = provider(&server).create_payout(payout_req()).await.unwrap();
    assert_eq!(
        handle,
        PayoutHandle {
            provider_payout_id: "6745ab12cd".into()
        }
    );
}

#[tokio::test]
async fn create_payout_timeout_is_unknown_and_never_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/payments/disburse/disbursements"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{"id":"x"}}))
                .set_delay(Duration::from_secs(3)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut cfg = config(&server.uri());
    cfg.timeout = Duration::from_millis(300);
    let mono = MonoProvider::new(cfg).unwrap();
    assert!(matches!(
        mono.create_payout(payout_req()).await,
        Err(ProviderError::Timeout)
    ));
    // `expect(1)` is checked when the server drops: exactly one POST.
}

#[tokio::test]
async fn create_payout_server_error_is_unknown_not_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/payments/disburse/disbursements"))
        .respond_with(ResponseTemplate::new(502))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server).create_payout(payout_req()).await,
        Err(ProviderError::Timeout)
    ));
}

#[tokio::test]
async fn create_payout_without_disburse_config_is_not_supported() {
    let server = MockServer::start().await;
    for strip in 0..3 {
        let mut cfg = config(&server.uri());
        match strip {
            0 => cfg.disburse_unit = None,
            1 => cfg.disburse_source_account = None,
            _ => cfg.payout_recipient_email = None,
        }
        let mono = MonoProvider::new(cfg).unwrap();
        assert!(matches!(
            mono.create_payout(payout_req()).await,
            Err(ProviderError::NotSupported)
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn create_payout_refuses_fractional_naira() {
    let server = MockServer::start().await;
    let mut cfg = config(&server.uri());
    cfg.disburse_unit = Some(AmountUnit::Naira);
    let mut req = payout_req();
    req.amount_kobo = 945_001;
    assert!(matches!(
        MonoProvider::new(cfg).unwrap().create_payout(req).await,
        Err(ProviderError::Rejected { .. })
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// Unconfirmed shape: the GET response's per-distribution list.
fn disbursement(batch_status: &str, dist_status: &str, fee: Value) -> Value {
    json!({
        "status": "successful",
        "message": "Disbursement retrieved",
        "data": {
            "id": "6745ab12cd",
            "reference": "PO12345678901234567890",
            "status": batch_status,
            "total_amount": 945000,
            "total_fee": 3500,
            "distributions": [{
                "reference": "PO12345678901234567890",
                "status": dist_status,
                "amount": 945000,
                "fee": fee,
                "account_number": ACCOUNT_NUMBER,
                "bank_code": "058"
            }]
        }
    })
}

async fn status_of(body: Value) -> PayoutState {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/payments/disburse/disbursements/6745ab12cd"))
        .and(header("mono-sec-key", SECRET_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    provider(&server)
        .payout_status("PO12345678901234567890", Some("6745ab12cd"))
        .await
        .unwrap()
}

#[tokio::test]
async fn payout_status_mapping() {
    assert_eq!(
        status_of(disbursement("completed", "successful", json!(3000))).await,
        PayoutState::Successful { fee_kobo: 3000 }
    );
    // No per-distribution fee: the batch total_fee is used.
    assert_eq!(
        status_of(disbursement("completed", "successful", Value::Null)).await,
        PayoutState::Successful { fee_kobo: 3500 }
    );
    // A completed batch can carry a failed distribution (recorded sample).
    assert!(matches!(
        status_of(disbursement("completed", "failed", json!(0))).await,
        PayoutState::Failed { .. }
    ));
    for st in ["processing", "pending", "brand_new_state"] {
        assert_eq!(
            status_of(disbursement("processing", st, json!(0))).await,
            PayoutState::Pending,
            "{st}"
        );
    }
    // A batch status alone never settles a payout.
    let mut batch_only = disbursement("successful", "", json!(0));
    batch_only["data"]
        .as_object_mut()
        .unwrap()
        .remove("distributions");
    assert_eq!(status_of(batch_only).await, PayoutState::Pending);
}

#[tokio::test]
async fn payout_status_failed_reason_is_fixed_text() {
    let mut body = disbursement("completed", "failed", json!(0));
    body["data"]["distributions"][0]["message"] =
        json!("Account 0987654321 ADA OBI does not exist");
    let PayoutState::Failed { reason } = status_of(body).await else {
        panic!("expected failed")
    };
    assert!(!reason.contains(ACCOUNT_NUMBER) && !reason.contains("ADA"));
}

fn list_body(items: Value) -> Value {
    json!({ "status": "successful", "message": "Disbursements retrieved", "data": items })
}

async fn mount_list(server: &MockServer, items: Value) {
    Mock::given(method("GET"))
        .and(path("/v3/payments/disburse/disbursements"))
        .and(query_param("reference", "PO12345678901234567890"))
        .and(header("mono-sec-key", SECRET_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_body(items)))
        .mount(server)
        .await;
}

#[tokio::test]
async fn find_payout_by_reference() {
    let server = MockServer::start().await;
    mount_list(
        &server,
        json!([
            {"id": "other", "reference": "PO99999999999999999999"},
            {"id": "6745ab12cd", "reference": "PO12345678901234567890"}
        ]),
    )
    .await;
    assert_eq!(
        provider(&server)
            .find_payout("PO12345678901234567890")
            .await
            .unwrap(),
        Some(PayoutHandle {
            provider_payout_id: "6745ab12cd".into()
        })
    );
}

#[tokio::test]
async fn find_payout_empty_list_is_not_proof_of_absence() {
    // REVISIT(Q5,Q6): until Mono confirms the filter and that in-flight
    // disbursements are listed, an empty list cannot tell either way.
    let server = MockServer::start().await;
    mount_list(&server, json!([])).await;
    assert!(matches!(
        provider(&server)
            .find_payout("PO12345678901234567890")
            .await,
        Err(ProviderError::Inconclusive)
    ));
}

#[tokio::test]
async fn find_payout_cannot_conclude_absent_from_an_unfiltered_page() {
    // Other disbursements but not ours: the filter may have been ignored and
    // ours may sit on a later page. Never report "absent" (that would allow a
    // second create).
    let server = MockServer::start().await;
    mount_list(
        &server,
        json!([{"id": "other", "reference": "PO99999999999999999999"}]),
    )
    .await;
    assert!(matches!(
        provider(&server)
            .find_payout("PO12345678901234567890")
            .await,
        Err(ProviderError::Inconclusive)
    ));
}

#[tokio::test]
async fn find_payout_transport_timeout_stays_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/payments/disburse/disbursements"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(list_body(json!([])))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&server)
        .await;
    let mut cfg = config(&server.uri());
    cfg.timeout = Duration::from_millis(300);
    assert!(matches!(
        MonoProvider::new(cfg)
            .unwrap()
            .find_payout("PO12345678901234567890")
            .await,
        Err(ProviderError::Timeout)
    ));
}

#[tokio::test]
async fn payout_status_without_id_looks_the_payout_up_first() {
    let server = MockServer::start().await;
    mount_list(
        &server,
        json!([{"id": "6745ab12cd", "reference": "PO12345678901234567890"}]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/v3/payments/disburse/disbursements/6745ab12cd"))
        .respond_with(ResponseTemplate::new(200).set_body_json(disbursement(
            "completed",
            "successful",
            json!(3500),
        )))
        .mount(&server)
        .await;
    assert_eq!(
        provider(&server)
            .payout_status("PO12345678901234567890", None)
            .await
            .unwrap(),
        PayoutState::Successful { fee_kobo: 3500 }
    );

    // No id and an empty lookup: the status cannot be known.
    let server = MockServer::start().await;
    mount_list(&server, json!([])).await;
    assert!(matches!(
        provider(&server)
            .payout_status("PO12345678901234567890", None)
            .await,
        Err(ProviderError::Inconclusive)
    ));
}

#[tokio::test]
async fn create_payout_possible_duplicate_is_unknown_not_rejected() {
    let cases = [
        (409, json!({"status": "failed", "message": "Conflict"})),
        (408, json!({})),
        (425, json!({})),
        (
            400,
            json!({"status": "failed", "message": "Duplicate reference"}),
        ),
        (
            422,
            json!({"status": "failed", "message": "Reference ALREADY EXISTS"}),
        ),
        (
            400,
            json!({"status": "failed", "message": "Disbursement already existed"}),
        ),
    ];
    for (code, body) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/payments/disburse/disbursements"))
            .respond_with(ResponseTemplate::new(code).set_body_json(body.clone()))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            matches!(
                provider(&server).create_payout(payout_req()).await,
                Err(ProviderError::Timeout)
            ),
            "{code} {body}"
        );
    }
}

#[tokio::test]
async fn oversize_bodies_are_failed_reads() {
    let big = "x".repeat(server::payments::mono::MAX_RESPONSE_BODY + 1);
    let server = MockServer::start().await;
    mount_verify(&server, 200, json!({ "status": "successful", "pad": big })).await;
    Mock::given(method("POST"))
        .and(path("/v3/payments/disburse/disbursements"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "data": {"id": "x"}, "pad": big })),
        )
        .mount(&server)
        .await;
    let mono = provider(&server);
    assert!(matches!(
        mono.verify_payment("CHK1ABCDEFGHIJKLMNOPQRST").await,
        Err(ProviderError::Unavailable(_))
    ));
    assert!(matches!(
        mono.create_payout(payout_req()).await,
        Err(ProviderError::Timeout)
    ));
}

// ---------------------------------------------------- refunds and lookups

#[tokio::test]
async fn refunds_and_account_lookup_are_not_supported() {
    let server = MockServer::start().await;
    let mono = provider(&server);
    let full = RefundRequest {
        reference: "RF1".into(),
        payment_reference: "CHK1ABCDEFGHIJKLMNOPQRST".into(),
        amount_kobo: 1_050_000,
        reason: "cancelled".into(),
    };
    let partial = RefundRequest {
        amount_kobo: 500_000,
        ..full.clone()
    };
    assert!(matches!(
        mono.refund(full).await,
        Err(ProviderError::NotSupported)
    ));
    assert!(matches!(
        mono.refund(partial).await,
        Err(ProviderError::NotSupported)
    ));
    assert!(matches!(
        mono.resolve_account("058", ACCOUNT_NUMBER).await,
        Err(ProviderError::NotSupported)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ----------------------------------------------------------------- webhooks

fn headers(secret: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(s) = secret {
        h.insert("mono-webhook-secret", HeaderValue::from_str(s).unwrap());
    }
    h
}

fn hook(mono: &MonoProvider, body: &Value) -> Result<WebhookEvent, ProviderError> {
    mono.verify_webhook(&headers(Some(WEBHOOK_SECRET)), body.to_string().as_bytes())
}

fn payment_event(event: &str, livemode: bool) -> Value {
    // Recorded sample shape (payments webhook page): payload under data.object.
    json!({
        "event": event,
        "event_id": "evt_01",
        "data": {
            "object": {
                "id": "txd_77",
                "status": "successful",
                "amount": 1050000,
                "fee": 15000,
                "currency": "NGN",
                "reference": "CHK1ABCDEFGHIJKLMNOPQRST",
                "livemode": livemode,
                "flagged": false,
                "held_settlement": false,
                "account": { "name": "ADA OBI", "account_number": ACCOUNT_NUMBER }
            }
        }
    })
}

fn transaction_event(event: &str) -> Value {
    // Recorded sample shape (disburse webhook page): no data.id.
    json!({
        "event": event,
        "event_id": "evt_02",
        "data": {
            "disbursement_reference": "PO12345678901234567890",
            "reference": "PO12345678901234567890",
            "account_number": ACCOUNT_NUMBER,
            "bank_code": "058",
            "amount": 945000,
            "fee": 3500,
            "narration": "Marketplace payout",
            "status": "successful"
        }
    })
}

#[test]
fn webhook_wrong_secret_rejected() {
    let mono = MonoProvider::new(config("http://127.0.0.1:1")).unwrap();
    let body = payment_event("direct_debit.payment_successful", false).to_string();
    for h in [
        headers(None),
        headers(Some("wrong")),
        headers(Some(&format!("{WEBHOOK_SECRET}x"))),
        headers(Some("")),
    ] {
        assert!(matches!(
            mono.verify_webhook(&h, body.as_bytes()),
            Err(ProviderError::InvalidSignature)
        ));
    }
    // The secret is checked before the body is parsed.
    assert!(matches!(
        mono.verify_webhook(&headers(Some("wrong")), b"not json"),
        Err(ProviderError::InvalidSignature)
    ));
}

#[test]
fn webhook_parses_payment_and_disbursement_events() {
    let mono = MonoProvider::new(config("http://127.0.0.1:1")).unwrap();
    for event in [
        "direct_debit.payment_successful",
        "direct_debit.payment_failed",
        "direct_debit.payment_cancelled",
        "direct_debit.payment_abandoned",
    ] {
        assert_eq!(
            hook(&mono, &payment_event(event, false)).unwrap(),
            WebhookEvent::Payment {
                event_id: "evt_01".into(),
                reference: "CHK1ABCDEFGHIJKLMNOPQRST".into(),
            },
            "{event}"
        );
    }
    for event in [
        "event.disbursement.transaction.successful",
        "event.disbursement.transaction.failed",
    ] {
        assert_eq!(
            hook(&mono, &transaction_event(event)).unwrap(),
            WebhookEvent::Payout {
                event_id: "evt_02".into(),
                reference: "PO12345678901234567890".into(),
            },
            "{event}"
        );
    }
}

#[test]
fn webhook_batch_account_and_unknown_events_are_ignored() {
    let mono = MonoProvider::new(config("http://127.0.0.1:1")).unwrap();
    // Recorded sample: a "completed" batch whose status is failed.
    let completed = json!({
        "event": "event.disbursement.completed",
        "event_id": "evt_03",
        "data": {
            "reference": "PO12345678901234567890",
            "status": "failed",
            "message": "Disbursement completed successfully",
            "successful": 0,
            "failed": 1
        }
    });
    let account = json!({
        "event": "event.disburse.account.created",
        "event_id": "evt_04",
        "data": { "account_id": "688a0b672f88771f77d05cf5" }
    });
    let connected = json!({
        "event": "mono.events.account_connected",
        "event_id": "evt_05",
        "data": { "id": "acc_1", "meta": { "ref": "x" } }
    });
    let unknown = json!({"event": "direct_debit.something_new", "event_id": "evt_06", "data": {}});
    let no_reference = json!({
        "event": "direct_debit.payment_successful",
        "event_id": "evt_07",
        "data": { "object": { "status": "successful" } }
    });
    for (body, id) in [
        (completed, "evt_03"),
        (account, "evt_04"),
        (connected, "evt_05"),
        (unknown, "evt_06"),
        (no_reference, "evt_07"),
    ] {
        assert_eq!(
            hook(&mono, &body).unwrap(),
            WebhookEvent::Ignored {
                event_id: id.into()
            },
            "{body}"
        );
    }
}

#[test]
fn webhook_livemode_must_match_configured_mode() {
    let test_mode = MonoProvider::new(config("http://127.0.0.1:1")).unwrap();
    let mut live_cfg = config("http://127.0.0.1:1");
    live_cfg.live = true;
    let live_mode = MonoProvider::new(live_cfg).unwrap();

    let live_event = payment_event("direct_debit.payment_successful", true);
    let test_event = payment_event("direct_debit.payment_successful", false);
    assert!(matches!(
        hook(&test_mode, &live_event).unwrap(),
        WebhookEvent::Ignored { .. }
    ));
    assert!(matches!(
        hook(&live_mode, &test_event).unwrap(),
        WebhookEvent::Ignored { .. }
    ));
    assert!(matches!(
        hook(&live_mode, &live_event).unwrap(),
        WebhookEvent::Payment { .. }
    ));
    // `liveMode` casing is accepted too.
    let mut camel = live_event.clone();
    let obj = camel["data"]["object"].as_object_mut().unwrap();
    obj.remove("livemode");
    obj.insert("liveMode".into(), json!(true));
    assert!(matches!(
        hook(&test_mode, &camel).unwrap(),
        WebhookEvent::Ignored { .. }
    ));
}

#[test]
fn webhook_livemode_mismatch_on_transaction_event_is_ignored() {
    let mono = MonoProvider::new(config("http://127.0.0.1:1")).unwrap();
    for key in ["livemode", "liveMode"] {
        let mut body = transaction_event("event.disbursement.transaction.successful");
        body["data"][key] = json!(true);
        assert_eq!(
            hook(&mono, &body).unwrap(),
            WebhookEvent::Ignored {
                event_id: "evt_02".into()
            },
            "{key}"
        );
    }
}

#[test]
fn webhook_payment_event_without_event_id_gets_a_stable_one() {
    let mono = MonoProvider::new(config("http://127.0.0.1:1")).unwrap();
    let mut body = payment_event("direct_debit.payment_successful", false);
    body.as_object_mut().unwrap().remove("event_id");
    let first = hook(&mono, &body).unwrap();
    assert_eq!(first, hook(&mono, &body).unwrap());
    let WebhookEvent::Payment {
        event_id,
        reference,
    } = first
    else {
        panic!("expected payment")
    };
    assert_eq!(reference, "CHK1ABCDEFGHIJKLMNOPQRST");
    assert_eq!(
        event_id,
        "direct_debit.payment_successful:CHK1ABCDEFGHIJKLMNOPQRST"
    );
}

#[test]
fn webhook_transaction_event_without_event_id_gets_a_stable_one() {
    let mono = MonoProvider::new(config("http://127.0.0.1:1")).unwrap();
    let mut body = transaction_event("event.disbursement.transaction.successful");
    body.as_object_mut().unwrap().remove("event_id");
    let first = hook(&mono, &body).unwrap();
    assert_eq!(first, hook(&mono, &body).unwrap());
    let WebhookEvent::Payout {
        event_id,
        reference,
    } = first
    else {
        panic!("expected payout")
    };
    assert_eq!(reference, "PO12345678901234567890");
    assert!(!event_id.is_empty() && !event_id.contains(ACCOUNT_NUMBER));
}

#[test]
fn webhook_malformed_body_is_an_error_not_a_panic() {
    let mono = MonoProvider::new(config("http://127.0.0.1:1")).unwrap();
    for body in [&b"not json"[..], b"[]", b"{}", b"{\"event\":5}"] {
        assert!(
            mono.verify_webhook(&headers(Some(WEBHOOK_SECRET)), body)
                .is_err()
        );
    }
}

// ------------------------------------------------------------ data hygiene

#[test]
fn secret_not_in_debug_output() {
    let cfg = config("https://api.withmono.com");
    let mono = MonoProvider::new(cfg.clone()).unwrap();
    for out in [
        format!("{cfg:?}"),
        format!("{cfg:#?}"),
        format!("{mono:?}"),
        format!("{mono:#?}"),
        format!("{:?}", Secret::new(SECRET_KEY)),
    ] {
        assert!(!out.contains(SECRET_KEY), "{out}");
        assert!(!out.contains(WEBHOOK_SECRET), "{out}");
        assert!(!out.contains("9f8e7d6c"), "{out}");
    }
}

#[test]
fn name_methods_and_hosts() {
    let mono = MonoProvider::new(config("http://127.0.0.1:1")).unwrap();
    assert_eq!(mono.name(), "mono");
    assert_eq!(mono.methods(), &["mono_directpay"]);
    let hosts = mono.payment_url_hosts();
    assert!(payment_url_allowed("https://checkout.mono.co/abc", hosts));
    assert!(!payment_url_allowed(
        "https://checkout.mono.co.evil.test/abc",
        hosts
    ));
    assert!(!payment_url_allowed("http://checkout.mono.co/abc", hosts));
}

#[tokio::test]
#[tracing_test::traced_test]
async fn logs_carry_no_bodies_secrets_or_account_numbers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/payments/disburse/disbursements"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "status": "failed",
            "message": "Account 0987654321 for ADA OBI is invalid"
        })))
        .mount(&server)
        .await;
    mount_verify(&server, 500, json!({"message": "boom 0987654321"})).await;
    let mono = provider(&server);
    let _ = mono.create_payout(payout_req()).await;
    let _ = mono.verify_payment("CHK1ABCDEFGHIJKLMNOPQRST").await;
    let _ = hook(&mono, &transaction_event("event.disbursement.completed"));
    // Something was logged (the sanitised cause) ...
    assert!(logs_contain("mono"));
    // ... but never the body, the key or the account number.
    assert!(!logs_contain(ACCOUNT_NUMBER));
    assert!(!logs_contain("ADA OBI"));
    assert!(!logs_contain(SECRET_KEY));
    assert!(!logs_contain("boom"));
}

// ------------------------------------------------------------- live smoke

/// **Run by hand only**, against Mono's sandbox with real test keys:
///
/// ```text
/// MONO_SMOKE_SECRET_KEY=test_sk_... cargo test --test mono_adapter -- --ignored --nocapture
/// ```
///
/// Initiates a ₦100 DirectPay payment (10,000 if the unit is kobo) and prints
/// the hosted `mono_url`. An echo of the amount proves nothing, so a person
/// opens the URL and checks the page shows **₦100.00**, then records the
/// result (date, who, what the page showed) in the doc comment of
/// `MONO_DIRECTPAY_AMOUNT_UNIT` handling in `src/payments/mono.rs`. The
/// Disburse unit needs its own live check (Disburse is live-only): a minimum
/// payout to an owner account, checked against the bank statement.
#[tokio::test]
#[ignore = "needs a Mono sandbox key (MONO_SMOKE_SECRET_KEY); run by hand"]
async fn sandbox_smoke_test() {
    let Ok(key) = std::env::var("MONO_SMOKE_SECRET_KEY") else {
        panic!("set MONO_SMOKE_SECRET_KEY to a Mono test_sk_ key");
    };
    assert!(
        key.starts_with("test_sk_"),
        "the smoke test only runs with a sandbox (test_sk_) key"
    );
    let base =
        std::env::var("MONO_SMOKE_BASE_URL").unwrap_or_else(|_| "https://api.withmono.com".into());
    let mut cfg = config(&base);
    cfg.secret_key = Secret::new(&key);
    cfg.insecure_http_for_tests = false;
    let mono = MonoProvider::new(cfg).unwrap();
    let reference = server::payments::new_reference("SMK");
    let out = mono
        .initiate(InitiatePayment {
            reference: reference.clone(),
            amount_kobo: 10_000,
            description: "Amount unit smoke test".into(),
            customer_name: "Smoke Test".into(),
            customer_email: None,
            redirect_url: "https://example.com/return".into(),
        })
        .await
        .expect("initiate");
    assert!(payment_url_allowed(
        &out.payment_url,
        mono.payment_url_hosts()
    ));
    println!("reference: {reference}");
    println!(
        "open this page and check it shows NGN 100.00: {}",
        out.payment_url
    );
    let status = mono.verify_payment(&reference).await.expect("verify");
    println!("verify right after initiate: {status:?}");
}
