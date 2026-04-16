# pear

A small Rust static file server for frontend build output, with SPA fallback, reverse proxy support, keep-alive, and a Linux `epoll` runtime.

[中文文档](README.zh-CN.md)

## Table of Contents

1. [About The Project](#about-the-project)
2. [Built With](#built-with)
3. [Getting Started](#getting-started)
4. [Usage](#usage)
5. [Configuration](#configuration)
6. [Runtime Tuning](#runtime-tuning)
7. [Implementation Notes](#implementation-notes)
8. [Current Limitations](#current-limitations)
9. [Testing](#testing)
10. [Roadmap](#roadmap)
11. [Contributing](#contributing)
12. [License](#license)
13. [Acknowledgments](#acknowledgments)

## About The Project

`pear` is built for frontend-oriented workflows where one small binary is more useful than a full web framework stack.

It focuses on a narrow but practical use case:

- serve a built frontend directory such as `dist` or `build`
- fall back to `index.html` for SPA routes
- proxy backend APIs under the same origin
- run on Linux, macOS, and Windows with the same CLI and config shape

The runtime strategy is platform-aware:

- Linux uses `epoll + keep-alive`
- macOS and Windows use a compatibility runtime with keep-alive

This keeps the external behavior consistent while allowing Linux to use an event-driven backend.

Current streaming status:

- static file responses stream large file bodies
- compatibility-runtime proxy responses stream upstream bodies directly to the client
- Linux `epoll` proxy streaming is under active refactor and still needs Linux-target validation

<p align="right">(<a href="#pear">back to top</a>)</p>

## Built With

- Rust
- Rust standard library networking and IO primitives
- Linux `epoll` via FFI on Linux builds

<p align="right">(<a href="#pear">back to top</a>)</p>

## Getting Started

### Prerequisites

- Rust toolchain
- A frontend build directory such as `dist` or `build`

### Build

```sh
cargo build --release
```

### Run

Serve the current directory:

```sh
pear
```

Serve a frontend build directory:

```sh
pear --port 8080 ./dist
```

Run from inside `dist`:

```sh
cd dist
/path/to/pear/target/release/pear -p 8080
```

Open:

```text
http://127.0.0.1:8080
```

<p align="right">(<a href="#pear">back to top</a>)</p>

## Usage

### Serve A SPA Build

```sh
pear ./dist
```

By default, unknown routes fall back to `index.html`, which is useful for React, Vue, Vite, and similar SPA builds.

### Serve Static Assets And Proxy A Backend

```sh
pear --port 8080 --proxy /api=http://127.0.0.1:3000 ./dist
```

A browser request to:

```text
http://127.0.0.1:8080/api/users
```

is forwarded to:

```text
http://127.0.0.1:3000/api/users
```

You can also proxy HTTPS upstreams:

```sh
pear --port 8080 --proxy /api=https://api.example.com ./dist
```

### Proxy Multiple Prefixes

```sh
pear \
  --proxy /api=http://127.0.0.1:3000 \
  --proxy /upload=http://127.0.0.1:4000 \
  ./dist
```

### Disable SPA Fallback

```sh
pear --no-spa ./dist
```

### Use A Config File

```sh
pear --config ./config.toml
```

<p align="right">(<a href="#pear">back to top</a>)</p>

## Configuration

`pear` reads `./config.toml` automatically when present, or you can specify a file explicitly with `--config`.

Example:

```toml
host = "127.0.0.1"
port = 8080
root = "./dist"
spa_fallback = true
keep_alive_timeout_secs = 5
keep_alive_max = 100
max_events = 256
max_connections = 4096
max_request_size = 1048576

[[proxy]]
prefix = "/api"
target = "http://127.0.0.1:3000"

[[proxy]]
prefix = "/upload"
target = "http://127.0.0.1:4000"

[[proxy]]
prefix = "/secure-api"
target = "https://api.example.com"
```

The proxy section also supports a compact map style:

```toml
[proxy]
"/api" = "http://127.0.0.1:3000"
"/upload" = "http://127.0.0.1:4000"
"/secure-api" = "https://api.example.com"
```

### Config Fields

- `host`: bind address, default `127.0.0.1`
- `port`: listen port, default `8080`
- `root`: directory to serve, default current directory
- `spa_fallback`: when `true`, unknown routes fall back to `index.html`
- `keep_alive_timeout_secs`: idle timeout for keep-alive connections
- `keep_alive_max`: maximum requests served on a single client connection
- `max_events`: Linux `epoll_wait` batch size
- `max_connections`: maximum concurrent client connections
- `max_request_size`: request size limit in bytes

### CLI Options

```text
-p, --port <PORT>                Port to listen on, default 8080
-H, --host <HOST>                Host to bind, default 127.0.0.1
-d, --dir <DIR>                  Directory to serve, default current directory
-c, --config <FILE>              Config file, default ./config.toml when present
    --keep-alive-timeout <SECS>  Idle keep-alive timeout, default 5
    --keep-alive-max <N>         Max requests per connection, default 100
    --max-events <N>             epoll batch size on Linux, default 256
    --max-connections <N>        Max open client connections, default auto-detected
    --max-request-size <BYTES>   Max request size, default 1048576
    --no-spa                     Disable fallback to index.html
    --proxy <RULE>               Reverse proxy rule, e.g. /api=https://api.example.com
-h, --help                       Show help
```

CLI scalar options override config file values such as `port`, `root`, and runtime limits.

CLI `--proxy` options are appended to proxy rules loaded from the config file.

<p align="right">(<a href="#pear">back to top</a>)</p>

## Runtime Tuning

The defaults are conservative and intended to work safely on development machines.

### `max_connections`

- On Unix systems, the default is auto-derived from the process file descriptor limit
- On non-Unix systems, it falls back to `4096`
- The derived value is clamped into a conservative range

If your environment has lower FD limits or tighter memory constraints, set it explicitly.

### `keep_alive_timeout_secs`

Shorter timeouts reduce idle connection cost. Longer timeouts improve client reuse, especially for browser-heavy workflows.

Suggested starting points:

- local development: `5`
- lower-resource environments: `2` to `5`
- more aggressive connection reuse: `10`

### `keep_alive_max`

This controls how many requests a client connection may reuse before reconnecting. It is more about fairness and connection lifetime than raw CPU tuning.

### `max_events`

Linux only. This controls how many ready events are pulled per `epoll_wait`.

- lower values reduce per-loop work
- higher values can improve throughput under heavier concurrency

### `max_request_size`

This protects the server from unbounded request buffering. For static serving and lightweight API proxying, `1048576` is typically enough.

<p align="right">(<a href="#pear">back to top</a>)</p>

## Implementation Notes

At a high level:

- request parsing is handwritten and intentionally minimal
- static file serving is optimized for frontend build artifacts and now streams large file bodies
- Linux uses an `epoll` event loop
- non-Linux platforms use a compatibility runtime
- proxied responses preserve client-side keep-alive semantics
- compatibility-runtime proxies stream upstream bodies directly to the client
The project favors a small codebase and low dependency surface over complete HTTP feature coverage.

<p align="right">(<a href="#pear">back to top</a>)</p>

## Current Limitations

`pear` is intentionally lightweight. It is useful, but it is not a full general-purpose production web server.

Current limitations include:

- request chunked transfer encoding is not supported
- proxying uses a buffering response path instead of event-loop streaming
- there is no TLS termination
- there is no HTTP/2 or advanced cache negotiation support

For frontend preview, same-origin local API proxying, and small deployments, these tradeoffs are often acceptable.

<p align="right">(<a href="#pear">back to top</a>)</p>

## Testing

Run the test suite:

```sh
cargo test
```

The current tests cover:

- request parsing
- keep-alive behavior decisions
- proxy response header rewriting
- HTTP and HTTPS upstream parsing
- HTTPS upstream proxying with a local TLS server
- path sanitization
- runtime config validation
- streamed static response shape
- reverse-proxy query merge behavior (`--proxy /api=http://upstream/base?fixed=1`)

### Load Testing With oha

Install `oha` first:

```sh
cargo install oha
```

Run a baseline load test and generate a machine-readable report:

```sh
python scripts/oha_bench.py \
  --url http://127.0.0.1:8080/ \
  --url http://127.0.0.1:8080/api/users \
  --requests 20000 \
  --connections 200 \
  --min-rps 3000 \
  --max-p99-ms 120 \
  --max-error-rate 0.01
```

The script writes `bench/oha_report.json` and exits non-zero when thresholds are exceeded, which makes it suitable for CI checks.

<p align="right">(<a href="#pear">back to top</a>)</p>

## Roadmap

- [x] Add keep-alive support
- [x] Add Linux `epoll` runtime
- [x] Add macOS and Windows compatibility runtime
- [x] Make runtime limits configurable
- [x] Add unit tests for protocol and config behavior
- [x] Add streaming responses for large static files
- [x] Add streaming proxy forwarding on the compatibility runtime
- [ ] Finish and validate streaming proxy forwarding on Linux `epoll`
- [ ] Support friendlier size and duration syntax in config and CLI
- [ ] Improve end-to-end integration tests

<p align="right">(<a href="#pear">back to top</a>)</p>

## Contributing

Suggestions, bug reports, and targeted improvements are welcome.

If you want to contribute:

1. Make a focused change
2. Add or update tests when behavior changes
3. Keep the implementation aligned with the project goal of being small and dependency-light

<p align="right">(<a href="#pear">back to top</a>)</p>

## License

No license file is included in this repository at the moment.

If you plan to publish or distribute this project more broadly, adding an explicit license would be a good next step.

<p align="right">(<a href="#pear">back to top</a>)</p>

## Acknowledgments

- The README structure was inspired by [othneildrew/Best-README-Template](https://github.com/othneildrew/Best-README-Template)

<p align="right">(<a href="#pear">back to top</a>)</p>
