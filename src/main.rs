#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::{
    env, fs,
    io::{self, ErrorKind, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned, pki_types::ServerName};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;
const DEFAULT_CONFIG: &str = "config.toml";
const SERVER_HEADER: &str = "pear";
const DEFAULT_KEEP_ALIVE_TIMEOUT_SECS: u64 = 5;
const DEFAULT_KEEP_ALIVE_MAX: usize = 100;
const DEFAULT_MAX_EVENTS: usize = 256;
const DEFAULT_MAX_CONNECTIONS: usize = 4096;
const DEFAULT_MAX_REQUEST_SIZE: usize = 1024 * 1024;
const MAX_AUTO_CONNECTIONS: usize = 16_384;
const RESERVED_FDS: usize = 128;
const MIN_CONNECTIONS: usize = 1;
static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct Config {
    host: String,
    port: u16,
    root: PathBuf,
    spa_fallback: bool,
    proxies: Vec<ProxyRule>,
    runtime: RuntimeConfig,
}

#[derive(Clone, Debug)]
struct RuntimeConfig {
    keep_alive_timeout_secs: u64,
    keep_alive_max: usize,
    max_events: usize,
    max_connections: usize,
    max_request_size: usize,
}

#[derive(Clone, Debug)]
struct ProxyRule {
    prefix: String,
    upstream: Upstream,
}

#[derive(Clone, Debug)]
struct Upstream {
    scheme: UpstreamScheme,
    host: String,
    port: u16,
    base_path: String,
    base_query: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpstreamScheme {
    Http,
    Https,
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Default)]
struct PartialProxy {
    prefix: Option<String>,
    target: Option<String>,
}

enum ConfigSection {
    Root,
    Proxy(PartialProxy),
    ProxyMap,
}

fn main() {
    let config = Config::parse().unwrap_or_else(|err| {
        eprintln!("{err}");
        eprintln!();
        print_usage();
        process::exit(2);
    });

    let root = fs::canonicalize(&config.root).unwrap_or_else(|err| {
        eprintln!("Cannot read directory '{}': {err}", config.root.display());
        process::exit(1);
    });

    if !root.is_dir() {
        eprintln!("Root path is not a directory: {}", root.display());
        process::exit(1);
    }

    #[cfg(target_os = "linux")]
    {
        if let Err(err) = linux::run_server(config, root) {
            eprintln!("Server failed: {err}");
            process::exit(1);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        if let Err(err) = compat::run_server(config, root) {
            eprintln!("Server failed: {err}");
            process::exit(1);
        }
    }
}

impl Config {
    fn parse() -> Result<Self, String> {
        let args = env::args().skip(1).collect::<Vec<_>>();
        let (config_path, explicit_config) = config_path_from_args(&args)?;
        let mut config = Self::from_file_if_present(&config_path, explicit_config)?;

        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    process::exit(0);
                }
                "-H" | "--host" => {
                    index += 1;
                    config.host = args
                        .get(index)
                        .ok_or_else(|| "Missing value for --host".to_string())?
                        .to_string();
                }
                "-p" | "--port" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "Missing value for --port".to_string())?;
                    config.port = value
                        .parse()
                        .map_err(|_| format!("Invalid port value: {value}"))?;
                }
                "-d" | "--dir" => {
                    index += 1;
                    config.root = PathBuf::from(
                        args.get(index)
                            .ok_or_else(|| "Missing value for --dir".to_string())?,
                    );
                }
                "-c" | "--config" => {
                    index += 1;
                    args.get(index)
                        .ok_or_else(|| "Missing value for --config".to_string())?;
                }
                "--no-spa" => config.spa_fallback = false,
                "--keep-alive-timeout" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "Missing value for --keep-alive-timeout".to_string())?;
                    config.runtime.keep_alive_timeout_secs =
                        parse_usize_arg(value, "--keep-alive-timeout")? as u64;
                }
                "--keep-alive-max" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "Missing value for --keep-alive-max".to_string())?;
                    config.runtime.keep_alive_max = parse_usize_arg(value, "--keep-alive-max")?;
                }
                "--max-events" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "Missing value for --max-events".to_string())?;
                    config.runtime.max_events = parse_usize_arg(value, "--max-events")?;
                }
                "--max-connections" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "Missing value for --max-connections".to_string())?;
                    config.runtime.max_connections = parse_usize_arg(value, "--max-connections")?;
                }
                "--max-request-size" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "Missing value for --max-request-size".to_string())?;
                    config.runtime.max_request_size = parse_usize_arg(value, "--max-request-size")?;
                }
                "--proxy" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "Missing value for --proxy".to_string())?;
                    config.proxies.push(ProxyRule::parse(value)?);
                }
                value if value.starts_with('-') => {
                    return Err(format!("Unknown option: {value}"));
                }
                value => config.root = PathBuf::from(value),
            }
            index += 1;
        }

        config.runtime.adjust_to_os_fd_limit();
        config.runtime.validate()?;
        Ok(config)
    }

    fn default_values() -> Result<Self, String> {
        Ok(Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            root: env::current_dir().map_err(|err| format!("Cannot read current dir: {err}"))?,
            spa_fallback: true,
            proxies: Vec::new(),
            runtime: RuntimeConfig::default_values(),
        })
    }

    fn from_file_if_present(path: &Path, explicit: bool) -> Result<Self, String> {
        let mut config = Self::default_values()?;
        if path.exists() {
            config.apply_toml(path)?;
        } else if explicit {
            return Err(format!("Config file not found: {}", path.display()));
        }
        Ok(config)
    }

    fn apply_toml(&mut self, path: &Path) -> Result<(), String> {
        let text = fs::read_to_string(path)
            .map_err(|err| format!("Cannot read config '{}': {err}", path.display()))?;
        let mut section = ConfigSection::Root;

        for (line_number, raw_line) in text.lines().enumerate() {
            let line_number = line_number + 1;
            let line = strip_inline_comment(raw_line).trim().to_string();
            if line.is_empty() {
                continue;
            }

            if line == "[[proxy]]" || line == "[[proxies]]" {
                flush_proxy_section(&mut section, &mut self.proxies, path, line_number)?;
                section = ConfigSection::Proxy(PartialProxy::default());
                continue;
            }

            if line == "[proxy]" || line == "[proxies]" {
                flush_proxy_section(&mut section, &mut self.proxies, path, line_number)?;
                section = ConfigSection::ProxyMap;
                continue;
            }

            if line.starts_with('[') {
                return Err(format!(
                    "{}:{line_number}: unsupported TOML section: {line}",
                    path.display()
                ));
            }

            let (key, value) = line.split_once('=').ok_or_else(|| {
                format!(
                    "{}:{line_number}: expected key = value, got: {line}",
                    path.display()
                )
            })?;
            let key = parse_key(key.trim());
            let value = value.trim();

            match &mut section {
                ConfigSection::Root => self.apply_root_config(&key, value, path, line_number)?,
                ConfigSection::Proxy(proxy) => match key.as_str() {
                    "prefix" => proxy.prefix = Some(parse_string(value, path, line_number)?),
                    "target" | "upstream" => {
                        proxy.target = Some(parse_string(value, path, line_number)?)
                    }
                    _ => {
                        return Err(format!(
                            "{}:{line_number}: unsupported proxy key: {key}",
                            path.display()
                        ));
                    }
                },
                ConfigSection::ProxyMap => {
                    let target = parse_string(value, path, line_number)?;
                    self.proxies.push(ProxyRule::new(&key, &target)?);
                }
            }
        }

        flush_proxy_section(&mut section, &mut self.proxies, path, text.lines().count())?;
        Ok(())
    }

    fn apply_root_config(
        &mut self,
        key: &str,
        value: &str,
        path: &Path,
        line_number: usize,
    ) -> Result<(), String> {
        match key {
            "host" => self.host = parse_string(value, path, line_number)?,
            "port" => self.port = parse_u16(value, path, line_number)?,
            "root" | "dir" => self.root = PathBuf::from(parse_string(value, path, line_number)?),
            "spa_fallback" => self.spa_fallback = parse_bool(value, path, line_number)?,
            "keep_alive_timeout" | "keep_alive_timeout_secs" => {
                self.runtime.keep_alive_timeout_secs = parse_usize(value, path, line_number)? as u64
            }
            "keep_alive_max" => {
                self.runtime.keep_alive_max = parse_usize(value, path, line_number)?
            }
            "max_events" => self.runtime.max_events = parse_usize(value, path, line_number)?,
            "max_connections" => {
                self.runtime.max_connections = parse_usize(value, path, line_number)?
            }
            "max_request_size" => {
                self.runtime.max_request_size = parse_usize(value, path, line_number)?
            }
            _ => {
                return Err(format!(
                    "{}:{line_number}: unsupported config key: {key}",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

impl RuntimeConfig {
    fn default_values() -> Self {
        Self {
            keep_alive_timeout_secs: DEFAULT_KEEP_ALIVE_TIMEOUT_SECS,
            keep_alive_max: DEFAULT_KEEP_ALIVE_MAX,
            max_events: DEFAULT_MAX_EVENTS,
            max_connections: default_max_connections(),
            max_request_size: DEFAULT_MAX_REQUEST_SIZE,
        }
    }

    fn keep_alive_timeout(&self) -> Duration {
        Duration::from_secs(self.keep_alive_timeout_secs)
    }

    fn validate(&self) -> Result<(), String> {
        if self.keep_alive_timeout_secs == 0 {
            return Err("keep-alive timeout must be greater than 0".to_string());
        }
        if self.keep_alive_max == 0 {
            return Err("keep-alive max must be greater than 0".to_string());
        }
        if self.max_events == 0 {
            return Err("max-events must be greater than 0".to_string());
        }
        if self.max_connections == 0 {
            return Err("max-connections must be greater than 0".to_string());
        }
        if self.max_request_size == 0 {
            return Err("max-request-size must be greater than 0".to_string());
        }
        Ok(())
    }

    fn adjust_to_os_fd_limit(&mut self) {
        let Some(limit) = os_fd_limit() else {
            return;
        };

        let capped = limit
            .saturating_sub(RESERVED_FDS)
            .clamp(MIN_CONNECTIONS, MAX_AUTO_CONNECTIONS);

        if self.max_connections > capped {
            eprintln!(
                "Warning: max_connections={} exceeds OS file descriptor limit {} (reserved {}), capping to {}",
                self.max_connections, limit, RESERVED_FDS, capped
            );
            self.max_connections = capped;
        }
    }
}

fn config_path_from_args(args: &[String]) -> Result<(PathBuf, bool), String> {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-c" | "--config" => {
                index += 1;
                return args
                    .get(index)
                    .map(PathBuf::from)
                    .map(|path| (path, true))
                    .ok_or_else(|| "Missing value for --config".to_string());
            }
            _ => index += 1,
        }
    }

    Ok((PathBuf::from(DEFAULT_CONFIG), false))
}

fn flush_proxy_section(
    section: &mut ConfigSection,
    proxies: &mut Vec<ProxyRule>,
    path: &Path,
    line_number: usize,
) -> Result<(), String> {
    let current = std::mem::replace(section, ConfigSection::Root);
    if let ConfigSection::Proxy(proxy) = current {
        let prefix = proxy.prefix.ok_or_else(|| {
            format!(
                "{}:{line_number}: proxy section is missing prefix",
                path.display()
            )
        })?;
        let target = proxy.target.ok_or_else(|| {
            format!(
                "{}:{line_number}: proxy section is missing target",
                path.display()
            )
        })?;
        proxies.push(ProxyRule::new(&prefix, &target)?);
    }
    Ok(())
}

fn strip_inline_comment(line: &str) -> String {
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in line.char_indices() {
        match ch {
            '"' if !escaped => in_string = !in_string,
            '#' if !in_string => return line[..index].to_string(),
            _ => {}
        }
        escaped = ch == '\\' && !escaped;
        if ch != '\\' {
            escaped = false;
        }
    }

    line.to_string()
}

fn parse_key(value: &str) -> String {
    parse_quoted_string(value).unwrap_or_else(|| value.to_string())
}

fn parse_string(value: &str, path: &Path, line_number: usize) -> Result<String, String> {
    parse_quoted_string(value).ok_or_else(|| {
        format!(
            "{}:{line_number}: expected quoted string, got: {value}",
            path.display()
        )
    })
}

fn parse_quoted_string(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes.first() != Some(&b'"') || bytes.last() != Some(&b'"') {
        return None;
    }

    let inner = &value[1..value.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }

    Some(out)
}

fn parse_bool(value: &str, path: &Path, line_number: usize) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!(
            "{}:{line_number}: expected boolean, got: {value}",
            path.display()
        )),
    }
}

fn parse_u16(value: &str, path: &Path, line_number: usize) -> Result<u16, String> {
    value.parse().map_err(|_| {
        format!(
            "{}:{line_number}: expected port number, got: {value}",
            path.display()
        )
    })
}

fn parse_usize(value: &str, path: &Path, line_number: usize) -> Result<usize, String> {
    value.parse().map_err(|_| {
        format!(
            "{}:{line_number}: expected integer, got: {value}",
            path.display()
        )
    })
}

fn parse_usize_arg(value: &str, option: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("Invalid value for {option}: {value}"))
}

fn default_max_connections() -> usize {
    let derived = os_fd_limit()
        .map(|limit| limit.saturating_sub(RESERVED_FDS))
        .unwrap_or(DEFAULT_MAX_CONNECTIONS);

    derived.clamp(MIN_CONNECTIONS, MAX_AUTO_CONNECTIONS)
}

#[cfg(unix)]
fn os_fd_limit() -> Option<usize> {
    #[repr(C)]
    struct Rlimit {
        rlim_cur: u64,
        rlim_max: u64,
    }

    unsafe extern "C" {
        fn getrlimit(resource: i32, rlim: *mut Rlimit) -> i32;
    }

    #[cfg(target_os = "linux")]
    const RLIMIT_NOFILE: i32 = 7;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const RLIMIT_NOFILE: i32 = 8;
    #[cfg(all(
        unix,
        not(any(target_os = "linux", target_os = "macos", target_os = "ios"))
    ))]
    const RLIMIT_NOFILE: i32 = 7;

    let mut limit = Rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let result = unsafe { getrlimit(RLIMIT_NOFILE, &mut limit) };
    if result == 0 {
        usize::try_from(limit.rlim_cur).ok()
    } else {
        None
    }
}

#[cfg(not(unix))]
fn os_fd_limit() -> Option<usize> {
    None
}

impl ProxyRule {
    fn new(prefix: &str, target: &str) -> Result<Self, String> {
        if !prefix.starts_with('/') {
            return Err(format!("Proxy prefix must start with '/': {prefix}"));
        }

        Ok(Self {
            prefix: trim_proxy_prefix(prefix),
            upstream: Upstream::parse(target)?,
        })
    }

    fn parse(value: &str) -> Result<Self, String> {
        let (prefix, target) = value.split_once('=').ok_or_else(|| {
            format!("Invalid proxy rule '{value}', expected /path=http[s]://host:port")
        })?;

        Self::new(prefix, target)
    }

    fn matches(&self, path: &str) -> bool {
        path == self.prefix
            || path
                .strip_prefix(&self.prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

impl Upstream {
    fn parse(value: &str) -> Result<Self, String> {
        let (scheme, default_port, rest) = if let Some(rest) = value.strip_prefix("http://") {
            (UpstreamScheme::Http, 80, rest)
        } else if let Some(rest) = value.strip_prefix("https://") {
            (UpstreamScheme::Https, 443, rest)
        } else {
            return Err(format!(
                "Proxy target must start with http:// or https://: {value}"
            ));
        };
        let split_index = rest.find(['/', '?']);
        let (authority, path_and_query) = match split_index {
            Some(index) if rest.as_bytes()[index] == b'/' => (&rest[..index], &rest[index + 1..]),
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return Err(format!("Missing proxy upstream host: {value}"));
        }

        let (host, port) = parse_upstream_authority(authority, default_port)?;
        let (base_path, base_query) = parse_base_path_and_query(path_and_query);

        Ok(Self {
            scheme,
            host,
            port,
            base_path,
            base_query,
        })
    }

    fn scheme_name(&self) -> &'static str {
        match self.scheme {
            UpstreamScheme::Http => "http",
            UpstreamScheme::Https => "https",
        }
    }

    fn authority_header(&self) -> String {
        let default_port = match self.scheme {
            UpstreamScheme::Http => 80,
            UpstreamScheme::Https => 443,
        };
        if self.port == default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_upstream_authority(authority: &str, default_port: u16) -> Result<(String, u16), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest
            .split_once(']')
            .ok_or_else(|| format!("Invalid IPv6 proxy upstream authority: {authority}"))?;
        if host.is_empty() {
            return Err(format!("Missing proxy upstream host: {authority}"));
        }
        if port.is_empty() {
            return Ok((host.to_string(), default_port));
        }
        let port = port
            .strip_prefix(':')
            .ok_or_else(|| format!("Invalid IPv6 proxy upstream authority: {authority}"))?
            .parse()
            .map_err(|_| format!("Invalid proxy upstream port in authority: {authority}"))?;
        return Ok((host.to_string(), port));
    }

    let colon_count = authority.bytes().filter(|byte| *byte == b':').count();
    if colon_count > 1 {
        return Ok((authority.to_string(), default_port));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port
                .parse()
                .map_err(|_| format!("Invalid proxy upstream port: {port}"))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

impl Request {
    fn parse_from_buffer(buffer: &[u8]) -> io::Result<Option<(Self, usize)>> {
        let header_end = match find_header_end(buffer) {
            Some(index) => index,
            None => return Ok(None),
        };

        let header_bytes = &buffer[..header_end];
        let header_text = std::str::from_utf8(header_bytes)
            .map_err(|_| io::Error::new(ErrorKind::InvalidData, "invalid request headers"))?;

        let mut lines = header_text.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "missing request line"))?;
        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "missing method"))?;
        let target = parts
            .next()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "missing target"))?;
        let version = parts.next().unwrap_or("HTTP/1.1");

        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "invalid header"))?;
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }

        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let total_length = header_end + 4 + content_length;
        if buffer.len() < total_length {
            return Ok(None);
        }

        let body = buffer[header_end + 4..total_length].to_vec();
        Ok(Some((
            Self {
                method: method.to_string(),
                target: target.to_string(),
                version: version.to_string(),
                headers,
                body,
            },
            total_length,
        )))
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn handle_request(
    request: &Request,
    root: &Path,
    spa_fallback: bool,
    proxies: &[ProxyRule],
    runtime: &RuntimeConfig,
    keep_alive: bool,
) -> io::Result<Response> {
    if let Some(proxy) = find_proxy(proxies, &request.target) {
        if has_chunked_transfer_encoding(request) {
            return Ok(Response::close(build_response(
                501,
                "Not Implemented",
                "text/plain",
                b"Chunked transfer encoding is not supported",
                false,
                runtime,
            )));
        }

        return match proxy_request(request, proxy, runtime, keep_alive) {
            Ok(response) => Ok(response),
            Err(err) => {
                eprintln!("Proxy request failed: {err}");
                Ok(Response::close(build_response(
                    502,
                    "Bad Gateway",
                    "text/plain",
                    b"Bad Gateway",
                    false,
                    runtime,
                )))
            }
        };
    }

    serve_static(request, root, spa_fallback, runtime, keep_alive)
}

fn wants_keep_alive(request: &Request) -> bool {
    if request.version == "HTTP/1.0" {
        return header_value(request, "connection")
            .is_some_and(|value| value.eq_ignore_ascii_case("keep-alive"));
    }

    !header_value(request, "connection").is_some_and(|value| value.eq_ignore_ascii_case("close"))
}

fn serve_static(
    request: &Request,
    root: &Path,
    spa_fallback: bool,
    runtime: &RuntimeConfig,
    keep_alive: bool,
) -> io::Result<Response> {
    if request.method != "GET" && request.method != "HEAD" {
        return Ok(Response::new(build_response(
            405,
            "Method Not Allowed",
            "text/plain",
            b"Method Not Allowed",
            keep_alive,
            runtime,
        )));
    }

    let Some(request_path) = clean_request_path(&request.target) else {
        return Ok(Response::new(build_response(
            400,
            "Bad Request",
            "text/plain",
            b"Bad Request",
            false,
            runtime,
        )));
    };

    let mut path = root.join(&request_path);
    let (file_path, meta) = match fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => {
            path = path.join("index.html");
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => (path, meta),
                _ if spa_fallback => {
                    let path = root.join("index.html");
                    match fs::metadata(&path) {
                        Ok(meta) if meta.is_file() => (path, meta),
                        _ => {
                            return Ok(Response::new(build_response(
                                404,
                                "Not Found",
                                "text/plain",
                                b"Not Found",
                                keep_alive,
                                runtime,
                            )));
                        }
                    }
                }
                _ => {
                    return Ok(Response::new(build_response(
                        404,
                        "Not Found",
                        "text/plain",
                        b"Not Found",
                        keep_alive,
                        runtime,
                    )));
                }
            }
        }
        Ok(meta) if meta.is_file() => (path, meta),
        _ if spa_fallback => {
            let path = root.join("index.html");
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => (path, meta),
                _ => {
                    return Ok(Response::new(build_response(
                        404,
                        "Not Found",
                        "text/plain",
                        b"Not Found",
                        keep_alive,
                        runtime,
                    )));
                }
            }
        }
        _ => {
            return Ok(Response::new(build_response(
                404,
                "Not Found",
                "text/plain",
                b"Not Found",
                keep_alive,
                runtime,
            )));
        }
    };

    if !is_inside(root, &file_path) {
        return Ok(Response::new(build_response(
            404,
            "Not Found",
            "text/plain",
            b"Not Found",
            keep_alive,
            runtime,
        )));
    }

    let content_type = content_type(&file_path);
    let len = meta.len();
    if request.method == "HEAD" {
        return Ok(Response::new(build_head_response(
            200,
            "OK",
            content_type,
            len as usize,
            keep_alive,
            runtime,
        )));
    }

    let mut file = fs::File::open(&file_path)?;
    if len <= INLINE_FILE_THRESHOLD {
        let mut body = Vec::with_capacity(len as usize);
        file.read_to_end(&mut body)?;
        let mut bytes =
            build_head_response(200, "OK", content_type, body.len(), keep_alive, runtime);
        bytes.extend_from_slice(&body);
        return Ok(Response::new(bytes));
    }

    Ok(Response::streamed(
        build_head_response(200, "OK", content_type, len as usize, keep_alive, runtime),
        file,
    ))
}

fn find_proxy<'a>(proxies: &'a [ProxyRule], target: &str) -> Option<&'a ProxyRule> {
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    proxies
        .iter()
        .filter(|proxy| proxy.matches(path))
        .max_by_key(|proxy| proxy.prefix.len())
}

enum UpstreamStream {
    Plain(TcpStream),
    Tls(StreamOwned<ClientConnection, TcpStream>),
}

impl Read for UpstreamStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for UpstreamStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buf),
            Self::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

impl UpstreamStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.set_read_timeout(timeout),
            Self::Tls(stream) => stream.sock.set_read_timeout(timeout),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.set_write_timeout(timeout),
            Self::Tls(stream) => stream.sock.set_write_timeout(timeout),
        }
    }

    fn shutdown_write(&self) -> io::Result<()> {
        match self {
            // For plain HTTP upstreams we can half-close write to signal request completion.
            Self::Plain(stream) => stream.shutdown(Shutdown::Write),
            // For TLS upstreams, shutting down the underlying TCP write side bypasses TLS
            // close-notify semantics and can trigger premature EOF behavior from some peers.
            // We keep the TLS connection open and rely on `Connection: close` plus response
            // framing (Content-Length / EOF) to finish the exchange.
            Self::Tls(_) => Ok(()),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn connect_upstream(proxy: &ProxyRule) -> io::Result<UpstreamStream> {
    connect_upstream_with_root_store(proxy, default_root_store())
}

fn connect_upstream_with_root_store(
    proxy: &ProxyRule,
    root_store: RootCertStore,
) -> io::Result<UpstreamStream> {
    let tcp = TcpStream::connect((proxy.upstream.host.as_str(), proxy.upstream.port))?;
    match proxy.upstream.scheme {
        UpstreamScheme::Http => Ok(UpstreamStream::Plain(tcp)),
        UpstreamScheme::Https => {
            let config = ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            let server_name = ServerName::try_from(proxy.upstream.host.clone())
                .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid upstream host"))?;
            let connection = ClientConnection::new(std::sync::Arc::new(config), server_name)
                .map_err(|err| io::Error::other(format!("TLS setup failed: {err}")))?;
            Ok(UpstreamStream::Tls(StreamOwned::new(connection, tcp)))
        }
    }
}

fn default_root_store() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

fn write_proxy_request(
    upstream: &mut UpstreamStream,
    request: &Request,
    proxy: &ProxyRule,
) -> io::Result<()> {
    let target = proxied_target(&request.target, proxy);
    write!(
        upstream,
        "{} {} {}\r\n",
        request.method, target, request.version
    )?;
    write!(upstream, "Host: {}\r\n", proxy.upstream.authority_header())?;
    write!(
        upstream,
        "X-Forwarded-Host: {}\r\n",
        header_value(request, "host").unwrap_or("")
    )?;
    write!(
        upstream,
        "X-Forwarded-Proto: {}\r\n",
        forwarded_proto(request)
    )?;

    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("proxy-connection")
        {
            continue;
        }
        write!(upstream, "{name}: {value}\r\n")?;
    }

    write!(upstream, "Connection: close\r\n\r\n")?;
    upstream.write_all(&request.body)?;
    upstream.flush()?;
    let _ = upstream.shutdown_write();
    Ok(())
}

fn forwarded_proto(request: &Request) -> &str {
    header_value(request, "x-forwarded-proto").unwrap_or("http")
}

fn proxy_request(
    request: &Request,
    proxy: &ProxyRule,
    runtime: &RuntimeConfig,
    keep_alive: bool,
) -> io::Result<Response> {
    proxy_request_with_root_store(request, proxy, runtime, keep_alive, default_root_store())
}

fn proxy_request_with_root_store(
    request: &Request,
    proxy: &ProxyRule,
    runtime: &RuntimeConfig,
    keep_alive: bool,
    root_store: RootCertStore,
) -> io::Result<Response> {
    let mut upstream = connect_upstream_with_root_store(proxy, root_store)?;
    upstream.set_read_timeout(Some(Duration::from_secs(30)))?;
    upstream.set_write_timeout(Some(Duration::from_secs(30)))?;
    write_proxy_request(&mut upstream, request, proxy)?;

    let mut received = Vec::with_capacity(8192);
    let mut buf = [0_u8; 8192];
    let header_end = loop {
        if let Some(index) = find_header_end(&received) {
            break index;
        }

        let read = upstream.read(&mut buf)?;
        if read == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "upstream closed before sending headers",
            ));
        }
        received.extend_from_slice(&buf[..read]);
    };

    let upstream_head = &received[..header_end];
    let content_length = upstream_content_length(upstream_head);
    let head = rewrite_proxy_response_head(upstream_head, runtime, keep_alive)?;
    let body_start = header_end + 4;
    if content_length == Some(0) {
        return Ok(Response::new(head));
    }

    let mut temp = TempBodyFile::new()?;
    let mut copied = 0;
    if body_start < received.len() {
        temp.write_all(&received[body_start..])?;
        copied = received.len() - body_start;
    }
    copy_upstream_body(&mut upstream, &mut temp, copied, content_length)?;
    temp.rewind()?;
    Ok(Response::with_temp_body(head, temp))
}

#[cfg(not(target_os = "linux"))]
fn proxy_request_streaming<W: Write>(
    request: &Request,
    proxy: &ProxyRule,
    runtime: &RuntimeConfig,
    keep_alive: bool,
    writer: &mut W,
) -> io::Result<()> {
    let mut upstream = connect_upstream(proxy)?;
    upstream.set_read_timeout(Some(Duration::from_secs(30)))?;
    upstream.set_write_timeout(Some(Duration::from_secs(30)))?;
    write_proxy_request(&mut upstream, request, proxy)?;

    let mut received = Vec::with_capacity(8192);
    let mut buf = [0_u8; 8192];
    let header_end = loop {
        if let Some(index) = find_header_end(&received) {
            break index;
        }

        let read = upstream.read(&mut buf)?;
        if read == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "upstream closed before sending headers",
            ));
        }
        received.extend_from_slice(&buf[..read]);
    };

    let upstream_head = &received[..header_end];
    let content_length = upstream_content_length(upstream_head);
    let head = rewrite_proxy_response_head(upstream_head, runtime, keep_alive)?;
    writer.write_all(&head)?;

    let body_start = header_end + 4;
    let mut copied = 0;
    if body_start < received.len() {
        writer.write_all(&received[body_start..])?;
        copied = received.len() - body_start;
    }
    copy_upstream_body(&mut upstream, writer, copied, content_length)?;
    Ok(())
}

fn upstream_content_length(header_bytes: &[u8]) -> Option<usize> {
    let header_text = std::str::from_utf8(header_bytes).ok()?;
    for line in header_text.split("\r\n").skip(1) {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse().ok();
        }
    }
    None
}

fn copy_upstream_body<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut copied: usize,
    content_length: Option<usize>,
) -> io::Result<()> {
    let mut buf = [0_u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                writer.write_all(&buf[..read])?;
                copied += read;
                if content_length.is_some_and(|expected| copied >= expected) {
                    return Ok(());
                }
            }
            Err(err) if should_treat_upstream_eof_as_clean_close(&err, copied, content_length) => {
                return Ok(());
            }
            Err(err) => return Err(err),
        }
    }
}

fn should_treat_upstream_eof_as_clean_close(
    err: &io::Error,
    copied: usize,
    content_length: Option<usize>,
) -> bool {
    if !is_tls_close_notify_eof(err) {
        return false;
    }

    match content_length {
        Some(expected) => copied >= expected,
        None => copied > 0,
    }
}

fn is_tls_close_notify_eof(err: &io::Error) -> bool {
    err.kind() == ErrorKind::UnexpectedEof
        && err
            .to_string()
            .contains("peer closed connection without sending TLS close_notify")
}

fn rewrite_proxy_response_head(
    header_bytes: &[u8],
    runtime: &RuntimeConfig,
    keep_alive: bool,
) -> io::Result<Vec<u8>> {
    let header_text = std::str::from_utf8(header_bytes)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "invalid upstream response headers"))?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "missing upstream status line"))?;

    let mut rewritten = Vec::with_capacity(header_bytes.len() + 64);
    rewritten.extend_from_slice(status_line.as_bytes());
    rewritten.extend_from_slice(b"\r\n");

    for line in lines {
        if line.is_empty() {
            continue;
        }

        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "invalid upstream header"))?;
        if is_hop_by_hop_header(name) {
            continue;
        }

        rewritten.extend_from_slice(name.trim().as_bytes());
        rewritten.extend_from_slice(b": ");
        rewritten.extend_from_slice(value.trim().as_bytes());
        rewritten.extend_from_slice(b"\r\n");
    }

    rewritten.extend_from_slice(connection_headers(runtime, keep_alive).as_bytes());
    rewritten.extend_from_slice(b"\r\n");
    Ok(rewritten)
}

fn is_hop_by_hop_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("keep-alive")
        || name.eq_ignore_ascii_case("proxy-connection")
}

fn connection_headers(runtime: &RuntimeConfig, keep_alive: bool) -> String {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    format!(
        "Connection: {connection}\r\nKeep-Alive: timeout={}, max={}\r\n",
        runtime.keep_alive_timeout_secs, runtime.keep_alive_max,
    )
}

fn proxied_target(target: &str, proxy: &ProxyRule) -> String {
    let (path, request_query) = target.split_once('?').unwrap_or((target, ""));
    let merged_query = merge_queries(proxy.upstream.base_query.as_deref(), request_query);

    if proxy.upstream.base_path == "/" {
        let mut next = path.to_string();
        if let Some(query) = merged_query {
            next.push('?');
            next.push_str(&query);
        }
        return next;
    }

    let suffix = path.strip_prefix(&proxy.prefix).unwrap_or(path);
    let mut next = join_url_paths(&proxy.upstream.base_path, suffix);
    if let Some(query) = merged_query {
        next.push('?');
        next.push_str(&query);
    }
    next
}

fn merge_queries(base_query: Option<&str>, request_query: &str) -> Option<String> {
    let has_base = base_query.is_some_and(|value| !value.is_empty());
    let has_request = !request_query.is_empty();

    match (has_base, has_request) {
        (false, false) => None,
        (true, false) => base_query.map(str::to_string),
        (false, true) => Some(request_query.to_string()),
        (true, true) => Some(format!(
            "{}&{}",
            base_query.expect("base query should exist when has_base is true"),
            request_query
        )),
    }
}

fn parse_base_path_and_query(path_and_query: &str) -> (String, Option<String>) {
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let base_path = normalize_base_path(path);
    let base_query = if query.is_empty() {
        None
    } else {
        Some(query.to_string())
    };
    (base_path, base_query)
}

fn header_value<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn has_chunked_transfer_encoding(request: &Request) -> bool {
    request
        .headers
        .iter()
        .filter(|(header, _)| header.eq_ignore_ascii_case("transfer-encoding"))
        .any(|(_, value)| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("chunked"))
        })
}

fn trim_proxy_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_base_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn join_url_paths(base: &str, suffix: &str) -> String {
    let base = base.trim_end_matches('/');
    let suffix = suffix.trim_start_matches('/');
    if suffix.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{suffix}")
    }
}

fn clean_request_path(target: &str) -> Option<PathBuf> {
    let path_part = target.split_once('?').map_or(target, |(path, _)| path);
    let decoded = percent_decode(path_part)?;
    let trimmed = decoded.trim_start_matches('/');
    let mut path = PathBuf::new();

    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    Some(path)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = from_hex(bytes[index + 1])?;
                let low = from_hex(bytes[index + 2])?;
                out.push(high << 4 | low);
                index += 3;
            }
            b'%' => return None,
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8(out).ok()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_inside(root: &Path, path: &Path) -> bool {
    fs::canonicalize(path)
        .map(|path| path.starts_with(root))
        .unwrap_or(false)
}

fn build_response(
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    keep_alive: bool,
    runtime: &RuntimeConfig,
) -> Vec<u8> {
    let mut bytes = build_head_response(
        status,
        reason,
        content_type,
        body.len(),
        keep_alive,
        runtime,
    );
    bytes.extend_from_slice(body);
    bytes
}

fn build_head_response(
    status: u16,
    reason: &str,
    content_type: &str,
    content_length: usize,
    keep_alive: bool,
    runtime: &RuntimeConfig,
) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Server: {SERVER_HEADER}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {content_length}\r\n\
         Cache-Control: no-cache\r\n\
         {}\
         \r\n",
        connection_headers(runtime, keep_alive),
    )
    .into_bytes()
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        "pdf" => "application/pdf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

fn print_usage() {
    eprintln!(
        "Usage: pear [OPTIONS] [DIR]\n\n\
         Options:\n\
           -p, --port <PORT>    Port to listen on, default 8080\n\
           -H, --host <HOST>    Host to bind, default 127.0.0.1\n\
           -d, --dir <DIR>      Directory to serve, default current directory\n\
           -c, --config <FILE>  Config file, default ./config.toml when present\n\
               --keep-alive-timeout <SECS>\n\
                                Idle keep-alive timeout, default 5\n\
               --keep-alive-max <N>\n\
                                Max requests per connection, default 100\n\
               --max-events <N> epoll batch size on Linux, default 256\n\
               --max-connections <N>\n\
                                Max open client connections, default auto-detected\n\
               --max-request-size <BYTES>\n\
                                Max request size, default 1048576\n\
               --no-spa         Disable fallback to index.html\n\
               --proxy <RULE>   Reverse proxy rule, e.g. /api=https://api.example.com\n\
           -h, --help           Show this help\n\n\
         Examples:\n\
           pear\n\
           pear -p 3000\n\
           pear --config config.toml\n\
           pear --proxy /api=http://127.0.0.1:3000 ./dist\n\
           pear --proxy /api=https://api.example.com ./dist\n\
           pear --host 0.0.0.0 --port 8080 ./dist"
    );
}

const FILE_CHUNK_SIZE: usize = 64 * 1024;
const INLINE_FILE_THRESHOLD: u64 = 16 * 1024;

struct Response {
    head: Vec<u8>,
    body: ResponseBody,
    close: bool,
}

enum ResponseBody {
    Empty,
    File(fs::File),
    TempFile(TempBodyFile),
}

impl Response {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            head: bytes,
            body: ResponseBody::Empty,
            close: false,
        }
    }

    fn close(bytes: Vec<u8>) -> Self {
        Self {
            head: bytes,
            body: ResponseBody::Empty,
            close: true,
        }
    }

    fn streamed(head: Vec<u8>, file: fs::File) -> Self {
        Self {
            head,
            body: ResponseBody::File(file),
            close: false,
        }
    }

    fn with_temp_body(head: Vec<u8>, file: TempBodyFile) -> Self {
        Self {
            head,
            body: ResponseBody::TempFile(file),
            close: false,
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn stream_response_body<W: Write>(writer: &mut W, body: &mut ResponseBody) -> io::Result<()> {
    match body {
        ResponseBody::Empty => Ok(()),
        ResponseBody::File(file) => {
            let mut buf = vec![0_u8; FILE_CHUNK_SIZE];
            loop {
                let read = file.read(&mut buf)?;
                if read == 0 {
                    break;
                }
                writer.write_all(&buf[..read])?;
            }
            Ok(())
        }
        ResponseBody::TempFile(file) => {
            let mut buf = vec![0_u8; FILE_CHUNK_SIZE];
            loop {
                let read = file.read(&mut buf)?;
                if read == 0 {
                    break;
                }
                writer.write_all(&buf[..read])?;
            }
            Ok(())
        }
    }
}

struct TempBodyFile {
    file: Option<fs::File>,
    path: PathBuf,
}

impl TempBodyFile {
    fn new() -> io::Result<Self> {
        let dir = env::temp_dir();
        for _ in 0..16 {
            let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("pear-body-{}-{}.tmp", process::id(), seq));
            match fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        path,
                    });
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }

        Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "cannot allocate temporary body file",
        ))
    }

    fn rewind(&mut self) -> io::Result<()> {
        use std::io::Seek;
        use std::io::SeekFrom;

        if let Some(file) = &mut self.file {
            file.seek(SeekFrom::Start(0))?;
        }
        Ok(())
    }
}

impl Read for TempBodyFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.file {
            Some(file) => file.read(buf),
            None => Ok(0),
        }
    }
}

impl Write for TempBodyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.file {
            Some(file) => file.write(buf),
            None => Err(io::Error::other("temporary file already taken")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for TempBodyFile {
    fn drop(&mut self) {
        let _ = self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(not(target_os = "linux"))]
mod compat {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    const HANDLER_STACK_SIZE: usize = 64 * 1024;

    static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn run_server(config: Config, root: PathBuf) -> io::Result<()> {
        let addr = format!("{}:{}", config.host, config.port);
        let listener = TcpListener::bind(&addr)?;

        println!("Serving {}", root.display());
        println!("Open http://{addr}");
        for proxy in &config.proxies {
            println!(
                "Proxy {} -> {}://{}:{}{}",
                proxy.prefix,
                proxy.upstream.scheme_name(),
                proxy.upstream.host,
                proxy.upstream.port,
                proxy.upstream.base_path
            );
        }
        println!("Runtime compat + keep-alive");
        println!(
            "keep_alive_timeout={}s keep_alive_max={} max_connections={} max_request_size={}",
            config.runtime.keep_alive_timeout_secs,
            config.runtime.keep_alive_max,
            config.runtime.max_connections,
            config.runtime.max_request_size
        );
        println!("Press Ctrl+C to stop");

        let root = Arc::new(root);
        let proxies = Arc::new(config.proxies);
        let runtime = Arc::new(config.runtime);

        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!("Connection failed: {err}");
                    continue;
                }
            };

            if ACTIVE_CONNECTIONS.load(Ordering::Relaxed) >= runtime.max_connections {
                send_busy(stream);
                continue;
            }

            let root = Arc::clone(&root);
            let proxies = Arc::clone(&proxies);
            let runtime = Arc::clone(&runtime);
            let spa_fallback = config.spa_fallback;

            let spawn = thread::Builder::new()
                .stack_size(HANDLER_STACK_SIZE)
                .spawn(move || {
                    ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
                    if let Err(err) =
                        handle_connection(stream, &root, spa_fallback, &proxies, &runtime)
                    {
                        eprintln!("Request failed: {err}");
                    }
                    ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
                });

            if let Err(err) = spawn {
                eprintln!("Cannot spawn connection handler: {err}");
            }
        }

        Ok(())
    }

    fn handle_connection(
        stream: TcpStream,
        root: &Path,
        spa_fallback: bool,
        proxies: &[ProxyRule],
        runtime: &RuntimeConfig,
    ) -> io::Result<()> {
        stream.set_read_timeout(Some(runtime.keep_alive_timeout()))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let _ = stream.set_nodelay(true);

        let reader_stream = stream.try_clone()?;
        let mut reader = BufReader::new(reader_stream);
        let mut write_stream = stream;

        for served in 0..runtime.keep_alive_max {
            let request = match read_request_blocking(&mut reader, runtime.max_request_size) {
                Ok(Some(request)) => request,
                Ok(None) => break,
                Err(err)
                    if matches!(
                        err.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::UnexpectedEof
                    ) =>
                {
                    break;
                }
                Err(err) => return Err(err),
            };

            let keep_alive = served + 1 < runtime.keep_alive_max && wants_keep_alive(&request);
            if let Some(proxy) = find_proxy(proxies, &request.target) {
                if has_chunked_transfer_encoding(&request) {
                    write_stream.write_all(&build_response(
                        501,
                        "Not Implemented",
                        "text/plain",
                        b"Chunked transfer encoding is not supported",
                        false,
                        runtime,
                    ))?;
                    write_stream.flush()?;
                    break;
                }

                if let Err(err) =
                    proxy_request_streaming(&request, proxy, runtime, keep_alive, &mut write_stream)
                {
                    eprintln!("Proxy request failed: {err}");
                    write_stream.write_all(&build_response(
                        502,
                        "Bad Gateway",
                        "text/plain",
                        b"Bad Gateway",
                        false,
                        runtime,
                    ))?;
                    write_stream.flush()?;
                    break;
                }

                write_stream.flush()?;
                if !keep_alive {
                    break;
                }
                continue;
            }

            let mut response =
                handle_request(&request, root, spa_fallback, proxies, runtime, keep_alive)?;
            write_stream.write_all(&response.head)?;
            stream_response_body(&mut write_stream, &mut response.body)?;
            write_stream.flush()?;

            if response.close || !keep_alive {
                break;
            }
        }

        Ok(())
    }

    fn read_request_blocking(
        reader: &mut BufReader<TcpStream>,
        max_request_size: usize,
    ) -> io::Result<Option<Request>> {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(None);
        }
        if buffer.len() > max_request_size {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "request exceeds maximum size",
            ));
        }

        let mut scratch = Vec::with_capacity(buffer.len());
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if scratch.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "connection closed mid-request",
                ));
            }

            scratch.extend_from_slice(available);
            if scratch.len() > max_request_size {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "request exceeds maximum size",
                ));
            }

            if let Some((request, consumed)) = Request::parse_from_buffer(&scratch)? {
                let consumed_now = consumed.min(available.len());
                reader.consume(consumed_now);
                if consumed_now < consumed {
                    let mut remaining = consumed - consumed_now;
                    while remaining > 0 {
                        let next = reader.fill_buf()?;
                        if next.is_empty() {
                            return Err(io::Error::new(
                                ErrorKind::UnexpectedEof,
                                "connection closed mid-request",
                            ));
                        }
                        let used = remaining.min(next.len());
                        reader.consume(used);
                        remaining -= used;
                    }
                }
                return Ok(Some(request));
            }

            let len = available.len();
            reader.consume(len);
        }
    }

    fn send_busy(mut stream: TcpStream) {
        let _ = stream.write_all(
            b"HTTP/1.1 503 Service Unavailable\r\n\
              Server: pear\r\n\
              Content-Length: 0\r\n\
              Connection: close\r\n\
              \r\n",
        );
        let _ = stream.shutdown(Shutdown::Both);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        collections::HashMap,
        os::{
            fd::{AsRawFd, RawFd},
            raw::c_int,
        },
    };

    const EPOLLIN: u32 = 0x001;
    const EPOLLOUT: u32 = 0x004;
    const EPOLLERR: u32 = 0x008;
    const EPOLLHUP: u32 = 0x010;
    const EPOLLRDHUP: u32 = 0x2000;
    const EPOLL_CTL_ADD: c_int = 1;
    const EPOLL_CTL_DEL: c_int = 2;
    const EPOLL_CTL_MOD: c_int = 3;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EpollEvent {
        events: u32,
        data: u64,
    }

    unsafe extern "C" {
        fn epoll_create1(flags: c_int) -> c_int;
        fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
        fn epoll_wait(
            epfd: c_int,
            events: *mut EpollEvent,
            maxevents: c_int,
            timeout: c_int,
        ) -> c_int;
        fn close(fd: c_int) -> c_int;
    }

    pub(super) fn run_server(config: Config, root: PathBuf) -> io::Result<()> {
        let addr = format!("{}:{}", config.host, config.port);
        let listener = TcpListener::bind(&addr)?;
        listener.set_nonblocking(true)?;

        println!("Serving {}", root.display());
        println!("Open http://{addr}");
        for proxy in &config.proxies {
            println!(
                "Proxy {} -> {}://{}:{}{}",
                proxy.prefix,
                proxy.upstream.scheme_name(),
                proxy.upstream.host,
                proxy.upstream.port,
                proxy.upstream.base_path
            );
        }
        println!("Runtime epoll + keep-alive");
        println!(
            "keep_alive_timeout={}s keep_alive_max={} max_events={} max_connections={} max_request_size={}",
            config.runtime.keep_alive_timeout_secs,
            config.runtime.keep_alive_max,
            config.runtime.max_events,
            config.runtime.max_connections,
            config.runtime.max_request_size
        );
        println!("Press Ctrl+C to stop");

        let mut server = Server::new(
            listener,
            root,
            config.spa_fallback,
            config.proxies,
            config.runtime,
        )?;
        server.run()
    }

    struct Connection {
        stream: TcpStream,
        read_buf: Vec<u8>,
        write_head: Vec<u8>,
        head_written: usize,
        write_body: PendingBody,
        last_active: Instant,
        requests_served: usize,
        close_after_write: bool,
        peer_closed: bool,
    }

    enum PendingBody {
        Empty,
        File {
            file: fs::File,
            buf: Vec<u8>,
            written: usize,
            filled: usize,
            eof: bool,
        },
        TempFile {
            file: TempBodyFile,
            buf: Vec<u8>,
            written: usize,
            filled: usize,
            eof: bool,
        },
    }

    impl Connection {
        fn new(stream: TcpStream) -> io::Result<Self> {
            stream.set_nonblocking(true)?;
            let _ = stream.set_nodelay(true);
            Ok(Self {
                stream,
                read_buf: Vec::with_capacity(8192),
                write_head: Vec::new(),
                head_written: 0,
                write_body: PendingBody::Empty,
                last_active: Instant::now(),
                requests_served: 0,
                close_after_write: false,
                peer_closed: false,
            })
        }

        fn fd(&self) -> RawFd {
            self.stream.as_raw_fd()
        }

        fn has_pending_write(&self) -> bool {
            self.head_written < self.write_head.len() || self.write_body.has_pending_data()
        }
    }

    impl PendingBody {
        fn empty() -> Self {
            Self::Empty
        }

        fn from_response_body(body: ResponseBody) -> Self {
            match body {
                ResponseBody::Empty => Self::Empty,
                ResponseBody::File(file) => Self::File {
                    file,
                    buf: vec![0_u8; FILE_CHUNK_SIZE],
                    written: 0,
                    filled: 0,
                    eof: false,
                },
                ResponseBody::TempFile(file) => Self::TempFile {
                    file,
                    buf: vec![0_u8; FILE_CHUNK_SIZE],
                    written: 0,
                    filled: 0,
                    eof: false,
                },
            }
        }

        fn has_pending_data(&self) -> bool {
            match self {
                Self::Empty => false,
                Self::File {
                    written,
                    filled,
                    eof,
                    ..
                }
                | Self::TempFile {
                    written,
                    filled,
                    eof,
                    ..
                } => !(*eof && *written == *filled),
            }
        }

        fn flush_to(&mut self, stream: &mut TcpStream) -> io::Result<bool> {
            match self {
                Self::Empty => Ok(true),
                Self::File {
                    file,
                    buf,
                    written,
                    filled,
                    eof,
                } => loop {
                    if *written < *filled {
                        match stream.write(&buf[*written..*filled]) {
                            Ok(0) => return Ok(false),
                            Ok(n) => *written += n,
                            Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(false),
                            Err(err) => return Err(err),
                        }
                    }

                    if *written == *filled {
                        if *eof {
                            return Ok(true);
                        }

                        let read = file.read(buf)?;
                        *written = 0;
                        *filled = read;
                        if read == 0 {
                            *eof = true;
                            return Ok(true);
                        }
                    }
                },
                Self::TempFile {
                    file,
                    buf,
                    written,
                    filled,
                    eof,
                } => loop {
                    if *written < *filled {
                        match stream.write(&buf[*written..*filled]) {
                            Ok(0) => return Ok(false),
                            Ok(n) => *written += n,
                            Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(false),
                            Err(err) => return Err(err),
                        }
                    }

                    if *written == *filled {
                        if *eof {
                            return Ok(true);
                        }

                        let read = file.read(buf)?;
                        *written = 0;
                        *filled = read;
                        if read == 0 {
                            *eof = true;
                            return Ok(true);
                        }
                    }
                },
            }
        }
    }

    struct Epoll {
        fd: RawFd,
    }

    impl Epoll {
        fn new() -> io::Result<Self> {
            let fd = unsafe { epoll_create1(0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd })
        }

        fn add(&self, fd: RawFd, events: u32) -> io::Result<()> {
            self.ctl(EPOLL_CTL_ADD, fd, events)
        }

        fn modify(&self, fd: RawFd, events: u32) -> io::Result<()> {
            self.ctl(EPOLL_CTL_MOD, fd, events)
        }

        fn delete(&self, fd: RawFd) -> io::Result<()> {
            let result = unsafe { epoll_ctl(self.fd, EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn wait(&self, events: &mut [EpollEvent], timeout_ms: i32) -> io::Result<usize> {
            let ready = unsafe {
                epoll_wait(
                    self.fd,
                    events.as_mut_ptr(),
                    events.len() as c_int,
                    timeout_ms,
                )
            };
            if ready < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == ErrorKind::Interrupted {
                    return Ok(0);
                }
                return Err(err);
            }
            Ok(ready as usize)
        }

        fn ctl(&self, op: c_int, fd: RawFd, events: u32) -> io::Result<()> {
            let mut event = EpollEvent {
                events,
                data: fd as u64,
            };
            let result = unsafe { epoll_ctl(self.fd, op, fd, &mut event) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for Epoll {
        fn drop(&mut self) {
            unsafe {
                let _ = close(self.fd);
            }
        }
    }

    struct Server {
        listener: TcpListener,
        listener_fd: RawFd,
        epoll: Epoll,
        root: PathBuf,
        spa_fallback: bool,
        proxies: Vec<ProxyRule>,
        runtime: RuntimeConfig,
        connections: HashMap<RawFd, Connection>,
    }

    impl Server {
        fn new(
            listener: TcpListener,
            root: PathBuf,
            spa_fallback: bool,
            proxies: Vec<ProxyRule>,
            runtime: RuntimeConfig,
        ) -> io::Result<Self> {
            let epoll = Epoll::new()?;
            let listener_fd = listener.as_raw_fd();
            epoll.add(listener_fd, EPOLLIN)?;

            Ok(Self {
                listener,
                listener_fd,
                epoll,
                root,
                spa_fallback,
                proxies,
                runtime,
                connections: HashMap::new(),
            })
        }

        fn run(&mut self) -> io::Result<()> {
            let mut events = vec![EpollEvent { events: 0, data: 0 }; self.runtime.max_events];

            loop {
                let ready = self.epoll.wait(&mut events, 1000)?;
                for event in &events[..ready] {
                    let fd = event.data as RawFd;
                    if fd == self.listener_fd {
                        self.accept_ready()?;
                        continue;
                    }

                    if event.events & (EPOLLERR | EPOLLHUP) != 0 {
                        self.close_connection(fd);
                        continue;
                    }

                    if event.events & EPOLLIN != 0 {
                        self.read_ready(fd)?;
                    }

                    if event.events & EPOLLOUT != 0 {
                        self.write_ready(fd)?;
                    }

                    if event.events & EPOLLRDHUP != 0 {
                        self.mark_peer_closed(fd);
                    }
                }

                self.sweep_idle();
            }
        }

        fn accept_ready(&mut self) -> io::Result<()> {
            loop {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        if self.connections.len() >= self.runtime.max_connections {
                            send_busy(stream);
                            continue;
                        }

                        let conn = Connection::new(stream)?;
                        let fd = conn.fd();
                        self.epoll.add(fd, EPOLLIN | EPOLLRDHUP)?;
                        self.connections.insert(fd, conn);
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                    Err(err) if is_fd_exhausted(&err) => {
                        eprintln!(
                            "Accept paused: file descriptor limit reached ({err}). Reduce max_connections or raise ulimit -n."
                        );
                        break;
                    }
                    Err(err) => return Err(err),
                }
            }
            Ok(())
        }

        fn read_ready(&mut self, fd: RawFd) -> io::Result<()> {
            let mut parsed_request = None;
            let mut close_now = false;

            {
                let Some(conn) = self.connections.get_mut(&fd) else {
                    return Ok(());
                };

                let mut buf = [0_u8; 8192];
                loop {
                    match conn.stream.read(&mut buf) {
                        Ok(0) => {
                            conn.peer_closed = true;
                            break;
                        }
                        Ok(read) => {
                            conn.last_active = Instant::now();
                            conn.read_buf.extend_from_slice(&buf[..read]);
                            if conn.read_buf.len() > self.runtime.max_request_size {
                                conn.write_head = build_response(
                                    413,
                                    "Payload Too Large",
                                    "text/plain",
                                    b"Payload Too Large",
                                    false,
                                    &self.runtime,
                                );
                                conn.head_written = 0;
                                conn.write_body = PendingBody::empty();
                                conn.close_after_write = true;
                                break;
                            }

                            if conn.has_pending_write() {
                                continue;
                            }

                            match Request::parse_from_buffer(&conn.read_buf) {
                                Ok(Some((request, consumed))) => {
                                    conn.read_buf.drain(..consumed);
                                    parsed_request = Some(request);
                                    break;
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    eprintln!("Request parse failed: {err}");
                                    conn.write_head = build_response(
                                        400,
                                        "Bad Request",
                                        "text/plain",
                                        b"Bad Request",
                                        false,
                                        &self.runtime,
                                    );
                                    conn.head_written = 0;
                                    conn.write_body = PendingBody::empty();
                                    conn.close_after_write = true;
                                    break;
                                }
                            }
                        }
                        Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                        Err(err) => {
                            close_now = true;
                            eprintln!("Read failed: {err}");
                            break;
                        }
                    }
                }
            }

            if close_now {
                self.close_connection(fd);
                return Ok(());
            }

            if let Some(request) = parsed_request {
                self.dispatch_request(fd, request)?;
                self.write_ready(fd)?;
                return Ok(());
            }

            self.refresh_interest(fd)?;
            Ok(())
        }

        fn dispatch_request(&mut self, fd: RawFd, request: Request) -> io::Result<()> {
            let keep_alive = {
                let Some(conn) = self.connections.get(&fd) else {
                    return Ok(());
                };
                wants_keep_alive(&request) && conn.requests_served + 1 < self.runtime.keep_alive_max
            };

            let response = handle_request(
                &request,
                &self.root,
                self.spa_fallback,
                &self.proxies,
                &self.runtime,
                keep_alive,
            )?;

            let Some(conn) = self.connections.get_mut(&fd) else {
                return Ok(());
            };
            conn.requests_served += 1;
            conn.last_active = Instant::now();
            conn.write_head = response.head;
            conn.head_written = 0;
            conn.write_body = PendingBody::from_response_body(response.body);
            conn.close_after_write = response.close || !keep_alive;
            Ok(())
        }

        fn write_ready(&mut self, fd: RawFd) -> io::Result<()> {
            let mut parse_next = false;
            let mut close_now = false;

            {
                let Some(conn) = self.connections.get_mut(&fd) else {
                    return Ok(());
                };

                while conn.head_written < conn.write_head.len() {
                    match conn.stream.write(&conn.write_head[conn.head_written..]) {
                        Ok(0) => {
                            close_now = true;
                            break;
                        }
                        Ok(written) => {
                            conn.head_written += written;
                            conn.last_active = Instant::now();
                        }
                        Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                        Err(err) => {
                            close_now = true;
                            eprintln!("Write failed: {err}");
                            break;
                        }
                    }
                }

                if !close_now && conn.head_written == conn.write_head.len() {
                    match conn.write_body.flush_to(&mut conn.stream) {
                        Ok(true) => {
                            conn.write_head.clear();
                            conn.head_written = 0;
                            conn.write_body = PendingBody::empty();
                            conn.last_active = Instant::now();
                            if conn.close_after_write || conn.peer_closed {
                                close_now = true;
                            } else if !conn.read_buf.is_empty() {
                                parse_next = true;
                            }
                        }
                        Ok(false) => {}
                        Err(err)
                            if matches!(
                                err.kind(),
                                ErrorKind::BrokenPipe
                                    | ErrorKind::ConnectionReset
                                    | ErrorKind::ConnectionAborted
                            ) =>
                        {
                            close_now = true;
                        }
                        Err(err) => return Err(err),
                    }
                }
            }

            if close_now {
                self.close_connection(fd);
                return Ok(());
            }

            if parse_next {
                let request = {
                    let Some(conn) = self.connections.get_mut(&fd) else {
                        return Ok(());
                    };

                    match Request::parse_from_buffer(&conn.read_buf) {
                        Ok(Some((request, consumed))) => {
                            conn.read_buf.drain(..consumed);
                            Some(request)
                        }
                        Ok(None) => None,
                        Err(err) => {
                            eprintln!("Request parse failed: {err}");
                            conn.write_head = build_response(
                                400,
                                "Bad Request",
                                "text/plain",
                                b"Bad Request",
                                false,
                                &self.runtime,
                            );
                            conn.head_written = 0;
                            conn.write_body = PendingBody::empty();
                            conn.close_after_write = true;
                            None
                        }
                    }
                };

                if let Some(request) = request {
                    self.dispatch_request(fd, request)?;
                }
            }

            self.refresh_interest(fd)?;
            Ok(())
        }

        fn refresh_interest(&mut self, fd: RawFd) -> io::Result<()> {
            let Some(conn) = self.connections.get(&fd) else {
                return Ok(());
            };

            let mut events = EPOLLIN | EPOLLRDHUP;
            if conn.has_pending_write() {
                events |= EPOLLOUT;
            }
            self.epoll.modify(fd, events)
        }

        fn mark_peer_closed(&mut self, fd: RawFd) {
            if let Some(conn) = self.connections.get_mut(&fd) {
                conn.peer_closed = true;
            }
        }

        fn sweep_idle(&mut self) {
            let now = Instant::now();
            let stale = self
                .connections
                .iter()
                .filter_map(|(&fd, conn)| {
                    if !conn.has_pending_write()
                        && now.duration_since(conn.last_active) >= self.runtime.keep_alive_timeout()
                    {
                        Some(fd)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            for fd in stale {
                self.close_connection(fd);
            }
        }

        fn close_connection(&mut self, fd: RawFd) {
            let _ = self.epoll.delete(fd);
            self.connections.remove(&fd);
        }
    }

    fn send_busy(mut stream: TcpStream) {
        let _ = stream.write_all(
            b"HTTP/1.1 503 Service Unavailable\r\n\
              Server: pear\r\n\
              Content-Length: 0\r\n\
              Connection: close\r\n\
              \r\n",
        );
        let _ = stream.shutdown(Shutdown::Both);
    }

    fn is_fd_exhausted(err: &io::Error) -> bool {
        matches!(err.raw_os_error(), Some(23 | 24))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::generate_simple_self_signed;
    use rustls::{
        ServerConfig, ServerConnection,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    };
    use std::{net::TcpListener, sync::mpsc, thread};

    fn runtime() -> RuntimeConfig {
        RuntimeConfig {
            keep_alive_timeout_secs: 5,
            keep_alive_max: 100,
            max_events: 256,
            max_connections: 1024,
            max_request_size: 1024 * 1024,
        }
    }

    fn request(version: &str, connection: Option<&str>) -> Request {
        let mut headers = Vec::new();
        if let Some(value) = connection {
            headers.push(("Connection".to_string(), value.to_string()));
        }
        Request {
            method: "GET".to_string(),
            target: "/".to_string(),
            version: version.to_string(),
            headers,
            body: Vec::new(),
        }
    }

    fn read_response_body(mut response: Response) -> Vec<u8> {
        let mut body = Vec::new();
        match &mut response.body {
            ResponseBody::Empty => {}
            ResponseBody::File(file) => {
                file.read_to_end(&mut body).expect("file body should read");
            }
            ResponseBody::TempFile(file) => {
                file.read_to_end(&mut body).expect("temp body should read");
            }
        }
        body
    }

    fn spawn_https_upstream() -> (u16, RootCertStore, thread::JoinHandle<()>) {
        let certified =
            generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
                .expect("certificate should generate");
        let cert_der: CertificateDer<'static> = certified.cert.der().clone();
        let key_der: PrivateKeyDer<'static> =
            PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()).into();
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server config should build");

        let mut root_store = RootCertStore::empty();
        root_store
            .add(cert_der.clone())
            .expect("test cert should be trusted");

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let port = listener
            .local_addr()
            .expect("listener addr should exist")
            .port();
        let (ready_tx, ready_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            ready_tx.send(()).expect("ready signal should send");
            let (stream, _) = listener.accept().expect("upstream should accept");
            let connection =
                ServerConnection::new(std::sync::Arc::new(server_config)).expect("tls server");
            let mut tls = StreamOwned::new(connection, stream);

            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let read = tls.read(&mut buf).expect("request should read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                if find_header_end(&request).is_some() {
                    break;
                }
            }

            let request_text = String::from_utf8(request).expect("request should be utf8");
            assert!(request_text.starts_with("GET /secure HTTP/1.1\r\n"));
            assert!(request_text.contains(&format!("Host: localhost:{port}\r\n")));
            assert!(request_text.contains("X-Forwarded-Proto: https\r\n"));

            tls.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            )
            .expect("response should write");
            tls.flush().expect("response should flush");
            tls.conn.send_close_notify();
            tls.flush().expect("close notify should flush");
        });
        ready_rx.recv().expect("ready signal should arrive");

        (port, root_store, handle)
    }

    #[test]
    fn parses_request_with_body_from_buffer() {
        let raw =
            b"POST /api/items HTTP/1.1\r\nHost: example.test\r\nContent-Length: 5\r\n\r\nhello";
        let parsed = Request::parse_from_buffer(raw)
            .expect("request parse should succeed")
            .expect("request should be complete");

        assert_eq!(parsed.1, raw.len());
        assert_eq!(parsed.0.method, "POST");
        assert_eq!(parsed.0.target, "/api/items");
        assert_eq!(parsed.0.version, "HTTP/1.1");
        assert_eq!(parsed.0.body, b"hello");
    }

    #[test]
    fn returns_none_for_incomplete_request_buffer() {
        let raw = b"POST /api/items HTTP/1.1\r\nContent-Length: 5\r\n\r\nhel";
        let parsed = Request::parse_from_buffer(raw).expect("partial parse should not fail");
        assert!(parsed.is_none());
    }

    #[test]
    fn http11_defaults_to_keep_alive() {
        assert!(wants_keep_alive(&request("HTTP/1.1", None)));
        assert!(!wants_keep_alive(&request("HTTP/1.1", Some("close"))));
    }

    #[test]
    fn http10_requires_explicit_keep_alive() {
        assert!(!wants_keep_alive(&request("HTTP/1.0", None)));
        assert!(wants_keep_alive(&request("HTTP/1.0", Some("keep-alive"))));
    }

    #[test]
    fn rewrites_proxy_response_connection_headers() {
        let upstream =
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\nKeep-Alive: timeout=1\r\nX-Test: ok";
        let rewritten = rewrite_proxy_response_head(upstream, &runtime(), true)
            .expect("rewrite should succeed");
        let text = String::from_utf8(rewritten).expect("response should be utf8");

        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 4\r\n"));
        assert!(text.contains("X-Test: ok\r\n"));
        assert!(text.contains("Connection: keep-alive\r\n"));
        assert!(text.contains("Keep-Alive: timeout=5, max=100\r\n"));
        assert!(!text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn cleans_and_rejects_unsafe_paths() {
        assert_eq!(
            clean_request_path("/assets/app.js?x=1"),
            Some(PathBuf::from("assets/app.js"))
        );
        assert_eq!(
            clean_request_path("/nested/%66ile.txt"),
            Some(PathBuf::from("nested/file.txt"))
        );
        assert_eq!(clean_request_path("/../secret.txt"), None);
        assert_eq!(clean_request_path("/bad/%zz"), None);
    }

    #[test]
    fn proxied_target_replaces_prefix_with_upstream_base_path() {
        let proxy = ProxyRule::new("/api", "http://127.0.0.1:3000/backend")
            .expect("proxy rule should parse");
        assert_eq!(
            proxied_target("/api/users?id=1", &proxy),
            "/backend/users?id=1"
        );
    }

    #[test]
    fn proxied_target_merges_fixed_upstream_query_with_request_query() {
        let proxy = ProxyRule::new("/api", "http://127.0.0.1:3000/backend?fixed=1")
            .expect("proxy rule should parse");
        assert_eq!(
            proxied_target("/api/users?id=1&name=test", &proxy),
            "/backend/users?fixed=1&id=1&name=test"
        );
    }

    #[test]
    fn proxied_target_preserves_prefix_for_root_upstream_and_appends_fixed_query() {
        let proxy = ProxyRule::new("/api", "http://127.0.0.1:3000?fixed=1")
            .expect("proxy rule should parse");
        assert_eq!(proxied_target("/api/users", &proxy), "/api/users?fixed=1");
        assert_eq!(
            proxied_target("/api/users?id=1", &proxy),
            "/api/users?fixed=1&id=1"
        );
    }

    #[test]
    fn missing_spa_fallback_index_returns_404_instead_of_io_error() {
        let root = std::env::temp_dir().join(format!(
            "pear-test-missing-index-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("temp root should be created");
        let canonical_root = fs::canonicalize(&root).expect("root should canonicalize");
        let request = Request {
            method: "GET".to_string(),
            target: "/missing".to_string(),
            version: "HTTP/1.1".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        };

        let response =
            serve_static(&request, &canonical_root, true, &runtime(), true).expect("response");
        let text = String::from_utf8(response.head).expect("response should be utf8");
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));

        let _ = fs::remove_dir(root);
    }

    #[test]
    fn tls_close_notify_eof_is_treated_as_clean_after_complete_body() {
        let err = io::Error::new(
            ErrorKind::UnexpectedEof,
            "peer closed connection without sending TLS close_notify",
        );
        assert!(should_treat_upstream_eof_as_clean_close(&err, 5, Some(5)));
        assert!(!should_treat_upstream_eof_as_clean_close(&err, 4, Some(5)));
        assert!(should_treat_upstream_eof_as_clean_close(&err, 1, None));
    }

    #[test]
    fn parses_https_upstream_and_omits_default_port_in_host_header() {
        let proxy =
            ProxyRule::new("/secure", "https://localhost/base").expect("https proxy should parse");

        assert_eq!(proxy.upstream.scheme, UpstreamScheme::Https);
        assert_eq!(proxy.upstream.host, "localhost");
        assert_eq!(proxy.upstream.port, 443);
        assert_eq!(proxy.upstream.base_path, "/base");
        assert_eq!(proxy.upstream.authority_header(), "localhost");
    }

    #[test]
    fn proxy_request_handles_split_header_and_body_reads() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let port = listener
            .local_addr()
            .expect("listener addr should exist")
            .port();
        let (ready_tx, ready_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            ready_tx.send(()).expect("ready signal should send");
            let (mut stream, _) = listener.accept().expect("upstream should accept");
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buf).expect("request should read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                if find_header_end(&request).is_some() {
                    break;
                }
            }

            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
                .expect("response head should write");
            stream.flush().expect("response head should flush");
            thread::sleep(std::time::Duration::from_millis(20));
            stream
                .write_all(b"hello")
                .expect("response body should write");
            stream.flush().expect("response body should flush");
        });
        ready_rx.recv().expect("ready signal should arrive");

        let proxy = ProxyRule::new("/api", &format!("http://127.0.0.1:{port}/secure"))
            .expect("http proxy should parse");
        let request = Request {
            method: "GET".to_string(),
            target: "/api".to_string(),
            version: "HTTP/1.1".to_string(),
            headers: vec![("Host".to_string(), "pear.local".to_string())],
            body: Vec::new(),
        };

        let response =
            proxy_request_with_root_store(&request, &proxy, &runtime(), true, RootCertStore::empty())
                .expect("http proxy request should succeed");
        let text = String::from_utf8(response.head.clone()).expect("head should be utf8");
        let body = read_response_body(response);

        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 5\r\n"));
        assert_eq!(body, b"hello");
        handle.join().expect("http upstream thread should finish");
    }

    #[test]
    fn proxy_request_supports_https_upstream() {
        let (port, root_store, handle) = spawn_https_upstream();
        let proxy = ProxyRule::new("/api", &format!("https://localhost:{port}/secure"))
            .expect("https proxy should parse");
        let request = Request {
            method: "GET".to_string(),
            target: "/api".to_string(),
            version: "HTTP/1.1".to_string(),
            headers: vec![
                ("Host".to_string(), "pear.local".to_string()),
                ("X-Forwarded-Proto".to_string(), "https".to_string()),
            ],
            body: Vec::new(),
        };

        let response =
            proxy_request_with_root_store(&request, &proxy, &runtime(), true, root_store)
                .expect("https proxy request should succeed");
        let text = String::from_utf8(response.head.clone()).expect("head should be utf8");
        let body = read_response_body(response);

        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Connection: keep-alive\r\n"));
        assert_eq!(body, b"hello");
        handle.join().expect("https upstream thread should finish");
    }

    #[test]
    fn runtime_validation_rejects_zero_values() {
        let invalid = RuntimeConfig {
            keep_alive_timeout_secs: 0,
            keep_alive_max: 1,
            max_events: 1,
            max_connections: 1,
            max_request_size: 1,
        };
        assert!(invalid.validate().is_err());

        let invalid = RuntimeConfig {
            keep_alive_timeout_secs: 1,
            keep_alive_max: 0,
            max_events: 1,
            max_connections: 1,
            max_request_size: 1,
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn head_response_uses_runtime_keep_alive_values() {
        let bytes = build_head_response(200, "OK", "text/plain", 3, true, &runtime());
        let text = String::from_utf8(bytes).expect("header should be utf8");

        assert!(text.contains("Connection: keep-alive\r\n"));
        assert!(text.contains("Keep-Alive: timeout=5, max=100\r\n"));
        assert!(text.contains("Content-Length: 3\r\n"));
    }

    #[test]
    fn static_get_inlines_small_file_body() {
        let root = std::env::temp_dir().join(format!(
            "pear-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("temp root should be created");
        let file_path = root.join("index.html");
        fs::write(&file_path, b"hello world").expect("temp file should be written");
        let canonical_root = fs::canonicalize(&root).expect("root should canonicalize");

        let request = Request {
            method: "GET".to_string(),
            target: "/".to_string(),
            version: "HTTP/1.1".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        };

        let response = serve_static(&request, &canonical_root, true, &runtime(), true)
            .expect("static response");

        assert!(response.head.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response.head.ends_with(b"\r\nhello world"));
        match response.body {
            ResponseBody::Empty => {}
            ResponseBody::File(_) => panic!("expected inline small file body"),
            ResponseBody::TempFile(_) => panic!("expected static file body, not temp body"),
        }

        let _ = fs::remove_file(file_path);
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn static_get_streams_large_file_body() {
        let root = std::env::temp_dir().join(format!(
            "pear-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("temp root should be created");
        let file_path = root.join("index.html");
        fs::write(&file_path, vec![b'a'; INLINE_FILE_THRESHOLD as usize + 1])
            .expect("temp file should be written");
        let canonical_root = fs::canonicalize(&root).expect("root should canonicalize");

        let request = Request {
            method: "GET".to_string(),
            target: "/".to_string(),
            version: "HTTP/1.1".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        };

        let response = serve_static(&request, &canonical_root, true, &runtime(), true)
            .expect("static response");

        assert!(response.head.starts_with(b"HTTP/1.1 200 OK\r\n"));
        match response.body {
            ResponseBody::File(_) => {}
            ResponseBody::TempFile(_) => panic!("expected static file body, not temp body"),
            ResponseBody::Empty => panic!("expected streamed large file body"),
        }

        let _ = fs::remove_file(file_path);
        let _ = fs::remove_dir(root);
    }
}
