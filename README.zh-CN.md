# pear

一个面向前端构建产物的小型 Rust 静态文件服务器，支持 SPA 回退、反向代理、keep-alive，以及 Linux 下的 `epoll` 运行时。

[English](README.md)

## 目录

1. [关于项目](#关于项目)
2. [技术栈](#技术栈)
3. [快速开始](#快速开始)
4. [使用方式](#使用方式)
5. [配置说明](#配置说明)
6. [运行时调优](#运行时调优)
7. [实现说明](#实现说明)
8. [当前限制](#当前限制)
9. [测试](#测试)
10. [路线图](#路线图)
11. [贡献](#贡献)
12. [许可证](#许可证)
13. [致谢](#致谢)

## 关于项目

`pear` 主要面向前端开发和轻量部署场景，尤其适合本地环境或私有网络内的测试与部署，用一个很小的二进制完成这些事情：

- 托管 `dist`、`build` 一类的前端产物目录
- 为 SPA 路由回退到 `index.html`
- 在同源下代理后端接口
- 在 Linux、macOS、Windows 上保持同一套 CLI 和配置格式

运行时策略按平台区分：

- Linux：`epoll + keep-alive`
- macOS / Windows：兼容运行时 + keep-alive

也就是说，三类系统都能用，只是底层 IO 模型不同。

当前流式传输状态：

- 静态文件响应已经支持大文件流式输出
- 兼容运行时下的代理响应已经支持直接流式转发
- Linux `epoll` 下的代理流式转发仍在重构中，当前工作区里还没有在 Linux 目标上完成验证

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 技术栈

- Rust
- Rust 标准库网络与 IO 原语
- Linux 下通过 FFI 调用 `epoll`

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 快速开始

### 前置条件

- Rust 工具链
- 一个前端构建目录，例如 `dist` 或 `build`

### 构建

```sh
cargo build --release
```

### 运行

托管当前目录：

```sh
pear
```

托管前端构建目录：

```sh
pear --port 8080 ./dist
```

如果你已经在 `dist` 目录中：

```sh
cd dist
/path/to/pear/target/release/pear -p 8080
```

访问：

```text
http://127.0.0.1:8080
```

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 使用方式

### 托管 SPA 构建产物

```sh
pear ./dist
```

默认情况下，未知路由会回退到 `index.html`，适合 React、Vue、Vite 等 SPA 项目。

### 托管静态资源并代理后端接口

```sh
pear --port 8080 --proxy /api=http://127.0.0.1:3000 ./dist
```

浏览器访问：

```text
http://127.0.0.1:8080/api/users
```

会被转发到：

```text
http://127.0.0.1:3000/api/users
```

也可以代理 HTTPS 上游：

```sh
pear --port 8080 --proxy /api=https://api.example.com ./dist
```

### 代理多个前缀

```sh
pear \
  --proxy /api=http://127.0.0.1:3000 \
  --proxy /upload=http://127.0.0.1:4000 \
  ./dist
```

### 关闭 SPA 回退

```sh
pear --no-spa ./dist
```

### 使用配置文件

```sh
pear --config ./config.toml
```

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 配置说明

如果当前目录存在 `config.toml`，`pear` 会自动读取；也可以通过 `--config` 指定其他文件。

示例：

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

代理配置也支持简写：

```toml
[proxy]
"/api" = "http://127.0.0.1:3000"
"/upload" = "http://127.0.0.1:4000"
"/secure-api" = "https://api.example.com"
```

### 配置字段说明

- `host`：监听地址，默认 `127.0.0.1`
- `port`：监听端口，默认 `8080`
- `root`：静态目录，默认当前目录
- `spa_fallback`：是否对未知路由回退到 `index.html`
- `keep_alive_timeout_secs`：keep-alive 空闲超时
- `keep_alive_max`：单连接最多处理请求数
- `max_events`：Linux 下 `epoll_wait` 每次提取的最大事件数
- `max_connections`：最大并发客户端连接数
- `max_request_size`：最大请求大小，单位字节

### 命令行参数

```text
-p, --port <PORT>                监听端口，默认 8080
-H, --host <HOST>                绑定地址，默认 127.0.0.1
-d, --dir <DIR>                  静态目录，默认当前目录
-c, --config <FILE>              配置文件，默认当前目录下的 ./config.toml（如果存在）
    --keep-alive-timeout <SECS>  keep-alive 空闲超时，默认 5
    --keep-alive-max <N>         单连接最多处理请求数，默认 100
    --max-events <N>             Linux 下 epoll 批量事件数，默认 256
    --max-connections <N>        最大客户端连接数，默认自动推导
    --max-request-size <BYTES>   最大请求大小，默认 1048576
    --no-spa                     关闭 index.html 回退
    --proxy <RULE>               反向代理规则，例如 /api=https://api.example.com
-h, --help                       显示帮助
```

CLI 中的标量参数会覆盖配置文件里的同名值，例如 `port`、`root`、`max_connections`。

CLI 中多个 `--proxy` 会追加到配置文件已有的代理规则后面。

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 运行时调优

这些参数都不是适合所有机器的绝对固定值，因此现在都支持配置。

### `max_connections`

- Unix 下会根据进程文件描述符上限自动推导
- 非 Unix 下回退到默认值 `4096`
- 推导值还会被限制在一个保守范围内

如果你的机器 FD 上限较低，或内存预算更严格，建议显式配置一个更小的值。

### `keep_alive_timeout_secs`

超时越短，空闲连接占用越少；超时越长，浏览器连接复用越充分。

建议起点：

- 本地开发：`5`
- 资源较紧的环境：`2` 到 `5`
- 更重视连接复用：`10`

### `keep_alive_max`

它控制一个连接最多复用多少次，主要是资源生命周期和公平性参数，不是直接的 CPU 参数。

### `max_events`

仅 Linux 生效。它控制一次 `epoll_wait` 最多返回多少 ready 事件。

- 较小的值：单轮处理更轻
- 较大的值：高并发下吞吐可能更好

### `max_request_size`

这个值用于限制请求缓冲区增长，避免无界内存占用。对于静态服务和轻量 API 代理，`1048576` 通常够用。

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 实现说明

从架构上看：

- 请求解析是手写的最小实现
- 静态文件服务针对前端构建产物做了优化，并已支持大文件流式输出
- Linux 使用 `epoll` 事件循环
- 非 Linux 使用兼容运行时
- 代理响应会重写连接相关头，保持客户端侧 keep-alive 语义
- 兼容运行时下，代理上游响应体会直接流式转发给客户端
- Linux 下的代理流式转发正在并入 `epoll` 事件循环，但当前工作区里还没有在 Linux 目标上完成验证

这个项目更强调体积小、依赖少、结构直接，而不是完整覆盖通用 Web Server 的所有 HTTP 能力。

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 当前限制

`pear` 是轻量工具，不是全功能通用生产级 Web Server。

当前限制包括：

- 不支持 request 侧的 chunked transfer encoding
- 代理响应目前走缓冲转发路径，不走事件循环内的流式转发
- 不支持 TLS 终止
- 不支持可配置的自定义错误页（例如 404/500 HTML 页面）
- 不支持 HTTP/2 和更完整的缓存协商

对于前端预览、同源 API 联调和小规模部署，这些限制通常是可以接受的。

### 本地/内网部署的 HTTPS 建议

`pear` 有意不内建 TLS 终止。如果你在本地、局域网演示或私有环境中需要 HTTPS，建议在 `pear` 前面加一层 Caddy 或 Nginx：

- 边缘代理负责证书与 HTTPS 监听
- `pear` 继续在内网/本机端口提供 HTTP
- 业务路由与静态/反代逻辑仍放在 `pear`

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 测试

运行测试：

```sh
cargo test
```

当前测试覆盖：

- 请求解析
- keep-alive 判定逻辑
- 代理响应头重写
- HTTP / HTTPS 上游解析
- 通过本地 TLS 服务验证 HTTPS 上游代理
- 路径清洗
- 运行时配置校验
- 流式静态响应形态
- 反向代理 query 合并行为（`--proxy /api=http://upstream/base?fixed=1`）

### 使用 oha 做压测评估

先安装 `oha`：

```sh
cargo install oha
```

执行压测并输出机器可读报告：

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

脚本会输出 `bench/oha_report.json`，并在阈值不达标时返回非 0 退出码，适合接入 CI。

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 路线图

- [x] 增加 keep-alive 支持
- [x] 增加 Linux `epoll` 运行时
- [x] 增加 macOS / Windows 兼容运行时
- [x] 将运行时限制改为可配置
- [x] 增加协议与配置相关单元测试
- [x] 为大文件增加流式静态响应
- [x] 为兼容运行时增加代理流式转发
- [ ] 完成并验证 Linux `epoll` 下的代理流式转发
- [ ] 支持更友好的大小和时间配置语法
- [ ] 可选支持可配置自定义错误页（例如 404/500）
- [ ] 增加端到端集成测试

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 贡献

欢迎提出建议、缺陷反馈和有针对性的改进。

如果你准备提交改动：

1. 保持改动聚焦
2. 行为变化时补上或更新测试
3. 尽量维持项目“小而轻、低依赖”的目标

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 许可证

当前仓库中还没有明确的许可证文件。

如果后续要更广泛地发布或分发，建议补上明确的 License。

<p align="right">(<a href="#pear">回到顶部</a>)</p>

## 致谢

- README 结构参考了 [othneildrew/Best-README-Template](https://github.com/othneildrew/Best-README-Template)

<p align="right">(<a href="#pear">回到顶部</a>)</p>
