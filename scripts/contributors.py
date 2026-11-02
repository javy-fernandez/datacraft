#!/usr/bin/env python3
"""Generate the README contributor chart from the git history.

Counts non-merge commits and changed lines per author, resolves GitHub
accounts for avatars, then writes a light and a dark SVG chart and refreshes
the section between the contributor markers in README.md.

Usage: scripts/contributors.py [--repo owner/name] [--top 5]

Needs a full clone (``git fetch --unshallow``). GITHUB_TOKEN, when set, is
used to look up the GitHub account behind commit e-mail addresses; without it
only GitHub no-reply addresses get avatars and the others show initials.
"""

from __future__ import annotations

import argparse
import base64
import collections
import dataclasses
import datetime
import html
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
README = ROOT / "README.md"
CHART_DIR = ROOT / "docs" / "contributors"
START = "<!-- contributors:start -->"
END = "<!-- contributors:end -->"

# Files whose line counts say nothing about authorship.
IGNORED_PATHS = {"Cargo.lock"}
IGNORED_PREFIXES = ("docs/contributors/",)

NOREPLY = re.compile(r"^(?:(\d+)\+)?([^@]+)@users\.noreply\.github\.com$", re.I)

# Validated categorical pair (CVD-safe in both themes) and GitHub surfaces.
THEMES = {
    "light": {
        "surface": "#ffffff",
        "border": "#d1d9e0",
        "ink": "#1f2328",
        "ink2": "#59636e",
        "muted": "#818b98",
        "track": "#eff2f5",
        "rule": "#eff2f5",
        "commits": "#1f97a6",
        "lines": "#eb6834",
        "initials_bg": "#ddf4f6",
        "initials_ink": "#0f5f69",
    },
    "dark": {
        "surface": "#0d1117",
        "border": "#3d444d",
        "ink": "#f0f6fc",
        "ink2": "#9198a1",
        "muted": "#656c76",
        "track": "#212830",
        "rule": "#1c2128",
        "commits": "#1d9aaa",
        "lines": "#d95926",
        "initials_bg": "#123b41",
        "initials_ink": "#9fe3eb",
    },
}

FONT = '-apple-system, BlinkMacSystemFont, "Segoe UI", "Noto Sans", Helvetica, Arial, sans-serif'


@dataclasses.dataclass
class Author:
    key: str
    names: collections.Counter = dataclasses.field(default_factory=collections.Counter)
    emails: set = dataclasses.field(default_factory=set)
    commits: int = 0
    added: int = 0
    deleted: int = 0
    first: str = ""
    sample_sha: str = ""
    login: str | None = None
    user_id: int | None = None
    avatar: str | None = None  # data: URI

    @property
    def lines(self) -> int:
        return self.added + self.deleted

    @property
    def name(self) -> str:
        return self.names.most_common(1)[0][0]

    @property
    def handle(self) -> str | None:
        """The GitHub login, when it adds something to the display name."""
        if self.login and self.login.lower() != self.name.lower():
            return f"@{self.login}"
        return None


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, check=True, capture_output=True, text=True).stdout


def is_bot(name: str, email: str) -> bool:
    return "[bot]" in name.lower() or "[bot]" in email.lower()


def read_history() -> tuple[dict[str, Author], str]:
    if git("rev-parse", "--is-shallow-repository").strip() == "true":
        sys.exit("The clone is shallow: run `git fetch --unshallow` first.")
    log = git("log", "--no-merges", "--reverse", "--numstat", "--format=%x1e%H%x1f%an%x1f%ae%x1f%aI", "HEAD")
    authors: dict[str, Author] = {}
    for record in log.split("\x1e")[1:]:
        header, _, body = record.partition("\n")
        sha, name, email, date = header.split("\x1f")
        if is_bot(name, email):
            continue
        key = email.strip().lower()
        author = authors.setdefault(key, Author(key, first=date, sample_sha=sha))
        author.names[name] += 1
        author.emails.add(email.strip())
        author.commits += 1
        for line in body.splitlines():
            parts = line.split("\t")
            if len(parts) != 3 or parts[0] == "-":
                continue
            path = parts[2]
            if path in IGNORED_PATHS or path.startswith(IGNORED_PREFIXES):
                continue
            author.added += int(parts[0])
            author.deleted += int(parts[1])
    as_of = git("log", "-1", "--format=%cI", "HEAD").strip()
    return authors, as_of


def http_get(url: str, token: str | None, accept: str = "application/vnd.github+json") -> tuple[bytes, str] | None:
    request = urllib.request.Request(url, headers={"Accept": accept, "User-Agent": "datacraft-contributors"})
    if token and url.startswith("https://api.github.com/"):
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.read(), response.headers.get_content_type()
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        print(f"warning: {url}: {error}", file=sys.stderr)
        return None


def resolve_accounts(authors: dict[str, Author], repo: str | None, token: str | None) -> None:
    for author in authors.values():
        match = next(filter(None, map(NOREPLY.match, author.emails)), None)
        if match:
            author.login = match.group(2)
            author.user_id = int(match.group(1)) if match.group(1) else None
            continue
        if not repo:
            continue
        reply = http_get(f"https://api.github.com/repos/{repo}/commits/{author.sample_sha}", token)
        if reply is None:
            continue
        account = json.loads(reply[0]).get("author") or {}
        if account.get("type") == "Bot":
            continue
        author.login = account.get("login")
        author.user_id = account.get("id")


def merge_by_account(authors: dict[str, Author]) -> list[Author]:
    merged: dict[str, Author] = {}
    for author in sorted(authors.values(), key=lambda a: a.first):
        key = f"login:{author.login.lower()}" if author.login else author.key
        if key not in merged:
            merged[key] = author
            continue
        target = merged[key]
        target.names.update(author.names)
        target.emails |= author.emails
        target.commits += author.commits
        target.added += author.added
        target.deleted += author.deleted
        target.user_id = target.user_id or author.user_id
    return sorted(merged.values(), key=lambda a: (-a.commits, -a.lines, a.name.lower()))


def fetch_avatar(author: Author) -> None:
    if author.user_id:
        url = f"https://avatars.githubusercontent.com/u/{author.user_id}?s=96&v=4"
    elif author.login:
        url = f"https://github.com/{author.login}.png?size=96"
    else:
        return
    reply = http_get(url, None, accept="image/*")
    if reply and reply[1].startswith("image/"):
        author.avatar = f"data:{reply[1]};base64,{base64.b64encode(reply[0]).decode()}"


def percent(part: int, whole: int) -> str:
    return f"{100 * part / whole:.1f}%" if whole else "0.0%"


def number(value: int) -> str:
    return f"{value:,}"


def initials(name: str) -> str:
    words = [w for w in re.split(r"[\s._-]+", name) if w]
    letters = "".join(w[0] for w in words[:2]) or name[:1]
    return letters.upper()


def esc(text: str) -> str:
    return html.escape(text, quote=True)


def render_svg(rows: list[Author], others: list[Author], totals: tuple[int, int], as_of: datetime.date, theme: str) -> str:
    c = THEMES[theme]
    total_commits, total_lines = totals
    width, pad, row_h, top0 = 840, 28, 68, 118
    bar_x, bar_w, label_x = 330, 392, width - pad
    entries: list[tuple[str, str, Author | None, int, int, int, int]] = []
    for a in rows:
        entries.append((a.name, f"{number(a.commits)} commits · +{number(a.added)} −{number(a.deleted)}", a, a.commits, a.lines, 0, 0))
    if others:
        commits = sum(a.commits for a in others)
        lines = sum(a.lines for a in others)
        label = f"{len(others)} other contributor" + ("s" if len(others) != 1 else "")
        entries.append((label, f"{number(commits)} commits · {number(lines)} lines", None, commits, lines, len(others), 0))
    height = top0 + len(entries) * row_h + 34
    count = len(rows)
    title = f"Top {count} contributor" + ("s" if count != 1 else "")
    subtitle = (
        f"Share of {number(total_commits)} non-merge commits and {number(total_lines)} changed lines"
        f" · main as of {as_of.day} {as_of:%b %Y}"
    )
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="title desc">',
        f'<title id="title">{esc(title)}</title>',
        "<desc id=\"desc\">" + esc("; ".join(
            f"{e[0]}: {percent(e[3], total_commits)} of commits, {percent(e[4], total_lines)} of changed lines" for e in entries
        )) + "</desc>",
        "<style>",
        f"text{{font-family:{FONT};fill:{c['ink']}}}",
        f".t{{font-size:18px;font-weight:600}} .s{{font-size:13px;fill:{c['ink2']}}} .n{{font-size:15px;font-weight:600}}",
        f".d{{font-size:12px;fill:{c['ink2']};font-variant-numeric:tabular-nums}} .l{{font-size:12.5px;fill:{c['ink2']}}}",
        f".p{{font-size:13px;font-weight:600;font-variant-numeric:tabular-nums}} .q{{font-size:13px;fill:{c['ink2']};font-variant-numeric:tabular-nums}}",
        f".h{{font-size:13px;font-weight:400;fill:{c['ink2']}}}",
        f".f{{font-size:11px;fill:{c['muted']}}} .i{{font-size:14px;font-weight:600;fill:{c['initials_ink']}}}",
        "</style>",
        f'<rect x="0.5" y="0.5" width="{width - 1}" height="{height - 1}" rx="12" fill="{c["surface"]}" stroke="{c["border"]}"/>',
        f'<text class="t" x="{pad}" y="44">{esc(title)}</text>',
        f'<text class="s" x="{pad}" y="66">{esc(subtitle)}</text>',
        f'<rect x="{pad}" y="84" width="12" height="12" rx="3" fill="{c["commits"]}"/>',
        f'<text class="l" x="{pad + 18}" y="94.5">Commits</text>',
        f'<rect x="{pad + 96}" y="84" width="12" height="12" rx="3" fill="{c["lines"]}"/>',
        f'<text class="l" x="{pad + 114}" y="94.5">Lines changed</text>',
    ]
    for index, (name, detail, author, commits, lines, extra, _) in enumerate(entries):
        top = top0 + index * row_h
        cx, cy, r = pad + 22, top + 26, 20
        if index:
            out.append(f'<line x1="{pad}" x2="{width - pad}" y1="{top - 8}" y2="{top - 8}" stroke="{c["rule"]}"/>')
        if author is not None and author.avatar:
            out.append(f'<clipPath id="a{index}"><circle cx="{cx}" cy="{cy}" r="{r}"/></clipPath>')
            out.append(f'<image href="{author.avatar}" x="{cx - r}" y="{cy - r}" width="{2 * r}" height="{2 * r}" clip-path="url(#a{index})" preserveAspectRatio="xMidYMid slice"/>')
            out.append(f'<circle cx="{cx}" cy="{cy}" r="{r - 0.5}" fill="none" stroke="{c["border"]}"/>')
        else:
            mark = f"+{extra}" if author is None else initials(name)
            out.append(f'<circle cx="{cx}" cy="{cy}" r="{r}" fill="{c["initials_bg"]}"/>')
            out.append(f'<text class="i" x="{cx}" y="{cy + 5}" text-anchor="middle">{esc(mark)}</text>')
        shown = name if len(name) <= 24 else name[:23] + "…"
        handle = f'<tspan class="h" dx="6">{esc(author.handle)}</tspan>' if author is not None and author.handle else ""
        out.append(f'<text class="n" x="{pad + 58}" y="{top + 21}">{esc(shown)}{handle}</text>')
        out.append(f'<text class="d" x="{pad + 58}" y="{top + 41}">{esc(detail)}</text>')
        for offset, value, whole, color, css in ((10, commits, total_commits, c["commits"], "p"), (28, lines, total_lines, c["lines"], "q")):
            share = value / whole if whole else 0
            y = top + offset
            out.append(f'<rect x="{bar_x}" y="{y}" width="{bar_w}" height="10" rx="5" fill="{c["track"]}"/>')
            if share > 0:
                w = max(4.0, round(bar_w * share, 1))
                out.append(f'<rect x="{bar_x}" y="{y}" width="{w}" height="10" rx="5" fill="{color}"/>')
            out.append(f'<text class="{css}" x="{label_x}" y="{y + 9.5}" text-anchor="end">{percent(value, whole)}</text>')
    out.append(f'<text class="f" x="{pad}" y="{height - 16}">Bars span 100% of the total. Merge commits, bots and Cargo.lock are not counted.</text>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


def render_markdown(rows: list[Author], others: list[Author], totals: tuple[int, int]) -> str:
    total_commits, total_lines = totals
    lines = [
        START,
        "<!-- Generated by scripts/contributors.py; edits here are overwritten. -->",
        '<p align="center">',
        "  <picture>",
        '    <source media="(prefers-color-scheme: dark)" srcset="docs/contributors/top-dark.svg">',
        f'    <img src="docs/contributors/top-light.svg" alt="Top {len(rows)} contributors by share of commits and changed lines" width="100%">',
        "  </picture>",
        "</p>",
        "",
        "<details>",
        "<summary>Contributor numbers</summary>",
        "",
        "| # | Contributor | Commits | Share | Lines changed | Share |",
        "| ---: | --- | ---: | ---: | ---: | ---: |",
    ]
    for rank, a in enumerate(rows, 1):
        who = f"[{a.name}](https://github.com/{a.login})" if a.login else a.name
        lines.append(
            f"| {rank} | {who} | {number(a.commits)} | {percent(a.commits, total_commits)} "
            f"| +{number(a.added)} / −{number(a.deleted)} | {percent(a.lines, total_lines)} |"
        )
    if others:
        commits = sum(a.commits for a in others)
        changed = sum(a.lines for a in others)
        lines.append(f"| | {len(others)} others | {number(commits)} | {percent(commits, total_commits)} | {number(changed)} | {percent(changed, total_lines)} |")
    lines += [
        "",
        "Counted from non-merge commits on `main`; merge commits, bots and `Cargo.lock` are excluded.",
        "The chart is refreshed by the [contributors workflow](.github/workflows/contributors.yml).",
        "",
        "</details>",
        END,
    ]
    return "\n".join(lines)


def default_repo() -> str | None:
    if os.environ.get("GITHUB_REPOSITORY"):
        return os.environ["GITHUB_REPOSITORY"]
    try:
        url = git("remote", "get-url", "origin").strip()
    except subprocess.CalledProcessError:
        return None
    match = re.search(r"github\.com[:/]([^/]+/[^/]+?)(?:\.git)?$", url)
    return match.group(1) if match else None


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", default=default_repo(), help="GitHub repository used to look up accounts")
    parser.add_argument("--top", type=int, default=5)
    args = parser.parse_args()

    authors, as_of = read_history()
    resolve_accounts(authors, args.repo, os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN"))
    ranked = merge_by_account(authors)
    rows, others = ranked[: args.top], ranked[args.top :]
    for author in rows:
        fetch_avatar(author)
    totals = (sum(a.commits for a in ranked), sum(a.lines for a in ranked))
    date = datetime.datetime.fromisoformat(as_of).astimezone(datetime.timezone.utc).date()

    CHART_DIR.mkdir(parents=True, exist_ok=True)
    for theme in THEMES:
        (CHART_DIR / f"top-{theme}.svg").write_text(render_svg(rows, others, totals, date, theme), encoding="utf-8")

    readme = README.read_text(encoding="utf-8")
    if START not in readme or END not in readme:
        sys.exit(f"README.md needs the {START} and {END} markers.")
    head, rest = readme.split(START, 1)
    _, tail = rest.split(END, 1)
    README.write_text(head + render_markdown(rows, others, totals) + tail, encoding="utf-8")

    for rank, a in enumerate(rows, 1):
        print(f"{rank}. {a.name}: {a.commits} commits ({percent(a.commits, totals[0])}), {a.lines} lines ({percent(a.lines, totals[1])})")


if __name__ == "__main__":
    main()
