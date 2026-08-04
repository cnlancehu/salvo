use std::fmt::{self, Formatter};

use crate::async_trait;
use crate::http::Request;
use crate::routing::{Filter, PathState};

#[derive(Clone, Copy, Debug)]
pub struct Or<T, U> {
    pub(super) first: T,
    pub(super) second: U,
}

#[async_trait]
impl<T, U> Filter for Or<T, U>
where
    T: Filter + Send,
    U: Filter + Send,
{
    #[inline]
    async fn filter(&self, req: &mut Request, state: &mut PathState<'_>) -> bool {
        let first = if self.first.is_sync() {
            self.first.filter_sync(req, state)
        } else {
            self.first.filter(req, state).await
        };
        if first {
            true
        } else if self.second.is_sync() {
            self.second.filter_sync(req, state)
        } else {
            self.second.filter(req, state).await
        }
    }

    #[inline]
    fn is_sync(&self) -> bool {
        self.first.is_sync() && self.second.is_sync()
    }

    #[inline]
    fn filter_sync(&self, req: &mut Request, state: &mut PathState<'_>) -> bool {
        if self.first.filter_sync(req, state) {
            true
        } else {
            self.second.filter_sync(req, state)
        }
    }
}

#[derive(Clone, Copy)]
pub struct OrElse<T, F> {
    pub(super) filter: T,
    pub(super) callback: F,
}
#[async_trait]
impl<T, F> Filter for OrElse<T, F>
where
    T: Filter,
    F: for<'a> Fn(&mut Request, &mut PathState<'a>) -> bool + Send + Sync + 'static,
{
    #[inline]
    async fn filter(&self, req: &mut Request, state: &mut PathState<'_>) -> bool {
        let matched = if self.filter.is_sync() {
            self.filter.filter_sync(req, state)
        } else {
            self.filter.filter(req, state).await
        };
        if matched {
            true
        } else {
            (self.callback)(req, state)
        }
    }

    #[inline]
    fn is_sync(&self) -> bool {
        self.filter.is_sync()
    }

    #[inline]
    fn filter_sync(&self, req: &mut Request, state: &mut PathState<'_>) -> bool {
        if self.filter.filter_sync(req, state) {
            true
        } else {
            (self.callback)(req, state)
        }
    }
}

impl<T, F> fmt::Debug for OrElse<T, F> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "opt:or_else")
    }
}

#[derive(Clone, Copy, Debug)]
pub struct And<T, U> {
    pub(super) first: T,
    pub(super) second: U,
}

#[async_trait]
impl<T, U> Filter for And<T, U>
where
    T: Filter,
    U: Filter,
{
    #[inline]
    async fn filter(&self, req: &mut Request, state: &mut PathState<'_>) -> bool {
        let first = if self.first.is_sync() {
            self.first.filter_sync(req, state)
        } else {
            self.first.filter(req, state).await
        };
        if !first {
            false
        } else if self.second.is_sync() {
            self.second.filter_sync(req, state)
        } else {
            self.second.filter(req, state).await
        }
    }

    #[inline]
    fn static_path_segment(&self) -> Option<&str> {
        self.first.static_path_segment()
    }

    #[inline]
    fn is_sync(&self) -> bool {
        self.first.is_sync() && self.second.is_sync()
    }

    #[inline]
    fn filter_sync(&self, req: &mut Request, state: &mut PathState<'_>) -> bool {
        if !self.first.filter_sync(req, state) {
            false
        } else {
            self.second.filter_sync(req, state)
        }
    }
}

#[derive(Clone, Copy)]
pub struct AndThen<T, F> {
    pub(super) filter: T,
    pub(super) callback: F,
}

#[async_trait]
impl<T, F> Filter for AndThen<T, F>
where
    T: Filter,
    F: for<'a> Fn(&mut Request, &mut PathState<'a>) -> bool + Send + Sync + 'static,
{
    #[inline]
    async fn filter(&self, req: &mut Request, state: &mut PathState<'_>) -> bool {
        let matched = if self.filter.is_sync() {
            self.filter.filter_sync(req, state)
        } else {
            self.filter.filter(req, state).await
        };
        if !matched {
            false
        } else {
            (self.callback)(req, state)
        }
    }

    #[inline]
    fn static_path_segment(&self) -> Option<&str> {
        self.filter.static_path_segment()
    }

    #[inline]
    fn is_sync(&self) -> bool {
        self.filter.is_sync()
    }

    #[inline]
    fn filter_sync(&self, req: &mut Request, state: &mut PathState<'_>) -> bool {
        if !self.filter.filter_sync(req, state) {
            false
        } else {
            (self.callback)(req, state)
        }
    }
}

impl<T, F> fmt::Debug for AndThen<T, F> {
    #[inline]
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "opt:and_then")
    }
}
