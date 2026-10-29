# Windows / Mac 原生客户端首次配对

2026-10-29（UTC+08），Windows 包基于干净 `0a4bb44`，Mac 包基于干净
`dd0a9a3`；两提交之间仅有文档差异。本记录不代表阶段 0 整体验收通过。

## 已确认

- 用户通过两端真实界面手动完成配对，未输入 IP 或选择身份文件。
- Windows 原生设备页显示“这台 Mac”和“已保存配对信息”；打包核心的
  `peers` 返回一个有效记录、`invalid_records=0`。公开记录保存在
  `runs/windows-paired-peers-20261029.json`。
- Mac 验证聊天回报设备页显示选中的 `Crossflow Windows PC`，打包核心
  `peers` 返回一个有效记录、`invalid_records=0`；公开记录在 Mac 的
  `runs/macos-paired-peers-20261029.json`。配对后短码消失，配对辅助进程退出。
- 双端未自动开启共享。Windows 仍为“尚未连接”，Mac 接收按钮可用但未自动启动。
- 先前取消的邀请已清除短码并退出进程，没有保存设备；取消后仍显示“正在刷新”
  是一项待修的非阻塞文案问题。

## 实机发现的启动故障

随后通过 Mac UI 点击“接收鼠标移动”（实验键盘/点击/滚轮关闭）。Mac 日志
报告应用宿主上下文的 Accessibility、listen_event、post_event 都为 false，
接收器在监听前拒绝启动。此前 Codex 命令行宿主预检为 true，不能外推为
Crossflow App 已获授权。

Mac 原始日志：`~/Library/Application Support/Crossflow/runs/receiver-62F90E3D-D535-467C-AD67-82E4FDB7E7A8.jsonl`。
远端验证者报告仅有 metadata/error/log_summary，零日志丢失，没有监听或输入；
进程已退出。Windows 未连接或启用共享。

旧界面把所有 receiver 非零退出都标为“停止结果未确认”，导致重试和正常
退出被阻止。修复使用专门的 `startup_rejected` 终态，仅涵盖输入拥有者创建前的
权限拒绝；不能把普通运行错误或强制退出当作清理成功。修复后仍需原生测试和
实际 App 权限验收，系统授权由用户亲自完成。

## 未验证

已配对身份的实际输入连接、暂停/启动/停止、断线和 Escape、物理光标运动及
本地 / ShareMouse / Crossflow 对照仍待执行。Mac 内建屏系统报告 60 Hz，
不是刷新率测量，也不覆盖外接高刷新率显示器。Windows 自动截图与点击工具在
本轮报 GPU/geometry 错误，用户完成手动操作，文字控件读取与公开存储核对成功；
不把此工具故障认定为产品渲染故障。
