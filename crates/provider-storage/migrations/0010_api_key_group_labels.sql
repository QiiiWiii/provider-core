-- no-transaction
PRAGMA foreign_keys = OFF;
BEGIN;

CREATE TABLE api_keys_new (
    id TEXT PRIMARY KEY NOT NULL,
    owner_user_id TEXT NOT NULL,
    group_labels TEXT NOT NULL CHECK (
        json_valid(group_labels)
        AND json_type(group_labels) = 'array'
        AND json_array_length(group_labels) >= 1
    ),
    label TEXT NOT NULL CHECK (length(trim(label)) > 0 AND length(label) <= 128),
    key TEXT NOT NULL UNIQUE,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    expires_at INTEGER,
    quota_limit_atoms TEXT CHECK (
        quota_limit_atoms IS NULL OR (
            length(quota_limit_atoms) > 0 AND length(quota_limit_atoms) <= 64
            AND quota_limit_atoms GLOB '[1-9]*'
            AND quota_limit_atoms NOT GLOB '*[^0-9]*'
        )
    ),
    spent_atoms TEXT NOT NULL DEFAULT '0' CHECK (
        length(spent_atoms) > 0 AND length(spent_atoms) <= 64
        AND spent_atoms GLOB '[0-9]*'
        AND spent_atoms NOT GLOB '*[^0-9]*'
    ),
    last_used_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (owner_user_id) REFERENCES users(id) ON DELETE CASCADE
);

INSERT INTO api_keys_new (
    id,
    owner_user_id,
    group_labels,
    label,
    key,
    enabled,
    expires_at,
    quota_limit_atoms,
    spent_atoms,
    last_used_at,
    created_at,
    updated_at
)
SELECT
    id,
    owner_user_id,
    json_array(group_label),
    label,
    key,
    enabled,
    expires_at,
    quota_limit_atoms,
    spent_atoms,
    last_used_at,
    created_at,
    updated_at
FROM api_keys;

DROP TABLE api_keys;
ALTER TABLE api_keys_new RENAME TO api_keys;

CREATE INDEX api_keys_owner_idx ON api_keys (owner_user_id, created_at);
CREATE INDEX api_keys_active_idx ON api_keys (enabled, expires_at);

CREATE TABLE usage_logical_requests_new (
    request_id TEXT PRIMARY KEY NOT NULL,
    owner_user_id TEXT NOT NULL,
    api_key_id TEXT,
    api_key_label TEXT,
    api_key_group_labels TEXT CHECK (
        api_key_group_labels IS NULL OR (
            json_valid(api_key_group_labels)
            AND json_type(api_key_group_labels) = 'array'
            AND json_array_length(api_key_group_labels) >= 1
        )
    ),
    client_model_raw TEXT,
    routing_model TEXT,
    reasoning_effort TEXT CHECK (
        reasoning_effort IS NULL OR (
            length(trim(reasoning_effort)) > 0 AND length(reasoning_effort) <= 32
        )
    ),
    started_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    logical_status TEXT NOT NULL CHECK (
        logical_status IN ('in_progress', 'succeeded', 'failed', 'canceled', 'incomplete')
    ),
    execution_outcome TEXT CHECK (
        execution_outcome IS NULL OR execution_outcome IN (
            'stable_success_terminal',
            'stable_failure',
            'translator_or_stream_error',
            'eof_without_success_terminal',
            'recovered_old_run_active'
        )
    ),
    delivery_outcome TEXT CHECK (
        delivery_outcome IS NULL OR delivery_outcome IN (
            'clean_eof', 'client_drop', 'error_before_bytes', 'error_after_bytes', 'unknown'
        )
    ),
    final_attempt_id TEXT,
    tracking_state TEXT NOT NULL DEFAULT 'complete'
        CHECK (tracking_state IN ('complete', 'gap')),
    tracking_gap_reason TEXT CHECK (
        tracking_gap_reason IS NULL OR tracking_gap_reason IN (
            'write_failed',
            'writer_saturated',
            'recovered_in_flight',
            'ambiguous_cancel',
            'observation_lost'
        )
    ),
    state_version INTEGER NOT NULL DEFAULT 0 CHECK (state_version >= 0),
    endpoint TEXT CHECK (
        endpoint IS NULL OR endpoint IN (
            'openai_responses',
            'openai_chat_completions',
            'claude_messages'
        )
    ),
    CHECK ((logical_status = 'in_progress') = (completed_at_ms IS NULL)),
    CHECK ((tracking_state = 'gap') = (tracking_gap_reason IS NOT NULL))
);

INSERT INTO usage_logical_requests_new (
    request_id,
    owner_user_id,
    api_key_id,
    api_key_label,
    api_key_group_labels,
    client_model_raw,
    routing_model,
    reasoning_effort,
    started_at_ms,
    completed_at_ms,
    logical_status,
    execution_outcome,
    delivery_outcome,
    final_attempt_id,
    tracking_state,
    tracking_gap_reason,
    state_version,
    endpoint
)
SELECT
    request_id,
    owner_user_id,
    api_key_id,
    api_key_label,
    CASE
        WHEN api_key_group_label IS NULL THEN NULL
        ELSE json_array(api_key_group_label)
    END,
    client_model_raw,
    routing_model,
    reasoning_effort,
    started_at_ms,
    completed_at_ms,
    logical_status,
    execution_outcome,
    delivery_outcome,
    final_attempt_id,
    tracking_state,
    tracking_gap_reason,
    state_version,
    endpoint
FROM usage_logical_requests;

DROP TABLE usage_logical_requests;
ALTER TABLE usage_logical_requests_new RENAME TO usage_logical_requests;

CREATE INDEX usage_logical_requests_owner_idx
    ON usage_logical_requests (owner_user_id, completed_at_ms DESC, request_id DESC);
CREATE INDEX usage_logical_requests_key_idx
    ON usage_logical_requests (api_key_id, completed_at_ms DESC, request_id DESC);
CREATE INDEX usage_logical_requests_in_flight_idx
    ON usage_logical_requests (started_at_ms)
    WHERE logical_status = 'in_progress';

COMMIT;
PRAGMA foreign_keys = ON;
