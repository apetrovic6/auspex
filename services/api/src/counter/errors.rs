use connectrpc::ConnectError;

#[derive(Debug, thiserror::Error)]
pub enum CounterError {
    /// Anything the database refused. The cause is logged; the wire error deliberately
    /// says nothing about it, because a database message can carry schema details and
    /// occasionally data.
    #[error("the counter store is unavailable")]
    Store(#[from] sea_orm::DbErr),
}

impl From<CounterError> for ConnectError {
    fn from(error: CounterError) -> Self {
        match error {
            CounterError::Store(ref cause) => {
                tracing::error!(%cause, "counter store failure");
                Self::internal(error.to_string())
            }
        }
    }
}
