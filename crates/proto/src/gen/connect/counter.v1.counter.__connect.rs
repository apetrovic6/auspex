///Shorthand for `OwnedView<GetRequestView<'static>>`.
pub type OwnedGetRequestView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::GetRequestView<'static>,
>;
///Shorthand for `OwnedView<GetResponseView<'static>>`.
pub type OwnedGetResponseView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::GetResponseView<'static>,
>;
///Shorthand for `OwnedView<IncrementRequestView<'static>>`.
pub type OwnedIncrementRequestView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::IncrementRequestView<'static>,
>;
///Shorthand for `OwnedView<IncrementResponseView<'static>>`.
pub type OwnedIncrementResponseView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::IncrementResponseView<'static>,
>;
///Shorthand for `OwnedView<DecrementRequestView<'static>>`.
pub type OwnedDecrementRequestView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::DecrementRequestView<'static>,
>;
///Shorthand for `OwnedView<DecrementResponseView<'static>>`.
pub type OwnedDecrementResponseView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::DecrementResponseView<'static>,
>;
///Shorthand for `OwnedView<ResetRequestView<'static>>`.
pub type OwnedResetRequestView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::ResetRequestView<'static>,
>;
///Shorthand for `OwnedView<ResetResponseView<'static>>`.
pub type OwnedResetResponseView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::ResetResponseView<'static>,
>;
///Shorthand for `OwnedView<WhoamiRequestView<'static>>`.
pub type OwnedWhoamiRequestView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::WhoamiRequestView<'static>,
>;
///Shorthand for `OwnedView<WhoamiResponseView<'static>>`.
pub type OwnedWhoamiResponseView = ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::WhoamiResponseView<'static>,
>;
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::GetResponse>
for crate::r#gen::buffa::counter::v1::__buffa::view::GetResponseView<'_> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self, codec)
    }
}
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::GetResponse>
for ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::GetResponseView<'static>,
> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self.reborrow(), codec)
    }
    /// An `OwnedView` still holds the buffer it was decoded from, so
    /// its large fields can be handed to the response body by
    /// reference count instead of copied. The bare view impl above
    /// cannot do this: it has borrows but no buffer to name.
    fn encode_segments(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::connectrpc::EncodedBody, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body_segments(
            self.reborrow(),
            self.bytes(),
            codec,
        )
    }
}
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::IncrementResponse>
for crate::r#gen::buffa::counter::v1::__buffa::view::IncrementResponseView<'_> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self, codec)
    }
}
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::IncrementResponse>
for ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::IncrementResponseView<'static>,
> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self.reborrow(), codec)
    }
    /// An `OwnedView` still holds the buffer it was decoded from, so
    /// its large fields can be handed to the response body by
    /// reference count instead of copied. The bare view impl above
    /// cannot do this: it has borrows but no buffer to name.
    fn encode_segments(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::connectrpc::EncodedBody, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body_segments(
            self.reborrow(),
            self.bytes(),
            codec,
        )
    }
}
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::DecrementResponse>
for crate::r#gen::buffa::counter::v1::__buffa::view::DecrementResponseView<'_> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self, codec)
    }
}
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::DecrementResponse>
for ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::DecrementResponseView<'static>,
> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self.reborrow(), codec)
    }
    /// An `OwnedView` still holds the buffer it was decoded from, so
    /// its large fields can be handed to the response body by
    /// reference count instead of copied. The bare view impl above
    /// cannot do this: it has borrows but no buffer to name.
    fn encode_segments(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::connectrpc::EncodedBody, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body_segments(
            self.reborrow(),
            self.bytes(),
            codec,
        )
    }
}
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::ResetResponse>
for crate::r#gen::buffa::counter::v1::__buffa::view::ResetResponseView<'_> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self, codec)
    }
}
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::ResetResponse>
for ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::ResetResponseView<'static>,
> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self.reborrow(), codec)
    }
    /// An `OwnedView` still holds the buffer it was decoded from, so
    /// its large fields can be handed to the response body by
    /// reference count instead of copied. The bare view impl above
    /// cannot do this: it has borrows but no buffer to name.
    fn encode_segments(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::connectrpc::EncodedBody, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body_segments(
            self.reborrow(),
            self.bytes(),
            codec,
        )
    }
}
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::WhoamiResponse>
for crate::r#gen::buffa::counter::v1::__buffa::view::WhoamiResponseView<'_> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self, codec)
    }
}
impl ::connectrpc::Encodable<crate::r#gen::buffa::counter::v1::WhoamiResponse>
for ::buffa::view::OwnedView<
    crate::r#gen::buffa::counter::v1::__buffa::view::WhoamiResponseView<'static>,
> {
    fn encode(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::buffa::bytes::Bytes, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body(self.reborrow(), codec)
    }
    /// An `OwnedView` still holds the buffer it was decoded from, so
    /// its large fields can be handed to the response body by
    /// reference count instead of copied. The bare view impl above
    /// cannot do this: it has borrows but no buffer to name.
    fn encode_segments(
        &self,
        codec: ::connectrpc::CodecFormat,
    ) -> ::std::result::Result<::connectrpc::EncodedBody, ::connectrpc::ConnectError> {
        ::connectrpc::__codegen::encode_view_body_segments(
            self.reborrow(),
            self.bytes(),
            codec,
        )
    }
}
/// Full service name for this service.
pub const COUNTER_SERVICE_SERVICE_NAME: &str = "counter.v1.CounterService";
/// Static [`Spec`](::connectrpc::Spec) for the `Get` RPC, as seen by the server; the generated client passes it with [`origin`](::connectrpc::Spec::origin) `Client` (compare across sides with [`Spec::same_method`](::connectrpc::Spec::same_method)).
pub const COUNTER_SERVICE_GET_SPEC: ::connectrpc::Spec = ::connectrpc::Spec::server(
        "/counter.v1.CounterService/Get",
        ::connectrpc::StreamType::Unary,
    )
    .with_idempotency_level(::connectrpc::IdempotencyLevel::Unknown);
/// Static [`Spec`](::connectrpc::Spec) for the `Increment` RPC, as seen by the server; the generated client passes it with [`origin`](::connectrpc::Spec::origin) `Client` (compare across sides with [`Spec::same_method`](::connectrpc::Spec::same_method)).
pub const COUNTER_SERVICE_INCREMENT_SPEC: ::connectrpc::Spec = ::connectrpc::Spec::server(
        "/counter.v1.CounterService/Increment",
        ::connectrpc::StreamType::Unary,
    )
    .with_idempotency_level(::connectrpc::IdempotencyLevel::Unknown);
/// Static [`Spec`](::connectrpc::Spec) for the `Decrement` RPC, as seen by the server; the generated client passes it with [`origin`](::connectrpc::Spec::origin) `Client` (compare across sides with [`Spec::same_method`](::connectrpc::Spec::same_method)).
pub const COUNTER_SERVICE_DECREMENT_SPEC: ::connectrpc::Spec = ::connectrpc::Spec::server(
        "/counter.v1.CounterService/Decrement",
        ::connectrpc::StreamType::Unary,
    )
    .with_idempotency_level(::connectrpc::IdempotencyLevel::Unknown);
/// Static [`Spec`](::connectrpc::Spec) for the `Reset` RPC, as seen by the server; the generated client passes it with [`origin`](::connectrpc::Spec::origin) `Client` (compare across sides with [`Spec::same_method`](::connectrpc::Spec::same_method)).
pub const COUNTER_SERVICE_RESET_SPEC: ::connectrpc::Spec = ::connectrpc::Spec::server(
        "/counter.v1.CounterService/Reset",
        ::connectrpc::StreamType::Unary,
    )
    .with_idempotency_level(::connectrpc::IdempotencyLevel::Unknown);
/// Static [`Spec`](::connectrpc::Spec) for the `Whoami` RPC, as seen by the server; the generated client passes it with [`origin`](::connectrpc::Spec::origin) `Client` (compare across sides with [`Spec::same_method`](::connectrpc::Spec::same_method)).
pub const COUNTER_SERVICE_WHOAMI_SPEC: ::connectrpc::Spec = ::connectrpc::Spec::server(
        "/counter.v1.CounterService/Whoami",
        ::connectrpc::StreamType::Unary,
    )
    .with_idempotency_level(::connectrpc::IdempotencyLevel::Unknown);
/// Server trait for CounterService.
///
/// # Implementing handlers
///
/// Implement methods with plain `async fn`; the returned future satisfies
/// the `Send` bound automatically.
///
/// **Unary and server-streaming requests** arrive as
/// [`ServiceRequest<'_, Req>`](::connectrpc::ServiceRequest): a zero-copy
/// view of the request plus its body, valid for the duration of the call.
/// Fields are read directly (`request.name` is a `&str` into the decoded
/// buffer) and the borrow may be held across `.await` points. Anything
/// that must outlive the call — `tokio::spawn`, channels, server state,
/// or data captured by a returned response stream — takes owned data:
/// call `request.to_owned_message()` (or copy the specific fields)
/// first.
///
/// **Client-streaming and bidi requests** arrive as
/// [`InboundStream<Req>`](::connectrpc::InboundStream) — a
/// `ServiceStream` of [`StreamMessage`](::connectrpc::StreamMessage)s.
/// Each item owns its decoded buffer and is `Send + 'static`, so items
/// can be buffered or moved into spawned tasks; read fields zero-copy
/// through the generated accessor methods (`item.name()`) or `.view()`,
/// convert with `.to_owned_message()`, or yield an item back unchanged —
/// `StreamMessage<M>` implements `Encodable<M>`.
///
/// Request types resolved through `extern_path` (e.g. well-known types
/// from another crate) use the same wrappers; the crate that owns the
/// type must be generated with buffa ≥ 0.9.0 and views enabled so the
/// backing `HasMessageView` impl exists.
///
/// The `impl Encodable<Out>` return bound accepts the owned `Out`, the
/// generated `OutView<'_>` / `OwnedOutView`,
/// [`MaybeBorrowed`](::connectrpc::MaybeBorrowed), or
/// [`PreEncoded`](::connectrpc::PreEncoded) for handlers that encode a
/// non-`'static` view internally and pass the bytes across the handler
/// boundary. View bodies are not emitted for output types mapped via
/// `extern_path` (the impl would be an orphan); return owned for
/// WKT/extern outputs.
///
/// Server-streaming and bidi-streaming methods return
/// `ServiceStream<impl Encodable<Out> + Send + use<Self>>`. The
/// `use<Self>` precise-capturing clause excludes `&self`'s lifetime and
/// the request's lifetime (unary methods use `use<'a, Self>` and may
/// borrow from `&self`), so stream items must be `'static` and cannot
/// borrow from the request. To stream view-encoded data, encode each
/// item inside the stream body and yield
/// [`PreEncoded`](::connectrpc::PreEncoded) — see its `# Streaming
/// example` doc.
#[allow(clippy::type_complexity)]
pub trait CounterService: Send + Sync + 'static {
    /// Handle the Get RPC.
    ///
    /// `'a` lets the response body borrow from `&self` (e.g. server-resident state).
    ///
    /// `request` is borrowed from the request body and is valid for the
    /// duration of the call; message fields are read directly on it
    /// (zero-copy). The response cannot borrow from `request` — use
    /// `.to_owned_message()` (or copy the specific fields) for anything
    /// returned, stored, or moved into `tokio::spawn`.
    fn get<'a>(
        &'a self,
        ctx: ::connectrpc::RequestContext,
        request: ::connectrpc::ServiceRequest<
            '_,
            crate::r#gen::buffa::counter::v1::GetRequest,
        >,
    ) -> impl ::std::future::Future<
        Output = ::connectrpc::ServiceResult<
            impl ::connectrpc::Encodable<
                crate::r#gen::buffa::counter::v1::GetResponse,
            > + Send + use<'a, Self>,
        >,
    > + Send;
    /// Handle the Increment RPC.
    ///
    /// `'a` lets the response body borrow from `&self` (e.g. server-resident state).
    ///
    /// `request` is borrowed from the request body and is valid for the
    /// duration of the call; message fields are read directly on it
    /// (zero-copy). The response cannot borrow from `request` — use
    /// `.to_owned_message()` (or copy the specific fields) for anything
    /// returned, stored, or moved into `tokio::spawn`.
    fn increment<'a>(
        &'a self,
        ctx: ::connectrpc::RequestContext,
        request: ::connectrpc::ServiceRequest<
            '_,
            crate::r#gen::buffa::counter::v1::IncrementRequest,
        >,
    ) -> impl ::std::future::Future<
        Output = ::connectrpc::ServiceResult<
            impl ::connectrpc::Encodable<
                crate::r#gen::buffa::counter::v1::IncrementResponse,
            > + Send + use<'a, Self>,
        >,
    > + Send;
    /// Handle the Decrement RPC.
    ///
    /// `'a` lets the response body borrow from `&self` (e.g. server-resident state).
    ///
    /// `request` is borrowed from the request body and is valid for the
    /// duration of the call; message fields are read directly on it
    /// (zero-copy). The response cannot borrow from `request` — use
    /// `.to_owned_message()` (or copy the specific fields) for anything
    /// returned, stored, or moved into `tokio::spawn`.
    fn decrement<'a>(
        &'a self,
        ctx: ::connectrpc::RequestContext,
        request: ::connectrpc::ServiceRequest<
            '_,
            crate::r#gen::buffa::counter::v1::DecrementRequest,
        >,
    ) -> impl ::std::future::Future<
        Output = ::connectrpc::ServiceResult<
            impl ::connectrpc::Encodable<
                crate::r#gen::buffa::counter::v1::DecrementResponse,
            > + Send + use<'a, Self>,
        >,
    > + Send;
    /// Handle the Reset RPC.
    ///
    /// `'a` lets the response body borrow from `&self` (e.g. server-resident state).
    ///
    /// `request` is borrowed from the request body and is valid for the
    /// duration of the call; message fields are read directly on it
    /// (zero-copy). The response cannot borrow from `request` — use
    /// `.to_owned_message()` (or copy the specific fields) for anything
    /// returned, stored, or moved into `tokio::spawn`.
    fn reset<'a>(
        &'a self,
        ctx: ::connectrpc::RequestContext,
        request: ::connectrpc::ServiceRequest<
            '_,
            crate::r#gen::buffa::counter::v1::ResetRequest,
        >,
    ) -> impl ::std::future::Future<
        Output = ::connectrpc::ServiceResult<
            impl ::connectrpc::Encodable<
                crate::r#gen::buffa::counter::v1::ResetResponse,
            > + Send + use<'a, Self>,
        >,
    > + Send;
    /// Requires an identity; the four above do not.
    ///
    /// It exists to demonstrate the seam (crates/identity) and nothing else: it needs an
    /// identity and no database, which is what makes the authenticated path assertable in CI.
    /// The core never emits it, so no shell maps it and no binding is regenerated for it.
    ///
    /// A product deletes it the day a real method needs protecting — and `tests/api/auth.rs`
    /// fails until that method is classified, which is the point of the test. Deleting it
    /// without moving the classification pair AND the authenticated-success test onto the
    /// replacement leaves the seam with no positive coverage at all, so all three move in the
    /// same commit.
    ///
    /// `'a` lets the response body borrow from `&self` (e.g. server-resident state).
    ///
    /// `request` is borrowed from the request body and is valid for the
    /// duration of the call; message fields are read directly on it
    /// (zero-copy). The response cannot borrow from `request` — use
    /// `.to_owned_message()` (or copy the specific fields) for anything
    /// returned, stored, or moved into `tokio::spawn`.
    fn whoami<'a>(
        &'a self,
        ctx: ::connectrpc::RequestContext,
        request: ::connectrpc::ServiceRequest<
            '_,
            crate::r#gen::buffa::counter::v1::WhoamiRequest,
        >,
    ) -> impl ::std::future::Future<
        Output = ::connectrpc::ServiceResult<
            impl ::connectrpc::Encodable<
                crate::r#gen::buffa::counter::v1::WhoamiResponse,
            > + Send + use<'a, Self>,
        >,
    > + Send;
}
/// Extension trait for registering a service implementation with a Router.
///
/// This trait is automatically implemented for all types that implement the service trait.
/// Prefer [`Router::add_service`](::connectrpc::Router::add_service) for
/// top-down registration; `register` remains available for compatibility
/// and cases where the service-first call shape is more convenient.
///
/// # Example
///
/// ```rust,ignore
/// use std::sync::Arc;
///
/// let service = Arc::new(MyServiceImpl);
/// let router = service.register(Router::new());
/// ```
pub trait CounterServiceExt: CounterService {
    /// Register this service implementation with a Router.
    ///
    /// Takes ownership of the `Arc<Self>` and returns a new Router with
    /// this service's methods registered.
    fn register(
        self: ::std::sync::Arc<Self>,
        router: ::connectrpc::Router,
    ) -> ::connectrpc::Router;
}
impl<S: CounterService> CounterServiceExt for S {
    fn register(
        self: ::std::sync::Arc<Self>,
        router: ::connectrpc::Router,
    ) -> ::connectrpc::Router {
        router
            .route_view(
                COUNTER_SERVICE_SERVICE_NAME,
                "Get",
                {
                    let svc = ::std::sync::Arc::clone(&self);
                    ::connectrpc::view_handler_fn(move |
                        ctx,
                        req: ::buffa::view::OwnedView<
                            crate::r#gen::buffa::counter::v1::__buffa::view::GetRequestView<
                                'static,
                            >,
                        >,
                        format|
                    {
                        let svc = ::std::sync::Arc::clone(&svc);
                        async move {
                            let sreq = ::connectrpc::ServiceRequest::<
                                crate::r#gen::buffa::counter::v1::GetRequest,
                            >::from_parts(req.reborrow(), req.bytes());
                            svc.get(ctx, sreq)
                                .await?
                                .encode::<
                                    crate::r#gen::buffa::counter::v1::GetResponse,
                                >(format)
                        }
                    })
                },
            )
            .with_spec(COUNTER_SERVICE_GET_SPEC)
            .route_view(
                COUNTER_SERVICE_SERVICE_NAME,
                "Increment",
                {
                    let svc = ::std::sync::Arc::clone(&self);
                    ::connectrpc::view_handler_fn(move |
                        ctx,
                        req: ::buffa::view::OwnedView<
                            crate::r#gen::buffa::counter::v1::__buffa::view::IncrementRequestView<
                                'static,
                            >,
                        >,
                        format|
                    {
                        let svc = ::std::sync::Arc::clone(&svc);
                        async move {
                            let sreq = ::connectrpc::ServiceRequest::<
                                crate::r#gen::buffa::counter::v1::IncrementRequest,
                            >::from_parts(req.reborrow(), req.bytes());
                            svc.increment(ctx, sreq)
                                .await?
                                .encode::<
                                    crate::r#gen::buffa::counter::v1::IncrementResponse,
                                >(format)
                        }
                    })
                },
            )
            .with_spec(COUNTER_SERVICE_INCREMENT_SPEC)
            .route_view(
                COUNTER_SERVICE_SERVICE_NAME,
                "Decrement",
                {
                    let svc = ::std::sync::Arc::clone(&self);
                    ::connectrpc::view_handler_fn(move |
                        ctx,
                        req: ::buffa::view::OwnedView<
                            crate::r#gen::buffa::counter::v1::__buffa::view::DecrementRequestView<
                                'static,
                            >,
                        >,
                        format|
                    {
                        let svc = ::std::sync::Arc::clone(&svc);
                        async move {
                            let sreq = ::connectrpc::ServiceRequest::<
                                crate::r#gen::buffa::counter::v1::DecrementRequest,
                            >::from_parts(req.reborrow(), req.bytes());
                            svc.decrement(ctx, sreq)
                                .await?
                                .encode::<
                                    crate::r#gen::buffa::counter::v1::DecrementResponse,
                                >(format)
                        }
                    })
                },
            )
            .with_spec(COUNTER_SERVICE_DECREMENT_SPEC)
            .route_view(
                COUNTER_SERVICE_SERVICE_NAME,
                "Reset",
                {
                    let svc = ::std::sync::Arc::clone(&self);
                    ::connectrpc::view_handler_fn(move |
                        ctx,
                        req: ::buffa::view::OwnedView<
                            crate::r#gen::buffa::counter::v1::__buffa::view::ResetRequestView<
                                'static,
                            >,
                        >,
                        format|
                    {
                        let svc = ::std::sync::Arc::clone(&svc);
                        async move {
                            let sreq = ::connectrpc::ServiceRequest::<
                                crate::r#gen::buffa::counter::v1::ResetRequest,
                            >::from_parts(req.reborrow(), req.bytes());
                            svc.reset(ctx, sreq)
                                .await?
                                .encode::<
                                    crate::r#gen::buffa::counter::v1::ResetResponse,
                                >(format)
                        }
                    })
                },
            )
            .with_spec(COUNTER_SERVICE_RESET_SPEC)
            .route_view(
                COUNTER_SERVICE_SERVICE_NAME,
                "Whoami",
                {
                    let svc = ::std::sync::Arc::clone(&self);
                    ::connectrpc::view_handler_fn(move |
                        ctx,
                        req: ::buffa::view::OwnedView<
                            crate::r#gen::buffa::counter::v1::__buffa::view::WhoamiRequestView<
                                'static,
                            >,
                        >,
                        format|
                    {
                        let svc = ::std::sync::Arc::clone(&svc);
                        async move {
                            let sreq = ::connectrpc::ServiceRequest::<
                                crate::r#gen::buffa::counter::v1::WhoamiRequest,
                            >::from_parts(req.reborrow(), req.bytes());
                            svc.whoami(ctx, sreq)
                                .await?
                                .encode::<
                                    crate::r#gen::buffa::counter::v1::WhoamiResponse,
                                >(format)
                        }
                    })
                },
            )
            .with_spec(COUNTER_SERVICE_WHOAMI_SPEC)
    }
}
/// Type-inference marker used by [`Router::add_service`](::connectrpc::Router::add_service).
#[doc(hidden)]
pub struct CounterServiceRegisterMarker;
impl<S: CounterService> ::connectrpc::ServiceRegister<CounterServiceRegisterMarker>
for ::std::sync::Arc<S> {
    fn register_service(self, router: ::connectrpc::Router) -> ::connectrpc::Router {
        <S as CounterServiceExt>::register(self, router)
    }
}
/// Monomorphic dispatcher for `CounterService`.
///
/// Unlike `.register(Router)` which type-erases each method into an `Arc<dyn ErasedHandler>` stored in a `HashMap`, this struct dispatches via a compile-time `match` on method name: no vtable, no hash lookup.
///
/// # Example
///
/// ```rust,ignore
/// use connectrpc::ConnectRpcService;
///
/// let server = CounterServiceServer::new(MyImpl);
/// let service = ConnectRpcService::new(server);
/// // hand `service` to axum/hyper as a fallback_service
/// ```
pub struct CounterServiceServer<T> {
    inner: ::std::sync::Arc<T>,
}
impl<T: CounterService> CounterServiceServer<T> {
    /// Wrap a service implementation in a monomorphic dispatcher.
    pub fn new(service: T) -> Self {
        Self {
            inner: ::std::sync::Arc::new(service),
        }
    }
    /// Wrap an already-`Arc`'d service implementation.
    pub fn from_arc(inner: ::std::sync::Arc<T>) -> Self {
        Self { inner }
    }
}
impl<T> Clone for CounterServiceServer<T> {
    fn clone(&self) -> Self {
        Self {
            inner: ::std::sync::Arc::clone(&self.inner),
        }
    }
}
impl<T: CounterService> ::connectrpc::Dispatcher for CounterServiceServer<T> {
    #[inline]
    fn lookup(
        &self,
        path: &str,
    ) -> Option<::connectrpc::dispatcher::codegen::MethodDescriptor> {
        let method = path.strip_prefix("counter.v1.CounterService/")?;
        match method {
            "Get" => {
                Some(
                    ::connectrpc::dispatcher::codegen::MethodDescriptor::unary(false)
                        .with_spec(COUNTER_SERVICE_GET_SPEC),
                )
            }
            "Increment" => {
                Some(
                    ::connectrpc::dispatcher::codegen::MethodDescriptor::unary(false)
                        .with_spec(COUNTER_SERVICE_INCREMENT_SPEC),
                )
            }
            "Decrement" => {
                Some(
                    ::connectrpc::dispatcher::codegen::MethodDescriptor::unary(false)
                        .with_spec(COUNTER_SERVICE_DECREMENT_SPEC),
                )
            }
            "Reset" => {
                Some(
                    ::connectrpc::dispatcher::codegen::MethodDescriptor::unary(false)
                        .with_spec(COUNTER_SERVICE_RESET_SPEC),
                )
            }
            "Whoami" => {
                Some(
                    ::connectrpc::dispatcher::codegen::MethodDescriptor::unary(false)
                        .with_spec(COUNTER_SERVICE_WHOAMI_SPEC),
                )
            }
            _ => None,
        }
    }
    fn call_unary(
        &self,
        path: &str,
        ctx: ::connectrpc::RequestContext,
        request: ::connectrpc::Payload,
        format: ::connectrpc::CodecFormat,
    ) -> ::connectrpc::dispatcher::codegen::UnaryResult {
        let Some(method) = path.strip_prefix("counter.v1.CounterService/") else {
            return ::connectrpc::dispatcher::codegen::unimplemented_unary(path);
        };
        let _ = (&ctx, &request, &format);
        match method {
            "Get" => {
                let svc = ::std::sync::Arc::clone(&self.inner);
                Box::pin(async move {
                    let body = ::connectrpc::dispatcher::codegen::request_proto_bytes::<
                        crate::r#gen::buffa::counter::v1::GetRequest,
                    >(request.encoded()?, format)?;
                    let req: crate::r#gen::buffa::counter::v1::__buffa::view::GetRequestView<
                        '_,
                    > = ::connectrpc::dispatcher::codegen::decode_borrowed_request_view(
                        &body,
                        ctx.decode_options(),
                    )?;
                    let req = ::connectrpc::ServiceRequest::<
                        crate::r#gen::buffa::counter::v1::GetRequest,
                    >::from_parts(&req, &body);
                    svc.get(ctx, req)
                        .await?
                        .encode::<crate::r#gen::buffa::counter::v1::GetResponse>(format)
                })
            }
            "Increment" => {
                let svc = ::std::sync::Arc::clone(&self.inner);
                Box::pin(async move {
                    let body = ::connectrpc::dispatcher::codegen::request_proto_bytes::<
                        crate::r#gen::buffa::counter::v1::IncrementRequest,
                    >(request.encoded()?, format)?;
                    let req: crate::r#gen::buffa::counter::v1::__buffa::view::IncrementRequestView<
                        '_,
                    > = ::connectrpc::dispatcher::codegen::decode_borrowed_request_view(
                        &body,
                        ctx.decode_options(),
                    )?;
                    let req = ::connectrpc::ServiceRequest::<
                        crate::r#gen::buffa::counter::v1::IncrementRequest,
                    >::from_parts(&req, &body);
                    svc.increment(ctx, req)
                        .await?
                        .encode::<
                            crate::r#gen::buffa::counter::v1::IncrementResponse,
                        >(format)
                })
            }
            "Decrement" => {
                let svc = ::std::sync::Arc::clone(&self.inner);
                Box::pin(async move {
                    let body = ::connectrpc::dispatcher::codegen::request_proto_bytes::<
                        crate::r#gen::buffa::counter::v1::DecrementRequest,
                    >(request.encoded()?, format)?;
                    let req: crate::r#gen::buffa::counter::v1::__buffa::view::DecrementRequestView<
                        '_,
                    > = ::connectrpc::dispatcher::codegen::decode_borrowed_request_view(
                        &body,
                        ctx.decode_options(),
                    )?;
                    let req = ::connectrpc::ServiceRequest::<
                        crate::r#gen::buffa::counter::v1::DecrementRequest,
                    >::from_parts(&req, &body);
                    svc.decrement(ctx, req)
                        .await?
                        .encode::<
                            crate::r#gen::buffa::counter::v1::DecrementResponse,
                        >(format)
                })
            }
            "Reset" => {
                let svc = ::std::sync::Arc::clone(&self.inner);
                Box::pin(async move {
                    let body = ::connectrpc::dispatcher::codegen::request_proto_bytes::<
                        crate::r#gen::buffa::counter::v1::ResetRequest,
                    >(request.encoded()?, format)?;
                    let req: crate::r#gen::buffa::counter::v1::__buffa::view::ResetRequestView<
                        '_,
                    > = ::connectrpc::dispatcher::codegen::decode_borrowed_request_view(
                        &body,
                        ctx.decode_options(),
                    )?;
                    let req = ::connectrpc::ServiceRequest::<
                        crate::r#gen::buffa::counter::v1::ResetRequest,
                    >::from_parts(&req, &body);
                    svc.reset(ctx, req)
                        .await?
                        .encode::<
                            crate::r#gen::buffa::counter::v1::ResetResponse,
                        >(format)
                })
            }
            "Whoami" => {
                let svc = ::std::sync::Arc::clone(&self.inner);
                Box::pin(async move {
                    let body = ::connectrpc::dispatcher::codegen::request_proto_bytes::<
                        crate::r#gen::buffa::counter::v1::WhoamiRequest,
                    >(request.encoded()?, format)?;
                    let req: crate::r#gen::buffa::counter::v1::__buffa::view::WhoamiRequestView<
                        '_,
                    > = ::connectrpc::dispatcher::codegen::decode_borrowed_request_view(
                        &body,
                        ctx.decode_options(),
                    )?;
                    let req = ::connectrpc::ServiceRequest::<
                        crate::r#gen::buffa::counter::v1::WhoamiRequest,
                    >::from_parts(&req, &body);
                    svc.whoami(ctx, req)
                        .await?
                        .encode::<
                            crate::r#gen::buffa::counter::v1::WhoamiResponse,
                        >(format)
                })
            }
            _ => ::connectrpc::dispatcher::codegen::unimplemented_unary(path),
        }
    }
    fn call_server_streaming(
        &self,
        path: &str,
        ctx: ::connectrpc::RequestContext,
        request: ::buffa::bytes::Bytes,
        format: ::connectrpc::CodecFormat,
    ) -> ::connectrpc::dispatcher::codegen::StreamingResult {
        let Some(method) = path.strip_prefix("counter.v1.CounterService/") else {
            return ::connectrpc::dispatcher::codegen::unimplemented_streaming(path);
        };
        let _ = (&ctx, &request, &format);
        match method {
            _ => ::connectrpc::dispatcher::codegen::unimplemented_streaming(path),
        }
    }
    fn call_client_streaming(
        &self,
        path: &str,
        ctx: ::connectrpc::RequestContext,
        requests: ::connectrpc::dispatcher::codegen::RequestStream,
        format: ::connectrpc::CodecFormat,
    ) -> ::connectrpc::dispatcher::codegen::UnaryResult {
        let Some(method) = path.strip_prefix("counter.v1.CounterService/") else {
            return ::connectrpc::dispatcher::codegen::unimplemented_unary(path);
        };
        let _ = (&ctx, &requests, &format);
        match method {
            _ => ::connectrpc::dispatcher::codegen::unimplemented_unary(path),
        }
    }
    fn call_bidi_streaming(
        &self,
        path: &str,
        ctx: ::connectrpc::RequestContext,
        requests: ::connectrpc::dispatcher::codegen::RequestStream,
        format: ::connectrpc::CodecFormat,
    ) -> ::connectrpc::dispatcher::codegen::StreamingResult {
        let Some(method) = path.strip_prefix("counter.v1.CounterService/") else {
            return ::connectrpc::dispatcher::codegen::unimplemented_streaming(path);
        };
        let _ = (&ctx, &requests, &format);
        match method {
            _ => ::connectrpc::dispatcher::codegen::unimplemented_streaming(path),
        }
    }
}
/// Client for this service.
///
/// Generic over `T: ClientTransport`. For **gRPC** (HTTP/2), use
/// `Http2Connection` — it has honest `poll_ready` and composes with
/// `tower::balance` for multi-connection load balancing. For **Connect
/// over HTTP/1.1** (or unknown protocol), use `HttpClient`.
///
/// # Example (gRPC / HTTP/2)
///
/// ```rust,ignore
/// use connectrpc::client::{Http2Connection, ClientConfig};
/// use connectrpc::Protocol;
///
/// let uri: http::Uri = "http://localhost:8080".parse()?;
/// let conn = Http2Connection::connect_plaintext(uri.clone()).await?.shared(1024);
/// let config = ClientConfig::new(uri).with_protocol(Protocol::Grpc);
///
/// let client = CounterServiceClient::new(conn, config);
/// let response = client.get(request).await?;
/// ```
///
/// # Example (Connect / HTTP/1.1 or ALPN)
///
/// ```rust,ignore
/// use connectrpc::client::{HttpClient, ClientConfig};
///
/// let http = HttpClient::plaintext();  // cleartext http:// only
/// let config = ClientConfig::new("http://localhost:8080".parse()?);
///
/// let client = CounterServiceClient::new(http, config);
/// let response = client.get(request).await?;
/// ```
///
/// # Working with the response
///
/// Unary calls return [`UnaryResponse<OwnedView<FooView>>`](::connectrpc::client::UnaryResponse).
/// [`view()`](::connectrpc::client::UnaryResponse::view) borrows the response
/// message, so field access is zero-copy:
///
/// ```rust,ignore
/// let resp = client.get(request).await?;
/// let name: &str = resp.view().name;  // borrow into the response buffer
/// ```
///
/// If you need the owned struct (e.g. to store or pass by value), use
/// [`into_owned()`](::connectrpc::client::UnaryResponse::into_owned):
///
/// ```rust,ignore
/// let owned = client.get(request).await?.into_owned();
/// ```
///
/// [`into_view()`](::connectrpc::client::UnaryResponse::into_view) keeps the
/// zero-copy decoded body (an `OwnedView`) without copying; field access on it
/// goes through `.reborrow()`. Streaming responses yield one
/// [`StreamMessage`](::connectrpc::StreamMessage) per received message from
/// `.message().await` — read fields zero-copy through the generated accessor
/// methods (`msg.name()`) or `.view()`, or convert with `.to_owned_message()`.
#[derive(Clone)]
pub struct CounterServiceClient<T> {
    transport: T,
    config: ::connectrpc::client::ClientConfig,
}
impl<T> CounterServiceClient<T>
where
    T: ::connectrpc::client::ClientTransport,
    <T::ResponseBody as ::connectrpc::http_body::Body>::Error: ::std::fmt::Display,
{
    /// Create a new client with the given transport and configuration.
    pub fn new(transport: T, config: ::connectrpc::client::ClientConfig) -> Self {
        Self { transport, config }
    }
    /// Get the client configuration.
    pub fn config(&self) -> &::connectrpc::client::ClientConfig {
        &self.config
    }
    /// Get a mutable reference to the client configuration.
    pub fn config_mut(&mut self) -> &mut ::connectrpc::client::ClientConfig {
        &mut self.config
    }
    /// Call the Get RPC. Sends a request to /counter.v1.CounterService/Get.
    pub async fn get(
        &self,
        request: crate::r#gen::buffa::counter::v1::GetRequest,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::GetResponseView<'static>,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        self.get_with_options(request, ::connectrpc::client::CallOptions::default())
            .await
    }
    /// Call the Get RPC with explicit per-call options. Options override [`ClientConfig`](::connectrpc::client::ClientConfig) defaults.
    pub async fn get_with_options(
        &self,
        request: crate::r#gen::buffa::counter::v1::GetRequest,
        options: ::connectrpc::client::CallOptions,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::GetResponseView<'static>,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        ::connectrpc::client::call_unary(
                &self.transport,
                &self.config,
                COUNTER_SERVICE_GET_SPEC.with_origin(::connectrpc::SpecOrigin::Client),
                request,
                options,
            )
            .await
    }
    /// Call the Increment RPC. Sends a request to /counter.v1.CounterService/Increment.
    pub async fn increment(
        &self,
        request: crate::r#gen::buffa::counter::v1::IncrementRequest,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::IncrementResponseView<
                    'static,
                >,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        self.increment_with_options(
                request,
                ::connectrpc::client::CallOptions::default(),
            )
            .await
    }
    /// Call the Increment RPC with explicit per-call options. Options override [`ClientConfig`](::connectrpc::client::ClientConfig) defaults.
    pub async fn increment_with_options(
        &self,
        request: crate::r#gen::buffa::counter::v1::IncrementRequest,
        options: ::connectrpc::client::CallOptions,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::IncrementResponseView<
                    'static,
                >,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        ::connectrpc::client::call_unary(
                &self.transport,
                &self.config,
                COUNTER_SERVICE_INCREMENT_SPEC
                    .with_origin(::connectrpc::SpecOrigin::Client),
                request,
                options,
            )
            .await
    }
    /// Call the Decrement RPC. Sends a request to /counter.v1.CounterService/Decrement.
    pub async fn decrement(
        &self,
        request: crate::r#gen::buffa::counter::v1::DecrementRequest,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::DecrementResponseView<
                    'static,
                >,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        self.decrement_with_options(
                request,
                ::connectrpc::client::CallOptions::default(),
            )
            .await
    }
    /// Call the Decrement RPC with explicit per-call options. Options override [`ClientConfig`](::connectrpc::client::ClientConfig) defaults.
    pub async fn decrement_with_options(
        &self,
        request: crate::r#gen::buffa::counter::v1::DecrementRequest,
        options: ::connectrpc::client::CallOptions,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::DecrementResponseView<
                    'static,
                >,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        ::connectrpc::client::call_unary(
                &self.transport,
                &self.config,
                COUNTER_SERVICE_DECREMENT_SPEC
                    .with_origin(::connectrpc::SpecOrigin::Client),
                request,
                options,
            )
            .await
    }
    /// Call the Reset RPC. Sends a request to /counter.v1.CounterService/Reset.
    pub async fn reset(
        &self,
        request: crate::r#gen::buffa::counter::v1::ResetRequest,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::ResetResponseView<
                    'static,
                >,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        self.reset_with_options(request, ::connectrpc::client::CallOptions::default())
            .await
    }
    /// Call the Reset RPC with explicit per-call options. Options override [`ClientConfig`](::connectrpc::client::ClientConfig) defaults.
    pub async fn reset_with_options(
        &self,
        request: crate::r#gen::buffa::counter::v1::ResetRequest,
        options: ::connectrpc::client::CallOptions,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::ResetResponseView<
                    'static,
                >,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        ::connectrpc::client::call_unary(
                &self.transport,
                &self.config,
                COUNTER_SERVICE_RESET_SPEC.with_origin(::connectrpc::SpecOrigin::Client),
                request,
                options,
            )
            .await
    }
    /// Call the Whoami RPC. Sends a request to /counter.v1.CounterService/Whoami.
    pub async fn whoami(
        &self,
        request: crate::r#gen::buffa::counter::v1::WhoamiRequest,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::WhoamiResponseView<
                    'static,
                >,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        self.whoami_with_options(request, ::connectrpc::client::CallOptions::default())
            .await
    }
    /// Call the Whoami RPC with explicit per-call options. Options override [`ClientConfig`](::connectrpc::client::ClientConfig) defaults.
    pub async fn whoami_with_options(
        &self,
        request: crate::r#gen::buffa::counter::v1::WhoamiRequest,
        options: ::connectrpc::client::CallOptions,
    ) -> Result<
        ::connectrpc::client::UnaryResponse<
            ::buffa::view::OwnedView<
                crate::r#gen::buffa::counter::v1::__buffa::view::WhoamiResponseView<
                    'static,
                >,
            >,
        >,
        ::connectrpc::ConnectError,
    > {
        ::connectrpc::client::call_unary(
                &self.transport,
                &self.config,
                COUNTER_SERVICE_WHOAMI_SPEC
                    .with_origin(::connectrpc::SpecOrigin::Client),
                request,
                options,
            )
            .await
    }
}
