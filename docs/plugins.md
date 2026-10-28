# 写游戏插件

插件运行在**电脑上**（`flipad-host` 里），编译进主机程序。它根据正在运行的游戏决定是否激活；激活后，掌机的「游戏」标签页显示它提供的内容。只在 Wi-Fi 模式下可用。

## 生命周期

1. 主机每秒检测一次当前游戏：在当前用户的进程里找环境变量 `SteamAppId`，再从 Steam 的 `appmanifest_<id>.acf` 读出游戏名。结果是一个 `GameInfo { app_id, name, process, pid, started }`。
2. 按 `registry()` 里的顺序，第一个 `matches(Some(&game))` 返回 true 的插件被激活（`activate`）。没有游戏时传入 `None`。
3. 激活期间，每 100 ms 调用一次 `tick`；掌机上点按钮时调用 `button(id)`。
4. 游戏退出或换成别的插件时，调用 `deactivate`，掌机上的「游戏」标签页随之消失。

## 接口

```rust
pub trait GamePlugin: Send {
    fn id(&self) -> &'static str;                         // 稳定 ID，按钮事件按它分发
    fn matches(&self, game: Option<&GameInfo>) -> bool;
    fn activate(&mut self, ctx: &mut Ctx) {}
    fn deactivate(&mut self) {}
    fn tick(&mut self, ctx: &mut Ctx) {}                   // 约 10 Hz
    fn button(&mut self, id: u16, ctx: &mut Ctx) {}
}
```

`Ctx` 提供的能力：

| 方法 | 作用 |
|---|---|
| `ctx.game` | 当前游戏信息 |
| `ctx.set_panel(title, widgets)` | 设置面板，有变化才发送 |
| `ctx.action(HostAction::…)` | 执行内置快捷操作 |
| `ctx.keys(&[KeyCode::…])` | 按下任意组合键（依次按下，倒序松开） |
| `ctx.send_rgba(image_id, w, h, &rgba)` | 把 RGBA 像素编码成 JPEG，推送给某个图像控件 |
| `ctx.send_jpeg(image_id, bytes)` | 直接推送 JPEG |

控件（`flipad_proto::Widget`）：

- `Text { text, big }`：一行文字，`big` 用于标题或数值。
- `Button { id, label }`：按钮，点击后回调 `button(id)`。
- `Image { id, aspect }`：图像区域，`aspect` = 宽 / 高 × 1000，比如 4:3 就是 1333。

## 例子：小地图插件的骨架

```rust
// host/src/plugins/my_game.rs
use flipad_proto::Widget;
use super::{Ctx, GameInfo, GamePlugin};

#[derive(Default)]
pub struct MyGame { last: Option<std::time::Instant> }

impl GamePlugin for MyGame {
    fn id(&self) -> &'static str { "my_game" }

    fn matches(&self, game: Option<&GameInfo>) -> bool {
        game.is_some_and(|g| g.app_id == Some(123456))
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        ctx.set_panel("我的游戏", vec![
            Widget::Image { id: 1, aspect: 1000 },
            Widget::Text { text: "小地图".into(), big: true },
            Widget::Button { id: 1, label: "打开地图".into() },
        ]);
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        // 5 fps 对地图足够，也能把 Wi-Fi 流量压到很低
        if self.last.is_some_and(|t| t.elapsed().as_millis() < 200) { return; }
        self.last = Some(std::time::Instant::now());
        let (w, h, rgba) = capture_minimap(); // 你的截图 / 内存读取逻辑
        ctx.send_rgba(1, w, h, &rgba);
    }

    fn button(&mut self, id: u16, ctx: &mut Ctx) {
        if id == 1 { ctx.keys(&[evdev::KeyCode::KEY_M]); }
    }
}
```

然后在 `host/src/plugins/mod.rs` 里声明 `mod my_game;`，并在 `registry()` 中把它加在通用的 `current_game` **之前**。

## 参考实现

- `current_game.rs`：通用插件，任何 Steam 游戏都会激活，显示游戏名、已玩时长，以及几个游戏内常用按钮。
- `demo.rs`：雷达演示（设置环境变量 `FLIPAD_DEMO_PLUGIN=1` 启用），演示怎样用 tiny-skia 画图，再用 `send_rgba` 以 5 fps 推送。

## 关于截取游戏画面

主机端还没有内置截屏。在 KDE / GNOME 的 Wayland 下截屏需要走 xdg-desktop-portal 的 ScreenCast（PipeWire 视频流），或者针对具体游戏读日志、内存、官方 API。插件接口只要求最后交出 RGBA 或 JPEG，取数据的方式由插件自己决定。

## 流量

图像按 1200 字节一片走 UDP，丢一片整帧作废，下一帧补上。400×300、质量 72 的 JPEG 大约 15–30 KB，5 fps 约 100–150 KB/s，对手柄延迟的影响可以忽略。
