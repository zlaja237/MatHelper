"""MCP server for matmetrics: shopping list, receipt questions and BankID sync.

A thin adapter: everything goes through the matmetrics HTTP API (default
http://localhost:7878), which owns all data. This process stores nothing.

Run:
  matmetrics_mcp.py                      # stdio (Claude Code on the Pi)
  matmetrics_mcp.py --http --host <vpn-ip> --port 7879
                                         # streamable HTTP at /mcp, bind to the
                                         # VPN address only — no auth here.

Env:
  MATMETRICS_URL         API the server talks to (default http://localhost:7878)
  MATMETRICS_PUBLIC_URL  base URL the *user* can open, for BankID login links
                         (e.g. http://sezhost.your-tailnet.ts.net:7878)
"""

from __future__ import annotations

import argparse
import json
import os
import urllib.error
import urllib.parse
import urllib.request
from collections import defaultdict
from datetime import date, datetime, timedelta

from mcp.server.fastmcp import FastMCP

API = os.environ.get("MATMETRICS_URL", "http://localhost:7878").rstrip("/")
PUBLIC = os.environ.get("MATMETRICS_PUBLIC_URL", API).rstrip("/")
SOURCES = {"ica": "ICA", "willys": "Willys", "hemkop": "Hemköp"}

mcp = FastMCP(
    "matmetrics",
    instructions=(
        "Household groceries: a shared shopping list, purchase history from "
        "ICA/Willys/Hemköp receipts, and fetching new receipts. Amounts are in "
        "SEK. Fetching receipts needs the user to approve BankID: call "
        "start_receipt_sync, give the user the login_url to open on their "
        "phone, then call receipt_sync_status until it is done."
    ),
)


# --------------------------------------------------------------------------
# HTTP helpers
# --------------------------------------------------------------------------

def _call(path: str, body: dict | None = None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        API + path,
        data=data,
        method="GET" if body is None else "POST",
        headers={"Content-Type": "application/json"} if body is not None else {},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            raw = resp.read()
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"matmetrics svarade HTTP {e.code}: {e.read().decode(errors='replace')}") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"når inte matmetrics på {API}: {e.reason}") from e
    return json.loads(raw) if raw else None


def _list_view(lst: dict) -> dict:
    items = lst.get("items", [])
    return {
        "to_buy": [{"name": i["name"], "category": i["category"]} for i in items if not i["done"]],
        "bought": [i["name"] for i in items if i["done"]],
    }


def _parse_day(s: str | None, field: str) -> date | None:
    if not s:
        return None
    try:
        return date.fromisoformat(s)
    except ValueError as e:
        raise ValueError(f"{field} måste vara YYYY-MM-DD, fick {s!r}") from e


def _receipts(since: str | None, until: str | None) -> list[dict]:
    lo, hi = _parse_day(since, "since"), _parse_day(until, "until")
    out = []
    for r in _call("/api/receipts"):
        d = datetime.fromisoformat(r["purchased_at"].replace("Z", "+00:00")).astimezone().date()
        if (lo and d < lo) or (hi and d > hi):
            continue
        r["_date"] = d
        out.append(r)
    return out


def _kr(ore: int) -> float:
    return round(ore / 100, 2)


def _store_label(r: dict) -> str:
    chain = SOURCES.get(r["store"]["chain"], r["store"]["chain"])
    name = r["store"].get("name") or ""
    return f"{chain} {name}".strip()


# --------------------------------------------------------------------------
# Shopping list
# --------------------------------------------------------------------------

@mcp.tool()
def get_shopping_list() -> dict:
    """The current shared shopping list: items still to buy (with category) and items already bought."""
    return _list_view(_call("/api/list"))


@mcp.tool()
def add_to_shopping_list(items: list[str]) -> dict:
    """Add one or more items to the shopping list. Category is chosen automatically from purchase history. Items already on the list are skipped."""
    lst = None
    for name in items:
        if name.strip():
            lst = _call("/api/list/add", {"name": name.strip()})
    return _list_view(lst or _call("/api/list"))


@mcp.tool()
def mark_bought(items: list[str]) -> dict:
    """Tick off items on the shopping list by name (case-insensitive)."""
    return _list_view(_call("/api/list/done-by-name", {"names": items}))


@mcp.tool()
def remove_from_shopping_list(items: list[str]) -> dict:
    """Remove items from the shopping list entirely (not marking them bought)."""
    wanted = {" ".join(n.lower().split()) for n in items}
    lst = _call("/api/list")
    for i in lst["items"]:
        if " ".join(i["name"].lower().split()) in wanted:
            lst = _call("/api/list/remove", {"id": i["id"]})
    return _list_view(lst)


@mcp.tool()
def clear_bought_items() -> dict:
    """Remove every ticked-off item from the shopping list."""
    return _list_view(_call("/api/list/clear-done", {}))


@mcp.tool()
def purchase_patterns(
    query: str | None = None,
    category: str | None = None,
    min_times: int = 2,
    limit: int = 60,
) -> list[dict]:
    """Facts per item from all receipts, most frequently bought first: times bought (distinct days), first/last purchase date, days since last purchase, median/min/max days between purchases, and whether it is on the shopping list now. No ranking or "due" judgement — use these facts, together with what the user says, to decide what to buy. Filter by part of the item name (query) or category."""
    q = " ".join(query.lower().split()) if query else None
    c = category.lower() if category else None
    rows = [
        p for p in _call("/api/patterns")
        if p["times"] >= max(1, min_times)
        and (q is None or q in " ".join(p["name"].lower().split()))
        and (c is None or c in p["category"].lower())
    ]
    return rows[: max(1, limit)]


# --------------------------------------------------------------------------
# Purchase history
# --------------------------------------------------------------------------

@mcp.tool()
def search_purchases(query: str, since: str | None = None, until: str | None = None, limit: int = 50) -> dict:
    """Find purchases of an item by (part of) its name, newest first. Dates are YYYY-MM-DD. Returns date, store, quantity and price per purchase, plus totals."""
    q = " ".join(query.lower().split())
    hits = []
    for r in _receipts(since, until):
        for line in r["lines"]:
            if line["kind"] != "Item" or q not in " ".join(line["description"].lower().split()):
                continue
            qty_kind, qty = next(iter(line["quantity"].items())) if isinstance(line["quantity"], dict) else ("Count", 1)
            hits.append({
                "date": r["_date"].isoformat(),
                "store": _store_label(r),
                "item": line["description"],
                "category": line.get("category"),
                "quantity": qty,
                "unit": {"Count": "st", "Kilograms": "kg", "Litres": "l"}.get(qty_kind, qty_kind),
                "amount_kr": _kr(line["amount"]),
                "unit_price_kr": _kr(line["unit_price"]) if line.get("unit_price") else None,
            })
    hits.sort(key=lambda h: h["date"], reverse=True)
    return {
        "matches": len(hits),
        "total_kr": round(sum(h["amount_kr"] for h in hits), 2),
        "purchases": hits[: max(1, limit)],
    }


@mcp.tool()
def spending_summary(since: str | None = None, until: str | None = None, group_by: str = "category") -> dict:
    """Total spending grouped by "category", "store", "month" or "item" (top 40). Dates are YYYY-MM-DD; default is all receipts. Discounts and deposits are included in the totals."""
    if group_by not in {"category", "store", "month", "item"}:
        raise ValueError('group_by måste vara "category", "store", "month" eller "item"')
    rs = _receipts(since, until)
    totals: dict[str, int] = defaultdict(int)
    for r in rs:
        if group_by == "store":
            totals[_store_label(r)] += r["total"]
        elif group_by == "month":
            totals[r["_date"].strftime("%Y-%m")] += r["total"]
        else:
            lines = r["lines"]
            for idx, line in enumerate(lines):
                # Rabatt/pant räknas på varan de hör till.
                owner = lines[line["applies_to"]] if line.get("applies_to") is not None and line["applies_to"] < len(lines) else line
                key = (owner.get("category") or "Okategoriserat") if group_by == "category" else owner["description"]
                totals[key] += line["amount"]
    rows = sorted(totals.items(), key=lambda kv: kv[1], reverse=True)
    if group_by == "month":
        rows.sort()
    if group_by == "item":
        rows = rows[:40]
    return {
        "receipts": len(rs),
        "period": {
            "from": min((r["_date"] for r in rs), default=None) and min(r["_date"] for r in rs).isoformat(),
            "to": max((r["_date"] for r in rs), default=None) and max(r["_date"] for r in rs).isoformat(),
        },
        "total_kr": _kr(sum(r["total"] for r in rs)),
        "groups": [{"name": k, "kr": _kr(v)} for k, v in rows],
    }


@mcp.tool()
def list_receipts(since: str | None = None, until: str | None = None, limit: int = 20, with_items: bool = False) -> list[dict]:
    """Receipts newest first (date, store, total, number of items). Set with_items to include each line. Dates are YYYY-MM-DD."""
    rs = sorted(_receipts(since, until), key=lambda r: r["purchased_at"], reverse=True)[: max(1, limit)]
    out = []
    for r in rs:
        row = {
            "date": r["_date"].isoformat(),
            "store": _store_label(r),
            "total_kr": _kr(r["total"]),
            "items": sum(1 for l in r["lines"] if l["kind"] == "Item"),
        }
        if with_items:
            row["lines"] = [
                {"item": l["description"], "kind": l["kind"], "category": l.get("category"), "amount_kr": _kr(l["amount"])}
                for l in r["lines"]
            ]
        out.append(row)
    return out


# --------------------------------------------------------------------------
# Fetching new receipts (BankID)
# --------------------------------------------------------------------------

def _status_view(profile: str, source: str, d: dict) -> dict:
    view = {"source": SOURCES[source], "profile": profile, "status": d.get("status", "idle")}
    if view["status"] == "authenticating":
        view["login_url"] = f"{PUBLIC}/login?" + urllib.parse.urlencode({"source": source, "profile": profile})
        view["next_step"] = "Be användaren öppna login_url på telefonen och godkänna i BankID-appen."
    elif view["status"] == "done":
        view["newly_fetched"] = d.get("newly_fetched")
    elif view["status"] == "error":
        view["message"] = d.get("message")
    return view


def _check_source(source: str) -> str:
    s = source.lower().replace("ö", "o")
    if s not in SOURCES:
        raise ValueError(f"source måste vara en av {', '.join(SOURCES)}")
    return s


@mcp.tool()
def start_receipt_sync(source: str, profile: str = "du") -> dict:
    """Start fetching new receipts from "ica", "willys" or "hemkop". Requires BankID: the result has a login_url the user must open on their phone to approve (it shows a live QR code and an "open BankID" button). Then poll receipt_sync_status."""
    source = _check_source(source)
    path = f"/api/sync/status/{urllib.parse.quote(profile)}/{source}"
    cur = _call(path)
    if cur.get("status") in {"authenticating", "working"}:
        return _status_view(profile, source, cur)
    return _status_view(profile, source, _call("/api/sync/start", {"profile": profile, "source": source}))


@mcp.tool()
def receipt_sync_status(source: str, profile: str = "du") -> dict:
    """Status of a receipt sync: authenticating (waiting for BankID), working, done (with number of new receipts) or error."""
    source = _check_source(source)
    return _status_view(profile, source, _call(f"/api/sync/status/{urllib.parse.quote(profile)}/{source}"))


@mcp.tool()
def list_profiles() -> list[str]:
    """Household profiles that receipts can be fetched for."""
    return _call("/api/profiles")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--http", action="store_true", help="serve streamable HTTP at /mcp instead of stdio")
    ap.add_argument("--host", default="127.0.0.1", help="address to bind with --http (use the VPN address)")
    ap.add_argument("--port", type=int, default=7879)
    args = ap.parse_args()
    if args.http:
        mcp.settings.host = args.host
        mcp.settings.port = args.port
        mcp.run(transport="streamable-http")
    else:
        mcp.run()


if __name__ == "__main__":
    main()
