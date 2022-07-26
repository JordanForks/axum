//! Async functions that can be used to handle requests.
//!
#![doc = include_str!("../docs/handlers_intro.md")]
//!
//! Some examples of handlers:
//!
//! ```rust
//! use axum::body::Bytes;
//! use http::StatusCode;
//!
//! // Handler that immediately returns an empty `200 OK` response.
//! async fn unit_handler() {}
//!
//! // Handler that immediately returns an empty `200 OK` response with a plain
//! // text body.
//! async fn string_handler() -> String {
//!     "Hello, World!".to_string()
//! }
//!
//! // Handler that buffers the request body and returns it.
//! //
//! // This works because `Bytes` implements `FromRequest`
//! // and therefore can be used as an extractor.
//! //
//! // `String` and `StatusCode` both implement `IntoResponse` and
//! // therefore `Result<String, StatusCode>` also implements `IntoResponse`
//! async fn echo(body: Bytes) -> Result<String, StatusCode> {
//!     if let Ok(string) = String::from_utf8(body.to_vec()) {
//!         Ok(string)
//!     } else {
//!         Err(StatusCode::BAD_REQUEST)
//!     }
//! }
//! ```
//!
#![doc = include_str!("../docs/debugging_handler_type_errors.md")]

use crate::{
    body::{boxed, Body, Bytes, HttpBody},
    extract::{connect_info::IntoMakeServiceWithConnectInfo, FromRequest, RequestParts},
    response::{IntoResponse, Response},
    routing::IntoMakeService,
    BoxError,
};
use http::Request;
use std::{fmt, future::Future, marker::PhantomData, pin::Pin};
use tower::ServiceExt;
use tower_layer::Layer;
use tower_service::Service;

pub mod future;
mod into_service;
mod into_service_state_in_extension;
mod with_state;

pub(crate) use self::into_service_state_in_extension::IntoServiceStateInExtension;
pub use self::{into_service::IntoService, with_state::WithState};

/// Trait for async functions that can be used to handle requests.
///
/// You shouldn't need to depend on this trait directly. It is automatically
/// implemented to closures of the right types.
///
/// See the [module docs](crate::handler) for more details.
///
#[doc = include_str!("../docs/debugging_handler_type_errors.md")]
pub trait Handler<T, S = (), B = Body>: Clone + Send + Sized + 'static {
    /// The type of future calling this handler returns.
    type Future: Future<Output = Response> + Send + 'static;

    /// Call the handler with the given request.
    fn call(self, state: S, req: Request<B>) -> Self::Future;

    /// Apply a [`tower::Layer`] to the handler.
    ///
    /// All requests to the handler will be processed by the layer's
    /// corresponding middleware.
    ///
    /// This can be used to add additional processing to a request for a single
    /// handler.
    ///
    /// Note this differs from [`routing::Router::layer`](crate::routing::Router::layer)
    /// which adds a middleware to a group of routes.
    ///
    /// If you're applying middleware that produces errors you have to handle the errors
    /// so they're converted into responses. You can learn more about doing that
    /// [here](crate::error_handling).
    ///
    /// # Example
    ///
    /// Adding the [`tower::limit::ConcurrencyLimit`] middleware to a handler
    /// can be done like so:
    ///
    /// ```rust
    /// use axum::{
    ///     routing::get,
    ///     handler::Handler,
    ///     Router,
    /// };
    /// use tower::limit::{ConcurrencyLimitLayer, ConcurrencyLimit};
    ///
    /// async fn handler() { /* ... */ }
    ///
    /// let layered_handler = handler.layer(ConcurrencyLimitLayer::new(64));
    /// let app = Router::new().route("/", get(layered_handler));
    /// # async {
    /// # axum::Server::bind(&"".parse().unwrap()).serve(app.into_make_service()).await.unwrap();
    /// # };
    /// ```
    fn layer<L>(self, layer: L) -> Layered<L, Self, T, S, B>
    where
        L: Layer<WithState<Self, T, S, B>>,
    {
        Layered {
            layer,
            handler: self,
            _marker: PhantomData,
        }
    }

    /// Convert the handler into a [`Service`] by providing the state
    fn with_state(self, state: S) -> WithState<Self, T, S, B> {
        WithState {
            service: IntoService::new(self, state),
        }
    }
}

impl<F, Fut, Res, S, B> Handler<(), S, B> for F
where
    F: FnOnce() -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Res> + Send,
    Res: IntoResponse,
    B: Send + 'static,
{
    type Future = Pin<Box<dyn Future<Output = Response> + Send>>;

    fn call(self, _state: S, _req: Request<B>) -> Self::Future {
        Box::pin(async move { self().await.into_response() })
    }
}

macro_rules! impl_handler {
    ( $($ty:ident),* $(,)? ) => {
        #[allow(non_snake_case)]
        impl<F, Fut, B, S, Res, $($ty,)*> Handler<($($ty,)*), S, B> for F
        where
            F: FnOnce($($ty,)*) -> Fut + Clone + Send + 'static,
            Fut: Future<Output = Res> + Send,
            B: Send + 'static,
            S: Send + 'static,
            Res: IntoResponse,
            $( $ty: FromRequest<B, S> + Send,)*
        {
            type Future = Pin<Box<dyn Future<Output = Response> + Send>>;

            fn call(self, state: S, req: Request<B>) -> Self::Future {
                Box::pin(async move {
                    let mut req = RequestParts::new(state, req);

                    $(
                        let $ty = match $ty::from_request(&mut req).await {
                            Ok(value) => value,
                            Err(rejection) => return rejection.into_response(),
                        };
                    )*

                    let res = self($($ty,)*).await;

                    res.into_response()
                })
            }
        }
    };
}

all_the_tuples!(impl_handler);

/// A [`Service`] created from a [`Handler`] by applying a Tower middleware.
///
/// Created with [`Handler::layer`]. See that method for more details.
pub struct Layered<L, H, T, S, B> {
    layer: L,
    handler: H,
    _marker: PhantomData<fn() -> (T, S, B)>,
}

impl<L, H, T, S, B> fmt::Debug for Layered<L, H, T, S, B>
where
    L: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Layered")
            .field("layer", &self.layer)
            .finish()
    }
}

impl<L, H, T, S, B> Clone for Layered<L, H, T, S, B>
where
    L: Clone,
    H: Clone,
{
    fn clone(&self) -> Self {
        Self {
            layer: self.layer.clone(),
            handler: self.handler.clone(),
            _marker: PhantomData,
        }
    }
}

impl<H, S, T, B, ResBody, L> Handler<T, S, B> for Layered<L, H, T, S, B>
where
    L: Layer<WithState<H, T, S, B>> + Clone + Send + 'static,
    H: Handler<T, S, B>,
    L::Service: Service<Request<B>, Response = Response<ResBody>> + Clone + Send + 'static,
    <L::Service as Service<Request<B>>>::Error: IntoResponse,
    <L::Service as Service<Request<B>>>::Future: Send,
    T: 'static,
    S: 'static,
    B: Send + 'static,
    ResBody: HttpBody<Data = Bytes> + Send + 'static,
    ResBody::Error: Into<BoxError>,
{
    type Future = future::LayeredFuture<B, L::Service>;

    fn call(self, state: S, req: Request<B>) -> Self::Future {
        use futures_util::future::{FutureExt, Map};

        let svc = self.handler.with_state(state);
        let svc = self.layer.layer(svc);

        let future: Map<
            _,
            fn(
                Result<
                    <L::Service as Service<Request<B>>>::Response,
                    <L::Service as Service<Request<B>>>::Error,
                >,
            ) -> _,
        > = svc.oneshot(req).map(|result| match result {
            Ok(res) => res.map(boxed),
            Err(res) => res.into_response(),
        });

        future::LayeredFuture::new(future)
    }
}

/// Extension trait for [`Handler`]s who doesn't have state.
///
/// This provides convenience methods to convert the [`Handler`] into a [`Service`] or [`MakeService`].
///
/// [`MakeService`]: tower::make::MakeService
pub trait HandlerWithoutStateExt<T, B>: Handler<T, (), B> {
    /// Convert the handler into a [`Service`] and no state.
    fn into_service(self) -> WithState<Self, T, (), B>;

    /// Convert the handler into a [`MakeService`] and no state.
    ///
    /// See [`WithState::into_make_service`] for more details.
    ///
    /// [`MakeService`]: tower::make::MakeService
    fn into_make_service(self) -> IntoMakeService<IntoService<Self, T, (), B>>;

    /// Convert the handler into a [`MakeService`] which stores information
    /// about the incoming connection and has no state.
    ///
    /// See [`WithState::into_make_service_with_connect_info`] for more details.
    ///
    /// [`MakeService`]: tower::make::MakeService
    fn into_make_service_with_connect_info<C>(
        self,
    ) -> IntoMakeServiceWithConnectInfo<IntoService<Self, T, (), B>, C>;
}

impl<H, T, B> HandlerWithoutStateExt<T, B> for H
where
    H: Handler<T, (), B>,
{
    fn into_service(self) -> WithState<Self, T, (), B> {
        self.with_state(())
    }

    fn into_make_service(self) -> IntoMakeService<IntoService<Self, T, (), B>> {
        self.with_state(()).into_make_service()
    }

    fn into_make_service_with_connect_info<C>(
        self,
    ) -> IntoMakeServiceWithConnectInfo<IntoService<Self, T, (), B>, C> {
        self.with_state(()).into_make_service_with_connect_info()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use http::StatusCode;

    #[tokio::test]
    async fn handler_into_service() {
        async fn handle(body: String) -> impl IntoResponse {
            format!("you said: {}", body)
        }

        let client = TestClient::new(handle.into_service());

        let res = client.post("/").body("hi there!").send().await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.text().await, "you said: hi there!");
    }
}
