# Crossflow

跨设备键鼠共享系统。各端使用同一个 **Rust 核心**，平台适配层负责系统输入接口；客户端使用统一设计语言。当前为阶段 0：默认 Windows → Mac 鼠标移动，另有显式开启的实验键盘、按钮和滚轮模式，不是完整 KVM 产品。

## 当前可以做什么

- Windows Raw Input 采集相对鼠标移动；全局快捷键明确开启和停止共享。
- 同一套 Rust 协议、身份认证、加密传输、控制会话、日志代码运行于 Windows 和 macOS。
- Mac 使用 CoreGraphics 注入光标移动，并记录接收和注入调用耗时。
- 显式离线配对、双方固定公钥认证、每次连接建立新的加密会话。
- 有界移动合并、短期接收许可、断线停止、重新连接后保持暂停。
- 不操作系统光标的加密回环测试、采集日志和统计摘要。

默认仍只发送鼠标移动。实验模式加入按钮、滚轮与物理键身份；剪贴板尚未实现。Windows 本地输入不会被抑制，Mac 仍限主显示器和线性鼠标增益。不能据此宣称完成双向控制、游戏保护、多屏穿越或 ShareMouse 替代。

**当前协议为 v2，两端必须同时更新和重新构建。** 旧版 v1 会被明确拒绝，已有配对身份可保留；不能混用旧 Mac 程序与新 Windows 程序。

## 构建

需要 Rust 工具链。Windows 的本项目便携工具位于 `.tools/`，不会进入 Git，也不修改系统 PATH：

```powershell
./scripts/cargo-local.ps1 build --release
./scripts/cargo-local.ps1 test
```

在已有标准 Rust 工具链的 Windows、Mac 或 Linux 上使用：

```sh
cargo build --release --locked
cargo test --locked
```

Mac 需要 Xcode Command Line Tools 提供系统框架链接能力。Windows GNU 便携工具只用于本机开发，不是产品分发依赖；生成的 Windows 程序可单独测试。`Cargo.lock` 固定依赖版本。

## 首次配对

```powershell
./target/release/crossflow.exe pair --out .pairing
```

只会输出公钥及配对标识。通过可信的本地方式把 `.pairing/mac/` 交给 Mac，Windows 保留 `.pairing/windows/`。每个目录内的 `identity.json` 包含该设备私钥，不能提交 Git、发到公开聊天或放在公共下载地址。Mac 设置目录权限 `700`、文件权限 `600`。已存在的配对目录不会被覆盖。重新配对时双方都替换为新的一对身份，原身份不再被对方接受。

阶段 0 采用 `Noise_KK_25519_ChaChaPoly_BLAKE2s` 固定公钥认证与加密，运行于 `TCP_NODELAY` 连接；使用 `snow` 的纯 Rust 算法实现，不自创加密协议。配对包交换是最小实验流程，正式的双端确认界面属于后续产品工作。

## 先做不注入的连接检查

Mac 终端运行，`MAC_LAN_IP` 替换为 Mac 的实际局域网地址：

```sh
./target/release/crossflow environment
./target/release/crossflow receive --bind MAC_LAN_IP:24800 --identity .pairing/mac --log runs/mac-dry-01.jsonl --seconds 60
```

Windows 运行人工合成输入测试，此模式不会采集实际鼠标：

```powershell
./target/release/crossflow.exe send --host MAC_LAN_IP:24800 --identity .pairing/windows --log runs/windows-dry-01.jsonl --seconds 5 --synthetic
```

接收端默认只累加数据，**没有 `--inject` 就不向系统注入输入**。本机完整回环检查可以运行 `./scripts/smoke-loopback.ps1`。

## Windows 鼠标控制 Mac

### 可重复启动入口

构建并配对后，可用启动脚本自动生成本轮独立日志。先启动 Mac，再启动 Windows；相对路径均以项目根目录为准。

```sh
sh scripts/start-mac.sh --bind MAC_LAN_IP:24800 --identity .pairing/mac --seconds 180 --inject
```

```powershell
./scripts/start-windows.ps1 -MacHost MAC_LAN_IP -Identity .pairing/windows -Seconds 180
```

接收端脚本默认 **DrySink、不移动光标**，只有 `--inject` 才启用实际控制。Windows 默认采集真实鼠标，但仍需在控制窗口点击开始或按 Ctrl+Alt+空格。进行合成链路测试时，Mac 去掉 `--inject`，Windows 显式加 `-DryRunSynthetic`；不要混用合成发送与实际注入。

已有实验身份位于其他目录时，用 `-Identity` / `--identity` 指向该目录，脚本不会重新配对或覆盖身份。以下底层命令仍可用于手工指定日志。

Mac 本地终端先确认命令所属应用具备“辅助功能”和“输入监控”权限。程序只检查权限，不会自动授予；失败时按提示在系统设置中授权后重新启动。

```sh
./target/release/crossflow receive --bind MAC_LAN_IP:24800 --identity .pairing/mac --log runs/mac-input-01.jsonl --seconds 60 --inject --gain 1
```

Windows 运行：

```powershell
./target/release/crossflow.exe send --host MAC_LAN_IP:24800 --identity .pairing/windows --log runs/windows-input-01.jsonl --seconds 60 --max-hz 1000
```

- 发送端打开原生控制窗口，显示连接与共享状态。连接成功后仍是暂停状态，点击“开始共享”按钮或按 **Ctrl+Alt+空格** 开启；再次点击或按键可暂停。
- Windows 点击“停止并退出”、关闭控制窗口或按 **Ctrl+Alt+Esc** 结束发送；Mac 按 **Escape** 结束接收。两个命令默认最多运行 60 秒，Ctrl+C 也可退出。
- 重新连接必须再次手动开启，避免无意抢回控制。显示器变化、休眠、权限或注入错误会终止接收运行。
- 开启时 Mac 光标需位于主显示器；每次开启从当前位置建立基线。
- 上述默认模式不发送点击：Mac 光标移动时，Windows 本地鼠标仍可移动和点击，测试时注意自己正在操作哪一台设备。

### 实验键盘、点击、拖动与滚轮

仅在有人在场时开启系统注入。先以不带 `--inject` 的 Mac DrySink 验证完整输入；Windows 脚本增加 `-ExperimentalInputs`，仍需手动开始。实际控制需要 Mac 同时指定 `--inject --inject-actions`。两个开关默认关闭，现有快捷入口仍只共享移动。

```powershell
./scripts/start-windows.ps1 -MacHost MAC_LAN_IP -Identity .pairing/windows -Seconds 180 -ExperimentalInputs
```

```sh
sh scripts/start-mac.sh --bind MAC_LAN_IP:24800 --identity .pairing/mac --seconds 180 --inject --inject-actions
```

Windows 本地键盘和点击仍会作用于本地应用，当前不是无缝焦点接管。Mac 支持 ANSI 物理字母／数字／标点、基础导航、F1–F12 和左右修饰键；目标输入法与键盘布局决定字符。Ctrl 保持 Ctrl，GUI 键映射 Command；自定义 Ctrl/Cmd 映射、长按重复、Caps Lock、数字键盘、国际键、Fn/媒体键和双击次数尚不支持。接收端遇到不支持的键会停止并清理当前会话，不能假定所有 Windows 可采集按键都能在 Mac 使用。

按钮／键盘／滚轮不会跨越相邻移动被合并。暂停、Stop、EOF、过期输入、紧急停止及普通错误会尝试释放本会话持有的键与按钮；释放失败终止接收器并报告。自动化测试使用记录适配器，尚不能证明系统实际释放成功；强制杀死接收进程后的释放没有独立监督机制，仍是阻塞缺陷。滚轮暂用 120 原始单位换一行，水平符号、自然滚动、拖动与修饰键行为都待实机核验。

终端停止接入同一个退出流程：Windows 的 Ctrl+C / Ctrl+Break，以及 macOS 的 SIGINT / SIGTERM / SIGHUP 请求停止采集和网络循环，接收器清理拥有的输入后结束日志。信号回调只提交停止请求，实际释放在正常线程完成。已经进入系统适配器的调用不能中途抢占；连接／握手、域名解析与日志刷新也可能延后退出，不能承诺固定的毫秒级退出时间。关闭 Windows 控制台只提供有界的尽力清理窗口；任务管理器强制终止、SIGKILL、崩溃和关机不属于此保证。

`--max-hz` 是发送上限，**不是实际输入频率或流畅度保证**。原型不插入平滑缓冲；积压超过设定界限会停止该会话，保留错误证据。

## 记录和评估

```powershell
./target/release/crossflow.exe capture --log runs/local-capture-01.jsonl --seconds 10
./target/release/crossflow.exe summarize runs/windows-input-01.jsonl
./target/release/crossflow.exe summarize runs/mac-input-01.jsonl
```

日志文件名每轮必须不同，已有日志不会被覆盖。日志不含键盘字符、剪贴板或私钥。每份日志的 metadata 记录 Git 提交、工作区是否有修改、编译器和构建时间；源代码归档缺少 Git 信息时明确标为 unknown，不继承上级目录的仓库版本。

连接诊断分别记录 TCP 建立/接收、握手开始、本端握手完成及进入传输。每次尝试有本端编号和套接字地址；两端应通过反向匹配的地址关联，不能假定各自编号相同。`handshake_completed` 的 `local_only` 表示本端完成，不证明对端已收到响应。错误会保留具体读写阶段和底层原因；两端时钟未同步，跨机时间不能直接相减。

`sent` 是历史事件名，实际表示已加密并排入本地待写缓冲区；新增 `completion:queued_local` 明确这一边界。发送会话结束时的 `transport_summary` 区分排队与完整写入 socket 的数量，并保留未写完尾帧的序号和字节数。完整 socket 写入仍不是接收端确认。停止时直接丢弃待写尾帧，不为计数一致而继续发送旧输入；旧日志没有这一汇总时不能反推实际写入数。

摘要给出本机各段耗时的 P50、P95、P99 和最大值、样本量、错误与日志完整性。完整输入间隔与筛选后的短间隔分别报告；超过 50 ms 的间隔可能是空闲或停顿，不能仅凭日志判断，也不能自动删除后声称流畅。文件结尾损坏、时间倒退、缺少字段和日志写盘失败需保留为证据缺陷。软件时间戳不能替代外部录像里的可见光标更新。

在同一 Mac 显示器上分别对比本地鼠标、ShareMouse、Crossflow；保持 DPI、显示模式、网络和任务一致。按照 V0.2 计划每组预热 2 分钟、至少 5 轮，每轮 60 秒。没有可靠物理触发时间和录像误差分析时，不报告精确端到端延迟；两机单调时钟不能直接相减。

## 项目结构与状态

### 屏幕布局开发工具

`ui/index.html` 是离线交互草稿。正式界面以轻量、美观、响应快为目标，采用 macOS SwiftUI 与 Windows WinUI 3 方向，共享设计资源并调用同一 Rust 内核。Windows 已有可编译的原生布局客户端，见 [构建与功能说明](clients/windows/README.md)；Mac 原生客户端正在独立验证聊天中开发，尚未汇入主分支。HTML 草稿不作为正式客户端技术选型。

Windows 原生客户端构建：

```powershell
./scripts/build-windows-ui.ps1 -BootstrapSdk
```

该脚本在项目目录内准备 .NET SDK，调用 Rust 桥并构建 WinUI 3 应用，不安装全局开发工具。客户端通过 `crates/crossflow-bridge` 的 C ABI 复用布局校验和穿越计算；不会启动输入共享。示例需明确加载，不代表发现或连接的设备。共享会话控制、设备发现及实时连接状态尚未接入界面，导出布局不会应用到正在运行的会话。

原生构建和七项 C# → Rust 无窗口自检通过；窗口启动、视觉排版、实际拖动、文件对话框及闲置 CPU/内存尚未验证，Computer Use 应用访问授权等待超时。当前自包含构建约 170.54 MiB（含运行库），不是已验收的轻量发行包。

Rust 核心可验证同一份布局，并计算一次边缘穿越：

```powershell
./target/debug/crossflow.exe layout-check examples/layout-two-devices.json
./target/debug/crossflow.exe layout-trace examples/layout-two-devices.json --from windows-main --start-x 1910 --start-y 500 --end-x 1930 --end-y 500
```

示例数据不代表实机配置。布局计算已独立实现，自动发现显示器、跨屏切换控制权以及正式桌面客户端封装尚未接入。键盘、点击和滚轮使用独立显式开启的实验输入模式，尚未与布局编辑器连接。

- `src/`：共享核心和 CLI；`src/platform/`：平台输入适配。
- `docs/architecture.md`：统一核心、客户端一致性与平台边界。
- `docs/phase0-status.md`：实际验证记录和仍需完成的项目。
- `docs/reference/`：V0.2 需求和验收计划原文。
- `CONTRIBUTING.md`：分支、审查、测试和多人协作约定。

阶段 0 尚未完成。尤其是 Mac 实机光标效果、ShareMouse 对照与高刷新率验证，必须在目标机器上取得证据。
