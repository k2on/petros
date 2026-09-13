//! Small things every half needs.

/// The URL a client sends a person to, to sign in.
///
/// `server` is the server's base URL — `https://harken.example.com` or
/// `http://127.0.0.1:8787` — and `redirect` where the login code should come
/// back to. `user` is honoured only by a server in dev mode, and is how a
/// laptop runs two peers with two names and no browser.
pub fn login_url(server: &str, redirect: &str, user: Option<&str>) -> String {
    let mut url = format!(
        "{}/auth/login?redirect={}",
        server.trim_end_matches('/'),
        percent_encode(redirect)
    );
    if let Some(user) = user {
        url.push_str("&user=");
        url.push_str(&percent_encode(user));
    }
    url
}

/// The socket beside the login: `/sync`, with the scheme to match.
///
/// A client is told one address and derives the other, so a server moved
/// behind TLS changes one setting and not two.
pub fn socket_url(server: &str) -> String {
    let server = server.trim_end_matches('/');
    let socket = if let Some(rest) = server.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = server.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if server.starts_with("ws://") || server.starts_with("wss://") {
        server.to_string()
    } else {
        format!("ws://{server}")
    };
    format!("{socket}/sync")
}

/// Enough percent-encoding for a query value: everything but the unreserved
/// characters. Written here rather than pulled in, because it is twelve lines
/// and the crate that does it properly would be the only reason this one is
/// not tiny.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The other way, for a query value the server or a client reads back.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(b) => {
                    out.push(b);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of one key in a query string, decoded.
pub fn query_value(query: &str, key: &str) -> Option<String> {
    query
        .trim_start_matches('?')
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        .map(percent_decode)
}

/// `url` with `code` added to its query, whichever punctuation that takes.
#[cfg(any(feature = "server", test))]
pub fn with_code(url: &str, code: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}code={}", percent_encode(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_follows_the_scheme() {
        assert_eq!(socket_url("https://h.example"), "wss://h.example/sync");
        assert_eq!(
            socket_url("http://127.0.0.1:8787/"),
            "ws://127.0.0.1:8787/sync"
        );
        assert_eq!(socket_url("10.0.2.2:8787"), "ws://10.0.2.2:8787/sync");
    }

    #[test]
    fn a_query_value_round_trips() {
        let url = login_url("http://s", "harken://auth?x=1 2", Some("al ice"));
        let query = url.split('?').nth(1).unwrap();
        assert_eq!(
            query_value(query, "redirect").as_deref(),
            Some("harken://auth?x=1 2")
        );
        assert_eq!(query_value(query, "user").as_deref(), Some("al ice"));
        assert_eq!(query_value(query, "missing"), None);
    }

    #[test]
    fn the_code_joins_whatever_query_there_is() {
        assert_eq!(
            with_code("harken://auth", "c d"),
            "harken://auth?code=c%20d"
        );
        assert_eq!(with_code("http://x/?a=1", "c"), "http://x/?a=1&code=c");
    }
}
