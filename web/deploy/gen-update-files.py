#!/usr/bin/env python3
"""gen-update-files.py — generate Amparo's update-notification surface.

Stdlib-only. Parses CHANGELOG.md (Keep a Changelog, SemVer sections) and
emits three files for the site:

  changelog.html  styled public changelog page
  version.json    machine-readable version endpoint for update checks
  feed.xml        RSS 2.0 feed of releases

Wired into web/deploy/deploy.sh: after the app rsync, the script runs
on the site box and writes into /srv/amparo/site, where Caddy serves
/changelog, /version.json, and /feed.xml (amparo.Caddyfile handles).

The Amparo repo is private (the public flip is pending), so relative
links in changelog entries are rendered as plain text (they would 404
for the public); absolute http(s) links are kept.
"""

import argparse
import html
import json
import os
import re
import sys
from email.utils import format_datetime
from datetime import datetime, timezone

PRODUCT = "amparo"

SECTION_RE = re.compile(r"^##\s+\[([^\]]+)\](?:\s+—\s+(\d{4}-\d{2}-\d{2}))?\s*$")


def parse_changelog(text):
    """Split CHANGELOG.md into (title, preamble, [Section]).

    Section = (version, date_or_None, body_lines). Sections are returned
    in file order (newest first, as Keep a Changelog dictates)."""
    lines = text.splitlines()
    title = ""
    preamble = []
    sections = []
    cur = None  # [version, date, body]

    i = 0
    if lines and lines[0].startswith("# "):
        title = lines[0][2:].strip()
        i = 1

    while i < len(lines):
        m = SECTION_RE.match(lines[i])
        if m:
            if cur is not None:
                sections.append(tuple(cur))
            cur = [m.group(1), m.group(2), []]
        else:
            if cur is None:
                preamble.append(lines[i])
            else:
                cur[2].append(lines[i])
        i += 1
    if cur is not None:
        sections.append(tuple(cur))
    return title, preamble, sections


def md_to_html(text, drop_relative_links):
    """Minimal markdown → HTML for the subset CHANGELOG.md uses.

    Headings (###), bullet lists, bold, inline code, paragraphs. Relative
    [text](path) links become plain text when drop_relative_links is set
    (private repo — the public page would 404); absolute http(s) links stay."""
    out = []
    para = []

    def flush_para():
        if para:
            out.append("<p>" + "\n".join(para) + "</p>")
            para.clear()

    def inline(s):
        s = html.escape(s, quote=False)
        if drop_relative_links:
            # [text](relative/path) → text; [text](https://…) keeps the link.
            s = re.sub(
                r"\[([^\]]+)\]\((?!https?://)([^)]*)\)",
                r"\1",
                s,
            )
        s = re.sub(
            r"\[([^\]]+)\]\((https?://[^)]+)\)",
            r'<a href="\1" rel="noopener">\2</a>',
            s,
        )
        s = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", s)
        s = re.sub(r"`([^`]+)`", r"<code>\1</code>", s)
        return s

    in_list = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("### "):
            flush_para()
            if in_list:
                out.append("</ul>")
                in_list = False
            out.append("<h3>" + inline(stripped[4:]) + "</h3>")
        elif stripped.startswith("- ") or stripped.startswith("* "):
            flush_para()
            if not in_list:
                out.append("<ul>")
                in_list = True
            out.append("<li>" + inline(stripped[2:]) + "</li>")
        elif not stripped:
            flush_para()
            if in_list:
                out.append("</ul>")
                in_list = False
        elif in_list and out and out[-1].endswith("</li>"):
            # Wrapped continuation of the previous bullet: fold into that
            # <li> so the <ul> never contains bare <p> elements.
            out[-1] = out[-1][:-5] + " " + inline(stripped) + "</li>"
        else:
            para.append(inline(line))
    flush_para()
    if in_list:
        out.append("</ul>")
    return "\n".join(out)


# The changelog page wears the landing's slate tokens (web/public/
# landing.css :root), dark-first with a light variant.
PAGE_CSS = """
:root {
  --bg: #0b0f14; --panel: #151b24; --text: #e2e8f0; --muted: #94a3b8;
  --accent: #3b82f6; --border: #232c3a;
}
@media (prefers-color-scheme: light) {
  :root:not([data-theme="dark"]) {
    --bg: #f6f8fb; --panel: #ffffff; --text: #16202e; --muted: #4a5a6e;
    --accent: #2563eb; --border: #d9e0ea;
  }
}
:root[data-theme="dark"] {
  --bg: #0b0f14; --panel: #151b24; --text: #e2e8f0; --muted: #94a3b8;
  --accent: #3b82f6; --border: #232c3a;
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--text);
  font: 15px/1.6 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
}
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
.wrap { max-width: 760px; margin: 0 auto; padding: 48px 20px 80px; }
header { border-bottom: 1px solid var(--border); padding-bottom: 24px; margin-bottom: 32px; }
header h1 { margin: 0 0 6px; font-size: 24px; }
header .sub { color: var(--muted); font-size: 14px; }
header .nav { margin-top: 10px; font-size: 13px; }
header .nav a { margin-right: 14px; }
section.release { margin-bottom: 40px; }
h2 {
  font-size: 18px; display: flex; align-items: baseline; gap: 10px;
  margin: 0 0 12px; padding-bottom: 8px; border-bottom: 1px solid var(--border);
}
h2 .ver { font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
          color: var(--accent); font-size: 17px; }
h2 .date { color: var(--muted); font-size: 13px; font-weight: 400; margin-left: auto; }
h3 { font-size: 14px; margin: 18px 0 8px; color: var(--muted);
     text-transform: uppercase; letter-spacing: 0.06em; }
p { margin: 10px 0; }
ul { margin: 8px 0; padding-left: 22px; }
li { margin: 6px 0; }
code { background: var(--panel); border: 1px solid var(--border);
       border-radius: 4px; padding: 1px 5px; font-size: 13px; }
footer { margin-top: 48px; padding-top: 16px; border-top: 1px solid var(--border);
         color: var(--muted); font-size: 13px; }
footer a { margin-right: 14px; }
"""


def changelog_page(title, preamble, sections, site):
    """Full styled changelog page; sections newest-first (file order)."""
    body = []
    for version, date, lines in sections:
        date_txt = date or "unreleased"
        body.append('<section class="release">')
        body.append(
            f'<h2><span class="ver">{html.escape(version)}</span>'
            f'<span class="date">{html.escape(date_txt)}</span></h2>'
        )
        body.append(md_to_html("\n".join(lines), drop_relative_links=True))
        body.append("</section>")
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark light">
<title>Changelog — Amparo by EL AI Intelligence</title>
<style>{PAGE_CSS}</style>
</head>
<body>
<div class="wrap">
  <header>
    <h1>Amparo changelog</h1>
    <div class="sub">Amparo by EL AI Intelligence — the policy-governed AI
      agent. Release notes for every version.</div>
    <div class="nav">
      <a href="{html.escape(site)}/version.json">version.json</a>
      <a href="{html.escape(site)}/feed.xml" type="application/rss+xml">RSS feed</a>
      <a href="{html.escape(INSTALL_URL)}">Install</a>
      <a href="{html.escape(site)}">Amparo</a>
    </div>
  </header>
  {chr(10).join(body)}
  <footer>
    <a href="{html.escape(site)}">Amparo</a>
    <a href="{html.escape(INSTALL_URL)}">Install</a>
    <span>Generated from CHANGELOG.md at deploy time.</span>
  </footer>
</div>
</body>
</html>
"""


def version_json(sections, site):
    """Machine-readable endpoint. Newest dated release (not [Unreleased])."""
    newest = None
    for version, date, _lines in sections:
        if version == "Unreleased":
            continue
        if date:
            newest = (version, date)
            break
    version = newest[0] if newest else "0.0.0"
    published_at = newest[1] if newest else None
    payload = {
        "product": PRODUCT,
        "version": version,
        "changelog_url": f"{site}/changelog",
        "rss_url": f"{site}/feed.xml",
        "install_url": INSTALL_URL,
    }
    if published_at:
        payload["published_at"] = published_at
    return json.dumps(payload, indent=2) + "\n"


def rss_feed(title, sections, site):
    """RSS 2.0; [Unreleased] is skipped; bodies are CDATA HTML."""
    items = []
    for version, date, lines in sections:
        if version == "Unreleased" or not date:
            continue
        body_html = md_to_html("\n".join(lines), drop_relative_links=True)
        try:
            dt = datetime.strptime(date, "%Y-%m-%d").replace(tzinfo=timezone.utc)
        except ValueError:
            continue
        pub = format_datetime(dt)
        desc = f"<![CDATA[{body_html}]]>"
        items.append(
            f"""    <item>
      <title>{html.escape(version)}</title>
      <link>{html.escape(site)}/changelog</link>
      <guid isPermaLink="false">{html.escape(PRODUCT)}-{html.escape(version)}</guid>
      <pubDate>{pub}</pubDate>
      <description>{desc}</description>
    </item>"""
        )
    if not items:
        items.append(
            "    <item>\n      <title>No releases yet</title>\n"
            f"      <link>{html.escape(site)}/changelog</link>\n"
            "      <guid isPermaLink=\"false\">no-releases</guid>\n"
            "    </item>"
        )
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>{html.escape(title)} releases</title>
    <link>{html.escape(site)}/changelog</link>
    <description>Release notes for {html.escape(title)} by EL AI Intelligence</description>
    <language>en</language>
{chr(10).join(items)}
  </channel>
</rss>
"""


def cargo_version(path):
    """Read `version` from Cargo.toml — [package] or [workspace.package].

    Amparo versions the whole workspace from one line, so the generator
    accepts either section; [package] is checked first for single-crate
    manifests."""
    with open(path, encoding="utf-8") as f:
        in_section = None
        for line in f:
            if line.strip() == "[package]":
                in_section = "package"
                continue
            if line.strip() == "[workspace.package]":
                in_section = "workspace.package"
                continue
            if in_section and line.startswith("["):
                in_section = None
            if in_section:
                m = re.match(r"^version\s*=\s*\"([^\"]+)\"", line)
                if m:
                    return m.group(1)
    raise SystemExit(f"no version in {path}")


INSTALL_URL = "https://downloads.ellmstack.dev/amparo/install.sh"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--changelog", required=True, help="path to CHANGELOG.md")
    ap.add_argument("--out-dir", required=True, help="directory to write files into")
    ap.add_argument("--site", required=True, help="public site base URL, e.g. https://amparo.ellmstack.dev")
    ap.add_argument(
        "--version-from-cargo",
        required=True,
        help="Cargo.toml whose [package]/[workspace.package] version is the product version",
    )
    args = ap.parse_args()

    with open(args.changelog, encoding="utf-8") as f:
        text = f.read()
    title, preamble, sections = parse_changelog(text)
    if not sections:
        print("no changelog sections found", file=sys.stderr)
        sys.exit(1)

    # The changelog's newest dated section must match the crate version —
    # a stale changelog would publish a wrong /version.json.
    crate_ver = cargo_version(args.version_from_cargo)
    newest_dated = next((v for v, d, _ in sections if v != "Unreleased" and d), None)
    if newest_dated is None or newest_dated != crate_ver:
        print(
            f"changelog newest section {newest_dated!r} != crate version {crate_ver!r}",
            file=sys.stderr,
        )
        sys.exit(1)

    os.makedirs(args.out_dir, exist_ok=True)
    with open(os.path.join(args.out_dir, "changelog.html"), "w", encoding="utf-8") as f:
        f.write(changelog_page(title, preamble, sections, args.site))
    with open(os.path.join(args.out_dir, "version.json"), "w", encoding="utf-8") as f:
        f.write(version_json(sections, args.site))
    with open(os.path.join(args.out_dir, "feed.xml"), "w", encoding="utf-8") as f:
        f.write(rss_feed(title, sections, args.site))
    print(
        f"wrote changelog.html, version.json ({crate_ver}), feed.xml → {args.out_dir}"
    )


if __name__ == "__main__":
    main()
