# Marketplace App — Design

Date: 2026-10-22
Status: Approved by owner 2026-10-23. No fixed launch date; build the full version as designed (no reduced first release).
Working name: "the Market" (no product name chosen yet)

## 1. Intent

A mobile marketplace where individuals — mainly business owners and marketers — run their own stores inside one shared market, and the general public signs in to buy from them. Launch market is **Abuja (FCT)**, chosen to win one city first.

What makes it different from "just another shop app":

- **Buyer protection by default.** Buyers pay inside the app; the money is held by a licensed payment provider and only released to the seller after delivery is confirmed (or a short window passes with no complaint).
- **Fast, trackable delivery.** Next-day delivery within Abuja, with a clear, honest tracking screen.
- **Curated sellers.** Opening a store requires an access code issued by the owner.
- **Clean, respectful conversations.** All buyer–seller chat is moderated for abuse, tone and scam attempts.

### Success looks like

- Abuja buyers trust it enough to pay up-front to sellers they've never met.
- Most orders placed before the daily cut-off arrive the next day.
- Sellers get paid within about 4–5 days of a sale.
- Disputes are rare and the owner can settle each one from a single screen.

### Anti-goals (would be failure even if it "works")

- A UI that looks templated or "vibe coded". Tracking and checkout especially must feel deliberate and polished.
- Any path where a buyer is pushed to pay a seller outside the app.
- Card numbers, BVN/NIN or bank credentials stored by our system.
- Personal data (addresses, phone numbers, chats) visible to anyone who doesn't need it.

## 2. People and roles

| Role | Who | Can do |
|---|---|---|
| **Buyer** | Anyone who signs up | Browse, chat with sellers, buy, track, confirm delivery, complain, review |
| **Seller** | A user who redeemed an access code | Everything a buyer can, plus own **up to 2 stores** |
| **Owner/Admin** | The business running the market | Web dashboard: codes, users, stores, orders, disputes, flagged chats, platform cut |

One account type; "seller" is an unlock on a normal account, not a separate login.

## 3. Accounts and access

- **Sign up / sign in** in the mobile app (email or phone + password; phone verified by OTP). *(builder's call on exact auth method; passwords hashed with Argon2, never logged)*
- **Shopping is open**: any signed-up user can browse and buy.
- **Selling needs an access code** issued by the owner from the dashboard.
  - Codes can be single-use or multi-use with an optional expiry and usage cap. *(my call — covers both "one code per person" and "shared code" options)*
  - Owner can revoke a code; already-created stores are unaffected.
- **Store limit: 2 per user**, enforced on the server.

## 4. Stores and products

**Store:** name, logo, banner, description, Abuja pickup address (with NIPOST postcode), delivery method (courier / self-delivery / both), bank account for payouts.

**Before a store can accept orders:**
1. Access code redeemed.
2. Payout bank account verified through the payment provider (account-name match).
3. Optional extra identity check (BVN/NIN) **done through the provider** — our system stores only the pass/fail result and provider reference, never the number.

**Product ("everything a buyer needs to trust it"):**
name, price (₦), multiple photos, description, category, brand, condition (new / used / refurbished), variants (e.g. size, colour) each with its own stock count and optional price, weight/size (needed for courier rates), store info.

Stock is decremented at payment and restored on refund/cancellation.

## 5. Payments (escrow via provider)

**Provider:** Paystack or Flutterwave *(builder to confirm which supports collect-to-platform + controlled payouts best; Paystack is the starting assumption)*. Card details never touch our servers.

**Flow:**
1. Buyer pays in-app → money collected into the **platform's provider balance**.
2. Seller notified: "Paid — please ship." Seller sees the order, **not** the money.
3. Delivery happens (section 6).
4. **Release** to the seller's verified bank account via provider payout, minus the platform cut, when either:
   - buyer confirms (handover code at the door, or "Received" button), **or**
   - **3 days after the order is marked delivered** with no complaint.
5. **Backstop:** if an order is never marked delivered, nothing auto-releases. At **30 days from payment** it is surfaced to the owner for a decision; buyer may request a refund.
6. **Complaint** before release → funds frozen until the owner decides (section 8).

**Reminders to confirm:** when marked delivered, next day, and a final "payment will be released tomorrow" notice.

**Platform cut:** **5%** of the item total (owner decision), adjustable in the dashboard, applied at release.

**Who pays what** *(my recommendation, owner set the 5% cut)*:
- **Buyer** pays the delivery fee, added at checkout (courier rate from Shipbubble, or the seller's own flat fee for self-delivery). The delivery portion is collected by the platform; for courier orders it pays Shipbubble from a funded platform wallet.
- **Seller** pays the payment-provider collection fee and the payout transfer fee, deducted from the payout.
- **Seller payout = item total − 5% cut − provider fees.** The payout screen shows this breakdown.
- **Refunds:** full refund returns item total + delivery to the buyer; the platform waives its cut on refunded orders. Provider fees that can't be recovered are charged to the side at fault, as decided by the owner.

**Legal flag:** holding funds for third parties is regulated by the CBN. Design assumes the licensed provider holds funds; **a Nigerian lawyer/compliance adviser must confirm the setup before launch.**

All money movements are written to an **append-only ledger** (payment, hold, release, refund, cut) for audit.

**Multi-store cart** *(my call)*: a cart may hold items from several stores. Checkout is one payment, split into **one order per store**. Each order has its own delivery, handover code, release clock, dispute and refund.

**Cancellations and non-shipment** *(my call)*:
- Buyer can cancel for a full refund until the order is picked up.
- Seller must hand over / mark picked up within **48 hours** of payment, or the order is auto-cancelled and refunded, and the seller gets a strike.
- In a dispute, the owner decides whether the item must be returned first; return shipping is paid by whichever side is at fault.

## 6. Delivery

**Coverage at launch:** stores and delivery addresses must be inside the FCT. Buyers elsewhere can browse but not check out ("Coming to your city soon").

**Addresses:** at checkout the buyer's address is resolved to a **NIPOST 11-character digital postcode** (live nationwide since 1 Oct 2026) via the NIPOST postcode API (`api.postcode.gov.ng`). Free basic lookup/validity; deeper levels need KYB and credits. **Owner must request developer access** (g2g@postcode.gov.ng / "Integrate with us" on postcode.gov.ng). Official API docs were not found publicly — builder must design this behind an interface so a manual/Google-Maps fallback works until access is granted.

**Delivery methods (chosen per store):**
- **Courier via Shipbubble** — one integration covering 20+ Nigerian couriers (GIG, Kwik, etc.): rates at checkout, label/pickup booking, status updates via signed webhooks (HMAC-SHA512, `x-ship-signature`). Terminal Africa is the fallback aggregator.
- **Self-delivery** — seller taps *Picked up → Out for delivery → Delivered*.

**Handover code:** every order gets a short code shown to the buyer; the person delivering enters it on handover. Entering it = buyer confirmation (triggers release).

**What counts as "delivered"** *(my call, protects buyers)*:
- **Self-delivery:** the seller **cannot** mark Delivered without the buyer's handover code (or the buyer pressing "Received"). No code → no release clock.
- **Courier:** couriers won't use our app, so Shipbubble's "delivered" webhook starts the 3-day release window; the buyer can still complain within it.

**Phone numbers** *(my call)*: chat blocks phone numbers. The buyer's phone is shown to a self-delivering seller only once the order is "Out for delivery", and hidden again when it completes. The tracking call button calls the courier's or seller's business line, not a personal number shared in chat.

**Next-day promise:** orders paid before a daily **cut-off (6pm, owner-configurable)** are promised for next-day delivery; after cut-off, the day after. Shown honestly at checkout.

### Tracking screen (must be polished, not templated)

Based on Baymard order-tracking research and established delivery-app patterns:
1. **Top:** expected delivery day + current status, large and unambiguous.
2. **Timeline:** vertical, timestamped steps (Paid → Seller preparing → Picked up → Out for delivery → Delivered → Confirmed).
3. **Courier/seller name + call button** — buyers never leave the app to track.
4. **Handover code** clearly visible on "Out for delivery".
5. **Map** only during "Out for delivery" (and only if the courier provides location) — not a constant, battery-heavy map.
6. Order details below.

## 7. Chat (moderated)

- Buyer ↔ seller chat, always tied to a product or an order.
- Every message is checked **before delivery**:
  - **Word filter** (instant) for vulgar/abusive words.
  - **AI tone check** (Claude Haiku, `claude-haiku-5-5`) for insults, threats, harassment, aggression without "bad words".
  - **Scam patterns**: phone numbers, bank account numbers, "pay me directly", moving to WhatsApp etc.
  - Must handle **English, Pidgin, Hausa, Yoruba, Igbo** and mixtures.
- **Outcomes:**
  - **Serious** (threats, slurs, sexual content, scam/off-platform contact) → **blocked**, sender warned with reason, strike recorded.
  - **Mild** (rude tone, mild swearing, shouting) → sender is shown a **suggested nicer version** with one-tap "Send suggested" or edit. The original is never delivered.
- **Strikes:** warning → temporary mute → owner review for ban. *(my guess on thresholds; builder sets defaults, owner-configurable)*
- Originals of flagged messages are kept, visible **only to the owner**, for disputes.
- Owner can read an order's chat when a dispute is open.

**Data protection:** chat is personal data processed by a third party (AI provider, possibly outside Nigeria). Requires: disclosure in the privacy notice, provider configured not to retain/train on data, only message text sent (no names/IDs), retention of **12 months** (longer while a dispute is open). **DPO/legal sign-off needed.**

## 8. Disputes

1. Buyer opens a complaint (reason + photos) before funds are released → funds frozen.
2. Seller has **48 hours** to respond with their side and evidence.
3. Owner sees one screen: order, payment, delivery timeline, handover-code status, both sides' evidence, the order chat (including flagged originals).
4. Owner decides: **full refund**, **partial refund** (amount set), or **release to seller**.
5. Both parties notified; decision written to the audit log permanently.

## 9. Reviews

- Only buyers with a **confirmed delivery** can review that order's store.
- 1–5 stars, text, optional photos.
- Seller may reply once.
- Reviews pass the same word/tone/scam checks as chat.
- Store page shows average rating and count.

## 10. Notifications

Push (plus in-app inbox) for: payment received (seller), order status changes (buyer), new chat message, confirm-delivery reminders, funds released (seller), dispute opened/decided, moderation warnings.

## 11. Owner dashboard (web)

- Access codes: create (single/multi-use, cap, expiry), list, revoke.
- Users & stores: search, view, suspend, ban.
- Orders & payments: status, ledger view, stuck orders (30-day backstop queue).
- Disputes: queue + decision screen (section 8).
- Moderation: flagged messages/reviews, strikes, bans.
- Settings: platform cut %, delivery cut-off time, strike thresholds.
- Every admin action is audit-logged (who, what, when).
- **Several admins with roles** *(my call)*: **Owner** (everything, including settings and adding admins) and **Support** (disputes and moderation only; can see personal data only inside an open dispute).

## 12. How it's built

| Part | Choice | Decided by |
|---|---|---|
| Mobile app (Android + iOS) | **Flutter** | Owner chose Flutter/RN; Flutter is my call |
| Server/API | **Rust** (e.g. Axum) | Owner |
| Database | **PostgreSQL** | My call |
| Owner dashboard | Web app | My call; stack left to builder |
| Payments | Paystack (Flutterwave fallback), 5% platform cut | Owner chose provider-held escrow and the 5% cut; provider is my call |
| Couriers | Shipbubble (Terminal Africa fallback) | My call from research |
| Postcodes | NIPOST postcode API | Owner |
| Chat moderation AI | Claude Haiku | My call |
| Photos | Object storage (S3-compatible) | Builder's call |
| Push | Firebase Cloud Messaging | Builder's call |

## 13. Data protection (NDPA 2023 primary)

- **Minimise:** collect only what each feature needs. No BVN/NIN/card numbers stored.
- **Access:** buyer address and phone visible only to the seller of that order and its courier, and only while the order is active.
- **Encryption:** TLS everywhere; sensitive columns (addresses, phone numbers) encrypted at rest.
- **No PII in logs or URLs.**
- **Audit:** append-only logs for money movements, admin actions and access to personal data in disputes.
- **Data-subject rights:** users can download their data and request deletion; deletion is soft-delete followed by purge after the retention period, except records that must be kept for financial/legal reasons.
- **Third parties:** Paystack, Shipbubble, NIPOST, AI moderation provider, push provider — all listed in the privacy notice.
- **Needs DPO/legal review:** escrow arrangement (CBN), AI processing of chats, retention periods.

## 14. Not in the first version (non-goals)

- Cities outside Abuja.
- A web storefront for buyers (mobile only).
- Cash on delivery.
- More than 2 stores per user.
- Seller ads/promoted listings.

## 15. Left to the builder

- Exact auth method details (email vs phone primary).
- Screen-by-screen visual design, within the "polished, not templated" bar.
- Database schema, API shape, job scheduling for auto-release and reminders.
- Default strike thresholds.
- Search and category structure.
