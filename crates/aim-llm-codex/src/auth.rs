//! OAuth credentials for the Codex subscription backend (docs/adr/0010, research §§1-3).
//!
//! Sources, in order: aim's own store (`~/.aim/auth/codex.json`, 0600 in a 0700 directory), then
//! the Codex CLI's `$CODEX_HOME/auth.json` or `~/.codex/auth.json`, **read-only**. A borrowed token
//! is never refreshed or written: rotating its refresh token would log out every running Codex
//! CLI. Only aim-owned credentials are refreshed, under the store's refresh lease so concurrent
//! processes never spend the same rotating refresh token twice.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aim_llm::{BoxFuture, LlmError, LlmErrorKind};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore as _;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::CodexConfig;
use crate::errors::sanitize_code;

/// The Codex CLI's OAuth client id (refs:login/src/auth/manager.rs:1716-1729).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Scopes requested by the browser login (refs:login/src/server.rs:584-615).
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// Refresh (or refuse a borrowed token) when it expires within this many seconds.
const EXPIRY_MARGIN_SECS: u64 = 300;
/// How long credentials are served from memory before the sources are read again.
const CACHE_TTL: Duration = Duration::from_secs(60);
/// Longest wait for another process's refresh lease.
const LEASE_WAIT: Duration = Duration::from_secs(30);

fn auth_error(message: impl Into<String>) -> LlmError {
    LlmError::new(LlmErrorKind::Auth, message)
}

fn io_error(message: &str) -> LlmError {
    LlmError::new(LlmErrorKind::Auth, message)
}

/// A secret string. `Debug` never shows it; read it with [`Secret::expose`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself: only for putting it on the wire.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Codex OAuth credentials, aim-owned or borrowed. `Debug` redacts everything but the expiry.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// OAuth access token.
    pub access_token: Secret,
    /// Rotating refresh token; only aim-owned credentials have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<Secret>,
    /// OAuth ID token, when issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<Secret>,
    /// `ChatGPT` account id (from the JWT claim).
    pub account_id: String,
    /// Access-token expiry, Unix seconds.
    pub expires_at: u64,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &self.access_token)
            .field("refresh_token", &self.refresh_token)
            .field("id_token", &self.id_token)
            .field("account_id", &"***")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Selected claims of an OAuth JWT (the backend verifies signatures; aim only reads claims).
#[derive(Clone, PartialEq, Eq)]
pub struct JwtClaims {
    /// Expiry, Unix seconds.
    pub exp: u64,
    /// `ChatGPT` account id.
    pub account_id: Option<String>,
    /// Subscription plan.
    pub plan_type: Option<String>,
}

impl fmt::Debug for JwtClaims {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JwtClaims")
            .field("exp", &self.exp)
            .field("account_id", &self.account_id.as_ref().map(|_| "***"))
            .field("plan_type", &self.plan_type)
            .finish()
    }
}

/// Decodes the claims of a JWT without verifying its signature.
///
/// # Errors
/// `Auth` if the token is not a JWT with an `exp` claim.
pub fn jwt_claims(token: &str) -> Result<JwtClaims, LlmError> {
    let payload = token.split('.').nth(1).ok_or_else(|| auth_error("invalid OAuth token"))?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).map_err(|_| auth_error("invalid OAuth token"))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| auth_error("invalid OAuth token"))?;
    let auth = value.get("https://api.openai.com/auth");
    Ok(JwtClaims {
        exp: value.get("exp").and_then(Value::as_u64).ok_or_else(|| auth_error("OAuth token has no expiry"))?,
        account_id: auth.and_then(|v| v.get("chatgpt_account_id")).and_then(Value::as_str).map(str::to_owned),
        plan_type: auth.and_then(|v| v.get("chatgpt_plan_type")).and_then(Value::as_str).map(str::to_owned),
    })
}

/// An exclusive refresh lease on a credential store, released when dropped.
pub struct Lease {
    _guard: Option<Box<dyn Send + Sync>>,
}

impl Lease {
    /// No lease (a store without cross-process coordination).
    #[must_use]
    pub fn none() -> Self {
        Self { _guard: None }
    }

    /// A lease held for as long as `guard` lives.
    #[must_use]
    pub fn new(guard: impl Send + Sync + 'static) -> Self {
        Self { _guard: Some(Box::new(guard)) }
    }
}

/// Storage for aim-owned credentials (a keyring store can replace the file later).
pub trait CredentialStore: Send + Sync {
    /// Loads the stored credentials, if any.
    fn load(&self) -> BoxFuture<'_, Result<Option<Credentials>, LlmError>>;
    /// Atomically replaces the stored credentials.
    fn save(&self, credentials: Credentials) -> BoxFuture<'_, Result<(), LlmError>>;
    /// Takes the exclusive refresh lease, so only one process refreshes (and rotates) the token
    /// at a time. The default has no cross-process coordination.
    fn lease(&self) -> BoxFuture<'_, Result<Lease, LlmError>> {
        Box::pin(async { Ok(Lease::none()) })
    }
}

/// Runs blocking file I/O off the async workers.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, LlmError> + Send + 'static) -> Result<T, LlmError> {
    tokio::task::spawn_blocking(work).await.map_err(|_| io_error("credential file task failed"))?
}

/// A 0600 file in a 0700 directory, replaced atomically; its refresh lease is an advisory lock
/// on a sibling `.lock` file.
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    /// A store at `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `~/.aim/auth/codex.json`.
    ///
    /// # Errors
    /// `Auth` if `HOME` is not set.
    pub fn default_path() -> Result<PathBuf, LlmError> {
        let home = std::env::var_os("HOME").ok_or_else(|| auth_error("HOME is not set"))?;
        Ok(PathBuf::from(home).join(".aim/auth/codex.json"))
    }
}

impl CredentialStore for FileCredentialStore {
    fn load(&self) -> BoxFuture<'_, Result<Option<Credentials>, LlmError>> {
        let path = self.path.clone();
        Box::pin(blocking(move || read_store(&path)))
    }

    fn save(&self, credentials: Credentials) -> BoxFuture<'_, Result<(), LlmError>> {
        let path = self.path.clone();
        Box::pin(blocking(move || write_store(&path, &credentials)))
    }

    fn lease(&self) -> BoxFuture<'_, Result<Lease, LlmError>> {
        let path = self.path.clone();
        Box::pin(blocking(move || take_lease(&path)))
    }
}

fn read_store(path: &Path) -> Result<Option<Credentials>, LlmError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(io_error("cannot read aim's Codex credentials")),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|_| io_error("aim's Codex credentials are corrupt; run `aim login codex`"))
}

fn write_store(path: &Path, credentials: &Credentials) -> Result<(), LlmError> {
    let parent = path.parent().ok_or_else(|| io_error("invalid credential path"))?;
    private_dir(parent).map_err(|_| io_error("cannot create aim's auth directory"))?;
    let mut random = [0_u8; 8];
    rand::rng().fill_bytes(&mut random);
    let temp = parent.join(format!(".codex-{}.tmp", URL_SAFE_NO_PAD.encode(random)));
    let result = write_private(&temp, credentials).and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        drop(fs::remove_file(&temp));
    }
    result.map_err(|_| io_error("cannot save aim's Codex credentials"))?;
    // Make the rename durable; a failure here leaves a correct file that may not survive a crash.
    drop(File::open(parent).and_then(|dir| dir.sync_all()));
    Ok(())
}

fn take_lease(path: &Path) -> Result<Lease, LlmError> {
    let parent = path.parent().ok_or_else(|| io_error("invalid credential path"))?;
    private_dir(parent).map_err(|_| io_error("cannot create aim's auth directory"))?;
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("codex.json");
    let file = open_private(&parent.join(format!("{name}.lock")), false).map_err(|_| io_error("cannot open aim's credential lock"))?;
    let deadline = std::time::Instant::now() + LEASE_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Lease::new(file)),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return Err(io_error("another process holds aim's Codex refresh lease")),
        }
    }
}

#[cfg(unix)]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // A directory created by an older aim with the umask default is tightened too.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_private(path: &Path, create_new: bool) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut options = OpenOptions::new();
    options.read(true).write(true).mode(0o600);
    if create_new {
        options.create_new(true)
    } else {
        options.create(true).truncate(false)
    };
    options.open(path)
}

#[cfg(not(unix))]
fn open_private(path: &Path, create_new: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true)
    } else {
        options.create(true).truncate(false)
    };
    options.open(path)
}

fn write_private(path: &Path, credentials: &Credentials) -> std::io::Result<()> {
    let mut file = open_private(path, true)?;
    let data = serde_json::to_vec(credentials).map_err(std::io::Error::other)?;
    file.write_all(&data)?;
    file.sync_all()
}

/// `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`.
///
/// # Errors
/// `Auth` if neither `CODEX_HOME` nor `HOME` is set.
pub fn codex_cli_auth_path() -> Result<PathBuf, LlmError> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home).join("auth.json"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| auth_error("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".codex/auth.json"))
}

/// Reads the Codex CLI's credentials. Read-only by construction: this function only reads.
fn read_borrowed(path: &Path) -> Result<Credentials, LlmError> {
    let bytes = fs::read(path).map_err(|_| auth_error("no Codex credentials; run `aim login codex`"))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| auth_error("the Codex CLI's credentials are not valid JSON"))?;
    let tokens = value.get("tokens").ok_or_else(|| auth_error("the Codex CLI has no ChatGPT login; run `aim login codex`"))?;
    let access_token = tokens.get("access_token").and_then(Value::as_str).ok_or_else(|| auth_error("the Codex CLI has no access token"))?;
    let claims = jwt_claims(access_token)?;
    let account_id = claims
        .account_id
        .or_else(|| tokens.get("account_id").and_then(Value::as_str).map(str::to_owned))
        .ok_or_else(|| auth_error("the Codex CLI token has no account id"))?;
    Ok(Credentials { access_token: Secret::new(access_token), refresh_token: None, id_token: None, account_id, expires_at: claims.exp })
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn fresh(credentials: &Credentials) -> bool {
    credentials.expires_at > now().saturating_add(EXPIRY_MARGIN_SECS)
}

struct Cached {
    credentials: Credentials,
    loaded: Instant,
    /// Held in memory until expiry because saving them failed (the old refresh token is spent).
    pinned: bool,
}

/// Chooses the credential source and refreshes aim-owned tokens. Share one per process.
pub struct AuthManager {
    client: Client,
    store: Arc<dyn CredentialStore>,
    issuer: String,
    request_timeout: Duration,
    borrowed_path: Option<PathBuf>,
    cache: Mutex<Option<Cached>>,
}

impl AuthManager {
    /// A manager for `store` with the default issuer.
    #[must_use]
    pub fn new(client: Client, store: Arc<dyn CredentialStore>) -> Self {
        Self::with_config(client, store, &CodexConfig::default())
    }

    /// A manager for `store` using `config`'s issuer and request timeout.
    #[must_use]
    pub fn with_config(client: Client, store: Arc<dyn CredentialStore>, config: &CodexConfig) -> Self {
        Self {
            client,
            store,
            issuer: config.issuer.clone(),
            request_timeout: config.request_timeout,
            borrowed_path: None,
            cache: Mutex::new(None),
        }
    }

    /// Reads borrowed credentials from `path` instead of [`codex_cli_auth_path`].
    #[must_use]
    pub fn with_borrowed_path(mut self, path: PathBuf) -> Self {
        self.borrowed_path = Some(path);
        self
    }

    /// A usable token: aim's own (refreshed when due), else the Codex CLI's, which is never
    /// refreshed or written.
    ///
    /// # Errors
    /// `Auth` when there is no usable login; `Transport`/`Unavailable` when a refresh could not
    /// reach the issuer.
    pub async fn credentials(&self) -> Result<Credentials, LlmError> {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref()
            && (cached.pinned || cached.loaded.elapsed() < CACHE_TTL)
            && fresh(&cached.credentials)
        {
            return Ok(cached.credentials.clone());
        }
        let (credentials, pinned) = if let Some(owned) = self.store.load().await? {
            if fresh(&owned) { (owned, false) } else { self.refresh().await? }
        } else {
            let path = match &self.borrowed_path {
                Some(path) => path.clone(),
                None => codex_cli_auth_path()?,
            };
            let borrowed = blocking(move || read_borrowed(&path)).await?;
            if !fresh(&borrowed) {
                return Err(auth_error("the Codex CLI token has expired (aim never refreshes it); run `aim login codex`"));
            }
            (borrowed, false)
        };
        *cache = Some(Cached { credentials: credentials.clone(), loaded: Instant::now(), pinned });
        Ok(credentials)
    }

    /// Forgets cached credentials (after the backend rejected them).
    pub async fn invalidate(&self) {
        *self.cache.lock().await = None;
    }

    /// Stores newly issued aim-owned credentials.
    ///
    /// # Errors
    /// If the store cannot be written.
    pub async fn save(&self, credentials: Credentials) -> Result<(), LlmError> {
        let mut cache = self.cache.lock().await;
        self.store.save(credentials.clone()).await?;
        *cache = Some(Cached { credentials, loaded: Instant::now(), pinned: false });
        Ok(())
    }

    /// Refreshes aim's own token under the store's lease. Returns the credentials and whether
    /// they must be pinned in memory because saving them failed.
    async fn refresh(&self) -> Result<(Credentials, bool), LlmError> {
        let _lease = self.store.lease().await?;
        // Another process may have refreshed while this one waited for the lease.
        let current = self.store.load().await?.ok_or_else(|| auth_error("aim's Codex credentials disappeared; run `aim login codex`"))?;
        if fresh(&current) {
            return Ok((current, false));
        }
        let refresh_token = current
            .refresh_token
            .as_ref()
            .ok_or_else(|| auth_error("aim's Codex credentials cannot be refreshed; run `aim login codex`"))?;
        // JSON body, as codex does (refs:login/src/auth/manager.rs:1598-1648).
        let response = self
            .client
            .post(format!("{}/oauth/token", self.issuer))
            .timeout(self.request_timeout)
            .json(&json!({"grant_type": "refresh_token", "client_id": CLIENT_ID, "refresh_token": refresh_token.expose()}))
            .send()
            .await
            .map_err(|_| LlmError::new(LlmErrorKind::Transport, "Codex token refresh failed: network error"))?;
        let body = token_response(response, "Codex token refresh").await?;
        let refreshed = credentials_from_tokens(&body, Some(&current))?;
        // The old refresh token is spent: if saving fails, keep the new one in memory.
        let pinned = self.store.save(refreshed.clone()).await.is_err();
        Ok((refreshed, pinned))
    }
}

/// Checks an OAuth token response: 5xx is `Unavailable`, other failures `Auth`, and a success
/// must carry an access token.
async fn token_response(response: reqwest::Response, what: &str) -> Result<Value, LlmError> {
    let status = response.status();
    if !status.is_success() {
        let body: Value = response.json().await.unwrap_or(Value::Null);
        let code = body.get("error").and_then(|e| e.as_str().or_else(|| e.get("code").and_then(Value::as_str))).map(sanitize_code);
        let code = code.map(|code| format!(" [{code}]")).unwrap_or_default();
        let (kind, advice) =
            if status.is_server_error() { (LlmErrorKind::Unavailable, "") } else { (LlmErrorKind::Auth, "; run `aim login codex`") };
        let mut error = LlmError::new(kind, format!("{what} rejected (HTTP {}){code}{advice}", status.as_u16()));
        error.status = Some(status.as_u16());
        return Err(error);
    }
    let body: Value = response.json().await.map_err(|_| LlmError::new(LlmErrorKind::Protocol, format!("{what} returned invalid JSON")))?;
    if body.get("access_token").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err(auth_error(format!("{what} returned no access token")));
    }
    Ok(body)
}

fn credentials_from_tokens(value: &Value, previous: Option<&Credentials>) -> Result<Credentials, LlmError> {
    let access_token = value.get("access_token").and_then(Value::as_str).ok_or_else(|| auth_error("OAuth response has no access token"))?;
    let claims = jwt_claims(access_token)?;
    let id_token = value.get("id_token").and_then(Value::as_str).map(Secret::new).or_else(|| previous.and_then(|c| c.id_token.clone()));
    let account_id = claims
        .account_id
        .or_else(|| id_token.as_ref().and_then(|t| jwt_claims(t.expose()).ok()).and_then(|c| c.account_id))
        .or_else(|| previous.map(|c| c.account_id.clone()))
        .ok_or_else(|| auth_error("OAuth response has no account id"))?;
    Ok(Credentials {
        access_token: Secret::new(access_token),
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(Secret::new)
            .or_else(|| previous.and_then(|c| c.refresh_token.clone())),
        id_token,
        account_id,
        expires_at: claims.exp,
    })
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A browser login in progress: show [`BrowserChallenge::url`], then call [`finish_browser_login`].
pub struct BrowserChallenge {
    /// Authorization URL to open in a browser.
    pub url: String,
    state: Secret,
    verifier: Secret,
    redirect_uri: String,
    listener: TcpListener,
    issuer: String,
    request_timeout: Duration,
    login_timeout: Duration,
}

impl BrowserChallenge {
    /// The loopback port the callback server listens on.
    ///
    /// # Errors
    /// If the listener's address cannot be read.
    pub fn port(&self) -> Result<u16, LlmError> {
        self.listener.local_addr().map(|addr| addr.port()).map_err(|_| auth_error("cannot read the OAuth callback address"))
    }
}

/// Binds the loopback callback (the configured ports in order) and builds the PKCE
/// authorization URL (research §1, refs:login/src/server.rs:584-615).
///
/// # Errors
/// `Auth` if no callback port can be bound.
pub async fn begin_browser_login(config: &CodexConfig) -> Result<BrowserChallenge, LlmError> {
    let mut listener = None;
    for port in &config.callback_ports {
        if let Ok(bound) = TcpListener::bind(("127.0.0.1", *port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.ok_or_else(|| auth_error("cannot bind the OAuth callback port (is another login running?)"))?;
    let port = listener.local_addr().map_err(|_| auth_error("cannot read the OAuth callback address"))?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
    let verifier = random_token();
    let state = random_token();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse(&format!("{}/oauth/authorize", config.issuer)).map_err(|_| auth_error("invalid OAuth issuer URL"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", crate::ORIGINATOR);
    Ok(BrowserChallenge {
        url: url.into(),
        state: Secret::new(state),
        verifier: Secret::new(verifier),
        redirect_uri,
        listener,
        issuer: config.issuer.clone(),
        request_timeout: config.request_timeout,
        login_timeout: config.login_timeout,
    })
}

/// Waits for the loopback callback and exchanges its verified code. The caller persists the
/// result ([`AuthManager::save`]).
///
/// # Errors
/// `Auth` if the callback times out, fails or carries a wrong state; the exchange's errors otherwise.
pub async fn finish_browser_login(client: &Client, challenge: BrowserChallenge) -> Result<Credentials, LlmError> {
    let (mut socket, _) = tokio::time::timeout(challenge.login_timeout, challenge.listener.accept())
        .await
        .map_err(|_| auth_error("OAuth callback timed out"))?
        .map_err(|_| auth_error("OAuth callback failed"))?;
    let mut buffer = [0_u8; 8192];
    let mut count = 0;
    loop {
        let remaining = buffer.get_mut(count..).ok_or_else(|| auth_error("OAuth callback too large"))?;
        if remaining.is_empty() {
            return Err(auth_error("OAuth callback too large"));
        }
        let received = tokio::time::timeout(Duration::from_secs(15), socket.read(remaining))
            .await
            .map_err(|_| auth_error("OAuth callback timed out"))?
            .map_err(|_| auth_error("OAuth callback failed"))?;
        if received == 0 {
            return Err(auth_error("OAuth callback closed early"));
        }
        count += received;
        if buffer.get(..count).is_some_and(|bytes| bytes.windows(4).any(|window| window == b"\r\n\r\n")) {
            break;
        }
    }
    let request = std::str::from_utf8(buffer.get(..count).ok_or_else(|| auth_error("invalid OAuth callback"))?)
        .map_err(|_| auth_error("invalid OAuth callback"))?;
    let target = request.split_whitespace().nth(1).ok_or_else(|| auth_error("invalid OAuth callback"))?;
    let url = url::Url::parse(&format!("http://localhost{target}")).map_err(|_| auth_error("invalid OAuth callback"))?;
    let state = url.query_pairs().find(|(key, _)| key == "state").map(|(_, value)| value.into_owned());
    if url.path() != "/auth/callback" || state.as_deref() != Some(challenge.state.expose()) {
        return Err(auth_error("OAuth callback state mismatch"));
    }
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .ok_or_else(|| auth_error("OAuth callback has no code"))?;
    drop(socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 23\r\n\r\nAuthentication complete").await);
    exchange_code(client, &challenge.issuer, challenge.request_timeout, &code, challenge.verifier.expose(), &challenge.redirect_uri).await
}

/// Exchanges an authorization code (form-encoded, refs:login/src/server.rs:795-827).
async fn exchange_code(
    client: &Client,
    issuer: &str,
    timeout: Duration,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Credentials, LlmError> {
    let response = client
        .post(format!("{issuer}/oauth/token"))
        .timeout(timeout)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|_| LlmError::new(LlmErrorKind::Transport, "OAuth code exchange failed: network error"))?;
    let body = token_response(response, "OAuth code exchange").await?;
    credentials_from_tokens(&body, None)
}

/// A device login in progress: show the URL and code, then call [`finish_device_login`].
pub struct DeviceChallenge {
    /// Where the user enters the code.
    pub verification_url: String,
    /// The code to enter.
    pub user_code: String,
    device_auth_id: Secret,
    interval: u64,
    issuer: String,
    request_timeout: Duration,
    login_timeout: Duration,
}

/// Requests a device code (research §2, refs:login/src/device_code_auth.rs:19-96).
///
/// # Errors
/// `Auth` if the issuer cannot be reached or refuses.
pub async fn begin_device_login(client: &Client, config: &CodexConfig) -> Result<DeviceChallenge, LlmError> {
    let response = client
        .post(format!("{}/api/accounts/deviceauth/usercode", config.issuer))
        .timeout(config.request_timeout)
        .json(&json!({"client_id": CLIENT_ID}))
        .send()
        .await
        .map_err(|_| auth_error("device authorization failed"))?;
    if !response.status().is_success() {
        return Err(auth_error("device authorization rejected"));
    }
    let body: Value = response.json().await.map_err(|_| auth_error("invalid device authorization response"))?;
    let string =
        |key| body.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| auth_error("invalid device authorization response"));
    Ok(DeviceChallenge {
        verification_url: format!("{}/codex/device", config.issuer),
        user_code: string("user_code")?,
        device_auth_id: Secret::new(string("device_auth_id")?),
        interval: body
            .get("interval")
            .and_then(Value::as_u64)
            .or_else(|| body.get("interval").and_then(Value::as_str).and_then(|s| s.parse().ok()))
            .unwrap_or(5)
            .max(1),
        issuer: config.issuer.clone(),
        request_timeout: config.request_timeout,
        login_timeout: config.login_timeout,
    })
}

/// Polls until device authorization completes or the login window ends.
///
/// # Errors
/// `Auth` on expiry, a failed poll or a rejected exchange.
pub async fn finish_device_login(client: &Client, challenge: &DeviceChallenge) -> Result<Credentials, LlmError> {
    let deadline = Instant::now() + challenge.login_timeout;
    loop {
        tokio::time::sleep(Duration::from_secs(challenge.interval)).await;
        if Instant::now() >= deadline {
            return Err(auth_error("device authorization timed out"));
        }
        let response = client
            .post(format!("{}/api/accounts/deviceauth/token", challenge.issuer))
            .timeout(challenge.request_timeout)
            .json(&json!({"device_auth_id": challenge.device_auth_id.expose(), "user_code": challenge.user_code}))
            .send()
            .await
            .map_err(|_| auth_error("device authorization poll failed"))?;
        if matches!(response.status().as_u16(), 403 | 404) {
            continue;
        }
        if !response.status().is_success() {
            return Err(auth_error("device authorization rejected"));
        }
        let body: Value = response.json().await.map_err(|_| auth_error("invalid device authorization response"))?;
        let code = body.get("authorization_code").and_then(Value::as_str).ok_or_else(|| auth_error("device authorization has no code"))?;
        let verifier =
            body.get("code_verifier").and_then(Value::as_str).ok_or_else(|| auth_error("device authorization has no verifier"))?;
        let redirect = format!("{}/deviceauth/callback", challenge.issuer);
        return exchange_code(client, &challenge.issuer, challenge.request_timeout, code, verifier, &redirect).await;
    }
}

#[cfg(test)]
pub(crate) mod tests;
