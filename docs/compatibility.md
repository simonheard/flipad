# 兼容性与验证情况

## 已测试的设备和系统

### 掌机

| 项目 | 版本 / 型号 |
|---|---|
| 设备 | Retroid Pocket Flip 2（骁龙 865 / SM8250，Adreno 650，1920×1080 AMOLED） |
| 系统 | Pocknix（基于 Arch Linux ARM），内核 7.2.0，glibc 2.43 |
| 前端 | 原生 ARM64 Steam，运行在 gamescope 中（游戏在 Xwayland `:1`） |
| 输入 | InputPlumber 0.75.2（手柄由 `retroid-pocket-gamepad` 驱动提供） |
| 蓝牙 / Wi-Fi | QCA6390（蓝牙 5.2，支持 LE 广播；Wi-Fi 6） |
| 音频 | PipeWire 1.6.8，WirePlumber 0.5.15；独立的耳机 / 扬声器 sink，耳机孔有插拔检测 |
| 其他 | BlueZ 5.87，python-dbus、python-evdev（只用于工具脚本） |

### 电脑

| 项目 | 版本 / 型号 |
|---|---|
| 系统 | Bazzite（Fedora 44），内核 7.2.9，KDE Plasma 6.7.5（Wayland） |
| 音频 | PipeWire 1.6.9 |
| 蓝牙 | Intel AX211（btusb），BlueZ 5.87 |
| 其他 | NVIDIA RTX 3060（GPU 状态通过 `nvidia-smi` 读取）、Intel CPU（`coretemp`） |

## 对其他设备的要求

**客户端（掌机）**：
- Linux aarch64，glibc ≥ 2.43（或者用与目标系统匹配的 sysroot 重新编译）。
- 用 InputPlumber 管理手柄，并且支持 `gamepad` 输出目标（客户端靠它借用按键）。不是 InputPlumber 的系统需要改 `client/src/plumber.rs`。
- 有 X11 / Xwayland 显示环境，例如 gamescope。
- 音频：PipeWire（需要 `pw-metadata`、`pactl`）。耳机检测依赖名为 `RetroidPocket Headset Jack` 的输入设备，其他设备需要改 `client/src/audio.rs`。
- 蓝牙模式：BlueZ 支持 LE 外设（`LEAdvertisingManager1`、`GattManager1`），并在 `main.conf` 里设置 `DeviceID`。

**主机（电脑）**：
- Linux x86_64，PipeWire（RTP 模块）。
- 用户能访问 `/dev/uinput`（Bazzite / SteamOS 默认可以）。
- 防火墙放行 UDP 47820、47830、47831。
- 蓝牙模式本身不需要主机端服务；装了 `flipad-host` 的 Linux 电脑会额外自动连接蓝牙音频。

## 功能验证情况

“自动”表示通过脚本在真机上端到端测试过（例如注入事件、读取电脑上的设备和事件）；“手动”表示在掌机上亲手操作过。

| 功能 | 验证方式 | 结果 |
|---|---|---|
| 从 Steam 启动 / 停止，按键接管与恢复 | 自动 | ✅ 停止后 0.5 秒内恢复 |
| 崩溃恢复（kill -9） | 自动 | ✅ 守护进程 1.5 秒内恢复按键和音频设置 |
| 待机时退出 | 手动 | ✅ 唤醒后完成恢复；主机通过超时发现（待机断网，来不及发告别消息） |
| Wi-Fi 手柄 → 电脑虚拟 Xbox 360 手柄 | 自动 + 手动 | ✅ 往返延迟约 2.2 ms（局域网 Wi-Fi 6） |
| 震动回传（Wi-Fi / 蓝牙） | 自动 | ✅ 指令到达掌机马达驱动 |
| 主机快捷操作（音量、去重） | 自动 | ✅ |
| 触控板鼠标（Wi-Fi） | 自动 | ✅ 光标移动；手势需在实机上体验 |
| Wi-Fi 音频（耳机）与麦克风 | 自动 | ✅ 512 帧周期下无欠载；音质需人耳评估 |
| 默认音频设备的切换与恢复 | 自动 | ✅ |
| 游戏插件面板与图像通道 | 自动（演示插件） | ✅ 400×300 JPEG 到达并解码；「当前游戏」插件还没在真实游戏上验证 |
| 蓝牙配对，被识别为 Xbox 手柄 | 自动 | ✅ Linux 加载 `hid-microsoft`；Windows / macOS 未测试 |
| 蓝牙鼠标、媒体键 | 自动 | ✅ 收到移动和按键事件 |
| 蓝牙音频（A2DP） | 自动 | ✅ aptX HD，按设置输出到扬声器或耳机 |
| 多台蓝牙主机切换 | 自动（一台真实主机 + 一个虚构地址） | ✅ 拦截与解除生效；未用第二台真实设备测试 |
| 蓝牙配对窗口（平时拒绝新配对，开窗 60 秒）、忘记设备 | 自动（虚拟触摸屏点击真实界面 + 电脑配对） | ✅ 窗口外配对被拒；窗口内配对成功后自动关闭并信任该主机 |
| Wi-Fi ↔ 蓝牙热切换 | 自动 | ✅ |
| 触控面板的点击与手势 | 自动（虚拟触摸屏注入）+ 手动 | ✅ 直接读触摸屏：单指移动、双指滚动、拖动后按钮仍可点 |

## 已知限制

- gamescope 把触摸转成单个模拟鼠标，没有多点信息，双指操作后还可能卡在“按下”状态。所以客户端直接读取触摸屏（`/dev/input` 中带 `INPUT_PROP_DIRECT` 的多点设备，deck 用户可读、未被独占）。每次启动后，第一次触摸照常走模拟鼠标，同时用它的位置校准屏幕方向；之后每次点击都会复核。
- Steam 默认让非 Steam 游戏以 1280×720 渲染再放大。部署工具会把 Flip Pad 的分辨率设成“原生”（`SetAppResolutionOverride(…, "Native")`）。

- 蓝牙模式没有耳机麦克风：HFP 依赖内核的 RFCOMM，而 Pocknix 内核没有开启 `CONFIG_BT_RFCOMM`（BlueZ 报 `RFCOMM server failed for Hands-Free unit: Protocol not supported`）。需要麦克风请用 Wi-Fi 模式。如果你的系统内核支持 RFCOMM，在 WirePlumber 里开启 `hfp_hf` 角色即可；电脑在有程序录音时会自动切到 HFP。
- 蓝牙模式没有主机状态和插件面板。
- 蓝牙音频延迟明显高于 Wi-Fi 模式，玩游戏建议用 Wi-Fi。
- Linux 电脑不会自己打开蓝牙音频连接，装了 `flipad-host` 才会自动连接；其他系统需要在声音设置里手动选择「Flip Pad」。
- 关闭掌机的 Wi-Fi 省电模式需要一条 sudo 规则（允许 `iw dev wlan0 set power_save on|off`）；没有这条规则时会跳过，偶尔会有延迟尖峰。
- Pocknix 会把 PipeWire 周期锁在 1024 帧。客户端传音频期间把它临时改成 512，结束后改回。
