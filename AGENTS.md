# AGENTS.md — inox-xwayland

本仓库遵循根级 [AGENTS.md](https://github.com/Inox-desktop/inox/blob/main/AGENTS.md) 的**全部硬性规则**，本文件只补充仓库特有内容，不放宽任何根级约束。

## 职责

XWayland 集成服务。所属层级：**L2**。

## 仓库特有约束

- 默认禁止 `unsafe`：`Cargo.toml` 中 `[lints.rust] unsafe_code = "forbid"`。
- 非测试路径禁止 `unwrap()` / `expect()` / `panic!()` / `todo!()` / `unimplemented!()`。
- 日志统一使用 `tracing`，禁止 `println!` / `eprintln!`。
- M0 阶段**不引入 Inox 跨仓库依赖**：跨仓库依赖在首次需要它的里程碑中引入，并按根级 §8 使用锁定 `tag` 的 git 依赖。

## 主要依赖

`anyhow`, `tracing`, `tracing-subscriber`

## 命令

> 仅作参考，不作为本地验收依据；真正判定在 CI。

| 目的 | 命令 |
|------|------|
| 格式化 | `cargo fmt --all` |
| 格式检查 | `cargo fmt --all -- --check` |
| 静态检查 | `cargo clippy --all-targets --all-features -- -D warnings` |
| 测试 | `cargo test --all-features` |
| 构建 | `cargo build --release --all-features` |
| 文档 | `cargo doc --no-deps --all-features` |
| 依赖检查 | `cargo deny check` |

## 测试要求

- 纯逻辑必须有单元测试（`#[cfg(test)]`）。
- 测试代码不受“禁止 unwrap/expect/panic”约束。
- M0 骨架阶段暂无业务逻辑，测试随功能里程碑补充。

## 目录结构

```
src/            源码（lib.rs 库入口 / main.rs 可执行入口）
tests/          集成测试（按需创建）
.github/        CI 工作流
deny.toml       cargo-deny 依赖与许可证策略
```

## 提交信息格式

Conventional Commits，scope 使用 `xwayland`。
