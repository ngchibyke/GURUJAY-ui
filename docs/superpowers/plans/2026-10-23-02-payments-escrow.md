# Payments & Escrow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Buyers can put items from several stores in a cart and pay once. The money is held, and each store's share is released to that seller (minus the 5% cut and fees) after delivery is confirmed or 3 days pass. All of this sits behind a payment-provider layer, with Mono as the first provider.

**Architecture:**
- The escrow logic (`checkout`, `orders`, `ledger`, `escrow` jobs) talks only to a `PaymentProvider` trait.
- `MonoProvider` implements that trait with DirectPay (collect) and Disburse (payout). `FakeProvider` implements it for tests.
- Webhooks are treated as hints. Every state change re-checks the payment or payout with the provider's API first.
- Time-based rules (checkout expiry, 48h ship deadline, 3-day release, 30-day backstop) run in a background job loop. Each job is a plain function that takes `now`, so tests drive time directly.

**Tech Stack:** the existing server (Rust, axum 0.8, sqlx 0.8 / Postgres 17) plus `reqwest` 0.12 (rustls) for provider HTTP calls and `wiremock` 0.6 (dev-only) to test the Mono adapter against recorded responses.

**Spec:** `docs/superpowers/specs/2026-10-22-abuja-marketplace-design.md`, sections 4 and 5, with section 11 for the owner settings. The carry-forward list is in `docs/superpowers/plans/2026-10-23-00-roadmap.md`.

## Global Constraints

- **Money:** integer kobo (`i64`). Every kobo sum uses checked arithmetic, and overflow returns 422 `amount_too_large`.
  - Caps: unit price ≤ 1,000,000,000 kobo (₦10m); stock ≤ 100,000; quantity per line ≤ 100; checkout total ≤ 5,000,000,000 kobo (₦50m).
- **Platform cut:** 500 basis points (5%) by default, stored in `platform_settings.platform_cut_bps` and changed only by the owner.
  - The cut is computed per order as `items_total_kobo * bps / 10000`, rounded half-up. It is snapshotted on the order at checkout.
- **Release rules:**
  - Release happens on buyer confirmation, or **3 days (72h) after `delivered_at`**, provided there is no complaint.
  - Nothing auto-releases without `delivered_at`.
  - Orders unreleased **30 days after `paid_at`** go to the owner's backstop queue.
- **Shipping deadline:** an order still `paid` (not picked up) **48 hours after `paid_at`** is auto-cancelled and refunded, and the seller gets a strike.
- **Checkout expiry:** an unpaid checkout expires after **30 minutes**, and its reserved stock goes back.
- **Seller payout:** `items_total − platform_cut − collection_fee_share − payout_fee`.
  - The payout fee follows `platform_settings`: 3,500 kobo when the payout is ≤ 5,000,000 kobo, else 5,000 kobo.
  - A full refund returns items plus delivery to the buyer, and the platform takes no cut on refunded orders.
- **Ledger:** append-only, with DB triggers rejecting UPDATE, DELETE and TRUNCATE (same pattern as `audit_log`).
  - For every order, the entries must sum to zero across accounts. A test enforces this.
- **Webhooks:**
  - Compare the shared-secret header in constant time.
  - Deduplicate on the provider event id.
  - Re-fetch status from the provider API before any state change.
  - Answer 200 for duplicates or unknown references, and 401 for a bad secret.
- **Locks:** one global order: `users → stores → products → product_variants → access_codes → checkouts → orders → payouts`.
  - Row locks on `users` use `FOR NO KEY UPDATE`.
- **No PII or secrets in logs, URLs or audit detail.** That covers bank account numbers, provider secret keys, webhook secrets and buyers' bank details.
  - Seller payout account numbers are encrypted with `FieldCipher`; only the last 4 digits are shown.
  - We store no buyer bank details at all, only provider ids and references.
  - **Data sent to Mono (a third party):** buyer display name and the amount for collection. Email is sent only if the user has one, and no phone or address is ever sent. For payouts, the seller's bank code and account number, with a platform-owned `recipient_email` (config `MONO_PAYOUT_RECIPIENT_EMAIL`) instead of the seller's own. The privacy notice must name Mono as a processor (DPO item).
- **Errors:** always the JSON envelope. Path ids use `Path<String>` + `parse_id`, and bodies use `AppJson<T>`.
- **Mono facts** (from docs.mono.co, 2026-10-23):
  - Base URL `https://api.withmono.com`, auth header `mono-sec-key`.
  - DirectPay initiate: `POST /v2/payments/initiate` with `type:"onetime-debit"`, `method:"account"`, `amount`, `reference`, `description`, `redirect_url`, `customer{name,email}`. The response carries `data.mono_url` and `data.id`.
  - DirectPay verify: `GET /v2/payments/verify/{reference}`.
  - Webhook header `mono-webhook-secret`. Events `direct_debit.payment_successful|failed|cancelled|abandoned`, with the payload under `data.object` and a top-level `event_id`.
  - Disburse create: `POST /v3/payments/disburse/disbursements` with `reference`, `source:"mandate"`, `account` (source account id), `type:"instant"`, `total_amount`, `description`, `distribution[{reference, recipient_email, account{account_number, bank_code}, amount, narration}]`.
  - Disburse status: `GET /v3/payments/disburse/disbursements/{id}`.
  - Disburse events: `event.disbursement.transaction.successful|failed`, `event.disbursement.completed`.
  - **Unconfirmed:** the amount unit. Assume kobo, behind one constant `MONO_AMOUNT_UNIT_IS_KOBO = true`, confirmed by Task 11's sandbox smoke test before live.
  - Disburse is documented as live-only.
- Commit messages carry no AI attribution.

## Review Focus

1. **A replayed or duplicate payment webhook**, including one arriving after the checkout already expired, applies at most once. A paid-after-expiry checkout is re-reserved if stock allows, otherwise it is refunded. Tested in Task 6.
2. **A forged webhook** (wrong secret), or a correct-secret webhook the provider's verify call does not confirm, changes nothing. Tested in Task 6.
3. **Two buyers checking out the last unit at the same moment:** one succeeds, the other gets 409 `out_of_stock`. Tested in Task 4.
4. **A buyer pressing "Received" while the 3-day release job runs:** exactly one payout is created, because the payout reference is unique per order. Tested in Task 8.
5. **An amount mismatch** (provider says paid, amount ≠ checkout total) does not mark the checkout paid. It is flagged `amount_mismatch` for the owner. Tested in Task 6.

---

## File Structure

```
server/migrations/
  0007_carry_forward.sql          caps as CHECKs, variant upsert support
  0008_platform_settings.sql
  0009_payout_accounts.sql
  0010_cart_checkout_orders.sql   carts, checkouts, orders, order_items, stock reservations
  0011_ledger.sql                 ledger_entries (append-only)
  0012_payments_payouts.sql       provider_events, payouts, refunds, seller_strikes
server/src/
  money.rs                        Kobo helpers: checked_sum, cut_bps, payout_fee
  settings/{mod,service,routes}.rs
  payments/
    mod.rs                        PaymentProvider trait + types
    fake.rs                       FakeProvider (tests and dev)
    mono.rs                       MonoProvider (reqwest)
    webhooks.rs                   POST /webhooks/{provider}
  payout_accounts/{mod,service,routes}.rs
  cart/{mod,service,routes}.rs
  checkout/{mod,service,routes}.rs
  orders/{mod,service,routes}.rs  state machine + buyer/seller views
  ledger.rs
  escrow/{mod,release,jobs}.rs    release/refund engine + job loop
  admin/{mod,routes}.rs           backstop queue, manual verify, manual refund
server/tests/
  money.rs settings.rs payout_accounts.rs cart.rs checkout.rs webhooks.rs
  orders.rs escrow.rs mono_adapter.rs carry_forward.rs
```

---

### Task 0: Carry-forward fixes from plan 1

**Files:**
- Create: `server/migrations/0007_carry_forward.sql`, `server/tests/carry_forward.rs`
- Modify: `src/auth/otp.rs`, `src/access_codes/service.rs`, `src/stores/service.rs`, `src/products/service.rs`, `src/main.rs`

**Interfaces:**
- Produces:
  - Every `users` row lock becomes `FOR NO KEY UPDATE`.
  - `PUT /products/{id}/variants` upserts by `attributes`: existing variants keep their id, new ones are inserted, and missing ones are deleted only if no `order_items` reference them, otherwise 409 `variant_in_use`. The `order_items` check activates once Task 4's table exists; until then the delete path is unconditional. Task 4 adds that test.
  - Caps from Global Constraints are enforced in validation and as DB CHECKs.
  - `server serve` stops gracefully on Ctrl-C/SIGTERM (`axum::serve(...).with_graceful_shutdown`).

- [ ] **Step 1: Write failing tests** in `tests/carry_forward.rs`:
  - `variant_replace_keeps_ids_for_same_attributes`: replace `[M, L]` with `[M (stock 9), XL]`. M keeps its id with stock 9, L is gone, XL is new.
  - `price_above_cap_rejected`: `base_price_kobo` 1,000,000,001 → 422 `validation_failed`.
  - `stock_above_cap_rejected`: stock 100,001 → 422.
- [ ] **Step 2: Run** `cargo test --test carry_forward` → FAIL.
- [ ] **Step 3: Implement** the migration (CHECK constraints), the upsert, the caps, the lock changes and graceful shutdown.
- [ ] **Step 4: Run** the full `cargo test` → PASS. Run `cargo clippy --all-targets -- -D warnings` → clean.
- [ ] **Step 5: Commit** `fix: carry-forward items from plan 1 (locks, caps, variant upsert, shutdown)`

---

### Task 1: Money helpers

**Files:** Create `server/src/money.rs` (unit tests inline).

**Interfaces — Produces:**
- `pub fn checked_sum(items: &[i64]) -> Result<i64, AppError>` returns `Unprocessable("amount_too_large")` on overflow.
- `pub fn cut_kobo(items_total: i64, bps: i32) -> i64` rounds half-up with i128 maths.
- `pub fn payout_fee_kobo(payout_before_fee: i64, s: &FeeSchedule) -> i64`
- `pub struct FeeSchedule { pub small_fee: i64, pub large_fee: i64, pub threshold: i64 }`, with `Default` = 3,500 / 5,000 / 5,000,000.
- `pub fn split_proportional(total: i64, weights: &[i64]) -> Vec<i64>` splits by largest remainder; the parts always sum to `total`.

- [ ] **Step 1: Failing tests:**
  - `cut_kobo(1_000_000, 500) == 50_000`
  - `cut_kobo(1_001, 500) == 50` (50.05 rounds down)
  - `cut_kobo(1_010, 500) == 51` (50.5 rounds up)
  - `payout_fee_kobo(5_000_000) == 3_500`, `payout_fee_kobo(5_000_001) == 5_000`
  - `split_proportional(100, &[1,1,1]) == [34,33,33]`, and its sum is 100
  - `checked_sum(&[i64::MAX, 1])` is an error
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: add kobo money helpers`

---

### Task 2: Platform settings

**Files:** `migrations/0008_platform_settings.sql`, `src/settings/*`, `tests/settings.rs`

**Interfaces — Produces:**
- A single-row table `platform_settings` with columns:
  - `platform_cut_bps int not null default 500 check (between 0 and 3000)`
  - `fee_small_kobo bigint default 3500`, `fee_large_kobo bigint default 5000`, `fee_threshold_kobo bigint default 5000000`
  - `enabled_methods text[] not null default '{mono_directpay}'`
  - `payout_provider text not null default 'mono'`
  - `checkout_ttl_minutes int default 30`, `ship_deadline_hours int default 48`, `release_after_hours int default 72`, `backstop_days int default 30`
  - `updated_at`
- `pub async fn load(db) -> Result<Settings, AppError>`
- Routes:
  - `GET /admin/settings` (owner or support)
  - `PATCH /admin/settings` (owner only), audit `settings.updated` with the field names changed
- Validation: every `enabled_methods` entry must be registered in the provider registry (Task 5). The list must not be empty.

- [ ] **Step 1: Failing tests:**
  - `defaults_are_spec_values`: bps 500, methods `["mono_directpay"]`, payout `"mono"`
  - `owner_updates_cut`: 450 → GET shows 450, audit row present
  - `support_cannot_patch`: 403
  - `cut_out_of_range_rejected`: 3001 → 422
  - `unknown_method_rejected`: `["paystack_card"]` → 422 `unknown_payment_method` (Paystack isn't registered yet)
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: add owner-controlled platform settings`

**Research amendments (binding; they override the text above where they differ):**
- Task 1 extended `FeeSchedule` with `stamp_duty_kobo` and `stamp_duty_threshold` (the ₦50 stamp duty on transfers ≥ ₦10,000). Add the columns `stamp_duty_kobo bigint default 5000` and `stamp_duty_threshold_kobo bigint default 1000000`, and let the owner PATCH them.
  - All fee columns must be ≥ 0.
  - Mono's published payout fee tiers contradict each other, so the fee fields must stay owner-editable.
- Add `payout_settlement_buffer_hours int default 2` (used by Task 8's `send_after`).
- Method validation uses `const KNOWN_METHODS: &[&str] = &["mono_directpay"]` until Task 5 replaces it with the registry.

---

### Task 3: Payout accounts and store activation

**Files:** `migrations/0009_payout_accounts.sql`, `src/payout_accounts/*`, `tests/payout_accounts.rs`

**Interfaces:**
- Consumes: `PaymentProvider::resolve_account` (Task 5; this task defines a minimal stub of the trait that Task 5 extends) and `FieldCipher`.
- Produces:
  - Table `payout_accounts` with columns `store_id pk fk`, `bank_code text`, `account_number_enc bytea`, `account_last4 text`, `account_name text`, `status payout_account_status ('pending','verified','rejected')`, `verified_by text ('provider'|'owner')`, `verified_at`, `updated_at`.
  - `PUT /stores/{key}/payout-account {bank_code, account_number}` (store owner only; 10-digit NUBAN; resets status to pending):
    - The service calls `resolve_account`. On `Ok(name)` the status becomes `verified` and `verified_by = 'provider'`. On `Err(NotSupported)` the account stays `pending` for owner review.
  - `GET /stores/{key}/payout-account` returns `{bank_code, last4, account_name, status}` and never the full number.
  - `POST /admin/stores/{id}/payout-account/verify {account_name}` (owner only) sets `verified`, `verified_by = 'owner'`, with audit `payout_account.verified`.
  - **Store activation:** a store becomes `active` when its payout account is `verified` and it is not suspended. Changing the account sets the store back to `pending` until it is verified again.
- **Ruling:** Mono's account-name-lookup endpoint isn't documented publicly yet. `MonoProvider::resolve_account` returns `Err(NotSupported)` until it is confirmed, so the owner verifies manually in the meantime.

- [ ] **Step 1: Failing tests:**
  - `provider_verified_account_activates_store`: FakeProvider resolves "ADA OBI" → status verified, store active, public GET of the store → 200
  - `unsupported_lookup_leaves_pending`: FakeProvider set to NotSupported → pending, store pending
  - `owner_manual_verify_activates`
  - `changing_account_deactivates_store`
  - `account_number_encrypted_and_masked`: the raw column doesn't contain the digits, and GET shows `last4` only
  - `invalid_nuban_rejected`: 9 digits → 422
  - `non_owner_cannot_set`: → 404
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: add seller payout accounts and store activation`

---

### Task 4: Cart and checkout (stock reservation, one order per store)

**Files:** `migrations/0010_cart_checkout_orders.sql`, `src/cart/*`, `src/checkout/*`, `tests/cart.rs`, `tests/checkout.rs`

**Interfaces — Produces:**
- **Tables:**
  - `cart_items(user_id, variant_id, quantity int check 1..100, pk(user_id, variant_id))`
  - `checkouts(id uuid pk, buyer_id, reference text unique /* "chk" + uuid simple: letters and digits only, in case the provider rejects symbols */, method text, provider text, total_kobo bigint, status checkout_status ('pending','paid','expired','amount_mismatch','refunded'), provider_payment_id text, collection_fee_kobo bigint, expires_at, paid_at, created_at)`
  - `orders(id uuid pk, checkout_id fk, store_id fk, buyer_id fk, status order_status ('pending_payment','paid','picked_up','delivered','completed','cancelled','refund_pending','refunded'), items_total_kobo, delivery_fee_kobo default 0, platform_cut_bps int, platform_cut_kobo, collection_fee_share_kobo default 0, paid_at, picked_up_at, delivered_at, completed_at, cancelled_at, backstop_flagged_at, created_at)`
  - `order_items(order_id, variant_id fk, product_name text, attributes jsonb, unit_price_kobo, quantity)`, snapshotted at checkout
- **Routes:**
  - Cart: `GET /cart`, `PUT /cart/items {variant_id, quantity}` (0 removes the line), `DELETE /cart`
  - `POST /checkout {method}` → 201 `{checkout_id, reference, payment_url, total_kobo, orders:[{order_id, store_name, items_total_kobo}]}`
  - `GET /checkout/{id}` (buyer only)
- **Checkout service** (one transaction, in the global lock order):
  1. Load the cart and lock the variant rows `FOR UPDATE`.
  2. Every product must be published and its store active, otherwise 422 `item_unavailable`.
  3. Stock must cover the quantity, otherwise 409 `out_of_stock` naming the variant id.
  4. Decrement stock (this is the reservation).
  5. Group by store into orders, snapshot the cut bps from settings, and compute totals with `money`.
  6. Create the checkout with `expires_at = now + ttl`.
  7. **After commit**, call `provider.initiate(...)` (Task 5) to get `payment_url`. If that fails, expire the checkout immediately, restore stock, and return 502 `payment_provider_unavailable`.
  8. The cart is cleared only when the checkout is paid (Task 6).
- The method must be in `settings.enabled_methods`, otherwise 422 `payment_method_disabled`.
- A buyer can't check out their own store's items: 422 `own_store_item`.

- [ ] **Step 1: Failing tests:**
  - `cart_add_update_remove`
  - `checkout_splits_orders_per_store`: items from 2 stores → 2 orders; each total is correct; cut is 5% half-up; checkout total = sum of orders
  - `checkout_reserves_stock`: stock 5, qty 2 → 3
  - `concurrent_last_unit_one_wins`: stock 1, two buyers race with `tokio::join!` → one 201, one 409 `out_of_stock`, final stock 0
  - `unpublished_or_pending_store_item_unavailable`: → 422 `item_unavailable`
  - `disabled_method_rejected`
  - `own_store_item_rejected`
  - `provider_failure_restores_stock`: FakeProvider set to fail initiate → 502, stock restored, checkout status expired
  - `variant_in_order_cannot_be_deleted`: the Task 0 follow-up → 409 `variant_in_use`
  - `quantity_cap`: 101 → 422
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: add cart and checkout with stock reservation`

**Research amendments (binding; they override the text above where they differ):**
- **References are 24 characters or fewer, letters and digits only** (Mono changelog, 8 Oct 2026).
  - A checkout reference is `"CK"` followed by 20 random uppercase letters and digits from `OsRng`. It has a unique index, and a collision retries generation.
  - Add a shared helper `payments::new_reference(prefix: &str) -> String`, used again for payouts (`"PO"`) and refunds (`"RF"`).
  - Because Task 5 is built before this task, put the helper in `src/payments/mod.rs` here if Task 5 did not already add it.
- **Variant locks are always taken ordered by variant id** (`... WHERE id = ANY($1) ORDER BY id FOR UPDATE`), so two checkouts never deadlock.
- `orders` gains `held_at timestamptz null` and `hold_reason text null`. A held order never auto-releases. Plan 5 sets these from complaints; Task 10 lets the owner set and clear them.
- `checkouts` gains `provider_flagged boolean not null default false` (set in Task 6).
- Wire the Task 0 caps: line quantity ≤ `limits::MAX_LINE_QUANTITY` (422), and checkout total ≤ `limits::MAX_CHECKOUT_TOTAL_KOBO` (422 `amount_too_large`). Each gets a cap+1 test.

---

### Task 5: PaymentProvider trait, registry, FakeProvider

**Files:** `src/payments/mod.rs`, `src/payments/fake.rs`; modify `src/lib.rs` (AppState gets `providers: Arc<ProviderRegistry>`)

**Interfaces — Produces:**

```rust
#[async_trait]
pub trait PaymentProvider: Send + Sync {
    fn name(&self) -> &'static str;                       // "mono", "fake"
    fn methods(&self) -> &'static [&'static str];         // ["mono_directpay"]
    async fn initiate(&self, req: InitiatePayment) -> Result<InitiatedPayment, ProviderError>;
    async fn verify_payment(&self, reference: &str) -> Result<PaymentStatus, ProviderError>;
    async fn create_payout(&self, req: PayoutRequest) -> Result<PayoutHandle, ProviderError>;
    async fn payout_status(&self, provider_payout_id: &str) -> Result<PayoutState, ProviderError>;
    async fn refund(&self, req: RefundRequest) -> Result<RefundHandle, ProviderError>;
    async fn resolve_account(&self, bank_code: &str, account_number: &str) -> Result<String, ProviderError>;
    fn verify_webhook(&self, headers: &HeaderMap, body: &[u8]) -> Result<WebhookEvent, ProviderError>;
}
pub enum ProviderError { NotSupported, Rejected(String), Unavailable(anyhow::Error) }
pub struct InitiatePayment { pub reference: String, pub amount_kobo: i64, pub description: String, pub customer_name: String, pub customer_email: Option<String>, pub redirect_url: String }
pub struct InitiatedPayment { pub provider_payment_id: String, pub payment_url: String }
pub enum PaymentStatus { Pending, Successful { amount_kobo: i64, fee_kobo: i64 }, Failed }
pub struct PayoutRequest { pub reference: String, pub amount_kobo: i64, pub bank_code: String, pub account_number: String, pub narration: String }
pub struct PayoutHandle { pub provider_payout_id: String }
pub enum PayoutState { Pending, Successful { fee_kobo: i64 }, Failed { reason: String } }
pub struct RefundRequest { pub payment_reference: String, pub amount_kobo: i64, pub reason: String }
pub struct RefundHandle { pub provider_refund_id: String }
pub enum WebhookEvent {
    Payment { event_id: String, reference: String },
    Payout { event_id: String, provider_payout_id: String },
    Ignored { event_id: String },
}
pub struct ProviderRegistry { /* by name; method → provider */ }
impl ProviderRegistry {
    pub fn for_method(&self, method: &str) -> Option<Arc<dyn PaymentProvider>>;
    pub fn by_name(&self, name: &str) -> Option<Arc<dyn PaymentProvider>>;
    pub fn is_registered_method(&self, m: &str) -> bool;
}
```

- `FakeProvider` is a programmable test double. It records calls, and test code sets the outcome of each call (`set_payment(reference, PaymentStatus)`, `fail_initiate()`, `set_lookup(Result)`, `fail_payout()`, and so on). It handles method `"mono_directpay"` in tests, so the test registry mirrors production. Its webhook secret is `"test-secret"`.
- `TestApp::new` registers a FakeProvider and exposes it as `app.fake`.

- [ ] **Step 1:** `ProviderRegistry` unit tests: method → provider lookup, unknown method → None.
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: add payment provider layer and fake provider`

**Research amendments to the trait (binding; they override the code above where they differ):**
- `PaymentStatus::Successful { amount_kobo, fee_kobo, currency: String, held: bool }`
  - `held` is true when the provider flags the payment or holds its settlement.
  - Mono's verify returns `flagged` / `held_settlement`.
- Payouts are identified by **our reference**, because Mono's Disburse webhooks carry no id.
  - `payout_status(&self, reference: &str, provider_payout_id: Option<&str>) -> Result<PayoutState, ProviderError>`
  - `find_payout(&self, reference: &str) -> Result<Option<PayoutHandle>, ProviderError>` looks up a payout that may exist after a timeout.
  - `WebhookEvent::Payout { event_id, reference }`
- `ProviderError` gains `Timeout`. An unknown outcome is different from `Unavailable`, which means the call definitely did not happen: a connect failure or a 5xx before sending.
- `pub fn new_reference(prefix: &str) -> String`: the prefix plus 20 random uppercase letters and digits, 24 characters or fewer in total.
- FakeProvider supports: `set_payment(ref, status)`, `fail_initiate()`, `set_lookup(result)`, `fail_payout_with(ProviderError)`, `set_payout_state(ref, state)`, `set_found_payout(ref, Option<id>)`, `verify_unavailable_n(n)` (the next n verify calls return Unavailable), and call counters.

*(Build order note: Task 5's trait is needed by Tasks 3 and 4. Execute Task 5 before Task 3. The numbering follows the spec order, not the build order. See Build Order at the end.)*

---

### Task 6: Webhooks and marking checkouts paid

**Files:** `migrations/0011_ledger.sql`, `migrations/0012_payments_payouts.sql`, `src/payments/webhooks.rs`, `src/ledger.rs`, `src/checkout/service.rs` (`settle_payment`), `tests/webhooks.rs`

**Interfaces — Produces:**
- Table `provider_events(provider text, event_id text, received_at, pk(provider, event_id))` for dedupe.
- Table `ledger_entries(id bigserial, at, order_id uuid null, checkout_id uuid null, account ledger_account ('buyer_funds','escrow','platform_revenue','provider_fees','seller_payable','refunds'), amount_kobo bigint /* signed */, kind text, ref text)` with append-only triggers.
- `pub async fn ledger::post(tx, entries: &[Entry]) -> Result<(), AppError>` asserts that the entries sum to 0 before inserting.
- `POST /webhooks/{provider}`, unauthenticated:
  1. `verify_webhook` → 401 `invalid_signature` on failure.
  2. Insert into `provider_events`. A conflict means a duplicate → 200 `{"status":"duplicate"}`.
  3. `Payment` → `checkout::settle_payment(reference)`. `Payout` → `escrow::settle_payout` (Task 8).
- `pub async fn settle_payment(state, reference) -> Result<SettleOutcome, AppError>`:
  1. Call `verify_payment(reference)`. Only `Successful` continues.
  2. Lock the checkout. If it's already `paid` → no-op.
  3. If `amount_kobo != total_kobo` → set `amount_mismatch`, audit, stop.
  4. If the checkout is `expired`, try to re-reserve stock. If stock is short → status `refunded`, call `provider.refund`, record refund rows.
  5. Otherwise mark the checkout `paid` and every order `paid` with `paid_at = now`.
  6. Split `collection_fee` across orders with `split_proportional` by order total.
  7. Post ledger entries per order, where `T = items_total + delivery_fee`: `buyer_funds −T`, `escrow +T`. Fee entries are posted at release (Task 8).
  8. Clear the buyer's cart lines for those variants.
- A `GET /checkout/{id}` poll also calls `settle_payment` when the status is pending. That's the fallback if a webhook is lost.

- [ ] **Step 1: Failing tests:**
  - `valid_webhook_marks_paid`: orders paid, ledger sums to 0, cart cleared
  - `duplicate_event_applies_once`: same event_id twice → second returns duplicate, one set of ledger rows
  - `wrong_secret_is_401_no_change`
  - `provider_not_confirming_changes_nothing`: webhook OK, but FakeProvider verify returns Pending → still pending
  - `amount_mismatch_flagged`: verify returns 999 vs a total of 1000 → `amount_mismatch`, orders stay `pending_payment`, audit row present
  - `paid_after_expiry_rereserves_when_stock_allows`
  - `paid_after_expiry_refunds_when_stock_gone`: refund recorded, checkout refunded
  - `poll_settles_without_webhook`
  - `ledger_is_append_only`
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: settle payments from verified webhooks into escrow`

**Research amendments (binding; they override the text above where they differ):**
- **Never lose an event.** Mono retries a non-200 up to 25 times over 48h with the same `event_id`.
  - `provider_events` gains `processed_at timestamptz null`.
  - On receipt, insert the row if it is missing. If it already exists **with** `processed_at`, answer 200 duplicate.
  - Otherwise settle. Set `processed_at` in the **same transaction** as the state change, or after a confirmed no-op.
  - If the verify call is unavailable or times out, answer **503** so Mono retries. Test: `verify_unavailable_then_retry_applies`.
- **Lock order.** `settle_payment` first reads the checkout without a lock to get its variant ids. In one transaction it then locks the variants (ordered by id), then the checkout `FOR UPDATE`, then the orders. It re-checks status after locking.
- Verify must also confirm `currency == "NGN"` and that the reference matches ours. A mismatch is treated as `amount_mismatch`.
- `held: true` still marks the checkout paid, but sets `checkouts.provider_flagged = true` with an audit row. Task 8 will not pay out a flagged checkout until the owner clears it (Task 10).
- **Refunds:** this task builds `escrow::refund::refund_order(state, order_id, restore_stock: bool)`, which Task 8 reuses.
  - The refund is items + delivery, with no cut. It gets a refund row with reference `new_reference("RF")` and calls `provider.refund`. `NotSupported` sets the refund status to `manual`.
  - Order → `refunded`. Ledger: `escrow −T`, `refunds +T`.
  - The paid-after-expiry path uses `restore_stock = false`.
- **Ledger accounts:** `buyer_funds`, `escrow`, `platform_revenue`, `provider_fees`, `seller_payable`, `seller_paid`, `refunds`.
  - Every `ledger::post` asserts a zero sum.
  - Signs: an inflow to the platform's books is positive on the internal account and negative on the external one.
- Migration 0012 creates `provider_events`, `payouts`, `refunds` and `seller_strikes`, so Tasks 8 and 9 add no migration. `payouts` gets the extra columns from the Task 8 amendment.

---

### Task 7: Order views, cancellation, delivery hooks

**Files:** `src/orders/*`, `tests/orders.rs`

**Interfaces — Produces:**
- Routes:
  - `GET /orders` (buyer's orders)
  - `GET /orders/{id}` (buyer, or the store's owner)
  - `GET /stores/{key}/orders` (store owner; includes buyer display name only, never phone or address until plan 3)
  - `POST /orders/{id}/cancel` (buyer; allowed only while `paid`, i.e. before pickup) → status `refund_pending` → refund engine (Task 8)
  - `POST /orders/{id}/received` (buyer; allowed when `picked_up` or `delivered`) → release now (Task 8)
- Service functions for plan 3 (no HTTP route yet):
  - `pub async fn mark_picked_up(db, order_id, at: OffsetDateTime)`: allowed `paid → picked_up`
  - `pub async fn mark_delivered(db, order_id, at)`: allowed `picked_up → delivered`
- Invalid transitions return 409 `invalid_order_state`.

- [ ] **Step 1: Failing tests:**
  - `buyer_sees_own_orders_only`
  - `seller_sees_store_orders_without_contact_details`
  - `cancel_before_pickup_starts_refund`
  - `cancel_after_pickup_rejected`: → 409
  - `received_triggers_release`: the payout is created (via FakeProvider)
  - `invalid_transition_rejected`
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: add order views, buyer cancel and received`

---

### Task 8: Release and refund engine

**Files:** `src/escrow/release.rs`, `tests/escrow.rs`

**Interfaces — Produces:**
- Table `payouts(id uuid, order_id uuid unique, reference text unique /* "po" + order uuid simple */, provider text, provider_payout_id text, amount_kobo, fee_kobo, status payout_status ('pending','successful','failed','manual'), attempts int, last_error text, created_at, updated_at)`
- Table `refunds(id, order_id unique, amount_kobo, provider_refund_id, status ('pending','successful','manual'), ...)`
- `pub async fn release_order(state, order_id, reason: ReleaseReason) -> Result<(), AppError>`, with `ReleaseReason::{BuyerConfirmed, AutoAfterDelivery, OwnerDecision}`:
  1. Lock the order. It must be `picked_up` or `delivered` and have no payout row; otherwise it's a no-op (idempotent).
  2. Load the payout account; it must be verified, otherwise payout status `manual`.
  3. `amount = items_total − cut − collection_fee_share − payout_fee`. If that's ≤ 0, use status `manual`.
  4. Post ledger entries: `escrow −(items + delivery)`, `platform_revenue +(cut + delivery)`, `provider_fees +(collection_fee_share + payout_fee)`, `seller_payable +payout`. These sum to zero. Delivery money sits in `platform_revenue` until plan 3 routes it to the courier or the self-delivering seller.
  5. Insert the payout row as `pending`, then commit.
  6. After commit, call `provider.create_payout` with the payout reference. On error the payout stays `pending` with `last_error`, and the job retries it.
  7. Set the order to `completed`.
- `pub async fn refund_order(state, order_id)`:
  - Refund amount = items + delivery. No cut is taken, and the collection fee is absorbed by the platform (the "fault" rules wait for plan 5 disputes).
  - Restore stock, post ledger entries, call `provider.refund`. `NotSupported` → refund status `manual` for the owner.
  - Order → `refunded`.
- `pub async fn settle_payout(state, provider_payout_id)`: calls `payout_status`, then updates the payout to `successful` (recording the actual fee) or `failed`.

- [ ] **Step 1: Failing tests:**
  - `release_amount_matches_formula`: items 1,000,000, cut 50,000, fee share 1,500, payout fee 3,500 → payout 945,000; ledger sums to 0 per order
  - `concurrent_received_and_job_one_payout`: `tokio::join!(received, release job)` → exactly 1 payout row and 1 provider call
  - `unverified_account_goes_manual`
  - `provider_payout_error_stays_pending_with_error`
  - `refund_restores_stock_and_no_cut`
  - `refund_not_supported_goes_manual`
  - `payout_webhook_settles_status`
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: add escrow release and refund engine`

**Research amendments (binding; they override the text above where they differ):**
- This task also creates `orders/service.rs` with `mark_picked_up` and `mark_delivered`, which Task 7 then exposes. The concurrency test calls `release_order` twice concurrently; Task 9 adds the received-vs-job race test.
- **`payouts` columns:**
  - `status payout_status ('scheduled','sending','pending','unknown','successful','failed','manual')`
  - `send_after timestamptz`, `next_attempt_at timestamptz`, `attempts int`, `last_error text`
  - `reference` = `new_reference("PO")` (24 characters or fewer, unique). The reference is **never re-minted** for the same payout.
- **Payout timing.** Mono settles the next day at 7 PM WAT; Friday–Sunday payments settle Monday.
  - `send_after = max(now, settlement_time(checkout.paid_at) + payout_settlement_buffer_hours)`.
  - `pub fn settlement_time(paid_at) -> OffsetDateTime` gives the next working day at 19:00 Africa/Lagos (UTC+1, no DST). Friday, Saturday and Sunday map to Monday. Public holidays are not modelled.
  - `release_order` completes the order and posts the ledger entries immediately, but inserts the payout as `scheduled`. Sending happens in `send_payout(state, payout_id)`, called inline when `send_after <= now`, and by the Task 9 job otherwise.
  - Unit tests on `settlement_time` cover Monday 10:00, Thursday 20:00, Friday 10:00 and Sunday 23:00.
- **Sending (`send_payout`) never double-pays:**
  1. Claim the row: `scheduled|unknown → sending` in its own short transaction.
  2. If the status was `unknown`, call `find_payout(reference)` first. If the payout is found, adopt its id → `pending`.
  3. Otherwise call `create_payout` with the **same** reference. `Ok` → `pending` with the id. `Rejected` → `failed`. `Unavailable` → `scheduled` with `next_attempt_at` set by backoff. `Timeout` → `unknown`.
- **Do not send** while the order is held or its checkout is `provider_flagged`. The payout stays `scheduled`.
- **Release ledger** (at release): `escrow −(items + delivery)`, `platform_revenue +(cut + delivery)`, `provider_fees +collection_fee_share`, `platform_revenue +payout_fee_charged`, `seller_payable +payout`.
  - The payout fee charged to the seller is platform revenue; the real provider fee is booked when it is known.
- **On payout success** (`settle_payout`): `seller_payable −payout`, `seller_paid +payout`. If the provider reports an actual fee: `provider_fees +actual_fee`, `platform_revenue −actual_fee`.
- **Payout failure after success** (a reversal): post the reverse of the success entries and set the payout to `failed`. It then appears in the owner's failed list.
- **Refund after release** is out of scope for plan 2. `refund_order` on a `completed` order returns 409 `already_released`; plan 5 adds clawback with a `seller_receivable` account.
- Tests added: `payout_waits_for_settlement_time`, `timeout_then_lookup_adopts_existing_payout`, `held_order_not_paid_out`, `flagged_checkout_not_paid_out`, `payout_success_moves_payable_to_paid`, `refund_after_release_rejected`.

---

### Task 9: Background jobs

**Files:** `src/escrow/jobs.rs`, `tests/escrow.rs` (job tests); modify `src/main.rs` (spawn the loop, stop on shutdown)

**Interfaces — Produces:**
- `pub async fn run_due_jobs(state: &AppState, now: OffsetDateTime) -> Result<JobReport, AppError>` runs, in order:
  1. Expire checkouts that are `pending` with `expires_at < now`: restore stock, then status `expired`.
  2. Ship deadline: orders `paid` with `paid_at + ship_deadline_hours < now` → `refund_order` plus an insert into `seller_strikes(store_id, order_id, reason 'ship_deadline', at)`.
  3. Auto-release: orders `delivered` with `delivered_at + release_after_hours < now` → `release_order(AutoAfterDelivery)`.
  4. Backstop: orders not `completed/refunded/cancelled` with `paid_at + backstop_days < now` and `backstop_flagged_at IS NULL` → set `backstop_flagged_at`, audit `order.backstop_flagged`.
  5. Payout retry: `pending` payouts with no `provider_payout_id` and `attempts < 5` → retry. At 5 attempts → `manual`.
- Each job selects rows `FOR UPDATE SKIP LOCKED LIMIT 100`, so several server instances don't double-process.
- `main.rs` runs `run_due_jobs(now())` every 60 seconds and stops on graceful shutdown.
- `pub struct JobReport { pub expired: u32, pub auto_cancelled: u32, pub released: u32, pub flagged: u32, pub payouts_retried: u32 }`

- [ ] **Step 1: Failing tests** (each one passes an explicit `now`):
  - `checkout_expires_after_30_min_restores_stock`: 29 min → untouched; 31 min → expired
  - `ship_deadline_48h_refunds_and_strikes`: 47h → untouched; 49h → refund + strike
  - `release_72h_after_delivered`: 71h → untouched; 73h → payout
  - `no_release_without_delivered_at`: picked_up, 10 days → untouched
  - `backstop_flags_once_at_30_days`
  - `payout_retry_then_manual_after_5`
  - `job_rerun_is_idempotent`: running twice at the same `now` gives the same state
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: add escrow background jobs`

**Research amendments (binding; they override the text above where they differ):**
- **Claim, then act.** Each job selects due ids with `FOR UPDATE SKIP LOCKED LIMIT 100` in a short transaction, which only proves no one else is working on them, then commits. It then calls the engine function per id. Engine functions take their own locks and re-check state, so they are idempotent.
  - Never call an engine function while the job's own lock is open on the same row.
- Set `idle_in_transaction_session_timeout = '60s'` for the pool's connections (sqlx `after_connect`).
- **Payout job** (replaces job 5) processes these payouts:
  - `scheduled` with `send_after <= now` and `next_attempt_at` null or `<= now`
  - `unknown`, which must go through the lookup first
  - `pending` older than 1 hour, which gets a `payout_status` poll
  - Stuck `sending` older than 10 minutes moves to `unknown`.
  - Backoff is `next_attempt_at = now + min(2^attempts minutes, 6h) ± 20% jitter`. After 8 attempts → `manual`.
  - A definite `failed` → `manual` in the same job, with an audit row.
- Auto-release skips held orders and orders whose checkout is `provider_flagged`.
- Test added: `received_and_job_race_one_payout`, which runs the T7 received route and `run_due_jobs` concurrently and asserts one payout row and one provider call.

---

### Task 10: Owner admin endpoints

**Files:** `src/admin/*`, `tests/escrow.rs` or a new `tests/admin.rs`

**Interfaces — Produces** (all owner only except where noted; every action is audited):
- `GET /admin/orders/backstop`: flagged orders (owner and support)
- `POST /admin/orders/{id}/release` → `release_order(OwnerDecision)`
- `POST /admin/orders/{id}/refund` → `refund_order`
- `GET /admin/payouts?status=manual|failed`
- `POST /admin/payouts/{id}/mark-paid {note}`: for payouts done by hand
- `POST /admin/refunds/{id}/mark-refunded {note}`
- `GET /admin/checkouts?status=amount_mismatch`

- [ ] **Step 1: Failing tests:**
  - `owner_releases_backstop_order`
  - `support_can_view_but_not_release`: release → 403
  - `manual_payout_mark_paid_audited`
  - `mismatch_list_shows_flagged`
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: add owner escrow admin endpoints`

**Research amendments (binding; they override the text above where they differ):**
- `POST /admin/orders/{id}/hold {reason}` and `POST /admin/orders/{id}/unhold` (owner only, audited).
- `POST /admin/checkouts/{id}/clear-flag` (owner only, audited): clears `provider_flagged` after the owner checks the payment with Mono.
- `GET /admin/reconciliation` (owner and support): lists `unknown` / `manual` / `failed` payouts, `manual` refunds, flagged checkouts and `amount_mismatch` checkouts.
- Daily automated reconciliation against Mono statements is deferred to plan 6 (owner dashboard). The roadmap notes it.

---

### Task 11: MonoProvider adapter

**Files:** `src/payments/mono.rs`, `tests/mono_adapter.rs`; modify `src/config.rs` (`MONO_SECRET_KEY`, `MONO_WEBHOOK_SECRET`, `MONO_DISBURSE_SOURCE_ACCOUNT`, `MONO_BASE_URL` defaulting to `https://api.withmono.com`, `MONO_PAYOUT_RECIPIENT_EMAIL`, `PUBLIC_REDIRECT_URL`) and `src/main.rs` (register Mono when its keys are set)

**Interfaces — Produces:** `MonoProvider::new(cfg) -> Self` implementing `PaymentProvider`:
- `name()` = `"mono"`, `methods()` = `["mono_directpay"]`
- `initiate` → `POST /v2/payments/initiate` with header `mono-sec-key` and body `{amount, type:"onetime-debit", method:"account", description, reference, redirect_url, customer:{name, email}}`. Map `data.id` → `provider_payment_id` and `data.mono_url` → `payment_url`.
- `verify_payment` → `GET /v2/payments/verify/{reference}`. Map `status` (`successful` | `failed` | other → Pending), `amount` and `fee`. A 404 → `Pending`.
- `create_payout` → `POST /v3/payments/disburse/disbursements` with `{reference, source:"mandate", account: <source account>, type:"instant", total_amount, description, distribution:[{reference, recipient_email: MONO_PAYOUT_RECIPIENT_EMAIL, account:{account_number, bank_code}, amount, narration}]}`. Map `data.id`.
- `payout_status` → `GET /v3/payments/disburse/disbursements/{id}`. Map `successful`/`completed` → Successful (fee from `total_fee` if present, else 0), `failed` → Failed, anything else → Pending.
- `refund` and `resolve_account` → `Err(NotSupported)` (ruling: endpoints not confirmed; owner handles these by hand).
- `verify_webhook`:
  - Constant-time compare of the `mono-webhook-secret` header against the configured secret.
  - Parse `event` and `event_id`. `direct_debit.payment_*` → `Payment{reference: data.object.reference}`. `event.disbursement.*` → `Payout{provider_payout_id: data.id ?? data.object.id}`. Anything else → `Ignored`.
- Amount conversion lives in one place (`to_mono_amount`/`from_mono_amount`), controlled by `MONO_AMOUNT_UNIT_IS_KOBO`.
- The `reqwest` client has a 10s timeout. Secrets never appear in `Debug` or logs.

- [ ] **Step 1: Failing tests** with `wiremock`:
  - `initiate_sends_expected_body_and_headers`: asserts the path, `mono-sec-key` and the JSON body; returns `mono_url`
  - `verify_maps_successful_with_amount_and_fee`
  - `verify_404_is_pending`
  - `create_payout_body_shape`
  - `payout_status_mapping`
  - `webhook_wrong_secret_rejected`
  - `webhook_parses_payment_and_disbursement_events`
  - `secret_not_in_debug_output`
  - `sandbox_smoke_test`: `#[ignore]`, run by hand with real test keys (`cargo test --test mono_adapter -- --ignored`). It initiates a ₦100 payment and checks the returned amount echo, which **confirms the amount unit** before going live.
- [ ] **Steps 2–5:** run → FAIL, implement, run → PASS, then commit `feat: add Mono DirectPay and Disburse adapter`

**Research amendments (binding; they override the text above where they differ):**
- **Disburse webhooks have no `data.id`.**
  - Disbursement-level events carry `data.reference`. Transaction events carry `data.disbursement_reference` (the batch) and `data.reference` (the distribution).
  - We set the batch reference and the single distribution reference to the **same** payout reference, so either one maps to `WebhookEvent::Payout { reference }`.
- **Outcome comes from the distribution's own status.** A batch `completed` event can still carry a failed distribution.
- `find_payout(reference)` uses Mono's "Fetch All Disbursements" list filtered by reference, if the API supports that filter; otherwise it scans the recent page. Mark the exact query shape **unconfirmed** in a code comment.
- `verify_payment` maps `flagged` / `held_settlement` to `held: true`, and passes `currency` through.
- Timeouts (reqwest `is_timeout()`) map to `ProviderError::Timeout`. Connect errors map to `Unavailable`. A 4xx maps to `Rejected`.
- **Refunds:** Mono has a refund API (v2) and dashboard refunds (₦15 each), but the request shape is unconfirmed. Keep `NotSupported` → manual, and leave a code comment pointing at the v2 refund endpoint to wire up once confirmed.
- **The smoke test can't prove the amount unit from an echo.** It initiates ₦100 (10,000 if kobo, or 100 if naira) and prints the hosted `mono_url`. A person opens it and checks that the page shows ₦100.00, then records the result in `MONO_AMOUNT_UNIT_IS_KOBO`'s doc comment.

---

### Task 12: Full-suite check

- [ ] Postgres is up (`pg_isready -h localhost -p 5433`). Full `cargo test` → all pass. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` → clean.
- [ ] End-to-end with FakeProvider in one test (`tests/escrow.rs::happy_path_end_to_end`):
  1. Seller with a verified account; buyer carts 2 items from 2 stores.
  2. Checkout, then webhook → paid.
  3. `mark_picked_up`, then `mark_delivered`.
  4. Jobs at +73h → both payouts created.
  5. Payout webhooks → successful.
  6. Ledger per order sums to 0.
- [ ] Commit any fixes.

---

## Build Order

Tasks are numbered by topic. Execute them in this order, because of the interface dependencies:

**0 → 1 → 2 → 5 → 3 → 4 → 6 → 8 → 7 → 9 → 10 → 11 → 12**

- Task 2's method validation uses the Task 5 registry. Build Task 5 first, or validate against a constant list and switch to the registry in Task 5.
- Task 7's `received` and `cancel` call Task 8's engine.
