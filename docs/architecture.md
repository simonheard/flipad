# 架构说明

## 按键接管（client/src/plumber.rs）

掌机的手柄由 InputPlumber 管理，它把输入转发给本机 Steam（`deck` 目标）。客户端启动时：

1. 读取当前的输出目标（通常是 `deck` + `keyboard`），写入 `$XDG_RUNTIME_DIR/flipad-restore`；
2. 用 `SetTargetDevices(["gamepad"])` 把输出换成一个普通的 evdev Xbox 360 设备；
3. 打开这个设备并独占（`EVIOCGRAB`），这样本机的 Steam 就收不到任何输入。

退出时按文件恢复原来的目标。另外有一个通过 `systemd-run --user` 启动的守护进程（`--guard PID`）：它运行在 Steam 的进程树之外，主进程无论以什么方式结束，它都会负责恢复按键、PipeWire 周期和蓝牙名称。

## 按键整形（client/src/shaper.rs）

物理按键状态要经过整形才发出去，这部分是纯逻辑，带单元测试：

- **Home 组合键**：按下 Home 先暂扣不发。单独轻点则发一次短按；按住超过 0.35 秒则照常发给电脑；和别的键一起按则触发快捷功能，这两个键都不发给电脑。
- **布局**：任天堂布局对调 A/B 和 X/Y。
- **连发**：按设定频率让按住的键闪烁。
- **宏**：录制的是整形之后的输出（会去掉开头的空闲），回放期间由宏接管所有输出，只有 Home 仍然可用。

## Wi-Fi 链路（client/src/link.rs ↔ host/src/main.rs）

- **传输**：UDP，端口 47820，协议定义在 `proto/src/lib.rs`。
- **手柄状态**：每次变化都发送完整状态，另外每 20 ms 重发一次，丢包能自动修复，不需要确认。主机 250 ms 收不到数据就把手柄归零，10 s 收不到就移除虚拟手柄。
- **其他消息**：Ping/Pong 测延迟；还有震动、主机状态、快捷操作、指针、插件面板、图像分片等。
- **QoS**：数据包打上 DSCP EF 和 `SO_PRIORITY 6`，Wi-Fi 会走 WMM 语音队列，优先发送。
- **认证**：16 字节 token。主机同一时间只接受一个会话。

## 音频（proto/src/audio.rs）

- 使用 PipeWire 自带的 `module-rtp-sink` / `module-rtp-source`，每个方向是一个单独的 `pipewire -c` 子进程，设置了 `PR_SET_PDEATHSIG`，父进程退出时一起退出。
- 格式：S16BE、48 kHz，耳机双声道，麦克风单声道。
- 主机把「Flip 2 耳机」/「Flip 2 麦克风」设为默认设备，并把原来的默认设备记在 `~/.config/flipad/previous-default-*`，断开后恢复。
- 掌机播放时，按“掌机出声”设置决定输出到哪里（跟随 / 耳机 / 扬声器）。

## 蓝牙（client/src/ble.rs、btlink.rs、host/src/btaudio.rs）

- 通过 BlueZ D-Bus 注册 GATT 应用：
  - HID 服务 #1：Xbox Wireless Controller 固件 5.x 的报告格式（输入报告 1 号，震动输出报告 3 号）；
  - HID 服务 #2：鼠标（2 号）、媒体键（4 号）、键盘（5 号）；
  - 电池服务。
- 广播名为「Flip Pad」，外观（Appearance）为手柄；配对代理是 NoInputNoOutput。
- PnP ID 来自 BlueZ `main.conf` 里的 `DeviceID`（045E:0B13），电脑据此加载自带的 Xbox 驱动。
- 记住用过这个手柄的主机。指定连接某一台时，其他主机设为 `Blocked`（保留配对），退出时全部解除。
- 音频：掌机作为 A2DP Sink。运行期间把适配器设为可连接；收到的流按“掌机出声”设置通过 `pw-metadata target.object` 改变去向。装了 `flipad-host` 的 Linux 电脑会主动连接 A2DP，并切换默认输出。

## 界面（client/src/ui.rs、panel.rs、paint.rs）

- winit（X11）+ softbuffer + tiny-skia 软件渲染，字体用 ab_glyph 按需光栅化 Noto Sans CJK SC。
- 设计坐标是 1920×1080，按窗口大小缩放。最多 30 fps，没有变化时 1 秒刷新一次。
- 触摸：优先使用多点触摸事件；如果系统只给模拟的鼠标事件，就退化为单指操作。
