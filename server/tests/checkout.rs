mod common;

use axum::http::Method;
use common::{Listing, TEST_REDIRECT_URL, TestApp};
use serde_json::json;
use server::limits::MAX_CHECKOUT_TOTAL_KOBO;
use sqlx::PgPool;
use uuid::Uuid;

const METHOD: &str = "mono_directpay";
const BUYER: &str = "08022222221";
const BUYER2: &str = "08022222222";

async fn listing(app: &TestApp, price: i64, stock: i32) -> Listing {
    app.make_active_store_with_product("08011111111", "Mama Put", price, stock)
        .await
}

async fn checkout_status(app: &TestApp, id: &str) -> String {
    sqlx::query_scalar("SELECT status::text FROM checkouts WHERE id = $1")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

async fn count(app: &TestApp, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&app.pool).await.unwrap()
}

#[sqlx::test]
async fn checkout_splits_orders_per_store(pool: PgPool) {
    let app = TestApp::new(pool).await;
    // 1_050 * 5% = 52.5 -> 53 (half-up).
    let a = app
        .make_active_store_with_product("08011111111", "Mama Put", 1_050, 5)
        .await;
    let b = app
        .make_active_store_with_product("08011111112", "Bukka Hut", 1_000_000, 5)
        .await;
    let (buyer_id, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, a.variant_id, 1).await;
    app.set_cart(&buyer, b.variant_id, 2).await;

    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["total_kobo"], 2_001_050);
    let reference = res.json["reference"].as_str().unwrap();
    assert!(
        reference.starts_with("CK") && reference.len() == 22,
        "{reference}"
    );
    assert_eq!(
        res.json["payment_url"],
        format!("https://pay.fake.test/{reference}")
    );
    let orders = res.json["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2);
    let mut totals: Vec<(String, i64)> = orders
        .iter()
        .map(|o| {
            (
                o["store_name"].as_str().unwrap().to_string(),
                o["items_total_kobo"].as_i64().unwrap(),
            )
        })
        .collect();
    totals.sort();
    assert_eq!(
        totals,
        vec![("Bukka Hut".into(), 2_000_000), ("Mama Put".into(), 1_050)]
    );

    let rows: Vec<(Uuid, i64, i32, i64, i64, String, Uuid)> = sqlx::query_as(
        "SELECT store_id, items_total_kobo, platform_cut_bps, platform_cut_kobo, \
                delivery_fee_kobo, status::text, buyer_id FROM orders ORDER BY items_total_kobo",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                a.store_id,
                1_050,
                500,
                53,
                0,
                "pending_payment".into(),
                buyer_id
            ),
            (
                b.store_id,
                2_000_000,
                500,
                100_000,
                0,
                "pending_payment".into(),
                buyer_id
            ),
        ]
    );
    let sum: i64 = rows.iter().map(|r| r.1 + r.4).sum();
    assert_eq!(sum, 2_001_050);

    let (provider, payment_id, total, ttl_ok, method): (String, String, i64, bool, String) =
        sqlx::query_as(
            "SELECT provider, provider_payment_id, total_kobo, \
                    expires_at BETWEEN now() + interval '29 minutes' AND now() + interval '31 minutes', \
                    method FROM checkouts",
        )
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(provider, "fake");
    assert_eq!(payment_id, format!("fake_{reference}"));
    assert_eq!(total, 2_001_050);
    assert!(ttl_ok);
    assert_eq!(method, METHOD);

    // Items are snapshotted.
    let items: Vec<(Uuid, String, i64, i32)> = sqlx::query_as(
        "SELECT variant_id, product_name, unit_price_kobo, quantity FROM order_items ORDER BY unit_price_kobo",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        items,
        vec![
            (a.variant_id, "Ankara Shirt".into(), 1_050, 1),
            (b.variant_id, "Ankara Shirt".into(), 1_000_000, 2),
        ]
    );

    // The provider got the buyer's display name only, no email, and the configured return URL.
    let sent = app.fake.initiated();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].reference, reference);
    assert_eq!(sent[0].amount_kobo, 2_001_050);
    assert_eq!(sent[0].customer_name, "Ada");
    assert_eq!(sent[0].customer_email, None);
    assert_eq!(sent[0].redirect_url, TEST_REDIRECT_URL);

    // The cart stays until the checkout is paid (Task 6).
    let cart = app.request(Method::GET, "/cart", None, Some(&buyer)).await;
    assert_eq!(cart.json["items"].as_array().unwrap().len(), 2);
}

#[sqlx::test]
async fn checkout_reserves_stock(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 2).await;
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(app.stock(l.variant_id).await, 3);
}

#[sqlx::test]
async fn concurrent_last_unit_one_wins(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 1).await;
    let (_, b1) = app.sign_up_verified(BUYER).await;
    let (_, b2) = app.sign_up_verified(BUYER2).await;
    app.set_cart(&b1, l.variant_id, 1).await;
    app.set_cart(&b2, l.variant_id, 1).await;

    let (r1, r2) = tokio::join!(app.checkout(&b1, METHOD), app.checkout(&b2, METHOD));
    let mut statuses = [r1.status, r2.status];
    statuses.sort();
    assert_eq!(statuses, [201, 409], "{} / {}", r1.json, r2.json);
    let loser = if r1.status == 409 { &r1 } else { &r2 };
    assert_eq!(loser.json["error"]["code"], "out_of_stock");
    assert!(
        loser.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&l.variant_id.to_string())
    );
    assert_eq!(app.stock(l.variant_id).await, 0);
    assert_eq!(count(&app, "SELECT count(*) FROM checkouts").await, 1);
}

#[sqlx::test]
async fn unpublished_or_pending_store_item_unavailable(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 1).await;

    app.request(
        Method::PATCH,
        &format!("/products/{}", l.product_id),
        Some(json!({ "is_published": false })),
        Some(&l.seller_token),
    )
    .await;
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "item_unavailable");

    app.request(
        Method::PATCH,
        &format!("/products/{}", l.product_id),
        Some(json!({ "is_published": true })),
        Some(&l.seller_token),
    )
    .await;
    sqlx::query("UPDATE stores SET status = 'pending' WHERE id = $1")
        .bind(l.store_id)
        .execute(&app.pool)
        .await
        .unwrap();
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "item_unavailable");

    // Nothing was reserved or created.
    assert_eq!(app.stock(l.variant_id).await, 5);
    assert_eq!(count(&app, "SELECT count(*) FROM checkouts").await, 0);
}

#[sqlx::test]
async fn disabled_method_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 1).await;
    let res = app.checkout(&buyer, "paystack_card").await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "payment_method_disabled");
    assert_eq!(app.stock(l.variant_id).await, 5);
    assert_eq!(app.fake.calls("initiate"), 0);
}

#[sqlx::test]
async fn own_store_item_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    // The cart refuses it (see tests/cart.rs), so put it there directly.
    sqlx::query("INSERT INTO cart_items (user_id, variant_id, quantity) VALUES ($1, $2, 1)")
        .bind(l.seller_id)
        .bind(l.variant_id)
        .execute(&app.pool)
        .await
        .unwrap();
    let res = app.checkout(&l.seller_token, METHOD).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "own_store_item");
    assert_eq!(app.stock(l.variant_id).await, 5);
}

#[sqlx::test]
async fn empty_cart_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "cart_empty");
    assert_eq!(
        app.request(
            Method::POST,
            "/checkout",
            Some(json!({"method": METHOD})),
            None
        )
        .await
        .status,
        401
    );
}

async fn assert_failed_and_restored(app: &TestApp, l: &Listing, res: &common::TestResponse) {
    assert_eq!(res.status, 502, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "payment_provider_unavailable");
    assert_eq!(app.stock(l.variant_id).await, 5);
    let (status, url): (String, Option<String>) =
        sqlx::query_as("SELECT status::text, payment_url FROM checkouts")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(status, "expired");
    assert_eq!(url, None);
    let orders: Vec<(String, bool)> =
        sqlx::query_as("SELECT status::text, cancelled_at IS NOT NULL FROM orders")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(orders, vec![("cancelled".into(), true)]);
}

#[sqlx::test]
async fn provider_failure_restores_stock(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 2).await;
    app.fake.fail_initiate();
    let res = app.checkout(&buyer, METHOD).await;
    assert_failed_and_restored(&app, &l, &res).await;
}

#[sqlx::test]
async fn provider_timeout_restores_stock(pool: PgPool) {
    // Timeout: the provider may have created the payment, but nothing is paid until
    // verified; a later webhook goes through Task 6's paid-after-expiry path.
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 2).await;
    app.fake.timeout_after_success_initiate();
    let res = app.checkout(&buyer, METHOD).await;
    assert_failed_and_restored(&app, &l, &res).await;
}

#[sqlx::test]
async fn untrusted_payment_url_is_a_failure(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 2).await;
    app.fake
        .set_payment_url(Some("https://pay.fake.test.evil.example/x"));
    let res = app.checkout(&buyer, METHOD).await;
    assert_failed_and_restored(&app, &l, &res).await;
    assert!(!res.json.to_string().contains("evil"));
}

#[sqlx::test]
async fn expiring_twice_restores_once(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 2).await;
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(app.stock(l.variant_id).await, 3);
    let id: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();

    for expected in [true, false] {
        let mut tx = app.pool.begin().await.unwrap();
        let expired = server::checkout::expire_and_restore(&mut tx, id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(expired, expected);
        assert_eq!(app.stock(l.variant_id).await, 5);
    }
    assert_eq!(checkout_status(&app, &id.to_string()).await, "expired");

    // A paid checkout is never expired and its stock never comes back.
    app.set_cart(&buyer, l.variant_id, 1).await;
    let res = app.checkout(&buyer, METHOD).await;
    let id: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE checkouts SET status = 'paid', paid_at = now() WHERE id = $1")
        .bind(id)
        .execute(&app.pool)
        .await
        .unwrap();
    let mut tx = app.pool.begin().await.unwrap();
    assert!(
        !server::checkout::expire_and_restore(&mut tx, id)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(app.stock(l.variant_id).await, 4);
}

#[sqlx::test]
async fn variant_in_order_cannot_be_deleted(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let product_id = app
        .add_product(
            &l.seller_token,
            l.store_id,
            100_000,
            &[(json!({"size": "M"}), 5), (json!({"size": "L"}), 5)],
        )
        .await;
    let medium: Uuid = sqlx::query_scalar(
        "SELECT id FROM product_variants WHERE product_id = $1 AND attributes = '{\"size\":\"M\"}'",
    )
    .bind(product_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, medium, 1).await;
    assert_eq!(app.checkout(&buyer, METHOD).await.status, 201);

    let path = format!("/products/{product_id}/variants");
    let res = app
        .request(
            Method::PUT,
            &path,
            Some(json!([{ "attributes": { "size": "L" }, "stock": 3 }])),
            Some(&l.seller_token),
        )
        .await;
    assert_eq!(res.status, 409, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "variant_in_use");
    assert_eq!(app.variant_ids(product_id).await.len(), 2);

    // Keeping the ordered variant (and dropping one never ordered) is fine.
    let res = app
        .request(
            Method::PUT,
            &path,
            Some(json!([{ "attributes": { "size": "M" }, "stock": 3 }])),
            Some(&l.seller_token),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(app.variant_ids(product_id).await, vec![medium]);
}

#[sqlx::test]
async fn checkout_total_cap(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 1_000_000_000, 10).await;
    let cheap = app
        .add_product(&l.seller_token, l.store_id, 1, &[(json!({}), 10)])
        .await;
    let cheap = app.variant_ids(cheap).await[0];
    let (_, buyer) = app.sign_up_verified(BUYER).await;

    // Exactly the cap is allowed.
    app.set_cart(&buyer, l.variant_id, 5).await;
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["total_kobo"], MAX_CHECKOUT_TOTAL_KOBO);

    // Cap + 1 kobo is refused and reserves nothing.
    app.set_cart(&buyer, cheap, 1).await;
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "amount_too_large");
    assert_eq!(app.stock(l.variant_id).await, 5);
    assert_eq!(app.stock(cheap).await, 10);
}

#[sqlx::test]
async fn pending_checkout_cap(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 50).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 1).await;
    for _ in 0..3 {
        assert_eq!(app.checkout(&buyer, METHOD).await.status, 201);
    }
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 429, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "too_many_pending_checkouts");
    assert_eq!(app.stock(l.variant_id).await, 47);

    // Once one is no longer pending, the buyer can check out again.
    sqlx::query(
        "UPDATE checkouts SET status = 'expired' \
         WHERE id = (SELECT id FROM checkouts ORDER BY created_at LIMIT 1)",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    assert_eq!(app.checkout(&buyer, METHOD).await.status, 201);

    // The cap is per buyer.
    let (_, other) = app.sign_up_verified(BUYER2).await;
    app.set_cart(&other, l.variant_id, 1).await;
    assert_eq!(app.checkout(&other, METHOD).await.status, 201);
}

#[sqlx::test]
async fn idempotent_checkout_returns_same(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 1).await;
    let send = |key: &'static str, method: &'static str| {
        let buyer = buyer.clone();
        let app = &app;
        async move {
            app.request_with_headers(
                Method::POST,
                "/checkout",
                Some(json!({ "method": method })),
                Some(&buyer),
                &[("idempotency-key", key)],
            )
            .await
        }
    };

    let first = send("order-1_abc", METHOD).await;
    assert_eq!(first.status, 201, "{}", first.json);
    let again = send("order-1_abc", METHOD).await;
    assert_eq!(again.status, 200, "{}", again.json);
    assert_eq!(again.json, first.json);
    assert_eq!(app.fake.calls("initiate"), 1);
    assert_eq!(count(&app, "SELECT count(*) FROM checkouts").await, 1);
    assert_eq!(app.stock(l.variant_id).await, 4);

    // Same key, different request.
    let res = send("order-1_abc", "paystack_card").await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "idempotency_key_mismatch");

    // A different key is a new checkout.
    assert_eq!(send("order-2", METHOD).await.status, 201);

    // Bad keys are refused.
    let res = send("has space", METHOD).await;
    assert_eq!(res.status, 422, "{}", res.json);
    let long: &'static str = Box::leak("k".repeat(65).into_boxed_str());
    assert_eq!(send(long, METHOD).await.status, 422);

    // Keys are per buyer: another buyer's same key is their own checkout.
    let (_, other) = app.sign_up_verified(BUYER2).await;
    app.set_cart(&other, l.variant_id, 1).await;
    let res = app
        .request_with_headers(
            Method::POST,
            "/checkout",
            Some(json!({ "method": METHOD })),
            Some(&other),
            &[("idempotency-key", "order-1_abc")],
        )
        .await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_ne!(res.json["checkout_id"], first.json["checkout_id"]);
}

#[sqlx::test]
async fn idempotent_replay_of_failed_checkout_is_refused(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 1).await;
    app.fake.fail_initiate();
    let send = || {
        app.request_with_headers(
            Method::POST,
            "/checkout",
            Some(json!({ "method": METHOD })),
            Some(&buyer),
            &[("idempotency-key", "k1")],
        )
    };
    assert_eq!(send().await.status, 502);
    let res = send().await;
    assert_eq!(res.status, 409, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "checkout_failed");
    assert_eq!(app.fake.calls("initiate"), 1);
}

#[sqlx::test]
async fn checkout_uses_current_price(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 2).await;
    let preview = app.request(Method::GET, "/cart", None, Some(&buyer)).await;
    assert_eq!(preview.json["total_kobo"], 200_000);
    app.request(
        Method::PATCH,
        &format!("/products/{}", l.product_id),
        Some(json!({ "base_price_kobo": 120_000 })),
        Some(&l.seller_token),
    )
    .await;
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["total_kobo"], 240_000);
}

#[sqlx::test]
async fn get_checkout_is_buyer_only(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = listing(&app, 100_000, 5).await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 2).await;
    let created = app.checkout(&buyer, METHOD).await;
    let id = created.json["checkout_id"].as_str().unwrap();
    let path = format!("/checkout/{id}");

    let res = app.request(Method::GET, &path, None, Some(&buyer)).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["checkout_id"], id);
    assert_eq!(res.json["status"], "pending");
    assert_eq!(res.json["total_kobo"], 200_000);
    assert_eq!(res.json["payment_url"], created.json["payment_url"]);
    assert_eq!(res.json["orders"][0]["store_name"], "Mama Put");
    assert_eq!(res.json["orders"][0]["status"], "pending_payment");
    assert_eq!(res.json["orders"][0]["items"][0]["quantity"], 2);
    assert_eq!(
        res.json["orders"][0]["items"][0]["unit_price_kobo"],
        100_000
    );
    // The platform's cut is not the buyer's business.
    assert!(!res.json.to_string().contains("platform_cut"));

    let (_, other) = app.sign_up_verified(BUYER2).await;
    assert_eq!(
        app.request(Method::GET, &path, None, Some(&other))
            .await
            .status,
        404
    );
    assert_eq!(
        app.request(Method::GET, &path, None, Some(&l.seller_token))
            .await
            .status,
        404
    );
    assert_eq!(
        app.request(Method::GET, "/checkout/not-a-uuid", None, Some(&buyer))
            .await
            .status,
        404
    );
    assert_eq!(
        app.request(Method::GET, &path, None, None).await.status,
        401
    );
}
