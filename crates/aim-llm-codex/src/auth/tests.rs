//! Offline auth tests against a local fake issuer, plus the maintainer-run `interactive_` logins.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::fake::{self, FakeServer, MemoryStore, Recorded, Reply, jwt, unix_now};

fn tokens(exp: u64, refresh: &str) -> Value {
    json!({"access_token": jwt(exp, "acct-fixture"), "refresh_token": refresh, "id_token": jwt(exp, "acct-fixture")})
}

#[test]
fn jwt_claims_and_redacted_debug() {
    let claims = jwt_claims(&jwt(4_102_444_800, "acct-secret")).unwrap();
    assert_eq!(claims.exp, 4_102_444_800);
    assert_eq!(claims.account_id.as_deref(), Some("acct-secret"));
    assert_eq!(claims.plan_type.as_deref(), Some("pro"));
    assert!(jwt_claims("bad").is_err());
    assert!(jwt_claims("a.b.c").is_err());
    let debug = format!("{claims:?} {:?}", fake::credentials(4_102_444_800, Some("refresh-secret")));
    assert!(
        !debug.contains("acct-secret") && !debug.contains("acct-fixture") && !debug.contains("refresh-secret") && !debug.contains("e30."),
        "{debug}"
    );
    assert!(debug.contains("expires_at: 4102444800"));
}

#[tokio::test]
async fn borrowed_credentials_are_never_refreshed_or_written() {
    let issuer = FakeServer::start(|_, _| Reply::json(200, &tokens(unix_now() + 3_600, "rotated"))).await;
    let dir = fake::temp_dir("borrowed");
    let borrowed = dir.join("codex-home/auth.json");
    fs::create_dir_all(borrowed.parent().unwrap()).unwrap();
    let store_path = dir.join("aim/auth/codex.json");
    let config = issuer.config();
    for exp in [unix_now() - 10, unix_now() + 60] {
        // Expired, and expiring within the 5-minute margin: both refused, neither refreshed.
        let file = json!({"auth_mode":"chatgpt","tokens":{"access_token":jwt(exp, "acct-fixture"),"refresh_token":"borrowed-refresh","account_id":"acct-fixture"},"last_refresh":"2026-09-22T00:00:00Z"});
        fs::write(&borrowed, serde_json::to_vec_pretty(&file).unwrap()).unwrap();
        let before = (fs::read(&borrowed).unwrap(), fs::metadata(&borrowed).unwrap().modified().unwrap());
        let manager = AuthManager::with_config(fake::client(&config), Arc::new(FileCredentialStore::new(store_path.clone())), &config)
            .with_borrowed_path(borrowed.clone());
        let error = manager.credentials().await.unwrap_err();
        assert_eq!(error.kind, LlmErrorKind::Auth);
        assert!(error.message.contains("aim login codex"), "{}", error.message);
        assert_eq!(issuer.requests().await.len(), 0, "no call to the token endpoint");
        assert_eq!(
            (fs::read(&borrowed).unwrap(), fs::metadata(&borrowed).unwrap().modified().unwrap()),
            before,
            "byte-identical, untouched"
        );
        assert!(!store_path.exists() && !dir.join("aim").exists(), "nothing written to aim's store either");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn borrowed_fresh_credentials_are_used_as_is() {
    let dir = fake::temp_dir("borrowed-fresh");
    let borrowed = dir.join("auth.json");
    let exp = unix_now() + 7 * 86_400;
    fs::write(&borrowed, json!({"tokens":{"access_token":jwt(exp, "acct-fixture"),"refresh_token":"borrowed-refresh"}}).to_string())
        .unwrap();
    let config = CodexConfig { issuer: "http://127.0.0.1:9".into(), ..CodexConfig::default() };
    let manager = AuthManager::with_config(fake::client(&config), MemoryStore::new(None), &config).with_borrowed_path(borrowed.clone());
    let credentials = manager.credentials().await.unwrap();
    assert_eq!((credentials.account_id.as_str(), credentials.expires_at), ("acct-fixture", exp));
    assert!(credentials.refresh_token.is_none(), "the borrowed refresh token is never even loaded");
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn owned_refresh_is_single_flight_across_managers_and_rotates() {
    let issuer = FakeServer::start(|_, _| Reply::Steps {
        headers: vec![("content-type".into(), "application/json".into())],
        steps: vec![(Duration::from_millis(200), tokens(unix_now() + 3_600, "rt-2").to_string().into_bytes())],
        hang: false,
    })
    .await;
    let dir = fake::temp_dir("refresh");
    let path = dir.join("auth/codex.json");
    FileCredentialStore::new(path.clone()).save(fake::credentials(unix_now() + 10, Some("rt-1"))).await.unwrap();
    let config = issuer.config();
    // Two managers on one store stand in for two processes; each serves four concurrent calls.
    let managers: Vec<Arc<AuthManager>> = (0..2)
        .map(|_| Arc::new(AuthManager::with_config(fake::client(&config), Arc::new(FileCredentialStore::new(path.clone())), &config)))
        .collect();
    let calls =
        managers.iter().flat_map(|m| (0..4).map(move |_| Arc::clone(m))).map(|m| tokio::spawn(async move { m.credentials().await }));
    let results = futures_util::future::join_all(calls).await;
    let tokens: Vec<String> = results.into_iter().map(|r| r.unwrap().unwrap().access_token.expose().to_owned()).collect();
    assert!(tokens.windows(2).all(|w| w[0] == w[1]), "everyone got the one refreshed token");
    let requests = issuer.requests().await;
    assert_eq!(requests.len(), 1, "exactly one refresh");
    assert_eq!((requests[0].method.as_str(), requests[0].path()), ("POST", "/oauth/token"));
    assert_eq!(requests[0].json(), json!({"grant_type":"refresh_token","client_id":CLIENT_ID,"refresh_token":"rt-1"}));
    let stored = FileCredentialStore::new(path).load().await.unwrap().unwrap();
    assert_eq!(stored.refresh_token.unwrap().expose(), "rt-2", "rotated");
    assert!(stored.expires_at > unix_now() + 3_000);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn refresh_failures_are_classified() {
    let cases: Vec<(Reply, LlmErrorKind, &str)> = vec![
        (Reply::json(500, &json!({})), LlmErrorKind::Unavailable, "HTTP 500"),
        (Reply::json(400, &json!({"error":"invalid_grant"})), LlmErrorKind::Auth, "[invalid_grant]; run `aim login codex`"),
        (Reply::json(401, &json!({"error":{"code":"refresh_token_reused"}})), LlmErrorKind::Auth, "[refresh_token_reused]"),
        (Reply::json(200, &json!({"refresh_token":"rt-2"})), LlmErrorKind::Auth, "returned no access token"),
    ];
    for (reply, kind, text) in cases {
        let reply = std::sync::Mutex::new(Some(reply));
        let issuer = FakeServer::start(move |_, _| reply.lock().unwrap().take().unwrap_or(Reply::Silent)).await;
        let config = issuer.config();
        let manager =
            AuthManager::with_config(fake::client(&config), MemoryStore::new(Some(fake::credentials(unix_now(), Some("rt-1")))), &config);
        let error = manager.credentials().await.unwrap_err();
        assert_eq!(error.kind, kind, "{}", error.message);
        assert!(error.message.contains(text), "{}", error.message);
    }
    // An unreachable issuer is a transport failure, not "log in again".
    let config = CodexConfig { issuer: "http://127.0.0.1:9".into(), request_timeout: Duration::from_secs(2), ..CodexConfig::default() };
    let manager =
        AuthManager::with_config(fake::client(&config), MemoryStore::new(Some(fake::credentials(unix_now(), Some("rt-1")))), &config);
    let error = manager.credentials().await.unwrap_err();
    assert_eq!(error.kind, LlmErrorKind::Transport, "{}", error.message);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn refreshed_credentials_survive_a_failed_save() {
    let issuer = FakeServer::start(|_, _| Reply::json(200, &tokens(unix_now() + 3_600, "rt-2"))).await;
    let config = issuer.config();
    let store = Arc::new(MemoryStore {
        credentials: Mutex::new(Some(fake::credentials(unix_now(), Some("rt-1")))),
        fail_saves: true,
        loads: std::sync::atomic::AtomicUsize::new(0),
    });
    let manager = AuthManager::with_config(fake::client(&config), Arc::clone(&store) as Arc<dyn CredentialStore>, &config);
    let first = manager.credentials().await.unwrap();
    assert_eq!(first.refresh_token.as_ref().unwrap().expose(), "rt-2");
    let loads = store.loads.load(Ordering::SeqCst);
    let second = manager.credentials().await.unwrap();
    assert_eq!(second.access_token, first.access_token);
    assert_eq!(store.loads.load(Ordering::SeqCst), loads, "served from memory");
    assert_eq!(issuer.requests().await.len(), 1, "the spent refresh token is not reused");
}

#[tokio::test]
async fn credentials_are_cached_and_invalidated() {
    let store = MemoryStore::new(Some(fake::credentials(unix_now() + 3_600, Some("rt-1"))));
    let config = CodexConfig::default();
    let manager = AuthManager::with_config(fake::client(&config), Arc::clone(&store) as Arc<dyn CredentialStore>, &config);
    manager.credentials().await.unwrap();
    manager.credentials().await.unwrap();
    assert_eq!(store.loads.load(Ordering::SeqCst), 1, "the second call does not touch the store");
    manager.invalidate().await;
    manager.credentials().await.unwrap();
    assert_eq!(store.loads.load(Ordering::SeqCst), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn file_store_is_private_and_atomic() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = fake::temp_dir("store");
    let auth_dir = dir.join("aim/auth");
    // An older aim may have created the directory with the umask default.
    fs::create_dir_all(&auth_dir).unwrap();
    fs::set_permissions(&auth_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let path = auth_dir.join("codex.json");
    let store = FileCredentialStore::new(path.clone());
    assert!(store.load().await.unwrap().is_none());
    let credentials = fake::credentials(4_102_444_800, Some("rt"));
    store.save(credentials.clone()).await.unwrap();
    store.save(credentials.clone()).await.unwrap();
    let loaded = store.load().await.unwrap().unwrap();
    assert_eq!(loaded.access_token, credentials.access_token);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&auth_dir).unwrap().permissions().mode() & 0o777, 0o700);
    let leftovers: Vec<_> =
        fs::read_dir(&auth_dir).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
    assert!(leftovers.is_empty());
    // The stored format is plain JSON with the tokens as strings.
    let raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["refresh_token"], "rt");
    // The lease is exclusive while held.
    let lease = store.lease().await.unwrap();
    let lock = File::open(auth_dir.join("codex.json.lock")).unwrap();
    assert!(lock.try_lock().is_err());
    drop(lease);
    assert!(lock.try_lock().is_ok());
    fs::remove_dir_all(dir).unwrap();
}

/// Sends raw bytes to the callback server and returns the response text (empty if none).
async fn raw_request(port: u16, request: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    drop(tokio::time::timeout(Duration::from_secs(10), socket.read_to_string(&mut response)).await);
    response
}

async fn get(port: u16, target: &str) -> String {
    raw_request(port, &format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")).await
}

fn state_of(url: &str) -> String {
    url::Url::parse(url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned()
}

async fn issuer_with(reply: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> FakeServer {
    FakeServer::start(move |request, _| reply(request)).await
}

#[tokio::test]
async fn browser_login_url_carries_the_pkce_parameters() {
    let config = CodexConfig { callback_ports: vec![0], ..CodexConfig::default() };
    let challenge = begin_browser_login(&config).await.unwrap();
    let url = url::Url::parse(&challenge.url).unwrap();
    assert_eq!((url.scheme(), url.host_str(), url.path()), ("https", Some("auth.openai.com"), "/oauth/authorize"));
    let params: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    let port = challenge.port().unwrap();
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], CLIENT_ID);
    assert_eq!(params["redirect_uri"], format!("http://127.0.0.1:{port}/auth/callback"));
    assert_eq!(params["scope"], SCOPE);
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["code_challenge"].len(), 43, "base64url SHA-256");
    assert_eq!(params["state"].len(), 43);
    assert_eq!(params["id_token_add_organizations"], "true");
    assert_eq!(params["codex_cli_simplified_flow"], "true");
    assert_eq!(params["originator"], "aim");
}

#[tokio::test]
async fn browser_login_survives_stray_requests_and_confirms_after_the_exchange() {
    let issuer = issuer_with(|_| Reply::json(200, &tokens(unix_now() + 3_600, "rt-new"))).await;
    let config = issuer.config();
    let challenge = begin_browser_login(&config).await.unwrap();
    let (port, state, url) = (challenge.port().unwrap(), state_of(&challenge.url), challenge.url.clone());
    let login = tokio::spawn({
        let client = fake::client(&config);
        async move { finish_browser_login(&client, challenge).await }
    });
    // A speculative preconnect that sends nothing and closes, and one that stays silent.
    drop(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
    let _silent = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert!(get(port, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
    let mismatch = get(port, "/auth/callback?code=stolen&state=wrong").await;
    assert!(mismatch.starts_with("HTTP/1.1 400") && mismatch.contains("State mismatch"), "{mismatch}");
    assert!(!login.is_finished(), "a wrong state never ends the login");
    let done = get(port, &format!("/auth/callback?code=good-code&state={state}")).await;
    assert!(done.starts_with("HTTP/1.1 200") && done.contains("Signed in"), "{done}");
    let credentials = login.await.unwrap().unwrap();
    assert_eq!(credentials.refresh_token.unwrap().expose(), "rt-new");
    let requests = issuer.requests().await;
    assert_eq!(requests.len(), 1, "only the valid callback was exchanged");
    let exchange = &requests[0];
    assert_eq!(exchange.path(), "/oauth/token");
    assert_eq!(exchange.header("content-type"), Some("application/x-www-form-urlencoded"));
    assert_eq!(exchange.form("grant_type").as_deref(), Some("authorization_code"));
    assert_eq!(exchange.form("code").as_deref(), Some("good-code"));
    assert_eq!(exchange.form("client_id").as_deref(), Some(CLIENT_ID));
    assert_eq!(exchange.form("redirect_uri"), Some(format!("http://127.0.0.1:{port}/auth/callback")));
    // PKCE: the exchanged verifier hashes to the challenge in the authorization URL.
    let verifier = exchange.form("code_verifier").unwrap();
    let challenge_param = url::Url::parse(&url).unwrap().query_pairs().find(|(k, _)| k == "code_challenge").unwrap().1.into_owned();
    assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())), challenge_param);
}

#[tokio::test]
async fn browser_login_accepts_the_onboarding_state_suffix() {
    let issuer = issuer_with(|_| Reply::json(200, &tokens(unix_now() + 3_600, "rt"))).await;
    let config = issuer.config();
    let challenge = begin_browser_login(&config).await.unwrap();
    let (port, state) = (challenge.port().unwrap(), state_of(&challenge.url));
    let client = fake::client(&config);
    let login = tokio::spawn(async move { finish_browser_login(&client, challenge).await });
    let suffixed = url::form_urlencoded::byte_serialize(format!("{state}{STATE_SUFFIX}").as_bytes()).collect::<String>();
    assert!(get(port, &format!("/auth/callback?code=c&state={suffixed}")).await.starts_with("HTTP/1.1 200"));
    assert!(login.await.unwrap().is_ok());
}

#[tokio::test]
async fn browser_login_reports_provider_errors_distinctly() {
    let issuer = issuer_with(|_| Reply::json(200, &json!({}))).await;
    let config = issuer.config();
    let challenge = begin_browser_login(&config).await.unwrap();
    let (port, state) = (challenge.port().unwrap(), state_of(&challenge.url));
    let client = fake::client(&config);
    let login = tokio::spawn(async move { finish_browser_login(&client, challenge).await });
    let page = get(port, &format!("/auth/callback?error=access_denied&error_description=The+user+said+%3Cno%3E&state={state}")).await;
    assert!(page.contains("Sign-in failed: access_denied") && page.contains("&lt;no&gt;"), "{page}");
    let error = login.await.unwrap().unwrap_err();
    assert_eq!(error.kind, LlmErrorKind::Auth);
    assert_eq!(error.message, "browser login failed: the provider returned access_denied: The user said <no>");
    assert!(issuer.requests().await.is_empty(), "nothing exchanged");
}

#[tokio::test]
async fn browser_login_shows_failure_when_the_exchange_fails() {
    let issuer = issuer_with(|_| Reply::json(400, &json!({"error":"invalid_grant"}))).await;
    let config = issuer.config();
    let challenge = begin_browser_login(&config).await.unwrap();
    let (port, state) = (challenge.port().unwrap(), state_of(&challenge.url));
    let client = fake::client(&config);
    let login = tokio::spawn(async move { finish_browser_login(&client, challenge).await });
    let page = get(port, &format!("/auth/callback?code=c&state={state}")).await;
    assert!(page.starts_with("HTTP/1.1 500") && page.contains("Sign-in failed") && !page.contains("Signed in"), "{page}");
    let error = login.await.unwrap().unwrap_err();
    assert_eq!(error.kind, LlmErrorKind::Auth);
    assert!(error.message.contains("[invalid_grant]"), "{}", error.message);
}

#[tokio::test]
async fn browser_login_cancel_missing_code_and_timeout() {
    let config = CodexConfig { callback_ports: vec![0], login_timeout: Duration::from_secs(20), ..CodexConfig::default() };
    let client = fake::client(&config);
    let challenge = begin_browser_login(&config).await.unwrap();
    let port = challenge.port().unwrap();
    let login = tokio::spawn({
        let client = client.clone();
        async move { finish_browser_login(&client, challenge).await }
    });
    assert!(get(port, "/cancel").await.contains("Sign-in cancelled"));
    assert_eq!(login.await.unwrap().unwrap_err().message, "browser login cancelled");

    let challenge = begin_browser_login(&config).await.unwrap();
    let (port, state) = (challenge.port().unwrap(), state_of(&challenge.url));
    let login = tokio::spawn({
        let client = client.clone();
        async move { finish_browser_login(&client, challenge).await }
    });
    assert!(get(port, &format!("/auth/callback?state={state}")).await.starts_with("HTTP/1.1 400"));
    assert!(login.await.unwrap().unwrap_err().message.contains("no authorization code"));

    let quick = CodexConfig { login_timeout: Duration::from_millis(200), ..config };
    let challenge = begin_browser_login(&quick).await.unwrap();
    assert_eq!(finish_browser_login(&client, challenge).await.unwrap_err().message, "browser login timed out");
}

#[tokio::test]
async fn device_login_polls_until_approved() {
    let issuer = issuer_with(|request| match request.path() {
        // `usercode` is the alias codex also accepts; the interval arrives as a string.
        "/api/accounts/deviceauth/usercode" => Reply::json(200, &json!({"device_auth_id":"dev-1","usercode":"ABCD-1234","interval":"1"})),
        "/api/accounts/deviceauth/token" if request.json()["device_auth_id"] == "dev-1" => {
            Reply::json(200, &json!({"authorization_code":"auth-code","code_challenge":"x","code_verifier":"device-verifier"}))
        }
        "/oauth/token" => Reply::json(200, &tokens(unix_now() + 3_600, "rt-device")),
        _ => Reply::json(404, &json!({})),
    })
    .await;
    let config = issuer.config();
    let client = fake::client(&config);
    let challenge = begin_device_login(&client, &config).await.unwrap();
    assert_eq!(challenge.user_code, "ABCD-1234");
    assert_eq!(challenge.verification_url, format!("{}/codex/device", issuer.url));
    let credentials = finish_device_login(&client, &challenge).await.unwrap();
    assert_eq!(credentials.refresh_token.unwrap().expose(), "rt-device");
    let requests = issuer.requests().await;
    let poll = requests.iter().find(|r| r.path() == "/api/accounts/deviceauth/token").unwrap();
    assert_eq!(poll.json(), json!({"device_auth_id":"dev-1","user_code":"ABCD-1234"}));
    let exchange = requests.iter().find(|r| r.path() == "/oauth/token").unwrap();
    assert_eq!(exchange.form("code_verifier").as_deref(), Some("device-verifier"));
    assert_eq!(exchange.form("redirect_uri"), Some(format!("{}/deviceauth/callback", issuer.url)));
}

#[tokio::test]
async fn device_login_pending_transient_denied_and_expired() {
    // Pending (403), then a transient 503, then approved.
    let issuer = FakeServer::start(|request, index| match (request.path(), index) {
        ("/api/accounts/deviceauth/usercode", _) => Reply::json(200, &json!({"device_auth_id":"d","user_code":"C","interval":1})),
        ("/api/accounts/deviceauth/token", 1) => Reply::json(403, &json!({})),
        ("/api/accounts/deviceauth/token", 2) => Reply::text(503, "busy"),
        ("/api/accounts/deviceauth/token", _) => Reply::json(200, &json!({"authorization_code":"a","code_verifier":"v"})),
        _ => Reply::json(200, &tokens(unix_now() + 3_600, "rt")),
    })
    .await;
    let config = issuer.config();
    let client = fake::client(&config);
    let challenge = begin_device_login(&client, &config).await.unwrap();
    assert!(finish_device_login(&client, &challenge).await.is_ok());

    let denied = issuer_with(|request| match request.path() {
        "/api/accounts/deviceauth/usercode" => Reply::json(200, &json!({"device_auth_id":"d","user_code":"C","interval":1})),
        _ => Reply::json(410, &json!({"error":"expired_token"})),
    })
    .await;
    let config = denied.config();
    let challenge = begin_device_login(&client, &config).await.unwrap();
    let error = finish_device_login(&client, &challenge).await.unwrap_err();
    assert!(error.message.contains("denied or has expired"), "{}", error.message);

    let pending = issuer_with(|request| match request.path() {
        "/api/accounts/deviceauth/usercode" => Reply::json(200, &json!({"device_auth_id":"d","user_code":"C","interval":600})),
        _ => Reply::json(404, &json!({})),
    })
    .await;
    // The sleep is clamped to the login window even when the server asks for a long interval.
    let config = CodexConfig { login_timeout: Duration::from_millis(1_500), ..pending.config() };
    let challenge = begin_device_login(&client, &config).await.unwrap();
    let started = std::time::Instant::now();
    let error = finish_device_login(&client, &challenge).await.unwrap_err();
    assert!(error.message.contains("expired"), "{}", error.message);
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Maintainer-run: a real browser login into aim's own store. Never run by agents
/// (docs/adr/0010 `live_codex_oauth_browser`; renamed `interactive_` so `-- --ignored live_`
/// never starts it). Set `AIM_INTERACTIVE_SAVE=1` to persist the result to `~/.aim/auth`.
#[tokio::test]
#[ignore = "interactive: needs the maintainer's browser; never touches ~/.codex"]
async fn interactive_codex_oauth_browser() {
    let config = CodexConfig::default();
    let client = config.http_client().unwrap();
    let challenge = begin_browser_login(&config).await.unwrap();
    eprintln!("open this URL to sign in:\n{}", challenge.url);
    let credentials = finish_browser_login(&client, challenge).await.unwrap();
    eprintln!("signed in; token expires_at={} account=***", credentials.expires_at);
    assert!(credentials.refresh_token.is_some());
    if std::env::var_os("AIM_INTERACTIVE_SAVE").is_some() {
        let store = Arc::new(FileCredentialStore::new(FileCredentialStore::default_path().unwrap()));
        AuthManager::new(client, store).save(credentials).await.unwrap();
        eprintln!("saved to ~/.aim/auth/codex.json");
    }
}

/// Maintainer-run: a real device-code login (docs/adr/0010 `live_codex_oauth_device`).
#[tokio::test]
#[ignore = "interactive: needs the maintainer to approve a device code; never touches ~/.codex"]
async fn interactive_codex_oauth_device() {
    let config = CodexConfig::default();
    let client = config.http_client().unwrap();
    let challenge = begin_device_login(&client, &config).await.unwrap();
    eprintln!("visit {} and enter {}", challenge.verification_url, challenge.user_code);
    let credentials = finish_device_login(&client, &challenge).await.unwrap();
    eprintln!("signed in; token expires_at={} account=***", credentials.expires_at);
    assert!(credentials.refresh_token.is_some());
    if std::env::var_os("AIM_INTERACTIVE_SAVE").is_some() {
        let store = Arc::new(FileCredentialStore::new(FileCredentialStore::default_path().unwrap()));
        AuthManager::new(client, store).save(credentials).await.unwrap();
        eprintln!("saved to ~/.aim/auth/codex.json");
    }
}
