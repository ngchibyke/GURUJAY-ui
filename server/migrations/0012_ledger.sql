-- Double-entry ledger (append-only). Sign convention: a positive amount is a
-- debit to that account, a negative amount a credit. Money flowing into the
-- platform's books is positive on the internal account (e.g. escrow) and
-- negative on the external one (e.g. buyer_funds).
--
-- Every write goes through ledger::post, which stamps one txn_id on a balanced
-- set of entries. The database also enforces the balance per txn_id at commit
-- (constraint trigger below), so no other writer can store an unbalanced set.
-- Personal data: none (ids, amounts and our own references only).

CREATE TYPE ledger_account AS ENUM (
    'buyer_funds', 'escrow', 'platform_revenue', 'provider_fees',
    'seller_payable', 'seller_paid', 'refunds'
);

CREATE TABLE ledger_entries (
    id          bigserial PRIMARY KEY,
    txn_id      uuid NOT NULL,
    at          timestamptz NOT NULL DEFAULT now(),
    order_id    uuid REFERENCES orders (id),
    checkout_id uuid REFERENCES checkouts (id),
    account     ledger_account NOT NULL,
    amount_kobo bigint NOT NULL CHECK (amount_kobo <> 0),
    kind        text NOT NULL,
    ref         text
);
CREATE INDEX ledger_entries_txn_idx ON ledger_entries (txn_id);
CREATE INDEX ledger_entries_order_account_idx ON ledger_entries (order_id, account);
CREATE INDEX ledger_entries_checkout_idx ON ledger_entries (checkout_id);

-- Append-only: reject UPDATE, DELETE and TRUNCATE.
CREATE FUNCTION ledger_entries_reject_change() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'ledger_entries is append-only (% not allowed)', TG_OP;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER ledger_entries_no_update
    BEFORE UPDATE ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION ledger_entries_reject_change();
CREATE TRIGGER ledger_entries_no_delete
    BEFORE DELETE ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION ledger_entries_reject_change();
CREATE TRIGGER ledger_entries_no_truncate
    BEFORE TRUNCATE ON ledger_entries
    FOR EACH STATEMENT EXECUTE FUNCTION ledger_entries_reject_change();

-- Zero sum per txn_id, checked at commit (deferred), whoever inserts.
CREATE FUNCTION ledger_entries_check_balanced() RETURNS trigger AS $$
DECLARE
    total numeric;
BEGIN
    SELECT sum(amount_kobo) INTO total FROM ledger_entries WHERE txn_id = NEW.txn_id;
    IF total <> 0 THEN
        RAISE EXCEPTION 'ledger transaction % does not balance', NEW.txn_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER ledger_entries_balanced
    AFTER INSERT ON ledger_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ledger_entries_check_balanced();

-- ALWAYS: the triggers fire even with session_replication_role = replica.
-- (A superuser or the table owner can still drop them; production should run
-- the app as a role that owns nothing and has no UPDATE/DELETE/TRUNCATE grant.)
ALTER TABLE ledger_entries ENABLE ALWAYS TRIGGER ledger_entries_no_update;
ALTER TABLE ledger_entries ENABLE ALWAYS TRIGGER ledger_entries_no_delete;
ALTER TABLE ledger_entries ENABLE ALWAYS TRIGGER ledger_entries_no_truncate;
ALTER TABLE ledger_entries ENABLE ALWAYS TRIGGER ledger_entries_balanced;
