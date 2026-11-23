# Codex New API Gateway

轻量、无状态的 Codex 专用 New API 适配网关。

在不要求客户端放弃 ChatGPT 登录态、不保存用户 Key、不建立用户数据库的前提下，为 Codex 提供按 Key 实时生成的模型目录与全链路代理（HTTP / SSE / WebSocket / Search / Image）。

```text
Codex -> Nginx (/codex/) -> codex-newapi-gateway -> New API -> upstream channels
```

## 核心特性

- **严格凭据隔离**：默认仅接受专用请求头（默认 `X-NewAPI-Key`），缺失、为空或重复直接拒绝；不会把客户端原有的 ChatGPT `Authorization`、`Cookie` 或账号身份头转发给 New API。
- **按 Key 实时模型目录**：每次 `GET /models` 使用当前 Key 实时查询 New API，响应标记 `Cache-Control: no-store` 且按实际 `key_header` 输出 `Vary`，撤销即时生效、不跨用户缓存目录。
- **公共元信息 + 稀疏覆盖**：自动同步 [models.dev](https://models.dev/api.json) 公共模型规格（支持 ETag 与本地磁盘缓存，失败时自动降级保留最后有效数据）。合并优先级：`[models."<id>"]` > `models.dev` > `[defaults]`。
- **全配置化策略**：默认隐藏关键词、自动评审模型优先级（`auto_review_model_override`）、默认模型排序、服务档位（`service_tiers` / `extra_service_tiers`）、`ultra` 推理档位、上游超时和请求体上限均通过单一 TOML 文件配置，无私有模型硬编码。
- **无损流式与双向 WebSocket 透传**：支持 HTTP/SSE 与 `GET /responses` WebSocket 升级字节级透传；请求头采用精准协议白名单（完整透传 `session-id` / `thread-id` / `x-codex-turn-state` / `originator` / `openai-beta` / `x-openai-internal-codex-responses-lite` 等会话与特性头，严防官方身份头外泄），响应头采用安全黑名单透传（回传 `x-codex-turn-state`、限流与模型诊断头，剥离 hop-by-hop、`Set-Cookie`、`WWW-Authenticate`、`X-Models-Etag` 等）；拒绝上游重定向；不改写推理请求正文与 `service_tier`；日志仅输出方法、路径、状态码、时延和 WS 字节数，不记录 Key 或正文。

## 目录结构

```text
.
├── configs/
│   ├── gateway.example.toml         # 统一服务端配置模板（本地 & Docker 共用）
│   └── client.chatgpt.example.toml  # Codex 客户端配置示例
├── deploy/
│   ├── docker-compose.yml           # Docker Compose 部署编排
│   └── nginx-location.conf          # Nginx 反向代理片段
├── src/
│   ├── main.rs                      # CLI 入口、缓存刷新与优雅退出
│   ├── config.rs                    # 配置结构与严格校验
│   ├── proxy.rs                     # 路由白名单、鉴权隔离、HTTP/SSE/WS 转发
│   ├── catalog.rs                   # Codex 模型目录文档构建
│   ├── model_policy.rs              # 模型元信息合并、可见性与压缩策略
│   └── models_dev.rs                # models.dev 公共规格拉取与缓存
├── tests/
│   └── gateway.rs                   # 集成与回归测试
└── Dockerfile
```

## 快速开始

### 1. 本地运行

```sh
cp configs/gateway.example.toml config.toml
# 按需修改 config.toml 中的 upstream.base_url 等参数

cargo run --locked -- --config config.toml
```

也支持通过环境变量 `GATEWAY_CONFIG=/path/to/config.toml` 或 `-c /path/to/config.toml` 指定配置文件。

### 2. Docker Compose 部署

1. 复制统一配置模板到仓库根目录：
   ```sh
   cp configs/gateway.example.toml config.toml
   ```
2. 按容器网络修改 `config.toml`：
   - `server.listen = "0.0.0.0:8080"`
   - `upstream.base_url = "http://new-api:3000/v1"`
   - `metadata.cache_dir = "/data/cache"`
3. 确保外部 Docker 网络（示例为 `edge-proxy`）存在后启动：
   ```sh
   docker compose -f deploy/docker-compose.yml up -d --build
   ```
4. 容器默认仅映射宿主机 `127.0.0.1:17329:8080`，配合 [deploy/nginx-location.conf](deploy/nginx-location.conf) 挂载到你的 HTTPS 域名 `/codex/` 路径下（保持 `Upgrade` 透传并关闭流缓冲）。

> 请勿将网关自身的公网 `/codex` 地址配置为 `upstream.base_url`，以免形成回环。

### 3. Codex 客户端配置

参考 [configs/client.chatgpt.example.toml](configs/client.chatgpt.example.toml)，建议先在独立的 Codex Profile 中验证：

- 保留正常 ChatGPT 登录（`requires_openai_auth = true`），不覆盖内置 `openai` Provider ID，使用自定义 Provider ID（例如 `newapi`）。
- 将 `base_url` 指向你的网关地址（例如 `https://gateway.example.com/codex`）。
- 通过 `http_headers` 或 `env_http_headers` 传递 `X-NewAPI-Key`（不要同时配置，也不要把 ChatGPT Token 填入其中）。

## 配置说明

完整配置项与默认值见 [configs/gateway.example.toml](configs/gateway.example.toml)，所有段均开启 `deny_unknown_fields` 严格校验：

| 配置段 | 说明 |
| --- | --- |
| `[server]` | 监听地址、是否拒绝浏览器 `Origin`、策略检查请求体上限、`GET /models` 响应体上限 |
| `[upstream]` | New API 上游地址（禁止携带凭据/查询参数）、连接与读取超时 |
| `[auth]` | 专用 Key 请求头名称（禁止为 `Authorization`/`Cookie`）、是否允许旧客户端 Bearer 回退（默认 `false`） |
| `[metadata]` | `models.dev` 同步开关、地址、刷新间隔、超时、响应大小上限、本地缓存目录及优先命名空间列表 |
| `[catalog]` | 默认置顶模型、默认隐藏关键词（默认 `["image", "autoreview"]`）、自动评审候选模型列表、描述是否附带上下文长度、`ultra` 推理档位开关与文案等 |
| `[defaults]` | 全局模型默认元信息（压缩比例、上下文窗口、推理档位、`service_tiers`、`additional_speed_tiers`、`use_responses_lite` 等） |
| `[models."<id>"]` | 按精确上游模型 ID 进行稀疏覆盖，支持 `enabled`、`visible`、`source`、`description`、`compaction`、`service_tiers`、`extra_service_tiers` 等 |

> 说明：`visible = false` 仅在模型选择器中隐藏，仍可通过工具或自动评审调用；`enabled = false` 会彻底拒绝该模型的请求（并在存在禁用模型时对 WS 返回 `426` 引导客户端回退到 HTTP 以便检查请求体）。

## 支持的接口

Nginx 剥离 `/codex/` 前缀后，网关暴露以下白名单接口（其余路径一律返回 `404`）：

- `GET /healthz`：健康检查（唯一免鉴权接口）
- `GET /models`：按当前 Key 实时转换 Codex 模型目录
- `POST /responses`：HTTP / SSE 流式转发
- `GET /responses`：WebSocket Upgrade 双向字节透传
- `POST /responses/compact`：兼容转发
- `POST /alpha/search`：搜索转发
- `POST /images/generations`：生图转发
- `POST /images/edits`：修图转发

## 开发与测试

```sh
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

## License

[MIT](LICENSE)
