#![allow(missing_docs)]
//! Benchmarks for the routing hot path: `Router::detect` and
//! `CompiledRouter::detect`.
//!
//! Scenarios cover the shapes that stress different parts of the matcher:
//! flat static routes, dynamic params, deep nesting, wide sibling fan-out
//! (worst case for the linear scan), and wildcard tails. Run with
//! `cargo bench -p salvo_core --bench router_detect`.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use salvo_core::routing::{CompiledRouter, PathState};
use salvo_core::test::TestClient;
use salvo_core::{Router, handler};

#[handler]
async fn goal() -> &'static str {
    "ok"
}

fn bench_detect(c: &mut Criterion, name: &str, router: Router, url: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build runtime");
    let compiled = CompiledRouter::new(router);
    let mut source_req = TestClient::get(url).build();
    let mut compiled_req = TestClient::get(url).build();
    let path = source_req.uri().path().to_owned();
    let source_name = format!("router/{name}");
    let compiled_name = format!("compiled/{name}");

    c.bench_function(&source_name, |b| {
        b.iter(|| {
            let mut state = PathState::from_borrowed_path(&path);
            let matched = rt.block_on(compiled.router().detect(&mut source_req, &mut state));
            assert!(matched.is_some(), "route must match in benchmark");
            black_box(state);
        });
    });
    c.bench_function(&compiled_name, |b| {
        b.iter(|| {
            let mut state = PathState::from_borrowed_path(&path);
            let matched = rt.block_on(compiled.detect(&mut compiled_req, &mut state));
            assert!(matched.is_some(), "route must match in benchmark");
            black_box(state);
        });
    });
}

fn static_shallow(c: &mut Criterion) {
    let router = Router::new()
        .push(Router::with_path("users").goal(goal))
        .push(Router::with_path("articles").goal(goal))
        .push(Router::with_path("health").goal(goal));
    bench_detect(c, "static_shallow", router, "http://t.dev/health");
}

fn dynamic_params(c: &mut Criterion) {
    let router = Router::new().push(
        Router::with_path("users").push(
            Router::with_path("{id}")
                .push(Router::with_path("articles").push(Router::with_path("{aid}").goal(goal))),
        ),
    );
    bench_detect(
        c,
        "dynamic_params",
        router,
        "http://t.dev/users/12345/articles/67890",
    );
}

fn deep_tree(c: &mut Criterion) {
    // Eight nested levels alternating static and param segments.
    let mut leaf = Router::with_path("leaf").goal(goal);
    for level in (0..8).rev() {
        let seg = if level % 2 == 0 {
            format!("level{level}")
        } else {
            format!("{{p{level}}}")
        };
        leaf = Router::with_path(seg).push(leaf);
    }
    let router = Router::new().push(leaf);
    bench_detect(
        c,
        "deep_tree",
        router,
        "http://t.dev/level0/v1/level2/v3/level4/v5/level6/v7/leaf",
    );
}

fn wide_siblings(c: &mut Criterion) {
    // Requests match the last sibling, stressing the linear scan and showing
    // where compiled static dispatch starts paying for its hash lookup.
    for count in [8, 16, 100] {
        let mut parent = Router::with_path("api");
        for index in 0..count {
            parent = parent.push(Router::with_path(format!("res{index:03}")).goal(goal));
        }
        let router = Router::new().push(parent);
        bench_detect(
            c,
            &format!("wide_siblings_{count}_last"),
            router,
            &format!("http://t.dev/api/res{:03}", count - 1),
        );
    }
}

fn wildcard_tail(c: &mut Criterion) {
    let router = Router::new().push(Router::with_path("assets/{**rest}").goal(goal));
    bench_detect(
        c,
        "wildcard_tail",
        router,
        "http://t.dev/assets/css/site/theme/main.css",
    );
}

fn sibling_param_backtrack(c: &mut Criterion) {
    // Earlier siblings capture params and then fail on a deeper segment,
    // exercising the snapshot/rollback path before the last sibling matches.
    let router = Router::new().push(
        Router::with_path("users")
            .push(Router::with_path("{id}/profile").goal(goal))
            .push(Router::with_path("{id}/settings").goal(goal))
            .push(Router::with_path("{name}").goal(goal)),
    );
    bench_detect(
        c,
        "sibling_param_backtrack",
        router,
        "http://t.dev/users/alice",
    );
}

fn benches(c: &mut Criterion) {
    static_shallow(c);
    dynamic_params(c);
    deep_tree(c);
    wide_siblings(c);
    wildcard_tail(c);
    sibling_param_backtrack(c);
}

criterion_group!(router_detect, benches);
criterion_main!(router_detect);
