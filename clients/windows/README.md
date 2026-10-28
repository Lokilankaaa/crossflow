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

## 本轮功能

- 原生排列画布、屏幕拖动与边缘吸附；方向键移动 10 个布局单位，Shift + 方向键移动 50。
- A「静白」主题，设备分组颜色、每组屏幕数与位置/尺寸属性；同设备可添加多块独立屏幕。
- 原生文件选择器导入/导出布局；导出前通过 Rust 再次校验。无自动保存或自动覆盖文件。
- 从所选屏幕中心向四个边缘发起纯布局穿越预览，不移动系统光标。
- 统一主题、浅色、高 DPI、原生键盘焦点、关键控件 AutomationId。
- 示例通过“显示示例”明确载入，不表示已发现或已连接的设备。
- 设备页指定 IP:端口与已有身份文件夹；身份文件仅由 Rust 读取。每轮最多一小时。
- 会话状态直接来自 Rust JSONL；仅 paused + capture_ready 可开始，并使用当前 epoch。重连不会自动恢复共享。
- 关闭窗口先异步停止并回收子进程，不显示第二个诊断窗口。状态超过两秒未更新、协议错误或异常退出会停止连接。

当前这个会话入口仅转发鼠标移动，尚未接入键盘、点击、滚轮或布局自动跨屏。布局仍可独立编辑与导出，不能视为已经应用到运行中的共享。

修改提交在后台调用 Rust，仅通过校验才更新当前布局。任务期间禁止其他布局操作；异步导入结果与草稿版本不匹配时丢弃，避免旧结果覆盖新状态。无效修改保留上一份有效布局并显示错误。

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
