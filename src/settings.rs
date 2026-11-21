use std::{
    fmt,
    fs::{self, File, Metadata},
    io::{self, Read},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
};

use reqwest::{Url, header::HeaderValue};
use serde::Deserialize;

use crate::model_http::ApiConfig;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub(crate) listen: SocketAddr,
    pub(crate) web_origin: Option<String>,
    pub(crate) mcp_hosts: Vec<String>,
    pub(crate) mcp_origins: Vec<String>,
    pub(crate) embedding: Option<EmbeddingSettings>,
    pub(crate) rerank: Option<ApiConfig>,
}

#[derive(Deserialize)]
pub(crate) struct EmbeddingSettings {
    #[serde(flatten)]
    pub(crate) api: ApiConfig,
    pub(crate) dimensions: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 7336)),
            web_origin: None,
            // Bracketed authority and bare ::1 match the same IPv6 host in rmcp.
            mcp_hosts: ["localhost", "127.0.0.1", "[::1]"].map(String::from).into(),
            mcp_origins: Vec::new(),
            embedding: None,
            rerank: None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum SettingsError {
    Read(io::ErrorKind),
    NotRegularFile,
    Json {
        line: usize,
        column: usize,
    },
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(kind) => write!(f, "cannot read config.json: {kind}"),
            Self::NotRegularFile => {
                f.write_str("config.json must be a regular file, not a symlink or reparse point")
            }
            Self::Json { line, column } => {
                write!(f, "invalid config.json at line {line}, column {column}")
            }
            Self::Invalid { field, reason } => write!(f, "invalid config.json {field}: {reason}"),
        }
    }
}

impl std::error::Error for SettingsError {}

impl From<io::Error> for SettingsError {
    fn from(error: io::Error) -> Self {
        Self::Read(error.kind())
    }
}

pub(crate) fn read(root: &Path) -> Result<Settings, SettingsError> {
    let path = root.join("config.json");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => require_regular_file(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(error) => return Err(error.into()),
    }

    let mut file = open_direct(&path)?;
    require_regular_file(&file.metadata()?)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut settings: Settings = serde_json::from_slice(&bytes).map_err(|error| {
        // Serde messages can include unexpected keys or values, including secrets.
        SettingsError::Json {
            line: error.line(),
            column: error.column(),
        }
    })?;

    settings.web_origin = settings
        .web_origin
        .as_deref()
        .map(|value| canonical_origin(value, "web_origin"))
        .transpose()?;
    for origin in &mut settings.mcp_origins {
        *origin = canonical_origin(origin, "mcp_origins")?;
    }
    if settings.mcp_hosts.is_empty() {
        return Err(invalid("mcp_hosts", "must contain at least one host"));
    }
    for authority in &settings.mcp_hosts {
        let Some(host) = parse_host(authority, true) else {
            return Err(invalid(
                "mcp_hosts",
                "each entry must be a literal hostname or IP address with an optional numeric port; bracket IPv6 addresses with ports",
            ));
        };
        if matches!(host, Host::Ip(ip) if ip.to_canonical().is_unspecified()) {
            return Err(invalid(
                "mcp_hosts",
                "unspecified IP addresses are not allowed",
            ));
        }
    }
    Ok(settings)
}

impl Settings {
    /// Check the effective address after applying any command-line override.
    pub(crate) fn validate_listen(&self, address: SocketAddr) -> Result<(), SettingsError> {
        if address.ip().to_canonical().is_loopback() {
            return Ok(());
        }
        if self.web_origin.is_none() {
            return Err(invalid(
                "web_origin",
                "required for a non-loopback listen address",
            ));
        }
        if !self
            .mcp_hosts
            .iter()
            .any(|authority| parse_host(authority, true).is_some_and(|host| !host.is_loopback()))
        {
            return Err(invalid(
                "mcp_hosts",
                "at least one non-loopback host is required for a non-loopback listen address",
            ));
        }
        Ok(())
    }
}

fn invalid(field: &'static str, reason: &'static str) -> SettingsError {
    SettingsError::Invalid { field, reason }
}

pub(crate) fn canonical_origin(value: &str, field: &'static str) -> Result<String, SettingsError> {
    let Some((scheme, authority)) = value.split_once("://") else {
        return Err(invalid(field, "must be an explicit HTTP or HTTPS origin"));
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(invalid(field, "scheme must be HTTP or HTTPS"));
    }
    if authority.contains('@') {
        return Err(invalid(field, "credentials are not allowed"));
    }
    if authority.contains(['/', '\\', '?', '#']) {
        return Err(invalid(field, "path, query, and fragment are not allowed"));
    }
    if parse_host(authority, false).is_none() {
        return Err(invalid(field, "invalid host or port"));
    }
    let origin = Url::parse(value)
        .map_err(|_| invalid(field, "invalid HTTP(S) origin"))?
        .origin()
        .ascii_serialization();
    HeaderValue::from_str(&origin).map_err(|_| invalid(field, "not a valid HTTP header value"))?;
    Ok(origin)
}

enum Host<'a> {
    Ip(IpAddr),
    Dns(&'a str),
}

impl Host<'_> {
    fn is_loopback(&self) -> bool {
        match self {
            Self::Ip(ip) => ip.to_canonical().is_loopback(),
            Self::Dns(name) => {
                name.eq_ignore_ascii_case("localhost")
                    || name
                        .rsplit_once('.')
                        .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("localhost"))
            }
        }
    }
}

fn parse_host(authority: &str, allow_bare_ipv6: bool) -> Option<Host<'_>> {
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, tail) = bracketed.split_once(']')?;
        if !tail.is_empty() && !valid_port(tail.strip_prefix(':')?) {
            return None;
        }
        return Some(Host::Ip(host.parse::<Ipv6Addr>().ok()?.into()));
    }
    // rmcp 3.11.0 treats a bare IPv6 literal (including its default ::1) as
    // host-only and strips brackets from parsed HTTP request authorities.
    if allow_bare_ipv6 && let Ok(ip) = authority.parse::<Ipv6Addr>() {
        return Some(Host::Ip(ip.into()));
    }
    let host = if let Some((host, port)) = authority.split_once(':') {
        if !valid_port(port) {
            return None;
        }
        host
    } else {
        authority
    };
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Some(Host::Ip(ip.into()));
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    let last = name.rsplit('.').next()?;
    // Reject alternate IPv4 spellings that browser URL parsers can interpret
    // as IP addresses (e.g. 127.1, 2130706433 and 0x7f000001).
    if last.bytes().all(|byte| byte.is_ascii_digit())
        || last
            .strip_prefix("0x")
            .or_else(|| last.strip_prefix("0X"))
            .is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return None;
    }
    (name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }))
    .then_some(Host::Dns(name))
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

#[cfg(windows)]
fn open_direct(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    File::options()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(target_os = "linux")]
fn open_direct(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

fn require_regular_file(metadata: &Metadata) -> Result<(), SettingsError> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SettingsError::NotRegularFile);
        }
    }
    if !metadata.is_file() || metadata.is_symlink() {
        return Err(SettingsError::NotRegularFile);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_explicit_settings() {
        let dir = tempfile::tempdir().unwrap();
        let settings = read(dir.path()).unwrap();
        assert_eq!(
            settings.listen,
            "127.0.0.1:7336".parse::<SocketAddr>().unwrap()
        );
        assert!(settings.mcp_hosts.iter().any(|host| host == "[::1]"));
        assert!(settings.mcp_origins.is_empty());
        assert!(settings.web_origin.is_none());
        let external = "0.0.0.0:7336".parse().unwrap();
        assert!(settings.validate_listen(settings.listen).is_ok());
        assert!(settings.validate_listen(external).is_err());

        fs::write(
            dir.path().join("config.json"),
            r#"{
                "web_origin": "HTTPS://knowledge.example:443",
                "mcp_hosts": ["localhost", "[2001:db8::1]:7336"],
                "mcp_origins": ["http://[::1]:80", "https://knowledge.example:7336"],
                "embedding": {"base_url":"http://127.0.0.1:8000","model":"embed","dimensions":16},
                "rerank": {"base_url":"http://127.0.0.1:8001","model":"rerank"}
            }"#,
        )
        .unwrap();
        let settings = read(dir.path()).unwrap();
        assert!(settings.validate_listen(external).is_ok());
        assert_eq!(
            settings.web_origin.as_deref(),
            Some("https://knowledge.example")
        );
        assert_eq!(
            settings.mcp_origins,
            ["http://[::1]", "https://knowledge.example:7336"]
        );
        let embedding = settings.embedding.unwrap();
        assert_eq!(embedding.api.model, "embed");
        assert_eq!(embedding.dimensions, 16);
        assert_eq!(settings.rerank.unwrap().model, "rerank");
    }

    #[test]
    fn invalid_config_errors_do_not_echo_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        for content in [
            br#"{"SECRET":"unknown field"}"#.as_slice(),
            br#"{"mcp_hosts":["SECRET:*"]}"#,
            br#"{"web_origin":"https://SECRET@example.com"}"#,
            br#"{"mcp_origins":["https://example.com/SECRET"]}"#,
            b"{\"web_origin\":\"SECRET\xff\"}",
        ] {
            fs::write(&path, content).unwrap();
            let error = read(dir.path()).err().unwrap();
            assert!(!error.to_string().contains("SECRET"));
            assert!(!format!("{error:?}").contains("SECRET"));
        }
    }

    #[test]
    fn authorities_are_unambiguous_and_loopback_is_not_external() {
        for authority in [
            "localhost",
            "team.localhost.:443",
            "127.1.2.3",
            "::1",
            "[::ffff:127.0.0.1]:80",
        ] {
            assert!(parse_host(authority, true).unwrap().is_loopback());
        }
        for authority in [
            "*",
            "localhost:*",
            "127.1",
            "host:65536",
            "[::1]:",
            "host:80:81",
            "host/path",
        ] {
            assert!(parse_host(authority, true).is_none());
        }
    }
}
