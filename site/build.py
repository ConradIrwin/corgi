# /// script
# requires-python = ">=3.9"
# dependencies = ["markdown-it-py>=3"]
# ///
"""Render README.md into the site template.

The README's first line is the tagline (shown in the brass nameplate and the
<title>); everything after it becomes the page body.

Usage (from the repo root):
    uv run site/build.py            # writes _site/
    open _site/index.html
"""

import html
import re
import shutil
from pathlib import Path

from markdown_it import MarkdownIt

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
OUT = ROOT / "_site"
ASSETS = ["corgi.png"]


def slug(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def render(markdown: str) -> str:
    md = MarkdownIt("commonmark").enable(["table", "strikethrough"])
    tokens = md.parse(markdown)
    # Give headings ids so sections can be linked to.
    for i, token in enumerate(tokens):
        if token.type == "heading_open":
            token.attrSet("id", slug(tokens[i + 1].content))
    return md.renderer.render(tokens, md.options, {})


def main() -> None:
    tagline, _, body = (ROOT / "README.md").read_text().partition("\n")
    page = (
        (SITE / "template.html")
        .read_text()
        .replace("{{tagline}}", html.escape(tagline.strip()))
        .replace("{{content}}", render(body).strip())
    )

    shutil.rmtree(OUT, ignore_errors=True)
    OUT.mkdir()
    (OUT / "index.html").write_text(page)
    for asset in ASSETS:
        shutil.copy2(SITE / asset, OUT / asset)
    print(f"wrote {OUT / 'index.html'}")


if __name__ == "__main__":
    main()
