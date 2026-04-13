use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    process, thread,
};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;
const DEFAULT_CONFIG: &str = "config.toml";
const SERVER_HEADER: &str = "pear";

#[derive(Debug)]
struct Config {
    host: String,
    port: u16,
    root: PathBuf,
    spa_fallback: bool,
    proxies: Vec<ProxyRule>,
}

#[derive(Clone, Debug)]
struct ProxyRule {
    prefix: String,
    upstream: Upstream,
}

#[derive(Clone, Debug)]
struct Upstream {
    host: String,
    port: u16,
    base_path: String,
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

    let addr = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&addr).unwrap_or_else(|err| {
        eprintln!("Cannot bind {addr}: {err}");
        process::exit(1);
    });

    println!("Serving {}", root.display());
    println!("Open http://{addr}");
    for proxy in &config.proxies {
        println!(
            "Proxy {} -> http://{}:{}{}",
            proxy.prefix, proxy.upstream.host, proxy.upstream.port, proxy.upstream.base_path
        );
    }
    println!("Press Ctrl+C to stop");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let root = root.clone();
                let spa_fallback = config.spa_fallback;
                let proxies = config.proxies.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream, &root, spa_fallback, &proxies) {
                        eprintln!("Request failed: {err}");
                    }
                });
            }
            Err(err) => eprintln!("Connection failed: {err}"),
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

        Ok(config)
    }

    fn default_values() -> Result<Self, String> {
        Ok(Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            root: env::current_dir().map_err(|err| format!("Cannot read current dir: {err}"))?,
            spa_fallback: true,
            proxies: Vec::new(),
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
            format!("Invalid proxy rule '{value}', expected /path=http://host:port")
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
        let rest = value
            .strip_prefix("http://")
            .ok_or_else(|| format!("Only http:// proxy targets are supported: {value}"))?;
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() {
            return Err(format!("Missing proxy upstream host: {value}"));
        }

        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => {
                let port = port
                    .parse()
                    .map_err(|_| format!("Invalid proxy upstream port: {port}"))?;
                (host.to_string(), port)
            }
            _ => (authority.to_string(), 80),
        };

        Ok(Self {
            host,
            port,
            base_path: normalize_base_path(path),
        })
    }
}

impl Request {
    fn read_from(stream: &TcpStream) -> io::Result<Option<Self>> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        if request_line.trim().is_empty() {
            return Ok(None);
        }

        let mut parts = request_line.split_whitespace();
        let Some(method) = parts.next() else {
            return Ok(None);
        };
        let Some(target) = parts.next() else {
            return Ok(None);
        };
        let version = parts.next().unwrap_or("HTTP/1.1");

        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
        }

        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }

        Ok(Some(Self {
            method: method.to_string(),
            target: target.to_string(),
            version: version.to_string(),
            headers,
            body,
        }))
    }
}

fn handle_connection(
    mut stream: TcpStream,
    root: &Path,
    spa_fallback: bool,
    proxies: &[ProxyRule],
) -> io::Result<()> {
    let Some(request) = Request::read_from(&stream)? else {
        return write_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain",
            b"Bad Request",
        );
    };

    if let Some(proxy) = find_proxy(proxies, &request.target) {
        if has_chunked_transfer_encoding(&request) {
            return write_response(
                &mut stream,
                501,
                "Not Implemented",
                "text/plain",
                b"Chunked transfer encoding is not supported",
            );
        }

        if let Err(err) = proxy_request(&request, proxy, &mut stream) {
            eprintln!("Proxy request failed: {err}");
            return write_response(
                &mut stream,
                502,
                "Bad Gateway",
                "text/plain",
                b"Bad Gateway",
            );
        }

        return Ok(());
    }

    serve_static(request, stream, root, spa_fallback)
}

fn serve_static(
    request: Request,
    mut stream: TcpStream,
    root: &Path,
    spa_fallback: bool,
) -> io::Result<()> {
    if request.method != "GET" && request.method != "HEAD" {
        return write_response(
            &mut stream,
            405,
            "Method Not Allowed",
            "text/plain",
            b"Method Not Allowed",
        );
    }

    let Some(request_path) = clean_request_path(&request.target) else {
        return write_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain",
            b"Bad Request",
        );
    };

    let mut path = root.join(&request_path);
    if path.is_dir() {
        path = path.join("index.html");
    }

    let file_path = if path.is_file() {
        path
    } else if spa_fallback {
        root.join("index.html")
    } else {
        return write_response(&mut stream, 404, "Not Found", "text/plain", b"Not Found");
    };

    if !file_path.is_file() || !is_inside(root, &file_path) {
        return write_response(&mut stream, 404, "Not Found", "text/plain", b"Not Found");
    }

    let bytes = fs::read(&file_path)?;
    let content_type = content_type(&file_path);

    if request.method == "HEAD" {
        write_head_response(&mut stream, 200, "OK", content_type, bytes.len())
    } else {
        write_response(&mut stream, 200, "OK", content_type, &bytes)
    }
}

fn find_proxy<'a>(proxies: &'a [ProxyRule], target: &str) -> Option<&'a ProxyRule> {
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    proxies
        .iter()
        .filter(|proxy| proxy.matches(path))
        .max_by_key(|proxy| proxy.prefix.len())
}

fn proxy_request(request: &Request, proxy: &ProxyRule, client: &mut TcpStream) -> io::Result<()> {
    let mut upstream = TcpStream::connect((proxy.upstream.host.as_str(), proxy.upstream.port))?;
    let target = proxied_target(&request.target, proxy);

    write!(
        upstream,
        "{} {} {}\r\n",
        request.method, target, request.version
    )?;
    write!(
        upstream,
        "Host: {}:{}\r\n",
        proxy.upstream.host, proxy.upstream.port
    )?;
    write!(
        upstream,
        "X-Forwarded-Host: {}\r\n",
        header_value(request, "host").unwrap_or("")
    )?;
    write!(upstream, "X-Forwarded-Proto: http\r\n")?;

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
    upstream.shutdown(Shutdown::Write)?;
    io::copy(&mut upstream, client)?;
    Ok(())
}

fn proxied_target(target: &str, proxy: &ProxyRule) -> String {
    if proxy.upstream.base_path == "/" {
        return target.to_string();
    }

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let suffix = path.strip_prefix(&proxy.prefix).unwrap_or(path);
    let mut next = join_url_paths(&proxy.upstream.base_path, suffix);
    if !query.is_empty() {
        next.push('?');
        next.push_str(query);
    }
    next
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

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    write_head_response(stream, status, reason, content_type, body.len())?;
    stream.write_all(body)
}

fn write_head_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    content_length: usize,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Server: {SERVER_HEADER}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {content_length}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\
         \r\n"
    )
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
               --no-spa         Disable fallback to index.html\n\
               --proxy <RULE>    Reverse proxy rule, e.g. /api=http://127.0.0.1:3000\n\
           -h, --help           Show this help\n\n\
         Examples:\n\
           pear\n\
           pear -p 3000\n\
           pear --config config.toml\n\
           pear --proxy /api=http://127.0.0.1:3000 ./dist\n\
           pear --host 0.0.0.0 --port 8080 ./dist"
    );
}
