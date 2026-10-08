#!/usr/bin/env python3
"""从本地 pi 安装目录同步模型数据到 assets/models。

pi 将每个 provider 的模型数据发布在 @earendil-works/pi-ai 包的
dist/providers/data/<provider>.json（结构为 { api: { "type:modelId": model } }）。
本脚本按 pi 的发布布局（`generate-models.ts` 的 `jsonOutputDir` 输出）写出两份目录：

    assets/models/models.json               # { provider: { modelId: 模型 } }，仅 chat（pi 的键式遗留目录）
    assets/models/models.all.json           # { provider: [模型, ...] }，含 chat/image/classifier
    assets/models/providers.json            # [providerId, ...]，排序
    assets/models/providers/<id>.json       # 单 provider 的键式 chat 目录
    assets/models/providers/<id>.all.json   # 单 provider 的数组式全类型目录

数组式目录里同一上游 ID 可按类型各有一条（`models.all.json` 是运行时读取的那份）；
键式目录只含 chat（pi 的遗留形态，供第三方按旧格式消费）。
每个 provider 内的顺序与 pi 一致：chat → image → classifier，同类内按 id 字典序。

仅同步 src/core/model_resolver.rs 中 SUPPORTED_PROVIDERS 声明的 provider。

用法:
    scripts/sync-models.py              # 同步（写文件）
    scripts/sync-models.py --check      # 只报告差异，不写文件
    scripts/sync-models.py --pi-ai-data <dir>   # 显式指定 pi-ai data 目录
"""
import argparse
import json
import os
import re
import shutil
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MODELS_DIR = os.path.join(ROOT, "assets", "models")
PROVIDERS_DIR = os.path.join(MODELS_DIR, "providers")
RESOLVER = os.path.join(ROOT, "src", "core", "model_resolver.rs")
SYNC_MARK = os.path.join(ROOT, "sync.md")

# pi 的模型类型；其他取值（将来新增的操作）不属于本次同步范围，整条丢弃
MODEL_TYPES = ("chat", "image", "classifier")

# pi-ai data 目录的候选位置：pi 可执行文件所在 npm 安装树 + 常见全局目录
CANDIDATES = [
    "node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-ai/dist/providers/data",
    "node_modules/@earendil-works/pi-ai/dist/providers/data",
    "node_modules/@mariozechner/pi-ai/dist/providers/data",
]


def find_pi_ai_data_dir():
    """依次尝试：--pi-ai-data > 环境变量 > pi 可执行树 > 常见全局路径。"""
    pi_bin = shutil.which("pi")
    if pi_bin:
        try:
            real = os.path.realpath(pi_bin)
        except OSError:
            real = pi_bin
        # 从 bin 所在目录开始向上逐级查找候选相对路径
        d = os.path.dirname(real)
        while True:
            for rel in CANDIDATES:
                p = os.path.join(d, rel)
                if os.path.isdir(p):
                    return p
            parent = os.path.dirname(d)
            if parent == d:
                break
            d = parent
    for base in ("/usr/local/lib", "/usr/lib", os.path.expanduser("~/.npm-global/lib"),
                 os.path.expanduser("~/.local/lib"), os.path.expanduser("~/node_modules")):
        for rel in CANDIDATES:
            p = os.path.join(base, rel)
            if os.path.isdir(p):
                return p
    return None


def supported_providers():
    text = open(RESOLVER, encoding="utf-8").read()
    m = re.search(r"SUPPORTED_PROVIDERS:\s*&\[&str\]\s*=\s*&\[(.*?)\];", text, re.S)
    if not m:
        sys.exit(f"无法在 {RESOLVER} 中解析 SUPPORTED_PROVIDERS")
    return re.findall(r'"([^"]+)"', m.group(1))


def pi_ai_version(data_dir):
    pkg = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(data_dir))), "package.json")
    try:
        return json.load(open(pkg, encoding="utf-8")).get("version", "?")
    except OSError:
        return "?"


def flatten(data_path):
    """展开 { api: { "type:id": model } }：同类型内按 id 去重（先见者胜，与 pi 一致）。"""
    raw = json.load(open(data_path, encoding="utf-8"))
    buckets = {t: {} for t in MODEL_TYPES}
    for inner in raw.values():
        for model in inner.values():
            bucket = buckets.get(model.get("type"))
            if bucket is None:
                continue
            bucket.setdefault(model["id"], model)
    return buckets


def catalog_for(buckets):
    """单 provider 的数组式目录：chat → image → classifier，同类内按 id 排序。"""
    out = []
    for t in MODEL_TYPES:
        out.extend(buckets[t][k] for k in sorted(buckets[t]))
    return out


def chat_catalog(models):
    """键式遗留目录（仅 chat，键为模型 id）。"""
    return {m["id"]: m for m in models if m.get("type") == "chat"}


def serialize(value):
    return json.dumps(value, ensure_ascii=False, indent=2) + "\n"


def desired_files(data_dir, providers):
    """本次同步期望的 { 相对 assets/models 的路径: 文件内容 }。"""
    files = {}
    all_providers = {}
    for prov in providers:
        buckets = flatten(os.path.join(data_dir, prov + ".json"))
        models = catalog_for(buckets)
        all_providers[prov] = models
        files[f"providers/{prov}.json"] = serialize(chat_catalog(models))
        files[f"providers/{prov}.all.json"] = serialize(models)
    files["providers.json"] = serialize(sorted(providers))
    files["models.json"] = serialize({p: chat_catalog(m) for p, m in sorted(all_providers.items())})
    files["models.all.json"] = serialize(dict(sorted(all_providers.items())))
    return files


def current_files():
    """assets/models 下已有的对应文件（只有本脚本管辖的路径）。"""
    out = {}
    for rel in ("models.json", "models.all.json", "providers.json"):
        p = os.path.join(MODELS_DIR, rel)
        if os.path.exists(p):
            out[rel] = open(p, encoding="utf-8").read()
    if os.path.isdir(PROVIDERS_DIR):
        for name in sorted(os.listdir(PROVIDERS_DIR)):
            if name.endswith(".json"):
                out[f"providers/{name}"] = open(os.path.join(PROVIDERS_DIR, name), encoding="utf-8").read()
    return out


def stale_files():
    """assets/models 下旧布局的残留文件（`models.<provider>.json` 键式单文件）。"""
    keep = {"models.json", "models.all.json"}
    return [
        name
        for name in sorted(os.listdir(MODELS_DIR))
        if name not in keep and re.fullmatch(r"models\..+\.json", name)
    ]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="只报告差异，不写文件")
    ap.add_argument("--pi-ai-data", metavar="DIR", help="显式指定 pi-ai data 目录")
    args = ap.parse_args()

    data_dir = args.pi_ai_data or os.environ.get("PI_AI_DATA") or find_pi_ai_data_dir()
    if not data_dir or not os.path.isdir(data_dir):
        sys.exit("未找到 pi-ai 模型数据目录；请安装 pi 或使用 --pi-ai-data 指定")

    providers = supported_providers()
    missing = [p for p in providers if not os.path.exists(os.path.join(data_dir, p + ".json"))]
    if missing:
        sys.exit(f"pi-ai data 缺少以下 provider 文件: {', '.join(missing)}")

    ver = pi_ai_version(data_dir)
    sync_ver = open(SYNC_MARK, encoding="utf-8").read().strip() if os.path.exists(SYNC_MARK) else ""
    if sync_ver and ver != sync_ver:
        print(f"提示: sync.md 记录 {sync_ver}，pi-ai 为 {ver}（如需对齐请更新 sync.md）")

    wanted = desired_files(data_dir, providers)
    have = current_files()
    stale = stale_files()

    for rel in sorted(wanted):
        old = have.get(rel)
        if old is None:
            print(f"[+] {rel}")
        elif old != wanted[rel]:
            try:
                old_ids = {m["id"] for m in json.loads(old)} if old.lstrip().startswith("[") else None
            except (ValueError, TypeError, KeyError):
                old_ids = None
            delta = ""
            if old_ids is not None:
                new_ids = {m["id"] for m in json.loads(wanted[rel])}
                if new_ids != old_ids:
                    delta = f" (+{len(new_ids - old_ids)}/-{len(old_ids - new_ids)})"
            print(f"[~] {rel}{delta}")
        elif rel == "models.all.json":
            print(f"[=] {rel}: 无变化")
    for rel in stale:
        print(f"[-] {rel}")

    changed = [rel for rel in wanted if have.get(rel) != wanted[rel]]
    if args.check:
        print(f"\npi-ai {ver} · {len(providers)} 个 provider · check 模式："
              f"{len(changed)} 个文件需更新，{len(stale)} 个文件需删除")
        if changed or stale:
            sys.exit(1)
        return

    for rel in stale:
        os.remove(os.path.join(MODELS_DIR, rel))
    for rel in changed:
        path = os.path.join(MODELS_DIR, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            f.write(wanted[rel])
    print(f"\npi-ai {ver} · {len(providers)} 个 provider · "
          f"{len(changed)} 个文件更新，{len(stale)} 个文件删除")


if __name__ == "__main__":
    main()
