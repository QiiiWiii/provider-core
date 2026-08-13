UPDATE usage_logical_requests
SET logical_status = 'succeeded'
WHERE execution_outcome = 'stable_success_terminal'
  AND delivery_outcome = 'client_drop'
  AND logical_status = 'canceled';
