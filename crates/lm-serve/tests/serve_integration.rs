use std::{io::Write, path::PathBuf, time::Duration};

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use lm_core::fixture::build_fixture;
use lm_serve::{build_router, ServeConfig};
use tempfile::NamedTempFile;
use tower::ServiceExt;

// ── helpers ────────────────────────────────────────────────────────────────────

fn fixture_pmtiles(name_hint: &str) -> (NamedTempFile, PathBuf) {
    let data = build_fixture(&[(0, 0, 0, b"tile".to_vec())]);
    let mut f = tempfile::Builder::new()
        .prefix(name_hint)
        .suffix(".pmtiles")
        .tempfile()
        .unwrap();
    f.write_all(&data).unwrap();
    let path = f.path().to_path_buf();
    (f, path)
}

fn default_cfg() -> ServeConfig {
    ServeConfig {
        addr: "127.0.0.1:0".into(),
        base_url: Some("http://test".into()),
        cors_origins: vec![],
        request_timeout: Duration::from_secs(10),
        ..ServeConfig::default()
    }
}

fn make_router(paths: &[PathBuf], cfg: &ServeConfig) -> Router {
    build_router(paths, cfg).unwrap()
}

async fn send(router: Router, req: Request<Body>) -> axum::response::Response {
    router.oneshot(req).await.unwrap()
}

async fn body_bytes(res: axum::response::Response) -> Vec<u8> {
    res.into_body().collect().await.unwrap().to_bytes().to_vec()
}

async fn body_json(res: axum::response::Response) -> serde_json::Value {
    let bytes = body_bytes(res).await;
    serde_json::from_slice(&bytes).unwrap()
}

// ── /healthz ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn healthz_returns_200() {
    let (_f, path) = fixture_pmtiles("healthz");
    let router = make_router(&[path], &default_cfg());
    let req = Request::builder().uri("/healthz").body(Body::empty()).unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
}

// ── /tilesets ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tilesets_lists_loaded_set() {
    let (_f, path) = fixture_pmtiles("world");
    let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
    let router = make_router(&[path], &default_cfg());
    let req = Request::builder().uri("/tilesets").body(Body::empty()).unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let names = body["tilesets"].as_array().unwrap();
    assert!(names.iter().any(|v| v.as_str() == Some(&name)));
}

// ── TileJSON ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tilejson_returns_valid_json() {
    let (_f, path) = fixture_pmtiles("roads");
    let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
    let router = make_router(&[path], &default_cfg());
    let req = Request::builder()
        .uri(format!("/tilesets/{name}/tile.json"))
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["tilejson"], "3.0.0");
    assert!(body["tiles"][0].as_str().unwrap().contains(&name));
}

#[tokio::test]
async fn tilejson_unknown_set_is_404() {
    let (_f, path) = fixture_pmtiles("known");
    let router = make_router(&[path], &default_cfg());
    let req = Request::builder()
        .uri("/tilesets/unknown/tile.json")
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

// ── tile serving ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn tile_z0_returns_200_with_mvt_content_type() {
    let (_f, path) = fixture_pmtiles("tiles");
    let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
    let router = make_router(&[path], &default_cfg());
    let req = Request::builder()
        .uri(format!("/tiles/{name}/0/0/0.mvt"))
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
    assert!(ct.contains("mapbox-vector-tile"), "unexpected content-type: {ct}");
}

#[tokio::test]
async fn tile_missing_returns_204() {
    let (_f, path) = fixture_pmtiles("sparse");
    let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
    let router = make_router(&[path], &default_cfg());
    let req = Request::builder()
        .uri(format!("/tiles/{name}/2/3/3.mvt"))
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn tile_has_cache_control_and_etag() {
    let (_f, path) = fixture_pmtiles("cachetest");
    let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
    let router = make_router(&[path], &default_cfg());
    let req = Request::builder()
        .uri(format!("/tiles/{name}/0/0/0.mvt"))
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key(header::ETAG));
    let cc = res.headers().get(header::CACHE_CONTROL).unwrap().to_str().unwrap();
    assert!(cc.contains("max-age"), "expected max-age in cache-control: {cc}");
}

// ── conditional GET (304) ─────────────────────────────────────────────────────

#[tokio::test]
async fn conditional_get_returns_304() {
    let (_f, path) = fixture_pmtiles("etag");
    let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
    let cfg = default_cfg();

    // First request to get ETag.
    let router = make_router(&[path.clone()], &cfg);
    let req = Request::builder()
        .uri(format!("/tiles/{name}/0/0/0.mvt"))
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let etag = res.headers().get(header::ETAG).unwrap().clone();

    // Second request with If-None-Match.
    let router2 = make_router(&[path], &cfg);
    let req2 = Request::builder()
        .uri(format!("/tiles/{name}/0/0/0.mvt"))
        .header(header::IF_NONE_MATCH, etag)
        .body(Body::empty())
        .unwrap();
    let res2 = send(router2, req2).await;
    assert_eq!(res2.status(), StatusCode::NOT_MODIFIED);
}

// ── z/x/y validation ─────────────────────────────────────────────────────────

#[tokio::test]
async fn z_above_max_zoom_request_returns_400() {
    let (_f, path) = fixture_pmtiles("zval");
    let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
    let cfg = ServeConfig {
        max_zoom_request: 5,
        ..default_cfg()
    };
    let router = make_router(&[path], &cfg);
    let req = Request::builder()
        .uri(format!("/tiles/{name}/6/0/0.mvt"))
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn x_out_of_range_returns_400() {
    let (_f, path) = fixture_pmtiles("xyval");
    let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
    let router = make_router(&[path], &default_cfg());
    // At z=1, valid x is 0 or 1. x=2 is out of range.
    let req = Request::builder()
        .uri(format!("/tiles/{name}/1/2/0.mvt"))
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// ── auth ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn auth_rejects_missing_token() {
    let (_f, path) = fixture_pmtiles("authtest");
    let cfg = ServeConfig {
        api_key: Some("secret".into()),
        ..default_cfg()
    };
    let router = make_router(&[path], &cfg);
    let req = Request::builder().uri("/healthz").body(Body::empty()).unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_accepts_correct_token() {
    let (_f, path) = fixture_pmtiles("authgood");
    let cfg = ServeConfig {
        api_key: Some("secret".into()),
        ..default_cfg()
    };
    let router = make_router(&[path], &cfg);
    let req = Request::builder()
        .uri("/healthz")
        .header(header::AUTHORIZATION, "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_rejects_wrong_token() {
    let (_f, path) = fixture_pmtiles("authbad");
    let cfg = ServeConfig {
        api_key: Some("secret".into()),
        ..default_cfg()
    };
    let router = make_router(&[path], &cfg);
    let req = Request::builder()
        .uri("/healthz")
        .header(header::AUTHORIZATION, "Bearer wrong")
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

// ── CORS ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cors_wildcard_allows_any_origin() {
    let (_f, path) = fixture_pmtiles("corstest");
    let cfg = ServeConfig {
        cors_origins: vec!["*".into()],
        ..default_cfg()
    };
    let router = make_router(&[path], &cfg);
    let req = Request::builder()
        .uri("/healthz")
        .header(header::ORIGIN, "http://whatever.example.com")
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let acao = res.headers().get("access-control-allow-origin");
    assert!(
        acao.map(|v| v == "*").unwrap_or(false),
        "expected ACAO: *, got {:?}",
        acao
    );
}

#[tokio::test]
async fn cors_no_origins_sends_no_acao_header() {
    let (_f, path) = fixture_pmtiles("nocors");
    let router = make_router(&[path], &default_cfg());
    let req = Request::builder()
        .uri("/healthz")
        .header(header::ORIGIN, "http://attacker.example.com")
        .body(Body::empty())
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        !res.headers().contains_key("access-control-allow-origin"),
        "unexpected ACAO header present"
    );
}
