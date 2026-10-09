# codemode（JavaScript 工具编排）

`codemode` 是一个**内置扩展**（注册一个同名工具），让模型写一段 JavaScript，在沙箱里用
`tools.<name>(args)` 并行/串联调用 prux 的其它工具（内置工具、扩展工具、MCP 工具都能调），
只把脚本的输出交回模型。适合「一次批量调十几个工具」「把大输出过滤成小结果」这类场景，
替代一串逐个发出的工具调用。

**默认不启用**：在 `/extension` 面板开启，或写进 `settings.json` 的 `enabledExtensions`。
声明为 `Dev` / `Creator` 模式可用。

## 脚本能做什么

脚本是**一个 async 函数体**：顶层 `await` 与 `return` 都可用。

```js
// @options: {"max_output_tokens": 2000, "timeout_ms": 30000}
const [a, b] = await Promise.all([
  tools.read({ path: "Cargo.toml" }),
  tools.bash({ command: "git log --oneline -5" }),
]);
text(a.slice(0, 200));
return b;
```

| 全局 | 作用 |
|------|------|
| `tools.<name>(args)` | 调一个工具，返回它的结果（声明了 `outputSchema` 的工具返回结构化结果，否则返回文本）；失败/被拒/参数非法时抛出带工具错误文本的 `Error` |
| `tools["<原名>"](args)` | 工具名归一后的标识符（`my-tool` → `my_tool`）之外，原名也能用 |
| `ALL_TOOLS` | `{ name, description }` 列表（`description` 是完整声明样例） |
| `await searchTools(query, { limit, namespace })` | 按 BM25 检索可调工具（默认 8 条），返回 `ALL_TOOLS` 同形条目 |
| `await describeTool(name)` | 某个工具的声明样例；找不到返回 `null` |
| `await describeNamespace(name)` | 某命名空间下的 `{ namespace, tools }`；别名写法（`mcp__dev-radius` / `dev_radius` 等）都认 |
| `await models.getModelsOfType(type, provider?)` / `getAvailableOfType` / `getModelOfType` | 模型目录查询（`chat` / `image` / `classifier`）；`getAvailableOfType` 只列有凭据的 provider |
| `await models.classify(model, context)` | 跑一次分类器（如 Jev） |
| `await models.generateImages(model, context)` | 跑一次图片生成；结果里的 image block 用 `image()` 展示 |
| `text(v)` / `console.log(...)` | 追加一段文本输出（非字符串按 JSON 序列化）。多于一条文本项时各带 `==> text N/M <==` 编号（`M` 只算 `text()` 与顶层 `return`）；`console.*` 的行不占编号，汇总在末尾一个 `<console_output>` 块里 |
| `image(dataUriOrBlock)` | 追加一张图片（base64 `data:` URI，或 MCP / `models.generateImages()` 的 image block）；同时把图片落到临时文件，并在结果里给出它的路径供后续 `read` |
| `store(key, value)` / `load(key)` | 跨 `codemode` 调用保存 JSON 值（`store(key, undefined)` 删除）；写入在**脚本成功**时才落盘 |
| `exit()` | 立刻成功结束脚本（相当于顶层提前 `return`） |

读一个不存在的 `tools.<name>` 或命名空间成员会抛 `TypeError` 并点名近似名（例如
`tools.Echo` → `Did you mean tools.echo?`），探测成员是否存在请用 `"name" in tools`。

`models.classify()` / `models.generateImages()` 每个脚本最多同时跑 4 个（其余排队，
`Promise.all()` 直接铺开即可），只认 `model` 的 `provider` / `id`（脚本自带的 `baseUrl` /
`headers` 一律忽略），并且**不因 provider 报错而抛错**——检查 `stopReason` 与 `errorMessage`。
它们的用量计入会话成本。`generateImages()` 返回的图片不会自动展示，要逐块 `image(block)`；
一张都没展示时结果里会补一条提示。

限制：没有 Node、文件系统、网络、定时器（`setTimeout` 不存在），`tools` 表是冻结的，
脚本不能起脚本（`codemode` 自己声明 `model-only`）；内建（`Array.prototype`、`Object.prototype`、
各类原型与迭代器原型）也在脚本运行前被冻结，改原型（如 `Array.prototype.toJSON = …`）不再生效；
堆上限 256 MB，超出时脚本内抛 `InternalError: out of memory`。

## `models.*` 的类型定义

描述里只写一行签名清单（模型每次请求都要承担那份固定开销），类型与字段语义在这里：

```ts
type ModelType = "chat" | "image" | "classifier";

/** 模型目录条目。`provider` 与 `id` 定位它，其余字段随类型而异。 */
interface ModelInfo {
  type?: ModelType;
  provider: string;
  id: string;
  name: string;
  api: string;
  input: ("text" | "image")[];
  contextWindow?: number;
  [key: string]: unknown;
}

type ClassifierQuestion =
  | { type: "choice"; instructions: string; criteria: Record<string, string> }
  | { type: "score"; instructions: string; criteria: string[] }
  | { type: "bool"; instructions: string; criteria: { true: string; false: string } };
type ClassifierAnswer =
  | { type: "choice"; choice: string; probabilities: Record<string, number>; confidence: number }
  | { type: "score"; score: number; confidence: number }
  | { type: "bool"; probability: number };
interface ClassifierContext {
  state: Record<string, unknown>;
  questions: Record<string, ClassifierQuestion>;
}
interface ClassifierResult {
  api: string;
  provider: string;
  model: string;
  answers: Record<string, ClassifierAnswer>;
  /** 服务端报告用量时才有；成本单位为 USD。 */
  usage?: { input: number; output: number; totalTokens: number; cost: { total: number } };
  stopReason: "stop" | "error";
  errorMessage?: string;
  timestamp: number;
}

type TextBlock = { type: "text"; text: string };
/** `data` 是 base64。 */
type ImageBlock = { type: "image"; data: string; mimeType: string };
interface ImagesContext {
  /** 提示词文本块，外加用于编辑或参考的图片块。 */
  input: (TextBlock | ImageBlock)[];
}
interface ImagesResult {
  api: string;
  provider: string;
  model: string;
  /** 生成的图片；同时返回文本的模型还会有文本块。 */
  output: (TextBlock | ImageBlock)[];
  usage?: { input: number; output: number; totalTokens: number; cost: { total: number } };
  stopReason: "stop" | "error";
  errorMessage?: string;
  timestamp: number;
}
```

签名本身（描述里也有一份）：

```ts
declare const models: {
  getModelsOfType(type: ModelType, provider?: string): Promise<ModelInfo[]>;
  getAvailableOfType(type: ModelType, provider?: string): Promise<ModelInfo[]>;
  getModelOfType(type: ModelType, provider: string, id: string): Promise<ModelInfo | null>;
  classify(model: ModelInfo, context: ClassifierContext): Promise<ClassifierResult>;
  generateImages(model: ModelInfo, context: ImagesContext): Promise<ImagesResult>;
};
```

## 首行选项

脚本首行可以（也只有首行可以）写一行选项：

```
// @options: {"max_output_tokens": 1000, "timeout_ms": 60000}
```

- `max_output_tokens`：脚本输出的 token 预算（默认 10000，按 4 字符/token 估算）。超出时保留
  首尾各一半，完整输出落到临时文件并在结果里给出路径（可用 `read` 取回）。
- `timeout_ms`：整个脚本（含工具调用）的硬超时。默认不限时；兜底墙钟为 30 分钟。

## 嵌套调用怎么记账

脚本里的每次工具调用都走 agent 的嵌套调用管道：校验、暴露方式门控、取消信号、递归限深
与直接调用一致，事件带 `parentToolCallId`，并记进调用方结果的 `details.nestedCalls`
（`toolCallId` / `toolName` / `args` / `isError` / `text` / `durationMs` / `usage`），
用量并入调用方，因此**会话成本里能看到脚本花的钱**。嵌套结果本身不进对话，只有脚本输出进。

可调集合（与 pi 一致）：已激活的 `direct` 工具 + 全部 `codemode` exposure 工具 +
已激活的 `deferred` 工具。`model-only`（含 `codemode` 自己）与 `hidden` 不进脚本工具表。

## 呈现方式与预算

`mode` / `inlineBudget` 控制模型看到什么（配置在 `agent_dir()/extensions/codemode.json`）：`on`（默认）下 `direct` 工具照常声明、各自描述末尾附上
脚本调用样例，`codemode` 的描述只列「不单独声明」的工具；`only` 下可调工具全部收进
`codemode` 的描述、请求里不再声明它们。预算放不下的工具用 `searchTools()` 找。

改配置：`/codemode` 开关设置面板（Space/Enter 循环档位、立即落盘），`/codemode show` 打印
当前值与配置文件路径；也可直接手写 `codemode.json`（`inlineBudget` 填表外任意非负整数亦可）。
**本扩展默认不启用**，先经 `/extension` 面板开启，`/codemode` 才可用。

## 与 pi 的差异

- `store()` 快照按**当前分支**的 `codemode-store` 条目重建（与 pi 一致），每轮脚本执行前整体替换；
  落盘走 TUI 的持久化请求——无 TUI（headless/SDK）时写入只留在进程内。
- `models.*` 全局齐备（目录查询 / `classify` / `generateImages`），但只有 `models` namespace，
  pi 的模型注册表扩展接口（`ctx.modelRegistry`）没有对应物。
- 无实时嵌套调用进度（结束后统一出现在 `details.nestedCalls`），TUI 也没有专门的 codemode 卡片。
- `bash` 等声明了 `outputSchema` 的工具在脚本里返回**结构化结果**（`output` 最大 1 MiB，另有 `exit_code`、`wall_time_seconds` 等字段）；面向模型的文本截断预算（50 KiB / 2000 行）不适用于这条路径。
