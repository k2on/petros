//! The routes: `/auth/login`, `/auth/callback`, `/auth/exchange`,
//! `/auth/me`, `/auth/logout`. Mount them beside the socket.
//!
//! ```ignore
//! let auth = petros_auth::server::Auth::new(sessions, mode, "https://harken.example.com")
//!     .allow_redirect("harken://");
//! let auth = Arc::new(auth);
//! let hub = petros_axum::Hub::<HarkenApp>::open(conn, auth.authenticator())?;
//! let app = Router::new()
//!     .route("/sync", get(petros_axum::sync::<HarkenApp>))
//!     .with_state(hub)
//!     .merge(petros_auth::server::router(auth));
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use petros::{ActorId, Authenticate, Identity};
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};

use crate::oidc::{Challenge, Provider};
use crate::session::{now_ms, random_token, SessionStore};
use crate::util::with_code;
use crate::{Account, Login};

/// How people sign in.
#[derive(Debug)]
pub enum Mode {
    /// Through an OpenID Connect provider.
    Oidc(Provider),
    /// As whatever name they type. For a laptop and nowhere else; a server
    /// running this way should say so every time it starts.
    Dev,
}

/// A login that has been started and not finished: the state the provider
/// will hand back, and what has to match when it does.
#[derive(Debug)]
struct Pending {
    challenge: Challenge,
    redirect: String,
    started_ms: i64,
}

/// A login that has finished and not been collected: a one-minute,
/// single-use code standing in for the token in the URL.
#[derive(Debug)]
struct Issued {
    login: Login,
    issued_ms: i64,
}

/// A login has a minute to go from the provider to the exchange, and a
/// person ten minutes to sign in at the provider.
const CODE_TTL_MS: i64 = 60 * 1000;
const PENDING_TTL_MS: i64 = 10 * 60 * 1000;

/// Everything the routes share. `Arc` it; the same one is what the hub is
/// opened with, so the socket and the routes agree on who is who.
pub struct Auth {
    sessions: Mutex<SessionStore>,
    mode: Mode,
    /// Where this server is reachable from a browser, with no trailing
    /// slash: the provider sends people back to `{public_url}/auth/callback`.
    public_url: String,
    /// Prefixes a redirect may have, besides the loopback ones every server
    /// allows and its own `public_url`.
    redirects: Vec<String>,
    pending: Mutex<HashMap<String, Pending>>,
    codes: Mutex<HashMap<String, Issued>>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("mode", &self.mode)
            .field("public_url", &self.public_url)
            .field("redirects", &self.redirects)
            .finish_non_exhaustive()
    }
}

impl Auth {
    pub fn new(sessions: SessionStore, mode: Mode, public_url: &str) -> Self {
        Auth {
            sessions: Mutex::new(sessions),
            mode,
            public_url: public_url.trim_end_matches('/').to_string(),
            redirects: Vec::new(),
            pending: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
        }
    }

    /// Let a login code go back to anything starting with `prefix` — an
    /// app's URL scheme, `harken://`, or a web client served elsewhere.
    pub fn allow_redirect(mut self, prefix: &str) -> Self {
        self.redirects.push(prefix.to_string());
        self
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// Whether a code may be sent to `url`. Loopback is always fine — it is
    /// how a desktop program listens, and nothing off the machine can be
    /// there — and so is this server's own origin, where the browser client
    /// it serves lives.
    pub fn redirect_allowed(&self, url: &str) -> bool {
        // The origin, not a prefix: `https://h.example.evil/` starts with
        // `https://h.example` and is somebody else's.
        if let Some(rest) = url.strip_prefix(&self.public_url) {
            if rest.is_empty() || rest.starts_with(['/', '?', '#']) {
                return true;
            }
        }
        if let Some(rest) = url.strip_prefix("http://") {
            let host = rest.split(['/', '?', '#']).next().unwrap_or("");
            let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
            if matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
                return true;
            }
        }
        self.redirects.iter().any(|p| url.starts_with(p))
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, SessionStore> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sign `account` in and mint the code the redirect carries.
    fn finish(&self, account: &Account) -> Result<String, String> {
        let login = self
            .sessions()
            .issue(account)
            .map_err(|e| format!("could not record the session: {e}"))?;
        let code = random_token();
        let now = now_ms();
        let mut codes = self.codes.lock().unwrap_or_else(|e| e.into_inner());
        codes.retain(|_, c| now - c.issued_ms < CODE_TTL_MS);
        codes.insert(
            code.clone(),
            Issued {
                login,
                issued_ms: now,
            },
        );
        Ok(code)
    }

    /// The login a code stands for, once.
    pub fn redeem(&self, code: &str) -> Option<Login> {
        let mut codes = self.codes.lock().unwrap_or_else(|e| e.into_inner());
        let issued = codes.remove(code)?;
        (now_ms() - issued.issued_ms < CODE_TTL_MS).then_some(issued.login)
    }

    /// The login a bearer token proves, if it is live.
    pub fn whoami(&self, token: &str) -> Option<Login> {
        self.sessions().lookup(token).ok().flatten()
    }
}

/// What the hub is opened with: the same store the routes write to, so the
/// socket and the routes agree on who is who.
#[derive(Debug, Clone)]
pub struct Authenticator(Arc<Auth>);

impl Auth {
    /// The engine's view of this. `Hub::open(conn, auth.authenticator())`.
    pub fn authenticator(self: &Arc<Self>) -> Authenticator {
        Authenticator(self.clone())
    }
}

impl Authenticate for Authenticator {
    fn authenticate(&mut self, token: Option<&str>) -> Option<Identity> {
        let login = self.0.whoami(token?)?;
        Some(Identity {
            user: ActorId::new(login.user.id),
            session: login.session,
        })
    }

    fn owns(&mut self, user: &ActorId, session: &str) -> bool {
        self.0
            .sessions()
            .owned_by(user.as_str(), session)
            .unwrap_or(false)
    }
}

/// The routes, with state applied, ready to `merge` into an app's router.
///
/// Permissive CORS on all of them: the token is a bearer, never a cookie, so
/// another origin reading these answers learns only what it already sent.
/// A browser client served from another port on a laptop is the case.
pub fn router(auth: Arc<Auth>) -> Router {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/exchange", post(exchange))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(auth)
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    redirect: String,
    #[serde(default)]
    user: Option<String>,
}

/// Start a login, or in dev mode finish one.
async fn login(State(auth): State<Arc<Auth>>, Query(q): Query<LoginQuery>) -> Response {
    if !auth.redirect_allowed(&q.redirect) {
        return bad(format!(
            "a login code cannot be sent to {}; the server allows loopback, its own \
             origin, and what it was configured with",
            q.redirect
        ));
    }
    match &auth.mode {
        Mode::Dev => {
            let Some(user) = q.user.filter(|u| !u.trim().is_empty()) else {
                return Html(dev_form(&q.redirect)).into_response();
            };
            let user = user.trim().to_string();
            let account = Account {
                id: user.clone(),
                name: user,
                email: String::new(),
            };
            match auth.finish(&account) {
                Ok(code) => Redirect::to(&with_code(&q.redirect, &code)).into_response(),
                Err(e) => failed(e),
            }
        }
        Mode::Oidc(provider) => {
            let challenge = Challenge::new();
            let url = provider.authorize_url(&challenge, &callback_url(&auth));
            let now = now_ms();
            let mut pending = auth.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.retain(|_, p| now - p.started_ms < PENDING_TTL_MS);
            pending.insert(
                challenge.state.clone(),
                Pending {
                    challenge,
                    redirect: q.redirect,
                    started_ms: now,
                },
            );
            Redirect::to(&url).into_response()
        }
    }
}

fn callback_url(auth: &Auth) -> String {
    format!("{}/auth/callback", auth.public_url)
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// The provider sending the person back.
async fn callback(State(auth): State<Arc<Auth>>, Query(q): Query<CallbackQuery>) -> Response {
    let Mode::Oidc(provider) = &auth.mode else {
        return bad("this server does not use a provider".into());
    };
    if let Some(error) = q.error {
        return bad(format!(
            "the provider refused: {error} {}",
            q.error_description.unwrap_or_default()
        ));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return bad("the provider sent no code".into());
    };
    let Some(pending) = auth
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&state)
    else {
        return bad("this login was not started here, or took too long".into());
    };
    if now_ms() - pending.started_ms >= PENDING_TTL_MS {
        return bad("this login took too long; start again".into());
    }

    // Two round trips to the provider, off the runtime.
    let provider = provider.clone();
    let callback = callback_url(&auth);
    let exchanged = tokio::task::spawn_blocking(move || {
        provider.exchange(&code, &pending.challenge, &callback)
    })
    .await;
    let account = match exchanged {
        Ok(Ok(account)) => account,
        Ok(Err(e)) => return failed(e),
        Err(e) => return failed(e.to_string()),
    };
    match auth.finish(&account) {
        Ok(code) => Redirect::to(&with_code(&pending.redirect, &code)).into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Debug, Deserialize)]
struct Exchange {
    code: String,
}

/// The code, for the login it stands for. Once.
async fn exchange(State(auth): State<Arc<Auth>>, Json(body): Json<Exchange>) -> Response {
    match auth.redeem(&body.code) {
        Some(login) => Json(login).into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            "that code has been used, or has expired",
        )
            .into_response(),
    }
}

/// Who a bearer token is.
async fn me(State(auth): State<Arc<Auth>>, headers: HeaderMap) -> Response {
    match bearer(&headers).and_then(|t| auth.whoami(t)) {
        Some(login) => Json(login).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// End the session a bearer token proves.
async fn logout(State(auth): State<Arc<Auth>>, headers: HeaderMap) -> Response {
    let revoked = bearer(&headers)
        .map(|t| auth.sessions().revoke(t).unwrap_or(false))
        .unwrap_or(false);
    if revoked {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

fn bad(why: String) -> Response {
    (StatusCode::BAD_REQUEST, why).into_response()
}

fn failed(why: String) -> Response {
    (StatusCode::BAD_GATEWAY, format!("sign-in failed: {why}")).into_response()
}

/// Dev mode's provider: a text box.
fn dev_form(redirect: &str) -> String {
    let redirect = redirect
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;");
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
         <title>sign in</title>\
         <style>body{{font-family:system-ui;max-width:24em;margin:4em auto;padding:0 1em}}\
         input,button{{font:inherit;padding:.5em;width:100%;box-sizing:border-box;margin:.25em 0}}\
         p{{color:#666}}</style>\
         <h1>sign in</h1>\
         <p>This server is running without an identity provider, so it takes your word \
         for who you are. That is fine on a laptop and nowhere else.</p>\
         <form method=get action=/auth/login>\
         <input type=hidden name=redirect value=\"{redirect}\">\
         <input name=user placeholder=\"a name\" autofocus autocapitalize=off>\
         <button>sign in</button></form>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth::new(
            SessionStore::open(petros::open_memory().unwrap()).unwrap(),
            Mode::Dev,
            "https://harken.example/",
        )
        .allow_redirect("harken://")
    }

    #[test]
    fn a_code_goes_only_where_it_is_allowed() {
        let a = auth();
        assert!(a.redirect_allowed("http://127.0.0.1:53211/"));
        assert!(a.redirect_allowed("http://localhost:8080/?x=1"));
        assert!(a.redirect_allowed("https://harken.example/"));
        assert!(a.redirect_allowed("https://harken.example/app/"));
        assert!(a.redirect_allowed("harken://auth"));
        assert!(!a.redirect_allowed("https://harken.example.evil/"));
        assert!(!a.redirect_allowed("http://127.0.0.1.evil/"));
        assert!(!a.redirect_allowed("http://10.0.0.5:8080/"));
        assert!(!a.redirect_allowed("https://elsewhere/"));
    }

    #[test]
    fn a_code_is_redeemed_once() {
        let a = auth();
        let code = a
            .finish(&Account {
                id: "alice".into(),
                name: "alice".into(),
                email: String::new(),
            })
            .unwrap();
        let login = a.redeem(&code).expect("first time");
        assert_eq!(login.user.id, "alice");
        assert!(a.redeem(&code).is_none(), "second time");
        assert_eq!(a.whoami(&login.token).unwrap().session, login.session);

        let mut who = Arc::new(a).authenticator();
        let id = who.authenticate(Some(&login.token)).unwrap();
        assert_eq!(id.user.as_str(), "alice");
        assert!(who.owns(&ActorId::from("alice"), &login.session));
    }
}
