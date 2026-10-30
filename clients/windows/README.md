# Crossflow Windows 原生客户端

使用 C# + WinUI 3 原生控件；无 HTML、WebView 页面或另一个输入内核。布局校验和穿越计算调用统一 Rust 核心。原生窗口管理同包 Rust sender 的连接、开始、暂停与停止；打开窗口保持空闲，不自动启动共享。

## 构建

在仓库根目录执行：

```powershell
./scripts/build-windows-ui.ps1 -BootstrapSdk
```

脚本只将官方 .NET SDK 10.0.403 解压到本仓库 `.tools/dotnet`，核验微软发布的 SHA512；NuGet 缓存位于 `.tools/nuget`，不会安装全局 SDK、VS、Windows SDK 或管理员组件。Rust 构建优先使用仓库现有的 `scripts/cargo-local.ps1`（便携 GNU 工具链），不存在时才使用系统 Cargo。Cargo 使用 `--locked`，NuGet 默认锁定恢复。首次恢复需要网络；随后可加 `-NoRestore`。`-SkipBridge` 仅用于复用已编译的 Rust DLL。

直接依赖固定为 Microsoft.WindowsAppSDK.WinUI 2.3.6、Microsoft.WindowsAppSDK.InteractiveExperiences 2.1.6、Microsoft.Windows.SDK.BuildTools 10.0.28000.2705；Foundation 2.3.9 与 Base 2.0.4 由官方依赖关系引入。恢复锁文件随源码保存。使用微软支持的[组件式 SDK](https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/) 替代完整元包，避免附带未使用的 AI、ML、Search 与 Widgets。依赖图仍包含 WinUI 自带的 WebView2 支持库，但此应用不创建 WebView 控件。构建不手工删除运行时 DLL。

SDK 来源与哈希见构建脚本；微软发布元数据：<https://builds.dotnet.microsoft.com/dotnet/release-metadata/10.0/releases.json>。部署方式参考 [Microsoft 官方自包含部署文档](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/self-contained-deploy/deploy-self-contained-apps)。依赖升级时须显式运行 `dotnet restore ... -p:RestoreLockedMode=false --force-evaluate` 更新锁文件并审核变更；普通构建不会静默改锁。

输出目录：`Crossflow.Windows/bin/x64/Release/net10.0-windows10.0.19041.0/win-x64/`。运行该目录的 `Crossflow.Windows.exe`，移动应用时保留整个输出目录。构建复制 `crossflow.exe`、`crossflow_bridge.dll`、共享 `clients/design-tokens.json` 和 `examples/layout-four-screens.json`。仅从应用所在目录加载 Rust DLL 和启动 sender。构建脚本始终构建 Rust CLI；`-SkipBridge` 仅跳过 bridge 构建。

配对发起端使用同包 `crossflow-enroll.exe`，复用共享 Rust 身份和设备存储。首次打包此组件时，先按 `experiments/pairing-spake2/source-lock.json` 在本机构建 BoringSSL，再执行：

```powershell
./scripts/build-windows-ui.ps1 -WithPairing -PairingLibraryDir 'C:/absolute/path/to/boringssl'
```

`-PairingLibraryDir` 也可通过 `CROSSFLOW_BORINGSSL_LIB_DIR` 指定。脚本不会下载原生库，当前只支持已经验证的仓库 LLVM-MinGW 工具链；会同时复制 `libc++.dll`、`libunwind.dll`、`libwinpthread-1.dll`。单独重建组件可运行 `scripts/build-pairing-worker.ps1 -LibraryDir ... -Offline`，之后重建 WinUI 将其打包。已有本地组件也会随普通 WinUI 构建复制；发布前应以 `-WithPairing` 从当前源码重建。缺少任一配对组件时，界面明确提示且不能发起配对，已保存设备仍可连接。

## 本轮功能

- 原生排列画布、屏幕拖动与边缘吸附；方向键移动 10 个布局单位，Shift + 方向键移动 50。
- A「静白」主题，工作台与设备两页；按实际设备分组展示多屏，支持位置编辑，分辨率来自真实屏幕。
- 实际排列编辑、撤销、重做经 Rust 校验和真实屏幕绑定核对后，自动保存到对应已配对设备的预设；保存失败保留上次排列，明确显示错误。编辑不启动共享。
- 初始空态引导选择已配对设备、读取两端屏幕；产品界面不提供虚构屏幕、示例、草稿文件或模拟穿越。
- 统一主题、浅色、高 DPI、原生键盘焦点、关键控件 AutomationId。
- 示例文件与布局 ABI 仅保留供开发测试，不参与客户端设备列表。
- 设备页显示已配对设备，点选读取真实屏幕后使用应用保存的信息连接；每轮最多一小时。
- 附近设备列表浏览独立配对邀请服务；输入 8 位短码后，显示经认证的设备名称和公钥，再明确确认配对。成功后自动保存到当前用户 DPAPI 设备存储并刷新列表，不自动开启共享。配对码仅走私有进程管道，不进入命令行或日志。
- 会话状态直接来自 Rust JSONL protocol 2；严格读取八个字段，除了连接状态还区分 route_enabled、focus_device_id 与 focus_screen_id。共享已开启与当前键鼠焦点分别显示，编辑器选中屏幕不等于实际焦点。仅 paused + capture_ready 可开始，并使用当前 epoch。重连不会自动恢复共享。
- 关闭窗口先异步停止并回收子进程，不显示第二个诊断窗口。状态超过两秒未更新、协议错误或异常退出会停止连接。

跨屏入口位于“设备 → 已配对设备 → 排列屏幕”。它通过同包核心 `displays` 读取 Windows 物理显示器，通过 `peer-displays --peer` 从已认证的接收端读取显示器；读取期间不启动输入。Mac 必须开启接收，两端都须更新到支持此接口的版本。

每台设备的多块屏幕分别显示，保留同设备原生屏幕之间的相对偏移，首次把远端屏幕组放在本机最右侧屏幕的右边，并使接边的屏幕垂直居中。可拖动屏幕调整排列，然后点击“开启跨屏共享”。界面写入独立、不随编辑变化的运行时布局快照，并向 `desktop-send` 传递 `--runtime-layout`；这一次明确点击授权首次就绪时发送一次绑定当前 epoch 的开启命令，无需第二次点击。重复状态、操作拒绝、断线重连、停止或关闭都不会重复自动开启。运行中排列锁定，点击“停止并调整排列”正常停止连接后即可拖动。仅在相邻屏幕重叠的边缘区域穿越，预设不会因默认放置规则改变而被重新排列。运行时绑定包含原生显示器 ID 与坐标，恢复预设前重新读取并对照实际屏幕；更换显示器、分辨率或配对对象后不会直接复用旧映射。核心在开启共享前仍须再次核验屏幕状态，UI 读取不是永久有效的拓扑保证。

仅鼠标模式、手动地址与身份文件诊断保留在开发 CLI，不放入正常客户端导航。新跨屏入口的构建和无窗口测试不等于实际边缘切换、键盘或本地输入抑制验收，须另行进行双机实测。

Windows 配对发起端和 Mac 邀请界面、Keychain 保存、接收会话托管均已接入。Mac 原生构建及隔离测试已通过，但两台正式客户端的首次配对仍待用户设备实测。此前 Windows 界面验证使用真实本机网络对端 fixture；fixture 的 Mac 角色使用 Windows DPAPI，不是 Mac 实机。配对服务 `_crossflow-pair._tcp.local.` 和输入服务 `_crossflow._tcp.local.` 分开，只有受认证通道提供的输入端口会保存。当前发起端选用首个有效 LAN 候选地址，初次配对的多地址失败切换尚未实现。

验证或等待确认期间可以取消；窗口、页面和对话框关闭均请求取消并等待子进程回收。若已经确认且保存恰好完成，取消或通信失败不能保证撤销这次保存；界面提示刷新已配对列表检查状态。两端持久化不是原子事务，尚需恢复和解除配对流程。

修改提交在后台调用 Rust，通过校验和配对预设保存后才更新当前布局。任务期间禁止其他布局操作。无效修改或保存失败保留上一份有效布局并显示错误；读取另一台设备或更换显示器时清空撤销历史，避免恢复旧绑定。

## 无窗口自检

```powershell
$exe = './clients/windows/Crossflow.Windows/bin/x64/Release/net10.0-windows10.0.19041.0/win-x64/Crossflow.Windows.exe'
$output = Join-Path (Get-Location) 'runs/windows-ui-self-test.json'
$check = Start-Process -FilePath $exe -ArgumentList @('--self-test', ('"{0}"' -f $output)) -WindowStyle Hidden -Wait -PassThru
Get-Content $output
if ($check.ExitCode -ne 0) { throw 'Crossflow native client self-test failed.' }
```

此模式不创建窗口、不采集/注入输入，八项检查验证真实 C# → Rust 调用、共享主题、多设备多屏布局、重叠拒绝、未知字段拒绝、普通与大坐标跨屏计算。进程退出码非零表示失败。

另可使用 `--session-self-test <输出文件>` 运行假子进程协议测试，覆盖就绪/epoch门禁、stderr持续读取、异常EOF、错误版本、状态失联与强制回收；它只启动客户端自身的显式 fixture 模式，不启动 Rust 捕获。两种自检均不替代原生窗口视觉、交互与实机鼠标验收。

`--runtime-layout-self-test <输出文件>` 检查多显示器负坐标与组内偏移、原生绑定保留、热插拔/分辨率变更使预设失效、严格屏幕 JSON 解析，以及真实进程启动参数中的 `--runtime-layout` 独立参数和清理。它不读取系统屏幕、不连接远端，也不采集或注入键鼠。

`--pairing-self-test <输出文件>` 验证配对控制器拒绝提前成功、错误设备 ID、重复/未知字段、危险名称、过量输出、非零退出，以及取消后的实际子进程回收。它使用假子进程，不进行网络认证或写入身份；真实 Rust worker 测试见独立配对 crate 的 `tests/worker_enrollment.rs`。
