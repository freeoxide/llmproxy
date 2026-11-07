//! The replica schema: one DDL batch, idempotent, applied on connect.

/// Creates every table and index the replica stores use, if they do not
/// exist. Money columns are micro-USD integers; window rows carry their
/// window id (unix minute, calendar day) so rollover is a new row.
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS admission_minutes (
    caller   TEXT   NOT NULL,
    minute   BIGINT NOT NULL,
    requests BIGINT NOT NULL DEFAULT 0,
    tokens   BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (caller, minute)
);

CREATE TABLE IF NOT EXISTS admission_days (
    caller        TEXT                      NOT NULL,
    day           DATE                      NOT NULL,
    requests      BIGINT                    NOT NULL DEFAULT 0,
    spent_usd     BIGINT                    NOT NULL DEFAULT 0,
    reserved_usd  BIGINT                    NOT NULL DEFAULT 0,
    PRIMARY KEY (caller, day)
);

CREATE TABLE IF NOT EXISTS admission_leases (
    id                UUID        PRIMARY KEY,
    caller            TEXT        NOT NULL,
    member            TEXT        NOT NULL,
    model             TEXT        NOT NULL,
    minute            BIGINT      NOT NULL,
    day               DATE        NOT NULL,
    reserved_tokens   BIGINT      NOT NULL DEFAULT 0,
    reserved_usd      BIGINT      NOT NULL DEFAULT 0,
    holds_member_slot BOOLEAN     NOT NULL DEFAULT FALSE,
    holds_model_slot  BOOLEAN     NOT NULL DEFAULT FALSE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS admission_inflight_members (
    member TEXT   NOT NULL PRIMARY KEY,
    count  BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS admission_inflight_models (
    member TEXT   NOT NULL,
    model  TEXT   NOT NULL,
    count  BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (member, model)
);

CREATE TABLE IF NOT EXISTS cooldowns (
    scope TEXT       NOT NULL,
    name  TEXT       NOT NULL,
    until TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (scope, name)
);

CREATE TABLE IF NOT EXISTS usage_events (
    id          BIGSERIAL   PRIMARY KEY,
    appended_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    payload     JSONB       NOT NULL
);

CREATE INDEX IF NOT EXISTS admission_minutes_minute_idx ON admission_minutes (minute);
CREATE INDEX IF NOT EXISTS admission_days_day_idx ON admission_days (day);
CREATE INDEX IF NOT EXISTS admission_leases_created_at_idx ON admission_leases (created_at);
"#;
