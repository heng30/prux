"use strict";

/*
 * glue.js — 工作流沙箱的 JS 侧全局。
 * 宿主在求值脚本体之前先求值本文件；本文件把 `agent`/`workflow`/`phase`/
 * `log`/`parallel`/`pipeline`/`args`/`budget`/`console` 与 `__settle` 挂到 globalThis。
 *
 * 与宿主的边界是 **JSON 字符串**：`__dispatch(id, method, json)` 出，`__settle(id, ok, json)` 入。宿主在求值前注入：
 *   - `__dispatch` / `__progress` / `__budgetSpent`（宿主函数）
 *   - `__argsJson`（string | undefined）、`__itemCap`（number）、`__boundaryCap`（number）
 */

// 整个文件包在一个 IIFE 里：内部状态（pending/openLaunches/ambient 等）保持私有，
// 只把脚本可见的 API 显式挂到 globalThis（文件末尾）。
(function () {
  // 一次 `parallel()`/`pipeline()` 能接受的 item 上限，由宿主注入。
  var ITEM_CAP = __itemCap;

  // 宿主调用 id 分配器：每次出站调用自增，宿主 settle 时原样回传用于配对。
  var nextCallId = 1;

  // id -> {resolve, reject}：出站调用在此等待宿主 `__settle` 回来。
  var pending = new Map();

  // 未 await/return 的 agent() 启动：脚本结束时若仍有挂着的，整次运行判失败。否则一次“丢了”的启动会静默消失。
  var openLaunches = new Map();

  // openLaunches 的 key 分配器（仅用于在报错里定位是哪几次启动）。
  var nextLaunchId = 1;

  // 出站调用的统一入口：把 (method, payload) JSON 编码后交给宿主 `__dispatch`，
  // 返回一个在宿主 `__settle(id, ...)` 时兑现/拒绝的 Promise。挂起状态记在 `pending`，由 [`__settle`] 消费。
  function callHost(method, payload) {
    return new Promise(function (resolve, reject) {
      var id = nextCallId++;
      pending.set(id, { resolve: resolve, reject: reject });
      __dispatch(
        id,
        method,
        JSON.stringify(payload === undefined ? null : payload),
      );
    });
  }

  // 登记一次 launch，并在其 settle 时注销。
  // `agent()` 走这里而不是直接 callHost，是为了让脚本结束时能检测出“丢了”的启动。
  function tracked(payload, label) {
    var token = nextLaunchId++;
    openLaunches.set(token, label);

    // `finally` 保证无论成功/失败/被 reject，登记都会摘掉。
    return callHost("agent", payload).finally(function () {
      openLaunches.delete(token);
    });
  }

  // 脚本结束时检查：还有未结的启动就返回一条失败消息（否则 null）。
  function __checkUnawaited() {
    if (openLaunches.size === 0) return null;

    var labels = [];
    openLaunches.forEach(function (label) {
      labels.push(label);
    });

    return (
      "workflow script completed with unawaited agent launch(es): " +
      labels.join(", ") +
      ". Await or return each launch."
    );
  }

  /* --------------------------- JSON 边界 --------------------------- */
  // 跨边界值校验：宿主边界是 JSON 字符串，而`JSON.stringify` 会**静默降级**一批值
  // （NaN/Infinity→null、Map/Set→{}、稀疏数组→null、函数/undefined/符号被丢弃），不报错。
  // 这些值写进 journal/progress 后无法可靠重放，所以在交付前显式拒绝，而不是写出一份醒来对不上的 journal。
  // 按路径构造统一的边界错误（错误消息里带上出问题的字段路径，便于定位）。
  function boundaryError(what, path) {
    return new Error(
      "Cannot pass " +
        what +
        " across the workflow VM boundary (at " +
        path +
        ").",
    );
  }

  // 递归校验：`seen` 记录当前递归路径上的对象，用于检测环
  // （进入时 add、离开时 delete，因此只有真正的环会命中，而不是共享引用）。
  function assertBoundary(value, path, seen) {
    if (value === null) return;

    var kind = typeof value;
    if (kind === "string" || kind === "boolean") return;
    if (kind === "number") {
      if (!Number.isFinite(value))
        throw boundaryError("a non-finite number", path);
      return;
    }

    if (kind === "undefined") {
      // 顶层 `return undefined` 是合法的（归一为 null），
      // 但嵌在数据里的 undefined 会在 JSON 里被丢弃，必须拒绝。
      if (path === "the workflow result") return;

      throw boundaryError("undefined", path);
    }
    if (kind === "bigint") throw boundaryError("a BigInt", path);
    if (kind === "symbol") throw boundaryError("a symbol", path);
    if (kind === "function") throw boundaryError("a function", path);
    if (kind !== "object") throw boundaryError("a " + kind, path);
    if (seen.has(value)) throw boundaryError("a circular structure", path);

    seen.add(value);

    if (Object.getOwnPropertySymbols(value).length > 0) {
      throw boundaryError("an object with symbol keys", path);
    }

    if (Array.isArray(value)) {
      var length = value.length;
      for (var i = 0; i < length; i++) {
        // 稀疏数组经 JSON round-trip 会变成 null，静默改变数据
        if (!Object.prototype.hasOwnProperty.call(value, i)) {
          throw boundaryError("a sparse array", path + "[" + i + "]");
        }

        assertBoundary(value[i], path + "[" + i + "]", seen);
      }

      seen.delete(value);
      return;
    }

    var prototype = Object.getPrototypeOf(value);

    // 只接受纯对象（`null` 原型或本 realm 的 `Object.prototype`）；
    // Map/Set/Date/类实例过去在 JSON 里会失去意义。
    if (prototype !== null && prototype !== Object.prototype) {
      throw boundaryError("a non-plain object", path);
    }

    var keys = Object.keys(value);
    for (var k = 0; k < keys.length; k++) {
      assertBoundary(value[keys[k]], path + "." + keys[k], seen);
    }

    seen.delete(value);
  }

  // 边界的总闸：先结构性校验，再做一次真实的 JSON.stringify（能捕获 getter 抛错等），
  // 最后检查序列化后的字节数是否超过 `__boundaryCap`。返回原值以便链式使用。
  function checkBoundary(value, path) {
    assertBoundary(value, path, new Set());

    var json;
    try {
      json = JSON.stringify(value);
    } catch (_) {
      throw boundaryError("a value JSON cannot encode", path);
    }

    if (json !== undefined && json.length > __boundaryCap) {
      throw new Error(
        "Cannot pass " +
          path +
          " across the workflow VM boundary: it serializes to " +
          json.length +
          " bytes, over the limit of " +
          __boundaryCap +
          ".",
      );
    }

    return value;
  }

  // 宿主驱动循环 settle：ok=true 时 json 是结果的 JSON 编码；
  // ok=false 时是`{"message": string, "fatal"?: bool}`。
  // 出站调用与这里通过 id 配对：找到 pending 的 waiter、摘除、按 ok 兑现或拒绝。
  function __settle(id, ok, json) {
    var waiter = pending.get(id);
    if (!waiter) return;
    pending.delete(id);

    if (ok) {
      var value = undefined;

      try {
        value = JSON.parse(json);
      } catch (_) {
        value = json; // 结果不是合法 JSON 时退化为原始字符串（纯文本结果的情况）。
      }

      waiter.resolve(value);
      return;
    }

    var fatal = false;
    var message = "The workflow host rejected the call.";

    try {
      var info = JSON.parse(json);
      if (info && typeof info.message === "string") message = info.message;
      fatal = !!(info && info.fatal);
    } catch (_) {
      if (typeof json === "string" && json) message = json;
    }

    var err = new Error(message);

    // `workflowFatal` 标记：parallel/pipeline 见到它会向上冒泡而不是吞成 null。
    if (fatal) err.workflowFatal = true;

    waiter.reject(err);
  }

  // 是否是“重试无意义”的致命错误（配额、未知 host 方法、schema 闸门等）。
  function isFatal(e) {
    return !!(e && typeof e === "object" && e.workflowFatal === true);
  }

  // 往共享进度日志追加一条（JSON 编码后交给宿主 `__progress`）。
  function emit(entry) {
    __progress(JSON.stringify(entry));
  }

  // 必填的非空字符串校验（返回原值便于内联使用）。
  function requireText(value, what) {
    if (typeof value !== "string" || value.trim() === "")
      throw new Error(what + " requires a non-empty string.");
    return value;
  }

  // 选填文本：`undefined`/`null` 归一为 `undefined`，其余必须是非空字符串。
  function optionalText(value, what) {
    if (value === undefined || value === null) return undefined;
    return requireText(value, what);
  }

  // 把任意值渲染成一条可读消息（错误对象取 `.message`，对象走 JSON）。
  function describe(value) {
    if (typeof value === "string") return value;
    if (value && typeof value === "object" && typeof value.message === "string")
      return value.message;

    try {
      var json = JSON.stringify(value);
      if (json !== undefined) return json;
    } catch (_) {}

    return String(value);
  }

  /* ----------------------------- phase / log ----------------------------- */
  // 标题 -> 阶段下标：同名 phase 复用同一个下标（阶段树不会因重名分裂）。
  var phaseIndexByTitle = new Map();

  // ambient（环境）阶段：`phase('X')` 会把后续未显式归组的 agent 都挂到 X 下。
  var ambient = { index: undefined, title: undefined };

  // 分配（或复用）一个阶段下标，并在首次出现时发一条阶段条目。
  function definePhase(title) {
    var idx = phaseIndexByTitle.get(title);
    if (idx !== undefined) return idx;

    idx = __allocPhase();
    phaseIndexByTitle.set(title, idx);

    emit({ type: "workflow_phase", index: idx, title: title });

    return idx;
  }

  // 设置环境阶段：之后未显式指定 `opts.phase` 的 agent 都归入这里。
  function phase(title) {
    var text = requireText(title, "phase(title)");
    ambient.index = definePhase(text);
    ambient.title = text;
  }

  // 脚本主动发一条进度日志。
  function log(message) {
    emit({ type: "workflow_log", message: describe(message) });
  }

  /* -------------------------------- agent -------------------------------- */
  // 允许的 `opts` 键（白名单）：不在这里、也不在 DEFERRED_OPTIONS 的键直接报错，避免拼写错误被静默忽略。
  var AGENT_OPTIONS = [
    "label",
    "phase",
    "model",
    "agentType",
    "effort",
    "schema",
    "gate",
    "resume",
    "isolation",
  ];

  // 已知但在此上下文不支持的选项：key -> 原因（用于给出更具体的报错）。
  var DEFERRED_OPTIONS = {};

  // 合法的 effort 档位。
  var EFFORT_LEVELS = ["minimal", "low", "medium", "high", "xhigh", "max"];

  // 派发一次子代理：校验入参 → 登记 launch → 交给宿主运行；返回值已跨边界。
  async function agent(prompt, opts) {
    // ---- 1. 基本入参与 opts 形状 ----
    var text = requireText(prompt, "agent(prompt)");
    var options = opts === undefined || opts === null ? {} : opts;

    if (typeof options !== "object" || Array.isArray(options)) {
      throw new Error("agent(prompt, opts) expects opts to be an object.");
    }

    // 白名单检查：未知键要么是已声明不支持的（给原因），要么是拼错了（列出支持的键）。
    for (var key of Object.keys(options)) {
      if (AGENT_OPTIONS.indexOf(key) !== -1) continue;

      var why = DEFERRED_OPTIONS[key];
      throw new Error(
        why !== undefined
          ? "agent() opts." + key + " is not supported here: " + why
          : "agent() opts." +
              key +
              " is not a recognised option. Supported: " +
              AGENT_OPTIONS.join(", ") +
              ".",
      );
    }

    // ---- 2. 逐项取出并做类型/取值校验 ----
    var label = optionalText(options.label, "agent() opts.label");
    var phaseName = optionalText(options.phase, "agent() opts.phase");
    var model = optionalText(options.model, "agent() opts.model");
    var agentType = optionalText(options.agentType, "agent() opts.agentType");
    var effort = optionalText(options.effort, "agent() opts.effort");
    var isolation = optionalText(options.isolation, "agent() opts.isolation");

    if (isolation !== undefined && isolation !== "worktree") {
      throw new Error('agent() opts.isolation must be "worktree".');
    }

    var gate = optionalText(options.gate, "agent() opts.gate");
    var resume = optionalText(options.resume, "agent() opts.resume");
    var schema = options.schema;

    if (
      schema !== undefined &&
      (typeof schema !== "object" || schema === null || Array.isArray(schema))
    ) {
      throw new Error("agent() opts.schema must be a JSON Schema object.");
    }

    // schema 本身也要跨边界（Map/Date 等会在 JSON 里变质）
    if (schema !== undefined) checkBoundary(schema, "agent() opts.schema");
    if (effort !== undefined && EFFORT_LEVELS.indexOf(effort) === -1) {
      throw new Error(
        "agent() opts.effort must be one of: " + EFFORT_LEVELS.join(", ") + ".",
      );
    }

    // resume 续的是**已经存在**的 child：它保留启动时定下的 agent 类型、模型、工具契约与工作树，
    // 所以任何描述"怎么启动一个"的选项都不该和它一起给。静默忽略它们会看起来像生效了，直接拒绝才是对的。
    if (resume !== undefined) {
      var EXCLUSIVE = {
        agentType: "an agent type",
        model: "a model",
        effort: "an effort level",
        isolation: "isolation",
        gate: "a gate",
        schema: "a schema",
      };

      for (var key in EXCLUSIVE) {
        if (options[key] !== undefined) {
          throw new Error(
            "agent() opts.resume and opts." +
              key +
              " are mutually exclusive: a resumed" +
              " child keeps " +
              EXCLUSIVE[key] +
              " from when it started.",
          );
        }
      }
    }

    // ---- 3. 决定归属阶段 ----
    var phaseIndex =
      phaseName !== undefined ? definePhase(phaseName) : ambient.index;
    var phaseTitle = phaseName !== undefined ? phaseName : ambient.title;

    // 缺少 label 时用 prompt 首行当显示名（与宿主 `derived_label` 口径一致）。
    var launchLabel =
      label !== undefined && label.trim() !== ""
        ? label
        : text.split("\n")[0] || "agent";

    // ---- 4. 登记并派发 ----
    var result = await tracked(
      {
        prompt: text,
        label: label,
        model: model,
        agentType: agentType,
        effort: effort,
        isolation: isolation,
        phaseIndex: phaseIndex,
        phaseTitle: phaseTitle,
        gate: gate,
        resume: resume,
        schema: schema,
      },
      launchLabel,
    );

    // 失败/跳过的调用宿主给 `null`；把 `undefined` 也归一为 `null`。
    return result === undefined ? null : result;
  }

  /* --------------------------- parallel / pipeline ------------------------ */
  // 把入参转成一个真正的数组：顺带检查是数组、不超过 ITEM_CAP，并复制一份（避免外部后续修改）。
  function toList(value, what) {
    if (!Array.isArray(value)) throw new Error(what + " expects an array.");

    var length = value.length >>> 0;
    if (length > ITEM_CAP) {
      throw new Error(
        what +
          " was given " +
          length +
          " items, over the limit of " +
          ITEM_CAP +
          ".",
      );
    }

    var out = [];
    for (var i = 0; i < length; i++) out.push(value[i]);
    return out;
  }

  // 并发跑一组 thunk（每个 thunk 通常包着一次 agent()）。
  // 单项普通失败 → 该项结果为 `null`（不拖垮整体）；致命错误（isFatal）则向上抛。
  async function parallel(thunks) {
    var list = toList(thunks, "parallel(thunks)");

    for (var i = 0; i < list.length; i++) {
      if (typeof list[i] !== "function") {
        throw new Error(
          "parallel(thunks) expects an array of functions; item " +
            i +
            " is not one.",
        );
      }
    }

    return Promise.all(
      list.map(async function (thunk) {
        try {
          return await thunk();
        } catch (e) {
          if (isFatal(e)) throw e;
          return null;
        }
      }),
    );
  }

  // 对每个 item 依次跑各 stage（前一个 stage 的输出作为下一个的输入），但**不同 item 之间并发**。
  // `stages[s](prev, item, index)` 收到：上一步结果、原始 item、下标。同样：单项普通失败 → `null`；致命错误向上抛。
  async function pipeline(items) {
    var stages = Array.prototype.slice.call(arguments, 1);
    var list = toList(items, "pipeline(items)");

    if (stages.length === 0)
      throw new Error(
        "pipeline(items, ...stages) requires at least one stage.",
      );

    for (var i = 0; i < stages.length; i++) {
      if (typeof stages[i] !== "function")
        throw new Error("pipeline() stage " + i + " is not a function.");
    }

    return Promise.all(
      list.map(async function (item, index) {
        var prev = item;

        for (var s = 0; s < stages.length; s++) {
          try {
            prev = await stages[s](prev, item, index);
          } catch (e) {
            if (isFatal(e)) throw e;
            return null;
          }
        }

        return prev;
      }),
    );
  }

  /* -------------------------------- workflow ------------------------------ */
  // `workflow(nameOrRef, args?)`：内联运行另一个具名/路径工作流（只允许一层）。
  // 失败的“未知名”应当是脚本可捕获的 throw；只有配额等致命错误带 workflowFatal。
  async function workflow(nameOrRef, args) {
    // ---- 1. 归一化 ref：字符串 = 名字；对象 = { name?, scriptPath? }，至少要有一个 ----
    var ref;
    if (typeof nameOrRef === "string") {
      if (nameOrRef.trim() === "")
        throw new Error("workflow(nameOrRef) expects a non-empty name.");
      ref = { name: nameOrRef };
    } else if (
      nameOrRef &&
      typeof nameOrRef === "object" &&
      !Array.isArray(nameOrRef)
    ) {
      var scriptPath = optionalText(
        nameOrRef.scriptPath,
        "workflow() scriptPath",
      );
      var name = optionalText(nameOrRef.name, "workflow() name");

      if (scriptPath === undefined && name === undefined) {
        throw new Error(
          "workflow({ ... }) expects a `name` or a `scriptPath`.",
        );
      }

      ref = { name: name, scriptPath: scriptPath };
    } else {
      throw new Error(
        "workflow(nameOrRef) expects a saved workflow name or { scriptPath }.",
      );
    }

    // 用于消息与错误文案的显示名（优先 name）。
    var label = ref.name !== undefined ? ref.name : ref.scriptPath;

    // args 也要跨边界：Map/Date/NaN 等会在 JSON 里静默变质
    if (args !== undefined)
      checkBoundary(args, 'workflow("' + label + '") args');

    // ---- 2. 交给宿主内联跑子脚本；普通错误包一层带上下文的报错，致命错误原样抛 ----
    var value;
    try {
      value = await callHost("workflow", {
        name: ref.name,
        scriptPath: ref.scriptPath,
        args: args === undefined ? null : args,
      });
    } catch (e) {
      if (isFatal(e)) throw e;
      throw new Error('workflow("' + label + '"): ' + describe(e));
    }

    // ---- 3. 子脚本的返回值同样要过边界 ----
    checkBoundary(value, 'the result of workflow("' + label + '")');
    return value === undefined ? null : value;
  }

  // `budget`：给脚本一个查询已花费 output token 的入口。
  // `total`/`remaining` 为占位（当前不设硬上限，故 remaining 恒为 Infinity）。
  var budget = {
    total: null,
    spent: function () {
      return __budgetSpent();
    },
    remaining: function () {
      return Infinity;
    },
  };

  // `args`：宿主注入的 JSON 字符串（可能缺省）→ 解析成对象给脚本用。
  var args =
    typeof __argsJson === "string" && __argsJson.length > 0
      ? JSON.parse(__argsJson)
      : undefined;

  // ---- 把 API 挂到全局：只有下面这些名字脚本才看得到 ----
  // 宿主内部拨号函数（前三个）供 wrap_body / 驱动循环调用。
  globalThis.__settle = __settle;
  globalThis.__checkUnawaited = __checkUnawaited;
  globalThis.__checkBoundary = checkBoundary;

  // 脚本可见的 API。
  globalThis.agent = agent;
  globalThis.workflow = workflow;
  globalThis.phase = phase;
  globalThis.parallel = parallel;
  globalThis.pipeline = pipeline;
  globalThis.args = args;
  globalThis.budget = budget;

  // `console` 填充：所有级别都落到同一条日志上。
  var consoleShim = { log: log, info: log, warn: log, error: log, debug: log };
  globalThis.log = log;
  globalThis.console = consoleShim;
})();
