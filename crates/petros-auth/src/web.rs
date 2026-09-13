//! Getting a login, in a browser.
//!
//! The page *is* the redirect: it sends itself to the server's login and
//! comes back with `?code=` in its own URL, which it trades for a login and
//! then removes from the address bar.

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::util::{login_url, query_value};
use crate::Login;

/// Send the page to sign in, coming back to where it is now.
pub fn go_sign_in(server: &str, user: Option<&str>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let here = window
        .location()
        .href()
        .unwrap_or_default()
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    let _ = window.location().assign(&login_url(server, &here, user));
}

/// The code in this page's URL, if it just came back from signing in —
/// taken out of the address bar on the way, so a reload does not try again.
pub fn take_code() -> Option<String> {
    let window = web_sys::window()?;
    let location = window.location();
    let search = location.search().ok()?;
    let code = query_value(&search, "code")?;
    let clean = location.pathname().unwrap_or_default();
    let _ = window.history().ok()?.replace_state_with_url(
        &wasm_bindgen::JsValue::NULL,
        "",
        Some(&clean),
    );
    Some(code)
}

/// The code, for the login it stands for.
pub async fn exchange(server: &str, code: &str) -> Result<Login, String> {
    let url = format!("{}/auth/exchange", server.trim_end_matches('/'));
    let body = serde_json::json!({ "code": code }).to_string();
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_body(&wasm_bindgen::JsValue::from_str(&body));
    let headers = web_sys::Headers::new().map_err(js)?;
    headers
        .set("Content-Type", "application/json")
        .map_err(js)?;
    init.set_headers(&headers);
    let text = fetch_text(&url, &init).await?;
    serde_json::from_str(&text).map_err(|e| format!("the login did not parse: {e}"))
}

/// Whether `token` still proves a login at `server`, and whose.
pub async fn whoami(server: &str, token: &str) -> Result<Option<Login>, String> {
    let url = format!("{}/auth/me", server.trim_end_matches('/'));
    let init = web_sys::RequestInit::new();
    let headers = web_sys::Headers::new().map_err(js)?;
    headers
        .set("Authorization", &format!("Bearer {token}"))
        .map_err(js)?;
    init.set_headers(&headers);
    match fetch_text(&url, &init).await {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("the login did not parse: {e}")),
        Err(e) if e.starts_with("401") => Ok(None),
        Err(e) => Err(e),
    }
}

/// The browser's storage, for keeping a login between loads.
pub fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

async fn fetch_text(url: &str, init: &web_sys::RequestInit) -> Result<String, String> {
    let window = web_sys::window().ok_or("no window")?;
    let request = web_sys::Request::new_with_str_and_init(url, init).map_err(js)?;
    let resp = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|_| format!("cannot reach {url}"))?;
    let resp: web_sys::Response = resp.dyn_into().map_err(js)?;
    let text = JsFuture::from(resp.text().map_err(js)?)
        .await
        .map_err(js)?
        .as_string()
        .unwrap_or_default();
    if !resp.ok() {
        return Err(format!("{} {text}", resp.status()));
    }
    Ok(text)
}

fn js(e: wasm_bindgen::JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}
