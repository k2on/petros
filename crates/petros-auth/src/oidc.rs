//! The relying party: one OpenID Connect client, which is the server.
//!
//! Authorization code flow with PKCE, as a confidential client. The provider
//! is found from its issuer URL, the code is exchanged at its token endpoint
//! with the client secret, and the ID token that comes back says who signed
//! in.
//!
//! The ID token's signature is not checked, and that is not a shortcut. It
//! arrives over TLS in the direct answer to a request this server made with
//! its own secret; OpenID Connect Core 3.1.3.7 says a client that receives it
//! that way may rely on the TLS server validation instead. What *is* checked
//! is everything a signature would not have covered anyway: that it names
//! this issuer and this client, that it has not expired, and that its nonce
//! is the one this login started with — which is what ties the token to the
//! browser that asked, and not to a code somebody replayed.

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::util::percent_encode;
use crate::Account;

/// What the provider publishes about itself, the three parts of it used here.
#[derive(Debug, Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
}

/// A provider this server can send people to and get identities back from.
#[derive(Clone)]
pub struct Provider {
    issuer: String,
    client_id: String,
    client_secret: String,
    scopes: String,
    found: Discovery,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// What a login started with, and what the callback has to match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
}

impl Challenge {
    pub fn new() -> Self {
        Challenge {
            state: crate::session::random_token(),
            nonce: crate::session::random_token(),
            verifier: crate::session::random_token(),
        }
    }
}

impl Default for Challenge {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider {
    /// Read `{issuer}/.well-known/openid-configuration`. Blocking: call it at
    /// startup, or from a blocking task.
    pub fn discover(
        issuer: &str,
        client_id: &str,
        client_secret: &str,
        scopes: &[&str],
    ) -> Result<Self, String> {
        let issuer = issuer.trim_end_matches('/').to_string();
        let url = format!("{issuer}/.well-known/openid-configuration");
        let found: Discovery = get_json(&url)?;
        if found.issuer.trim_end_matches('/') != issuer {
            return Err(format!(
                "{url} says its issuer is {}, not {issuer}",
                found.issuer
            ));
        }
        Ok(Provider {
            issuer,
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            scopes: scopes.join(" "),
            found,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Where to send the person, for `challenge`, with the provider coming
    /// back to `callback`.
    pub fn authorize_url(&self, challenge: &Challenge, callback: &str) -> String {
        let code_challenge = base64url(&Sha256::digest(challenge.verifier.as_bytes()));
        format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}\
             &code_challenge={}&code_challenge_method=S256",
            self.found.authorization_endpoint,
            percent_encode(&self.client_id),
            percent_encode(callback),
            percent_encode(&self.scopes),
            percent_encode(&challenge.state),
            percent_encode(&challenge.nonce),
            percent_encode(&code_challenge),
        )
    }

    /// Trade the code the provider sent back for who signed in. Blocking.
    pub fn exchange(
        &self,
        code: &str,
        challenge: &Challenge,
        callback: &str,
    ) -> Result<Account, String> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", callback),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("code_verifier", &challenge.verifier),
        ];
        let tokens: Tokens = post_form(&self.found.token_endpoint, &form)?;
        let claims = decode_claims(&tokens.id_token)?;
        self.check(&claims, &challenge.nonce)?;

        let mut account = Account {
            id: claims.sub.clone(),
            name: claims.display_name(),
            email: claims.email.clone().unwrap_or_default(),
        };
        // A provider that keeps the ID token small says the rest at userinfo.
        if account.name.is_empty() || account.email.is_empty() {
            if let (Some(endpoint), Some(access)) =
                (&self.found.userinfo_endpoint, &tokens.access_token)
            {
                if let Ok(info) = get_json_bearer::<Claims>(endpoint, access) {
                    if account.name.is_empty() {
                        account.name = info.display_name();
                    }
                    if account.email.is_empty() {
                        account.email = info.email.unwrap_or_default();
                    }
                }
            }
        }
        Ok(account)
    }

    fn check(&self, claims: &Claims, nonce: &str) -> Result<(), String> {
        if claims.iss.trim_end_matches('/') != self.issuer {
            return Err(format!(
                "the ID token is from {}, not {}",
                claims.iss, self.issuer
            ));
        }
        if !claims.aud.contains(&self.client_id) {
            return Err("the ID token is not for this client".into());
        }
        if claims.exp * 1000 <= crate::session::now_ms() {
            return Err("the ID token has expired".into());
        }
        if claims.nonce.as_deref() != Some(nonce) {
            return Err("the ID token does not answer this login".into());
        }
        if claims.sub.is_empty() {
            return Err("the ID token names nobody".into());
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct Tokens {
    id_token: String,
    #[serde(default)]
    access_token: Option<String>,
}

/// The claims read out of an ID token, or a userinfo answer.
#[derive(Debug, Default, Deserialize)]
struct Claims {
    #[serde(default)]
    iss: String,
    #[serde(default)]
    sub: String,
    #[serde(default, deserialize_with = "one_or_many")]
    aud: Vec<String>,
    #[serde(default)]
    exp: i64,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

impl Claims {
    fn display_name(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.preferred_username.clone())
            .unwrap_or_default()
    }
}

/// `aud` is a string or an array of them, per the spec.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Aud {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Aud::deserialize(d)? {
        Aud::One(s) => vec![s],
        Aud::Many(v) => v,
    })
}

/// The payload of a JWT, without checking the signature — see the module
/// docs for why that is the right call here and only here.
fn decode_claims(jwt: &str) -> Result<Claims, String> {
    let payload = jwt.split('.').nth(1).ok_or("the ID token is not a JWT")?;
    let bytes = base64url_decode(payload).ok_or("the ID token's payload is not base64url")?;
    serde_json::from_slice(&bytes).map_err(|e| format!("the ID token's claims: {e}"))
}

pub(crate) fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..(chunk.len() + 1) {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        })
    };
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= value(*c)? << (18 - 6 * i);
        }
        for i in 0..(chunk.len() - 1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

// ----------------------------------------------------------------- transport

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build()
}

fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let body = agent()
        .get(url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?
        .into_string()
        .map_err(|e| format!("GET {url}: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("GET {url}: {e}"))
}

fn get_json_bearer<T: serde::de::DeserializeOwned>(url: &str, token: &str) -> Result<T, String> {
    let body = agent()
        .get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?
        .into_string()
        .map_err(|e| format!("GET {url}: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("GET {url}: {e}"))
}

fn post_form<T: serde::de::DeserializeOwned>(
    url: &str,
    form: &[(&str, &str)],
) -> Result<T, String> {
    let body = agent()
        .post(url)
        .set("Accept", "application/json")
        .send_form(form)
        .map_err(|e| format!("POST {url}: {e}"))?
        .into_string()
        .map_err(|e| format!("POST {url}: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("POST {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let enc = base64url(&bytes);
            assert!(!enc.contains('='));
            assert_eq!(base64url_decode(&enc).unwrap(), bytes, "{len}");
        }
    }

    #[test]
    fn claims_come_out_of_a_jwt_payload() {
        let payload = base64url(
            br#"{"iss":"https://p","sub":"42","aud":"harken","exp":4102444800,"nonce":"n","name":"A"}"#,
        );
        let claims = decode_claims(&format!("h.{payload}.s")).unwrap();
        assert_eq!(claims.sub, "42");
        assert_eq!(claims.aud, ["harken"]);
        assert_eq!(claims.display_name(), "A");
        let many = base64url(br#"{"aud":["a","b"]}"#);
        assert_eq!(
            decode_claims(&format!("h.{many}.s")).unwrap().aud,
            ["a", "b"]
        );
    }

    #[test]
    fn the_checks_hold_the_token_to_this_login() {
        let provider = Provider {
            issuer: "https://p".into(),
            client_id: "harken".into(),
            client_secret: String::new(),
            scopes: String::new(),
            found: Discovery {
                issuer: "https://p".into(),
                authorization_endpoint: String::new(),
                token_endpoint: String::new(),
                userinfo_endpoint: None,
            },
        };
        let good = Claims {
            iss: "https://p/".into(),
            sub: "42".into(),
            aud: vec!["other".into(), "harken".into()],
            exp: 4_102_444_800,
            nonce: Some("n".into()),
            ..Claims::default()
        };
        assert_eq!(provider.check(&good, "n"), Ok(()));
        assert!(provider.check(&good, "m").is_err(), "another login's nonce");
        assert!(provider
            .check(
                &Claims {
                    iss: "https://q".into(),
                    ..good_clone(&good)
                },
                "n"
            )
            .is_err());
        assert!(provider
            .check(
                &Claims {
                    aud: vec!["x".into()],
                    ..good_clone(&good)
                },
                "n"
            )
            .is_err());
        assert!(provider
            .check(
                &Claims {
                    exp: 1,
                    ..good_clone(&good)
                },
                "n"
            )
            .is_err());
    }

    fn good_clone(c: &Claims) -> Claims {
        Claims {
            iss: c.iss.clone(),
            sub: c.sub.clone(),
            aud: c.aud.clone(),
            exp: c.exp,
            nonce: c.nonce.clone(),
            name: None,
            preferred_username: None,
            email: None,
        }
    }
}
