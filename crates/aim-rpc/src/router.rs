//! A typed method router: register [`Method`] markers with async handlers over shared state.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::rpc::{Method, Notification};
use serde_json::Value;

use crate::peer::{BoxFuture, Handler, NotificationCtx, RequestCtx};

type MethodFn<S> = Box<dyn Fn(Arc<S>, RequestCtx, Value) -> BoxFuture<Result<Value, ProtoError>> + Send + Sync>;
type NotifyFn<S> = Box<dyn Fn(Arc<S>, NotificationCtx, Value) -> BoxFuture<()> + Send + Sync>;

/// Routes requests and notifications by name to typed handlers over shared state `S`.
pub struct Router<S> {
    state: Arc<S>,
    methods: HashMap<&'static str, MethodFn<S>>,
    notifications: HashMap<&'static str, NotifyFn<S>>,
}

impl<S: Send + Sync + 'static> Router<S> {
    /// A router with no routes over `state`.
    #[must_use]
    pub fn new(state: S) -> Self {
        Self { state: Arc::new(state), methods: HashMap::new(), notifications: HashMap::new() }
    }

    /// Registers a handler for method `M`. Parameters that do not match `M::Params` are rejected
    /// with `invalid_params` before the handler runs.
    #[must_use]
    pub fn method<M, F, Fut>(mut self, handler: F) -> Self
    where
        M: Method,
        F: Fn(Arc<S>, RequestCtx, M::Params) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<M::Result, ProtoError>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.methods.insert(
            M::NAME,
            Box::new(move |state, ctx, params| {
                let handler = Arc::clone(&handler);
                Box::pin(async move {
                    let params: M::Params = serde_json::from_value(params)
                        .map_err(|_| ProtoError::new(ErrorCode::InvalidParams, format!("{}: parameters do not match schema", M::NAME)))?;
                    let result = handler(state, ctx, params).await?;
                    serde_json::to_value(result)
                        .map_err(|err| ProtoError::new(ErrorCode::Internal, format!("encoding {} result: {err}", M::NAME)))
                })
            }),
        );
        self
    }

    /// Registers a handler for notification `N`. Malformed parameters are logged and dropped.
    #[must_use]
    pub fn notification<N, F, Fut>(mut self, handler: F) -> Self
    where
        N: Notification,
        F: Fn(Arc<S>, NotificationCtx, N::Params) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.notifications.insert(
            N::NAME,
            Box::new(move |state, ctx, params| {
                let handler = Arc::clone(&handler);
                Box::pin(async move {
                    if let Ok(params) = serde_json::from_value::<N::Params>(params) {
                        handler(state, ctx, params).await;
                    } else {
                        tracing::warn!(method = N::NAME, "dropping a malformed notification");
                    }
                })
            }),
        );
        self
    }

    /// The shared state.
    #[must_use]
    pub fn state(&self) -> &Arc<S> {
        &self.state
    }
}

impl<S: Send + Sync + 'static> Handler for Router<S> {
    fn request(&self, ctx: RequestCtx, method: String, params: Value) -> BoxFuture<Result<Value, ProtoError>> {
        match self.methods.get(method.as_str()) {
            Some(route) => route(Arc::clone(&self.state), ctx, params),
            None => Box::pin(async { Err(ProtoError::new(ErrorCode::MethodNotFound, "method not found")) }),
        }
    }

    fn notification(&self, ctx: NotificationCtx, method: String, params: Value) -> BoxFuture<()> {
        match self.notifications.get(method.as_str()) {
            Some(route) => route(Arc::clone(&self.state), ctx, params),
            None => Box::pin(async { tracing::debug!("ignoring an unknown notification") }),
        }
    }
}
