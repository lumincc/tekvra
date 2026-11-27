//! Secret-redacted credentials, ChatGPT/Codex OAuth flows, and multi-account quota failover.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::ProviderError;

/// Secret bearer/API token whose `Debug` implementation always redacts its value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretToken(String);

impl SecretToken {
    /// Wraps a raw credential string.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Exposes the raw secret strictly for outbound Authorization header construction.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken([REDACTED])")
    }
}

/// Stored OAuth credential record (including ChatGPT/Codex subscription metadata).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredOAuthAccount {
    /// Provider id (`"openai"`, `"github-copilot"`, etc.).
    pub provider: String,
    /// Account alias (`"default"`, `"work"`, etc.).
    pub account_name: String,
    /// Extracted provider account/organization identifier (`chatgpt_account_id` or `sub`).
    pub account_id: Option<String>,
    /// Subscription plan tier (`"plus"`, `"pro"`, `"team"`, `"enterprise"`).
    pub plan_type: Option<String>,
    /// Access token.
    pub access_token: SecretToken,
    /// Optional refresh token.
    pub refresh_token: Option<SecretToken>,
    /// Unix timestamp (seconds) when `access_token` expires.
    pub expires_at_epoch_secs: Option<i64>,
    /// Reported remaining quota ratio in `[0.0, 1.0]`.
    #[serde(default = "default_full_quota")]
    pub remaining_quota_ratio: f64,
}

const fn default_full_quota() -> f64 {
    1.0
}

/// Persistent credential store at `<state_dir>/credentials.json` (`0600` on Unix).
#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
    accounts: Vec<StoredOAuthAccount>,
}

impl CredentialStore {
    /// Loads stored credentials from `state_dir/credentials.json` (empty if absent).
    ///
    /// # Errors
    /// Returns [`ProviderError`] if an existing credentials file cannot be read or parsed.
    pub fn load(state_dir: &Path) -> Result<Self, ProviderError> {
        let path = state_dir.join("credentials.json");
        if !path.exists() {
            return Ok(Self {
                path,
                accounts: Vec::new(),
            });
        }
        let raw = fs::read_to_string(&path).map_err(|e| ProviderError::CredentialIo {
            message: format!("{}: {e}", path.display()),
        })?;
        let accounts: Vec<StoredOAuthAccount> =
            serde_json::from_str(&raw).map_err(|e| ProviderError::CredentialIo {
                message: format!("invalid JSON in {}: {e}", path.display()),
            })?;
        Ok(Self { path, accounts })
    }

    /// Inserts or updates an OAuth account and writes `credentials.json` atomically (`0600`).
    ///
    /// # Errors
    /// Returns [`ProviderError`] if writing the credential store fails.
    pub fn upsert(&mut self, record: StoredOAuthAccount) -> Result<(), ProviderError> {
        if let Some(existing) = self
            .accounts
            .iter_mut()
            .find(|a| a.provider == record.provider && a.account_name == record.account_name)
        {
            *existing = record;
        } else {
            self.accounts.push(record);
        }
        self.persist()
    }

    /// Removes a stored account and persists changes.
    ///
    /// # Errors
    /// Returns [`ProviderError`] if writing the credential store fails.
    pub fn remove(&mut self, provider: &str, account_name: &str) -> Result<bool, ProviderError> {
        let before = self.accounts.len();
        self.accounts
            .retain(|a| !(a.provider == provider && a.account_name == account_name));
        let changed = self.accounts.len() != before;
        if changed {
            self.persist()?;
        }
        Ok(changed)
    }

    /// Returns stored accounts for `provider`.
    #[must_use]
    pub fn list_for_provider(&self, provider: &str) -> Vec<&StoredOAuthAccount> {
        self.accounts
            .iter()
            .filter(|a| a.provider == provider)
            .collect()
    }

    /// Returns all stored OAuth and API-key credential records.
    #[must_use]
    pub fn all_accounts(&self) -> &[StoredOAuthAccount] {
        &self.accounts
    }

    fn persist(&self) -> Result<(), ProviderError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|e| ProviderError::CredentialIo {
            message: format!("{}: {e}", parent.display()),
        })?;
        let tmp = NamedTempFile::new_in(parent).map_err(|e| ProviderError::CredentialIo {
            message: e.to_string(),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(tmp.path(), perms).map_err(|e| ProviderError::CredentialIo {
                message: e.to_string(),
            })?;
        }
        let body =
            serde_json::to_vec_pretty(&self.accounts).map_err(|e| ProviderError::CredentialIo {
                message: e.to_string(),
            })?;
        fs::write(tmp.path(), body).map_err(|e| ProviderError::CredentialIo {
            message: e.to_string(),
        })?;
        tmp.persist(&self.path)
            .map_err(|e| ProviderError::CredentialIo {
                message: e.error.to_string(),
            })?;
        Ok(())
    }
}

/// Extracts `chatgpt_account_id` (from OpenAI JWT claims) or `sub`, plus subscription `plan_type`,
/// from an OAuth JWT `access_token` or `id_token`.
#[must_use]
pub fn extract_jwt_account_metadata(jwt: &str) -> (Option<String>, Option<String>) {
    let mut parts = jwt.split('.');
    let _header = parts.next();
    let Some(payload_b64) = parts.next() else {
        return (None, None);
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload_b64))
    else {
        return (None, None);
    };
    let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (None, None);
    };

    let auth_ns = claims.get("https://api.openai.com/auth");
    let account_id = auth_ns
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            claims
                .get("chatgpt_account_id")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| claims.get("sub").and_then(serde_json::Value::as_str))
        .map(str::to_string);

    let plan_type = auth_ns
        .and_then(|v| v.get("chatgpt_plan_type"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| claims.get("plan_type").and_then(serde_json::Value::as_str))
        .map(str::to_string);

    (account_id, plan_type)
}

/// Generates a deterministic/random PKCE `(code_verifier, code_challenge_s256)` pair.
#[must_use]
pub fn generate_pkce_pair() -> (String, String) {
    let verifier = format!(
        "{}{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

/// Loopback OAuth session waiting for the browser redirect on `127.0.0.1:<port>/callback`.
pub struct LoopbackOAuthListener {
    listener: TcpListener,
    redirect_uri: String,
    expected_state: String,
}

impl std::fmt::Debug for LoopbackOAuthListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoopbackOAuthListener")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl LoopbackOAuthListener {
    /// Binds an ephemeral local loopback listener on `127.0.0.1:0` and returns the listener
    /// together with the formatted authorization URL.
    ///
    /// # Errors
    /// Returns [`ProviderError`] if binding the local socket or building the URL fails.
    pub async fn bind(
        authorize_endpoint: &str,
        client_id: &str,
        code_challenge: &str,
        scope: &str,
    ) -> Result<(Self, String), ProviderError> {
        let listener =
            TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| ProviderError::OAuth {
                    message: format!("failed to bind loopback OAuth socket: {e}"),
                })?;
        let port = listener
            .local_addr()
            .map_err(|e| ProviderError::OAuth {
                message: e.to_string(),
            })?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
        let expected_state = uuid::Uuid::now_v7().simple().to_string();

        let mut auth_url = Url::parse(authorize_endpoint).map_err(|e| ProviderError::OAuth {
            message: format!("invalid authorize URL `{authorize_endpoint}`: {e}"),
        })?;
        auth_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", scope)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &expected_state);

        let url_str = auth_url.to_string();
        Ok((
            Self {
                listener,
                redirect_uri,
                expected_state,
            },
            url_str,
        ))
    }

    /// Redirect URI registered for token exchange.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Waits for a single HTTP GET callback and extracts the authorization `code`, verifying `state`.
    ///
    /// # Errors
    /// Returns [`ProviderError`] if the callback fails or carries a mismatched OAuth `state`.
    pub async fn accept_authorization_code(self) -> Result<String, ProviderError> {
        let (mut stream, _) = self
            .listener
            .accept()
            .await
            .map_err(|e| ProviderError::OAuth {
                message: format!("loopback accept failed: {e}"),
            })?;
        let mut buf = vec![0_u8; 4096];
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| ProviderError::OAuth {
                message: e.to_string(),
            })?;
        let req_text = String::from_utf8_lossy(&buf[..n]);
        let request_line = req_text.lines().next().unwrap_or_default();
        let path_part = request_line.split_whitespace().nth(1).unwrap_or("/");
        let full_url = format!("http://127.0.0.1{path_part}");
        let parsed = Url::parse(&full_url).map_err(|e| ProviderError::OAuth {
            message: format!("malformed callback URL: {e}"),
        })?;

        let params: BTreeMap<String, String> = parsed.query_pairs().into_owned().collect();
        if params.get("state").map(String::as_str) != Some(self.expected_state.as_str()) {
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 14\r\n\r\nState mismatch")
                .await;
            return Err(ProviderError::OAuth {
                message: "OAuth callback state mismatch".to_string(),
            });
        }
        let Some(code) = params.get("code").cloned() else {
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 12\r\n\r\nMissing code")
                .await;
            return Err(ProviderError::OAuth {
                message: "OAuth callback missing `code` parameter".to_string(),
            });
        };

        let html = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 31\r\n\r\nTekvra sign-in complete. Close.";
        let _ = stream.write_all(html.as_bytes()).await;
        Ok(code)
    }
}

/// Device-code authorization challenge returned by an OAuth server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCodeSession {
    /// Device code polled at the token endpoint.
    pub device_code: String,
    /// Short code displayed to the user.
    pub user_code: String,
    /// Verification URL opened by the user.
    pub verification_uri: String,
    /// Recommended poll interval in seconds.
    #[serde(default = "default_poll_interval")]
    pub interval: u64,
}

const fn default_poll_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenEndpointResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Exchanges an authorization code (or refresh token / device code) against `token_endpoint`
/// and constructs a [`StoredOAuthAccount`].
///
/// # Errors
/// Returns [`ProviderError`] if the HTTP exchange fails or returns a non-success response.
pub async fn exchange_oauth_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    provider: &str,
    account_name: &str,
    form_fields: &[(&str, &str)],
    now_epoch_secs: i64,
) -> Result<StoredOAuthAccount, ProviderError> {
    let resp = http
        .post(token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(encode_form_urlencoded(form_fields))
        .send()
        .await
        .map_err(|e| ProviderError::Transport {
            provider: provider.to_string(),
            message: e.to_string(),
        })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::OAuth {
            message: format!("token endpoint HTTP {status}: {body}"),
        });
    }

    let payload: TokenEndpointResponse = resp.json().await.map_err(|e| ProviderError::OAuth {
        message: format!("invalid token response JSON: {e}"),
    })?;

    let token_for_claims = payload
        .id_token
        .as_deref()
        .unwrap_or(payload.access_token.as_str());
    let (account_id, plan_type) = extract_jwt_account_metadata(token_for_claims);
    let expires_at_epoch_secs = payload.expires_in.map(|ttl| now_epoch_secs + ttl);

    Ok(StoredOAuthAccount {
        provider: provider.to_string(),
        account_name: account_name.to_string(),
        account_id,
        plan_type,
        access_token: SecretToken::new(payload.access_token),
        refresh_token: payload.refresh_token.map(SecretToken::new),
        expires_at_epoch_secs,
        remaining_quota_ratio: 1.0,
    })
}

/// OAuth 2.0 endpoint set for PKCE browser loopback, RFC 8628 device-code, and token refresh flows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthProviderEndpoints {
    /// Browser authorization endpoint.
    pub authorize_url: String,
    /// RFC 8628 device-authorization endpoint.
    pub device_code_url: String,
    /// Token exchange and refresh endpoint.
    pub token_url: String,
    /// Public OAuth client identifier.
    pub client_id: String,
    /// Space-delimited OAuth scopes.
    pub scope: String,
}

impl OAuthProviderEndpoints {
    /// Resolves built-in or environment-overridden (`TEKVRA_OAUTH_*`) OAuth endpoints for `provider`.
    ///
    /// # Errors
    /// Returns [`ProviderError::OAuth`] if `provider` has no OAuth defaults and `TEKVRA_OAUTH_TOKEN_URL` is unset.
    pub fn for_provider(provider: &str) -> Result<Self, ProviderError> {
        let env_opt = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        match provider {
            "openai" => Ok(Self {
                authorize_url: env_opt("TEKVRA_OAUTH_AUTHORIZE_URL")
                    .unwrap_or_else(|| "https://auth.openai.com/oauth/authorize".to_string()),
                device_code_url: env_opt("TEKVRA_OAUTH_DEVICE_URL")
                    .unwrap_or_else(|| "https://auth.openai.com/oauth/device/code".to_string()),
                token_url: env_opt("TEKVRA_OAUTH_TOKEN_URL")
                    .unwrap_or_else(|| "https://auth.openai.com/oauth/token".to_string()),
                client_id: env_opt("TEKVRA_OAUTH_CLIENT_ID")
                    .unwrap_or_else(|| "app_EMoamEEZ73f0CkXaXp7hrann".to_string()),
                scope: env_opt("TEKVRA_OAUTH_SCOPE")
                    .unwrap_or_else(|| "openid profile email offline_access".to_string()),
            }),
            "github-copilot" => Ok(Self {
                authorize_url: env_opt("TEKVRA_OAUTH_AUTHORIZE_URL")
                    .unwrap_or_else(|| "https://github.com/login/oauth/authorize".to_string()),
                device_code_url: env_opt("TEKVRA_OAUTH_DEVICE_URL")
                    .unwrap_or_else(|| "https://github.com/login/device/code".to_string()),
                token_url: env_opt("TEKVRA_OAUTH_TOKEN_URL")
                    .unwrap_or_else(|| "https://github.com/login/oauth/access_token".to_string()),
                client_id: env_opt("TEKVRA_OAUTH_CLIENT_ID")
                    .unwrap_or_else(|| "Iv1.b507a08c87ecfe98".to_string()),
                scope: env_opt("TEKVRA_OAUTH_SCOPE").unwrap_or_else(|| "read:user".to_string()),
            }),
            other => {
                let Some(token_url) = env_opt("TEKVRA_OAUTH_TOKEN_URL") else {
                    return Err(ProviderError::OAuth {
                        message: format!(
                            "provider `{other}` has no default OAuth endpoints; pass `--api-key` or set `TEKVRA_OAUTH_*`"
                        ),
                    });
                };
                Ok(Self {
                    authorize_url: env_opt("TEKVRA_OAUTH_AUTHORIZE_URL")
                        .unwrap_or_else(|| token_url.clone()),
                    device_code_url: env_opt("TEKVRA_OAUTH_DEVICE_URL")
                        .unwrap_or_else(|| token_url.clone()),
                    token_url,
                    client_id: env_opt("TEKVRA_OAUTH_CLIENT_ID")
                        .unwrap_or_else(|| "tekvra-cli".to_string()),
                    scope: env_opt("TEKVRA_OAUTH_SCOPE")
                        .unwrap_or_else(|| "openid profile".to_string()),
                })
            }
        }
    }
}

/// Builds an HTTP client for OAuth requests, bypassing system proxies when targeting loopback hosts.
///
/// # Errors
/// Returns [`ProviderError::OAuth`] if constructing the `reqwest::Client` fails.
pub fn build_oauth_http_client(target_url: &str) -> Result<reqwest::Client, ProviderError> {
    let is_loopback = Url::parse(target_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|h| matches!(h.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1"));

    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20));
    if is_loopback {
        builder = builder.no_proxy();
    }
    builder.build().map_err(|e| ProviderError::OAuth {
        message: format!("failed building OAuth HTTP client: {e}"),
    })
}

/// Initiates an RFC 8628 device-code authorization session.
///
/// # Errors
/// Returns [`ProviderError`] on transport or non-success HTTP status.
pub async fn request_device_code(
    http: &reqwest::Client,
    device_code_endpoint: &str,
    client_id: &str,
    scope: &str,
) -> Result<DeviceCodeSession, ProviderError> {
    let resp = http
        .post(device_code_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(encode_form_urlencoded(&[
            ("client_id", client_id),
            ("scope", scope),
        ]))
        .send()
        .await
        .map_err(|e| ProviderError::OAuth {
            message: format!("device-code request failed: {e}"),
        })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::OAuth {
            message: format!("device-code endpoint HTTP {status}: {body}"),
        });
    }

    resp.json::<DeviceCodeSession>()
        .await
        .map_err(|e| ProviderError::OAuth {
            message: format!("invalid device-code JSON response: {e}"),
        })
}

/// Polls `token_endpoint` for an RFC 8628 device-code grant, honoring `authorization_pending`
/// and `slow_down` backoff until tokens are issued or `max_attempts` is exhausted.
///
/// # Errors
/// Returns [`ProviderError::OAuth`] if authorization is denied, expires, or exceeds `max_attempts`.
pub async fn poll_device_code_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    provider: &str,
    account_name: &str,
    client_id: &str,
    session: &DeviceCodeSession,
    max_attempts: u32,
) -> Result<StoredOAuthAccount, ProviderError> {
    let mut delay = if session.interval == 0 {
        std::time::Duration::from_millis(15)
    } else {
        std::time::Duration::from_secs(session.interval)
    };
    let slow_down_step = if session.interval == 0 {
        std::time::Duration::from_millis(20)
    } else {
        std::time::Duration::from_secs(5)
    };

    for _attempt in 0..max_attempts.max(1) {
        tokio::time::sleep(delay).await;

        let resp = http
            .post(token_endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(encode_form_urlencoded(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &session.device_code),
                ("client_id", client_id),
            ]))
            .send()
            .await
            .map_err(|e| ProviderError::Transport {
                provider: provider.to_string(),
                message: e.to_string(),
            })?;

        let status = resp.status();
        let raw_body = resp.text().await.unwrap_or_default();
        let json_body: serde_json::Value =
            serde_json::from_str(&raw_body).map_err(|e| ProviderError::OAuth {
                message: format!("invalid device-token JSON (HTTP {}): {e}", status.as_u16()),
            })?;

        if let Some(err_code) = json_body.get("error").and_then(serde_json::Value::as_str) {
            match err_code {
                "authorization_pending" => continue,
                "slow_down" => {
                    delay = delay.saturating_add(slow_down_step);
                    continue;
                }
                other => {
                    let desc = json_body
                        .get("error_description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    return Err(ProviderError::OAuth {
                        message: format!("device-code authorization failed (`{other}`): {desc}"),
                    });
                }
            }
        }

        if !status.is_success() {
            return Err(ProviderError::OAuth {
                message: format!("device-code token HTTP {}: {raw_body}", status.as_u16()),
            });
        }

        let payload: TokenEndpointResponse =
            serde_json::from_value(json_body).map_err(|e| ProviderError::OAuth {
                message: format!("malformed OAuth token payload: {e}"),
            })?;

        let now_secs = current_epoch_secs();
        let token_for_claims = payload
            .id_token
            .as_deref()
            .unwrap_or(payload.access_token.as_str());
        let (account_id, plan_type) = extract_jwt_account_metadata(token_for_claims);

        return Ok(StoredOAuthAccount {
            provider: provider.to_string(),
            account_name: account_name.to_string(),
            account_id,
            plan_type,
            access_token: SecretToken::new(payload.access_token),
            refresh_token: payload.refresh_token.map(SecretToken::new),
            expires_at_epoch_secs: payload.expires_in.map(|ttl| now_secs + ttl),
            remaining_quota_ratio: 1.0,
        });
    }

    Err(ProviderError::OAuth {
        message: format!(
            "timed out waiting for device-code authorization after {max_attempts} poll attempts"
        ),
    })
}

/// Refreshes an expired (or expiring) [`StoredOAuthAccount`] using its `refresh_token`.
///
/// # Errors
/// Returns [`ProviderError`] if `refresh_token` is absent or the token endpoint rejects the refresh.
pub async fn refresh_oauth_account(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    existing: &StoredOAuthAccount,
    now_epoch_secs: i64,
) -> Result<StoredOAuthAccount, ProviderError> {
    let Some(refresh_tok) = &existing.refresh_token else {
        return Err(ProviderError::OAuth {
            message: format!(
                "stored account `{}:{}` has no `refresh_token`",
                existing.provider, existing.account_name
            ),
        });
    };

    let mut refreshed = exchange_oauth_token(
        http,
        token_endpoint,
        &existing.provider,
        &existing.account_name,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_tok.expose()),
            ("client_id", client_id),
        ],
        now_epoch_secs,
    )
    .await?;

    if refreshed.refresh_token.is_none() {
        refreshed.refresh_token = Some(refresh_tok.clone());
    }
    if refreshed.account_id.is_none() {
        refreshed.account_id.clone_from(&existing.account_id);
    }
    if refreshed.plan_type.is_none() {
        refreshed.plan_type.clone_from(&existing.plan_type);
    }
    Ok(refreshed)
}

fn current_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn encode_form_urlencoded(pairs: &[(&str, &str)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

/// Optional CLI `--account PROVIDER:NAME` pin disabling cross-account rotation for that provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountPin {
    /// Provider slug.
    pub provider: String,
    /// Pinned account alias.
    pub account_name: String,
}

impl std::str::FromStr for AccountPin {
    type Err = ProviderError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((p, n)) = s.split_once(':') else {
            return Err(ProviderError::InvalidAccountPin { raw: s.to_string() });
        };
        if p.trim().is_empty() || n.trim().is_empty() {
            return Err(ProviderError::InvalidAccountPin { raw: s.to_string() });
        }
        Ok(Self {
            provider: p.trim().to_string(),
            account_name: n.trim().to_string(),
        })
    }
}

/// Resolved runtime credential candidate in an [`AccountPool`].
#[derive(Debug, Clone)]
pub struct AccountCandidate {
    /// Provider slug.
    pub provider: String,
    /// Account alias (`"primary"`, `"secondary"`, `"oauth"`, etc.).
    pub name: String,
    /// Bearer or API token.
    pub token: SecretToken,
    /// Optional ChatGPT/Codex organization/account header value.
    pub account_id: Option<String>,
    /// Remaining quota fraction in `[0.0, 1.0]`.
    pub remaining_quota_ratio: f64,
    /// Whether this account is currently marked exhausted/rate-limited.
    pub exhausted: bool,
}

/// Multi-account credential pool with quota-threshold tracking and automatic failover.
#[derive(Debug, Clone, Default)]
pub struct AccountPool {
    by_provider: BTreeMap<String, Vec<AccountCandidate>>,
    quota_floor: f64,
    pin: Option<AccountPin>,
}

impl AccountPool {
    /// Creates an empty pool with a minimum remaining quota threshold (e.g. `0.02` = 2%).
    #[must_use]
    pub fn new(quota_floor: f64) -> Self {
        Self {
            by_provider: BTreeMap::new(),
            quota_floor: quota_floor.clamp(0.0, 1.0),
            pin: None,
        }
    }

    /// Public `(provider, account, remaining_ratio, exhausted)` rows for diagnostics
    /// surfaces such as the app-server `account/rateLimits/read` RPC (no tokens).
    #[must_use]
    pub fn summary_rows(&self) -> Vec<(String, String, f64, bool)> {
        self.by_provider
            .iter()
            .flat_map(|(provider, accounts)| {
                accounts.iter().map(|candidate| {
                    (
                        provider.clone(),
                        candidate.name.clone(),
                        candidate.remaining_quota_ratio,
                        candidate.exhausted,
                    )
                })
            })
            .collect()
    }

    /// Pins selection for a provider (`--account PROVIDER:NAME`), disabling failover onto other accounts.
    pub fn set_pin(&mut self, pin: Option<AccountPin>) {
        self.pin = pin;
    }

    /// Registers an account candidate for `provider`.
    pub fn add_account(&mut self, candidate: AccountCandidate) {
        self.by_provider
            .entry(candidate.provider.clone())
            .or_default()
            .push(candidate);
    }

    /// Updates remaining quota or exhaustion state after a provider response.
    pub fn record_quota_state(
        &mut self,
        provider: &str,
        account_name: &str,
        remaining_ratio: Option<f64>,
        exhausted: bool,
    ) {
        if let Some(list) = self.by_provider.get_mut(provider)
            && let Some(acct) = list.iter_mut().find(|a| a.name == account_name)
        {
            if let Some(r) = remaining_ratio {
                acct.remaining_quota_ratio = r.clamp(0.0, 1.0);
            }
            acct.exhausted = exhausted || acct.remaining_quota_ratio <= self.quota_floor;
        }
    }

    /// Selects the active healthy account for `provider`, automatically failing over when an earlier
    /// account is exhausted unless `--account PROVIDER:NAME` pinned a specific account.
    ///
    /// # Errors
    /// Returns [`ProviderError::NoEligibleAccount`] when no healthy (or pinned) account is available.
    pub fn select(&self, provider: &str) -> Result<&AccountCandidate, ProviderError> {
        let list =
            self.by_provider
                .get(provider)
                .ok_or_else(|| ProviderError::NoEligibleAccount {
                    provider: provider.to_string(),
                    reason: "no credentials configured".to_string(),
                })?;

        if let Some(pin) = &self.pin
            && pin.provider == provider
        {
            let acct = list
                .iter()
                .find(|a| a.name == pin.account_name)
                .ok_or_else(|| ProviderError::NoEligibleAccount {
                    provider: provider.to_string(),
                    reason: format!("pinned account `{}` not found", pin.account_name),
                })?;
            if acct.exhausted || acct.remaining_quota_ratio <= self.quota_floor {
                return Err(ProviderError::NoEligibleAccount {
                    provider: provider.to_string(),
                    reason: format!(
                        "pinned account `{}` has exhausted its quota window",
                        pin.account_name
                    ),
                });
            }
            return Ok(acct);
        }

        list.iter()
            .find(|a| !a.exhausted && a.remaining_quota_ratio > self.quota_floor)
            .ok_or_else(|| ProviderError::NoEligibleAccount {
                provider: provider.to_string(),
                reason: "all configured accounts are quota-exhausted".to_string(),
            })
    }
}
