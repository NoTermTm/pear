# mini-server

A small Rust static file server for frontend build output.

## Usage

Build it:

```sh
cargo build --release
```

Run it inside a frontend `dist` directory:

```sh
cd dist
/path/to/mini-server/target/release/mini-server -p 8080
```

Or serve a directory directly:

```sh
mini-server --port 8080 ./dist
```

Serve `dist` and proxy API requests to a backend while the browser still uses the frontend origin:

```sh
mini-server --port 8080 --proxy /api=http://127.0.0.1:3000 ./dist
```

Or put the same settings in `config.toml`:

```toml
host = "127.0.0.1"
port = 8080
root = "./dist"
spa_fallback = true

[[proxy]]
prefix = "/api"
target = "http://127.0.0.1:3000"
```

Then run:

```sh
mini-server
```

Open:

```text
http://127.0.0.1:8080
```

## Options

```text
-p, --port <PORT>    Port to listen on, default 8080
-H, --host <HOST>    Host to bind, default 127.0.0.1
-d, --dir <DIR>      Directory to serve, default current directory
-c, --config <FILE>  Config file, default ./config.toml when present
    --no-spa         Disable fallback to index.html
    --proxy <RULE>    Reverse proxy rule, e.g. /api=http://127.0.0.1:3000
-h, --help           Show help
```

By default, unknown routes fall back to `index.html`, which works well for React, Vue, Vite, and other SPA builds using frontend routing.

Proxy rules use `prefix=http://host:port`.

```sh
mini-server --proxy /api=http://127.0.0.1:3000 --proxy /upload=http://127.0.0.1:4000 ./dist
```

With `/api=http://127.0.0.1:3000`, a browser request to `http://127.0.0.1:8080/api/users` is forwarded to `http://127.0.0.1:3000/api/users`.

If the upstream target includes a path, that path replaces the matched prefix:

```sh
mini-server --proxy /api=http://127.0.0.1:3000/backend ./dist
```

Then `/api/users` is forwarded to `/backend/users` on the upstream server.

The config file can also use a compact proxy map:

```toml
[proxy]
"/api" = "http://127.0.0.1:3000"
"/upload" = "http://127.0.0.1:4000"
```

CLI options override scalar config values such as `port` and `root`. CLI `--proxy` values are appended to proxy rules loaded from the config file.
