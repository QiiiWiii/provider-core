use std::sync::Arc;

use provider_auth::{
    ApiKeyAuthenticator, AuthError, AuthRepository, CreateApiKeyInput, NewSession, NewUser,
    SessionId, UserId, UserRole,
};
use provider_core::{
    AccountId, CredentialKind, NewCredential, NewProviderAccount, ProviderKind,
    ProviderManagementRepository, ProviderVisibility,
};
use secrecy::SecretString;

use super::SqliteAccountRepository;

#[tokio::test]
async fn api_key_with_multiple_groups_routes_to_the_union_of_visible_accounts() {
    let repository = Arc::new(
        SqliteAccountRepository::in_memory()
            .await
            .expect("in-memory repository"),
    );
    let owner = seed_owner(repository.as_ref(), "multi-group-owner").await;
    seed_account(repository.as_ref(), &owner, "acct-alpha", "alpha").await;
    seed_account(repository.as_ref(), &owner, "acct-beta", "beta").await;

    let api_keys = ApiKeyAuthenticator::load(repository.clone())
        .await
        .expect("API key index");
    let created = api_keys
        .create(CreateApiKeyInput {
            owner_user_id: &owner,
            secret: SecretString::from("multi-group-key"),
            group_labels: vec![" beta ".to_owned(), "alpha".to_owned(), "beta".to_owned()],
            label: "multi".to_owned(),
            expires_at: None,
            quota_limit_usd: None,
            now: 100,
        })
        .await
        .expect("create multi-group key");

    assert_eq!(
        created.summary.group_labels,
        vec!["beta".to_owned(), "alpha".to_owned()]
    );

    let mut account_ids = api_keys
        .account_ids_for_key(&owner, &created.summary.group_labels)
        .await
        .expect("route accounts");
    account_ids.sort();
    assert_eq!(
        account_ids,
        vec!["acct-alpha".to_owned(), "acct-beta".to_owned()]
    );
}

#[tokio::test]
async fn api_key_create_rejects_a_group_without_visible_accounts() {
    let repository = Arc::new(
        SqliteAccountRepository::in_memory()
            .await
            .expect("in-memory repository"),
    );
    let owner = seed_owner(repository.as_ref(), "missing-group-owner").await;
    seed_account(repository.as_ref(), &owner, "acct-alpha", "alpha").await;

    let api_keys = ApiKeyAuthenticator::load(repository)
        .await
        .expect("API key index");
    let result = api_keys
        .create(CreateApiKeyInput {
            owner_user_id: &owner,
            secret: SecretString::from("missing-group-key"),
            group_labels: vec!["alpha".to_owned(), "missing".to_owned()],
            label: "missing".to_owned(),
            expires_at: None,
            quota_limit_usd: None,
            now: 100,
        })
        .await;
    assert!(matches!(result, Err(AuthError::GroupNotFound)));
}

async fn seed_owner(repository: &SqliteAccountRepository, username: &str) -> UserId {
    let owner = UserId::new(username).expect("user ID");
    repository
        .create_initial_user(
            NewUser {
                id: owner.clone(),
                username: username.to_owned(),
                password_hash: "password-hash".to_owned(),
                role: UserRole::SuperAdmin,
                enabled: true,
                created_at: 1,
            },
            NewSession {
                id: SessionId::new(&format!("{username}-session")).expect("session ID"),
                user_id: owner.clone(),
                token_hash: [9; 32],
                expires_at: 300,
                created_at: 100,
            },
        )
        .await
        .expect("create owner");
    owner
}

async fn seed_account(
    repository: &SqliteAccountRepository,
    owner: &UserId,
    account_id: &str,
    group_label: &str,
) {
    repository
        .create_provider_account(
            NewProviderAccount {
                id: AccountId::new(account_id).expect("account ID"),
                provider: ProviderKind::OpenAiCompatible,
                label: group_label.to_owned(),
                group_label: group_label.to_owned(),
                priority: 0,
                config_json: "{}".to_owned(),
                enabled: true,
                credential: NewCredential {
                    kind: CredentialKind::ApiKey,
                    format_version: 1,
                    credential_json: SecretString::from("seed-secret".to_owned()),
                    expires_at: None,
                    last_refreshed_at: None,
                },
            },
            owner.as_str(),
            ProviderVisibility::Private,
        )
        .await
        .expect("seed account");
}
