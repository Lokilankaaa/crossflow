# Crossflow

跨平台（**Windows 11 ↔ macOS**，支持 **N 台设备全互联 Full Mesh**）低延迟、端到端加密的对等（P2P）键鼠、剪贴板与文件跨屏共享工具。

核心协议、局域网发现、Noise 加密、多屏拓扑路由、全局快捷键引擎、剪贴板与文件传输全部采用 **纯 Rust** 实现；双端客户端分别基于原生 **WinUI 3（Windows）** 与 **SwiftUI / AppKit（macOS）** 构建，遵循统一的「静白（Quiet White）」视觉设计，零云端依赖、局域网开箱即连。

---

## 核心特性

### 1. N 设备全互联对等网络（Full Mesh P2P）
- **对称自动发现与自动重连**：基于 mDNS（`_crossflow._tcp.local.`）局域网自动发现，无主/从机之分，支持 3 台及以上 Windows / macOS 设备自由组网。
- **多网络信道智能优选**：自动识别并排序 `有线直连 > 有线局域网 > Wi-Fi > ZeroTier 虚拟局域网`（自动过滤代理 TUN 与无效链路本地地址），并支持手动指定网卡信道或直连对端 IP。
- **端到端加密**：全部通道采用 `Noise_NNpsk0`（Curve25519 + ChaCha20-Poly1305 + BLAKE2s）端到端加密，默认局域网免密自动连接，可选配置共享密码隔离分组。

### 2. 多屏可视化布局、全局编号与毫秒级闪切（QuickJump）
- **自由拖拽与实时同步**：支持任意多台设备的多块显示器可视化拖拽排列与边缘吸附，布局变更实时同步至全网节点；支持可拖拽调整宽度的侧边栏分隔条与双击标题栏最大化/还原。
- **全局屏幕编号（`#1`–`#9`）**：全网显示器按空间行列确定性编号（`#1`–`#9`），在画布与侧边栏同步标注。
- **毫秒级快捷键闪切**：无需等待修饰键松开（< 5 ms 即时切换），支持按全局编号直达指定屏幕以及 **上一屏 / 下一屏循环切换**。
- **全自由快捷键录制**：支持任意修饰键前缀、普通键盘按键、`F1–F24`、`CapsLock`，以及 **鼠标中键与侧键（`MouseMid` / `Mouse4` / `Mouse5`）**，通过系统底层钩子直接硬件录制。
- **手柄直通不中断**：切屏控制其他设备时保留本机游戏手柄（Gamepad）直通，互不干扰。

### 3. 低延迟全键位键鼠与系统功能键共享
- **最高 1000Hz 光标与平滑滚轮**：Windows 端基于 Raw Input + `WH_KEYBOARD_LL` / `WH_MOUSE_LL` 精准路由，macOS 端采用 `kCGHIDEventTap` + `kCGSessionEventTap` 双层捕获与零延迟光标回归，跨屏丝滑不卡边界。
- **1:1 原样键位直通**：默认不强制互换 `Ctrl` 与 `Cmd(Win)`，完整保留 macOS `Ctrl` 组合键及 Mission Control / Spaces 切屏习惯。
- **全功能键覆盖**：
  - 完整支持 **Caps Lock、Fn/Globe、小键盘、F1–F24**。
  - **双向音量与多媒体控制**：音量加/减、静音、播放/暂停、上一曲/下一曲在 Windows ↔ macOS 间双向直通且不误抢焦点。
  - **Windows 消费级快捷键与特殊键透传**：焦点在远端时正确拦截并转发计算器、邮件、资源管理器、媒体停止、浏览器主页等按键；`PrintScreen` / `ScrollLock` / `Pause` 自动映射为 macOS `F13–F15`。

### 4. Windows 可选系统服务模式（UAC / 管理员窗口 / 锁屏穿透）
- 内置可选 **`CrossflowInputService`**（`LocalSystem` 权限，通过受限本地命名管道通信），在设置页一键安装、更新或卸载。
- 启用后可在 **UAC 安全桌面、管理员权限窗口（任务管理器 / 终端等）以及 Windows 锁屏界面** 下无缝继续接收跨屏键鼠输入。

### 5. 熄屏防断连、远程唤醒与输入状态同步
- **智能防休眠**：macOS IOKit / Windows `SetThreadExecutionState` 阻止系统空闲休眠但允许熄屏节能；远端鼠标移入时自动唤醒屏幕并支持直接输入锁屏密码解锁。
- **中英文与大小写双向同步**：跨屏或切换输入法状态时，通过独立加密辅助通道实时同步 Windows（IMM32 / HKL + Caps Lock）与 macOS（HIToolbox / Carbon TIS + IOKit `IOHIDModifierLockState`）。

### 6. 加密剪贴板共享 & 流式文件/文件夹传输
- **独立双通道隔离**：剪贴板、文件传输与输入状态走独立加密辅助通道（默认 TCP `24801`），与键鼠实时主通道（默认 TCP `24800`）完全解耦，传大文件丝毫不影响鼠标延迟。
- **加密剪贴板**：支持纯文本（最高 4 MiB）与截图/PNG 图片（最高 16 MiB，自动处理 Windows `CF_DIB` ↔ PNG 转换），基于 BLAKE2s 哈希防止回声循环。
- **跨屏文件/文件夹直传**：支持 **`Ctrl+C` / `Cmd+C` 复制文件或文件夹跨屏自动直传**，以及拖入窗口或托盘菜单选文件发送；32 KiB 加密分块 + 逐文件 BLAKE2s 校验 + `.part` 原子落盘至 `Downloads/Crossflow`，完成后自动回填对端剪贴板可直接粘贴，并附实时进度条、取消与一键打开目录按钮。

### 7. 原生系统托盘 / 菜单栏常驻
- 自定义应用图标（`AppIcon.ico` / `AppIcon.icns`）与 4 态实时状态图标（已连接、正在控制远端、搜索中、已暂停）。
- 托盘/菜单栏快捷菜单支持一键暂停/恢复、剪贴板与输入法同步开关、网络信道切换、发送文件、打开接收目录及开机自启动（`--minimized` 静默常驻）。

---

## 快速开始

前往 [Releases](https://github.com/Lokilankaaa/crossflow/releases) 页面下载最新预编译版本：

| 平台 | 发行包 | 系统要求 |
| :--- | :--- | :--- |
| **Windows (x64)** | `Crossflow-vX.Y.Z-windows-x64.zip` | Windows 10 19041+ / Windows 11（内置自包含 WinUI 3 与 .NET 10 运行时，解压即用） |
| **macOS (Apple Silicon)** | `Crossflow-vX.Y.Z-macos-arm64.dmg` / `.zip` | macOS 14.0+（Apple Silicon） |

### 首次运行说明

- **Windows**：
  1. 解压后直接运行 `Crossflow.Windows.exe`。
  2. 如需控制 UAC 弹窗、管理员窗口或锁屏界面，可在应用设置中点击 **「安装系统服务」** 启用可选的 `CrossflowInputService`（升级版本后点 **「更新 / 重启服务」** 即可同步）。
- **macOS**：
  1. 将 `Crossflow.app` 拖入「应用程序」文件夹。若未公证触发门禁提示，可在终端执行一次：
     ```sh
     xattr -dr com.apple.quarantine /Applications/Crossflow.app
     ```
  2. 首次启动需在 **「系统设置 → 隐私与安全性」** 中为 `Crossflow.app` 开启 **辅助功能（Accessibility）** 与 **输入监控（Input Monitoring）** 权限。

### 常用快捷键

| 功能 | 说明 |
| :--- | :--- |
| **按屏幕编号闪切** | 默认使用自定义前缀 + `1`–`9`（对应画布上的 `#1`–`#9` 全局屏幕编号） |
| **上一屏 / 下一屏循环切换** | 可在设置页绑定任意键鼠组合（例如 `Ctrl+Alt+Mouse4` / `Ctrl+Alt+Mouse5`） |
| **安全紧急停止** | Windows：`Ctrl + Alt + Esc`；macOS：`Control + Option + Esc`（绝不占用单独 `Esc` 键） |

---

## 网络端口与目录结构

- **UDP `5353`**：mDNS 局域网自动发现（`_crossflow._tcp.local.`）
- **TCP `24800`**：Noise 加密键鼠实时主通道（`TCP_NODELAY`）
- **TCP `24801`**：Noise 加密辅助通道（剪贴板、流式文件/文件夹、输入法与大小写同步）

```text
.
├── src/                               # Rust 共享核心（peer_mesh, mesh_runtime, hotkeys, pair_*, aux_channel, clipboard, input_state, power, windows_service, platform/）
├── crates/crossflow-bridge/           # C ABI 桥接库（供 WinUI 3 / SwiftUI 调用布局校验与跨屏计算）
├── clients/
│   ├── design-tokens.json             # 双端共享「静白」设计令牌
│   ├── windows/Crossflow.Windows/     # Windows 原生 WinUI 3 客户端
│   └── macos/                         # macOS 原生 SwiftUI / AppKit 菜单栏应用
├── assets/                            # 应用图标母版（AppIcon-1024.png）
├── scripts/                           # 双端构建与工具链脚本
└── docs/                              # 架构与协议文档
```

---

## 从源码构建与测试

### Windows

需要 Rust 工具链（`1.85+`）与 .NET 10 SDK（项目根目录 `.tools/` 下的便携工具会自动被 `scripts/cargo-local.ps1` 识别）：

```powershell
# 运行 Rust 单元测试
.\scripts\cargo-local.ps1 test --lib

# 编译 Release 内核、桥接库与 WinUI 3 客户端
.\scripts\build-windows-ui.ps1 -Configuration Release
```

产物位于：`clients\windows\Crossflow.Windows\bin\x64\Release\net10.0-windows10.0.19041.0\win-x64\Crossflow.Windows.exe`

### macOS

需要 Rust 工具链（`1.85+`）与 Xcode Command Line Tools：

```sh
# 运行 Rust 单元测试
cargo test --lib

# 编译 Release 内核、Keychain 助手与桥接库
cargo build --release --target aarch64-apple-darwin \
  --bin crossflow --bin crossflow-keychain -p crossflow -p crossflow-bridge

# 打包原生 Crossflow.app（含自动生成 AppIcon.icns）
CROSSFLOW_MACOS_APP_PATH="$PWD/dist/cross-screen/Crossflow.app" \
CROSSFLOW_CORE_EXECUTABLE="$PWD/target/aarch64-apple-darwin/release/crossflow" \
CROSSFLOW_MACOS_KEYCHAIN_HELPER="$PWD/target/aarch64-apple-darwin/release/crossflow-keychain" \
CROSSFLOW_BRIDGE_LIBRARY="$PWD/target/aarch64-apple-darwin/release/libcrossflow_bridge.dylib" \
sh ./scripts/build-macos-ui.sh
```
