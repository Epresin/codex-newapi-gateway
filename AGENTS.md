# 项目约定

本项目是个人自用的轻量 Codex ↔ New API 适配网关，**无需严格的版本管理或正式发版流程**。

## 协作与发布边界

- **轻量维护**：不接入 `internal-release` 等重型发布规范，不强制升版本号、写变更日志、打包归档或打 Git Tag。
- **按需提交**：仅在用户明确要求时执行 `git commit`、`git push` 或修改 `Cargo.toml` 版本号；日常改动保持简洁直接即可。

## 基本底线

- **凭据隔离**：绝不提交真实 `config.toml`、`data/` 缓存或任何 API Key；代理层保持请求头精准白名单与响应头安全黑名单，防止 ChatGPT 官方身份/凭据头泄露给上游。
- **验证习惯**：若当前环境具备 Rust 工具链，改动后运行 `cargo test --locked` 与 `cargo clippy --all-targets --locked -- -D warnings`；若环境未安装 Rust/Docker，做好静态核对并如实说明即可。
