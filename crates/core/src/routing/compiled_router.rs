use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::sync::Arc;

use futures_util::future::Either;

use super::{DetectMatched, Filter, PathState, Router};
use crate::Request;
use crate::handler::Handler;

/// Minimum number of statically-discriminated children that makes hashing the
/// next path segment worthwhile. Smaller sibling lists retain their original
/// linear traversal, avoiding a hash lookup and an auxiliary table.
const STATIC_INDEX_THRESHOLD: usize = 8;

/// An immutable execution plan compiled from a [`Router`].
///
/// `CompiledRouter` keeps the source router alive and builds a lightweight,
/// parallel plan over it. The source tree remains the owner of filters,
/// children, middleware, and goal handlers; the plan only records how filters
/// can be executed and, for sufficiently wide nodes, which children can be
/// ruled out by their first static path segment.
///
/// Detection preserves the source router's depth-first, registration-order
/// semantics. In particular, custom and dynamic children remain ordered with
/// exact static children, and asynchronous filters are still evaluated in the
/// same order as on [`Router`].
pub struct CompiledRouter {
    source: Arc<Router>,
    // Narrow trees use `Router` directly and retain no parallel plan.
    root: Option<CompiledNode>,
}

impl CompiledRouter {
    /// Compile an immutable routing plan from a router.
    ///
    /// The compiled plan owns an [`Arc`] to `router`, so safe code cannot mutate
    /// the source tree while this plan exists. Build or otherwise mutate the
    /// router before calling this function.
    #[must_use]
    pub fn new(router: impl Into<Arc<Router>>) -> Self {
        let source = router.into();
        let root = CompiledNode::contains_index(&source).then(|| CompiledNode::compile(&source));
        Self { source, root }
    }

    /// Return the source router used to build this plan.
    #[inline]
    #[must_use]
    pub fn router(&self) -> &Router {
        &self.source
    }

    /// Return a shared reference-counted handle to the source router.
    #[inline]
    #[must_use]
    pub fn router_arc(&self) -> Arc<Router> {
        self.source.clone()
    }

    /// Detect the first route matching the request.
    ///
    /// This has the same DFS and rollback behavior as [`Router::detect`], while
    /// synchronous filters use [`Filter::filter_sync`] instead of allocating an
    /// `async_trait` future.
    #[inline]
    pub fn detect<'a, 'path>(
        &'a self,
        req: &'a mut Request,
        path_state: &'a mut PathState<'path>,
    ) -> impl Future<Output = Option<DetectMatched>> + 'a {
        // The source router already has the synchronous filter fast path. For
        // ordinary narrow trees, walking a parallel plan only adds pointer
        // chasing; use it only when at least one wide node can benefit from
        // indexed dispatch.
        match &self.root {
            None => Either::Left(self.source.detect(req, path_state)),
            Some(_) => Either::Right(self.detect_indexed(req, path_state)),
        }
    }

    #[inline]
    pub(crate) fn uses_indexed_dispatch(&self) -> bool {
        self.root.is_some()
    }

    pub(crate) async fn detect_indexed(
        &self,
        req: &mut Request,
        path_state: &mut PathState<'_>,
    ) -> Option<DetectMatched> {
        let root = self
            .root
            .as_ref()
            .expect("indexed detection requires a compiled plan");
        if !root
            .filters
            .matches(&self.source.filters, req, path_state)
            .await
        {
            return None;
        }

        let mut stack = vec![DetectFrame::new(&self.source, root, path_state)];
        loop {
            let child_index = stack
                .last_mut()
                .expect("compiled detect stack always contains the current router")
                .candidates
                .next();

            if let Some(child_index) = child_index {
                let (child, child_plan) = {
                    let frame = stack
                        .last()
                        .expect("parent frame exists while testing a child");
                    (
                        &frame.router.routers[child_index],
                        &frame.plan.children[child_index],
                    )
                };

                if child_plan
                    .filters
                    .matches(&child.filters, req, path_state)
                    .await
                {
                    stack.push(DetectFrame::new(child, child_plan, path_state));
                } else {
                    stack
                        .last()
                        .expect("parent frame exists while testing a child")
                        .rollback(path_state);
                }
                continue;
            }

            let frame = stack
                .last()
                .expect("compiled detect stack always contains the current router");
            if path_state.is_ended() {
                path_state.once_ended = true;
                if let Some(goal) = &frame.router.goal {
                    return Some(Self::matched_from_stack(&stack, goal));
                }
            }

            stack.pop();
            let parent = stack.last()?;
            parent.rollback(path_state);
        }
    }

    fn matched_from_stack(stack: &[DetectFrame<'_>], goal: &Arc<dyn Handler>) -> DetectMatched {
        let hoops_len = stack.iter().map(|frame| frame.router.hoops.len()).sum();
        let mut hoops = Vec::with_capacity(hoops_len);
        for frame in stack {
            hoops.extend_from_slice(&frame.router.hoops);
        }
        DetectMatched {
            hoops,
            goal: goal.clone(),
        }
    }
}

impl Debug for CompiledRouter {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledRouter")
            .field("router", &self.source)
            .finish()
    }
}

impl From<Router> for CompiledRouter {
    #[inline]
    fn from(router: Router) -> Self {
        Self::new(router)
    }
}

impl From<Arc<Router>> for CompiledRouter {
    #[inline]
    fn from(router: Arc<Router>) -> Self {
        Self::new(router)
    }
}

struct CompiledNode {
    filters: FilterExecPlan,
    children: Box<[Self]>,
    dispatch: ChildDispatch,
}

impl CompiledNode {
    fn compile(router: &Router) -> Self {
        let filters = FilterExecPlan::compile(&router.filters);
        let dispatch = ChildDispatch::compile(&router.routers);
        let children = router
            .routers
            .iter()
            .map(Self::compile)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            filters,
            children,
            dispatch,
        }
    }

    fn contains_index(router: &Router) -> bool {
        ChildDispatch::should_index(&router.routers)
            || router.routers.iter().any(Self::contains_index)
    }
}

struct FilterExecPlan {
    /// Sync classification for the first 64 filters. Router nodes almost
    /// always contain only one or two filters, so a single word avoids a
    /// per-node mode allocation while retaining exact filter order.
    sync_mask: u64,
}

impl FilterExecPlan {
    fn compile(filters: &[Box<dyn Filter>]) -> Self {
        let sync_mask =
            filters
                .iter()
                .take(u64::BITS as usize)
                .enumerate()
                .fold(0, |mask, (index, filter)| {
                    if filter.is_sync() {
                        mask | (1 << index)
                    } else {
                        mask
                    }
                });
        Self { sync_mask }
    }

    async fn matches(
        &self,
        filters: &[Box<dyn Filter>],
        req: &mut Request,
        path_state: &mut PathState<'_>,
    ) -> bool {
        for (index, filter) in filters.iter().enumerate() {
            let is_sync = if index < u64::BITS as usize {
                self.sync_mask & (1 << index) != 0
            } else {
                // Avoid allocating overflow mode words for the pathological
                // case of a router node with more than 64 filters.
                filter.is_sync()
            };
            let matched = if is_sync {
                filter.filter_sync(req, path_state)
            } else {
                filter.filter(req, path_state).await
            };
            if !matched {
                return false;
            }
        }
        true
    }
}

enum ChildDispatch {
    Linear {
        len: usize,
    },
    Indexed {
        exact: HashMap<String, Vec<usize>>,
        generic: Vec<usize>,
    },
}

impl ChildDispatch {
    fn should_index(children: &[Router]) -> bool {
        children
            .iter()
            .filter(|child| first_static_segment(child).is_some())
            .take(STATIC_INDEX_THRESHOLD)
            .count()
            == STATIC_INDEX_THRESHOLD
    }

    fn compile(children: &[Router]) -> Self {
        if !Self::should_index(children) {
            return Self::Linear {
                len: children.len(),
            };
        }

        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        let mut generic = Vec::new();

        for (index, child) in children.iter().enumerate() {
            if let Some(segment) = first_static_segment(child) {
                exact.entry(segment.to_owned()).or_default().push(index);
            } else {
                generic.push(index);
            }
        }
        Self::Indexed { exact, generic }
    }

    fn candidates<'a>(&'a self, path_state: &PathState<'_>) -> CandidateIter<'a> {
        match self {
            Self::Linear { len } => CandidateIter::Linear { next: 0, len: *len },
            Self::Indexed { exact, generic } => {
                let exact = path_state
                    .pick()
                    .and_then(|segment| exact.get(segment))
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                CandidateIter::Merged {
                    exact,
                    generic,
                    exact_pos: 0,
                    generic_pos: 0,
                }
            }
        }
    }
}

/// Return a declared exact first-segment discriminator. A mismatch is known to
/// fail before any later filter or path capture can have an observable effect.
fn first_static_segment(router: &Router) -> Option<&str> {
    let filter = router.filters.first()?;
    filter.static_path_segment()
}

enum CandidateIter<'a> {
    Linear {
        next: usize,
        len: usize,
    },
    Merged {
        exact: &'a [usize],
        generic: &'a [usize],
        exact_pos: usize,
        generic_pos: usize,
    },
}

impl Iterator for CandidateIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Linear { next, len } => {
                if *next < *len {
                    let value = *next;
                    *next += 1;
                    Some(value)
                } else {
                    None
                }
            }
            Self::Merged {
                exact,
                generic,
                exact_pos,
                generic_pos,
            } => match (exact.get(*exact_pos), generic.get(*generic_pos)) {
                (Some(exact), Some(generic)) => {
                    if exact < generic {
                        *exact_pos += 1;
                        Some(*exact)
                    } else {
                        *generic_pos += 1;
                        Some(*generic)
                    }
                }
                (Some(exact), None) => {
                    *exact_pos += 1;
                    Some(*exact)
                }
                (None, Some(generic)) => {
                    *generic_pos += 1;
                    Some(*generic)
                }
                (None, None) => None,
            },
        }
    }
}

struct DetectFrame<'a> {
    router: &'a Router,
    plan: &'a CompiledNode,
    candidates: CandidateIter<'a>,
    original_cursor: (usize, usize),
    params_snapshot: (usize, bool, usize),
    #[cfg(feature = "matched-path")]
    original_matched_parts_len: usize,
}

impl<'a> DetectFrame<'a> {
    fn new(router: &'a Router, plan: &'a CompiledNode, path_state: &PathState<'_>) -> Self {
        debug_assert_eq!(router.routers.len(), plan.children.len());
        Self {
            router,
            plan,
            candidates: plan.dispatch.candidates(path_state),
            original_cursor: path_state.cursor,
            params_snapshot: path_state.params.snapshot(),
            #[cfg(feature = "matched-path")]
            original_matched_parts_len: path_state.matched_parts.len(),
        }
    }

    fn rollback(&self, path_state: &mut PathState<'_>) {
        #[cfg(feature = "matched-path")]
        path_state
            .matched_parts
            .truncate(self.original_matched_parts_len);
        path_state.cursor = self.original_cursor;
        path_state.params.rollback(self.params_snapshot);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::async_trait;
    use crate::handler;
    use crate::test::TestClient;

    #[handler]
    async fn goal() {}

    #[derive(Debug)]
    struct AsyncFilter {
        calls: Arc<AtomicUsize>,
        result: bool,
    }

    #[async_trait]
    impl Filter for AsyncFilter {
        async fn filter(&self, _req: &mut Request, _path: &mut PathState<'_>) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result
        }
    }

    #[tokio::test]
    async fn preserves_dynamic_before_static_registration_order() {
        let router = Router::new()
            .push(Router::with_path("miss-0").goal(goal))
            .push(Router::with_path("miss-1").goal(goal))
            .push(Router::with_path("miss-2").goal(goal))
            .push(Router::with_path("miss-3").goal(goal))
            .push(Router::with_path("{dynamic}").goal(goal))
            .push(Router::with_path("miss-5").goal(goal))
            .push(Router::with_path("miss-6").goal(goal))
            .push(Router::with_path("target").goal(goal))
            .push(Router::with_path("miss-8").goal(goal));
        let compiled = CompiledRouter::new(router);
        let mut req = TestClient::get("http://localhost/target").build();
        let mut state = PathState::from_owned_path(req.uri().path().to_owned());

        assert!(compiled.detect(&mut req, &mut state).await.is_some());
        assert_eq!(
            state.params.get("dynamic").map(String::as_str),
            Some("target")
        );
    }

    #[test]
    fn narrow_router_does_not_retain_a_parallel_plan() {
        let router = Router::new().push(Router::with_path("health").goal(goal));
        let compiled = CompiledRouter::new(router);

        assert!(compiled.root.is_none());
    }

    #[tokio::test]
    async fn rolls_back_failed_indexed_candidate_params() {
        let mut router = Router::new();
        for index in 0..8 {
            router = router.push(Router::with_path(format!("static-{index}")).goal(goal));
        }
        router = router
            .push(Router::with_path("{failed}/profile").goal(goal))
            .push(Router::with_path("{matched}").goal(goal));

        let compiled = CompiledRouter::new(router);
        let mut req = TestClient::get("http://localhost/alice").build();
        let mut state = PathState::from_owned_path(req.uri().path().to_owned());

        assert!(compiled.detect(&mut req, &mut state).await.is_some());
        assert!(!state.params.contains_key("failed"));
        assert_eq!(
            state.params.get("matched").map(String::as_str),
            Some("alice")
        );
    }

    #[tokio::test]
    async fn indexed_dispatch_preserves_async_generic_and_mixed_filters() {
        let generic_calls = Arc::new(AtomicUsize::new(0));
        let target_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        for index in 0..8 {
            router = router.push(Router::with_path(format!("static-{index}")).goal(goal));
        }
        router = router
            .push(
                Router::with_filter(AsyncFilter {
                    calls: generic_calls.clone(),
                    result: false,
                })
                .goal(goal),
            )
            .push(
                Router::with_filter(crate::routing::filters::path("target").and(AsyncFilter {
                    calls: target_calls.clone(),
                    result: true,
                }))
                .goal(goal),
            );

        let compiled = CompiledRouter::new(router);
        let mut req = TestClient::get("http://localhost/target").build();
        let mut state = PathState::from_owned_path(req.uri().path().to_owned());

        assert!(compiled.detect(&mut req, &mut state).await.is_some());
        assert_eq!(generic_calls.load(Ordering::SeqCst), 1);
        assert_eq!(target_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn indexed_detection_matches_router_state_and_result() {
        fn build_router() -> Router {
            let mut router = Router::new().hoop(goal);
            for index in 0..8 {
                router = router.push(Router::with_path(format!("miss-{index}")).goal(goal));
            }
            router
                .push(Router::with_path("{failed}/profile").goal(goal))
                .push(Router::with_path("target/{id}").get(goal))
        }

        async fn compare(mut source_req: Request, mut compiled_req: Request) {
            let router = Arc::new(build_router());
            let compiled = CompiledRouter::new(router.clone());
            assert!(compiled.uses_indexed_dispatch());

            let path = source_req.uri().path().to_owned();
            let mut source_state = PathState::from_borrowed_path(&path);
            let mut compiled_state = PathState::from_borrowed_path(&path);
            let source_match = router.detect(&mut source_req, &mut source_state).await;
            let compiled_match = compiled
                .detect(&mut compiled_req, &mut compiled_state)
                .await;

            assert_eq!(source_match.is_some(), compiled_match.is_some());
            if let (Some(source_match), Some(compiled_match)) = (source_match, compiled_match) {
                assert_eq!(
                    source_match.goal.type_name(),
                    compiled_match.goal.type_name()
                );
                assert_eq!(source_match.hoops.len(), compiled_match.hoops.len());
                assert!(
                    source_match
                        .hoops
                        .iter()
                        .zip(&compiled_match.hoops)
                        .all(|(source, compiled)| source.type_name() == compiled.type_name())
                );
            }
            assert_eq!(source_state, compiled_state);
        }

        compare(
            TestClient::get("http://localhost/target/alice").build(),
            TestClient::get("http://localhost/target/alice").build(),
        )
        .await;
        compare(
            TestClient::post("http://localhost/target/alice").build(),
            TestClient::post("http://localhost/target/alice").build(),
        )
        .await;
    }
}
