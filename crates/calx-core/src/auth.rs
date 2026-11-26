//! Sign in with ChatGPT: lets calx spend a ChatGPT plan's usage instead of
//! pay-as-you-go API credits.
//!
//! Each install registers its own OAuth client during login (OpenAI returns the
//! issued client ID with the authorization code). The access token then works
//! as a bearer token for the public Responses API at api.openai.com.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use url::Url;

const DEFAULT_AUTH_BASE_URL: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AGENT_NAME: &str = "Calexandria";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Refresh this many seconds before expiry, so no request starts with a token about to lapse.
const EXPIRY_MARGIN_SECS: u64 = 180;

fn redirect_uri() -> String {
    format!("http://127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}")
}

/// `CALX_AUTH_BASE_URL` points login at a test server instead of OpenAI.
fn auth_base_url() -> String {
    std::env::var("CALX_AUTH_BASE_URL").unwrap_or_else(|_| DEFAULT_AUTH_BASE_URL.to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("no system randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The PKCE S256 challenge for a code verifier (RFC 7636).
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds when the access token expires.
    pub expires_at: u64,
    /// The OAuth client OpenAI issued to this install at login.
    pub client_id: String,
}

/// Where credentials live: `auth.json` in the calx home directory.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    chatgpt: Option<Credential>,
}

impl CredentialStore {
    pub fn open() -> Result<Self> {
        Ok(Self {
            path: crate::calx_home()?.join("auth.json"),
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn load(&self) -> Result<Option<Credential>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let file: AuthFile = serde_json::from_str(&text)
                    .with_context(|| format!("could not parse {}", self.path.display()))?;
                Ok(file.chatgpt)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("could not read {}", self.path.display())),
        }
    }

    pub fn save(&self, credential: Option<&Credential>) -> Result<()> {
        let file = AuthFile {
            chatgpt: credential.cloned(),
        };
        write_private(&self.path, &serde_json::to_string_pretty(&file)?)
    }
}

/// Writes a file only the current user can read (on Unix; Windows profile
/// directories are already private to the user).
fn write_private(path: &PathBuf, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options
        .open(path)
        .with_context(|| format!("could not write {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

/// A stable ID for this install, which OpenAI uses to tell one agent host from another.
fn device_id() -> Result<String> {
    let path = crate::calx_home()?.join("device_id");
    if let Ok(id) = std::fs::read_to_string(&path)
        && uuid::Uuid::parse_str(id.trim()).is_ok()
    {
        return Ok(id.trim().to_string());
    }
    let id = uuid::Uuid::new_v4().to_string();
    write_private(&path, &id)?;
    Ok(id)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    #[serde(default)]
    scope: String,
}

async fn request_token(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let response = http
        .post(format!("{}/api/accounts/oauth/token", auth_base_url()))
        .header("accept", "application/json")
        .form(form)
        .send()
        .await
        .context("could not reach the OpenAI sign-in server")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("OpenAI token request failed ({status}): {body}");
    }
    let token: TokenResponse =
        serde_json::from_str(&body).context("OpenAI token response was missing fields")?;
    if !token
        .scope
        .split_whitespace()
        .any(|s| s == DIRECT_TOKEN_SCOPE)
    {
        bail!(
            "the ChatGPT sign-in did not grant plan usage ({DIRECT_TOKEN_SCOPE}); run `calx login` again and allow \"Use your ChatGPT plan\""
        );
    }
    Ok(token)
}

fn credential_from(token: TokenResponse, client_id: String) -> Credential {
    Credential {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: now_secs() + token.expires_in,
        client_id,
    }
}

/// The authorization code the browser hands back after sign-in.
#[derive(Debug)]
pub struct AuthCode {
    code: String,
    client_id: String,
}

/// A login in progress: the browser URL to open, and the local server waiting
/// for the browser to come back.
pub struct PendingLogin {
    pub url: String,
    verifier: String,
    state: String,
    listener: TcpListener,
}

impl PendingLogin {
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await.with_context(|| {
            format!("port {CALLBACK_PORT} is in use, probably by another login in progress or by the Codex CLI; finish or cancel it and try again")
        })?;
        let verifier = random_token()?;
        let state = random_token()?;
        let mut url = Url::parse(&format!("{}/api/accounts/authorize", auth_base_url()))?;
        url.query_pairs_mut()
            .append_pair("client_id", DYNAMIC_CLIENT_ID)
            .append_pair("agent_name_hint", AGENT_NAME)
            .append_pair("ext_agent_host_id", &format!("urn:uuid:{}", device_id()?))
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri())
            .append_pair("resource", RESOURCE)
            .append_pair("scope", SCOPE)
            .append_pair("state", &state)
            .append_pair("code_challenge", &pkce_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("nonce", &random_token()?);
        Ok(Self {
            url: url.into(),
            verifier,
            state,
            listener,
        })
    }

    /// Waits for the browser to reach the local callback server.
    pub async fn wait_for_callback(&self) -> Result<AuthCode> {
        let (tx, mut rx) = mpsc::channel(4);
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    // Browsers open spare connections that never send a request,
                    // so each connection gets its own task instead of blocking the loop.
                    tokio::spawn(handle_callback(stream, self.state.clone(), tx.clone()));
                }
                Some(result) = rx.recv() => return result,
            }
        }
    }

    /// Reads the redirect URL the user pasted, for when the browser cannot
    /// reach this machine's callback server (for example over SSH).
    pub fn parse_pasted(&self, input: &str) -> Result<AuthCode> {
        let url = Url::parse(input.trim()).map_err(|_| {
            anyhow!(
                "paste the full URL from the browser's address bar, starting with {}",
                redirect_uri()
            )
        })?;
        parse_callback(&url, &self.state)
    }

    pub async fn finish(self, code: AuthCode) -> Result<Credential> {
        let http = reqwest::Client::new();
        let redirect = redirect_uri();
        let token = request_token(
            &http,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &code.client_id),
                ("code", &code.code),
                ("code_verifier", &self.verifier),
                ("redirect_uri", &redirect),
                ("resource", RESOURCE),
            ],
        )
        .await?;
        Ok(credential_from(token, code.client_id))
    }
}

fn parse_callback(url: &Url, expected_state: &str) -> Result<AuthCode> {
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
            .filter(|v| !v.is_empty())
    };
    if let Some(error) = param("error") {
        bail!("ChatGPT sign-in failed: {error}");
    }
    if param("state").as_deref() != Some(expected_state) {
        bail!("the sign-in response did not match this login (state mismatch)");
    }
    Ok(AuthCode {
        code: param("code").context("the sign-in response had no authorization code")?,
        client_id: param("client_id").context("the sign-in response had no issued client ID")?,
    })
}

async fn handle_callback(mut stream: TcpStream, state: String, tx: mpsc::Sender<Result<AuthCode>>) {
    let mut buf = vec![0u8; 8192];
    let mut len = 0;
    while len < buf.len() {
        match stream.read(&mut buf[len..]).await {
            Ok(0) | Err(_) => return,
            Ok(n) => len += n,
        }
        if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buf[..len]);
    let target = request.split_whitespace().nth(1).unwrap_or("/");
    let url = Url::parse(&format!("http://127.0.0.1:{CALLBACK_PORT}{target}"));
    let (status, message, result) = match url {
        Ok(url) if url.path() == CALLBACK_PATH => match parse_callback(&url, &state) {
            Ok(code) => (
                "200 OK",
                "Signed in to calx. You can close this window.".to_string(),
                Some(Ok(code)),
            ),
            Err(e) => {
                let message = e.to_string();
                // A stale callback from an earlier login should not end this one.
                let fatal = !message.contains("state mismatch");
                ("400 Bad Request", message, fatal.then_some(Err(e)))
            }
        },
        _ => ("404 Not Found", "Not found.".to_string(), None),
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>calx</title><body style=\"font-family:system-ui;padding:3em\"><h2>{}</h2></body>",
        html_escape(&message)
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    if let Some(result) = result {
        let _ = tx.send(result).await;
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Hands out a valid ChatGPT access token, refreshing and saving it when it is
/// about to expire.
pub struct ChatGptAuth {
    store: CredentialStore,
    credential: Mutex<Credential>,
    http: reqwest::Client,
}

impl ChatGptAuth {
    pub fn load(store: CredentialStore) -> Result<Self> {
        let credential = store
            .load()?
            .context("not signed in to ChatGPT; run `calx login`")?;
        Ok(Self {
            store,
            credential: Mutex::new(credential),
            http: reqwest::Client::new(),
        })
    }

    pub async fn access_token(&self) -> Result<String> {
        let mut credential = self.credential.lock().await;
        if now_secs() + EXPIRY_MARGIN_SECS >= credential.expires_at {
            self.refresh_locked(&mut credential).await?;
        }
        Ok(credential.access_token.clone())
    }

    /// Refreshes now, for when the server rejected a token that looked valid.
    pub async fn refresh(&self) -> Result<()> {
        let mut credential = self.credential.lock().await;
        self.refresh_locked(&mut credential).await
    }

    async fn refresh_locked(&self, credential: &mut Credential) -> Result<()> {
        let token = request_token(
            &self.http,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &credential.client_id),
                ("refresh_token", &credential.refresh_token),
                ("resource", RESOURCE),
            ],
        )
        .await
        .context("could not refresh your ChatGPT sign-in; run `calx login` again")?;
        *credential = credential_from(token, credential.client_id.clone());
        self.store.save(Some(credential))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        // RFC 7636, Appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
