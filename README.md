# Stock App

A mobile marketplace for Abuja. Individuals open up to two stores inside one shared market, and anyone can sign in to buy from them. Buyers pay in the app; the money is held by the payment provider and released to the seller after delivery is confirmed.

This repository holds the backend API. The Flutter phone app and the owner dashboard come in later parts of the build.

## What works today

The server foundation (part 1 of 8):

- **Accounts:** sign up with a Nigerian phone number and password, confirmed by an SMS code. Sign-in sessions last 30 days and can be revoked. Roles: user, support, owner.
- **Access codes:** the owner issues codes (single-use or shared, optional expiry and usage cap, revocable). Redeeming one turns a user into a seller.
- **Stores:** up to 2 per seller, Abuja (FCT) only, with an encrypted pickup address. Stores stay hidden from the public until payouts are verified (part 2).
- **Products:** photos, condition, brand, and variants such as size and colour, each with its own stock and optional price.
- **Data protection:** phone numbers and addresses are encrypted at rest, lookups use a keyed hash, personal data is never logged, and an append-only audit log records sensitive actions.

Payments and escrow (part 2):

- **Paying:** a cart becomes one checkout paid through Mono DirectPay (behind a provider layer that can be switched), split into one order per store. Stock is reserved while the buyer pays.
- **Held funds and ledger:** every money movement is a balanced entry in an append-only, double-entry ledger, and the platform takes a 5% cut.
- **Payouts:** sellers register a payout account, which is verified and stored encrypted. Funds are released to the seller through Mono Disburse after the buyer confirms, or 3 days after delivery. The pickup and delivery steps arrive in part 3, so until then orders stay at `paid` and nothing is released automatically.
- **Refunds and cancellations:** refunds are retried by a background sweeper.
- **Background jobs:** expiry, lost-webhook re-verification, auto-release, the 30-day backstop queue and payout polling.
- **Owner admin:** release, refund and hold orders; mark payouts paid or refunds refunded by hand; re-queue reversed payouts. A reconciliation list shows everything that needs attention. Every staff read and override is audited.

## Roadmap

| Part | What it adds |
|---|---|
| 1 | Server foundation (done) |
| 2 | Payments and escrow (Mono first, behind a switchable provider layer): cart, orders, held funds, release, refunds, 5% platform cut (done) |
| 3 | Delivery and tracking: NIPOST postcodes, Shipbubble couriers, self-delivery, handover codes |
| 4 | Moderated chat and reviews |
| 5 | Disputes and notifications |
| 6 | Owner web dashboard |
| 7 | Flutter phone app (Android and iPhone) |
| 8 | Data export, deletion and retention |

The design is in [docs/superpowers/specs/2026-10-22-abuja-marketplace-design.md](docs/superpowers/specs/2026-10-22-abuja-marketplace-design.md). The build plan and the items each later part must handle are in [docs/superpowers/plans/2026-10-23-00-roadmap.md](docs/superpowers/plans/2026-10-23-00-roadmap.md).

## Running it locally

You need Rust (stable) and Docker.

1. Start the database (Postgres 17 on `localhost:5433`):

   ```bash
   docker compose up -d db
   ```

2. Copy `server/.env.example` to `server/.env` and fill in `FIELD_KEY` and `INDEX_KEY` with two different random 32-byte keys, base64-encoded:

   ```bash
   openssl rand -base64 32
   ```

3. Start the API (it listens on `127.0.0.1:8080`):

   ```bash
   cd server && cargo run -- serve
   ```

4. Create the owner account (you'll be asked for the password):

   ```bash
   cd server && cargo run -- create-owner --phone 0803XXXXXXX --name "Your Name"
   ```

## Tests

The tests run against the local database:

```bash
cd server && DATABASE_URL=postgres://market:market@localhost:5433/market cargo test
```

Every push and pull request runs format, lint, the full test suite against Postgres, and a dependency security audit (`.github/workflows/ci.yml`).

## Before going live

These are tracked in the roadmap:

- Rate limits per network address on sign-up and sign-in.
- A restricted production database role.
- Legal review of the escrow setup, and data-protection (DPO) review.
- The ship-deadline job (`platform_settings.jobs_ship_deadline_enabled`) stays **off** until plan 3 ships a pickup route. Until then nothing can move an order out of `paid`, so the job would refund every paid order and strike its seller.
- Confirm with Mono which unit (kobo or naira) each product's `amount` fields use (`MONO_DIRECTPAY_AMOUNT_UNIT`, `MONO_DISBURSE_AMOUNT_UNIT`), then run a sandbox smoke test of a payment, a payout and a webhook before switching to live keys.
- DPO review of whether the Owner should keep seeing buyer display names on order reads. Support already does not, and every staff read is audited.
