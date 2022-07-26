use super::{FromRequest, RequestParts};
use async_trait::async_trait;
use std::convert::Infallible;

/// Extractor that extracts the raw query string, without parsing it.
///
/// # Example
///
/// ```rust,no_run
/// use axum::{
///     extract::RawQuery,
///     routing::get,
///     Router,
/// };
/// use futures::StreamExt;
///
/// async fn handler(RawQuery(query): RawQuery) {
///     // ...
/// }
///
/// let app = Router::new().route("/users", get(handler));
/// # async {
/// # axum::Server::bind(&"".parse().unwrap()).serve(app.into_make_service()).await.unwrap();
/// # };
/// ```
#[derive(Debug)]
pub struct RawQuery(pub Option<String>);

#[async_trait]
impl<B, S> FromRequest<B, S> for RawQuery
where
    B: Send,
    S: Send,
{
    type Rejection = Infallible;

    async fn from_request(req: &mut RequestParts<B, S>) -> Result<Self, Self::Rejection> {
        let query = req.uri().query().map(|query| query.to_owned());
        Ok(Self(query))
    }
}
