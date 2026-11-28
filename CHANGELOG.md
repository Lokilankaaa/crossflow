# Changelog

All notable changes to **Crossflow** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased] (`dev`)

---

## [0.4.3-rc.3] - 2026-11-28 (Pre-release)

### Fixed
- **彻底修复 Windows 屏幕排列画布「拖不动 / 拖一下又拖不动」**：
  - 将屏幕块底色与 `PointerPressedEvent` 从 WinUI 3 `ContentControl`（默认模板不渲染/不命中 `Background`）移至外层 `Border`，实现整块屏幕任意像素 100% 可抓住拖动，并置顶当前拖拽块。
  - 调整 `Focus` 与 `CapturePointer` 顺序防止焦点切换误丢捕获；扩大边缘磁吸半径至 `24px`，松手轻微重叠时自动吸附至最近无重叠贴边位置；补齐 `lastLayoutSignature` 编号签名并增加 `700ms` 过期快照防弹回保护。
- **彻底根治 Windows 本机微信打字时「候选框悬空、文字不上屏、Backspace 偶发失灵」**：
  - 彻底删除 `read_caps_lock()` 中每 `350ms` 对前台应用调用的 `AttachThreadInput`，改为零副作用直接读取 `GetKeyState(VK_CAPITAL)`。
  - Windows 本机打字期间停止每 `350ms` 向微信 IME 窗口同步发送 `SendMessageTimeoutW(WM_IME_CONTROL)`，仅在跨屏切换瞬间读取并同步一次完整输入法状态。
- **连接中改变屏幕朝向 / 分辨率 / 插拔显示器实时热同步**：
  - 修复 Windows `WM_DISPLAYCHANGE` 错误触发 `Shortcut::Stop` 锁死 `user_paused` 的问题，改为无感自动重启采集与注入。
  - 新增 `MeshControl::DisplaysUpdate` 加密控制帧，连接状态下实时检测本机显示器变动并广播给所有节点热更新画布与路由表；若横竖屏切换导致屏幕间出现空隙或重叠，自动重新边缘吸合连通。
- **修复 macOS 锁屏黑屏后移动鼠标过去未自动点亮屏幕**：
  - 移除 `CGDisplayIsAsleep(CGMainDisplayID())` 误判跳过逻辑，改为复用 `IOPMAssertionID` 无条件声明 `IOPMAssertionDeclareUserActivity(kIOPMUserActiveLocal)`，并在收到 `Kind::Arm` / `FocusState` / 远端输入时立即点亮黑屏（即使 `native_io` 暂未重建也立即唤醒并在重建后自动承接控制权）。

---

## [0.4.3-rc.2] - 2026-11-28 (Pre-release)

### Fixed
- **修复 Windows 中文输入法下 Backspace 偶发不识别、无法删除已打文字**：
  - Windows 接收端对 `Backspace`、`Enter`、`Space`、`Tab`、`Escape`、`Delete`、`Insert`、方向键及 `Home/End/PageUp/PageDown` 改为携带显式 `wVk` + 硬件 `wScan` 注入（不再使用易被微软拼音 / 搜狗 / 微信输入法 TSF 吞掉的 `wVk = 0` 纯扫描码模式）。
  - 接收端增加 `250ms` 自身刚释放键位宽限窗，根治 Wi-Fi 突发包或快速连按 Backspace/鼠标键时因 Windows `GetAsyncKeyState` 异步滞后误判「本地已按住」导致的丢键与中断。
  - 发送端底层钩子对 `VK_BACK` 等编辑控制键增加 `vkCode` 直接映射兜底，避免部分笔记本或输入法驱动出现 `scanCode == 0` 时漏发。

---

## [0.4.3-rc.1] - 2026-11-28 (Pre-release)

### Fixed
- **彻底修复 Windows 下打开任务管理器 / 管理员窗口导致卡死或无法来回切屏**：
  - 修复普通权限模式下 Windows UIPI 拦截 `SetPhysicalCursorPos` / `SendInput`（`Win32 5 / ERROR_ACCESS_DENIED`）时错误触发 `physical_stop` 并永久锁死 `user_paused` 或反复杀掉 `input-worker` 的问题；打开任务管理器等高权限窗口不再中断跨屏控制。
- **大幅优化双 Windows Wi-Fi 连接及老旧 CPU（如六七年前 i7 笔记本）流畅度与稳定性**：
  - 在 `WindowsMouseSink`（普通与服务模式）内对桌面状态与显示器列表增加 `100ms` 缓存并消除 `move_by` 重复校验，将每帧鼠标移动的系统调用开销降低 50 倍以上。
  - 有线直连 / 有线局域网保持完整 `1000 Hz`，Wi-Fi / ZeroTier 通道自动自适应 `125 Hz (8ms)` 平滑合并发送并加入 TCP 队列背压（阈值 8，无损累加位移，点击/按键/切边 0ms 即时转发），队列上限提至 `1024` 并批处理 `flush()`。
  - 接收端新增 `coalesce_inbound_packets` 无损合并 Wi-Fi 突发鼠标移动帧（严格保留按键/滚轮/切屏顺序屏障），异常时主动向对端回发 `RevokeControl`。
  - 放宽旧款 CPU 与 Wi-Fi 弱网场景下的超时阈值：`MAX_REQUEST_AGE_NS` / `RPC_TIMEOUT` / `INPUT_MAX_AGE_NS` `30ms → 250ms`、`ACTIVE_LEASE_NS` `150ms → 600ms`、`OWNER_LEASE_NS` `100ms → 1500ms`、Wi-Fi 建连超时 `300~350ms → 1200ms`、握手超时 `2s → 4s`、静默断连 `9s → 18s`。

---

## [0.4.2] - 2026-11-28

### Added
- **「关于与社区支持」页面（Windows + macOS）**：新增「更新与诊断」与「社区与支持」双页签页面（侧边栏顶部 `ⓘ`、侧边栏底部「关于与社区」及托盘/菜单栏快捷入口），展示 MIT 开源 / 免费无广告 / 本地零遥测理念以及项目仓库、作者 GitHub 与哔哩哔哩主页入口。
- **自更新加速线路与通道升级**：支持选择下载加速线路（`GitHub 官方直连`、`GH-Proxy · gh-proxy.com`、`GHFast · ghfast.top`、`自定义 HTTPS 镜像前缀`），镜像超时或 SHA-256 校验异常时自动回退 GitHub 官方直连；支持切换「仅正式版」与「含测试版 (-rc)」通道，主查免 API 限流的 `releases.atom`；新增一键复制脱敏诊断并前往 GitHub Issues 反馈以及本地日志/配置目录快捷入口。

### Fixed
- **彻底修复长待机唤醒后剪贴板共享失效**：
  - 在 `MeshControl::Hello` 中显式交换对端真实 `aux_port`（入站旧版兜底 `24801`），根治由对端主动建连时错误拨号 `临时源端口 + 1` 导致 `24801` 辅助通道无法连接的问题。
  - `AuxMeshHub::stop()` 改为同步等待旧 `accept` 线程释放端口后再重绑，并增加 10 次重试与动态端口兜底，根治唤醒重建时 `24801` 监听丢失的问题。
  - `AuxSession` 新增自愈重连循环与过期入站连接清理，并对重连失败日志做节流防止日志文件膨胀。
  - Windows 剪贴板写入改用标准 `HWND_MESSAGE` 线程所有者窗口修复无窗口后台进程下 `SetClipboardData` 失败隐患；移除 `last_sent_hash` 拦截，确保重启/重连后重复复制同一段文本也能立即同步。

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
