# Crossflow

跨平台（Windows 11 ↔ macOS）低延迟、端到端加密的对等（P2P）键鼠、剪贴板与文件跨屏共享工具。

核心协议、网络发现、Noise 加密、多屏路由、剪贴板与文件传输全部采用 **纯 Rust** 实现，双端客户端分别使用原生 **WinUI 3（Windows）** 与 **SwiftUI / AppKit（macOS）**，遵循统一的「静白（Quiet White）」视觉设计。

---

## 核心特性

- **对称 P2P 自动发现与自动连接**
  - 基于 mDNS（`_crossflow._tcp.local.`）局域网自动发现，双端开箱即连，无主/从机之分。
  - **多网络信道优选与指定**：自动识别并排序 `有线直连 > 有线局域网 > Wi-Fi > ZeroTier 虚拟局域网`（自动过滤代理 TUN 与无效链路本地地址），支持指定网卡信道与对端直连 IP。
  - **Noise_NNpsk0 端到端加密**：默认局域网免密自动加密连接，可选配置共享密码隔离分组（持久化于本地配置）。
- **低延迟双向键鼠与多屏布局**
  - 最高 1000Hz 跨屏光标与平滑滚轮，支持任意多显示器可视化拖拽排列，实时双向同步布局。
  - macOS 端采用 `kCGHIDEventTap` 抑制与零延迟 0s-suppression 光标回归，双向穿越丝滑不卡边界。
  - 完整键位共享：原样保留 `Ctrl` / `Cmd(Win)` 快捷键（不破坏 macOS `Ctrl` 组合键与 Mission Control / Spaces 切屏），支持 **Caps Lock、Fn/Globe、小键盘、F1–F20** 以及 **双向音量 / 静音 / 播放暂停 / 切歌多媒体键**。
- **熄屏防断连与远程唤醒解锁**
  - macOS IOKit / Windows `SetThreadExecutionState` 阻止系统空闲休眠但允许熄屏节能；远端鼠标移入时自动唤醒屏幕并支持直接输入锁屏密码解锁。
- **中英文与大小写双向同步**
  - 鼠标跨出本机或焦点端切换输入状态时，通过独立加密辅助通道实时同步 Windows（IMM32 / HKL + Caps Lock）与 macOS（`cfprefsd` HIToolbox / Carbon TIS + IOKit `IOHIDModifierLockState`）。
- **加密剪贴板共享 & 流式文件/文件夹传输**
  - 独立 `Noise_NNpsk0` 加密辅助通道（默认端口 `24801`），与键鼠主通道（`24800`）完全解耦，传大文件不影响鼠标丝滑度。
  - 支持文本（最高 4 MiB）、截图/PNG 图片（最高 16 MiB，含 Windows `CF_DIB` 自动转 PNG）及 BLAKE2s 回声防环。
  - 支持 **`Ctrl+C` / `Cmd+C` 复制文件或文件夹跨屏自动直传** 以及 **拖入窗口 / 托盘菜单选文件发送**；32 KiB 加密分块 + 每文件 BLAKE2s 校验 + `.part` 原子落盘到 `Downloads/Crossflow`，完成后自动回填对端剪贴板即可直接粘贴，并附实时进度条、取消与打开目录按钮。
- **原生系统托盘 / 菜单栏常驻**
  - 自定义应用图标（`AppIcon.ico` / `AppIcon.icns`）与 4 态实时状态托盘图标（已连接、正在控制远端、搜索中、已暂停）。
  - 托盘快捷菜单支持一键暂停/恢复、剪贴板与中英文/大小写开关、网络信道切换、发送文件、打开接收目录、开机自启动（`--minimized` 静默进托盘）。

---

## 目录结构

```text
.
├── src/                        # Rust 共享核心（peer_mesh, pair_*, aux_channel, clipboard, input_state, power, platform/）
├── crates/crossflow-bridge/    # C ABI 桥接库（供 WinUI 3 / SwiftUI 调用布局校验与跨屏计算）
├── clients/
│   ├── design-tokens.json      # 双端共享静白设计令牌
│   ├── windows/Crossflow.Windows/  # Windows WinUI 3 原生客户端
│   └── macos/                  # macOS SwiftUI / AppKit 原生客户端（LSUIElement 菜单栏应用）
├── assets/                     # 应用图标母版（AppIcon-1024.png）
├── scripts/                    # 双端构建与验证脚本
└── docs/                       # 架构与协议文档
```

---

## 构建与测试

### Windows

需要 Rust 工具链与 .NET 10 SDK（项目根目录 `.tools/` 下的便携工具会自动被 `scripts/cargo-local.ps1` 识别）：

```powershell
# 运行 Rust 单元测试
.\scripts\cargo-local.ps1 test --lib

# 构建 Release 内核、桥接库与 WinUI 3 客户端
.\scripts\build-windows-ui.ps1 -Configuration Release
```

产物位于：`clients\windows\Crossflow.Windows\bin\x64\Release\net10.0-windows10.0.19041.0\win-x64\Crossflow.Windows.exe`

### macOS

需要 Rust 工具链与 Xcode Command Line Tools：

```sh
# 运行 Rust 单元测试
cargo test --lib

# 构建 Release 内核与桥接库
cargo build --release --bin crossflow -p crossflow -p crossflow-bridge

# 打包原生 Crossflow.app（含自动生成 AppIcon.icns）
CROSSFLOW_MACOS_APP_PATH="$PWD/dist/cross-screen/Crossflow.app" \
CROSSFLOW_CORE_EXECUTABLE="$PWD/target/release/crossflow" \
CROSSFLOW_BRIDGE_LIBRARY="$PWD/target/release/libcrossflow_bridge.dylib" \
sh ./scripts/build-macos-ui.sh
```

> **macOS 权限说明**：首次运行需在「系统设置 → 隐私与安全性」中为 `Crossflow.app` 授予 **辅助功能** 与 **输入监控** 权限，并使用固定开发证书签名以保持权限连续性。
