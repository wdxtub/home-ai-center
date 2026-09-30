"""SQL 静态审计。

查两类**编译期发现不了、运行时才炸**的错误：

1. `[漏绑]`  SELECT/UPDATE 里的 `?` 数量与 `.bind()` 次数不符
   ——SQLite 把未绑定的 `?` 当 NULL，症状是「某列莫名其妙是 NULL」。

2. `[列数不符]`  INSERT 的列数与 VALUES 的值数不符
   ——症状同样是 NOT NULL 约束失败，但更难定位到是哪一列。

跑法：python3 scripts/audit_sql.py
"""
import re
import sys
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent / "crates" / "gateway" / "src"

INSERT_HEAD = re.compile(
    r"INSERT(?:\s+OR\s+\w+)?\s+INTO\s+(\w+)\s*\(", re.S | re.I
)
QUERY = re.compile(r'sqlx::query(?:_as|_scalar)?\s*\(\s*("(?:[^"\\]|\\.)*")\s*\)', re.S)


def split_top_level(text: str) -> list[str]:
    """按顶层逗号切分，忽略括号与引号内的逗号。"""
    out, depth, buf, quote = [], 0, [], None
    for ch in text:
        if quote:
            buf.append(ch)
            if ch == quote:
                quote = None
            continue
        if ch in "'\"`":
            quote = ch
            buf.append(ch)
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(buf).strip())
            buf = []
        else:
            buf.append(ch)
    if buf:
        out.append("".join(buf).strip())
    return [x for x in out if x]


def balanced_slice(text: str, open_idx: int) -> str:
    """返回从 open_idx 处 `(` 开始、到配对 `)` 结束的内容。"""
    depth = 0
    quote = None
    for i in range(open_idx, len(text)):
        ch = text[i]
        if quote:
            if ch == quote:
                quote = None
            continue
        if ch in "'\"":
            quote = ch
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return text[open_idx + 1 : i]
    return text[open_idx + 1 :]


def n_placeholders(sql: str) -> int:
    # ?NNN / ?NNN,NNN / :name 都不按 `?` 计
    return len(re.findall(r"(?<![?\d:])\?(?![?\d])", sql))


def main() -> int:
    bad = 0
    for f in sorted(ROOT.rglob("*.rs")):
        src = f.read_text()

        # ── 漏绑 ──
        for m in QUERY.finditer(src):
            tail = src[m.end() : m.end() + 3000]
            stop = tail.find(".await")
            if stop == -1:
                continue
            sql = m.group(1)[1:-1]
            want = n_placeholders(sql)
            got = tail[:stop].count(".bind(")
            if want != got:
                line = src[: m.start()].count("\n") + 1
                print(f"[漏绑] {f}:{line}  ?={want} bind={got}  {sql[:70]!r}")
                bad += 1

        # ── INSERT 列数 ──
        for m in INSERT_HEAD.finditer(src):
            cols = split_top_level(balanced_slice(src, m.end() - 1))
            rest = src[m.end() :]
            vm = re.search(r"\bVALUES\b", rest, re.I)
            if not vm:
                continue
            open_idx = rest.index("(", vm.end())
            vals = split_top_level(balanced_slice(rest, open_idx))
            if len(cols) != len(vals):
                line = src[: m.start()].count("\n") + 1
                print(
                    f"[列数不符] {f}:{line}  {m.group(1)}: "
                    f"列 {len(cols)} vs VALUES {len(vals)}"
                )
                bad += 1

    if bad == 0:
        print("SQL 审计：全部通过")
        return 0
    print(f"SQL 审计：{bad} 处问题")
    return 1


if __name__ == "__main__":
    sys.exit(main())
