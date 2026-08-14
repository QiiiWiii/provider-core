ALTER TABLE usage_logical_requests
ADD COLUMN endpoint TEXT CHECK (
    endpoint IS NULL OR endpoint IN (
        'openai_responses',
        'openai_chat_completions',
        'claude_messages'
    )
);
