//! Shared HTTP body types and small response constructors.

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::Response;

/// Boxed error type carried by [`ElrondBody`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The uniform response body type used throughout Elrond. Static buffers and
/// streamed upstream responses are both erased into this.
pub type ElrondBody = BoxBody<Bytes, BoxError>;

/// Wrap an in-memory buffer as an [`ElrondBody`].
pub fn full(data: impl Into<Bytes>) -> ElrondBody {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed()
}

/// Build a `text/plain` response with the given status and body.
pub fn text(status: u16, body: impl Into<Bytes>) -> Response<ElrondBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(full(body))
        .expect("a static text response is always well-formed")
}
