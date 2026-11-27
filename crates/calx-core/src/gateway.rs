//! Sign in to a company model gateway in the browser, and keep its token
//! fresh in the background.
//!
//! The real gateway's flow is not known yet, so this follows standard OAuth
//! with every endpoint configurable:
//!
//! 1. The browser opens `start_url` with `redirect_uri`, `state`, and, when
//!    set, `client_id`. After sign-in it comes back to calx on localhost
//!    with either `token=` (a long-lived credential) or `code=`.
//! 2. calx exchanges that at `token_url` for a short-lived access token: a
//!    `refresh_token` grant for a token, an `authorization_code` grant for a
//!    code. The JSON reply holds `access_token`, `expires_in`, and, when the
//!    gateway rotates it, a new `refresh_token`.
//! 3. A background task repeats the exchange before each access token
//!    expires, so a session left idle still has a valid one, and the
//!    credential is saved each time it changes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use url::Url;

use crate::auth::{Callback, now_secs, random_token};
use crate::store::CredentialStore;

/// The callback port when none is set; ChatGPT's login uses 1455.
pub const DEFAULT_PORT: u16 = 1456;

/// How to sign in to one gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub start_url: String,
    pub token_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The localhost port the browser comes back to; gateways usually allow
    /// only redirect URIs registered in advance.
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayCredential {
    pub config: GatewayConfig,
    pub access_token: String,
    /// What the next exchange sends.
    pub refresh_token: String,
    /// Unix seconds when the access token expires.
    pub expires_at: u64,
    /// Unix seconds when to exchange it, a while before it expires.
    pub refresh_at: u64,
}

/// What the browser brought back.
#[derive(Debug, PartialEq, Eq)]
pub enum Grant {
    Token(String),
    Code(String),
}

pub struct PendingGatewayLogin {
    pub url: String,
    config: GatewayConfig,
    state: String,
    callback: Callback,
}

impl PendingGatewayLogin {
    pub async fn start(config: GatewayConfig) -> Result<Self> {
        let callback = Callback::bind(config.port, "probably by another login in progress").await?;
        let state = random_token()?;
        let mut url = Url::parse(&config.start_url)
            .with_context(|| format!("the start URL {} is not valid", config.start_url))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("redirect_uri", &callback.redirect_uri())
                .append_pair("state", &state);
            if let Some(id) = &config.client_id {
                q.append_pair("client_id", id);
            }
        }
        Ok(Self {
            url: url.into(),
            config,
            state,
            callback,
        })
    }

    pub async fn wait_for_callback(&self) -> Result<Grant> {
        let state = self.state.clone();
        self.callback
            .wait(move |url| parse_grant(url, &state))
            .await
    }

    /// Reads the URL the browser ended on, when it could not reach calx.
    pub fn parse_pasted(&self, input: &str) -> Result<Grant> {
        let url = Url::parse(input.trim()).map_err(|_| {
            anyhow!(
                "paste the full URL from the browser's address bar, starting with {}",
                self.callback.redirect_uri()
            )
        })?;
        parse_grant(&url, &self.state)
    }

    pub async fn finish(self, grant: Grant) -> Result<GatewayCredential> {
        let http = reqwest::Client::new();
        let redirect = self.callback.redirect_uri();
        let (form, previous): (Vec<(&str, &str)>, Option<&str>) = match &grant {
            Grant::Token(t) => (
                vec![("grant_type", "refresh_token"), ("refresh_token", t)],
                Some(t),
            ),
            Grant::Code(c) => (
                vec![
                    ("grant_type", "authorization_code"),
                    ("code", c),
                    ("redirect_uri", &redirect),
                ],
                None,
            ),
        };
        let token = exchange(&http, &self.config, form).await?;
        credential(self.config.clone(), token, previous)
    }
}

fn parse_grant(url: &Url, expected_state: &str) -> Result<Grant> {
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
            .filter(|v| !v.is_empty())
    };
    if let Some(error) = param("error") {
        bail!("the gateway sign-in failed: {error}");
    }
    if param("state").as_deref() != Some(expected_state) {
        bail!("the sign-in response did not match this login (state mismatch)");
    }
    if let Some(token) = param("token").or_else(|| param("refresh_token")) {
        return Ok(Grant::Token(token));
    }
    param("code")
        .map(Grant::Code)
        .context("the sign-in response had neither a token nor a code")
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

async fn exchange(
    http: &reqwest::Client,
    config: &GatewayConfig,
    mut form: Vec<(&str, &str)>,
) -> Result<TokenReply> {
    if let Some(id) = &config.client_id {
        form.push(("client_id", id));
    }
    let response = http
        .post(&config.token_url)
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .with_context(|| format!("could not reach {}", config.token_url))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{} returned {status}: {body}", config.token_url);
    }
    serde_json::from_str(&body).context("the gateway's token reply is not valid")
}

/// Without an `expires_in`, assume this many seconds.
const DEFAULT_LIFETIME: u64 = 3600;

fn credential(
    config: GatewayConfig,
    token: TokenReply,
    previous: Option<&str>,
) -> Result<GatewayCredential> {
    let lifetime = token.expires_in.unwrap_or(DEFAULT_LIFETIME).max(1);
    let refresh_token = token
        .refresh_token
        .or(previous.map(str::to_string))
        .context("the gateway gave no refresh token, so calx could not keep the sign-in")?;
    let now = now_secs();
    Ok(GatewayCredential {
        config,
        access_token: token.access_token,
        refresh_token,
        expires_at: now + lifetime,
        refresh_at: now + lifetime - refresh_margin(lifetime),
    })
}

/// How long before expiry to exchange: a quarter of the lifetime, at most
/// three minutes, so short-lived tokens still get a margin.
fn refresh_margin(lifetime: u64) -> u64 {
    (lifetime / 4).min(180)
}

/// Hands out the gateway's current access token.
pub struct GatewayAuth {
    pub name: String,
    store: CredentialStore,
    credential: Mutex<GatewayCredential>,
    http: reqwest::Client,
}

impl GatewayAuth {
    /// Loads the sign-in saved under `name` and starts the background task
    /// that keeps it fresh. Must run inside a Tokio runtime.
    pub fn load(name: &str, store: CredentialStore) -> Result<Arc<Self>> {
        let credential = store.gateway(name)?.with_context(|| {
            format!("not signed in to the gateway {name}; run `calx gateway login {name}`")
        })?;
        let auth = Arc::new(Self {
            name: name.to_string(),
            store,
            credential: Mutex::new(credential),
            http: reqwest::Client::new(),
        });
        tokio::spawn(keep_fresh(Arc::downgrade(&auth)));
        Ok(auth)
    }

    pub async fn access_token(&self) -> Result<String> {
        let mut credential = self.credential.lock().await;
        if now_secs() >= credential.refresh_at {
            self.refresh_locked(&mut credential).await?;
        }
        Ok(credential.access_token.clone())
    }

    /// Exchanges now, for when the gateway rejected a token that looked valid.
    pub async fn refresh(&self) -> Result<()> {
        let mut credential = self.credential.lock().await;
        self.refresh_locked(&mut credential).await
    }

    async fn refresh_locked(&self, credential: &mut GatewayCredential) -> Result<()> {
        let previous = credential.refresh_token.clone();
        let form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", previous.as_str()),
        ];
        let token = exchange(&self.http, &credential.config, form)
            .await
            .with_context(|| {
                format!(
                    "could not refresh the sign-in to {}; run `calx gateway login {}` again",
                    self.name, self.name
                )
            })?;
        *credential = self::credential(credential.config.clone(), token, Some(&previous))?;
        self.store.set_gateway(&self.name, Some(credential))
    }

    async fn refresh_at(&self) -> u64 {
        self.credential.lock().await.refresh_at
    }
}

/// Exchanges the token whenever it comes due, until the auth is dropped. A
/// failed exchange is retried a minute later; meanwhile a request that
/// finds the token stale tries again itself and reports the error.
async fn keep_fresh(auth: std::sync::Weak<GatewayAuth>) {
    loop {
        let Some(due) = (match auth.upgrade() {
            Some(a) => Some(a.refresh_at().await),
            None => None,
        }) else {
            return;
        };
        let wait = due.saturating_sub(now_secs());
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let Some(auth) = auth.upgrade() else { return };
        let mut credential = auth.credential.lock().await;
        // A request may have refreshed it while this task slept.
        if now_secs() >= credential.refresh_at
            && auth.refresh_locked(&mut credential).await.is_err()
        {
            drop(credential);
            drop(auth);
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callback_brings_a_token_or_a_code_with_the_right_state() {
        let url =
            |q: &str| Url::parse(&format!("http://127.0.0.1:1456/auth/callback?{q}")).unwrap();
        assert_eq!(
            parse_grant(&url("state=s&token=t1"), "s").unwrap(),
            Grant::Token("t1".into())
        );
        assert_eq!(
            parse_grant(&url("code=c1&state=s"), "s").unwrap(),
            Grant::Code("c1".into())
        );
        assert!(parse_grant(&url("state=other&token=t1"), "s").is_err());
        assert!(parse_grant(&url("state=s"), "s").is_err());
        assert!(
            parse_grant(&url("state=s&error=denied"), "s")
                .unwrap_err()
                .to_string()
                .contains("denied")
        );
    }

    #[test]
    fn short_tokens_refresh_with_a_margin_and_keep_an_unrotated_credential() {
        assert_eq!(refresh_margin(8), 2);
        assert_eq!(refresh_margin(3600), 180);
        let config = GatewayConfig {
            start_url: "https://gw/start".into(),
            token_url: "https://gw/token".into(),
            client_id: None,
            port: DEFAULT_PORT,
        };
        let reply = TokenReply {
            access_token: "a".into(),
            refresh_token: None,
            expires_in: Some(100),
        };
        let c = credential(config, reply, Some("cred")).unwrap();
        assert_eq!(c.refresh_token, "cred");
        assert_eq!(c.expires_at - c.refresh_at, 25);
    }
}
