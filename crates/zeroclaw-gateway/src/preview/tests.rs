use super::*;

use std::collections::BTreeSet;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

fn bootstrap(list: &[&str], socket_env: Option<&str>) -> Result<Bootstrap, String> {
    match parse_args(args(list), socket_env.map(str::to_owned))? {
        Invocation::Serve(bootstrap) => Ok(bootstrap),
        other => panic!("expected a serve invocation, got {other:?}"),
    }
}

// ── Bootstrap ────────────────────────────────────────────────────

#[test]
fn the_endpoint_comes_from_the_socket_flag_then_the_environment_then_the_data_dir() {
    let flag = bootstrap(
        &["--socket", "/run/flag.sock", "--data-dir", "/var/zc"],
        Some("/run/env.sock"),
    )
    .unwrap();
    assert_eq!(flag.endpoint, PathBuf::from("/run/flag.sock"));

    let env = bootstrap(&["--data-dir", "/var/zc"], Some("/run/env.sock")).unwrap();
    assert_eq!(env.endpoint, PathBuf::from("/run/env.sock"));

    let data_dir = bootstrap(&["--data-dir", "/var/zc"], Some("  ")).unwrap();
    assert_eq!(
        data_dir.endpoint,
        zeroclaw_rpc_client::endpoint::default_endpoint(Path::new("/var/zc"))
    );
}

/// The admin token directory is the `--data-dir` flag and nothing else: a
/// socket path names no data directory, whichever way it arrives.
#[test]
fn only_the_data_dir_flag_names_where_the_admin_token_is() {
    let socket = bootstrap(&["--socket", "/run/zc.sock"], None).unwrap();
    assert_eq!(socket.data_dir, None);
    let env = bootstrap(&[], Some("/run/env.sock")).unwrap();
    assert_eq!(env.data_dir, None);

    let data_dir = bootstrap(&["--data-dir", "/var/zc"], None).unwrap();
    assert_eq!(data_dir.data_dir, Some(PathBuf::from("/var/zc")));
    // A socket given too decides the endpoint, not where the token is.
    let both = bootstrap(&["--socket", "/run/zc.sock", "--data-dir", "/var/zc"], None).unwrap();
    assert_eq!(both.endpoint, PathBuf::from("/run/zc.sock"));
    assert_eq!(both.data_dir, Some(PathBuf::from("/var/zc")));
}

#[test]
fn without_an_endpoint_it_refuses_and_never_falls_back_to_config() {
    let refused = bootstrap(&[], None).unwrap_err();
    assert!(
        refused.contains("--socket") && refused.contains("config.toml"),
        "{refused}"
    );
    for flag in ["--config", "--config-dir"] {
        let refused = bootstrap(&[flag, "/etc/zeroclaw"], None).unwrap_err();
        assert!(refused.contains("never reads config.toml"), "{refused}");
    }
}

#[test]
fn defaults_serve_on_loopback_without_tls_or_dashboard() {
    let plain = bootstrap(&["--socket", "/run/zc.sock"], None).unwrap();
    assert_eq!(plain.listen, DEFAULT_LISTEN.parse::<SocketAddr>().unwrap());
    assert_eq!(plain.web_dist, None);
    assert_eq!(plain.tls, None);
    assert_eq!(
        plain.request_timeout,
        Duration::from_secs(crate::REQUEST_TIMEOUT_SECS),
        "the in-process gateway's default"
    );
}

#[test]
fn a_public_listen_address_needs_an_explicit_opt_in() {
    let refused = bootstrap(&["--socket", "/s", "--listen", "0.0.0.0:8080"], None).unwrap_err();
    assert!(refused.contains("--allow-public-bind"), "{refused}");
    let allowed = bootstrap(
        &[
            "--socket",
            "/s",
            "--listen",
            "0.0.0.0:8080",
            "--allow-public-bind",
        ],
        None,
    )
    .unwrap();
    assert_eq!(allowed.listen.port(), 8080);
}

#[test]
fn tls_needs_both_files_and_malformed_arguments_are_refused() {
    let tls = bootstrap(
        &[
            "--socket",
            "/s",
            "--tls-cert",
            "c.pem",
            "--tls-key",
            "k.pem",
        ],
        None,
    )
    .unwrap();
    assert_eq!(
        tls.tls,
        Some(TlsFiles {
            cert: "c.pem".into(),
            key: "k.pem".into()
        })
    );
    for list in [
        &["--socket", "/s", "--tls-cert", "c.pem"][..],
        &["--socket", "/s", "--tls-key", "k.pem"][..],
        &["--socket"][..],
        &["--socket", "/s", "--listen", "not-an-address"][..],
        &["--socket", "/s", "--frobnicate"][..],
    ] {
        assert!(bootstrap(list, None).is_err(), "{list:?} must be refused");
    }
    assert_eq!(parse_args(args(&["--help"]), None), Ok(Invocation::Help));
    assert_eq!(parse_args(args(&["-V"]), None), Ok(Invocation::Version));
}

#[test]
fn the_request_timeout_is_a_positive_number_of_seconds() {
    let set = bootstrap(&["--socket", "/s", "--request-timeout", "5"], None).unwrap();
    assert_eq!(set.request_timeout, Duration::from_secs(5));
    for bad in ["0", "-1", "soon", ""] {
        assert!(
            bootstrap(&["--socket", "/s", "--request-timeout", bad], None).is_err(),
            "{bad:?} must be refused"
        );
    }
}

// ── The fail-closed route map ────────────────────────────────────

/// Routes of the in-process gateway the preview serves: its own, then the
/// dashboard routes the core serves.
const SERVED: &[&str] = &[
    "/health",
    "/api/openapi.json",
    "/api/docs",
    "/api/health",
    "/api/tuis",
    "/api/cost",
    "/api/events/history",
    "/api/sessions",
    "/admin/shutdown",
    "/hooks/claude-code",
];

/// Route paths the in-process gateway registers with a string literal, from
/// its production source (test modules excluded).
///
/// A source scan, not the router itself: it covers the four files that
/// register production routes today. A route registered elsewhere, or
/// through a `const`, `format!`, `route_service` or `nest`, is invisible to
/// it; add that file here, or list the path by hand, when one appears. The
/// other route-registering modules (`ws`, `sse`, `api_pairing`,
/// `api_config`, `webhook_ingress`) register routes only in their tests.
fn in_process_routes() -> BTreeSet<String> {
    let sources = [
        include_str!("../lib.rs"),
        include_str!("../api_oidc.rs"),
        include_str!("../a2a.rs"),
        include_str!("../plugin_webhook.rs"),
    ];
    let mut paths = BTreeSet::new();
    for source in sources {
        let production = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("split yields the head");
        let mut rest = production;
        while let Some(at) = rest.find(".route(") {
            rest = &rest[at + ".route(".len()..];
            let literal = rest.trim_start();
            if let Some(body) = literal.strip_prefix('"')
                && let Some(end) = body.find('"')
            {
                paths.insert(body[..end].to_owned());
            }
        }
    }
    paths
}

#[test]
fn every_in_process_route_is_served_or_refused() {
    let classified: BTreeSet<&str> = REFUSED
        .iter()
        .map(|(path, _, _)| *path)
        .chain(SERVED.iter().copied())
        .collect();
    let routes = in_process_routes();
    assert!(routes.len() > 100, "the scan found the router: {routes:?}");
    let unclassified: Vec<&String> = routes
        .iter()
        .filter(|path| !path.starts_with("/_app/") && !classified.contains(path.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "routes the preview neither serves nor refuses: {unclassified:?}"
    );

    // The table names no route the in-process gateway lacks, except the A2A
    // card paths, which the router builds from constants.
    let from_constants = [
        "/.well-known/agents-card.json",
        "/a2a/.well-known/agents-card.json",
        "/a2a/{alias}/.well-known/agent-card.json",
    ];
    let stale: Vec<&&str> = classified
        .iter()
        .filter(|path| !routes.contains(**path) && !from_constants.contains(path))
        .collect();
    assert!(
        stale.is_empty(),
        "the table names unknown routes: {stale:?}"
    );
}

#[test]
fn every_refused_route_names_known_methods() {
    for (path, methods, _) in REFUSED {
        assert!(!methods.is_empty(), "{path}");
        // Panics on a method the router cannot express.
        let _ = method_filter(methods);
    }
}

// ── Refusals the operator can act on ─────────────────────────────

async fn explained(error: CoreError) -> (StatusCode, serde_json::Value) {
    use http_body_util::BodyExt as _;
    let response = explain(error);
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn a_protocol_mismatch_says_to_install_matching_versions() {
    let (status, body) = explained(CoreError::Rpc(zeroclaw_api::jsonrpc::JsonRpcError {
        code: zeroclaw_api::jsonrpc::error_codes::VERSION_MISMATCH,
        message: "Protocol version mismatch: server=1, client=2".into(),
        data: None,
    }))
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "core_incompatible");
    assert_eq!(
        body["error"], "Protocol version mismatch: server=1, client=2",
        "the core's own words are kept"
    );
    assert!(
        body["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("matching versions")),
        "{body}"
    );
}

#[tokio::test]
async fn a_refused_credential_and_an_unreachable_core_explain_themselves_distinctly() {
    let (status, body) = explained(CoreError::AuthRequired("credential rejected".into())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "auth_required");
    assert!(
        body["hint"].as_str().is_some_and(|h| h.contains("Bearer")),
        "{body}"
    );

    let (status, body) = explained(CoreError::Unavailable("socket missing".into())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "core_unavailable");
    assert!(
        body["hint"]
            .as_str()
            .is_some_and(|h| h.contains("zeroclaw daemon")),
        "{body}"
    );
}

#[tokio::test]
async fn busy_timeout_and_an_untrusted_endpoint_get_their_own_hints() {
    let (status, body) = explained(CoreError::UntrustedEndpoint(
        "refusing to send the credential".into(),
    ))
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "core_untrusted_endpoint");
    assert!(
        body["hint"]
            .as_str()
            .is_some_and(|h| h.contains("same OS account")),
        "{body}"
    );

    let (_, busy) = explained(CoreError::Busy).await;
    let (_, timeout) = explained(CoreError::Timeout).await;
    assert_eq!(busy["code"], "core_busy");
    assert_eq!(timeout["code"], "core_timeout");
    for body in [&busy, &timeout] {
        assert!(
            !body["hint"]
                .as_str()
                .is_some_and(|h| h.contains("zeroclaw daemon")),
            "a running core is not told to start: {body}"
        );
    }
    assert_ne!(busy["hint"], timeout["hint"]);
}

// ── Gateway-local routes ─────────────────────────────────────────

/// What the gateway-local route tests send, and the routers they send it to.
mod gateway_local {
    use super::*;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    pub(super) const ADMIN_TOKEN: &str = "zc_admin_preview_test_token";
    pub(super) const HOOK_EVENT: &str =
        r#"{"session_id":"s1","event_type":"tool_use","tool_name":"Bash","summary":"ls"}"#;

    pub(super) fn loopback() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 40_000))
    }

    /// A caller on another machine.
    pub(super) fn remote() -> SocketAddr {
        SocketAddr::from(([203, 0, 113, 7], 40_000))
    }

    pub(super) async fn answer(router: &Router, request: Request<Body>) -> (StatusCode, String) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// A JSON `POST` from `peer`, with any extra `headers`.
    pub(super) fn request(
        path: &str,
        peer: SocketAddr,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Request<Body> {
        let mut request = Request::post(path)
            .header("content-type", "application/json")
            .extension(ConnectInfo(peer));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(Body::from(body.to_owned())).unwrap()
    }

    /// The preview's router with no core behind it (neither route may need
    /// one), reading its admin token from `admin_token_dir`.
    pub(super) fn preview(
        dir: &Path,
        admin_token_dir: Option<&Path>,
    ) -> (Router, watch::Receiver<bool>) {
        let endpoint = dir.join("daemon.sock");
        let (stop, stopped) = watch::channel(false);
        let router = router(
            CoreRpc::local(endpoint.clone(), EndpointOwner::SameAccount),
            endpoint,
            None,
            stop,
            admin_token_dir.map(Path::to_path_buf),
            Duration::from_secs(crate::REQUEST_TIMEOUT_SECS),
        );
        (router, stopped)
    }

    /// Write `token` where a gateway serving `data_dir` keeps its admin
    /// token, standing in for the in-process gateway that mints it.
    pub(super) fn write_admin_token(data_dir: &Path, token: &str) {
        std::fs::write(
            zeroclaw_config::pairing::gateway_admin_token_path(data_dir),
            token,
        )
        .unwrap();
    }

    pub(super) fn code_of(body: &str) -> String {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

/// `/admin/shutdown` and `/hooks/claude-code` need no core. To a caller the
/// preview admits, each answers exactly as the in-process route does. The
/// preview admits fewer callers: the hook only from loopback, and shutdown
/// only from loopback with the admin token.
#[tokio::test]
async fn the_gateway_local_routes_answer_as_the_in_process_gateway_does() {
    use gateway_local::{ADMIN_TOKEN, HOOK_EVENT, answer, loopback, remote, request};

    let tmp = tempfile::tempdir().unwrap();
    gateway_local::write_admin_token(tmp.path(), ADMIN_TOKEN);
    let (preview, preview_stopped) = gateway_local::preview(tmp.path(), Some(tmp.path()));
    let state = crate::api::test_state(zeroclaw_config::schema::Config {
        data_dir: tmp.path().to_path_buf(),
        config_path: tmp.path().join("config.toml"),
        ..Default::default()
    });
    let in_process_stopped = state.shutdown_tx.subscribe();
    let in_process: Router = Router::new()
        .route("/admin/shutdown", post(crate::handle_admin_shutdown))
        .route(
            "/hooks/claude-code",
            post(crate::api::handle_claude_code_hook),
        )
        .with_state(state);
    // What the in-process gateway answers a caller that is not on loopback.
    let localhost_only = answer(&in_process, request("/admin/shutdown", remote(), &[], "")).await;
    assert_eq!(localhost_only.0, StatusCode::FORBIDDEN);

    // The hook from loopback: the same answers, malformed bodies included.
    for body in [HOOK_EVENT, "not json", r#"{"event_type":"tool_use"}"#] {
        let served = answer(
            &preview,
            request("/hooks/claude-code", loopback(), &[], body),
        )
        .await;
        let reference = answer(
            &in_process,
            request("/hooks/claude-code", loopback(), &[], body),
        )
        .await;
        assert_eq!(served, reference, "{body}");
    }
    let (status, body) = answer(
        &preview,
        request("/hooks/claude-code", loopback(), &[], HOOK_EVENT),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        json!({ "ok": true })
    );

    // The hook from another machine, public or on the local network: the
    // in-process route acknowledges it; the preview refuses it as the
    // localhost-only routes refuse.
    for peer in [remote(), SocketAddr::from(([10, 0, 0, 5], 40_000))] {
        let reference = answer(
            &in_process,
            request("/hooks/claude-code", peer, &[], HOOK_EVENT),
        )
        .await;
        assert_eq!(reference.0, StatusCode::OK, "{peer}");
        let served = answer(
            &preview,
            request("/hooks/claude-code", peer, &[], HOOK_EVENT),
        )
        .await;
        assert_eq!(served, localhost_only, "{peer}");
    }

    // Shutdown from another machine: refused the same way, token or not,
    // and nothing stops.
    let token = [(
        zeroclaw_config::pairing::GATEWAY_ADMIN_TOKEN_HEADER,
        ADMIN_TOKEN,
    )];
    for headers in [&[][..], &token[..]] {
        let served = answer(&preview, request("/admin/shutdown", remote(), headers, "")).await;
        assert_eq!(served, localhost_only);
    }
    assert!(!*preview_stopped.borrow() && !*in_process_stopped.borrow());

    // Shutdown from loopback with the admin token: the in-process answer,
    // and each asks its own process to stop.
    let served = answer(&preview, request("/admin/shutdown", loopback(), &token, "")).await;
    let reference = answer(&in_process, request("/admin/shutdown", loopback(), &[], "")).await;
    assert_eq!(served, reference);
    assert_eq!(served.0, StatusCode::OK, "{}", served.1);
    assert!(*preview_stopped.borrow() && *in_process_stopped.borrow());
}

/// A loopback address alone stops nothing: a page in the operator's browser
/// and a same-host reverse proxy both reach the preview from loopback. Only
/// the admin token read from the data directory admits a shutdown, and a
/// preview that was given no data directory admits none.
#[tokio::test]
async fn shutdown_needs_the_admin_token_from_the_data_dir() {
    use gateway_local::{ADMIN_TOKEN, answer, code_of, loopback, request};

    const HEADER: &str = zeroclaw_config::pairing::GATEWAY_ADMIN_TOKEN_HEADER;
    let tmp = tempfile::tempdir().unwrap();
    let (preview, stopped) = gateway_local::preview(tmp.path(), Some(tmp.path()));

    // No gateway has written a token yet: nothing presented can match.
    for headers in [&[][..], &[(HEADER, ADMIN_TOKEN)][..], &[(HEADER, "")][..]] {
        let (status, body) = answer(
            &preview,
            request("/admin/shutdown", loopback(), headers, ""),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}: {body}");
        assert_eq!(code_of(&body), "admin_token_required", "{headers:?}");
    }

    gateway_local::write_admin_token(tmp.path(), &format!("{ADMIN_TOKEN}\n"));
    let longer = format!("{ADMIN_TOKEN}x");
    let refusals: [&[(&str, &str)]; 6] = [
        // A bare request, as the CLI and `curl -X POST` send it.
        &[],
        // What a page on another origin can send without a preflight.
        &[("origin", "https://attacker.example")],
        // Not the token: empty, a prefix of it, an extension of it, and a
        // dashboard bearer in the header a pairing token travels in.
        &[(HEADER, "")],
        &[(HEADER, &ADMIN_TOKEN[..ADMIN_TOKEN.len() - 1])],
        &[(HEADER, &longer)],
        &[("authorization", "Bearer zc_admin_preview_test_token")],
    ];
    for headers in refusals {
        let (status, body) = answer(
            &preview,
            request("/admin/shutdown", loopback(), headers, ""),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}: {body}");
        assert_eq!(code_of(&body), "admin_token_required", "{headers:?}");
        assert!(
            !body.contains(ADMIN_TOKEN),
            "the refusal does not echo the token: {body}"
        );
    }
    assert!(!*stopped.borrow(), "no refused request stops the preview");

    // A browser that did add the header must preflight first, and the
    // preview grants no origin anything.
    let preflight = axum::http::Request::builder()
        .method("OPTIONS")
        .uri("/admin/shutdown")
        .header("origin", "https://attacker.example")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", HEADER)
        .extension(ConnectInfo(loopback()))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(preview.clone(), preflight)
        .await
        .unwrap();
    assert!(!response.status().is_success(), "{}", response.status());
    assert!(
        response
            .headers()
            .keys()
            .all(|name| !name.as_str().starts_with("access-control-")),
        "{:?}",
        response.headers()
    );
    assert!(!*stopped.borrow());

    // The file is read per request: once a gateway rotates it, the old
    // token stops nothing and the new one does. Surrounding whitespace in
    // the header is not part of the token.
    gateway_local::write_admin_token(tmp.path(), "zc_admin_rotated");
    let (status, body) = answer(
        &preview,
        request("/admin/shutdown", loopback(), &[(HEADER, ADMIN_TOKEN)], ""),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!*stopped.borrow());
    let (status, body) = answer(
        &preview,
        request(
            "/admin/shutdown",
            loopback(),
            &[(HEADER, " zc_admin_rotated ")],
            "",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(*stopped.borrow());

    // Started from a socket path, the preview knows no data directory: it
    // refuses every caller, the holder of a real token included, and says
    // how to stop it instead.
    let (socket_only, socket_only_stopped) = gateway_local::preview(tmp.path(), None);
    for headers in [&[][..], &[(HEADER, "zc_admin_rotated")][..]] {
        let (status, body) = answer(
            &socket_only,
            request("/admin/shutdown", loopback(), headers, ""),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}: {body}");
        assert_eq!(code_of(&body), "admin_token_unavailable", "{headers:?}");
        assert!(
            body.contains("SIGTERM") && body.contains("--data-dir"),
            "{body}"
        );
    }
    assert!(!*socket_only_stopped.borrow());
}

// ── Request limits ───────────────────────────────────────────────

/// Every route, served, refused or unknown, answers `413` to a body over the
/// in-process limit and `408` to a request not done within the timeout,
/// before any handler runs: the preview's own router, at its boundary.
#[tokio::test]
async fn every_route_sits_behind_the_in_process_request_limits() {
    use axum::body::{Body, Bytes};
    use axum::http::Request;
    use futures_util::StreamExt as _;
    use tower::ServiceExt as _;

    let tmp = tempfile::tempdir().unwrap();
    // No core listens here: no limit may need one.
    let endpoint = tmp.path().join("daemon.sock");
    let (stop, stopped) = watch::channel(false);
    let preview = router(
        CoreRpc::local(endpoint.clone(), EndpointOwner::SameAccount),
        endpoint,
        None,
        stop,
        None,
        Duration::from_millis(300),
    );
    let loopback = ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40_000)));

    let oversized = vec![b' '; crate::MAX_BODY_SIZE + 1];
    for path in [
        "/hooks/claude-code",
        "/admin/shutdown",
        "/api/sessions",
        "/webhook",
        "/api/no-such-route",
    ] {
        let request = Request::post(path)
            .header("content-type", "application/json")
            .header("content-length", oversized.len())
            .extension(loopback)
            .body(Body::from(oversized.clone()))
            .unwrap();
        let response = preview.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{path}");
    }
    assert!(
        !*stopped.borrow(),
        "an oversized shutdown request stops nothing"
    );

    // A declared body that never finishes arriving.
    let stalled = Body::from_stream(
        futures_util::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"{")) })
            .chain(futures_util::stream::pending()),
    );
    let request = Request::post("/hooks/claude-code")
        .header("content-type", "application/json")
        .header("content-length", "100")
        .extension(loopback)
        .body(stalled)
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(10), preview.clone().oneshot(request))
        .await
        .expect("a stalled request is answered within the timeout")
        .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
}

/// The serving note comes from the Fluent catalog: a missing key would
/// render as the `{key}` sentinel instead of the text.
#[test]
fn the_serving_notice_resolves_via_fluent() {
    let notice = serving_notice("http://127.0.0.1:42617", Path::new("/run/zeroclaw.sock"));
    assert!(!notice.starts_with('{'), "missing Fluent string: {notice}");
    assert!(notice.contains("http://127.0.0.1:42617"), "{notice}");
    assert!(notice.contains("/run/zeroclaw.sock"), "{notice}");
}

// ── The router, against a real core on a real socket ─────────────

#[cfg(unix)]
mod against_a_core {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt as _;

    const TOKEN: &str = "zc_preview_token";

    /// A core serving its real local listener on a socket under `dir`.
    struct Core {
        ctx: Arc<zeroclaw_runtime::rpc::context::RpcContext>,
        endpoint: PathBuf,
        cancel: CancellationToken,
        listener: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Core {
        async fn start(dir: &Path) -> Self {
            Self::serve(Self::context(dir)).await
        }

        fn context(dir: &Path) -> Arc<zeroclaw_runtime::rpc::context::RpcContext> {
            assert!(
                std::env::var_os("ZEROCLAW_SOCKET").is_none(),
                "ZEROCLAW_SOCKET must be unset for these tests"
            );
            let mut config = zeroclaw_config::schema::Config {
                data_dir: dir.to_path_buf(),
                config_path: dir.join("config.toml"),
                ..Default::default()
            };
            config.gateway.paired_tokens = vec![TOKEN.into()];
            let sessions = Arc::new(zeroclaw_runtime::rpc::session::SessionStore::new(
                16,
                Arc::new(zeroclaw_infra::session_queue::SessionActorQueue::new(
                    4, 10, 60,
                )),
            ));
            zeroclaw_runtime::rpc::context::RpcContext::for_live_test(config, sessions)
        }

        async fn serve(ctx: Arc<zeroclaw_runtime::rpc::context::RpcContext>) -> Self {
            let endpoint = zeroclaw_runtime::rpc::local::socket_path(&ctx.config.read());
            let cancel = CancellationToken::new();
            let listener = {
                let ctx = Arc::clone(&ctx);
                let cancel = cancel.clone();
                zeroclaw_spawn::spawn!(async move {
                    zeroclaw_runtime::rpc::local::run_local_listener(
                        ctx,
                        cancel,
                        Arc::new(AtomicUsize::new(0)),
                        None,
                    )
                    .await
                })
            };
            for _ in 0..250 {
                if tokio::net::UnixStream::connect(&endpoint).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Self {
                ctx,
                endpoint,
                cancel,
                listener,
            }
        }

        /// Stop the listener and hand back the context for a restart.
        async fn stop(self) -> Arc<zeroclaw_runtime::rpc::context::RpcContext> {
            self.cancel.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(10), self.listener).await;
            self.ctx
        }
    }

    /// A dashboard build laid out as `vite build` writes it: the page at the
    /// root, assets under `assets/`, all referenced as `/_app/...`.
    fn web_dist(dir: &Path) -> PathBuf {
        let dist = dir.join("dist");
        std::fs::create_dir_all(dist.join("assets")).unwrap();
        std::fs::write(
            dist.join("index.html"),
            r#"<html>preview dashboard<script src="/_app/assets/app.js"></script></html>"#,
        )
        .unwrap();
        std::fs::write(dist.join("assets").join("app.js"), "console.log(1)").unwrap();
        dist
    }

    async fn get(router: &Router, path: &str, token: Option<&str>) -> (StatusCode, String) {
        send(router, "GET", path, token).await
    }

    async fn send(
        router: &Router,
        method: &str,
        path: &str,
        token: Option<&str>,
    ) -> (StatusCode, String) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn json_of(body: &str) -> serde_json::Value {
        serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
    }

    async fn headers_of(router: &Router, path: &str) -> axum::http::HeaderMap {
        router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .headers()
            .clone()
    }

    /// A listener that is not a core: it records what each connection
    /// writes, up to the first newline or EOF, then closes it.
    struct Recorder {
        endpoint: PathBuf,
        seen: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Recorder {
        fn bind(dir: &Path) -> Self {
            use tokio::io::AsyncBufReadExt as _;
            let endpoint = dir.join("daemon.sock");
            let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let task = {
                let seen = Arc::clone(&seen);
                zeroclaw_spawn::spawn!(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let mut reader = tokio::io::BufReader::new(stream);
                        let mut bytes = Vec::new();
                        let _ = tokio::time::timeout(
                            Duration::from_secs(5),
                            reader.read_until(b'\n', &mut bytes),
                        )
                        .await;
                        seen.lock().unwrap().push(bytes);
                    }
                })
            };
            Self {
                endpoint,
                seen,
                task,
            }
        }

        fn connections(&self) -> Vec<Vec<u8>> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for Recorder {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn wait_for_connections(recorder: &Recorder, count: usize) {
        for _ in 0..250 {
            if recorder.connections().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "expected {count} connections, saw {}",
            recorder.connections().len()
        );
    }

    #[tokio::test]
    async fn the_preview_serves_through_the_core_and_refuses_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let core = Core::start(tmp.path()).await;
        let router = router(
            CoreRpc::local(core.endpoint.clone(), EndpointOwner::SameAccount),
            core.endpoint.clone(),
            Some(web_dist(tmp.path())),
            watch::channel(false).0,
            None,
            Duration::from_secs(crate::REQUEST_TIMEOUT_SECS),
        );

        let (status, body) = get(&router, "/health", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(json_of(&body)["core"]["link"], "reachable");

        // The core-link diagnostic needs the caller's own credential.
        let (status, body) = get(&router, CORE_LINK_PATH, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(json_of(&body)["code"], "auth_required");
        assert!(json_of(&body)["hint"].is_string(), "{body}");
        let (status, body) = get(&router, CORE_LINK_PATH, Some("zc_not_paired")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, body) = get(&router, CORE_LINK_PATH, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let link = json_of(&body);
        assert_eq!(link["principal_id"], "shared-operator");
        assert_eq!(link["core"]["server_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(link["gateway"]["protocol_version"], RPC_PROTOCOL_VERSION);

        // A dashboard route not served yet: the credential first, then a
        // refusal that names the route.
        let (status, body) = get(&router, "/api/cron", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, body) = get(&router, "/api/cron", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let refused = json_of(&body);
        assert_eq!(refused["code"], "capability_missing");
        assert_eq!(refused["route"], "GET /api/cron");
        assert_eq!(refused["deferred"], false);
        let (_, body) = get(&router, "/api/sessions/abc/state", Some(TOKEN)).await;
        assert_eq!(json_of(&body)["route"], "GET /api/sessions/{id}/state");

        // A deferred route carries no bearer by nature and is refused as is.
        for (method, path, route) in [
            ("POST", "/webhook", "POST /webhook"),
            ("POST", "/pair", "POST /pair"),
            ("GET", "/acp", "GET /acp"),
            ("GET", "/oidc/callback", "GET /oidc/callback"),
            ("POST", "/plugin/inbox", "POST /plugin/{path}"),
        ] {
            let (status, body) = send(&router, method, path, None).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {body}");
            let refused = json_of(&body);
            assert_eq!(refused["code"], "capability_missing", "{path}");
            assert_eq!(refused["route"], route);
            assert_eq!(refused["deferred"], true);
        }

        // Nothing under an API prefix falls through to the dashboard page.
        for path in [
            "/api/no-such-route",
            "/ws/other",
            "/admin/other",
            "/api",
            "/ws",
            "/acp/",
            "/pair/",
        ] {
            let (status, body) = get(&router, path, Some(TOKEN)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
            assert_eq!(json_of(&body)["code"], "not_found", "{path}");
        }
        // Dashboard pages and assets are served from the build.
        let (status, body) = get(&router, "/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("preview dashboard"), "{body}");
        // Assets resolve where the page's own references point.
        let (status, body) = get(&router, "/_app/assets/app.js", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "console.log(1)");
        let (status, _) = get(&router, "/api/openapi.json", None).await;
        assert_eq!(status, StatusCode::OK);

        // Core down: the health check and core-backed routes say so, with a
        // hint the dashboard can show; a bad credential is still a 401.
        let ctx = core.stop().await;
        let (status, body) = get(&router, "/health", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let health = json_of(&body);
        assert_eq!(health["code"], "core_unavailable");
        assert_eq!(health["core"]["link"], "unreachable");
        let (status, body) = get(&router, CORE_LINK_PATH, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(json_of(&body)["code"], "core_unavailable");
        assert!(
            json_of(&body)["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("zeroclaw daemon")),
            "{body}"
        );
        let (status, _) = get(&router, CORE_LINK_PATH, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // The core comes back on the same endpoint: the next request
        // reconnects with the caller's credential.
        let core = Core::serve(ctx).await;
        let (status, body) = get(&router, "/health", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = get(&router, CORE_LINK_PATH, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        core.stop().await;
    }

    /// The in-process gateway's body for `path` when the request reaches the
    /// core through `core`.
    async fn in_process_body(state: &crate::AppState, core: &CoreRpc, path: &str) -> String {
        use crate::api::{
            handle_api_cost, handle_api_health, handle_api_sessions_list, handle_api_tuis,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().unwrap(),
        );
        let access = core
            .access(&headers)
            .await
            .expect("served through the core");
        assert!(matches!(access, CoreAccess::Core(_)));
        let state = State(state.clone());
        let response = match path {
            "/api/health" => handle_api_health(state, headers, access)
                .await
                .into_response(),
            "/api/tuis" => handle_api_tuis(state, headers, access)
                .await
                .into_response(),
            "/api/cost" => handle_api_cost(state, headers, Query(CostQuery::default()), access)
                .await
                .into_response(),
            "/api/cost?agent=main" => {
                let query = CostQuery {
                    agent: Some("main".into()),
                    ..CostQuery::default()
                };
                handle_api_cost(state, headers, Query(query), access)
                    .await
                    .into_response()
            }
            "/api/events/history" => crate::sse::handle_events_history(state, headers, access)
                .await
                .into_response(),
            // Served from the gateway's own store even with a core attached.
            "/api/sessions" => handle_api_sessions_list(state, headers)
                .await
                .into_response(),
            other => panic!("no in-process handler for {other}"),
        };
        let body = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&body).into_owned()
    }

    /// Each ported dashboard route answers through the separate gateway
    /// exactly what the in-process gateway answers when the same request
    /// reaches the same core, and lists only terminals as terminals.
    #[tokio::test]
    async fn the_ported_routes_answer_as_the_in_process_gateway_does() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = Core::context(tmp.path());
        let config = ctx.config.read().clone();
        let backend = zeroclaw_infra::make_session_backend(
            &config.data_dir,
            &config.channels.session_backend,
        )
        .expect("open the session store");
        backend
            .append("gw_alpha", &zeroclaw_providers::ChatMessage::user("hi"))
            .unwrap();
        backend.set_session_agent_alias("gw_alpha", "main").unwrap();
        let history = Arc::new(crate::sse::EventBuffer::new(8));
        history.push(json!({ "type": "agent_start", "provider": "test" }));
        {
            let ctx = Arc::get_mut(&mut ctx).expect("a fresh context has one owner");
            ctx.session_backend = Some(Arc::clone(&backend));
            ctx.event_history = Some(history);
        }
        let core = Core::serve(ctx).await;
        let rpc = CoreRpc::local(core.endpoint.clone(), EndpointOwner::SameAccount);
        let preview = router(
            rpc.clone(),
            core.endpoint.clone(),
            None,
            watch::channel(false).0,
            None,
            Duration::from_secs(crate::REQUEST_TIMEOUT_SECS),
        );
        let state = crate::api::tests::test_state_with_session_backend(config, backend);

        // A terminal attached to the core next to the gateway.
        let terminal = RpcClient::connect_local(
            &core.endpoint,
            zeroclaw_rpc_client::ConnectOptions {
                auth_token: Some(TOKEN.into()),
                ..Default::default()
            },
        )
        .await
        .expect("a terminal connects");
        let terminal_id = terminal
            .handshake()
            .tui_id
            .clone()
            .expect("the core names the terminal");

        for path in [
            "/api/health",
            "/api/tuis",
            "/api/cost",
            "/api/cost?agent=main",
            "/api/events/history",
            "/api/sessions",
        ] {
            let (status, body) = get(&preview, path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: {body}");
            assert!(json_of(&body)["hint"].is_string(), "{path}: {body}");

            // The health snapshot is process-wide and its `updated_at` is the
            // moment it was taken, so it is compared without that, and again
            // if a parallel test touched a component in between.
            let comparable = |body: &str| {
                let mut body = json_of(body);
                if let Some(health) = body.get_mut("health").and_then(|h| h.as_object_mut()) {
                    health.remove("updated_at");
                }
                body
            };
            for attempt in 0.. {
                let (status, served) = get(&preview, path, Some(TOKEN)).await;
                assert_eq!(status, StatusCode::OK, "{path}: {served}");
                let in_process = in_process_body(&state, &rpc, path).await;
                if comparable(&served) == comparable(&in_process) {
                    break;
                }
                assert!(
                    attempt < 20,
                    "{path}: the preview answered {served}, the in-process gateway {in_process}"
                );
            }
        }

        let (_, body) = get(&preview, "/api/tuis", Some(TOKEN)).await;
        let listed: Vec<String> = json_of(&body)["tuis"]
            .as_array()
            .expect("tuis")
            .iter()
            .map(|tui| tui["tui_id"].as_str().expect("an id").to_owned())
            .collect();
        assert_eq!(listed, [terminal_id], "{body}");
        assert!(
            core.ctx
                .tui_registry
                .list()
                .iter()
                .any(|tui| tui.client_kind.as_deref() == Some("gateway")),
            "the core registered the gateway's connection as a gateway's"
        );
        let (_, body) = get(&preview, "/api/sessions", Some(TOKEN)).await;
        assert_eq!(
            json_of(&body)["sessions"][0]["session_id"],
            "alpha",
            "{body}"
        );
        let (_, body) = get(&preview, "/api/events/history", Some(TOKEN)).await;
        assert_eq!(json_of(&body)["events"][0]["type"], "agent_start", "{body}");

        drop(terminal);
        core.stop().await;
    }

    #[tokio::test]
    async fn health_tells_the_dashboard_to_sign_in_and_names_no_path() {
        let tmp = tempfile::tempdir().unwrap();
        let core = Core::start(tmp.path()).await;
        let router = router(
            CoreRpc::local(core.endpoint.clone(), EndpointOwner::SameAccount),
            core.endpoint.clone(),
            None,
            watch::channel(false).0,
            None,
            Duration::from_secs(crate::REQUEST_TIMEOUT_SECS),
        );
        let private = tmp.path().display().to_string();
        let expected_sign_in = serde_json::json!({
            "pairing_code": false, "bearer": true, "verify": CORE_LINK_PATH,
        });

        let (status, body) = get(&router, "/health", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let health = json_of(&body);
        assert_eq!(
            health["require_pairing"], true,
            "a dashboard must never read a missing field as pairing off"
        );
        assert_eq!(health["sign_in"], expected_sign_in);
        assert!(!body.contains(&private), "no path in public health: {body}");

        let headers = headers_of(&router, "/health").await;
        assert_eq!(headers["x-frame-options"], "DENY");
        assert!(headers.contains_key("content-security-policy"));

        let ctx = core.stop().await;
        let (status, body) = get(&router, "/health", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let health = json_of(&body);
        assert_eq!(health["require_pairing"], true);
        assert_eq!(health["sign_in"], expected_sign_in);
        assert_eq!(health["code"], "core_unavailable");
        assert!(health["hint"].is_string(), "{body}");
        assert!(!body.contains(&private), "no path in public health: {body}");
        drop(ctx);
    }

    /// The requests a fresh browser makes, in order: no stored token, then
    /// a token the core refuses, then one it accepts.
    #[tokio::test]
    async fn a_fresh_browser_signs_in_with_an_existing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let core = Core::start(tmp.path()).await;
        let router = router(
            CoreRpc::local(core.endpoint.clone(), EndpointOwner::SameAccount),
            core.endpoint.clone(),
            Some(web_dist(tmp.path())),
            watch::channel(false).0,
            None,
            Duration::from_secs(crate::REQUEST_TIMEOUT_SECS),
        );

        // The page loads; health says to sign in with a token, and where to
        // check it.
        let (status, _) = get(&router, "/", None).await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = get(&router, "/health", None).await;
        let health = json_of(&body);
        assert_eq!(health["require_pairing"], true);
        assert_eq!(health["sign_in"]["bearer"], true);
        assert_eq!(health["sign_in"]["pairing_code"], false);
        let verify = health["sign_in"]["verify"].as_str().unwrap().to_owned();

        // A token the core refuses: 401, so the sign-in screen stays.
        let (status, body) = get(&router, &verify, Some("zc_not_paired")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(json_of(&body)["code"], "auth_required");

        // The token the core accepts: data.
        let (status, body) = get(&router, &verify, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(json_of(&body)["principal_id"], "shared-operator");
        core.stop().await;
    }

    #[tokio::test]
    async fn the_health_probe_sends_nothing_and_its_connection_serves_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::bind(tmp.path());
        let router = router(
            CoreRpc::local(recorder.endpoint.clone(), EndpointOwner::SameAccount),
            recorder.endpoint.clone(),
            None,
            watch::channel(false).0,
            None,
            Duration::from_secs(crate::REQUEST_TIMEOUT_SECS),
        );

        let (status, body) = get(&router, "/health", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(json_of(&body)["core"]["link"], "reachable");
        wait_for_connections(&recorder, 1).await;
        assert_eq!(
            recorder.connections()[0],
            Vec::<u8>::new(),
            "the probe writes nothing, not even a tokenless initialize"
        );

        // A domain call dials its own connection and presents its own
        // credential; the probe's connection is already gone.
        let (status, _) = get(&router, CORE_LINK_PATH, Some(TOKEN)).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "the recorder is no core"
        );
        wait_for_connections(&recorder, 2).await;
        let connections = recorder.connections();
        assert_eq!(connections.len(), 2);
        let frame: serde_json::Value = serde_json::from_slice(&connections[1]).unwrap();
        assert_eq!(frame["method"], "initialize");
        assert_eq!(frame["params"]["auth_token"], TOKEN);
    }

    #[tokio::test]
    async fn an_endpoint_that_fails_the_account_check_is_untrusted_and_hears_nothing() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let recorder = Recorder::bind(&shared);
        let router = router(
            CoreRpc::local(recorder.endpoint.clone(), EndpointOwner::SameAccount),
            recorder.endpoint.clone(),
            None,
            watch::channel(false).0,
            None,
            Duration::from_secs(crate::REQUEST_TIMEOUT_SECS),
        );

        let (status, body) = get(&router, "/health", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let health = json_of(&body);
        assert_eq!(health["code"], "core_untrusted_endpoint");
        assert_eq!(health["core"]["link"], "untrusted");
        assert!(!body.contains(&shared.display().to_string()), "{body}");

        let (status, body) = get(&router, CORE_LINK_PATH, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(json_of(&body)["code"], "core_untrusted_endpoint");
        assert!(json_of(&body)["hint"].is_string(), "{body}");

        wait_for_connections(&recorder, 2).await;
        assert!(
            recorder.connections().iter().all(Vec::is_empty),
            "nothing reaches an endpoint that fails the check"
        );
    }

    #[tokio::test]
    async fn no_dashboard_build_means_no_page_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let core = Core::start(tmp.path()).await;
        let router = router(
            CoreRpc::local(core.endpoint.clone(), EndpointOwner::SameAccount),
            core.endpoint.clone(),
            None,
            watch::channel(false).0,
            None,
            Duration::from_secs(crate::REQUEST_TIMEOUT_SECS),
        );
        let (status, body) = get(&router, "/sessions", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(json_of(&body)["code"], "dashboard_unavailable");
        core.stop().await;
    }
}
