//! "Sign in with Cloudflare": OAuth (authorization code + PKCE) in the browser, then create an R2
//! bucket and an S3 key scoped to that one bucket. Tokens and secrets are never logged.

use std::{fmt, time::Duration};

use anyhow::{Context, anyhow};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use rand::Rng;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use crate::store::BucketConfig;

/// redirect_uri = http://127.0.0.1:47821/callback
pub const REDIRECT_PORT: u16 = 47821;

/// OAuth scope IDs (space-separated): account read, R2 bucket create/write, account-owned API token create.
/// Cloudflare wants dot-delimited scope IDs (e.g. `workers-platform.read`), listed by
/// `GET /client/v4/oauth/scopes`. These defaults are unverified guesses: build with
/// `SHELF_CF_SCOPES="id1 id2 id3"` using the IDs the client was registered with.
pub fn scopes() -> &'static str {
    option_env!("SHELF_CF_SCOPES").unwrap_or("account-settings.read workers-r2-storage.write account-api-tokens.write")
}

const API: &str = "https://api.cloudflare.com/client/v4";
const OAUTH: &str = "https://dash.cloudflare.com/oauth2";
const R2_URL: &str = "https://dash.cloudflare.com/?to=/:account/r2/overview";
const PERMISSION: &str = "Workers R2 Storage Bucket Item Write";
const CODE_R2_DISABLED: i64 = 10042;
const CODE_BUCKET_EXISTS: i64 = 10004;

/// Set `SHELF_CF_CLIENT_ID` at build time. `None` means the UI hides the button.
pub fn client_id() -> Option<&'static str> {
    option_env!("SHELF_CF_CLIENT_ID").filter(|s| !s.is_empty())
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub name: String,
}

#[derive(Debug)]
pub enum CfError {
    Cancelled,
    R2NotEnabled { url: String },
    Other(anyhow::Error),
}

impl fmt::Display for CfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "Sign-in cancelled"),
            Self::R2NotEnabled { url } => write!(f, "R2 isn't enabled on this Cloudflare account. Enable it at {url}"),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for CfError {}

impl From<anyhow::Error> for CfError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

pub struct Session {
    http: reqwest::Client,
    api: String,
    oauth: String,
    token: String,
    client_id: String,
}

// ---- API plumbing ------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct ApiError {
    code: i64,
    message: String,
}

#[derive(Debug)]
struct ApiFail {
    status: u16,
    errors: Vec<ApiError>,
}

impl fmt::Display for ApiFail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msgs: Vec<_> = self.errors.iter().map(|e| format!("{} ({})", e.message, e.code)).collect();
        write!(f, "Cloudflare API {}: {}", self.status, msgs.join("; "))
    }
}

impl std::error::Error for ApiFail {}

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    result: Option<T>,
}

#[derive(Deserialize)]
struct Named {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Created {
    id: String,
    value: String,
}

impl Session {
    pub(crate) fn with_base(api: &str, oauth: &str, token: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("http client");
        Self {
            http,
            api: api.trim_end_matches('/').into(),
            oauth: oauth.trim_end_matches('/').into(),
            token: token.into(),
            client_id: String::new(),
        }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<T> {
        let mut req = self.http.request(method, format!("{}{path}", self.api)).bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.context("Cloudflare request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("reading Cloudflare response")?;
        let env: Envelope<T> = serde_json::from_str(&text).map_err(|_| anyhow!("Cloudflare API {status}: unexpected response"))?;
        match env.result {
            Some(r) if env.success && status.is_success() => Ok(r),
            _ => Err(ApiFail { status: status.as_u16(), errors: env.errors }.into()),
        }
    }

    pub async fn accounts(&self) -> Result<Vec<Account>, CfError> {
        let list: Vec<Named> = self.call(reqwest::Method::GET, "/accounts", None).await?;
        Ok(list.into_iter().map(|a| Account { id: a.id, name: a.name }).collect())
    }

    /// Creates `shelf-xxxxxx` plus an S3 key limited to that bucket. Returns config and secret.
    pub async fn provision(&self, account: &Account) -> Result<(BucketConfig, String), CfError> {
        use reqwest::Method;
        let acc = format!("/accounts/{}", account.id);

        let groups: Vec<Named> = self.call(Method::GET, &format!("{acc}/tokens/permission_groups"), None).await?;
        let group = groups
            .into_iter()
            .find(|g| g.name == PERMISSION)
            .with_context(|| format!("permission group \"{PERMISSION}\" not found"))?;

        let mut bucket = String::new();
        for attempt in 1..=3 {
            bucket = format!("shelf-{}", random_suffix());
            let made: anyhow::Result<Value> =
                self.call(Method::POST, &format!("{acc}/r2/buckets"), Some(json!({ "name": bucket }))).await;
            match made {
                Ok(_) => break,
                Err(e) => match e.downcast_ref::<ApiFail>() {
                    Some(f) if f.errors.iter().any(|e| e.code == CODE_R2_DISABLED || e.message.to_lowercase().contains("enable r2")) => {
                        return Err(CfError::R2NotEnabled { url: R2_URL.into() });
                    }
                    Some(f) if attempt < 3 && (f.status == 409 || f.errors.iter().any(|e| e.code == CODE_BUCKET_EXISTS)) => {}
                    _ => return Err(e.into()),
                },
            }
        }

        let resource = format!("com.cloudflare.edge.r2.bucket.{}_default_{bucket}", account.id);
        let body = json!({
            "name": format!("shelf-{bucket}"),
            "policies": [{
                "effect": "allow",
                "resources": { resource: "*" },
                "permission_groups": [{ "id": group.id }],
            }],
        });
        let token: Created = self.call(Method::POST, &format!("{acc}/tokens"), Some(body)).await?;
        let cfg = BucketConfig::new(format!("https://{}.r2.cloudflarestorage.com", account.id), bucket, token.id);
        Ok((cfg, secret_for(&token.value)))
    }

    /// Revokes the OAuth access token. Best effort: the caller may ignore the error.
    pub async fn revoke(self) -> Result<(), CfError> {
        let form = [("token", self.token.as_str()), ("token_type_hint", "access_token"), ("client_id", &self.client_id)];
        let resp = self.http.post(format!("{}/revoke", self.oauth)).form(&form).send().await.context("revoke failed")?;
        resp.error_for_status().context("revoke failed")?;
        Ok(())
    }
}

/// R2 S3 secret = lowercase hex SHA-256 of the API token value.
fn secret_for(token_value: &str) -> String {
    format!("{:x}", Sha256::digest(token_value))
}

fn random_suffix() -> String {
    let mut rng = rand::rng();
    (0..6).map(|_| char::from(b"abcdefghijklmnopqrstuvwxyz0123456789"[rng.random_range(0..36)])).collect()
}

// ---- OAuth login -------------------------------------------------------------------------

fn challenge_for(verifier: &str) -> String {
    B64.encode(Sha256::digest(verifier))
}

/// First request line -> `Ok(None)` if it isn't the callback (favicon etc.), else (code, state).
fn parse_callback(req: &str) -> Result<Option<(String, String)>, CfError> {
    let target = req.lines().next().unwrap_or("").split(' ').nth(1).unwrap_or("");
    let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else { return Ok(None) };
    if url.path() != "/callback" {
        return Ok(None);
    }
    let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    if let Some(err) = q("error") {
        return Err(match err.as_str() {
            "access_denied" => CfError::Cancelled,
            _ => anyhow!("Cloudflare sign-in failed: {err} {}", q("error_description").unwrap_or_default()).into(),
        });
    }
    match (q("code"), q("state")) {
        (Some(c), Some(s)) => Ok(Some((c, s))),
        _ => Err(anyhow!("Cloudflare redirect had no code or state").into()),
    }
}

async fn read_head(s: &mut TcpStream) -> anyhow::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn reply(s: &mut TcpStream, status: &str, html: &str) {
    let msg = format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len());
    let _ = s.write_all(msg.as_bytes()).await;
    let _ = s.shutdown().await;
}

/// Opens the browser, waits up to 5 minutes for the redirect, exchanges the code for a token.
pub async fn login(client_id: &str) -> Result<Session, CfError> {
    let verifier = B64.encode(rand::random::<[u8; 32]>());
    let state = B64.encode(rand::random::<[u8; 16]>());
    let redirect = format!("http://127.0.0.1:{REDIRECT_PORT}/callback");

    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
        .await
        .with_context(|| format!("port {REDIRECT_PORT} is busy; close the other Shelf sign-in and retry"))?;
    let mut url = reqwest::Url::parse(&format!("{OAUTH}/auth")).expect("static url");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", scopes())
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge_for(&verifier))
        .append_pair("code_challenge_method", "S256");
    open::that(url.as_str()).context("couldn't open the browser")?;

    let wait = async {
        loop {
            let (mut s, _) = listener.accept().await.context("accept")?;
            let parsed = parse_callback(&read_head(&mut s).await.unwrap_or_default());
            let page = match &parsed {
                Ok(Some(_)) => "<h3>Signed in. You can close this tab and return to Shelf.</h3>",
                Ok(None) => "",
                Err(_) => "<h3>Sign-in did not complete. You can close this tab.</h3>",
            };
            reply(&mut s, if page.is_empty() { "404 Not Found" } else { "200 OK" }, page).await;
            match parsed {
                Ok(None) => continue,
                other => return other.map(|o| o.expect("checked")),
            }
        }
    };
    let (code, got_state) = tokio::time::timeout(Duration::from_secs(300), wait).await.map_err(|_| CfError::Cancelled)??;
    drop(listener);
    if got_state != state {
        return Err(anyhow!("Cloudflare sign-in state mismatch").into());
    }

    let mut session = Session::with_base(API, OAUTH, "");
    session.client_id = client_id.into();
    session.token = session.exchange(&code, &verifier, &redirect).await?;
    Ok(session)
}

impl Session {
    async fn exchange(&self, code: &str, verifier: &str, redirect: &str) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Tok {
            access_token: String,
        }
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect),
            ("client_id", &self.client_id),
            ("code_verifier", verifier),
        ];
        let resp = self.http.post(format!("{}/token", self.oauth)).form(&form).send().await.context("token request failed")?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("Cloudflare token exchange failed ({status})"));
        }
        Ok(resp.json::<Tok>().await.context("bad token response")?.access_token)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn pkce_rfc7636_vector() {
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn secret_is_sha256_hex() {
        assert_eq!(secret_for("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn callback_parser() {
        let ok = parse_callback("GET /callback?code=c%2B1&state=s HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(ok.unwrap(), Some(("c+1".into(), "s".into())));
        assert!(matches!(parse_callback("GET /favicon.ico HTTP/1.1\r\n"), Ok(None)));
        assert!(matches!(parse_callback("GET /callback?error=access_denied&state=s HTTP/1.1\r\n"), Err(CfError::Cancelled)));
        assert!(matches!(parse_callback("GET /callback?error=server_error HTTP/1.1\r\n"), Err(CfError::Other(_))));
        assert!(matches!(parse_callback("GET /callback?code=c HTTP/1.1\r\n"), Err(CfError::Other(_))));
    }

    type Log = Arc<Mutex<Vec<(String, String)>>>;

    /// Tiny fake API: `handler(method, path, body) -> (status, json)`. Returns base URL and request log.
    async fn fake(handler: impl Fn(&str, &str, &str) -> (u16, String) + Send + 'static) -> (String, Log) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log: Log = Default::default();
        let log2 = log.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let n = s.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((h, b)) = text.split_once("\r\n\r\n") {
                        let len = h.to_lowercase().split("content-length:").nth(1).map_or(0, |v| v.lines().next().unwrap().trim().parse().unwrap());
                        if b.len() >= len || n == 0 {
                            break (h.to_string(), b.to_string());
                        }
                    }
                };
                let mut line = head.lines().next().unwrap().split(' ');
                let (m, p) = (line.next().unwrap().to_string(), line.next().unwrap().to_string());
                let (status, json) = handler(&m, &p, &body);
                log2.lock().unwrap().push((format!("{m} {p}"), body));
                let msg = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len());
                s.write_all(msg.as_bytes()).await.unwrap();
                s.shutdown().await.ok();
            }
        });
        (base, log)
    }

    fn ok(result: Value) -> (u16, String) {
        (200, json!({ "success": true, "errors": [], "result": result }).to_string())
    }

    fn fail(status: u16, code: i64, msg: &str) -> (u16, String) {
        (status, json!({ "success": false, "errors": [{ "code": code, "message": msg }], "result": null }).to_string())
    }

    fn acct() -> Account {
        Account { id: "acc1".into(), name: "Me".into() }
    }

    #[tokio::test]
    async fn provision_end_to_end() {
        let creates = Arc::new(Mutex::new(0));
        let c = creates.clone();
        let (base, log) = fake(move |m, p, _| match (m, p) {
            ("GET", "/accounts") => ok(json!([{ "id": "acc1", "name": "Me" }])),
            ("GET", "/accounts/acc1/tokens/permission_groups") => {
                ok(json!([{ "id": "g0", "name": "Other" }, { "id": "g1", "name": PERMISSION }]))
            }
            ("POST", "/accounts/acc1/r2/buckets") => {
                let mut n = c.lock().unwrap();
                *n += 1;
                if *n == 1 { fail(409, CODE_BUCKET_EXISTS, "exists") } else { ok(json!({})) }
            }
            ("POST", "/accounts/acc1/tokens") => ok(json!({ "id": "key-id", "value": "abc" })),
            _ => (404, "{}".into()),
        })
        .await;
        let s = Session::with_base(&base, &base, "tok");
        let accounts = s.accounts().await.unwrap();
        assert_eq!(accounts[0].id, "acc1");

        let (cfg, secret) = s.provision(&acct()).await.unwrap();
        assert_eq!(*creates.lock().unwrap(), 2);
        assert_eq!(cfg.endpoint, "https://acc1.r2.cloudflarestorage.com");
        assert_eq!(cfg.access_key_id, "key-id");
        assert!(cfg.bucket.starts_with("shelf-") && cfg.bucket.len() == 12);
        assert_eq!(secret, secret_for("abc"));

        let log = log.lock().unwrap();
        let (_, body) = log.last().unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        let key = format!("com.cloudflare.edge.r2.bucket.acc1_default_{}", cfg.bucket);
        assert_eq!(body["policies"][0]["resources"][&key], "*");
        assert_eq!(body["policies"][0]["permission_groups"][0]["id"], "g1");
    }

    #[tokio::test]
    async fn provision_r2_not_enabled() {
        let (base, _) = fake(|m, p, _| match (m, p) {
            ("GET", _) => ok(json!([{ "id": "g1", "name": PERMISSION }])),
            _ => fail(403, CODE_R2_DISABLED, "Please enable R2 through the Cloudflare Dashboard."),
        })
        .await;
        let err = Session::with_base(&base, &base, "tok").provision(&acct()).await.unwrap_err();
        assert!(matches!(err, CfError::R2NotEnabled { url } if url == R2_URL));
    }
}
