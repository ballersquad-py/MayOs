//! Just enough URL handling for a browser: split and resolve links.

use alloc::format;
use alloc::string::{String, ToString};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Path plus query, always starting with '/'.
    pub path: String,
}

impl Url {
    pub fn parse(s: &str) -> Option<Url> {
        let s = s.trim();
        let (scheme, rest) = match s.split_once("://") {
            Some((sc, r)) => (sc.to_ascii_lowercase(), r),
            None => ("http".to_string(), s),
        };
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            "file" => 0,
            _ => return None,
        };
        if scheme == "file" {
            return Some(Url { scheme, host: String::new(), port: 0, path: if rest.starts_with('/') { rest.to_string() } else { format!("/{}", rest) } });
        }
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(p) => (&rest[..p], &rest[p..]),
            None => (rest, "/"),
        };
        let path = path.split('#').next().unwrap_or("/");
        let path = if path.starts_with('?') { format!("/{}", path) } else if path.is_empty() { "/".to_string() } else { path.to_string() };
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, p.parse().ok()?),
            _ => (authority, default_port),
        };
        if host.is_empty() {
            return None;
        }
        Some(Url { scheme, host: host.to_ascii_lowercase(), port, path })
    }

    /// Resolve a link found on this page.
    pub fn join(&self, link: &str) -> Option<Url> {
        let link = link.trim();
        if link.is_empty() || link.starts_with('#') {
            return Some(self.clone());
        }
        if let Some(rest) = link.strip_prefix("//") {
            return Url::parse(&format!("{}://{}", self.scheme, rest));
        }
        if link.contains("://") {
            return Url::parse(link);
        }
        let lower = link.to_ascii_lowercase();
        if lower.starts_with("javascript:") || lower.starts_with("mailto:") || lower.starts_with("data:") || lower.starts_with("tel:") {
            return None;
        }
        let link = link.split('#').next().unwrap_or("");
        let path = if link.starts_with('/') {
            link.to_string()
        } else if link.starts_with('?') {
            format!("{}{}", self.path.split('?').next().unwrap_or("/"), link)
        } else {
            let base = self.path.split('?').next().unwrap_or("/");
            let dir = &base[..base.rfind('/').map(|p| p + 1).unwrap_or(0)];
            format!("{}{}", dir, link)
        };
        Some(Url { path: normalize(&path), ..self.clone() })
    }

    pub fn to_string(&self) -> String {
        if self.scheme == "file" {
            return format!("file://{}", self.path);
        }
        let default = (self.scheme == "http" && self.port == 80) || (self.scheme == "https" && self.port == 443);
        if default { format!("{}://{}{}", self.scheme, self.host, self.path) } else { format!("{}://{}:{}{}", self.scheme, self.host, self.port, self.path) }
    }
}

/// Remove `.` and `..` segments.
fn normalize(path: &str) -> String {
    let (p, q) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let mut parts: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
    for seg in p.split('/') {
        match seg {
            "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut out = parts.join("/");
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    if p.ends_with("/.") || p.ends_with("/..") {
        out.push('/');
    }
    if let Some(q) = q {
        out.push('?');
        out.push_str(q);
    }
    out
}
