use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};

use super::test_support::BindingRouter;
use super::*;
use crate::{AccountId, ProviderError};

#[test]
fn response_created_id_is_parsed_across_chunks() {
    let mut pending = BytesMut::from(&b"data: {\"type\":\"response.cre"[..]);
    assert!(take_response_linkage_events(&mut pending).is_empty());
    pending.extend_from_slice(b"ated\",\"response\":{\"id\":\"resp-1\"}}\n\n");
    assert_eq!(
        take_response_linkage_events(&mut pending),
        vec![("response.created".to_owned(), Some("resp-1".to_owned()))]
    );

    let mut bare_cr = BytesMut::from(
        &b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\"}}\r\r"[..],
    );
    assert_eq!(
        take_response_linkage_events(&mut bare_cr),
        vec![("response.completed".to_owned(), Some("resp-1".to_owned()))]
    );
}

#[tokio::test]
async fn binds_stateful_response_ids_at_created_and_stateless_ids_at_completed() {
    let account_id = AccountId::new("account-a").expect("account ID");
    let created = Bytes::from_static(
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\"}}\n\n",
    );
    let completed = Bytes::from_static(
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\"}}\n\n",
    );

    let stateful_bindings = Arc::new(Mutex::new(Vec::new()));
    let stateful_router = Arc::new(BindingRouter {
        bindings: stateful_bindings.clone(),
    });
    let mut stateful = observe_response_id(
        Box::pin(stream::iter(vec![
            Ok::<_, ProviderError>(created.clone()),
            Ok(completed.clone()),
        ])),
        stateful_router,
        "scope".to_owned(),
        account_id.clone(),
        true,
    );
    assert!(stateful.next().await.expect("created item").is_ok());
    assert_eq!(stateful_bindings.lock().expect("bindings lock").len(), 1);
    assert!(stateful.next().await.expect("completed item").is_ok());

    let stateless_bindings = Arc::new(Mutex::new(Vec::new()));
    let stateless_router = Arc::new(BindingRouter {
        bindings: stateless_bindings.clone(),
    });
    let mut stateless = observe_response_id(
        Box::pin(stream::iter(vec![
            Ok::<_, ProviderError>(created),
            Ok(completed),
        ])),
        stateless_router,
        "scope".to_owned(),
        account_id,
        false,
    );
    assert!(stateless.next().await.expect("created item").is_ok());
    assert!(stateless_bindings.lock().expect("bindings lock").is_empty());
    assert!(stateless.next().await.expect("completed item").is_ok());
    assert_eq!(stateless_bindings.lock().expect("bindings lock").len(), 1);
}

#[tokio::test]
async fn disables_response_linkage_after_an_oversized_incomplete_frame() {
    let bindings = Arc::new(Mutex::new(Vec::new()));
    let router = Arc::new(BindingRouter {
        bindings: bindings.clone(),
    });
    let oversized = vec![b'x'; 64 * 1024 + 1];
    let completed = Bytes::from_static(
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\"}}\n\n",
    );
    let mut observed = observe_response_id(
        Box::pin(stream::iter(vec![
            Ok::<_, ProviderError>(Bytes::from(oversized)),
            Ok(completed),
        ])),
        router,
        "scope".to_owned(),
        AccountId::new("account-a").expect("account ID"),
        false,
    );

    assert!(observed.next().await.expect("oversized item").is_ok());
    assert!(observed.next().await.expect("completed item").is_ok());
    assert!(bindings.lock().expect("bindings lock").is_empty());
}
