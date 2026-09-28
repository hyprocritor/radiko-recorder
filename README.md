# Radiko Recorder

Rust 终端程序：按 station ID 获取节目表，预约直播录音，同时支持直播试听。界面使用中文，节目保留日文原文。

## 启动

### Windows 发布包（无需 Rust）

从 [GitHub Releases](https://github.com/hyprocritor/radiko-recorder/releases/latest) 下载 `radiko-recorder-v版本-windows-x86_64.zip`，完整解压后在该目录运行：

```powershell
# 首次运行：安装外部 FFmpeg；已安装时可跳过
powershell -ExecutionPolicy Bypass -File .\scripts\setup-ffmpeg.ps1
.\radiko-recorder.exe JORF
```

也可双击 `radiko-recorder.exe` 后输入电台 ID。保留同目录的 `radiko_recorder.pdb` 以便崩溃诊断。发布包不包含 FFmpeg，可用 `--ffmpeg C:\ffmpeg\bin\ffmpeg.exe` 指定已有安装；缺少 FFmpeg 时仍能浏览节目表。可用 `Get-FileHash .\radiko-recorder-v版本-windows-x86_64.zip -Algorithm SHA256` 与发布页的 `SHA256SUMS.txt` 核对下载文件。

升级前请先正常退出旧程序。预约数据兼容旧版并继续使用原数据目录；若要沿用录音目录，请通过 `--output-dir` 显式指定。不要在录制中覆盖程序文件。

### 从源码构建

需要 Rust 1.88+、交互式终端和 FFmpeg。建议使用 Windows Terminal；Linux 编译试听功能还需要 ALSA 开发库（Debian/Ubuntu：`libasound2-dev`、`pkg-config`）。

```powershell
# Windows：下载到本项目 tools/，校验发行方 SHA256，不修改系统 PATH
.\scripts\setup-ffmpeg.ps1

# 在项目目录运行。若 PATH 找不到 FFmpeg，会自动查找 tools/bin/ffmpeg.exe。
cargo run --locked -- JORF

# 不传电台 ID 时在界面输入
cargo run --locked

# 自定义目录和可执行文件
cargo run --locked -- RN2 --output-dir D:\Radio --data-dir D:\Radio\state --ffmpeg C:\ffmpeg\bin\ffmpeg.exe

# 构建发布版
cargo build --locked --release
.\target\release\radiko-recorder.exe JORF
```

FFmpeg Windows 包来自 [FFmpeg 官方下载页](https://ffmpeg.org/download.html)列出的 [gyan.dev](https://www.gyan.dev/ffmpeg/builds/)。外部程序的许可证位于 `tools/ffmpeg/` 解压目录中。也可自行安装 FFmpeg，并使用 `--ffmpeg` 指定路径。

每次运行显示和执行一个电台的预约。同一数据目录只允许一个实例；其他电台的预约仍会保留，需用对应 station ID 启动才能执行。需要同时管理不同电台时，使用不同 `--data-dir` 启动多个实例。

## 操作

| 按键 | 操作 |
| --- | --- |
| `↑/↓`、`j/k` | 移动选择；详情面板中滚动 |
| `←/→` | 切换节目日期 |
| `Tab` / `Shift+Tab` | 切换节目表、详情、预约面板 |
| `PageUp/PageDown` | 翻页或滚动详情 |
| `Enter` | 新建预约；预约面板中编辑尚未准备录制的任务 |
| `r` | 刷新节目表（也会每 30 分钟自动刷新） |
| `s` | 无待录任务、录音及试听时重新输入电台 ID |
| `x` | 取消预约列表选中的任务，保存已录音频 |
| `p` | 开关当前电台直播试听 |
| `e` | 查看当前选中预约的错误时间段与补片结果 |
| `E`（Shift+e） | 查看本次试听的错误时间段与重连结果 |
| `+/-`、`m` | 音量、静音；不影响录音音量或文件 |
| `q`、`Ctrl+C` | 退出；存在待录或录制任务时确认 |

预约编辑中，`Tab` / `↑↓` 切换字段，`Ctrl+U` 清空当前字段，`Backspace` 删除末尾字符；`Ctrl+S` 保存，`Enter` 下一项/保存，`Esc` 返回。错误记录中用 `↑↓` / `PageUp` / `PageDown` 滚动、`Esc` 关闭。时间格式是 `YYYY-MM-DD HH:MM:SS`，**全部使用 JST（UTC+9）**。日期也必须填写，因而跨午夜不会混淆。

## 录制与恢复

- 默认采用节目起止时间，提前 15 秒、延后 60 秒，可分别调整为 0–3600 秒。详情同时显示电脑本地时间。
- 选择正在播出的节目会立即录制剩余内容；不会补下载已播出的内容。过期节目不显示为可选节目。
- 可以预约多个节目，包括时间重叠的任务。每个任务独立录制，试听也独立运行。试听遇到 404、连接中断或 20 秒没有音频会自动获取新会话、退避重试，保留音量和静音设置；`p` 可随时停止重试。明确的地区权限拒绝或音频设备故障会停止试听并显示原因。
- 节目表更新只标注节目变更，不自动更改已经确认的录制时间。暂不提供每周重复预约。
- **必须保持程序运行，电脑不能休眠**。关闭程序后不运行后台服务。恢复运行时，在窗口内的任务继续录音并标记缺失；已经结束的预约标记错过，未封装的有效片段会尝试恢复为 M4A。
- 录制停止由实际截止时间控制。FFmpeg 正常停止最多等待 5 秒，随后封装本地文件；此步骤可能使界面短暂显示“封装中”。
- 原始 AAC 直接复制到 M4A，无重新编码。网络延迟和 HLS 分片会影响音频边界，前后缓冲不保证精确剪辑到节目开头。
- 文件名包含电台、JST 开始时间、节目名称和任务标识；不会覆盖已有文件。恢复任务生成独立的“恢复”文件。
- 录音中间文件保存在输出目录的 `.parts/<任务 ID>/`。新版保存原始 AAC/TS 分片和 `segments.json` 时间索引，结束时用 FFmpeg stream copy 封装为 M4A。包括成功录制的中间文件也保留，便于恢复，因此会额外占用磁盘；确认 M4A 正常且任务已结束后，可自行删除对应任务的中间目录。
- 数据目录保存 `jobs.json`、`recorder.log`、`crash.log` 和实例锁。Windows 默认位于 `%LOCALAPPDATA%\radiko-recorder\data`；具体位置由 `directories` 根据平台确定。可用 `--data-dir` 明确指定。

### 短暂断网与分片补取

- 对 HAR 中带 `EXT-X-PROGRAM-DATE-TIME` 的普通直播清单，按分片时间建立索引。分片下载失败后保留原地址在内存中，以 1、2、4、8、16、30 秒退避重试，同时从刷新后的清单更新分片地址。并发下载最多 4 个，为新分片和补片都保留机会，失败地址最多保留 256 个。
- 网络恢复后，获取仍在直播窗口内的分片、重试缓存地址。已补回分片按时间排序、去重，再与其余分片一起封装；完整下载前不会登记成功。索引原子保存，重启可识别已经保存的分片，索引及预约文件均不保存流 URL 或 token。
- `e` 显示失败的起止时间、原因、重试次数，以及“尝试恢复 / 已恢复 / 未补回 / 可能缺口”。分片时间来自清单；网络连接错误及试听中断显示本机估计时间。报告区分节目时段与节目外的前后缓冲。补齐已发现的缺口后清除缺失标记，保留恢复记录；`↺` 表示已恢复记录，`⚠` 表示仍有待处理或未补回记录。
- 补片限于录制窗口内，取消或退出会停止网络请求并保存已录内容。没有在任何清单中见到地址、且恢复后的清单也不再包含的片段，无法仅靠直播接口定位；服务器已删除或持续拒绝的分片会标为未补回。不会使用会员权限或回听下载，也不保证超出直播缓存窗口的内容可补回。
- “分片连续”指已观察到的清单时间范围内连续；分片下载/音频格式完整性检查不能代替逐样本的听感判断。仍采用实际墙钟截止时间停止，不为等待未来分片而延后结束。
- 缺少分片绝对时间、使用加密/其他封装的清单，以及旧版留下的连续 TS 录制，使用 FFmpeg 兼容路径。它可自动重连并记录估计时间段，无法精确定位、去重补回旧版缺口。
- 提前 30 秒仅准备认证，录制开始时才建立直播会话，避免提前创建的短期流地址在真正使用前失效。Windows 原子保存遇到暂时的文件占用会短暂重试。

分片时间及跨清单定位遵循 [HLS RFC 8216](https://www.rfc-editor.org/rfc/rfc8216.html#section-6.3.3)；跨会话使用时间范围定位，不假设不同清单的序号指向相同音频。

## 日志与异常退出排查

双击 EXE 启动也会自动写入日志，无需设置 `RUST_BACKTRACE`：

- **`recorder.log`**：启动版本、进程/运行标识、节目表请求、录制状态、FFmpeg 错误输出、重连原因、试听错误以及退出原因。每条日志同步写入文件，不依赖后台日志队列。运行事件时间使用 UTC（`Z`）。
- **`crash.log`**：普通致命错误的完整错误链和报告时堆栈，以及所有 Rust 线程（包括 Tokio 后台任务、音频线程）panic 时的消息、源码位置、原始堆栈。报告时间含电脑本地时区偏移。写入后立即请求操作系统同步文件。
- 界面/预约保存出错时，**先记录最初错误，再停止录音**；清理或最后保存时的错误也会单独记录。后台录制任务 panic 会显示失败并保留 `.parts` 内的录音片段；试听或节目表任务 panic 会解除等待状态并显示原因。
- 日志追加保存，不会因重启覆盖；用 `pid` 和 `run` 区分各次运行。关闭程序后可以归档或删除旧日志。日志会隐藏网络 URL、认证 token 与会话参数，错误链和堆栈不截断。
- 若数据目录不可写，会尝试 `%TEMP%\radiko-recorder-logs`（其他平台使用系统临时目录）。两处都不可写时只能输出到标准错误。
- 发布版保留堆栈调试信息。Windows 分享/移动 EXE 时一并保留同目录的 `radiko_recorder.pdb`，以便堆栈显示函数名和源码行。

可在资源管理器地址栏输入 `%LOCALAPPDATA%\radiko-recorder\data` 查看。发生异常后提供 `crash.log` 末尾的一份完整报告及同一时间段的 `recorder.log` 即可。

进程被任务管理器强制结束、断电、内存耗尽、原生代码访问违规或磁盘故障时，Rust panic 钩子可能无法执行或日志无法写入，不能保证记录最后一条原因；这些情况需结合 Windows 事件查看器/系统崩溃转储排查。新日志机制无法补回升级前未被记录的错误。

## 网络与认证

支持当前网络地区有权限收听的免费直播；不实现会员登录、跨区权限或回听下载。认证与录制使用当前网络，不自动选择代理。若网络限制地区，界面会显示认证/播放失败原因。

节目表使用 `api.radiko.jp/program/v3/weekly/{station}.xml`。认证执行 `auth1 → ASCII 密钥切片/Base64 → auth2`，应用参数为 `pc_html5 / 0.0.1 / dummy_user / pc`。公开应用密钥优先从官方 `playerCommon.js` 提取。

认证缓存保留在内存，60 分钟后不复用；这只是刷新策略，并非已确认的服务端 token 有效期。播放被拒绝时重新认证，直播入口故障时尝试备用入口。短暂故障按 1、2、4、8、16、30 秒退避，超过录制窗口停止。

实际媒体清单 URL 包含短期会话参数，仅在内存中用于请求或传给 FFmpeg 进程，不写入配置和日志。日志隐藏 token、会话参数及网络 URL。HAR 仅用于分析协议和提取脱敏测试样本，不需要放进项目，运行时也不读取 HAR。

已处理 Radiko 媒体清单对 HTTP `Range` 头的特殊行为：FFmpeg 必须对首次读取使用 `-seekable 0`，对 HLS 内部请求使用 `-http_seekable 0`，否则服务器可能返回 HTTP 200 和非 HLS 错误内容。

## 验证

```powershell
cargo fmt --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked

# 约 30 秒；只访问本机合成 HLS，不连接 Radiko
$env:RADIKO_TEST_FFMPEG = (Resolve-Path .\tools\bin\ffmpeg.exe).Path
cargo test --locked --test media_integration -- --ignored --nocapture

# 分片补取、永久 404、重启索引恢复；还包括静音的试听重连（需音频输出设备）
cargo test --locked --test hls_recovery -- --ignored --nocapture

# 显式进行真实网络验证，会生成 target/live-check 下的短录音
cargo run --locked --example live_check -- JORF

# 验证真实试听解码、输出设备与退出，保持静音
cargo run --locked --example preview_check -- JORF

# 如遇网络问题：输出经过脱敏的接口/FFmpeg 诊断
cargo run --locked --example network_check -- JORF
```

本地测试覆盖节目表时间和 HTML、认证失败重试、流入口回退、预约重叠与重启恢复、原子保存和实例锁、中文/日文界面及小窗口、本地 HLS 录制与重连、取消/退出、恢复文件不覆盖、损坏片段封装失败及产物解码。FFmpeg 集成测试因依赖外部工具默认忽略，必须显式运行上述命令。

### 本次验证记录

在 Windows、Rust 1.94.1、FFmpeg 9.0.2 环境完成：

- `fmt --check`、`check`、`clippy --all-targets -- -D warnings`、发布版构建均通过。
- 10 项单元测试、2 项本地 HTTP 协议测试、1 项 FFmpeg 综合集成测试通过。
- 日志升级后新增 9 项回归测试：普通错误、panic/异步任务/音频线程堆栈、立即退出前写盘、脱敏、启动/实例锁失败、备用日志目录、重启追加保存。日志升级后重新运行了上述 FFmpeg 集成测试。
- 分片恢复升级新增本地 HLS 验证：短暂断网、分片 404/损坏后重试、永久丢片时间段、去重及顺序、重启索引恢复、M4A 可解码，以及试听 404 后自动重连、保持静音和可取消。该升级未重新进行真实 Radiko 网络录制验证。
- JORF 实际认证、清单持续刷新与 25 秒录制窗口完成，输出 AAC / 48 kHz / 双声道 M4A；HLS 初始缓冲使音频时长可长于墙钟录制窗口。
- 真实直播试听链路在静音下运行 8 秒，音频设备初始化、音量控制与进程停止通过；未进行主观听感评价。
- 在交互式终端操作了节目选择、预约编辑、原子保存、退出确认和终端恢复。

其他操作系统尚未实机验证。
