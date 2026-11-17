# pet 动画资源

桌面宠物（`/pet`）用的角色动画。**这些文件不内嵌进二进制**——`assets/` 只有被
`embedded!` / `embedded_bytes!` 显式引用才会编进去，这里没有任何引用，所以它们只随仓库发布，
下载与安装由用户自己完成。

## 桌宠

每个桌宠一个目录，目录名就是 `/pet switch <名字>` 的参数。

| 目录 | 说明 |
|---|---|
| `claude-girl/` | 暖橙配色，9 个动作，17MB |
| `dsh-girl/` | 深蓝配色，9 个动作，9.2MB |
| `codex-girl/` | 银白发色与白裙，9 个动作，24MB |

每个目录里的 `anim/README.md` 是动作对照表，`anim/*.webp` 是 spritesheet。

## 来源与许可

角色立绘与动画帧**不属于本仓库的 MIT 许可**。`claude-girl/` 与 `dsh-girl/` 来自
[aboutme-pets](https://github.com/hunmer/aboutme-pets)，再经
[linux-desktop-pet](https://github.com/linxuan-sys/linux-desktop-pet) 编码成 10 秒 WebP 片段。

`codex-girl/` 来自 linux-desktop-pet 的 `codex-girl/` 子目录（MIT，Copyleft © 2026 万昊轩）：
上游有 90 个动作，这里只抽了 `/pet` 认得的 9 个，对照表见 `codex-girl/anim/README.md`。
它的角色比另外两只高一头，抽取时按统一系数缩到同一档身材，画布仍是 640×360 一帧。

- 公开分发时必须署名上述上游项目。
- 未经许可不得商用。
- 上游完整声明见同目录 `LICENSE.upstream`。

## 安装

```bash
curl -LO https://github.com/heng30/prux/raw/main/assets/pet/anim.tar.gz
tar -xzf anim.tar.gz -C ~/.prux/extensions/pet/
```

`anim.tar.gz` 就是本目录下所有桌宠的 `anim/` 打包，展开后是
`~/.prux/extensions/pet/<桌宠>/anim/`。也可以只装一个：把某个桌宠目录整个拷进去。

动画目录也可以由 `~/.prux/extensions/pet/config.json` 的 `media` 指向别处
（此时按 `<media>/<桌宠>/anim/` 查找）。

缺资源时宠物不建窗，只在聊天区给一条带下载地址的提示。
