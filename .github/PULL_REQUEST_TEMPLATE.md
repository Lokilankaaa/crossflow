## 变更摘要

- **目标分支**：默认合并至 `dev`（仅发版合并或 `hotfix/*` 指向 `main`）
- **影响平台**：
  - [ ] Rust 共享核心（`src/`、`crates/`）
  - [ ] Windows 客户端（WinUI 3）
  - [ ] macOS 客户端（SwiftUI / AppKit）
  - [ ] 文档 / 工程配置

### 解决的问题 / 新增行为

<!-- 简述问题根因或新功能设计 -->

---

## 验证清单

- [ ] Windows Rust 单测通过（`.\scripts\cargo-local.ps1 test --lib`）
- [ ] macOS Rust 单测通过（`cargo test --lib`）
- [ ] 受影响平台 Release 编译 0 警告 0 错误
- [ ] 已进行双机实机验证（未测项请注明「未测」）
- [ ] 未提交任何私钥、`peer-config.json`、日志、`.tools/` 或编译产物
