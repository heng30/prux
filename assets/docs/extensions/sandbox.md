# OS 级命令沙箱（sandbox）

`permissions` 是**执行前的静态判定**：按规则和参数形状拦命令，拿不准就说拿不准。它挡不住
动态拼出来的命令（`sh -c "$(cat /tmp/x)"` 之类）。`sandbox` 补的是另一半——**内核层边界**：
命令照跑，但文件系统写权限由内核按红线裁决，跟你写了什么规则无关。

在 `/extension` 面板里默认是关闭的，用空格开启后立即生效。

## 后端

启用时会探测，不让你选（选错等于没有边界）：

| 平台 | 程序 | `/sandbox` 里的名字 |
|---|---|---|
| Linux | `PATH` 上的 `bwrap`（bubblewrap） | `linux-bwrap` |
| macOS | `/usr/bin/sandbox-exec`（Seatbelt） | `macos-seatbelt` |
| Windows | 没有 | 恒为不可用 |

探测不只是「文件在不在」：会真的用它起一个 `true` 进程。Linux 上禁用了用户命名空间
（容器里常见）时 bwrap 装得下却起不来，这一项就是为它设的。

## 边界

两个后端同一套红线：

| 面 | 规则 |
|---|---|
| 文件系统读 | 全局可读 |
| 文件系统写 | 只有**本次调用的工作目录**与系统临时目录（`/tmp`、macOS 的 `/private/tmp` + `/private/var/folders`）可写 |
| 进程 | 可起子进程；bwrap 带 `--die-with-parent`，prux 退出时一并收掉 |
| 网络 | 放行 |

工作目录可写这一点意味着：**在同一台机器上换个目录就能写别处**——沙箱保护的是工作目录之外
的东西（`~/.ssh`、`~/.aws`、源码之外的数据），不是「agent 只能碰这个项目」。

`~/.cargo`、`~/.npm`、`~/.cache` 都在只读区，所以 `cargo build` / `npm install` 会失败。
这是刻意的取舍：要让它们工作，把缓存目录设到工作目录里，例如：

```bash
CARGO_HOME=$PWD/.cargo npm_config_cache=$PWD/.npm-cache
```

## 失败即拒绝

启用时探测失败（后端没装、或装了起不来）**不会**退回无沙箱执行：扩展会装上一个
「一律拒绝启动」的包装器，模型每次调 `bash` 都会拿到一句明确的拒绝理由：

```text
Sandbox refused to start the command: bwrap (bubblewrap) was not found on PATH.
Fix the backend or turn the `sandbox` extension off to run commands unsandboxed.
```

同样的检查每次调用都会重走一遍，所以中途把 `bwrap` 删掉也不会静默放行。
要恢复无沙箱执行，就在 `/extension` 里把本扩展关掉——关掉这件事是显式的，不会自动发生。

## 生效范围

- 覆盖 `bash` 工具（含扩展经它发起的命令）里的每一次执行；
- **不覆盖** `powershell`（Windows 没有后端），也不覆盖 `grep` / `find` / `git` 这类工具内部
  自己起的固定命令——它们不接受模型给的任意命令行，不在同一条威胁模型里。

`/sandbox` 打印当前后端、可写目录与网络状态；扩展没启用时它会明说「未生效」。
