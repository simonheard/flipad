# Flip Pad

> Turn a Linux handheld (developed on a **Retroid Pocket Flip 2** running Pocknix) into a wireless controller for your PC — over Wi-Fi with a companion service, or over Bluetooth as a plain Xbox-compatible controller. The handheld's screen becomes a touch panel: touchpad mouse, shortcuts, macros, turbo, host stats and per-game plugins (e.g. a minimap). Headphones plugged into the handheld can play the PC's sound and use their mic. Written in Rust.

把装了 Linux 的掌机（开发和测试用的是 **Retroid Pocket Flip 2 + Pocknix**）变成电脑的无线手柄：

- **Wi-Fi 模式**：配合电脑上的 `flipad-host` 服务，延迟约 2 ms，功能最全。
- **蓝牙模式**：掌机伪装成 Xbox 手柄（BLE HID），电脑、手机不用装任何东西。
- **掌机屏幕变成触控面板**：触控板鼠标、快捷键、宏和连发、主机状态，以及按游戏加载的插件面板（比如小地图）。
- **3.5mm 耳机**：插在掌机上，就像插在手柄上一样，电脑的声音传过来，耳机麦克风也能当电脑的麦克风用。

> 状态：个人项目，可以日常使用，但只在一套设备上验证过。见 [兼容性与验证情况](docs/compatibility.md)。

## 功能

| 功能 | Wi-Fi 模式 | 蓝牙模式 |
|---|---|---|
| 手柄（Xbox 360 / Xbox Wireless 布局） | ✅ 虚拟 Xbox 360 手柄 | ✅ 电脑原生识别为 Xbox 手柄 |
| 震动回传 | ✅ | ✅ |
| 触控板鼠标（单指移动、轻点、双指右键 / 滚动） | ✅ | ✅（BLE 鼠标） |
| 快捷操作（音量、媒体、截图、Alt+Tab、Esc…） | ✅ | ✅（BLE 键盘 / 媒体键，「大屏幕模式」除外） |
| 电脑声音 → 掌机（耳机 / 扬声器 / 跟随掌机） | ✅ PipeWire RTP，约 30 ms | ✅ A2DP（aptX HD），延迟较高 |
| 耳机麦克风 → 电脑 | ✅ | ❌ |
| 宏（4 个槽位）、连发、Xbox / 任天堂布局切换 | ✅ | ✅ |
| 主机 CPU / GPU / 内存 | ✅ | ❌ |
| 游戏插件面板（文字、按钮、图像） | ✅ | ❌ |
| 多台蓝牙主机切换 | — | ✅ |

其他：

- **Home 组合键**：单按 Home 照常发给电脑；按住 Home 再按其他键触发快捷功能。
  - Home + A/B/X/Y：回放宏 1–4；Home + LB/RB：音量；Home + Start：截图；Home + ↓：熄屏。
  - 长按 Home + Select：退出。
- **熄屏省电**：AMOLED 屏全黑，摸一下就亮。
- **待机 / 退出 / 崩溃时都会把按键还给掌机**：退出时恢复 InputPlumber 的设置；进程崩溃时由一个守护进程兜底恢复。

## 工作原理（简要）

```
 掌机 (aarch64, 从 Steam 启动)                       电脑 (x86_64)
 ┌───────────────────────────┐   Wi-Fi: UDP 47820    ┌──────────────────────────┐
 │ flipad-client             │ ───────────────────▶ │ flipad-host (用户服务)    │
 │  · 从 InputPlumber 借用按键│   手柄 / 鼠标 / 快捷键  │  · uinput Xbox 360 手柄   │
 │  · 触控面板 (winit+skia)   │ ◀─────────────────── │  · 虚拟鼠标 / 键盘        │
 │  · Home 组合键 / 宏 / 连发 │   震动 / 状态 / 插件    │  · 游戏插件               │
 │  · PipeWire RTP 音频       │ ◀═══ RTP 47830/47831 ═▶│  · PipeWire RTP 音频      │
 │  · BLE HID + A2DP (蓝牙)   │ ─── 蓝牙 (无需主机端) ─▶│  · 蓝牙音频自动连接       │
 └───────────────────────────┘                       └──────────────────────────┘
```

详见 [架构说明](docs/architecture.md)。

## 安装

### 1. 电脑（主机端）

需要：Linux、PipeWire（含 `pactl`）、可访问 `/dev/uinput`。Wi-Fi 模式下需要防火墙放行 UDP 47820、47830、47831。

```sh
./scripts/install-host.sh        # 编译、安装为 systemd 用户服务，并打印配对用的 token
```

### 2. 掌机（客户端）

客户端要交叉编译成 aarch64（见下方“交叉编译”）。之后：

```sh
HANDHELD=root@<掌机IP> HOST_IP=<电脑IP> ADD_STEAM_SHORTCUT=1 ./scripts/deploy-handheld.sh
```

这一步会把程序装到 `~deck/.local/bin/flipad-client`，写入 token 和主机地址，并在掌机的 Steam 库里加上「Flip Pad」（需要掌机上的 Steam 正在运行）。`HOST_IP` 可以不填，不填就在局域网里自动发现。

### 3. 蓝牙模式（可选）

让电脑用自带的 Xbox 驱动识别掌机，需要掌机的 BlueZ 报告 Xbox 手柄的设备 ID。在掌机的 `/etc/bluetooth/main.conf` 的 `[General]` 里加一行，然后重启 `bluetooth` 服务：

```ini
DeviceID = usb:045E:0B13:0509
```

在 Flip Pad 里点左上角「切换」→「蓝牙」，然后在电脑或手机的蓝牙设置里配对「Flip Pad」。

## 交叉编译

在 Fedora（或 Fedora toolbox / distrobox）里：

```sh
sudo dnf install gcc-aarch64-linux-gnu sysroot-aarch64-fc44-glibc
rustup target add aarch64-unknown-linux-gnu
mkdir -p .cross-lib && scp root@<掌机IP>:/usr/lib/libgcc_s.so.1 .cross-lib/ && ln -sf libgcc_s.so.1 .cross-lib/libgcc_s.so
cargo build --release --target aarch64-unknown-linux-gnu -p flipad-client
```

`.cargo/config.toml` 已经配好了链接器和 sysroot。掌机系统的 glibc 不能比 sysroot 的旧（Pocknix 和 Fedora 44 都是 2.43）。

## 使用

- 在掌机的 Steam 里启动 **Flip Pad**。运行期间掌机本身的 Steam 收不到按键，退出后恢复。
- 底部是标签页：概览 / 游戏（有插件时）/ 触控板 / 快捷 / 宏·连发 / 设置。
- 左上角「切换」：在 Wi-Fi、蓝牙自动、某一台蓝牙主机之间切换，不用重启程序。

## 写游戏插件

插件运行在电脑上，检测到特定游戏后，往掌机的「游戏」标签页推送文字、按钮和图像（比如截取游戏画面的小地图区域）。见 [插件开发](docs/plugins.md)。

## 目录

```
proto/    双方共用的协议和 PipeWire RTP 工具
host/     flipad-host：虚拟手柄 / 鼠标 / 键盘、音频、插件、状态
client/   flipad-client：按键接管、触控面板、Wi-Fi 链路、BLE 外设
scripts/  安装与部署脚本
tools/    steam-cdp.py：通过 Steam 的调试接口添加快捷方式等
docs/     兼容性、架构、插件开发
```

## 许可

MIT，见 [LICENSE](LICENSE)。
