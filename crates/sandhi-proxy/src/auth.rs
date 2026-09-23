//! TD-0029: one OIDC identity boundary for dashboard and agent access.
use crate::{operator::constant_time_eq, ProxyState};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use openidconnect::{
    core::{CoreClient, CoreJsonWebKeySet, CoreProviderMetadata, CoreResponseType},
    AccessTokenHash, AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const LOGIN_COOKIE: &str = "__Host-sandhi-login";
const SESSION_COOKIE: &str = "__Host-sandhi-session";
const MAX_LOGINS: usize = 1024;
const MAX_SESSIONS: usize = 4096;
const MAX_TOKEN_BYTES: usize = 16384;
const MAX_RESPONSE_BYTES: usize = 262144;
const LOGIN_TTL: Duration = Duration::from_secs(300);
const SESSION_TTL: i64 = 3600;
type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Read,
    Operate,
    Admin,
    Diagnostics,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}
impl Role {
    fn permits(self, p: Permission) -> bool {
        match self {
            Self::Admin => true,
            Self::Operator => matches!(p, Permission::Read | Permission::Operate),
            Self::Viewer => p == Permission::Read,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub upstream: String,
    pub models: Vec<String>,
    pub group: Option<String>,
    pub budget_scope: Option<String>,
    pub rate_limit_per_min: Option<u32>,
}
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub role: Option<Role>,
    #[serde(default)]
    pub allow_diagnostics: bool,
    #[serde(default)]
    pub grants: HashMap<String, Grant>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub issuer: String,
    pub client_id: String,
    pub redirect_url: String,
    pub ca_file: Option<PathBuf>,
    pub client_secret_env: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub subjects: HashMap<String, Binding>,
}
impl Config {
    pub fn validate(&self) -> Result<(), String> {
        let issuer = secure_url(&self.issuer)?;
        let redirect = secure_url(&self.redirect_url)?;
        if issuer.query().is_some()
            || issuer.fragment().is_some()
            || self.client_id.trim().is_empty()
            || redirect.path() != "/auth/callback"
            || redirect.query().is_some()
            || redirect.fragment().is_some()
        {
            return Err("invalid OIDC issuer, client ID or exact /auth/callback URL".into());
        }
        for (subject, binding) in &self.subjects {
            if subject.is_empty()
                || (binding.role.is_none()
                    && binding.grants.is_empty()
                    && !binding.allow_diagnostics)
            {
                return Err("each nonempty OIDC subject needs a role, inference grant or diagnostics permission".into());
            }
            for (name, g) in &binding.grants {
                if name.is_empty()
                    || name.len() > 128
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    || g.upstream.is_empty()
                    || g.models.is_empty()
                    || g.models.iter().any(|m| m.trim().is_empty())
                {
                    return Err("inference grants need an unambiguous name, upstream and nonempty model allowlist".into());
                }
            }
        }
        Ok(())
    }
    pub fn permits(&self, subject: &str, p: Permission) -> bool {
        self.subjects.get(subject).is_some_and(|b| {
            b.role.is_some_and(|r| r.permits(p))
                || (p == Permission::Diagnostics && b.allow_diagnostics)
        })
    }
    fn grant(&self, subject: &str, selector: Option<&str>) -> Option<sandhi_core::VirtualKey> {
        let grants = &self.subjects.get(subject)?.grants;
        let (name, g) = match selector {
            Some(name) => (name, grants.get(name)?),
            None if grants.len() == 1 => {
                let (n, g) = grants.iter().next()?;
                (n.as_str(), g)
            }
            _ => return None,
        };
        // One derivation reused by attribution, rate limits, budgets and sessions. Never a bearer secret.
        let identity = serde_json::to_string(&(&self.issuer, subject, name)).ok()?;
        Some(sandhi_core::VirtualKey {
            id: format!("oidc_{}", sandhi_store::hash_secret(&identity)),
            subject_id: Some(subject.into()),
            group_id: g.group.clone(),
            upstream_ref: g.upstream.clone(),
            models: Some(g.models.clone()),
            budget_scope: g.budget_scope.clone(),
            rate_limit_per_min: g.rate_limit_per_min,
            expires_at: None,
        })
    }
}
fn secure_url(raw: &str) -> Result<reqwest::Url, String> {
    let u = reqwest::Url::parse(raw).map_err(|_| "invalid OIDC URL")?;
    if u.scheme() != "https"
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
    {
        return Err("OIDC URLs require HTTPS without embedded credentials".into());
    }
    Ok(u)
}
fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn random() -> String {
    CsrfToken::new_random().secret().to_owned()
}
fn digest(v: &str) -> String {
    sandhi_store::hash_secret(v)
}
fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}
fn unauthorized() -> Response {
    failure(
        StatusCode::UNAUTHORIZED,
        "OIDC authentication required or invalid",
    )
}
fn unavailable() -> Response {
    failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "OIDC authority unavailable",
    )
}

struct Pending {
    state: String,
    nonce: Nonce,
    verifier: PkceCodeVerifier,
    created: Instant,
}
#[derive(Clone)]
struct Session {
    subject: String,
    csrf: String,
    expires: i64,
}
struct Sessions {
    pending: HashMap<String, Pending>,
    active: HashMap<String, Session>,
}
impl Sessions {
    fn prune(&mut self) {
        self.pending.retain(|_, v| v.created.elapsed() < LOGIN_TTL);
        self.active.retain(|_, v| v.expires > unix_now());
    }
}
pub struct Oidc {
    pub config: Config,
    http: reqwest::Client,
    secret: Option<ClientSecret>,
    introspection_url: String,
    introspection_client_auth: bool,
    sessions: Mutex<Sessions>,
    permits: tokio::sync::Semaphore,
}
impl<'a> openidconnect::AsyncHttpClient<'a> for Oidc {
    type Error = std::io::Error;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<openidconnect::HttpResponse, std::io::Error>>
                + Send
                + 'a,
        >,
    >;
    fn call(&'a self, request: openidconnect::HttpRequest) -> Self::Future {
        Box::pin(self.fetch(request))
    }
}
impl Oidc {
    pub async fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|_| "cannot read SANDHI_OIDC_CONFIG")?;
        let config: Config =
            serde_json::from_slice(&data).map_err(|_| "invalid SANDHI_OIDC_CONFIG JSON")?;
        Self::new(config).await
    }
    pub async fn new(config: Config) -> Result<Self, String> {
        config.validate()?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // One total deadline includes DNS, connection, TLS and response reads.
            // A shorter connection cap rejected valid macOS .local DNS resolution.
            .timeout(Duration::from_secs(10));
        if let Some(path) = &config.ca_file {
            let pem = std::fs::read(path).map_err(|_| "cannot read OIDC public CA")?;
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(&pem).map_err(|_| "invalid OIDC public CA")?,
            );
        }
        let secret = config
            .client_secret_env
            .as_ref()
            .map(|name| {
                std::env::var(name)
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(ClientSecret::new)
                    .ok_or("OIDC client secret environment variable missing")
            })
            .transpose()?;
        let mut result = Self {
            introspection_url: String::new(),
            introspection_client_auth: false,
            config,
            http: builder
                .build()
                .map_err(|_| "cannot build OIDC HTTPS client")?,
            secret,
            sessions: Mutex::new(Sessions {
                pending: HashMap::new(),
                active: HashMap::new(),
            }),
            permits: tokio::sync::Semaphore::new(16),
        };
        let (_, url, client_auth) = result.discover().await?;
        result.introspection_url = url;
        result.introspection_client_auth = client_auth;
        Ok(result)
    }
    async fn fetch(
        &self,
        request: openidconnect::HttpRequest,
    ) -> Result<openidconnect::HttpResponse, std::io::Error> {
        use std::io::{Error, ErrorKind};
        let url = secure_url(&request.uri().to_string())
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "unsafe OIDC endpoint"))?;
        let origin = secure_url(&self.config.issuer)
            .map_err(|_| Error::other("invalid issuer"))?
            .origin();
        if url.origin() != origin {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "OIDC endpoint outside issuer origin",
            ));
        }
        let (parts, body) = request.into_parts();
        let mut response = self
            .http
            .request(parts.method, url)
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                // Endpoint query strings can contain secrets even without URL userinfo.
                // Omit the URL as well as request headers and bodies from diagnostics.
                let error = error.without_url();
                tracing::warn!(?error, "OIDC authority request failed");
                Error::other("OIDC HTTP request failed")
            })?;
        if response.status().is_server_error() || response.status() == StatusCode::TOO_MANY_REQUESTS
        {
            return Err(Error::other("OIDC authority unavailable"));
        }
        let mut builder = axum::http::Response::builder().status(response.status());
        *builder
            .headers_mut()
            .ok_or_else(|| Error::other("invalid HTTP response"))? = response.headers().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::other("OIDC response read failed"))?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(Error::other("OIDC response exceeds limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        builder
            .body(bytes)
            .map_err(|_| Error::other("invalid OIDC response"))
    }
    async fn discover(&self) -> Result<(Client, String, bool), String> {
        let issuer = IssuerUrl::new(self.config.issuer.clone()).map_err(|_| "invalid issuer")?;
        // Fetch once as JSON to validate extensions and same-origin endpoints before the library fetches JWKS.
        let endpoint = format!(
            "{}/.well-known/openid-configuration",
            self.config.issuer.trim_end_matches('/')
        );
        let request = axum::http::Request::builder()
            .uri(endpoint)
            .body(Vec::new())
            .map_err(|_| "invalid discovery request")?;
        let response = self
            .fetch(request)
            .await
            .map_err(|_| "OIDC discovery failed")?;
        if !response.status().is_success() {
            return Err("OIDC discovery rejected".into());
        }
        let metadata: Value =
            serde_json::from_slice(response.body()).map_err(|_| "invalid discovery JSON")?;
        let origin = secure_url(&self.config.issuer)?.origin();
        for name in [
            "authorization_endpoint",
            "token_endpoint",
            "jwks_uri",
            "introspection_endpoint",
        ] {
            let url = secure_url(metadata[name].as_str().ok_or("missing OIDC endpoint")?)?;
            if url.origin() != origin {
                return Err("OIDC endpoints must use issuer origin".into());
            }
        }
        if !metadata["code_challenge_methods_supported"]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v == "S256"))
        {
            return Err("OIDC client requires advertised S256 PKCE".into());
        }
        let introspection = metadata["introspection_endpoint"]
            .as_str()
            .ok_or("missing introspection endpoint")?
            .to_owned();
        // RFC 8414 provides no default for introspection authentication.
        // Require an explicit advertised method, including public/no-auth access.
        let methods = metadata
            .get("introspection_endpoint_auth_methods_supported")
            .ok_or("missing introspection authentication methods")?
            .as_array()
            .filter(|methods| methods.iter().all(Value::is_string))
            .ok_or("invalid introspection authentication methods")?;
        let client_auth = if self.secret.is_some()
            && methods.iter().any(|method| method == "client_secret_basic")
        {
            true
        } else if methods.iter().any(|method| method == "none") {
            false
        } else {
            return Err("no supported introspection authentication method".into());
        };
        let metadata: CoreProviderMetadata =
            serde_json::from_value(metadata).map_err(|_| "invalid OIDC metadata")?;
        if metadata.issuer() != &issuer {
            return Err("discovery issuer mismatch".into());
        }
        let request = axum::http::Request::builder()
            .uri(metadata.jwks_uri().as_str())
            .body(Vec::new())
            .map_err(|_| "invalid JWKS URL")?;
        let response = self.fetch(request).await.map_err(|_| "JWKS fetch failed")?;
        if !response.status().is_success() {
            return Err("JWKS request rejected".into());
        }
        let jwks: CoreJsonWebKeySet =
            serde_json::from_slice(response.body()).map_err(|_| "invalid JWKS")?;
        let metadata = metadata.set_jwks(jwks);
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.config.client_id.clone()),
            self.secret.clone(),
        )
        .set_redirect_uri(
            RedirectUrl::new(self.config.redirect_url.clone())
                .map_err(|_| "invalid redirect URL")?,
        );
        Ok((client, introspection, client_auth))
    }
    #[allow(clippy::result_large_err)] // Ready-to-return axum denial, same as other authorization gates.
    fn session(&self, headers: &HeaderMap) -> Result<Option<Session>, Response> {
        let Some(cookie) = cookie(headers, SESSION_COOKIE)? else {
            return Ok(None);
        };
        let mut sessions = self.sessions.lock().expect("OIDC sessions poisoned");
        sessions.prune();
        Ok(sessions.active.get(&digest(&cookie)).cloned())
    }
    #[allow(clippy::result_large_err)] // Ready-to-return axum denial; consistent with session().
    async fn bearer_subject(&self, token: &str) -> Result<String, Response> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return Err(unauthorized());
        }
        let _permit = self.permits.try_acquire().map_err(|_| unavailable())?;
        let endpoint = &self.introspection_url;
        // OAuth client auth, not the caller's token, authenticates confidential introspection.
        let mut request = self.http.post(endpoint).form(&[
            ("token", token),
            ("token_type_hint", "access_token"),
            ("client_id", &self.config.client_id),
        ]);
        if self.introspection_client_auth {
            let secret = self.secret.as_ref().ok_or_else(unavailable)?;
            // RFC 6749 §2.3.1: encode each component before HTTP Basic encoding.
            let encode = |value: &str| {
                openidconnect::url::form_urlencoded::byte_serialize(value.as_bytes())
                    .collect::<String>()
            };
            request = request.basic_auth(
                encode(&self.config.client_id),
                Some(encode(secret.secret())),
            );
        }
        let request = request.build().map_err(|_| unavailable())?;
        let mut wire = axum::http::Request::builder()
            .method(request.method().clone())
            .uri(request.url().as_str());
        *wire.headers_mut().ok_or_else(unavailable)? = request.headers().clone();
        let body = request
            .body()
            .and_then(|b| b.as_bytes())
            .ok_or_else(unavailable)?
            .to_vec();
        let response = self
            .fetch(wire.body(body).map_err(|_| unavailable())?)
            .await
            .map_err(|_| unavailable())?;
        if !response.status().is_success() {
            // Kanidm's advertised no-auth endpoint also returns 400/401 for
            // malformed/invalid-signature caller tokens. With client auth, a 401
            // instead denotes a gateway credential/configuration failure.
            if !self.introspection_client_auth
                && matches!(
                    response.status(),
                    StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED
                )
            {
                return Err(unauthorized());
            }
            return Err(unavailable());
        }
        let value: Value = serde_json::from_slice(response.body()).map_err(|_| unavailable())?;
        self.introspection_subject(&value).ok_or_else(unauthorized)
    }
    fn introspection_subject(&self, v: &Value) -> Option<String> {
        let aud = &v["aud"];
        let audience = aud.as_str() == Some(self.config.client_id.as_str())
            || aud.as_array().is_some_and(|a| {
                a.iter().all(Value::is_string) && a.iter().any(|a| a == &self.config.client_id)
            });
        let sub = v["sub"].as_str()?;
        if v["active"] != true
            || v["iss"].as_str() != Some(&self.config.issuer)
            || !audience
            || sub.is_empty()
            || v["exp"].as_i64()? <= unix_now()
            || !v["token_type"].as_str()?.eq_ignore_ascii_case("bearer")
            || (v.get("nbf").is_some() && v["nbf"].as_i64().map_or(true, |n| n > unix_now()))
        {
            return None;
        }
        Some(sub.into())
    }
    #[allow(clippy::result_large_err)] // Ready-to-return axum denial; consistent with session().
    async fn identity(&self, headers: &HeaderMap, mutation: bool) -> Result<String, Response> {
        let bearer = unique_header(headers, "authorization")?;
        let session = self.session(headers)?;
        if bearer.is_some() && cookie(headers, SESSION_COOKIE)?.is_some() {
            return Err(unauthorized());
        }
        if let Some(value) = bearer {
            let token = value.strip_prefix("Bearer ").ok_or_else(unauthorized)?;
            return self.bearer_subject(token).await;
        }
        let session = session.ok_or_else(unauthorized)?;
        if mutation {
            let origin = secure_url(&self.config.redirect_url)
                .map_err(|_| unavailable())?
                .origin()
                .ascii_serialization();
            if unique_header(headers, "origin")? != Some(origin.as_str())
                || !unique_header(headers, "x-sandhi-csrf")?
                    .is_some_and(|v| constant_time_eq(v.as_bytes(), session.csrf.as_bytes()))
            {
                return Err(failure(StatusCode::FORBIDDEN, "invalid session CSRF proof"));
            }
        }
        Ok(session.subject)
    }
    #[allow(clippy::result_large_err)] // Ready-to-return axum denial; consistent with session().
    pub async fn authorize(
        &self,
        headers: &HeaderMap,
        permission: Permission,
        mutation: bool,
    ) -> Result<(), Response> {
        let subject = self.identity(headers, mutation).await?;
        if self.config.permits(&subject, permission) {
            Ok(())
        } else {
            Err(failure(
                StatusCode::FORBIDDEN,
                "role does not permit this operation",
            ))
        }
    }
    #[allow(clippy::result_large_err)] // Ready-to-return axum denial; consistent with session().
    pub async fn inference(
        &self,
        token: &str,
        headers: &HeaderMap,
    ) -> Result<sandhi_core::VirtualKey, Response> {
        // A caller cannot select a different principal by sending competing dialect headers.
        let mut credentials = 0;
        for name in ["authorization", "x-api-key", "x-goog-api-key"] {
            if unique_header(headers, name)?.is_some() {
                credentials += 1;
            }
        }
        if credentials > 1 {
            return Err(unauthorized());
        }
        if cookie(headers, SESSION_COOKIE)?.is_some() {
            return Err(unauthorized());
        }
        let subject = self.bearer_subject(token).await?;
        self.config
            .grant(&subject, unique_header(headers, "x-sandhi-grant")?)
            .ok_or_else(|| failure(StatusCode::FORBIDDEN, "no matching inference grant"))
    }
}
#[allow(clippy::result_large_err)]
fn unique_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, Response> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .map(|v| v.to_str().map_err(|_| unauthorized()))
        .transpose()?;
    if values.next().is_some() {
        return Err(unauthorized());
    }
    Ok(value)
}
#[allow(clippy::result_large_err)]
fn cookie(headers: &HeaderMap, name: &str) -> Result<Option<String>, Response> {
    let mut found = None;
    for header in headers.get_all("cookie") {
        for pair in header.to_str().map_err(|_| unauthorized())?.split(';') {
            if let Some((key, value)) = pair.trim().split_once('=') {
                if key == name {
                    if found.is_some() || value.is_empty() || value.len() > 128 {
                        return Err(unauthorized());
                    }
                    found = Some(value.to_owned());
                }
            }
        }
    }
    Ok(found)
}
fn set_cookie(response: &mut Response, name: &str, value: &str, seconds: i64) {
    let value =
        format!("{name}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={seconds}");
    response.headers_mut().append(
        "set-cookie",
        HeaderValue::from_str(&value).expect("generated cookie"),
    );
}

pub(crate) async fn login(State(state): State<Arc<ProxyState>>) -> Response {
    let Some(oidc) = &state.oidc else {
        return failure(StatusCode::NOT_FOUND, "SSO is not configured");
    };
    let Ok(_permit) = oidc.permits.try_acquire() else {
        return unavailable();
    };
    let (client, _, _) = match oidc.discover().await {
        Ok(v) => v,
        Err(_) => return unavailable(),
    };
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut auth = client
        .authorize_url(
            AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .set_pkce_challenge(challenge);
    for scope in &oidc.config.scopes {
        if scope != "openid" {
            auth = auth.add_scope(Scope::new(scope.clone()));
        }
    }
    let (url, csrf, nonce) = auth.url();
    let binding = random();
    let mut sessions = oidc.sessions.lock().expect("OIDC sessions poisoned");
    sessions.prune();
    if sessions.pending.len() >= MAX_LOGINS {
        return unavailable();
    }
    sessions.pending.insert(
        digest(&binding),
        Pending {
            state: csrf.secret().clone(),
            nonce,
            verifier,
            created: Instant::now(),
        },
    );
    let mut response = Redirect::to(url.as_str()).into_response();
    set_cookie(
        &mut response,
        LOGIN_COOKIE,
        &binding,
        LOGIN_TTL.as_secs() as i64,
    );
    response
}
#[derive(Deserialize)]
pub(crate) struct Callback {
    code: String,
    state: String,
}
pub(crate) async fn callback(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Response {
    let Some(oidc) = &state.oidc else {
        return unauthorized();
    };
    let Ok(Some(binding)) = cookie(&headers, LOGIN_COOKIE) else {
        return unauthorized();
    };
    if query.code.len() > MAX_TOKEN_BYTES || query.state.len() > 256 {
        return unauthorized();
    }
    let pending = {
        let mut sessions = oidc.sessions.lock().expect("OIDC sessions poisoned");
        sessions.prune();
        sessions.pending.remove(&digest(&binding))
    };
    let Some(pending) = pending else {
        return unauthorized();
    };
    if !constant_time_eq(query.state.as_bytes(), pending.state.as_bytes()) {
        return unauthorized();
    }
    let Ok(_permit) = oidc.permits.try_acquire() else {
        return unavailable();
    };
    let (client, _, _) = match oidc.discover().await {
        Ok(v) => v,
        Err(_) => return unavailable(),
    };
    let Ok(exchange) = client.exchange_code(AuthorizationCode::new(query.code)) else {
        return unavailable();
    };
    let token = match exchange
        .set_pkce_verifier(pending.verifier)
        .request_async(oidc.as_ref())
        .await
    {
        Ok(v) => v,
        Err(openidconnect::RequestTokenError::ServerResponse(error))
            if error.error().as_ref() == "invalid_grant" =>
        {
            return unauthorized();
        }
        Err(_) => return unavailable(),
    };
    let Some(id) = token.id_token() else {
        return unauthorized();
    };
    let verifier = client.id_token_verifier();
    let claims = match id.claims(&verifier, &pending.nonce) {
        Ok(v) => v,
        Err(_) => return unauthorized(),
    };
    if let Some(expected) = claims.access_token_hash() {
        let actual = id.signing_alg().ok().and_then(|alg| {
            id.signing_key(&verifier)
                .ok()
                .and_then(|key| AccessTokenHash::from_token(token.access_token(), alg, key).ok())
        });
        if actual.as_ref() != Some(expected) {
            return unauthorized();
        }
    }
    let subject = claims.subject().as_str().to_owned();
    if !oidc.config.permits(&subject, Permission::Read) {
        return failure(StatusCode::FORBIDDEN, "no dashboard role assigned");
    }
    let expires = claims
        .expiration()
        .timestamp()
        .min(unix_now() + SESSION_TTL);
    if expires <= unix_now() {
        return unauthorized();
    }
    let value = random();
    let mut sessions = oidc.sessions.lock().expect("OIDC sessions poisoned");
    sessions.prune();
    if sessions.active.len() >= MAX_SESSIONS {
        return unavailable();
    }
    if let Ok(Some(old)) = cookie(&headers, SESSION_COOKIE) {
        sessions.active.remove(&digest(&old));
    }
    sessions.active.insert(
        digest(&value),
        Session {
            subject,
            csrf: random(),
            expires,
        },
    );
    let mut response = Redirect::to("/dashboard").into_response();
    set_cookie(&mut response, SESSION_COOKIE, &value, expires - unix_now());
    set_cookie(&mut response, LOGIN_COOKIE, "", 0);
    response
}
pub(crate) async fn session_status(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    let Some(oidc) = &state.oidc else {
        return Json(json!({"mode":"tokens"})).into_response();
    };
    match oidc.session(&headers) {
        Ok(Some(s)) => {
            let binding = oidc.config.subjects.get(&s.subject);
            let mut permissions = [Permission::Read, Permission::Operate, Permission::Admin]
                .into_iter()
                .filter(|p| oidc.config.permits(&s.subject, *p))
                .collect::<Vec<_>>();
            // Preserve existing session responses unless the capability is explicitly enabled.
            // Admin already implies diagnostics, as it did before this opt-in extension.
            if binding.is_some_and(|b| b.allow_diagnostics) {
                permissions.push(Permission::Diagnostics);
            }
            Json(
                json!({"mode":"oidc","subject":s.subject,"role":binding.and_then(|b| b.role),
                "permissions":permissions,"csrf":s.csrf,"expires_at":s.expires}),
            )
            .into_response()
        }
        Ok(None) => Json(json!({"mode":"oidc","permissions":[]})).into_response(),
        Err(r) => r,
    }
}
pub(crate) async fn logout(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    let Some(oidc) = &state.oidc else {
        return unauthorized();
    };
    if unique_header(&headers, "authorization")
        .ok()
        .flatten()
        .is_some()
    {
        return unauthorized();
    }
    if let Err(r) = oidc.identity(&headers, true).await {
        return r;
    }
    if let Ok(Some(value)) = cookie(&headers, SESSION_COOKIE) {
        oidc.sessions
            .lock()
            .expect("OIDC sessions poisoned")
            .active
            .remove(&digest(&value));
    }
    let mut response = Json(json!({"ok":true})).into_response();
    set_cookie(&mut response, SESSION_COOKIE, "", 0);
    response
}

#[cfg(test)]
mod tests;
