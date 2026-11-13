# review（把别人的 PR / issue 拉进来评审）

`review` 扩展做**入站**评审：给一个 GitHub 上的 PR 或 issue 链接，它在 `agent_dir()` 下开一份
只读副本、取回元数据与 diff，把评审简报交给 agent；需要时再把结论作为评论发回那个 PR / issue。

`pr` 扩展方向相反（把本地改动推出去变成 PR）。两者互不依赖，谁也不会动对方的分支。

- 扩展名：`review`（`settings.json` 的 `enabledExtensions` / `/extension` 面板切换）
- **默认关闭，且只在 `Dev` 模式注册**：它会建副本、跑网络请求，`--comment` 还会对外写评论。
- 不提供模型可见的工具：对工具列表与系统提示没有影响。
- **不动你的仓库**：副本在 `agent_dir()` 下，不建 worktree、不写远端引用、不切你的分支。
- **只有 `/review` 会发起评审**：没有自动触发点。
- **不会改 PR**：不推提交、不改标题、不合并；最多发一条评论。

## 目录

- [一次评审做什么](#一次评审做什么)
- [命令](#命令)
- [副本与 diff](#副本与-diff)
- [回帖：marker 与幂等](#回帖marker-与幂等)
- [凭证](#凭证)
- [和 pr 的分工](#和-pr-的分工)

---

## 一次评审做什么

`/review <URL>` 一次：

1. 解析链接（`…/pull/<n>`、`…/issues/<n>`，或简写 `owner/repo#<n>`），记下这次会话的目标。
2. `gh <pr|issue> view` 取标题、正文、作者、状态、base / head 与改动文件清单。
3. 在 `agent_dir()/extensions/review/<目标>/` 建一份副本：`git init` + `git fetch --depth 1`
   取 `refs/pull/<n>/head`（issue 取默认分支），然后 `checkout --detach`。
4. PR 的 diff 由 `gh pr diff` 取（GitHub 自己的三点差异），落成副本里的 `review.diff`。
5. 把一份**评审简报**作为用户消息投给 agent：目标身份、副本绝对路径、diff（超长时只带开头
   一段，其余靠读文件）、改动清单，以及评审口径（`file:line` + 会真的坏在哪 + 最小修法；
   没有发现就直说，不要凑数）。

取材料（第 2–4 步）在后台线程里做，界面不卡；成功后起一轮，失败则报原因并清掉目标（不留一个
「等着回帖」的悬空目标）。

## 命令

| 命令 | 作用 |
|------|------|
| `/review <URL>` | 拉取该 PR / issue 并开始评审 |
| `/review --comment <URL>` | 同上，并且这一轮回结时把**最后一条回答**作为评论发回去 |
| `/review status`（或裸 `/review`） | 打印当前目标、副本路径、回帖状态、marker 与已发布的评论记录 |
| `/review clean` | 删掉 `agent_dir()` 下的所有副本（保留发布记录） |
| `/review prs <仓库> [N-M]` | 列出该仓库 open 的 PR，输出 markdown 表格（`PR 地址` + `PR Title`，地址形如 `owner/repo#12`） |

`/review prs` 的仓库参数接受完整链接（`https://github.com/owner/repo`）、`github.com/owner/repo`
与短形式 `owner/repo`；企业版主机写成 `https://<host>/owner/repo`，表格里的地址仍只写
`owner/repo#<n>`。可选范围 `N-M` 是 **1-based 的第 N 到第 M 条**（含两端，顺序与 `gh pr list`
一致，也可写成 `2-10`），不给就是全部（最多 1000 条）。列表来自 `gh pr list --state open`，
只读、不建副本、不设评审目标。该命令同步跑 `gh`，网络慢时界面会等一下。

`/review` 忙碌时按普通文本入队（它会发起一轮 prompt）。

## 副本与 diff

- 副本目录：`agent_dir()/extensions/review/<host>-<owner>-<repo>-<n>/`（issue 多一个 `-issue`
  后缀）；同一个目标重复评审复用同一份，不重复克隆。
- 副本是**只读用途**：agent 被要求只在里面读文件、不改、不提交。它是 `agent_dir()` 下的
  临时目录，`/review clean` 可随时整份删掉。
- 文件在会话目录之外，因此 `permissions` 的 `external_directory` 层若已开启，读副本会按该层
  的规则询问——那是它在正常工作。
- diff 用 GitHub 的三点差异：本地 shallow 副本没有 merge-base，自己算不出同样的结果。

## 回帖：marker 与幂等

评论正文首行带一个去重标记：

```
<!-- prux-review:v1:<hash> -->
```

`hash` 由「目标 + 会话 id」算出。投递流程：

1. 先按 marker 扫一遍已有评论：命中就复用它的链接，**不再发**。
2. 没命中才发（`gh issue comment`）。
3. 发完再扫一遍确认落地。扫不到就报 `unconfirmed` 并**拒绝重发**——宁可让人手动看一眼，
   也不要在一个 PR 下贴两条一样的评审。

因此：同一次会话对同一个目标最多一条评论；换一个会话再评审一次，会是一条新评论（那是另一次
评审的记录）。发布结果落在 `agent_dir()/extensions/review/publications.json`，`/review status`
会列出来。

评论内容就是 agent 这一轮的最后一条回答（截断到 60000 字符）。指令里已经告诉模型「这段话会被
原样贴出去，只写结论、不写过程」。

## 凭证

只用 `gh` 自己的登录态：`gh auth status` 不通就报错停下，不读环境变量里的 token、不落盘任何
凭据。企业版（GHES）主机可以直接用它的 URL，`gh` 的 `--repo <host>/<owner>/<repo>` 会带上主机。

## 和 pr 的分工

- `review` 管**别人的东西**：读进来、评审、必要时回一条评论。
- `pr` 管**自己的东西**：提交、推、开 PR、跟 CI。

两者都不合并，也都不改对方的对象。
