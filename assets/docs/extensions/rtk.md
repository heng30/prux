# rtk（bash 命令改写）

`rtk` 扩展把每一次 `bash` 调用先交给外部 [rtk](https://github.com/rtk-ai/rtk) CLI 过一遍：
rtk 认为这条命令有更紧凑的等价写法时返回改写过的那条，prux 就用改写后的命令执行，从而让
工具输出（以及后续上下文）小很多。

**默认不启用**，且仅在 `Dev` / `Creator` 模式（及 `All`）下可用。在 `/extension` 面板开启，
或写 `settings.json`：

```json
{ "enabledExtensions": ["rtk"] }
```

## 依赖

`rtk` 必须装在 `PATH` 上，来源是 <https://github.com/rtk-ai/rtk>。

启用时（以及每个会话开始时）会试着起一次 `rtk --version` 看它在不在；起不来就**报错**，聊天区会给出：

```
`rtk` extension is enabled but `rtk` was not found in PATH (or it failed to start).
Install it from https://github.com/rtk-ai/rtk — until then bash commands run unchanged.
```

同一条提示一个进程只报一次（装好之后会重新可用）。

## 行为

- 只介入 `bash` 工具；其它工具、没有 `command` 参数的调用不受影响。
- 每次调用前跑 `rtk rewrite <命令>`：退出码 `0` 或 `3` 且 stdout 非空 → 用改写的命令；
  否则（退出码 `1`、输出为空、与原文相同）→ 原命令照跑。
- **全程 fail-open**：rtk 不在、起不来、超时（5 秒）、输出看不懂，都按原命令执行——
  一个可选的输出压缩工具永远不该挡住真正的命令。
- 改写发生在权限判定之前，`permissions` 等扩展看到的是**真正要执行**的那条命令。
