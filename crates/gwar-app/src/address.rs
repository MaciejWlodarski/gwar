//! What the user types as a server address, turned into the WebSocket endpoint.
//! Mirrors the desktop rules of `apps/web/src/net/address.ts`: TLS by default,
//! plaintext only to this machine or with an explicit LAN opt-in.

/// Port of a plaintext server when none is given.
pub const DEFAULT_PORT: u16 = 8790;
pub const DEFAULT_PATH: &str = "/ws";

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ServerAddress {
    /// Normalized endpoint to open and pin, e.g. `wss://voice.example.com/ws`.
    pub url: String,
    /// Short human label, e.g. `voice.example.com:8790`.
    pub label: String,
    pub secure: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AddressError {
    Empty,
    Invalid,
    /// Not `ws`, `wss`, `http` or `https`.
    Scheme,
    /// Plaintext to another machine without the LAN opt-in.
    Insecure,
}

pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host == "[::1]" || host == "::1" {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts[0] == "127"
        && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
        && parts.iter().all(|p| p.parse::<u16>().is_ok_and(|n| n <= 255))
}

fn is_ipv6_bare(text: &str) -> bool {
    !text.starts_with('[') && text.matches(':').count() >= 2
}

/// `scheme://` prefix, if the text starts with one.
fn split_scheme(text: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = text.split_once("://")?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    valid.then_some((scheme, rest))
}

/// `javascript:alert(1)`, `mailto:x`: a scheme without slashes.
fn has_slashless_scheme(text: &str) -> bool {
    let Some((scheme, rest)) = text.split_once(':') else { return false };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        && rest.chars().next().is_some_and(|c| !c.is_ascii_digit() && c != ':')
}

/// `host` or `[v6]`, then an optional `:port`.
fn split_host_port(authority: &str) -> Option<(String, Option<u16>)> {
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (v6, rest) = inner.split_once(']')?;
        if v6.is_empty() || !v6.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.') {
            return None;
        }
        (format!("[{v6}]"), rest)
    } else {
        let end = authority.find(':').unwrap_or(authority.len());
        let host = &authority[..end];
        if host.is_empty() || host.chars().any(|c| matches!(c, '[' | ']' | '/') || c.is_whitespace()) {
            return None;
        }
        (host.to_owned(), &authority[end..])
    };
    let port = match port {
        "" => None,
        p => {
            let digits = p.strip_prefix(':')?;
            if digits.is_empty() || digits.len() > 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            match digits.parse::<u32>().ok()? {
                n @ 1..=65535 => Some(n as u16),
                _ => return None,
            }
        }
    };
    Some((host.to_ascii_lowercase().trim_end_matches('.').to_owned(), port))
}

pub fn parse_server_address(input: &str, insecure_lan: bool) -> Result<ServerAddress, AddressError> {
    let mut text = input.trim();
    if text.is_empty() {
        return Err(AddressError::Empty);
    }
    let mut scheme = None;
    if let Some((s, rest)) = split_scheme(text) {
        scheme = Some(match s.to_ascii_lowercase().as_str() {
            "ws" | "http" => false,
            "wss" | "https" => true,
            _ => return Err(AddressError::Scheme),
        });
        text = rest;
    } else if has_slashless_scheme(text) && !is_ipv6_bare(text) {
        return Err(AddressError::Scheme);
    }

    let slash = text.find(['/', '?', '#']).unwrap_or(text.len());
    let (authority, path) = text.split_at(slash);
    if path.contains(['?', '#']) || text.contains(|c: char| c.is_whitespace() || c == '\\') || authority.contains('@') {
        return Err(AddressError::Invalid);
    }
    let authority = if is_ipv6_bare(authority) { format!("[{authority}]") } else { authority.to_owned() };
    let (host, mut port) = split_host_port(&authority).ok_or(AddressError::Invalid)?;
    if host.is_empty() || host.starts_with('.') || host.contains("..") {
        return Err(AddressError::Invalid);
    }

    let loopback = is_loopback_host(&host);
    let secure = scheme.unwrap_or(!(loopback || insecure_lan));
    if !secure && !loopback && !insecure_lan {
        return Err(AddressError::Insecure);
    }
    if port.is_none() && scheme.is_none() && !secure {
        port = Some(DEFAULT_PORT);
    }
    let path = if path.is_empty() || path == "/" { DEFAULT_PATH } else { path };
    let raw = format!(
        "{}://{host}{}{path}",
        if secure { "wss" } else { "ws" },
        port.map(|p| format!(":{p}")).unwrap_or_default()
    );
    let url = vc_proto::normalize_endpoint(&raw).ok_or(AddressError::Invalid)?;
    let parsed = url.parse::<UrlParts>().map_err(|_| AddressError::Invalid)?;
    let label = format!("{}{}", parsed.host_port, if parsed.path == DEFAULT_PATH { "" } else { &parsed.path });
    Ok(ServerAddress { url, label, secure })
}

/// Host, port and path of a normalized endpoint, for its label.
struct UrlParts {
    host_port: String,
    path: String,
}

impl std::str::FromStr for UrlParts {
    type Err = ();

    fn from_str(url: &str) -> Result<Self, ()> {
        let rest = url.split_once("://").ok_or(())?.1;
        let slash = rest.find('/').unwrap_or(rest.len());
        Ok(Self { host_port: rest[..slash].to_owned(), path: rest[slash..].to_owned() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(input: &str) -> String {
        match parse_server_address(input, false) {
            Ok(a) => a.url,
            Err(e) => format!("{e:?}").to_lowercase(),
        }
    }

    #[test]
    fn domain_names_use_tls_and_local_hosts_plain_ws() {
        assert_eq!(url("voice.example.com"), "wss://voice.example.com/ws");
        assert_eq!(url("Voice.Example.com."), "wss://voice.example.com/ws");
        assert_eq!(url("voice.example.com:8790"), "wss://voice.example.com:8790/ws");
        assert_eq!(url("1.2.3.4"), "wss://1.2.3.4/ws");
        assert_eq!(url("localhost"), "ws://localhost:8790/ws");
        assert_eq!(url("127.0.0.1:9000"), "ws://127.0.0.1:9000/ws");
        assert_eq!(url("::1"), "ws://[::1]:8790/ws");
        assert_eq!(url("nas"), "wss://nas/ws");
        assert_eq!(url("https://192.168.1.10"), "wss://192.168.1.10/ws");
        assert_eq!(url("wss://voice.example.com/gwar"), "wss://voice.example.com/gwar");
    }

    #[test]
    fn plaintext_to_another_machine_needs_the_lan_opt_in() {
        assert_eq!(url("ws://voice.example.com"), "insecure");
        assert_eq!(url("ws://127.attacker.example"), "insecure");
        let lan = parse_server_address("ws://192.168.1.2:8790", true).unwrap();
        assert_eq!((lan.url.as_str(), lan.secure), ("ws://192.168.1.2:8790/ws", false));
        // The emulator reaches the host machine as 10.0.2.2.
        assert_eq!(parse_server_address("10.0.2.2", true).unwrap().url, "ws://10.0.2.2:8790/ws");
        assert_eq!(parse_server_address("wss://10.0.2.2", true).unwrap().url, "wss://10.0.2.2/ws");
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(url(""), "empty");
        assert_eq!(url("   "), "empty");
        assert_eq!(url("javascript:alert(1)"), "scheme");
        assert_eq!(url("ftp://x"), "scheme");
        assert_eq!(url("user@host"), "invalid");
        assert_eq!(url("host/ws?x=1"), "invalid");
        assert_eq!(url("host name"), "invalid");
        assert_eq!(url("host:0"), "invalid");
        assert_eq!(url("host:65536"), "invalid");
        assert_eq!(url("a..b"), "invalid");
        assert_eq!(url(".a"), "invalid");
    }

    #[test]
    fn labels_are_short() {
        assert_eq!(parse_server_address("voice.example.com", false).unwrap().label, "voice.example.com");
        assert_eq!(parse_server_address("localhost", false).unwrap().label, "localhost:8790");
        assert_eq!(parse_server_address("wss://h/x", false).unwrap().label, "h/x");
    }
}
