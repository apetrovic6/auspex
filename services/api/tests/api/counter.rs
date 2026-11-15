use connectrpc::ErrorCode;
use proto::r#gen::buffa::counter::v1::GetRequest;
use proto::r#gen::connect::counter::v1::CounterServiceClient;

use crate::helpers::{client_config, client_transport};

#[tokio::test]
async fn a_database_failure_is_reported_as_internal_and_not_leaked() {
    // The pool points at a port with nothing behind it, so this exercises the error path as
    // a real caller experiences it: a typed ConnectError, not a JSON blob.
    let client = CounterServiceClient::new(client_transport(), client_config());

    let error = client
        .get(GetRequest::default())
        .await
        .expect_err("the store is unreachable");

    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(
        error.message.as_deref(),
        Some("the counter store is unavailable"),
        "the wire message must not carry the driver's error: it can name schema objects, \
         hosts and occasionally data"
    );
}
