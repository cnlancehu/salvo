//! Filter module
//!
//! This module provides filters for routing requests based on various criteria
//! such as uri scheme, hostname, port, path, and HTTP method.

mod opts;
mod others;
mod path;

use std::fmt::{self, Debug, Formatter};

pub use others::*;
pub use path::*;

use self::opts::*;
use crate::async_trait;
use crate::http::uri::Scheme;
use crate::http::{Method, Request};
use crate::routing::PathState;

/// Structured description of a [`Filter`], used by `Router`'s `Debug`
/// implementation and by `salvo-oapi` to introspect a routing tree.
///
/// Each built-in filter overrides [`Filter::info`] to return the matching
/// variant. Custom filters fall back to [`FilterInfo::Other`] with the
/// implementing type's name. Returning structured data instead of relying on
/// the `Debug` string format keeps consumers from breaking when a filter's
/// `Debug` representation changes.
#[derive(Clone, Debug)]
pub enum FilterInfo {
    /// Path pattern (raw, unparsed) of a [`PathFilter`].
    Path(String),
    /// HTTP method matched by a [`MethodFilter`].
    Method(Method),
    /// URI scheme matched by a [`SchemeFilter`].
    Scheme(Scheme),
    /// Host name matched by a [`HostFilter`].
    Host(String),
    /// Port matched by a [`PortFilter`].
    Port(u16),
    /// Catch-all for filter types that do not expose structured info — typically
    /// composite filters (`And`, `Or`, `AndThen`, `OrElse`), `FnFilter`, or
    /// user-defined filters. Carries the implementor's type name.
    Other(&'static str),
}

/// Trait for filter request.
///
/// View [module level documentation](../index.html) for more details.

#[async_trait]
pub trait Filter: Debug + Send + Sync + 'static {
    #[doc(hidden)]
    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<Self>()
    }
    #[doc(hidden)]
    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Returns a structured description of what this filter matches.
    ///
    /// The default returns [`FilterInfo::Other`] with the implementing type's
    /// name. Built-in filters override this to expose typed data so that
    /// downstream code (router debug printing, OpenAPI introspection) does
    /// not have to scrape the [`Debug`] output.
    fn info(&self) -> FilterInfo {
        FilterInfo::Other(self.type_name())
    }

    /// Returns an exact static path segment that must match before this filter
    /// can succeed, when one is available.
    ///
    /// This is an internal optimization hint. Implementations must return
    /// `Some` only when a different current path segment guarantees that
    /// [`Filter::filter`] returns `false` without observable side effects.
    #[doc(hidden)]
    #[inline]
    fn static_path_segment(&self) -> Option<&str> {
        None
    }

    /// Returns whether this filter can be evaluated synchronously.
    ///
    /// The default is `false`, preserving the behavior of existing custom
    /// filters. Built-in filters override this so the router can avoid
    /// allocating an async-trait future for purely synchronous matching.
    /// Implementations returning `true` must keep that classification stable
    /// for their lifetime and provide a non-blocking [`Filter::filter_sync`]
    /// with identical results and side effects.
    #[doc(hidden)]
    #[inline]
    fn is_sync(&self) -> bool {
        false
    }

    /// Evaluates a synchronous filter without creating a future.
    ///
    /// Callers must check [`Filter::is_sync`] first. The default deliberately
    /// panics so an asynchronous custom filter cannot accidentally be treated
    /// as synchronous.
    #[doc(hidden)]
    #[inline]
    fn filter_sync(&self, _req: &mut Request, _path: &mut PathState<'_>) -> bool {
        tracing::error!("filter_sync called for an asynchronous filter");
        false
    }

    /// Create a new filter use `And` filter.
    #[inline]
    fn and<F>(self, other: F) -> And<Self, F>
    where
        Self: Sized,
        F: Filter + Send + Sync,
    {
        And {
            first: self,
            second: other,
        }
    }

    /// Create a new filter use `Or` filter.
    #[inline]
    fn or<F>(self, other: F) -> Or<Self, F>
    where
        Self: Sized,
        F: Filter + Send + Sync,
    {
        Or {
            first: self,
            second: other,
        }
    }

    /// Create a new filter use `AndThen` filter.
    #[inline]
    fn and_then<F>(self, fun: F) -> AndThen<Self, F>
    where
        Self: Sized,
        F: for<'a> Fn(&mut Request, &mut PathState<'a>) -> bool + Send + Sync + 'static,
    {
        AndThen {
            filter: self,
            callback: fun,
        }
    }

    /// Create a new filter use `OrElse` filter.
    #[inline]
    fn or_else<F>(self, fun: F) -> OrElse<Self, F>
    where
        Self: Sized,
        F: for<'a> Fn(&mut Request, &mut PathState<'a>) -> bool + Send + Sync + 'static,
    {
        OrElse {
            filter: self,
            callback: fun,
        }
    }

    /// Filter `Request` and returns false or true.
    async fn filter(&self, req: &mut Request, path: &mut PathState<'_>) -> bool;
}

/// `FnFilter` accepts a function as its parameter, using this function to filter requests.
#[derive(Copy, Clone)]
#[allow(missing_debug_implementations)]
pub struct FnFilter<F>(pub F);

#[async_trait]
impl<F> Filter for FnFilter<F>
where
    F: for<'a> Fn(&mut Request, &mut PathState<'a>) -> bool + Send + Sync + 'static,
{
    #[inline]
    async fn filter(&self, req: &mut Request, path: &mut PathState<'_>) -> bool {
        self.filter_sync(req, path)
    }

    #[inline]
    fn is_sync(&self) -> bool {
        true
    }

    #[inline]
    fn filter_sync(&self, req: &mut Request, path: &mut PathState<'_>) -> bool {
        self.0(req, path)
    }
}

impl<F> fmt::Debug for FnFilter<F> {
    #[inline]
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "fn:fn")
    }
}

/// Filter request by uri scheme.
#[inline]
#[must_use]
pub fn scheme(scheme: Scheme) -> SchemeFilter {
    SchemeFilter::new(scheme)
}

/// Filter request by uri hostname.
#[inline]
pub fn host(host: impl Into<String>) -> HostFilter {
    HostFilter::new(host)
}

/// Filter request by uri port.
#[inline]
#[must_use]
pub fn port(port: u16) -> PortFilter {
    PortFilter::new(port)
}

/// Filter request use `PathFilter`.
///
/// Invalid path patterns are logged and converted into a filter that never matches.
/// Use [`try_path`] to handle malformed patterns explicitly.
#[inline]
pub fn path(path: impl Into<String>) -> PathFilter {
    PathFilter::new(path)
}

/// Try building a `PathFilter`.
///
/// # Errors
///
/// Returns an error when the path pattern is malformed.
#[inline]
pub fn try_path(path: impl Into<String>) -> Result<PathFilter, String> {
    PathFilter::try_new(path)
}
/// Filter request, only allow get method.
#[inline]
#[must_use]
pub fn get() -> MethodFilter {
    MethodFilter(Method::GET)
}
/// Filter request, only allow head method.
#[inline]
#[must_use]
pub fn head() -> MethodFilter {
    MethodFilter(Method::HEAD)
}
/// Filter request, only allow options method.
#[inline]
#[must_use]
pub fn options() -> MethodFilter {
    MethodFilter(Method::OPTIONS)
}
/// Filter request, only allow post method.
#[inline]
#[must_use]
pub fn post() -> MethodFilter {
    MethodFilter(Method::POST)
}
/// Filter request, only allow patch method.
#[inline]
#[must_use]
pub fn patch() -> MethodFilter {
    MethodFilter(Method::PATCH)
}
/// Filter request, only allow put method.
#[inline]
#[must_use]
pub fn put() -> MethodFilter {
    MethodFilter(Method::PUT)
}

/// Filter request, only allow delete method.
#[inline]
#[must_use]
pub fn delete() -> MethodFilter {
    MethodFilter(Method::DELETE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct AsyncValue(bool);

    #[async_trait]
    impl Filter for AsyncValue {
        async fn filter(&self, _req: &mut Request, _path: &mut PathState<'_>) -> bool {
            self.0
        }
    }

    /// A synchronous filter whose async entry point must never be used by a
    /// composite filter. This makes an accidental boxed child call fail loudly.
    #[derive(Clone, Copy, Debug)]
    struct SyncOnlyValue(bool);

    #[async_trait]
    impl Filter for SyncOnlyValue {
        async fn filter(&self, _req: &mut Request, _path: &mut PathState<'_>) -> bool {
            panic!("synchronous child was evaluated through its async entry point")
        }

        fn is_sync(&self) -> bool {
            true
        }

        fn filter_sync(&self, _req: &mut Request, _path: &mut PathState<'_>) -> bool {
            self.0
        }
    }

    #[test]
    fn test_methods() {
        assert_eq!(get(), MethodFilter(Method::GET));
        assert_eq!(head(), MethodFilter(Method::HEAD));
        assert_eq!(options(), MethodFilter(Method::OPTIONS));
        assert_eq!(post(), MethodFilter(Method::POST));
        assert_eq!(patch(), MethodFilter(Method::PATCH));
        assert_eq!(put(), MethodFilter(Method::PUT));
        assert_eq!(delete(), MethodFilter(Method::DELETE));
        assert!(get().is_sync());
    }

    #[test]
    fn built_in_filters_expose_the_sync_entry_point() {
        fn always(_req: &mut Request, _path: &mut PathState<'_>) -> bool {
            true
        }

        let filters: Vec<Box<dyn Filter>> = vec![
            Box::new(path("users")),
            Box::new(get()),
            Box::new(scheme(Scheme::HTTP)),
            Box::new(host("example.com")),
            Box::new(port(80)),
            Box::new(FnFilter(always)),
        ];
        assert!(filters.iter().all(|filter| filter.is_sync()));
        assert_eq!(path("users").static_path_segment(), Some("users"));
        assert_eq!(
            path("users").and(get()).static_path_segment(),
            Some("users")
        );
        assert!(
            path("users")
                .or(path("fallback"))
                .static_path_segment()
                .is_none()
        );
        assert!(
            path("users")
                .or_else(|_, _| true)
                .static_path_segment()
                .is_none()
        );
    }

    #[tokio::test]
    async fn mixed_composites_prefer_sync_child_entry_points() {
        let mut req = Request::default();
        let mut path_state = PathState::from_borrowed_path("one");

        let nested_sync = SyncOnlyValue(true).and(SyncOnlyValue(true));
        assert!(nested_sync.is_sync());
        assert!(nested_sync.filter_sync(&mut req, &mut path_state));

        let mixed_and = nested_sync.and(AsyncValue(true));
        assert!(!mixed_and.is_sync());
        assert!(mixed_and.filter(&mut req, &mut path_state).await);

        let mixed_or = AsyncValue(false).or(SyncOnlyValue(true));
        assert!(!mixed_or.is_sync());
        assert!(mixed_or.filter(&mut req, &mut path_state).await);

        let sync_and_then = SyncOnlyValue(true).and_then(|_, _| true);
        assert!(sync_and_then.is_sync());
        assert!(sync_and_then.filter_sync(&mut req, &mut path_state));

        let mixed_or_else = AsyncValue(false).or_else(|_, _| true);
        assert!(!mixed_or_else.is_sync());
        assert!(mixed_or_else.filter(&mut req, &mut path_state).await);
    }

    #[tokio::test]
    async fn test_opts() {
        fn has_one(_req: &mut Request, path: &mut PathState<'_>) -> bool {
            path.parts().any(|part| part == "one")
        }
        fn has_two(_req: &mut Request, path: &mut PathState<'_>) -> bool {
            path.parts().any(|part| part == "two")
        }

        let one_filter = FnFilter(has_one);
        let two_filter = FnFilter(has_two);

        let mut req = Request::default();
        let mut path_state = PathState::from_borrowed_path("http://localhost/one");
        assert!(one_filter.filter(&mut req, &mut path_state).await);
        assert!(!two_filter.filter(&mut req, &mut path_state).await);
        assert!(
            one_filter
                .or_else(has_two)
                .filter(&mut req, &mut path_state)
                .await
        );
        assert!(
            one_filter
                .or(two_filter)
                .filter(&mut req, &mut path_state)
                .await
        );
        assert!(
            !one_filter
                .and_then(has_two)
                .filter(&mut req, &mut path_state)
                .await
        );
        assert!(
            !one_filter
                .and(two_filter)
                .filter(&mut req, &mut path_state)
                .await
        );

        let mut path_state = PathState::from_borrowed_path("http://localhost/one/two");
        assert!(one_filter.filter(&mut req, &mut path_state).await);
        assert!(two_filter.filter(&mut req, &mut path_state).await);
        assert!(
            one_filter
                .or_else(has_two)
                .filter(&mut req, &mut path_state)
                .await
        );
        assert!(
            one_filter
                .or(two_filter)
                .filter(&mut req, &mut path_state)
                .await
        );
        assert!(
            one_filter
                .and_then(has_two)
                .filter(&mut req, &mut path_state)
                .await
        );
        assert!(
            one_filter
                .and(two_filter)
                .filter(&mut req, &mut path_state)
                .await
        );

        let mut path_state = PathState::from_borrowed_path("http://localhost/two");
        assert!(!one_filter.filter(&mut req, &mut path_state).await);
        assert!(two_filter.filter(&mut req, &mut path_state).await);
        assert!(
            one_filter
                .or_else(has_two)
                .filter(&mut req, &mut path_state)
                .await
        );
        assert!(
            one_filter
                .or(two_filter)
                .filter(&mut req, &mut path_state)
                .await
        );
        assert!(
            !one_filter
                .and_then(has_two)
                .filter(&mut req, &mut path_state)
                .await
        );
        assert!(
            !one_filter
                .and(two_filter)
                .filter(&mut req, &mut path_state)
                .await
        );
    }
}
