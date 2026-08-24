-- no-transaction
PRAGMA foreign_keys = OFF;
BEGIN;

CREATE TABLE provider_accounts_new (
    id TEXT PRIMARY KEY NOT NULL,
    owner_user_id TEXT,
    visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (visibility IN ('private', 'shared')),
    provider TEXT NOT NULL CHECK (
        provider IN ('grok', 'codex', 'openai_compatible', 'anthropic_compatible', 'claude_oauth')
    ),
    label TEXT NOT NULL CHECK (length(trim(label)) > 0 AND length(label) <= 128),
    group_label TEXT NOT NULL CHECK (
        length(trim(group_label)) > 0 AND length(group_label) <= 64
    ),
    config_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config_json)),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    auth_state TEXT NOT NULL DEFAULT 'active'
        CHECK (auth_state IN ('active', 'reauth_required')),
    safe_error_code TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    priority INTEGER NOT NULL DEFAULT 0 CHECK (priority >= 0),
    FOREIGN KEY (owner_user_id) REFERENCES users(id) ON DELETE RESTRICT
);

INSERT INTO provider_accounts_new (
    id,
    owner_user_id,
    visibility,
    provider,
    label,
    group_label,
    config_json,
    enabled,
    auth_state,
    safe_error_code,
    created_at,
    updated_at,
    priority
)
SELECT
    id,
    owner_user_id,
    visibility,
    provider,
    label,
    group_label,
    config_json,
    enabled,
    auth_state,
    safe_error_code,
    created_at,
    updated_at,
    priority
FROM provider_accounts;

DROP TABLE provider_accounts;
ALTER TABLE provider_accounts_new RENAME TO provider_accounts;

CREATE INDEX provider_accounts_enabled_provider_idx
    ON provider_accounts (enabled, provider);
CREATE INDEX provider_accounts_group_label_idx
    ON provider_accounts (group_label);
CREATE INDEX provider_accounts_owner_visibility_idx
    ON provider_accounts (owner_user_id, visibility, enabled);

COMMIT;
PRAGMA foreign_keys = ON;
