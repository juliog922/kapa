#!/usr/bin/env python3
"""Kapa - render the Sage fixture (schema, statistics, sample) as a readable Markdown summary.

Usage: sage-summary.py <sage.local.json> <sage-summary.local.md>   (standard library only)
"""
import json
import sys


def cell(value):
    """A Markdown table cell (None as a dash, pipes escaped)."""
    return "—" if value is None else str(value).replace("|", "\\|").replace("\n", " ")


def column_type(c):
    """SQL type with its length or precision, e.g. varchar(15), decimal(28,10)."""
    t, length = c.get("DATA_TYPE", "?"), c.get("CHARACTER_MAXIMUM_LENGTH")
    if t == "decimal":
        return f"decimal({c.get('NUMERIC_PRECISION')},{c.get('NUMERIC_SCALE')})"
    return f"{t}({length})" if length not in (None, 2147483647) else t


def table(name, info, stats, sample, masked):
    """Markdown section of one table."""
    cols = {c["COLUMN_NAME"]: c for c in info["columns"]}
    used = stats.get("columns", {})
    out = [f"## {name} · {info.get('schema')} · {stats.get('rows', info.get('rows_approx'))} rows · {len(cols)} columns", "",
           "| column | type | nullable | nulls | empty | zeros | distinct | min | max | length |",
           "|---|---|---|---|---|---|---|---|---|---|"]
    for col, m in used.items():
        c = cols.get(col, {})
        length = f"{m['min_len']}–{m['max_len']}" if m.get("max_len") is not None else "—"
        out.append("| " + " | ".join([col, column_type(c), c.get("IS_NULLABLE", "?"), cell(m.get("nulls")), cell(m.get("empty")),
                                      cell(m.get("zeros")), cell(m.get("distinct")), cell(m.get("min")), cell(m.get("max")), length]) + " |")
    tops = [(col, m["top"]) for col, m in used.items() if m.get("top")]
    if tops:
        out += ["", "**Top values** (value ×rows)", ""]
        out += [f"- `{col}`: " + ", ".join(f"{json.dumps(v, ensure_ascii=False)} ×{n}" for v, n in top) for col, top in tops]
    out += ["", "**All columns**: " + " ".join(cols), ""]
    if sample:
        out += ["**Sample rows**" + (" (personal data masked)" if masked else " (RAW)"), "", "```json"]
        out += [json.dumps(r, ensure_ascii=False) for r in sample[:2]] + ["```", ""]
    return out


def main():
    src, dst = sys.argv[1], sys.argv[2]
    with open(src, encoding="utf-8") as f:
        d = json.load(f)
    out = [f"# Sage · {d['database']} · exported {d['generated_at']}", "",
           f"{d['server']} · login {d['login']} · personal data {'masked' if d['masked'] else 'RAW'} · "
           f"sample of the {d['orders']} latest orders · statistics over the full tables", ""]
    for name, info in d["schema"].items():
        out += table(name, info, d.get("stats", {}).get(name, {}), d["sample"].get(name, []), d["masked"])
    with open(dst, "w", encoding="utf-8") as f:
        f.write("\n".join(out))


if __name__ == "__main__":
    main()
