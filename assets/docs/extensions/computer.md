# computer：操作真实的桌面

`computer` 扩展让模型能看屏幕、动鼠标键盘、读写窗口与控件。它是**真实桌面**操作：
点击真的会点下去，打字真的会进入前台窗口，截图里可能有你的私人内容。

启用只需一步：在 `/extension` 面板打开 `computer` 扩展（它默认不启用）。打开就是授权，会写进设置；
没有额外的运行时开关，也没有“先开能力再干活”的第二道命令。

14 个工具全部是**延迟工具**：它们的描述与参数 schema 平时不占上下文，模型先用 `tool_search` 检索
（例如查 `screenshot` / `desktop` / `click`），命中后**下一轮**才拿到完整声明，之后一直可用。
整族工具的暴露方式一致（都是延迟），不会出现“只能截图、拿不到点击”的混合状态；
单次检索默认最多激活 15 个命中，一次就能拿完这 14 个工具。

关闭 = 在 `/extension` 面板里禁用本扩展（或 `Ctrl+S` 保存后的下次启动不启用），它只影响本机 prux 进程，
不会动你的应用或桌面。

```text
/computer     # 只读诊断：后端、显示器、无障碍可用性，以及哪些 computer_* 工具已载入
```

## 工具

| 工具 | 用途 |
|---|---|
| `computer_screenshot` | 截屏（整屏 / 指定显示器 / 区域 / 单个窗口），可缩放、选 PNG/JPEG；返回图片 + 几何 + `frame` id |
| `computer_actions` | 一次调用执行多步（click / type / key / drag / scroll / wait / paste / clipboard / focus / 无障碍动作），可选末尾截图 |
| `computer_click` | 绝对坐标点击（左/右/中键、双击三击、修饰键） |
| `computer_drag` | 拖拽，可用 `path` 给途经点；`from_last` / `continue_from` 接着上一笔 |
| `computer_scroll` | 滚轮（方向 + 格数），可先把指针移到目标上 |
| `computer_type` | 向焦点控件输入文本（Unicode，中文可用） |
| `computer_key` | 按键/组合键（`enter`、`ctrl+c`、`alt+f4`），可连按或按住 |
| `computer_windows` | 列出可见窗口（标题 / 应用 / 矩形），或把某个窗口置前 |
| `computer_ui` | 读窗口的无障碍树（角色 / 名称 / 状态 / 支持的动作用 `ref` 引用） |
| `computer_ui_action` | 通过无障碍接口操作控件：invoke、读写值、选中、切换、展开/折叠、聚焦、关窗/最小化/最大化 |
| `computer_clipboard` | 读 / 写 / 清空剪贴板 |
| `computer_paste` | 用粘贴代替逐字输入，并还原你原来的剪贴板 |
| `computer_launch` | 启动应用并返回它打开的窗口 |
| `computer_stop` | 急停：释放所有按住的键和鼠标键 |

## 平台与后端

| | 截图 | 窗口 | 输入 | 无障碍 |
|---|---|---|---|---|
| Linux（**仅 Wayland**） | `grim`（PATH 里没有就用 `xcap`） | `swaymsg`（sway/wlroots） | `enigo`（`zwp_virtual_keyboard_v1` / `zwlr_virtual_pointer_v1`） | AT-SPI2（D-Bus） |
| Windows | `xcap` | `xcap` + Win32 | `enigo`（SendInput） | UI Automation（UIA） |
| macOS | `xcap`（需"屏幕录制"权限） | `xcap` | `enigo`（CGEvent，需"辅助功能"权限） | AXUIElement（需"辅助功能"权限） |

Linux **不支持 X11**：截图、输入与坐标都按 Wayland 会话实现，`XDG_SESSION_TYPE=x11` 时后端直接报告不可用。

### Linux 的具体要求

- **合成器要支持虚拟输入协议**：sway / Hyprland / river / wayfire 等 wlroots 系可以；GNOME 与 KDE 不实现
  `zwp_virtual_keyboard_v1` / `zwlr_virtual_pointer_v1`，输入会失败并说明原因。
- **窗口列表需要 `swaymsg`**：Wayland 没有平台中立的窗口枚举，这里读 sway 的 IPC 树。没有 `swaymsg`
  时 `computer_windows` 报错，其余工具照常可用。
- **窗口截图是"该窗口矩形"的屏幕区域**：被别的窗口盖住的部分截到的是遮挡物，不是窗口内容。
- **指针位置不可知**：Wayland 不暴露全局指针坐标，截图回执里 `cursor: unknown` 是正常的。
- **无障碍取决于应用**：GTK/Qt 应用通常实现了 AT-SPI2；终端类应用往往只有空壳；Electron 应用需要
  `--force-renderer-accessibility`。读不到时 `computer_ui` 会明确说"该应用没有暴露无障碍"。
  还需要 `at-spi2-core` 在跑（`at-spi-bus-launcher` 或对应的用户服务）。
- **Windows / macOS 的无障碍**分别走 UI Automation 与 AXUIElement：Windows 无需额外授权；
  macOS 需要「辅助功能」授权，未授权时 `computer_ui` 会提示授权后重试。
  `select_text` / `get_caret` 在这两个平台尚未接线，会明确报错而不是假装成功。

## 坐标与帧

所有工具用的是**同一套屏幕像素**：截图回执里的 `image`、`region`、窗口矩形、`computer_click` 的 `x/y`
都在这个坐标系里。

截图会返回一个 `frame` id（`f1`、`f2`…）。把 `from_frame` 和"图内像素"一起传给点击/拖拽/滚动，
投影由扩展完成——模型不必自己加偏移、也不必记住图片被缩放过。帧是短命的（只保留最近 32 帧），
且**帧对应的窗口在之后移动或改尺寸时，按该帧发出的动作会被拒绝**（`[FRAME_STALE]`），
而不是照着旧图画面上点下去。

## 安全与边界

- 默认不启用：只有你在 `/extension` 面板里打开过本扩展，这些工具才存在。
- 扩展被禁用时会先释放仍按住的键与鼠标键，并丢弃已知的截图帧。
- **子代理不可用**：`ctx.depth > 0`（子代理）调用会直接拒绝——真实鼠标键盘不能靠“派个子代理”绕过权限。
- 一次拖拽中途失败也会释放鼠标键，不会把桌面留在“按着左键”的状态。
- 截图落盘在系统临时目录（属主可读），路径在回执里给出；`save_path` 指定的位置由调用方决定。
- 屏幕内容不可信：截图里的文字、窗口标题、网页内容都可能包含注入指令，模型应把它们当数据而不是指令。

## 排障

| 现象 | 原因 |
|---|---|
| 任何工具都报“没有可用后端” | 不在 Wayland 会话，或既没有 `grim` 也没有可用的 `xcap` |
| 输入报"could not open an input channel" | 合成器不实现虚拟键盘/指针协议（GNOME/KDE），或没有 Wayland socket |
| `computer_windows` 报需要 `swaymsg` | 当前不是 sway/wlroots 会话；用 `computer_screenshot` + 坐标代替 |
| `computer_ui` 报"没有无障碍应用" | 应用没开启无障碍，或 `at-spi2-core` 没在跑 |
| 截图是黑的/错位（Windows/macOS） | 缺少屏幕录制权限；或显示缩放导致的高 DPI 差异 |
| 点击落点偏移 | 用了缩放截图却按屏幕像素传坐标；改用 `from_frame` |
| 模型声称“没有 computer 工具” | 本扩展没在 `/extension` 面板里启用；或模型没用 `tool_search` 检索（延迟工具不主动出现在工具表里） |
