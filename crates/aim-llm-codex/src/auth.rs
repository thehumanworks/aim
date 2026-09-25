//! OAuth credentials for the Codex subscription backend.
//! Borrowed Codex CLI credentials are read only and are never refreshed.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

fn auth_error(message: &str) -> LlmError {
    LlmError::new(LlmErrorKind::Auth, message)
}

/// Aim-owned or borrowed Codex credentials. Never log or debug-print this value.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// OAuth access token.
    pub access_token: String,
    /// Rotating refresh token, present only for aim-owned credentials.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// OAuth ID token, when returned.
    #[serde(default)]
    pub id_token: Option<String>,
    /// Account id from a JWT claim.
    pub account_id: String,
    /// Access token expiry as Unix seconds.
    pub expires_at: u64,
}

/// Selected non-secret claims from an OAuth JWT (signature validation belongs to the backend).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JwtClaims {
    /// Expiry as Unix seconds.
    pub exp: u64,
    /// Account id.
    pub account_id: Option<String>,
    /// Subscription plan.
    pub plan_type: Option<String>,
}

/// Decode the claims payload of a JWT without verifying its signature.
///
/// # Errors
/// Returns an authentication error if the token, network, callback, or store fails.
pub fn jwt_claims(token: &str) -> Result<JwtClaims, LlmError> {
    let payload = token.split('.').nth(1).ok_or_else(|| auth_error("invalid OAuth token"))?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).map_err(|_| auth_error("invalid OAuth token"))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| auth_error("invalid OAuth token"))?;
    let auth = value.get("https://api.openai.com/auth");
    Ok(JwtClaims {
        exp: value.get("exp").and_then(Value::as_u64).ok_or_else(|| auth_error("OAuth token has no expiry"))?,
        account_id: auth.and_then(|v| v.get("chatgpt_account_id")).and_then(Value::as_str).map(str::to_owned),
        plan_type: auth.and_then(|v| v.get("chatgpt_plan_type")).and_then(Value::as_str).map(str::to_owned),
    })
}

/// Pluggable storage for aim-owned credentials.
pub trait CredentialStore: Send + Sync {
    /// Load owned credentials, if present.
    fn load(&self) -> BoxFuture<'_, Result<Option<Credentials>, LlmError>>;
    /// Atomically replace owned credentials.
    fn save(&self, credentials: Credentials) -> BoxFuture<'_, Result<(), LlmError>>;
}

/// 0600 file-backed credential store.
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    /// Store at a caller-selected path.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Default `~/.aim/auth/codex.json` path.
    ///
    /// # Errors
    /// Returns an authentication error if the token, network, callback, or store fails.
    pub fn default_path() -> Result<PathBuf, LlmError> {
        let home = std::env::var_os("HOME").ok_or_else(|| auth_error("HOME is not set"))?;
        Ok(PathBuf::from(home).join(".aim/auth/codex.json"))
    }
}

impl CredentialStore for FileCredentialStore {
    fn load(&self) -> BoxFuture<'_, Result<Option<Credentials>, LlmError>> {
        Box::pin(async {
            let bytes = match fs::read(&self.path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(auth_error("cannot read aim Codex credentials")),
            };
            serde_json::from_slice(&bytes).map(Some).map_err(|_| auth_error("invalid aim Codex credentials"))
        })
    }

    fn save(&self, credentials: Credentials) -> BoxFuture<'_, Result<(), LlmError>> {
        Box::pin(async move {
            let parent = self.path.parent().ok_or_else(|| auth_error("invalid credential path"))?;
            fs::create_dir_all(parent).map_err(|_| auth_error("cannot create aim auth directory"))?;
            let mut random = [0_u8; 8];
            rand::rng().fill_bytes(&mut random);
            let temp = parent.join(format!(".codex-{}.tmp", URL_SAFE_NO_PAD.encode(random)));
            let result = write_private(&temp, &credentials).and_then(|()| fs::rename(&temp, &self.path));
            if result.is_err() {
                drop(fs::remove_file(&temp));
            }
            result.map_err(|_| auth_error("cannot save aim Codex credentials"))
        })
    }
}

#[cfg(unix)]
fn write_private(path: &Path, credentials: &Credentials) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    let data = serde_json::to_vec(credentials).map_err(std::io::Error::other)?;
    file.write_all(&data)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, credentials: &Credentials) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let data = serde_json::to_vec(credentials).map_err(std::io::Error::other)?;
    file.write_all(&data)?;
    file.sync_all()
}

fn borrowed_path() -> Result<PathBuf, LlmError> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home).join("auth.json"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| auth_error("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".codex/auth.json"))
}

fn borrowed_credentials(path: &Path) -> Result<Credentials, LlmError> {
    let bytes = fs::read(path).map_err(|_| auth_error("run aim login codex"))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| auth_error("invalid Codex CLI credentials"))?;
    let tokens = value.get("tokens").ok_or_else(|| auth_error("Codex CLI has no ChatGPT tokens"))?;
    let access_token = tokens.get("access_token").and_then(Value::as_str).ok_or_else(|| auth_error("Codex CLI has no access token"))?;
    let claims = jwt_claims(access_token)?;
    let account_id = claims
        .account_id
        .or_else(|| tokens.get("account_id").and_then(Value::as_str).map(str::to_owned))
        .ok_or_else(|| auth_error("Codex CLI token has no account id"))?;
    Ok(Credentials { access_token: access_token.to_owned(), refresh_token: None, id_token: None, account_id, expires_at: claims.exp })
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Coordinates credential source selection and aim-owned token refresh.
pub struct AuthManager {
    client: Client,
    store: Arc<dyn CredentialStore>,
    refresh_guard: Mutex<()>,
}

impl AuthManager {
    /// Create an auth manager backed by the supplied store.
    #[must_use]
    pub fn new(client: Client, store: Arc<dyn CredentialStore>) -> Self {
        Self { client, store, refresh_guard: Mutex::new(()) }
    }

    /// Resolve a usable token. Borrowed Codex credentials are never refreshed or written.
    ///
    /// # Errors
    /// Returns an authentication error if the token, network, callback, or store fails.
    pub async fn credentials(&self) -> Result<Credentials, LlmError> {
        let _guard = self.refresh_guard.lock().await;
        if let Some(mut credentials) = self.store.load().await? {
            if credentials.expires_at <= now().saturating_add(300) {
                let refresh = credentials.refresh_token.as_deref().ok_or_else(|| auth_error("run aim login codex"))?;
                let response = self
                    .client
                    .post(format!("{ISSUER}/oauth/token"))
                    .json(&json!({"grant_type":"refresh_token","client_id":CLIENT_ID,"refresh_token":refresh}))
                    .send()
                    .await
                    .map_err(|_| auth_error("Codex token refresh failed"))?;
                if !response.status().is_success() {
                    return Err(auth_error("Codex token refresh rejected; run aim login codex"));
                }
                let body: Value = response.json().await.map_err(|_| auth_error("invalid Codex refresh response"))?;
                credentials = credentials_from_tokens(&body, Some(&credentials))?;
                self.store.save(credentials.clone()).await?;
            }
            return Ok(credentials);
        }
        let credentials = borrowed_credentials(&borrowed_path()?)?;
        if credentials.expires_at <= now().saturating_add(300) {
            return Err(auth_error("Codex CLI token expires soon; run aim login codex"));
        }
        Ok(credentials)
    }

    /// Save newly issued aim-owned credentials.
    ///
    /// # Errors
    /// Returns an authentication error if the token, network, callback, or store fails.
    pub async fn save(&self, credentials: Credentials) -> Result<(), LlmError> {
        self.store.save(credentials).await
    }
}

fn credentials_from_tokens(value: &Value, previous: Option<&Credentials>) -> Result<Credentials, LlmError> {
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .or_else(|| previous.map(|c| c.access_token.as_str()))
        .ok_or_else(|| auth_error("OAuth response has no access token"))?;
    let claims = jwt_claims(access_token)?;
    let id_token = value.get("id_token").and_then(Value::as_str).map(str::to_owned).or_else(|| previous.and_then(|c| c.id_token.clone()));
    let account_id = claims
        .account_id
        .or_else(|| id_token.as_deref().and_then(|t| jwt_claims(t).ok()).and_then(|c| c.account_id))
        .or_else(|| previous.map(|c| c.account_id.clone()))
        .ok_or_else(|| auth_error("OAuth response has no account id"))?;
    Ok(Credentials {
        access_token: access_token.to_owned(),
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| previous.and_then(|c| c.refresh_token.clone())),
        id_token,
        account_id,
        expires_at: claims.exp,
    })
}

/// Browser PKCE challenge ready to present to the user.
pub struct BrowserChallenge {
    /// Authorization URL to open in a browser.
    pub url: String,
    state: String,
    verifier: String,
    redirect_uri: String,
    listener: TcpListener,
}

/// Bind the local callback and construct the authorization URL.
///
/// # Errors
/// Returns an authentication error if the token, network, callback, or store fails.
pub async fn begin_browser_login() -> Result<BrowserChallenge, LlmError> {
    let listener = match TcpListener::bind("127.0.0.1:1455").await {
        Ok(listener) => listener,
        Err(_) => TcpListener::bind("127.0.0.1:1457").await.map_err(|_| auth_error("cannot bind OAuth callback port"))?,
    };
    let port = listener.local_addr().map_err(|_| auth_error("cannot inspect OAuth callback"))?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    rand::rng().fill_bytes(&mut bytes);
    let state = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse(&format!("{ISSUER}/oauth/authorize")).map_err(|_| auth_error("invalid OAuth issuer"))?;
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
        .append_pair("originator", "aim");
    Ok(BrowserChallenge { url: url.into(), state, verifier, redirect_uri, listener })
}

/// Wait for the loopback callback and exchange its verified code. Caller persists the result.
///
/// # Errors
/// Returns an authentication error if the token, network, callback, or store fails.
pub async fn finish_browser_login(client: &Client, challenge: BrowserChallenge) -> Result<Credentials, LlmError> {
    let (mut socket, _) = tokio::time::timeout(std::time::Duration::from_mins(15), challenge.listener.accept())
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
        let received = tokio::time::timeout(std::time::Duration::from_secs(15), socket.read(remaining))
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
    if url.path() != "/auth/callback" || state.as_deref() != Some(challenge.state.as_str()) {
        return Err(auth_error("OAuth callback state mismatch"));
    }
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .ok_or_else(|| auth_error("OAuth callback has no code"))?;
    drop(socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 23\r\n\r\nAuthentication complete").await);
    exchange_code(client, &code, &challenge.verifier, &challenge.redirect_uri).await
}

async fn exchange_code(client: &Client, code: &str, verifier: &str, redirect_uri: &str) -> Result<Credentials, LlmError> {
    let response = client
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .map_err(|_| auth_error("OAuth code exchange failed"))?;
    if !response.status().is_success() {
        return Err(auth_error("OAuth code exchange rejected"));
    }
    let body: Value = response.json().await.map_err(|_| auth_error("invalid OAuth code response"))?;
    credentials_from_tokens(&body, None)
}

/// Device authorization challenge to display to the user.
pub struct DeviceChallenge {
    /// URL at which the user enters the code.
    pub verification_url: String,
    /// Short user code.
    pub user_code: String,
    device_auth_id: String,
    interval: u64,
}

/// Request a device authorization code.
///
/// # Errors
/// Returns an authentication error if the token, network, callback, or store fails.
pub async fn begin_device_login(client: &Client) -> Result<DeviceChallenge, LlmError> {
    let response = client
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .json(&json!({"client_id":CLIENT_ID}))
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
        verification_url: format!("{ISSUER}/codex/device"),
        user_code: string("user_code")?,
        device_auth_id: string("device_auth_id")?,
        interval: body
            .get("interval")
            .and_then(Value::as_u64)
            .or_else(|| body.get("interval").and_then(Value::as_str).and_then(|s| s.parse().ok()))
            .unwrap_or(5)
            .max(1),
    })
}

/// Poll until device authorization completes or the 15-minute window expires.
///
/// # Errors
/// Returns an authentication error if the token, network, callback, or store fails.
pub async fn finish_device_login(client: &Client, challenge: &DeviceChallenge) -> Result<Credentials, LlmError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_mins(15);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(challenge.interval)).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(auth_error("device authorization timed out"));
        }
        let response = client
            .post(format!("{ISSUER}/api/accounts/deviceauth/token"))
            .json(&json!({"device_auth_id":challenge.device_auth_id,"user_code":challenge.user_code}))
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
        return exchange_code(client, code, verifier, &format!("{ISSUER}/deviceauth/callback")).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_jwt_claims() -> Result<(), LlmError> {
        let payload = json!({"exp":4_102_444_800_u64,"https://api.openai.com/auth":{
            "chatgpt_account_id":"account_fixture","chatgpt_plan_type":"pro"}});
        let token = format!("header.{}.signature", URL_SAFE_NO_PAD.encode(payload.to_string()));
        let claims = jwt_claims(&token)?;
        assert_eq!(claims.exp, 4_102_444_800);
        assert_eq!(claims.account_id.as_deref(), Some("account_fixture"));
        assert_eq!(claims.plan_type.as_deref(), Some("pro"));
        assert!(jwt_claims("bad").is_err());
        Ok(())
    }

    #[test]
    fn private_store_round_trip() -> Result<(), LlmError> {
        let mut nonce = [0_u8; 8];
        rand::rng().fill_bytes(&mut nonce);
        let dir = std::env::temp_dir().join(format!("aim-codex-test-{}", URL_SAFE_NO_PAD.encode(nonce)));
        let path = dir.join("codex.json");
        let store = FileCredentialStore::new(path.clone());
        let credentials = Credentials {
            access_token: "synthetic".into(),
            refresh_token: Some("synthetic_refresh".into()),
            id_token: None,
            account_id: "account_fixture".into(),
            expires_at: 4_102_444_800,
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_| auth_error("test runtime failed"))?;
        runtime.block_on(store.save(credentials.clone()))?;
        let loaded = runtime.block_on(store.load())?.ok_or_else(|| auth_error("stored fixture missing"))?;
        assert_eq!(loaded.access_token, credentials.access_token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let metadata = fs::metadata(&path).map_err(|_| auth_error("stored fixture missing"))?;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        fs::remove_dir_all(dir).map_err(|_| auth_error("cannot remove test fixture"))?;
        Ok(())
    }
}
