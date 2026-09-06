use super::*;

impl SqliteAccountRepository {
    pub(super) async fn load_provider_models(
        &self,
        account_id: Option<&AccountId>,
    ) -> Result<Vec<StoredProviderModel>, AccountRepositoryError> {
        let rows = if let Some(account_id) = account_id {
            sqlx::query(
                r#"
                SELECT account_id, upstream_model, alias, enabled, available, routable,
                       input_modalities_json, metadata_json,
                       pricing_source, pricing_json, last_seen_at, created_at, updated_at
                FROM provider_models
                WHERE account_id = ?
                ORDER BY upstream_model
                "#,
            )
            .bind(account_id.as_str())
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                r#"
                SELECT account_id, upstream_model, alias, enabled, available, routable,
                       input_modalities_json, metadata_json,
                       pricing_source, pricing_json, last_seen_at, created_at, updated_at
                FROM provider_models
                ORDER BY account_id, upstream_model
                "#,
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|error| repository_error("failed to list provider models", error))?;
        rows.into_iter().map(stored_model).collect()
    }

    pub(super) async fn sync_provider_models(
        &self,
        account_id: &AccountId,
        models: Vec<DiscoveredProviderModel>,
        synced_at: i64,
    ) -> Result<Vec<StoredProviderModel>, AccountRepositoryError> {
        if models.iter().any(|model| {
            let upstream_model = model.upstream_model.as_str();
            upstream_model.is_empty() || upstream_model.trim() != upstream_model
        }) {
            return Err(AccountRepositoryError::new(
                "discovered provider model must not be empty or contain surrounding whitespace",
            ));
        }
        let mut transaction = self
            .write
            .begin()
            .await
            .map_err(|error| repository_error("failed to start model transaction", error))?;
        let result = async {
            let account_exists = sqlx::query("SELECT 1 FROM provider_accounts WHERE id = ?")
                .bind(account_id.as_str())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|error| repository_error("failed to verify provider account", error))?
                .is_some();
            if !account_exists {
                return Err(AccountRepositoryError::new(
                    "provider account was not found while synchronizing models",
                ));
            }
            sqlx::query(
                "UPDATE provider_models SET available = 0, updated_at = ? WHERE account_id = ?",
            )
            .bind(synced_at)
            .bind(account_id.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                repository_error("failed to mark provider models unavailable", error)
            })?;
            for model in models {
                let upstream_model = model.upstream_model.as_str();
                let (pricing_source, pricing_json) = encode_model_pricing(model.pricing.as_ref())?;
                let input_modalities_json =
                    encode_input_modalities(model.input_modalities.as_deref())?;
                sqlx::query(
                    r#"
                    INSERT INTO provider_models
                        (account_id, upstream_model, enabled, available, routable,
                         input_modalities_json, input_modalities_source, metadata_json,
                         pricing_source, pricing_json, last_seen_at, updated_at)
                    VALUES (?, ?, 1, 1, ?, ?, 'discovery', ?, ?, ?, ?, ?)
                    ON CONFLICT(account_id, upstream_model) DO UPDATE SET
                        available = 1,
                        routable = excluded.routable,
                        input_modalities_json = CASE
                            WHEN provider_models.input_modalities_source = 'manual'
                                THEN provider_models.input_modalities_json
                            ELSE excluded.input_modalities_json
                        END,
                        input_modalities_source = CASE
                            WHEN provider_models.input_modalities_source = 'manual'
                                THEN provider_models.input_modalities_source
                            ELSE excluded.input_modalities_source
                        END,
                        metadata_json = excluded.metadata_json,
                        pricing_source = CASE
                            WHEN provider_models.pricing_source = 'manual'
                                THEN provider_models.pricing_source
                            ELSE excluded.pricing_source
                        END,
                        pricing_json = CASE
                            WHEN provider_models.pricing_source = 'manual'
                                THEN provider_models.pricing_json
                            ELSE excluded.pricing_json
                        END,
                        last_seen_at = excluded.last_seen_at,
                        updated_at = excluded.updated_at
                    "#,
                )
                .bind(account_id.as_str())
                .bind(upstream_model)
                .bind(database_bool(model.routable))
                .bind(input_modalities_json)
                .bind(model.metadata_json)
                .bind(pricing_source)
                .bind(pricing_json)
                .bind(synced_at)
                .bind(synced_at)
                .execute(&mut *transaction)
                .await
                .map_err(|error| repository_error("failed to synchronize provider model", error))?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => transaction
                .commit()
                .await
                .map_err(|error| repository_error("failed to commit provider models", error))?,
            Err(error) => {
                let _ = transaction.rollback().await;
                return Err(error);
            }
        }
        self.load_provider_models(Some(account_id)).await
    }

    pub(super) async fn write_provider_model_update(
        &self,
        account_id: &AccountId,
        upstream_model: &str,
        update: ProviderModelOverride,
    ) -> Result<bool, AccountRepositoryError> {
        let (update_pricing, pricing_json) = match update.pricing {
            None => (false, None),
            Some(None) => (true, None),
            Some(Some(pricing)) => (
                true,
                Some(serde_json::to_string(&pricing).map_err(|error| {
                    repository_error("failed to encode provider model pricing", error)
                })?),
            ),
        };
        let input_modalities_json = encode_input_modalities(update.input_modalities.as_deref())?;
        let result = sqlx::query(
            r#"
            UPDATE provider_models
            SET alias = ?, enabled = ?,
                input_modalities_source = CASE
                    WHEN input_modalities_json IS NOT ? THEN 'manual'
                    ELSE input_modalities_source
                END,
                input_modalities_json = ?,
                pricing_source = CASE
                    WHEN ? = 0 THEN pricing_source
                    WHEN ? IS NULL THEN NULL
                    ELSE 'manual'
                END,
                pricing_json = CASE WHEN ? = 0 THEN pricing_json ELSE ? END,
                updated_at = ?
            WHERE account_id = ? AND upstream_model = ?
            "#,
        )
        .bind(update.alias)
        .bind(database_bool(update.enabled))
        .bind(input_modalities_json.as_deref())
        .bind(input_modalities_json)
        .bind(database_bool(update_pricing))
        .bind(pricing_json.as_deref())
        .bind(database_bool(update_pricing))
        .bind(pricing_json)
        .bind(update.updated_at)
        .bind(account_id.as_str())
        .bind(upstream_model)
        .execute(&mut *self.write.lock().await)
        .await
        .map_err(|error| repository_error("failed to update provider model", error))?;
        Ok(result.rows_affected() > 0)
    }
}
