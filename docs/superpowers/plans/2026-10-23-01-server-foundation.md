# Server Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A tested Rust HTTP API covering accounts (phone + password, OTP verification, sessions, roles), owner-issued access codes, stores (max 2 per user), and products with variants. Every later plan builds on it.

**Architecture:** One Axum service in `server/`, PostgreSQL via sqlx with checked-in migrations, and feature folders (`auth`, `access_codes`, `stores`, `products`), each with a `service.rs` holding the business rules and a `routes.rs` holding the HTTP handlers. Personal fields are encrypted at rest. Lookups by phone use a keyed hash (blind index). Integration tests run against a real Postgres through `#[sqlx::test]`.

**Tech Stack:** Rust (stable, edition 2024), axum 0.8, tokio 1, sqlx 0.8 (postgres, uuid, time, migrate), argon2 0.5, aes-gcm 0.10, hmac 0.12 + sha2 0.10, rand 0.8, serde 1, thiserror 2, tracing + tower-http, uuid 1, time 0.3. Dev: reqwest 0.12 is not needed; tests call the router with `tower::ServiceExt::oneshot`.

**Spec:** `docs/superpowers/specs/2026-10-22-abuja-marketplace-design.md` (sections 2, 3, 4, 11 roles, 13). Roadmap: `docs/superpowers/plans/2026-10-23-00-roadmap.md`.

## Global Constraints

- Store limit: **2 stores per user**, enforced on the server.
- Opening a store requires a redeemed **access code**; codes are single-use or multi-use, with an optional usage cap and optional expiry, and can be revoked. Revoking does not affect stores already created.
- Roles: `user`, `support`, `owner`. Only `owner` manages access codes.
- Passwords hashed with **Argon2id**; never logged.
- **No PII in logs or URLs.** Phone numbers never appear in a path or query string, and request/response bodies are never logged.
- Phone numbers stored **encrypted at rest** (AES-256-GCM). Lookups use an HMAC-SHA256 blind index.
- No BVN, NIN, card or bank-account numbers are stored in this plan.
- Money is stored as integer **kobo** (`i64`); ₦1 = 100 kobo.
- Launch region is the **FCT**: a store's pickup address must have `state = "FCT"`.
- Product condition is one of `new`, `used`, `refurbished`.
- Commit messages carry no AI attribution, co-author line or session link.
- Errors are JSON: `{"error":{"code":"<snake_case>","message":"<human text>"}}`.

## Review Focus

1. **Two concurrent "create store" requests from a user who has 1 store**: exactly one succeeds, the other gets `409 store_limit_reached`. Tested in Task 5.
2. **A single-use access code redeemed by two users at the same moment**: exactly one succeeds. Tested in Task 4.
3. **Nigerian phone formats** (`08031234567`, `+2348031234567`, `2348031234567`, with spaces or dashes) all map to the same account; a second sign-up with another format gets `409 phone_taken`. Tested in Task 2.
4. **Expired, revoked or used-up codes** return `422` with codes `code_expired` / `code_revoked` / `code_exhausted`, never a 500 or a generic error. Tested in Task 4.
5. **Bad money and stock values** (price 0 or negative, stock negative, more than 10 photos, empty variant list) are rejected with `422 validation_failed`. Tested in Task 6.

---

## File Structure

```
docker-compose.yml                  Postgres 17 for development and tests (port 5433)
server/
  Cargo.toml
  .env.example                      DATABASE_URL, FIELD_KEY, INDEX_KEY, BIND_ADDR
  migrations/
    0001_users_sessions.sql
    0003_access_codes.sql
    0004_stores.sql
    0005_products.sql
    0002_audit_log.sql
  src/
    main.rs                         CLI: `serve` (default) and `create-owner`
    lib.rs                          AppState, build_app(state) -> Router
    config.rs                       Config::from_env()
    error.rs                        AppError + IntoResponse
    crypto.rs                       FieldCipher (encrypt/decrypt), blind_index()
    phone.rs                        normalize_ng_phone()
    audit.rs                        record(tx, actor, action, subject)
    auth/
      mod.rs
      password.rs                   hash_password / verify_password
      sessions.rs                   create_session / resolve_session / revoke_session
      otp.rs                        OtpSender trait, LogOtpSender (dev), issue/verify OTP
      extractor.rs                  AuthUser extractor, require_role()
      routes.rs                     /auth/* handlers
    access_codes/
      mod.rs  service.rs  routes.rs
    stores/
      mod.rs  service.rs  routes.rs
    products/
      mod.rs  service.rs  routes.rs
  tests/
    common/mod.rs                   TestApp helper: request(), sign_up_verified(), make_owner(), make_seller()
    auth.rs
    access_codes.rs
    stores.rs
    products.rs
```

---

### Task 0: Toolchain and workspace

**Files:**
- Create: `docker-compose.yml`, `server/Cargo.toml`, `server/.env.example`, `server/src/main.rs`, `server/src/lib.rs`, `server/src/config.rs`, `server/src/error.rs`, `.gitignore`
- Test: `server/tests/health.rs`

**Interfaces:**
- Produces: `pub struct AppState { pub db: PgPool, pub cipher: Arc<FieldCipher>, pub index_key: Arc<[u8; 32]>, pub otp: Arc<dyn OtpSender> }` (fields after `db` are added in Tasks 1–2); `pub fn build_app(state: AppState) -> axum::Router`; `pub enum AppError` with `IntoResponse`.

- [ ] **Step 1: Install prerequisites (owner approves each download).** Rust via rustup from https://rustup.rs (stable toolchain). Confirm with `cargo --version`. Flutter is not needed until plan 7.

- [ ] **Step 2: Start Postgres.** `docker-compose.yml` defines service `db`: image `postgres:17`, container name `market-db`, port `5433:5432`, user/password `market`/`market`, db `market`, named volume. Run `docker ps -a` first; if `market-db` exists and is stopped, `docker start market-db`; otherwise `docker compose up -d db`. If the Docker daemon is down, start Docker Desktop first.
  Check: `pg_isready -h localhost -p 5433` prints `accepting connections`.

- [ ] **Step 3: Write the failing test** `server/tests/health.rs`:

```rust
#[sqlx::test]
async fn health_returns_ok(pool: PgPool) {
    let app = common::TestApp::new(pool).await;
    let res = app.request(Method::GET, "/health", None, None).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json["status"], "ok");
}
```

`common::TestApp::new(pool)` builds `AppState` from the pool (Tasks 1–2 extend it with fixed 32-byte test keys and a `RecordingOtpSender`), and `request(method, path, body: Option<Value>, token: Option<&str>) -> TestResponse { status: u16, json: Value }`.

- [ ] **Step 4: Run it and see it fail.** `cd server; $env:DATABASE_URL="postgres://market:market@localhost:5433/market"; cargo test --test health`. Expected: compile error (no `build_app`).

- [ ] **Step 5: Implement** `build_app` with `GET /health -> {"status":"ok"}`, `tower_http::trace::TraceLayer` configured to log method, matched route template and status only (no URI query, no bodies), and `AppError` with variants `Validation(String)`, `Unauthorized`, `Forbidden`, `NotFound`, `Conflict(&'static str)`, `Unprocessable(&'static str, String)`, `Internal(anyhow::Error)` mapped to 422/401/403/404/409/422/500. `Internal` logs the error server-side and returns `{"error":{"code":"internal","message":"Something went wrong"}}`. `.gitignore` covers `target/`, `.env`.

- [ ] **Step 6: Run the test.** Same command. Expected: PASS.

- [ ] **Step 7: Commit.** `git add -A; git commit -m "chore: scaffold Rust API server with health check"`

---

### Task 1: Field encryption, blind index, phone normalization

**Files:**
- Create: `server/src/crypto.rs`, `server/src/phone.rs`
- Test: unit tests in the same files (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `FieldCipher::new(key: [u8; 32]) -> Self`; `FieldCipher::encrypt(&self, plain: &str) -> Vec<u8>` (random 12-byte nonce prepended); `FieldCipher::decrypt(&self, blob: &[u8]) -> Result<String, AppError>`; `blind_index(key: &[u8; 32], value: &str) -> Vec<u8>` (HMAC-SHA256); `normalize_ng_phone(raw: &str) -> Result<String, AppError>` returning E.164 `+234XXXXXXXXXX`.

- [ ] **Step 1: Write failing tests.**

```rust
#[test] fn round_trips() { let c = FieldCipher::new([7; 32]); let b = c.encrypt("+2348031234567"); assert_eq!(c.decrypt(&b).unwrap(), "+2348031234567"); }
#[test] fn same_plain_different_ciphertext() { let c = FieldCipher::new([7; 32]); assert_ne!(c.encrypt("x"), c.encrypt("x")); }
#[test] fn tampered_blob_fails() { let c = FieldCipher::new([7; 32]); let mut b = c.encrypt("x"); *b.last_mut().unwrap() ^= 1; assert!(c.decrypt(&b).is_err()); }
#[test] fn index_is_deterministic() { assert_eq!(blind_index(&[1; 32], "a"), blind_index(&[1; 32], "a")); assert_ne!(blind_index(&[1; 32], "a"), blind_index(&[2; 32], "a")); }

#[test] fn normalizes_formats() {
    for raw in ["08031234567", "+2348031234567", "2348031234567", "0803 123 4567", "0803-123-4567"] {
        assert_eq!(normalize_ng_phone(raw).unwrap(), "+2348031234567");
    }
}
#[test] fn rejects_bad_numbers() {
    for raw in ["", "12345", "0803123456", "080312345678", "+447911123456", "08O31234567"] {
        assert!(normalize_ng_phone(raw).is_err());
    }
}
```

- [ ] **Step 2: Run and see them fail.** `cargo test --lib crypto phone`. Expected: compile errors.

- [ ] **Step 3: Implement.** Phone rule: strip spaces and dashes; accept `0` + 10 digits, `234` + 10 digits or `+234` + 10 digits; the 10 digits must start with `7`, `8` or `9`; otherwise `AppError::Unprocessable("invalid_phone", ...)`.

- [ ] **Step 4: Run tests.** Expected: PASS.

- [ ] **Step 5: Commit.** `git commit -am "feat: add field encryption, blind index and NG phone normalization"` (add new files first).

---

### Task 2: Accounts, OTP verification and sessions

**Files:**
- Create: `server/migrations/0001_users_sessions.sql`, `server/src/auth/{mod,password,sessions,otp,extractor,routes}.rs`, `server/tests/common/mod.rs`, `server/tests/auth.rs`
- Modify: `server/src/lib.rs` (mount `/auth`), `server/src/main.rs` (`create-owner`)

**Interfaces:**
- Consumes: `FieldCipher`, `blind_index`, `normalize_ng_phone` (Task 1).
- Produces:
  - Table `users(id uuid pk, phone_enc bytea not null, phone_idx bytea unique not null, display_name text not null, password_hash text not null, role user_role not null default 'user', phone_verified_at timestamptz, is_seller boolean not null default false, created_at, deleted_at timestamptz)`; enum `user_role('user','support','owner')`.
  - Table `sessions(id uuid pk, user_id uuid fk, token_hash bytea unique not null, created_at, expires_at not null, revoked_at)`. Lifetime **30 days**. Token = 32 random bytes, base64url; only its SHA-256 is stored.
  - Table `otp_codes(id uuid pk, user_id uuid fk, code_hash bytea not null, expires_at not null, attempts int not null default 0, consumed_at)`. 6 digits, **10-minute** expiry, **5** attempts max.
  - `pub trait OtpSender: Send + Sync { async fn send(&self, phone_e164: &str, code: &str) -> Result<(), AppError>; }` (use `async_trait`). `LogOtpSender` for dev logs only "OTP sent to user <id>" (never the code or phone) and writes the code to stdout only when `OTP_DEV_ECHO=1`. A real SMS provider adapter comes in a later plan.
  - `pub struct AuthUser { pub id: Uuid, pub role: Role, pub is_seller: bool, pub phone_verified: bool }` extractor reading `Authorization: Bearer <token>`; `AuthUser::require_role(&self, role: Role) -> Result<(), AppError>` (owner satisfies every role check).
  - Routes:
    - `POST /auth/sign-up {phone, password, display_name}` → 201 `{user_id}` and sends an OTP.
    - `POST /auth/verify-phone {user_id, code}` → 200 `{token}`.
    - `POST /auth/sign-in {phone, password}` → 200 `{token}`, or 403 `phone_unverified` (and a fresh OTP is sent).
    - `POST /auth/sign-out` → 204, revokes the current session.
    - `GET /me` → `{id, display_name, phone, role, is_seller}`, with the phone decrypted for the owner of the account only.
  - Test helpers in `tests/common/mod.rs`: `RecordingOtpSender` (stores the last code per phone), `TestApp::sign_up_verified(phone) -> (Uuid, String /*token*/)`, `TestApp::make_owner() -> String`.

- [ ] **Step 1: Write failing tests** in `server/tests/auth.rs`:

```rust
sign_up_then_verify_returns_token          // 201 then 200 with non-empty token; GET /me shows phone "+2348031234567"
sign_up_stores_phone_encrypted             // SELECT phone_enc FROM users: bytes do not contain "8031234567"
duplicate_phone_any_format_conflicts       // sign up "08031234567" then "+234 803 123 4567" → 409, code "phone_taken"
short_password_rejected                    // password "short1" (<10 chars) → 422 "validation_failed"
sign_in_wrong_password_is_401              // code "invalid_credentials"; same response for unknown phone
sign_in_unverified_is_403                  // code "phone_unverified"
otp_wrong_five_times_locks                 // 5 wrong codes → each 422 "otp_invalid"; 6th with the right code → 422 "otp_locked"
otp_expired_rejected                       // set expires_at in the past via SQL → 422 "otp_expired"
sign_out_revokes_token                     // after POST /auth/sign-out, GET /me → 401
expired_session_is_401                     // UPDATE sessions SET expires_at = now() - interval '1 second' → 401
password_never_in_logs                     // tracing capture (tracing-test) during sign-up contains neither the password nor the phone
```

Every test asserts the exact status code and `json["error"]["code"]` named above.

- [ ] **Step 2: Run and see them fail.** `cargo test --test auth`. Expected: FAIL / compile errors.

- [ ] **Step 3: Write the migration and implement** password hashing (Argon2id default params, minimum length **10**), sessions, OTP and handlers. Sign-in with an unknown phone still runs a dummy Argon2 verify, so response time doesn't reveal whether an account exists.

- [ ] **Step 4: Add `create-owner`** to `main.rs`: `server create-owner --phone <phone> --name <name>` reads the password from stdin (no echo, via `rpassword`), creates a verified owner user, and prints only the new user id.

- [ ] **Step 5: Run tests.** `cargo test --test auth`. Expected: PASS (11 tests).

- [ ] **Step 6: Commit.** `git commit -m "feat: add sign-up, OTP verification, sessions and roles"`

---

### Task 3: Audit log

**Files:**
- Create: `server/migrations/0002_audit_log.sql`, `server/src/audit.rs`
- Test: `server/tests/access_codes.rs` and `stores.rs` assert audit rows (Tasks 4–5); a unit-level test here

**Interfaces:**
- Produces: table `audit_log(id bigserial pk, at timestamptz not null default now(), actor_id uuid, action text not null, subject_type text not null, subject_id uuid, detail jsonb not null default '{}')`, with a trigger that raises an exception on `UPDATE` or `DELETE` (append-only). `pub async fn record(tx: &mut PgConnection, actor: Option<Uuid>, action: &str, subject_type: &str, subject_id: Option<Uuid>, detail: serde_json::Value) -> Result<(), AppError>`. `detail` must never contain phone numbers, passwords or codes.

- [ ] **Step 1: Write failing test** `audit_log_is_append_only` (`#[sqlx::test]` in `tests/auth.rs` or a new `tests/audit.rs`): insert via `record`, then `UPDATE audit_log SET action='x'` errors, and `DELETE FROM audit_log` errors.
- [ ] **Step 2: Run it and see it fail.** `cargo test --test audit`.
- [ ] **Step 3: Implement** the migration (with the trigger) and `record`.
- [ ] **Step 4: Run it.** Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -m "feat: add append-only audit log"`

---

### Task 4: Access codes

**Files:**
- Create: `server/migrations/0003_access_codes.sql`, `server/src/access_codes/{mod,service,routes}.rs`, `server/tests/access_codes.rs`
- Modify: `server/src/lib.rs`

**Interfaces:**
- Consumes: `AuthUser`, `require_role(Role::Owner)`, `audit::record`.
- Produces:
  - Table `access_codes(id uuid pk, code_hash bytea unique not null, code_last4 text not null, label text, max_uses int null /* null = unlimited */, uses int not null default 0, expires_at timestamptz null, revoked_at timestamptz null, created_by uuid fk, created_at)`; `access_code_redemptions(code_id fk, user_id fk unique, redeemed_at)`.
  - Codes: **10 characters** from the alphabet `ABCDEFGHJKLMNPQRSTUVWXYZ23456789` (no 0/O/1/I), shown as `XXXXX-XXXXX`. Input is case-insensitive and ignores the dash. Only the SHA-256 hash is stored; the plain code is returned once, at creation.
  - Routes (owner only):
    - `POST /admin/access-codes {label?, max_uses?, expires_at?}` → 201 `{id, code}`.
    - `GET /admin/access-codes` → list of `{id, label, last4, max_uses, uses, expires_at, revoked_at}`.
    - `POST /admin/access-codes/{id}/revoke` → 204.
  - Route (any verified user): `POST /seller/redeem {code}` → 200 `{is_seller: true}`.
  - `pub async fn redeem(db: &PgPool, user_id: Uuid, code: &str) -> Result<(), AppError>`. It locks the code row with `SELECT ... FOR UPDATE` inside one transaction, then increments `uses`, inserts the redemption and sets `users.is_seller = true`.

- [ ] **Step 1: Write failing tests:**

```rust
owner_creates_code_and_user_redeems         // 201 code matches ^[A-Z2-9]{5}-[A-Z2-9]{5}$; redeem → 200; GET /me is_seller true
code_redeem_is_case_and_dash_insensitive    // lower-case, no dash → 200
non_owner_cannot_create_codes               // plain user → 403 "forbidden"; support → 403
list_never_returns_plain_code               // GET list JSON has no "code" field; last4 present
expired_code_rejected                       // expires_at past → 422 "code_expired"
revoked_code_rejected                       // → 422 "code_revoked"; a store created before revoke still exists (after Task 5, add to stores.rs)
single_use_code_exhausted                   // max_uses 1: second user → 422 "code_exhausted"
unknown_code_rejected                       // → 422 "code_invalid"
already_seller_redeem_is_idempotent         // seller redeems another code → 200, uses NOT incremented
unverified_user_cannot_redeem               // → 403 "phone_unverified"
concurrent_single_use_redeem_one_wins       // two users, tokio::join! on redeem → exactly one 200, one 422 "code_exhausted"; uses == 1
redeem_and_create_are_audited               // audit_log rows with actions "access_code.created", "access_code.redeemed"
```

- [ ] **Step 2: Run them and see them fail.** `cargo test --test access_codes`.
- [ ] **Step 3: Implement** the migration, service and routes, rate-limiting redeem attempts to **10 per user per hour** (count recent failed attempts in a small `redeem_attempts` table; over the limit → 429 `too_many_attempts`).
- [ ] **Step 4: Run tests.** Expected: PASS (12 tests).
- [ ] **Step 5: Commit.** `git commit -m "feat: add owner-issued access codes and seller redemption"`

---

### Task 5: Stores

**Files:**
- Create: `server/migrations/0004_stores.sql`, `server/src/stores/{mod,service,routes}.rs`, `server/tests/stores.rs`
- Modify: `server/src/lib.rs`

**Interfaces:**
- Consumes: `AuthUser` (`is_seller`), `FieldCipher`, `audit::record`.
- Produces:
  - Table `stores(id uuid pk, owner_id uuid fk not null, name text not null, slug text unique not null, description text not null default '', logo_key text, banner_key text, pickup_address_enc bytea not null, pickup_state text not null check (pickup_state = 'FCT'), delivery_courier bool not null, delivery_self bool not null, check (delivery_courier or delivery_self), status store_status not null default 'pending', created_at, deleted_at)`; enum `store_status('pending','active','suspended')`. A store stays `pending` until plan 2 verifies its payout bank account.
  - `pub const MAX_STORES_PER_USER: i64 = 2;`
  - `pub async fn create_store(db: &PgPool, cipher: &FieldCipher, owner: Uuid, input: NewStore) -> Result<Store, AppError>`. Inside one transaction it runs `SELECT id FROM users WHERE id = $1 FOR UPDATE`, counts the user's non-deleted stores, and returns `AppError::Conflict("store_limit_reached")` at the limit.
  - Routes:
    - `POST /stores {name, description, pickup_address, pickup_state, delivery_courier, delivery_self}` → 201 store.
    - `GET /stores/mine`.
    - `GET /stores/{slug}` (public; no pickup address).
    - `PATCH /stores/{id}` (owner of the store only).
    - `DELETE /stores/{id}` (soft-delete; frees a slot).
  - Slugs come from the name (lowercase, hyphenated); clashes get the suffix `-2`, `-3` and so on.

- [ ] **Step 1: Write failing tests:**

```rust
seller_creates_store                       // 201, status "pending"; GET /stores/{slug} has no "pickup_address" field
non_seller_cannot_create_store             // → 403 "not_a_seller"
third_store_rejected                       // 2 created, 3rd → 409 "store_limit_reached"
deleted_store_frees_slot                   // create 2, delete 1, create → 201
concurrent_second_and_third_one_wins       // with 1 existing store, tokio::join! two creates → one 201, one 409; count == 2
non_fct_rejected                           // pickup_state "Lagos" → 422 "outside_service_area"
needs_a_delivery_method                    // both false → 422 "validation_failed"
only_owner_can_edit                        // other seller PATCH → 404 (do not reveal existence)
pickup_address_encrypted                   // raw column bytes do not contain the street text
slug_collision_suffixed                    // two stores named "Mama Put" → "mama-put", "mama-put-2"
revoked_code_keeps_existing_store          // from Task 4's list: revoke the code used, store still 200
store_creation_audited                     // audit action "store.created"
```

- [ ] **Step 2: Run them and see them fail.** `cargo test --test stores`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run tests.** Expected: PASS (12 tests).
- [ ] **Step 5: Commit.** `git commit -m "feat: add stores with two-per-user limit and FCT restriction"`

---

### Task 6: Products and variants

**Files:**
- Create: `server/migrations/0005_products.sql`, `server/src/products/{mod,service,routes}.rs`, `server/tests/products.rs`
- Modify: `server/src/lib.rs`

**Interfaces:**
- Consumes: `stores` (ownership check), `AuthUser`.
- Produces:
  - Table `products(id uuid pk, store_id uuid fk not null, name text not null, description text not null, category text not null, brand text, condition product_condition not null, photo_keys text[] not null check (cardinality(photo_keys) between 1 and 10), weight_grams int not null check (weight_grams > 0), length_cm int, width_cm int, height_cm int, base_price_kobo bigint not null check (base_price_kobo > 0), is_published bool not null default false, created_at, updated_at, deleted_at)`.
  - Table `product_variants(id uuid pk, product_id uuid fk not null, attributes jsonb not null /* e.g. {"size":"M","colour":"Black"} */, stock int not null check (stock >= 0), price_kobo bigint check (price_kobo > 0), sku text)`, unique on `(product_id, attributes)`. A product with no options has exactly one variant with `attributes = {}`.
  - `pub struct VariantInput { pub attributes: BTreeMap<String, String>, pub stock: i32, pub price_kobo: Option<i64>, pub sku: Option<String> }`.
  - `pub fn effective_price(product: &Product, variant: &Variant) -> i64` (`variant.price_kobo` or else `base_price_kobo`). Plan 2 uses it.
  - Routes:
    - `POST /stores/{store_id}/products`, with variants inline (1 to **50** variants).
    - `PATCH /products/{id}`.
    - `PUT /products/{id}/variants` (replace the full set).
    - `DELETE /products/{id}` (soft-delete).
    - `GET /stores/{slug}/products` (public; published only, with active stores only once plan 2 activates stores; for now `pending` stores are visible to their owner only).
    - `GET /products/{id}` (public if published).
  - Photo uploads come in a later plan; this plan stores photo keys as given.

- [ ] **Step 1: Write failing tests:**

```rust
create_product_with_variants               // 2 variants (M/Black, L/Black) → 201; GET shows both with stock
simple_product_gets_default_variant        // variants [{attributes:{}, stock:5}] → 201
zero_or_negative_price_rejected            // base_price_kobo 0 and -100 → 422 "validation_failed"
negative_stock_rejected                    // stock -1 → 422
photo_count_bounds                         // 0 photos → 422; 11 photos → 422; 10 → 201
empty_variants_rejected                    // [] → 422
duplicate_variant_attributes_rejected      // two {"size":"M"} → 422 "duplicate_variant"
condition_must_be_known                    // "fake" → 422
only_store_owner_can_add                   // other seller → 404
unpublished_hidden_from_public             // GET /products/{id} unauthenticated → 404 until is_published
pending_store_products_hidden_from_public  // store status pending → public list empty; owner sees them
effective_price_uses_override              // unit test: variant override 1500000 wins over base 1200000
```

- [ ] **Step 2: Run them and see them fail.** `cargo test --test products`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run tests.** Expected: PASS (12 tests).
- [ ] **Step 5: Commit.** `git commit -m "feat: add products with variants, stock and validation"`

---

### Task 7: Full-suite check

- [ ] **Step 1:** Confirm Postgres is up: `pg_isready -h localhost -p 5433`. If it isn't, `docker start market-db`.
- [ ] **Step 2:** From `server/`, run `cargo test` (the whole suite) and `cargo clippy -- -D warnings`. Expected: all tests pass, no warnings.
- [ ] **Step 3:** Run `cargo run -- serve`, then `curl http://localhost:8080/health`. Expected: `{"status":"ok"}`.
- [ ] **Step 4:** Commit any fixes: `git commit -am "chore: clippy fixes"`
