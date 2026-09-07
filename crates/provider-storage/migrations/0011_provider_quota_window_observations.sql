ALTER TABLE provider_credentials
ADD COLUMN quota_identity_revision INTEGER NOT NULL DEFAULT 0 CHECK (quota_identity_revision >= 0);

ALTER TABLE usage_attempts
ADD COLUMN credential_identity_revision INTEGER NOT NULL DEFAULT 0 CHECK (credential_identity_revision >= 0);

CREATE TABLE provider_quota_window_observations (
    account_id TEXT NOT NULL,
    credential_revision INTEGER NOT NULL CHECK (credential_revision >= 0),
    credential_identity_revision INTEGER NOT NULL CHECK (credential_identity_revision >= 0),
    observed_at_ms INTEGER NOT NULL,
    group_key TEXT NOT NULL CHECK (length(group_key) > 0),
    metric_key TEXT NOT NULL CHECK (length(metric_key) > 0),
    metric_position INTEGER NOT NULL CHECK (metric_position >= 0),
    used_hundredths INTEGER NOT NULL CHECK (used_hundredths BETWEEN 0 AND 10000),
    period_kind TEXT NOT NULL CHECK (period_kind IN ('weekly', 'monthly', 'rolling')),
    starts_at_ms INTEGER NOT NULL,
    ends_at_ms INTEGER NOT NULL,
    duration_seconds INTEGER,
    PRIMARY KEY (
        account_id,
        credential_identity_revision,
        credential_revision,
        observed_at_ms,
        group_key,
        metric_key,
        starts_at_ms,
        ends_at_ms
    ),
    CHECK (ends_at_ms > starts_at_ms),
    CHECK (observed_at_ms >= starts_at_ms AND observed_at_ms <= ends_at_ms),
    CHECK (duration_seconds IS NULL OR duration_seconds > 0),
    FOREIGN KEY (account_id) REFERENCES provider_accounts(id) ON DELETE CASCADE
);

CREATE INDEX provider_quota_observations_history_idx
    ON provider_quota_window_observations (
        account_id,
        credential_identity_revision,
        ends_at_ms DESC,
        group_key,
        metric_position
    );
