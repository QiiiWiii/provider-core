UPDATE provider_accounts
SET config_json = json_set(
    config_json,
    '$.upstream_protocol',
    'chat_completions'
)
WHERE provider = 'openai_compatible';
