CREATE TYPE user_role AS ENUM ('user', 'support', 'owner');

CREATE TABLE users (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    phone_enc         bytea NOT NULL,
    phone_idx         bytea NOT NULL UNIQUE,
    display_name      text NOT NULL,
    password_hash     text NOT NULL,
    role              user_role NOT NULL DEFAULT 'user',
    phone_verified_at timestamptz,
    is_seller         boolean NOT NULL DEFAULT false,
    created_at        timestamptz NOT NULL DEFAULT now(),
    deleted_at        timestamptz
);

CREATE TABLE sessions (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     uuid NOT NULL REFERENCES users (id),
    token_hash  bytea NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    revoked_at  timestamptz
);
CREATE INDEX sessions_user_id_idx ON sessions (user_id);

CREATE TABLE otp_codes (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     uuid NOT NULL REFERENCES users (id),
    code_hash   bytea NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    attempts    integer NOT NULL DEFAULT 0,
    consumed_at timestamptz
);
CREATE INDEX otp_codes_user_id_idx ON otp_codes (user_id);
