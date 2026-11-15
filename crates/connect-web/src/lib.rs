//! A Connect RPC client transport for the browser.
//!
//! connectrpc's stock transport is hyper, which needs sockets; a wasm bundle has `fetch`.
//! The generated clients do not care — they are written against the [`ClientTransport`]
//! trait, which is compiled unconditionally — so a web shell needs exactly this one impl
//! and then uses the same generated client every other shell does.
//!
//! [`ClientTransport`]: connectrpc::client::ClientTransport
//!
//! # The `Send` problem, and why there is no `unsafe` here
//!
//! `ClientTransport::send` must return a `Send` future. Browser futures are not: `JsFuture`
//! holds a `JsValue`, which is tied to the one JS thread and marked `!Send` to say so. The
//! two are irreconcilable head-on.
//!
//! They are reconcilable sideways. The non-`Send` work runs on the local executor via
//! `spawn_local`, and only its *result* crosses back through a oneshot channel —
//! `oneshot::Receiver<T>` is `Send` whenever `T` is, and the result is `Bytes` and a status
//! code. The returned future holds nothing but the receiver, so it is honestly `Send`, and
//! nothing has been lied to: the `JsValue` never leaves the thread that made it.
//!
//! On any target other than wasm32 this crate is empty rather than absent, so it can stay
//! an ordinary workspace member that the host-side lint and test passes still visit.

#![cfg(target_arch = "wasm32")]

use bytes::Bytes;
use connectrpc::ConnectError;
use connectrpc::client::{ClientBody, ClientTransport};
use futures_channel::oneshot;
use http::{Request, Response};
use http_body_util::{BodyExt as _, Full};
use js_sys::Uint8Array;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};

/// The transport. Stateless: `fetch` lives on the window, and the browser owns the
/// connection pool, so there is nothing to keep.
#[derive(Clone, Copy, Debug, Default)]
pub struct FetchTransport;

impl FetchTransport {
    /// A transport that sends through the page's `fetch`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ClientTransport for FetchTransport {
    // Fully buffered. Streaming would need a `ReadableStream` adapter, and Connect unary —
    // which is all the generated clients here use — buffers anyway.
    type ResponseBody = Full<Bytes>;
    // A `ConnectError` rather than a transport-specific type, so the client surfaces its
    // code and message verbatim instead of wrapping them in another layer.
    type Error = ConnectError;

    fn send(
        &self,
        request: Request<ClientBody>,
    ) -> connectrpc::client::BoxFuture<'static, Result<Response<Full<Bytes>>, ConnectError>> {
        let (tx, rx) = oneshot::channel();

        spawn_local(async move {
            // The receiver is gone only if the caller stopped waiting, in which case there
            // is nobody to tell.
            let _ = tx.send(fetch(request).await);
        });

        Box::pin(async move {
            rx.await.unwrap_or_else(|_| {
                // The sender was dropped without sending, which the body above cannot do.
                Err(ConnectError::unavailable("the request was abandoned"))
            })
        })
    }
}

/// One round trip, entirely on the JS thread.
///
/// Not `Send`, and cannot be: it holds `JsValue`s across awaits. That is the reason
/// [`FetchTransport::send`] hands it to `spawn_local` instead of returning it.
#[allow(clippy::future_not_send)]
async fn fetch(request: Request<ClientBody>) -> Result<Response<Full<Bytes>>, ConnectError> {
    let (parts, body) = request.into_parts();

    let body = body
        // `ClientBody`'s own error is already a `ConnectError`, so there is nothing to
        // translate here.
        .collect()
        .await?
        .to_bytes();

    let headers = web_sys::Headers::new().map_err(|error| js_error("new Headers", &error))?;
    for (name, value) in &parts.headers {
        let value = value
            .to_str()
            .map_err(|_| ConnectError::internal(format!("header {name} is not valid text")))?;
        headers
            .append(name.as_str(), value)
            .map_err(|error| js_error("Headers.append", &error))?;
    }

    let init = web_sys::RequestInit::new();
    init.set_method(parts.method.as_str());
    init.set_headers(&headers);
    // Copies into JS memory. `Uint8Array::view` would not, but it borrows the wasm linear
    // memory, and any allocation before `fetch` reads it can move that memory underneath.
    init.set_body(&Uint8Array::from(body.as_ref()));

    let request = web_sys::Request::new_with_str_and_init(&parts.uri.to_string(), &init)
        .map_err(|error| js_error("new Request", &error))?;

    let window = web_sys::window()
        .ok_or_else(|| ConnectError::internal("no window: this transport needs a browser"))?;

    let response = JsFuture::from(window.fetch_with_request(&request))
        .await
        // A rejected fetch is a network failure — the server is down, DNS failed, CORS
        // refused. `unavailable` is the code for all of those.
        .map_err(|error| ConnectError::unavailable(describe(&error)))?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| {
            ConnectError::internal("fetch resolved to something that is not a Response")
        })?;

    let buffer = JsFuture::from(
        response
            .array_buffer()
            .map_err(|error| js_error("Response.arrayBuffer", &error))?,
    )
    .await
    .map_err(|error| ConnectError::unavailable(describe(&error)))?;

    let bytes = Bytes::from(Uint8Array::new(&buffer).to_vec());

    let mut builder = Response::builder().status(response.status());
    // Every header, not just content-type: Connect carries its error details and trailing
    // metadata in them, and the client is what decides which of those matter.
    for entry in js_sys::try_iter(&response.headers().entries().into())
        .map_err(|error| js_error("Headers.entries", &error))?
        .into_iter()
        .flatten()
        .flatten()
    {
        let pair = js_sys::Array::from(&entry);
        if let (Some(name), Some(value)) = (pair.get(0).as_string(), pair.get(1).as_string()) {
            builder = builder.header(name, value);
        }
    }

    builder
        .body(Full::new(bytes))
        .map_err(|error| ConnectError::internal(format!("malformed response: {error}")))
}

/// A JS exception, as something a `ConnectError` can carry.
fn describe(error: &JsValue) -> String {
    error
        .as_string()
        .or_else(|| {
            error
                .dyn_ref::<js_sys::Error>()
                .map(|error| String::from(error.message()))
        })
        .unwrap_or_else(|| "the browser rejected the request".to_owned())
}

fn js_error(what: &str, error: &JsValue) -> ConnectError {
    ConnectError::internal(format!("{what} failed: {}", describe(error)))
}
