ALTER TABLE usage_logical_requests
ADD COLUMN user_agent TEXT CHECK (
    user_agent IS NULL OR (
        length(trim(user_agent)) > 0 AND length(user_agent) <= 256
    )
);

ALTER TABLE usage_logical_requests
ADD COLUMN client_type TEXT NOT NULL DEFAULT 'unknown' CHECK (
    client_type IN ('unknown', 'claude_code')
);
