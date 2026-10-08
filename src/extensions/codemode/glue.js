// codemode 沙箱的 JS 预置（rquickjs 版）。
//
// 由宿主注入的四个全局函数 `__call(id, kind, name, argsJson)` / `__output(kind, a, b)` / `__done(ok, payload, writes)`。
// 另有宿主提供的 `stalled()` 检测在 Rust 侧做（见 sandbox.rs），故这里不再返回 `stalled`。
//
// `__call` / `__output` / `__done` 是宿主注入的全局函数（见 sandbox.rs）；本文件被
// `include_str!` 进 Rust，对 4 个参数的工厂函数求值，工厂返回 `{ settle, run, stalled }`，
// 并把 `__settle` 装到 globalThis 上（宿主每轮取它调一次，不跨 await 持有）。
(function (toolsJson, globalsJson, storeJson, limitsJson) {
  "use strict";
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const promiseThen = Promise.prototype.then;
  const ErrorCtor = Error;
  const TypeErrorCtor = TypeError;
  const RangeErrorCtor = RangeError;
  const pending = new Map();
  let nextId = 1;
  let finished = false;
  const EXIT = Object.freeze({});

  const limits = parse(limitsJson);
  const MAX_STORE_VALUE_CHARS = limits.maxStoreValueChars;
  const MAX_STORE_TOTAL_CHARS = limits.maxStoreTotalChars;
  const MAX_OUTPUT_CHARS = limits.maxOutputChars;
  const MAX_OUTPUT_ITEMS = limits.maxOutputItems;
  const IMAGE_HELPER_EXPECTS = limits.imageHelperExpects;

  function done(ok, payload, writes) {
    if (finished) return;
    finished = true;
    __done(
      ok,
      payload === undefined ? "" : payload,
      writes === undefined ? "" : writes,
    );
  }

  function serialize(value) {
    return value === undefined ? undefined : stringify(value);
  }

  // QuickJS 的 stack 只有帧；像 V8 一样补上 "Name: message" 头，并丢掉本预置的帧。
  function errorText(error) {
    const head = error.message
      ? error.name + ": " + error.message
      : String(error.name);
    const frames =
      typeof error.stack === "string"
        ? error.stack
            .split("\n")
            .filter((line) => line.trim() && !line.includes("codemode-glue.js"))
        : [];
    return [head, ...frames].join("\n");
  }

  function format(value) {
    if (typeof value === "string") return value;
    if (value instanceof ErrorCtor) return errorText(value);
    try {
      const json = stringify(value);
      return json === undefined ? String(value) : json;
    } catch {
      return String(value);
    }
  }

  function describeError(error) {
    if (error instanceof ErrorCtor) {
      return stringify({
        name: error.name,
        message: error.message,
        stack: errorText(error),
      });
    }
    return stringify({ message: format(error) });
  }

  let outputChars = 0;
  let outputItems = 0;

  // 超过输出上限时脚本失败：done() 先报错，因此捕获它也无法继续输出（与 pi 一致）。
  function output(kind, data, mimeType) {
    if (finished) return;
    outputChars += data.length;
    outputItems++;
    if (outputChars > MAX_OUTPUT_CHARS || outputItems > MAX_OUTPUT_ITEMS) {
      const error = new RangeErrorCtor(
        "script output exceeded the limit of " +
          MAX_OUTPUT_CHARS +
          " characters or " +
          MAX_OUTPUT_ITEMS +
          " text(), image(), and console calls. Print a summary instead, or write large data to a file with a tool.",
      );
      done(false, describeError(error));
      throw error;
    }
    __output(kind, data, mimeType);
  }

  function caller(kind, name, spread) {
    return (...args) =>
      new Promise((resolve, reject) => {
        let json;
        try {
          json = serialize(spread ? args : args[0]);
        } catch (error) {
          reject(error);
          return;
        }
        const id = nextId++;
        pending.set(id, { resolve, reject });
        __call(id, kind, name, json === undefined ? "" : json);
      });
  }

  const tools = Object.create(null);
  const allTools = [];
  for (const { name, jsName, description } of parse(toolsJson)) {
    const fn = caller("call", name);
    // 两个工具名归一成同一标识符时，第一个赢。
    if (!(jsName in tools)) {
      tools[jsName] = fn;
      allTools.push(Object.freeze({ name: jsName, description }));
    }
    if (!(name in tools)) tools[name] = fn;
  }
  Object.freeze(tools);
  Object.freeze(allTools);

  // 读一个不存在的成员时抛错并点名近似名，而不是返回 undefined 让调用方稍后报
  // "not a function"；`in` 探测照常可用。
  function comparable(name) {
    return name.toLowerCase().replace(/[^a-z0-9]/g, "");
  }

  function guard(target, label, names, hint) {
    return new Proxy(target, {
      get(object, property, receiver) {
        if (
          typeof property !== "string" ||
          property in object ||
          property in Object.prototype ||
          property === "then" ||
          property === "toJSON"
        ) {
          return Reflect.get(object, property, receiver);
        }
        const wanted = comparable(property);
        const exact = names.filter((name) => comparable(name) === wanted);
        const close =
          exact.length > 0
            ? exact
            : names.filter(
                (name) =>
                  wanted &&
                  (comparable(name).includes(wanted) ||
                    wanted.includes(comparable(name))),
              );
        let message = label + "." + property + " does not exist.";
        if (close.length > 0) {
          message +=
            " Did you mean " +
            close
              .slice(0, 5)
              .map((name) => label + "." + name)
              .join(", ") +
            "?";
        } else if (names.length <= 20) {
          message += " Available: " + names.join(", ") + ".";
        }
        if (hint) message += " " + hint;
        message +=
          ' Check for a member with "' + property + '" in ' + label + ".";
        throw new TypeErrorCtor(message);
      },
    });
  }

  const toolsProxy = guard(
    tools,
    "tools",
    allTools.map((tool) => tool.name),
    "ALL_TOOLS lists every tool; searchTools(query) finds tools by topic.",
  );

  const namespaces = new Map();
  for (const { name, spread } of parse(globalsJson)) {
    const fn = caller("global", name, spread);
    const dot = name.indexOf(".");
    if (dot === -1) {
      Object.defineProperty(globalThis, name, { value: fn, enumerable: true });
      continue;
    }
    const namespace = name.slice(0, dot);
    if (!namespaces.has(namespace))
      namespaces.set(namespace, Object.create(null));
    namespaces.get(namespace)[name.slice(dot + 1)] = fn;
  }
  for (const [namespace, members] of namespaces) {
    Object.freeze(members);
    const value = guard(members, namespace, Object.keys(members));
    Object.defineProperty(globalThis, namespace, {
      value,
      enumerable: true,
    });
  }

  // key -> JSON 文本；体积按 key 与 JSON 文本的字符数计。
  const STORE_HINT =
    "store() is for small state such as IDs or summaries. Show images with image(), keep large data in variables, or write it to a file with a tool.";
  const stored = new Map(Object.entries(parse(storeJson)));
  const writes = new Map();
  let storedChars = 0;
  for (const [key, json] of stored) storedChars += key.length + json.length;

  function checkKey(name, key) {
    if (typeof key !== "string")
      throw new TypeErrorCtor(name + "() key must be a string");
  }

  function store(key, value) {
    checkKey("store", key);
    const previous = stored.has(key) ? key.length + stored.get(key).length : 0;
    if (value === undefined) {
      stored.delete(key);
      storedChars -= previous;
      writes.set(key, undefined);
      return;
    }
    let json;
    try {
      json = stringify(value);
    } catch (error) {
      throw new TypeErrorCtor(
        "store(" +
          stringify(key) +
          ") value is not JSON-serializable: " +
          format(error),
      );
    }
    if (json === undefined) {
      throw new TypeErrorCtor(
        "store(" + stringify(key) + ") value is not JSON-serializable",
      );
    }
    if (json.length > MAX_STORE_VALUE_CHARS) {
      throw new RangeErrorCtor(
        "store(" +
          stringify(key) +
          ") value has " +
          json.length +
          " characters of JSON, more than the limit of " +
          MAX_STORE_VALUE_CHARS +
          ". " +
          STORE_HINT,
      );
    }
    const next = storedChars - previous + key.length + json.length;
    if (next > MAX_STORE_TOTAL_CHARS) {
      throw new RangeErrorCtor(
        "store is full: stored values would exceed " +
          MAX_STORE_TOTAL_CHARS +
          " characters of JSON. Delete keys with store(key, undefined). " +
          STORE_HINT,
      );
    }
    stored.set(key, json);
    storedChars = next;
    writes.set(key, json);
  }

  function load(key) {
    checkKey("load", key);
    const json = stored.get(key);
    return json === undefined ? undefined : parse(json);
  }

  function serializeWrites() {
    const entries = [];
    for (const [key, json] of writes)
      entries.push(json === undefined ? [key] : [key, json]);
    return stringify(entries);
  }

  Object.defineProperty(globalThis, "store", {
    value: store,
    enumerable: true,
  });
  Object.defineProperty(globalThis, "load", { value: load, enumerable: true });

  // 基本类型取字符串形式，其余 JSON 化。
  function outputText(value) {
    if (
      value === undefined ||
      value === null ||
      (typeof value !== "object" && typeof value !== "function")
    ) {
      return String(value);
    }
    const json = stringify(value);
    return json === undefined ? String(value) : json;
  }

  function text(value) {
    let rendered;
    try {
      rendered = outputText(value);
    } catch (error) {
      throw new TypeErrorCtor(
        error instanceof ErrorCtor ? error.message : String(error),
      );
    }
    if (!finished) output("text", rendered, "");
  }

  function imageUrl(value) {
    if (typeof value === "string") return value;
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
      throw new TypeErrorCtor(IMAGE_HELPER_EXPECTS);
    }
    if (value.image_url !== undefined) {
      if (typeof value.image_url !== "string")
        throw new TypeErrorCtor(IMAGE_HELPER_EXPECTS);
      return value.image_url;
    }
    if (typeof value.type !== "string")
      throw new TypeErrorCtor(IMAGE_HELPER_EXPECTS);
    if (value.type !== "image") {
      throw new TypeErrorCtor(
        'image only accepts MCP image blocks, got "' + value.type + '"',
      );
    }
    if (typeof value.data !== "string" || value.data === "")
      throw new TypeErrorCtor("image expected MCP image data");
    if (value.data.toLowerCase().startsWith("data:")) return value.data;
    const mimeType =
      typeof value.mimeType === "string" && value.mimeType
        ? value.mimeType
        : "application/octet-stream";
    return "data:" + mimeType + ";base64," + value.data;
  }

  // 仅接受 provider 能内联的格式（PNG、非 JPEG-LS 的 JPEG、GIF、"RIFF....WEBP"）的 base64 签名。
  // 签名从字节 0 开始，因此其 base64 编码就是前缀。
  const IMAGE_SIGNATURES = [
    ["image/png", /^iVBORw0KGg/],
    ["image/jpeg", /^[/]9j[/](?!9)/],
    ["image/gif", /^R0lGOD[dl]h/],
    ["image/webp", /^UklG.{8}RUJQ/],
  ];

  function image(value) {
    const url = imageUrl(value);
    if (url === "") throw new TypeErrorCtor(IMAGE_HELPER_EXPECTS);
    const colon = url.indexOf(":");
    const scheme = colon === -1 ? "" : url.slice(0, colon).toLowerCase();
    if (scheme === "http" || scheme === "https") {
      throw new TypeErrorCtor(
        "remote image URLs are not supported in tool outputs. Pass a base64 data URI instead",
      );
    }
    const comma = url.indexOf(",");
    const header = comma === -1 ? [] : url.slice(colon + 1, comma).split(";");
    if (
      scheme !== "data" ||
      comma === -1 ||
      header.slice(1).every((part) => part.toLowerCase() !== "base64")
    ) {
      throw new TypeErrorCtor(
        "invalid image output. Pass a base64 data URI instead",
      );
    }
    // provider 会因为坏图拒绝整个请求，而持久化的 image block 会在之后每轮重发。
    // 丢弃换行包裹的 base64；忽略声明的 type，改用签名探测到的 type（provider 也拒绝不匹配）。
    const data = url.slice(comma + 1).replace(/\s+/g, "");
    if (data.length % 4 !== 0 || !/^[A-Za-z0-9+/]+={0,2}$/.test(data)) {
      throw new TypeErrorCtor(
        "invalid image output. The image data is not valid base64 (truncated or corrupted?)",
      );
    }
    const head = data.slice(0, 16);
    const signature = IMAGE_SIGNATURES.find(([, pattern]) =>
      pattern.test(head),
    );
    if (!signature) {
      throw new TypeErrorCtor(
        "invalid image output. The image data is not a PNG, JPEG, GIF, or WebP image",
      );
    }
    output("image", data, signature[0]);
  }

  function exit() {
    let writesJson;
    try {
      writesJson = serializeWrites();
    } catch (error) {
      done(false, describeError(error));
      throw EXIT;
    }
    done(true, undefined, writesJson);
    throw EXIT;
  }

  const console = {};
  for (const level of ["log", "info", "warn", "error", "debug"]) {
    console[level] = (...args) => {
      output("text", args.map(format).join(" "), "");
    };
  }
  Object.freeze(console);

  Object.defineProperty(globalThis, "tools", {
    value: toolsProxy,
    enumerable: true,
  });
  Object.defineProperty(globalThis, "ALL_TOOLS", {
    value: allTools,
    enumerable: true,
  });
  Object.defineProperty(globalThis, "console", {
    value: console,
    enumerable: true,
  });
  Object.defineProperty(globalThis, "text", { value: text, enumerable: true });
  Object.defineProperty(globalThis, "image", {
    value: image,
    enumerable: true,
  });
  Object.defineProperty(globalThis, "exit", { value: exit, enumerable: true });

  const api = {
    settle(id, ok, payload) {
      const entry = pending.get(id);
      if (!entry) return;
      pending.delete(id);
      if (!ok) {
        entry.reject(new ErrorCtor(payload));
        return;
      }
      let value;
      try {
        value = payload === "" ? undefined : parse(payload);
      } catch (error) {
        entry.reject(error);
        return;
      }
      entry.resolve(value);
    },
    run(fn) {
      let promise;
      try {
        promise = fn(toolsProxy, console);
      } catch (error) {
        done(false, describeError(error));
        return;
      }
      promiseThen.call(
        promise,
        (value) => {
          let json;
          try {
            json = serialize(value);
          } catch (error) {
            done(false, describeError(error));
            return;
          }
          done(true, json, serializeWrites());
        },
        (error) => {
          done(false, describeError(error));
        },
      );
    },
    // 脚本是否还在等一个永远不能 settle 的 promise（没有工具调用在飞）：
    // 无定时器、无 I/O，只有微任务；宿主据此把脚本判成卡死。
    stalled() {
      if (finished || pending.size > 0) return false;
      done(
        false,
        stringify({
          name: "Error",
          message:
            "The script is waiting on a promise that can never settle: no tool call is pending, and timers do not exist here.",
        }),
      );
      return true;
    },
  };

  Object.defineProperty(globalThis, "__settle", {
    value: (id, ok, payload) => api.settle(id, ok, payload),
    enumerable: false,
  });

  return api;
});
