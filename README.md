# Crossflow

跨设备键鼠共享系统。各端使用同一个 **Rust 核心**，平台适配层负责系统输入接口；客户端将使用统一设计语言与共享组件。当前是阶段 0 的 **Windows → Mac 鼠标移动实验**，不是完整 KVM 产品。

## 当前可以做什么

- Windows Raw Input 采集相对鼠标移动；全局快捷键明确开启和停止共享。
- 同一套 Rust 协议、身份认证、加密传输、控制会话、日志代码运行于 Windows 和 macOS。
- Mac 使用 CoreGraphics 注入光标移动，并记录接收和注入调用耗时。
- 显式离线配对、双方固定公钥认证、每次连接建立新的加密会话。
- 有界移动合并、短期接收许可、断线停止、重新连接后保持暂停。
- 不操作系统光标的加密回环测试、采集日志和统计摘要。

当前**只发送鼠标移动**。不发送按钮、滚轮、键盘或剪贴板；不抑制 Windows 本地光标；Mac 只使用主显示器，按线性增益映射 Raw Input 位移，尚未复现 macOS 原生加速曲线。不能据此宣称完成双向控制、游戏保护、多屏穿越或 ShareMouse 替代。

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
- 本版不发送点击：Mac 光标移动时，Windows 本地鼠标仍可移动和点击，测试时注意自己正在操作哪一台设备。

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

`ui/index.html` 是两端共用的离线布局编辑器，可拖动、调整屏幕属性并导入／导出布局 JSON。界面里的设备是布局草稿，不代表已发现或已连接的真实设备；导出不会改变系统显示排列或开启共享。

Rust 核心可验证同一份布局，并计算一次边缘穿越：

```powershell
./target/debug/crossflow.exe layout-check examples/layout-two-devices.json
./target/debug/crossflow.exe layout-trace examples/layout-two-devices.json --from windows-main --start-x 1910 --start-y 500 --end-x 1930 --end-y 500
```

示例数据不代表实机配置。布局计算已独立实现，自动发现显示器、跨屏切换控制权以及正式桌面客户端封装尚未接入。键盘、点击和滚轮已有共享输入队列开发，但现有网络程序仍只转发鼠标移动。

- `src/`：共享核心和 CLI；`src/platform/`：平台输入适配。
- `docs/architecture.md`：统一核心、客户端一致性与平台边界。
- `docs/phase0-status.md`：实际验证记录和仍需完成的项目。
- `docs/reference/`：V0.2 需求和验收计划原文。
- `CONTRIBUTING.md`：分支、审查、测试和多人协作约定。

阶段 0 尚未完成。尤其是 Mac 实机光标效果、ShareMouse 对照与高刷新率验证，必须在目标机器上取得证据。
