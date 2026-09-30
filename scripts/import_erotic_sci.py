#!/usr/bin/env python3
"""把 erotic_sci 里现有的 LM Studio / ComfyUI 配置导入 home-ai-center 网关。

设计约束
--------
1. **脚本里没有任何明文密钥**。地址、key、ComfyUI 口令全部在运行时从
   erotic_sci 读出来，所以这个文件可以安全提交 git。
2. **幂等**。按名字 upsert：已存在就更新，不存在才新建。可以反复跑。
3. **走管理 API 而不是直接写 SQLite**。校验、快照重建都在服务端做，
   绕过它就会出现「库里改了但内存快照没变」。

用法
----
    # 需要一个能 import yaml 的解释器（erotic_sci 自带的 venv 即可）
    /path/to/erotic_sci/.venv/bin/python scripts/import_erotic_sci.py \\
        --password "$HOME_AI_ADMIN_TOKEN"

    # 只看要写什么，不落库
    ... --dry-run

网关必须已经跑起来，且用 HOME_AI_ADMIN_TOKEN 初始化过管理员。
"""

from __future__ import annotations

import argparse
import http.cookiejar
import json
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

try:
    import yaml
except ImportError:  # pragma: no cover
    sys.exit("需要 pyyaml：python -m pip install pyyaml（或直接用 erotic_sci 的 .venv 解释器）")

MICRO_PER_YUAN = 1_000_000

# 模板里出现这些槽位，说明需要网关先把图片传上去。
# 网关目前只做「提交 workflow + 轮询取图」，没有 /upload/image，
# 所以这类工作流导入后一律停用 —— 停了不会误报可用，启用了必然失败。
NEEDS_UPLOAD_SLOTS = {"image", "init_image", "image_url"}


# ── HTTP ────────────────────────────────────────────────────────────────────


class Client:
    """极简管理 API 客户端。会话靠 Cookie，登录一次就够。"""

    def __init__(self, base_url: str) -> None:
        self.base = base_url.rstrip("/")
        self.jar = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(self.jar)
        )

    def _call(self, method: str, path: str, body: Any = None) -> Any:
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(
            f"{self.base}{path}",
            data=data,
            method=method,
            headers={"content-type": "application/json"} if data else {},
        )
        try:
            with self.opener.open(req, timeout=30) as resp:
                raw = resp.read().decode()
                return json.loads(raw) if raw.strip() else {}
        except urllib.error.HTTPError as e:
            detail = e.read().decode(errors="replace")[:400]
            raise SystemExit(f"✗ {method} {path} → HTTP {e.code}\n  {detail}") from e
        except urllib.error.URLError as e:
            raise SystemExit(
                f"✗ 连不上网关 {self.base}：{e.reason}\n"
                f"  先把网关跑起来，或用 --base-url 指定地址。"
            ) from e

    def login(self, username: str, password: str) -> None:
        self._call("POST", "/api/admin/auth/login",
                   {"username": username, "password": password})

    def get(self, path: str) -> Any:
        return self._call("GET", path)

    def post(self, path: str, body: Any) -> Any:
        return self._call("POST", path, body)

    def put(self, path: str, body: Any) -> Any:
        return self._call("PUT", path, body)


# ── 源配置读取 ────────────────────────────────────────────────────────────────


def read_env(path: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        v = v.strip()
        if len(v) >= 2 and v[0] == v[-1] and v[0] in "\"'":
            v = v[1:-1]
        out[k.strip()] = v
    return out


def trim_url(url: str) -> str:
    """只去掉尾部斜杠，**保留路径前缀**。

    网关的约定和 OpenAI SDK 一致：`base_url` 是「端点前缀」，
    网关只在其后拼 `/chat/completions`。所以 LM Studio 必须填
    `http://host:7001/v1`，填成 `http://host:7001` 会变成
    `POST /chat/completions` —— 而 LM Studio 对错误路径回的是
    **200 + `{"error": "Unexpected endpoint..."}`**，不报 404。
    """
    return url.strip().rstrip("/")


PLACEHOLDER = re.compile(r"\{\{\s*([^}]*?)\s*\}\}")


def template_slots(doc: Any) -> list[str]:
    """扫出模板里用到的占位符名。

    参数槽从模板**推**出来而不是手写：手写的槽位表迟早和模板对不上，
    而对不上的后果是「传了参数却被拒」或「设了 seed 却没生效」。
    """
    found: set[str] = set()

    def walk(n: Any) -> None:
        if isinstance(n, dict):
            for v in n.values():
                walk(v)
        elif isinstance(n, list):
            for v in n:
                walk(v)
        elif isinstance(n, str):
            for m in PLACEHOLDER.findall(n):
                name = m.split("|")[0].strip()
                if name:
                    found.add(name)

    walk(doc)
    return sorted(found)


def load_source(src: Path) -> dict[str, Any]:
    ingest = yaml.safe_load((src / "config" / "ingest.yaml").read_text(encoding="utf-8"))
    remotes = ingest["llm"]["lmstudio"]["remotes"]
    env = read_env(src / ".env")

    workflows = []
    for f in sorted((src / "config" / "comfyui").glob("*.json")):
        text = f.read_text(encoding="utf-8")
        doc = json.loads(text)
        slots = template_slots(doc)
        workflows.append(
            {
                "name": f.stem,
                "json": text,
                "param_slots": slots,
                # 需要上传图片的槽位，网关目前满足不了。
                "blocked": sorted(NEEDS_UPLOAD_SLOTS & set(slots)),
            }
        )

    return {
        "remotes": remotes,
        "comfy_base_url": env.get("COMFYUI_BASE_URL", ""),
        "comfy_username": env.get("COMFYUI_USERNAME", ""),
        "comfy_password": env.get("COMFYUI_PASSWORD", ""),
        "workflows": workflows,
    }


# ── 计划 ────────────────────────────────────────────────────────────────────


def build_plan(cfg: dict[str, Any], args: argparse.Namespace) -> dict[str, Any]:
    nodes, routes = [], []
    seen_model_priority: dict[str, int] = {}

    for idx, r in enumerate(cfg["remotes"]):
        name = str(r["name"])
        window = r.get("disabled_hours") or {}
        extra_body = {}
        if r.get("reasoning_effort"):
            extra_body["reasoning_effort"] = r["reasoning_effort"]

        nodes.append(
            {
                "name": name,
                "kind": "lmstudio",
                "base_url": trim_url(r["base_url"]),
                "lan_base_url": (
                    trim_url(r["lan_base_url"]) if r.get("lan_base_url") else None
                ),
                "max_concurrency": int(r.get("max_concurrency", 2)),
                "default_max_output_tokens": None,
                # m2 当前不可达：配置留着，节点先停用，机器起来后在管理台点一下启用。
                "enabled": name not in set(args.disable_nodes),
                "sort_order": idx,
                "disabled_start": window.get("start"),
                "disabled_end": window.get("end"),
                "disabled_timezone": window.get("timezone"),
                "extra_headers": {},
                "extra_body": extra_body,
                "capabilities": {},
                "_key": str(r.get("api_key") or ""),
            }
        )

        model = str(r["model"])
        prio = seen_model_priority.get(model, 0)
        seen_model_priority[model] = prio + 1
        routes.append(
            {
                "model_name": model,
                "_node": name,
                "upstream_model": model,
                # 路由本身保持启用：节点停用时它压根不会被选中，
                # 这样在管理台启用节点就能直接生效，不用再来补一条路由。
                "enabled": True,
                "priority": prio,
            }
        )

    return {
        "nodes": nodes,
        "routes": routes,
        "prices": [
            {
                "model_name": m,
                "input_micro_per_1k": args.price_micro_per_1k,
                "output_micro_per_1k": args.price_micro_per_1k,
                "cached_input_micro_per_1k": None,
            }
            for m in sorted(seen_model_priority)
        ],
        "comfy_nodes": (
            [
                {
                    "name": args.comfy_node_name,
                    "base_url": cfg["comfy_base_url"].rstrip("/"),
                    "lan_base_url": None,
                    "username": cfg["comfy_username"],
                    "password": cfg["comfy_password"],
                    "max_concurrency": args.comfy_max_concurrency,
                    "enabled": True,
                    "sort_order": 0,
                    "disabled_start": None,
                    "disabled_end": None,
                    "disabled_timezone": None,
                }
            ]
            if cfg["comfy_base_url"]
            else []
        ),
        "workflows": [
            {
                "name": w["name"],
                "mode": "template",
                "comfy_workflow": w["json"],
                "param_slots": w["param_slots"],
                "price_micro": args.image_price_micro,
                "enabled": not w["blocked"],
                "archive": False,
                "archive_retention_days": None,
                "_blocked": w["blocked"],
            }
            for w in cfg["workflows"]
        ],
        "account": {
            "name": args.account_name,
            "balance_micro": int(args.account_topup_yuan * MICRO_PER_YUAN),
            "llm_max_concurrency": args.account_llm_concurrency,
            "llm_max_queue": args.account_llm_queue,
            "image_max_concurrency": args.comfy_max_concurrency,
            "image_max_queue": 8,
        },
    }


# ── 应用 ────────────────────────────────────────────────────────────────────


class Report:
    def __init__(self) -> None:
        self.created: list[str] = []
        self.updated: list[str] = []
        self.skipped: list[str] = []
        self.notes: list[str] = []
        self.secrets: list[tuple[str, str]] = []

    def line(self, verb: str, what: str) -> None:
        print(f"  {verb} {what}")


def apply_plan(cli: Client, plan: dict[str, Any], rep: Report) -> None:
    existing_nodes = {n["name"]: n for n in cli.get("/api/admin/nodes")["items"]}
    node_ids: dict[str, int] = {}

    for n in plan["nodes"]:
        body = {k: v for k, v in n.items() if not k.startswith("_")}
        if n["name"] in existing_nodes:
            nid = existing_nodes[n["name"]]["id"]
            cli.put(f"/api/admin/nodes/{nid}", body)
            node_ids[n["name"]] = nid
            rep.updated.append(f"节点 {n['name']}")
            rep.line("更新", f"节点 {n['name']}  ({n['base_url']}, 并发 {n['max_concurrency']}"
                             f"{'' if n['enabled'] else '，停用'})")
        else:
            nid = cli.post("/api/admin/nodes", body)["id"]
            node_ids[n["name"]] = nid
            rep.created.append(f"节点 {n['name']}")
            rep.line("新建", f"节点 {n['name']}  ({n['base_url']}, 并发 {n['max_concurrency']}"
                             f"{'' if n['enabled'] else '，停用'})")

        # key
        keys = cli.get(f"/api/admin/nodes/{nid}/keys")["items"]
        hit = next((k for k in keys if k["label"] == "primary"), None)
        if hit is not None:
            rep.skipped.append(f"key {n['name']}/primary")
            rep.line("跳过", f"节点 {n['name']} 的 key（已存在，避免覆盖线上正在用的那把）")
        else:
            cli.post(
                f"/api/admin/nodes/{nid}/keys",
                {"secret": n["_key"], "label": "primary", "sort_order": 0,
                 "enabled": True, "rate_limit_scope": "perKey"},
            )
            rep.created.append(f"key {n['name']}/primary")
            rep.line("新建", f"节点 {n['name']} 的 key")

    for r in plan["routes"]:
        body = {
            "model_name": r["model_name"],
            "node_id": node_ids[r["_node"]],
            "upstream_model": r["upstream_model"],
            "enabled": r["enabled"],
            "priority": r["priority"],
        }
        cli.post("/api/admin/routes", body)
        tag = "主" if r["priority"] == 0 else f"备{r['priority']}"
        rep.line("路由", f"{r['model_name']} → {r['_node']}（{tag}）")

    for p in plan["prices"]:
        cli.post("/api/admin/prices", p)
        rep.line("定价", f"{p['model_name']}  入 {p['input_micro_per_1k']} / 出 "
                         f"{p['output_micro_per_1k']} 微元每 1k")

    comfy_ids: list[int] = []
    existing_comfy = {c["name"]: c for c in cli.get("/api/admin/comfy-nodes")["items"]}
    for c in plan["comfy_nodes"]:
        body = {k: v for k, v in c.items() if not k.startswith("_")}
        if c["name"] in existing_comfy:
            cid = existing_comfy[c["name"]]["id"]
            cli.put(f"/api/admin/comfy-nodes/{cid}", body)
            rep.updated.append(f"ComfyUI 端点 {c['name']}")
            rep.line("更新", f"ComfyUI 端点 {c['name']}  ({c['base_url']})")
        else:
            cid = cli.post("/api/admin/comfy-nodes", body)["id"]
            rep.created.append(f"ComfyUI 端点 {c['name']}")
            rep.line("新建", f"ComfyUI 端点 {c['name']}  ({c['base_url']}, "
                             f"Basic Auth 用户 {c['username'] or '无'})")
        comfy_ids.append(cid)

    existing_wf = {w["name"]: w for w in cli.get("/api/admin/workflows")["items"]}
    for w in plan["workflows"]:
        body = {k: v for k, v in w.items() if not k.startswith("_")}
        body["node_ids"] = comfy_ids
        if w["name"] in existing_wf:
            cli.put(f"/api/admin/workflows/{existing_wf[w['name']]['id']}", body)
            rep.updated.append(f"工作流 {w['name']}")
        else:
            cli.post("/api/admin/workflows", body)
            rep.created.append(f"工作流 {w['name']}")
        slots = "、".join(w["param_slots"])
        mark = "" if w["enabled"] else f"  停用（需要 {', '.join(w['_blocked'])} 槽，网关不能上传）"
        rep.line("工作流", f"{w['name']}  槽位：{slots}  单价 {w['price_micro']} 微元{mark}")

    a = plan["account"]
    existing_acc = {x["name"]: x for x in cli.get("/api/admin/accounts")["items"]}
    if a["name"] in existing_acc:
        acc = existing_acc[a["name"]]
        rep.skipped.append(f"账号 {a['name']}")
        rep.line("跳过", f"账号 {a['name']}（已存在，余额 "
                         f"{acc['available_micro'] / MICRO_PER_YUAN:.2f} 元，没重复充值）")
    else:
        made = cli.post(
            "/api/admin/accounts",
            {
                "name": a["name"],
                "balance_micro": 0,
                "llm_max_concurrency": a["llm_max_concurrency"],
                "llm_max_queue": a["llm_max_queue"],
                "image_max_concurrency": a["image_max_concurrency"],
                "image_max_queue": a["image_max_queue"],
            },
        )
        bal = cli.post(
            f"/api/admin/accounts/{made['id']}/topup",
            {"amount_micro": a["balance_micro"], "note": "导入时初始化充值"},
        )["balance_micro"]
        rep.created.append(f"账号 {a['name']}")
        rep.secrets.append((a["name"], made["api_key"]))
        rep.line("新建", f"账号 {a['name']}  余额 {bal / MICRO_PER_YUAN:.2f} 元，"
                         f"LLM 并发 {a['llm_max_concurrency']}")


# ── 入口 ────────────────────────────────────────────────────────────────────


def main() -> int:
    ap = argparse.ArgumentParser(
        description="把 erotic_sci 的 LM Studio / ComfyUI 配置导入 home-ai-center")
    ap.add_argument("--source", type=Path,
                    default=Path(__file__).resolve().parent.parent.parent / "erotic_sci",
                    help="erotic_sci 仓库路径（只读）")
    ap.add_argument("--base-url", default=os.environ.get("HAC_BASE_URL", "http://127.0.0.1:8080"))
    ap.add_argument("--username", default="admin")
    ap.add_argument("--password", default=os.environ.get("HOME_AI_ADMIN_TOKEN"),
                    help="管理员口令，默认读 HOME_AI_ADMIN_TOKEN")
    ap.add_argument("--price-micro-per-1k", type=int, default=1200,
                    help="每 1k token 的微元单价（1200 = 1.2 元/百万）")
    ap.add_argument("--image-price-micro", type=int, default=100_000,
                    help="每张图的微元单价。**不能是 0**：网关对未配价的工作流"
                         "直接拒绝出图。默认 100000 微元 = 0.1 元/张。")
    ap.add_argument("--comfy-node-name", default="n1-comfy")
    ap.add_argument("--comfy-max-concurrency", type=int, default=2)
    ap.add_argument("--disable-nodes", nargs="*", default=["m2"],
                    help="导入但停用的节点名")
    ap.add_argument("--account-name", default="home")
    ap.add_argument("--account-topup-yuan", type=float, default=10.0)
    ap.add_argument("--account-llm-concurrency", type=int, default=8)
    ap.add_argument("--account-llm-queue", type=int, default=32)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    src: Path = args.source
    if not (src / "config" / "ingest.yaml").is_file():
        raise SystemExit(f"✗ {src} 不像 erotic_sci 仓库（缺 config/ingest.yaml）")

    cfg = load_source(src)
    plan = build_plan(cfg, args)

    print(f"源：{src}")
    print(f"LLM 节点 {len(plan['nodes'])} 个，工作流 {len(plan['workflows'])} 个，"
          f"ComfyUI 端点 {len(plan['comfy_nodes'])} 个\n")

    if args.dry_run:
        print(json.dumps(
            {k: [{kk: ("***" if kk in ("_key", "password") else vv)
                  for kk, vv in (v.items() if isinstance(v, dict) else [])} for v in vs]
             for k, vs in plan.items()},
            ensure_ascii=False, indent=2)[:4000])
        print("\n（dry-run，未连接网关）")
        return 0

    if not args.password:
        raise SystemExit("✗ 缺管理员口令：给 --password 或设 HOME_AI_ADMIN_TOKEN")

    cli = Client(args.base_url)
    cli.login(args.username, args.password)
    print(f"已登录 {args.base_url}\n")

    rep = Report()
    apply_plan(cli, plan, rep)

    if any(n["lan_base_url"] for n in plan["nodes"]):
        rep.notes.append(
            "内网地址走「内网优先 + 连不上自动回退公网」：内网不通只标记该节点的内网"
            "地址不可用 2 分钟，**不会**把节点拖进冷却——同一台机器的公网地址可能好好的。"
        )
    if plan["workflows"] and args.image_price_micro <= 0:
        rep.notes.append(
            "出图单价是 0：网关对未配价的工作流会**直接拒绝出图**，出图链路等于不可用。"
        )

    print()
    if rep.secrets:
        print("─── 保存好，明文只出现这一次 ───")
        for name, key in rep.secrets:
            print(f"  {name} 的 API Key:  {key}")
        print()

    if rep.notes:
        print("注意：")
        for x in rep.notes:
            print(f"  · {x}")
    print(f"完成：新建 {len(rep.created)}，更新 {len(rep.updated)}，跳过 {len(rep.skipped)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
