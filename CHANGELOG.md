# Changelog

All notable changes to **Crossflow** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased] (`dev`)

---

## [0.4.1] - 2026-11-25

### Added
- **双端应用内自更新（Windows + macOS）**：内置免 API 限流版本检测（优先读取 GitHub `releases/latest` + `SHA256SUMS.txt`）、启动 8 秒后静默检查、侧边栏与托盘/菜单栏快捷入口、实时进度条与取消下载、强制 **SHA-256 完整性校验**、防 Zip-Slip 解压、Windows 服务模式自动刷新以及 macOS 签名校验与失败自动回滚替换。
- **每设备独立光标与滚轮手感配置**：支持按对端设备分别调节光标速度（`0.25×`–`3.00×`）、滚轮速度（`0.25×`–`3.00×`，带浮点残差累积防丢步）、上下/左右滚轮反转及一键恢复默认，热生效无需重连并持久化于 `peer-config.json`。
- **规范化开发流程**：建立 `dev` 日常集成分支，新增 `CHANGELOG.md`、更新 `CONTRIBUTING.md` 及 GitHub PR / Issue 模板。

### Fixed
- **过夜待机唤醒自动重连**：取消 `crossflow peer` 24 小时强制退出上限（启用 `--seconds 0` 常驻）；新增内核时钟跳变/网卡变动自愈、Windows `WM_POWERBROADCAST` 与 macOS `didWakeNotification` 唤醒重连、异常退出 2 秒自动拉起，以及 macOS 菜单栏启动即刻拉起内核。
- **自动静默升级最优信道**：网卡短暂抖动不再清除用户固定信道偏好；当连接临时回落至 Wi-Fi / ZeroTier 时，空闲期后台自动探测并无感热升级至有线直连（`有线直连 > 有线局域网 > Wi-Fi > ZeroTier`），并拒绝慢信道反向覆盖。

---

## [0.4.0] - 2026-11-23

### Added
- **全局屏幕编号（`#1`–`#9`）与毫秒级快捷键闪切（QuickJump）**：支持按编号直达任意屏幕及上一屏/下一屏循环切换（< 5 ms 即时响应），切屏保留本机游戏手柄直通。
- **全自由快捷键与鼠标侧键录制**：支持自定义前缀、普通键、`F1–F24`、`CapsLock` 以及 `Mouse4` / `Mouse5` / `MouseMid` 底层硬件录制。
- **Windows 可选系统服务模式（`CrossflowInputService`）**：设置页一键安装/更新/卸载 `LocalSystem` 服务，支持 UAC 安全桌面、管理员窗口与锁屏输入。
- **界面优化**：双端可拖拽缩放侧边栏分隔条、Windows 标题栏双击最大化/还原、macOS 标准缩放及全面重写的双语 README 与 GitHub 主页展示图。

### Fixed
- 统一 Windows `INPUT_ORIGIN_TAG`，修复 macOS 发往 Windows 的音量/媒体键及鼠标侧键误触发控制权回收的问题。
- macOS 增加 `kCGSessionEventTap` 辅助拦截 `NX_SYSDEFINED`，补齐 F7/F9（Rewind/Fast）媒体键映射。
- 焦点位于远端时正确拦截并转发 Windows 计算器、邮件、资源管理器、媒体停止、浏览器主页等消费级功能键。
- 将 `PrintScreen` / `ScrollLock` / `Pause` 映射至 macOS `F13–F15`，其余未映射合法 HID 键静默忽略，不再导致注入进程重启或光标弹回边界。
- 修复焦点在远端时松开再重按修饰键导致快捷键失效的问题。

---

## [0.3.3] - 2026-11-22

### Added
- N 设备全互联 Full Mesh P2P 架构、多设备布局同步、按设备单独指定网络信道与多 IP 手动节点管理。
- 自定义应用图标、4 态托盘/菜单栏图标及开机自启动（`--minimized`）。
- 独立 `Noise_NNpsk0` 加密辅助通道（`24801`）：剪贴板文本/图片共享、流式文件与文件夹直传、中英文输入法与 Caps Lock 双向同步、熄屏防断连与远程唤醒。
