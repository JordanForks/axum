//! Routing between [`Service`]s and handlers.

use self::{future::RouteFuture, not_found::NotFound};
use crate::{
    body::{boxed, Body, Bytes, HttpBody},
    extract::{connect_info::IntoMakeServiceWithConnectInfo, Extension},
    handler::Handler,
    response::Response,
    routing::strip_prefix::StripPrefix,
    util::try_downcast,
    BoxError,
};
use http::Request;
use matchit::MatchError;
use std::{
    borrow::Cow,
    collections::HashMap,
    convert::Infallible,
    fmt,
    sync::Arc,
    task::{Context, Poll},
};
use tower::{layer::layer_fn, ServiceBuilder};
use tower_http::map_response_body::MapResponseBodyLayer;
use tower_layer::Layer;
use tower_service::Service;

pub mod future;
pub mod method_routing;

mod into_make_service;
mod method_filter;
mod not_found;
mod route;
mod strip_prefix;
pub(crate) mod url_params;

#[cfg(test)]
mod tests;

pub use self::{into_make_service::IntoMakeService, method_filter::MethodFilter, route::Route};

pub use self::method_routing::{
    any, any_service, delete, delete_service, get, get_service, head, head_service, on, on_service,
    options, options_service, patch, patch_service, post, post_service, put, put_service, trace,
    trace_service, MethodRouter,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RouteId(u32);

impl RouteId {
    fn next() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        // `AtomicU64` isn't supported on all platforms
        static ID: AtomicU32 = AtomicU32::new(0);
        let id = ID.fetch_add(1, Ordering::Relaxed);
        if id == u32::MAX {
            panic!("Over `u32::MAX` routes created. If you need this, please file an issue.");
        }
        Self(id)
    }
}

/// The router type for composing handlers and services.
pub struct Router<S = (), B = Body> {
    state: S,
    routes: HashMap<RouteId, Endpoint<S, B>>,
    node: Arc<Node>,
    fallback: Fallback<B>,
    nested_at_root: bool,
}

impl<S, B> Clone for Router<S, B>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            routes: self.routes.clone(),
            node: Arc::clone(&self.node),
            fallback: self.fallback.clone(),
            nested_at_root: self.nested_at_root,
        }
    }
}

impl<S, B> Default for Router<S, B>
where
    B: HttpBody + Send + 'static,
    S: Default + Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::with_state(S::default())
    }
}

impl<S, B> fmt::Debug for Router<S, B>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router")
            .field("state", &self.state)
            .field("routes", &self.routes)
            .field("node", &self.node)
            .field("fallback", &self.fallback)
            .field("nested_at_root", &self.nested_at_root)
            .finish()
    }
}

pub(crate) const NEST_TAIL_PARAM: &str = "__private__axum_nest_tail_param";
const NEST_TAIL_PARAM_CAPTURE: &str = "/*__private__axum_nest_tail_param";

impl<B> Router<(), B>
where
    B: HttpBody + Send + 'static,
{
    /// Create a new `Router`.
    ///
    /// Unless you add additional routes this will respond with `404 Not Found` to
    /// all requests.
    pub fn new() -> Self {
        Self::with_state(())
    }
}

impl<S, B> Router<S, B>
where
    B: HttpBody + Send + 'static,
    S: Clone + Send + Sync + 'static,
{
    /// Create a new `Router` with the given state.
    ///
    /// See [`State`](crate::extract::State) for more details about accessing state.
    ///
    /// Unless you add additional routes this will respond with `404 Not Found` to
    /// all requests.
    pub fn with_state(state: S) -> Self {
        Self {
            state,
            routes: Default::default(),
            node: Default::default(),
            fallback: Fallback::Default(Route::new(NotFound)),
            nested_at_root: false,
        }
    }

    #[doc = include_str!("../docs/routing/route.md")]
    pub fn route(mut self, path: &str, method_router: MethodRouter<S, B>) -> Self {
        if path.is_empty() {
            panic!("Paths must start with a `/`. Use \"/\" for root routes");
        } else if !path.starts_with('/') {
            panic!("Paths must start with a `/`");
        }

        let id = RouteId::next();

        let endpoint = if let Some((route_id, Endpoint::MethodRouter(prev_method_router))) = self
            .node
            .path_to_route_id
            .get(path)
            .and_then(|route_id| self.routes.get(route_id).map(|svc| (*route_id, svc)))
        {
            // if we're adding a new `MethodRouter` to a route that already has one just
            // merge them. This makes `.route("/", get(_)).route("/", post(_))` work
            let service = Endpoint::MethodRouter(prev_method_router.clone().merge(method_router));
            self.routes.insert(route_id, service);
            return self;
        } else {
            Endpoint::MethodRouter(method_router)
        };

        let mut node =
            Arc::try_unwrap(Arc::clone(&self.node)).unwrap_or_else(|node| (*node).clone());
        if let Err(err) = node.insert(path, id) {
            self.panic_on_matchit_error(err);
        }
        self.node = Arc::new(node);

        self.routes.insert(id, endpoint);

        self
    }

    #[doc = include_str!("../docs/routing/route_service.md")]
    pub fn route_service<T>(mut self, path: &str, service: T) -> Self
    where
        T: Service<Request<B>, Response = Response, Error = Infallible> + Clone + Send + 'static,
        T::Future: Send + 'static,
    {
        if path.is_empty() {
            panic!("Paths must start with a `/`. Use \"/\" for root routes");
        } else if !path.starts_with('/') {
            panic!("Paths must start with a `/`");
        }

        let service = match try_downcast::<Router<S, B>, _>(service) {
            Ok(_) => {
                panic!("Invalid route: `Router::route` cannot be used with `Router`s. Use `Router::nest` instead")
            }
            Err(svc) => svc,
        };

        let id = RouteId::next();

        let endpoint = Endpoint::Route(Route::new(service));

        let mut node =
            Arc::try_unwrap(Arc::clone(&self.node)).unwrap_or_else(|node| (*node).clone());
        if let Err(err) = node.insert(path, id) {
            self.panic_on_matchit_error(err);
        }
        self.node = Arc::new(node);

        self.routes.insert(id, endpoint);

        self
    }

    #[doc = include_str!("../docs/routing/nest.md")]
    pub fn nest<T>(mut self, mut path: &str, svc: T) -> Self
    where
        T: Service<Request<B>, Response = Response, Error = Infallible> + Clone + Send + 'static,
        T::Future: Send + 'static,
    {
        if path.is_empty() {
            // nesting at `""` and `"/"` should mean the same thing
            path = "/";
        }

        if path.contains('*') {
            panic!("Invalid route: nested routes cannot contain wildcards (*)");
        }

        let prefix = path;

        if path == "/" {
            self.nested_at_root = true;
        }

        match try_downcast::<Router<S, B>, _>(svc) {
            // if the user is nesting a `Router` we can implement nesting
            // by simplying copying all the routes and adding the prefix in
            // front
            Ok(router) => {
                let Router {
                    // nesting has changed in https://github.com/tokio-rs/axum/pull/1086
                    // so once that is merged we can make sure states work currectly with nesting
                    state: _,
                    mut routes,
                    node,
                    fallback,
                    // nesting a router that has something nested at root
                    // doesn't mean something is nested at root in _this_ router
                    // thus we don't need to propagate that
                    nested_at_root: _,
                } = router;

                if let Fallback::Custom(_) = fallback {
                    panic!("Cannot nest `Router`s that has a fallback");
                }

                for (id, nested_path) in &node.route_id_to_path {
                    let route = routes.remove(id).unwrap();
                    let full_path: Cow<str> = if &**nested_path == "/" {
                        path.into()
                    } else if path == "/" {
                        (&**nested_path).into()
                    } else if let Some(path) = path.strip_suffix('/') {
                        format!("{}{}", path, nested_path).into()
                    } else {
                        format!("{}{}", path, nested_path).into()
                    };
                    self = match route {
                        Endpoint::MethodRouter(method_router) => self.route(
                            &full_path,
                            method_router.layer(layer_fn(|s| StripPrefix::new(s, prefix))),
                        ),
                        Endpoint::Route(route) => {
                            self.route_service(&full_path, StripPrefix::new(route, prefix))
                        }
                    };
                }

                debug_assert!(routes.is_empty());
            }
            // otherwise we add a wildcard route to the service
            Err(svc) => {
                let path = if path.ends_with('/') {
                    format!("{}*{}", path, NEST_TAIL_PARAM)
                } else {
                    format!("{}/*{}", path, NEST_TAIL_PARAM)
                };

                self = self.route_service(&path, strip_prefix::StripPrefix::new(svc, prefix));
            }
        }

        self
    }

    #[doc = include_str!("../docs/routing/merge.md")]
    pub fn merge<S2, R>(mut self, other: R) -> Self
    where
        R: Into<Router<S2, B>>,
        S2: Clone + Send + Sync + 'static,
    {
        let Router {
            state,
            routes,
            node,
            fallback,
            nested_at_root,
        } = other.into();

        for (id, route) in routes {
            let path = node
                .route_id_to_path
                .get(&id)
                .expect("no path for route id. This is a bug in axum. Please file an issue");
            self = match route {
                Endpoint::MethodRouter(method_router) => self.route(
                    path,
                    method_router
                        // this will set the state for each route
                        // such we don't override the inner state later in `MethodRouterWithState`
                        .layer(Extension(state.clone()))
                        .downcast_state(),
                ),
                Endpoint::Route(route) => self.route_service(path, route),
            };
        }

        self.fallback = match (self.fallback, fallback) {
            (Fallback::Default(_), pick @ Fallback::Default(_)) => pick,
            (Fallback::Default(_), pick @ Fallback::Custom(_)) => pick,
            (pick @ Fallback::Custom(_), Fallback::Default(_)) => pick,
            (Fallback::Custom(_), Fallback::Custom(_)) => {
                panic!("Cannot merge two `Router`s that both have a fallback")
            }
        };

        self.nested_at_root = self.nested_at_root || nested_at_root;

        self
    }

    #[doc = include_str!("../docs/routing/layer.md")]
    pub fn layer<L, NewReqBody, NewResBody>(self, layer: L) -> Router<S, NewReqBody>
    where
        L: Layer<Route<B>>,
        L::Service:
            Service<Request<NewReqBody>, Response = Response<NewResBody>> + Clone + Send + 'static,
        <L::Service as Service<Request<NewReqBody>>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request<NewReqBody>>>::Future: Send + 'static,
        NewResBody: HttpBody<Data = Bytes> + Send + 'static,
        NewResBody::Error: Into<BoxError>,
    {
        let layer = ServiceBuilder::new()
            .map_err(Into::into)
            .layer(MapResponseBodyLayer::new(boxed))
            .layer(layer)
            .into_inner();

        let routes = self
            .routes
            .into_iter()
            .map(|(id, route)| {
                let route = match route {
                    Endpoint::MethodRouter(method_router) => {
                        Endpoint::MethodRouter(method_router.layer(&layer))
                    }
                    Endpoint::Route(route) => Endpoint::Route(Route::new(layer.layer(route))),
                };
                (id, route)
            })
            .collect();

        let fallback = self.fallback.map(|svc| Route::new(layer.layer(svc)));

        Router {
            state: self.state,
            routes,
            node: self.node,
            fallback,
            nested_at_root: self.nested_at_root,
        }
    }

    #[doc = include_str!("../docs/routing/route_layer.md")]
    pub fn route_layer<L, NewResBody>(self, layer: L) -> Self
    where
        L: Layer<Route<B>>,
        L::Service: Service<Request<B>, Response = Response<NewResBody>> + Clone + Send + 'static,
        <L::Service as Service<Request<B>>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request<B>>>::Future: Send + 'static,
        NewResBody: HttpBody<Data = Bytes> + Send + 'static,
        NewResBody::Error: Into<BoxError>,
    {
        let layer = ServiceBuilder::new()
            .map_err(Into::into)
            .layer(MapResponseBodyLayer::new(boxed))
            .layer(layer)
            .into_inner();

        let routes = self
            .routes
            .into_iter()
            .map(|(id, route)| {
                let route = match route {
                    Endpoint::MethodRouter(method_router) => {
                        Endpoint::MethodRouter(method_router.layer(&layer))
                    }
                    Endpoint::Route(route) => Endpoint::Route(Route::new(layer.layer(route))),
                };
                (id, route)
            })
            .collect();

        Router {
            state: self.state,
            routes,
            node: self.node,
            fallback: self.fallback,
            nested_at_root: self.nested_at_root,
        }
    }

    #[doc = include_str!("../docs/routing/fallback.md")]
    pub fn fallback<H, T>(self, handler: H) -> Self
    where
        H: Handler<T, S, B>,
        T: 'static,
    {
        let state = self.state.clone();
        self.fallback_service(handler.with_state(state))
    }

    /// Add a fallback [`Service`] to the router.
    ///
    /// See [`Router::fallback`] for more details.
    pub fn fallback_service<T>(mut self, svc: T) -> Self
    where
        T: Service<Request<B>, Response = Response, Error = Infallible> + Clone + Send + 'static,
        T::Future: Send + 'static,
    {
        self.fallback = Fallback::Custom(Route::new(svc));
        self
    }

    /// Convert this router into a [`MakeService`], that is a [`Service`] whose
    /// response is another service.
    ///
    /// This is useful when running your application with hyper's
    /// [`Server`](hyper::server::Server):
    ///
    /// ```
    /// use axum::{
    ///     routing::get,
    ///     Router,
    /// };
    ///
    /// let app = Router::new().route("/", get(|| async { "Hi!" }));
    ///
    /// # async {
    /// axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
    ///     .serve(app.into_make_service())
    ///     .await
    ///     .expect("server failed");
    /// # };
    /// ```
    ///
    /// [`MakeService`]: tower::make::MakeService
    pub fn into_make_service(self) -> IntoMakeService<Self> {
        IntoMakeService::new(self)
    }

    #[doc = include_str!("../docs/routing/into_make_service_with_connect_info.md")]
    pub fn into_make_service_with_connect_info<C>(self) -> IntoMakeServiceWithConnectInfo<Self, C> {
        IntoMakeServiceWithConnectInfo::new(self)
    }

    #[inline]
    fn call_route(
        &self,
        match_: matchit::Match<&RouteId>,
        mut req: Request<B>,
    ) -> RouteFuture<B, Infallible> {
        let id = *match_.value;

        #[cfg(feature = "matched-path")]
        if let Some(matched_path) = self.node.route_id_to_path.get(&id) {
            use crate::extract::MatchedPath;

            let matched_path = if let Some(previous) = req.extensions_mut().get::<MatchedPath>() {
                // a previous `MatchedPath` might exist if we're inside a nested Router
                let previous = if let Some(previous) =
                    previous.as_str().strip_suffix(NEST_TAIL_PARAM_CAPTURE)
                {
                    previous
                } else {
                    previous.as_str()
                };

                let matched_path = format!("{}{}", previous, matched_path);
                matched_path.into()
            } else {
                Arc::clone(matched_path)
            };
            req.extensions_mut().insert(MatchedPath(matched_path));
        } else {
            #[cfg(debug_assertions)]
            panic!("should always have a matched path for a route id");
        }

        url_params::insert_url_params(req.extensions_mut(), match_.params);

        let mut route = self
            .routes
            .get(&id)
            .expect("no route for id. This is a bug in axum. Please file an issue")
            .clone();

        match &mut route {
            Endpoint::MethodRouter(inner) => inner.clone().with_state(self.state.clone()).call(req),
            Endpoint::Route(inner) => inner.call(req),
        }
    }

    fn panic_on_matchit_error(&self, err: matchit::InsertError) {
        if self.nested_at_root {
            panic!(
                "Invalid route: {}. Note that `nest(\"/\", _)` conflicts with all routes. Use `Router::fallback` instead",
                err,
            );
        } else {
            panic!("Invalid route: {}", err);
        }
    }

    /// Get a reference to the state.
    pub fn state(&self) -> &S {
        &self.state
    }
}

impl<S, B> Service<Request<B>> for Router<S, B>
where
    B: HttpBody + Send + 'static,
    S: Clone + Send + Sync + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = RouteFuture<B, Infallible>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        #[cfg(feature = "original-uri")]
        {
            use crate::extract::OriginalUri;

            if req.extensions().get::<OriginalUri>().is_none() {
                let original_uri = OriginalUri(req.uri().clone());
                req.extensions_mut().insert(original_uri);
            }
        }

        let path = req.uri().path().to_owned();

        match self.node.at(&path) {
            Ok(match_) => self.call_route(match_, req),
            Err(
                MatchError::NotFound
                | MatchError::ExtraTrailingSlash
                | MatchError::MissingTrailingSlash,
            ) => match &self.fallback {
                Fallback::Default(inner) => inner.clone().call(req),
                Fallback::Custom(inner) => inner.clone().call(req),
            },
        }
    }
}

/// Wrapper around `matchit::Router` that supports merging two `Router`s.
#[derive(Clone, Default)]
struct Node {
    inner: matchit::Router<RouteId>,
    route_id_to_path: HashMap<RouteId, Arc<str>>,
    path_to_route_id: HashMap<Arc<str>, RouteId>,
}

impl Node {
    fn insert(
        &mut self,
        path: impl Into<String>,
        val: RouteId,
    ) -> Result<(), matchit::InsertError> {
        let path = path.into();

        self.inner.insert(&path, val)?;

        let shared_path: Arc<str> = path.into();
        self.route_id_to_path.insert(val, shared_path.clone());
        self.path_to_route_id.insert(shared_path, val);

        Ok(())
    }

    fn at<'n, 'p>(
        &'n self,
        path: &'p str,
    ) -> Result<matchit::Match<'n, 'p, &'n RouteId>, MatchError> {
        self.inner.at(path)
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("paths", &self.route_id_to_path)
            .finish()
    }
}

enum Fallback<B, E = Infallible> {
    Default(Route<B, E>),
    Custom(Route<B, E>),
}

impl<B, E> Clone for Fallback<B, E> {
    fn clone(&self) -> Self {
        match self {
            Fallback::Default(inner) => Fallback::Default(inner.clone()),
            Fallback::Custom(inner) => Fallback::Custom(inner.clone()),
        }
    }
}

impl<B, E> fmt::Debug for Fallback<B, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default(inner) => f.debug_tuple("Default").field(inner).finish(),
            Self::Custom(inner) => f.debug_tuple("Custom").field(inner).finish(),
        }
    }
}

impl<B, E> Fallback<B, E> {
    fn map<F, B2, E2>(self, f: F) -> Fallback<B2, E2>
    where
        F: FnOnce(Route<B, E>) -> Route<B2, E2>,
    {
        match self {
            Fallback::Default(inner) => Fallback::Default(f(inner)),
            Fallback::Custom(inner) => Fallback::Custom(f(inner)),
        }
    }
}

enum Endpoint<S, B> {
    MethodRouter(MethodRouter<S, B, Infallible>),
    Route(Route<B>),
}

impl<S, B> Clone for Endpoint<S, B>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        match self {
            Endpoint::MethodRouter(inner) => Endpoint::MethodRouter(inner.clone()),
            Endpoint::Route(inner) => Endpoint::Route(inner.clone()),
        }
    }
}

impl<S, B> fmt::Debug for Endpoint<S, B>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MethodRouter(inner) => inner.fmt(f),
            Self::Route(inner) => inner.fmt(f),
        }
    }
}

#[test]
#[allow(warnings)]
fn traits() {
    use crate::test_helpers::*;
    assert_send::<Router<(), ()>>();
}
