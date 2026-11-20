"""Generate `docs/llms.txt` and `docs/llms-full.txt` from the `zensical.toml` navigation.

See https://llmstxt.org/ for the format: `llms.txt` lists every page with a one-line
description, `llms-full.txt` holds the text of every page.

Usage:
    python scripts/build_llms_txt.py
    python scripts/build_llms_txt.py --check  # CI: fail when stale
"""

from __future__ import annotations

import argparse
import posixpath
import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
LLMS = DOCS / "llms.txt"
LLMS_FULL = DOCS / "llms-full.txt"
# Lines that start something other than prose: admonitions, content tabs, HTML, tables, lists,
# headings, quotes, and images.
NOT_PROSE = ("!!!", "===", "<", "|", "-", "* ", "#", ">", "[![", "![")
FENCES = ("```", "~~~")
NUMBERED_ITEM = re.compile(r"\d+\. ")
SENTENCE_END = re.compile(r"(?<=[.!?])\s+(?=[A-Z0-9])")
LINK = re.compile(r"\[([^\]]+)]\([^)]+\)")
SCHEME = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*:")
LINK_TARGET = re.compile(r"(\]\()([^)\s]+)(\))")


def page_url(site: str, path: str) -> str:
    slug = path.removesuffix(".md")
    if slug == "index":
        return f"{site}/"
    slug = slug.removesuffix("/index")
    return f"{site}/{slug}/"


def paragraphs(text: str):
    """The prose paragraphs of a page, each joined into one line, outside code blocks."""
    fence = None
    lines: list[str] = []
    for raw in text.splitlines():
        line = raw.strip()
        if fence:
            if line.startswith(fence):
                fence = None
            continue
        if line.startswith(FENCES):
            fence = line[:3]
        elif (
            line
            and not line.startswith(NOT_PROSE)
            and not NUMBERED_ITEM.match(line)
            and not raw[:1].isspace()
        ):
            lines.append(line)
            continue
        if lines:
            yield " ".join(lines)
            lines = []
    if lines:
        yield " ".join(lines)


def description(text: str) -> str:
    """The first sentence of a page's prose that does not only introduce a block (`...:`)."""
    for paragraph in paragraphs(text):
        plain = LINK.sub(r"\1", paragraph).replace("`", "").replace("**", "")
        sentence = SENTENCE_END.split(plain, maxsplit=1)[0]
        if not sentence.endswith(":"):
            return sentence.rstrip(".")
    return ""


def absolute_links(text: str, path: str, site: str, repo: str) -> str:
    """Resolve the page's relative links against its published URL.

    `llms-full.txt` holds every page at one URL, so a link copied verbatim (`foo.md` on
    `rules/index.md`) would resolve beside it. Pages become their site URLs, other files their
    site paths, and files outside `docs/` their place in the repository.
    """

    def resolve(match: re.Match[str]) -> str:
        target = match.group(2)
        if is_external(target):
            return match.group(0)
        ref, hash_, anchor = target.partition("#")
        resolved = posixpath.normpath(posixpath.join(posixpath.dirname(path), ref)) if ref else path
        if resolved.startswith("../"):
            url = f"{repo}/blob/main/{resolved.removeprefix('../')}"
        elif resolved.endswith(".md"):
            url = page_url(site, resolved)
        else:
            url = f"{site}/{resolved}"
        return f"{match.group(1)}{url}{hash_}{anchor}{match.group(3)}"

    out = []
    fence = None
    for line in text.splitlines(keepends=True):
        stripped = line.strip()
        if fence:
            fence = None if stripped.startswith(fence) else fence
        elif stripped.startswith(FENCES):
            fence = stripped[:3]
        else:
            line = LINK_TARGET.sub(resolve, line)
        out.append(line)
    return "".join(out)


def title(text: str, fallback: str) -> str:
    for line in text.splitlines():
        if line.startswith("# "):
            return line[2:].strip()
    return fallback


def is_external(target: str) -> bool:
    return bool(SCHEME.match(target))


def walk(nav: list, section: str, out: list[tuple[str, str, str]]) -> None:
    """Flatten the nav into `(section, title, target)`.

    An entry is `{title = target}`, `{title = [entries]}` (a section), or a bare target
    (`"index.md"`, titled by its page); a target is a page or an external URL.
    """
    for item in nav:
        entries = item.items() if isinstance(item, dict) else [("", item)]
        for name, value in entries:
            if isinstance(value, str):
                out.append((section, name, value))
            else:
                walk(value, name, out)


def build() -> tuple[str, str]:
    project = tomllib.loads((ROOT / "zensical.toml").read_text(encoding="utf-8"))["project"]
    site = project["site_url"].rstrip("/")
    repo = project["repo_url"].rstrip("/")
    pages: list[tuple[str, str, str]] = []
    walk(project["nav"], "Docs", pages)
    rules = sorted(p for p in (DOCS / "rules").glob("*.md") if p.name != "index.md")
    pages += [("Rules", "", f"rules/{p.name}") for p in rules]

    head = [f"# {project['site_name']}", "", f"> {project['site_description']}", ""]
    index = list(head)
    full = list(head)
    current = None
    for section, name, path in pages:
        if section != current:
            index += ["", f"## {section}", ""]
            current = section
        if is_external(path):
            index.append(f"- [{name or path}]({path})")
            continue
        text = (DOCS / path).read_text(encoding="utf-8")
        text = text.replace("<!-- Generated by scripts/build_docs.py. Do not edit by hand. -->\n", "")
        page_title = name or title(text, path)
        desc = description(text.split("\n", 1)[1] if text.startswith("# ") else text)
        index.append(f"- [{page_title}]({page_url(site, path)}): {desc}")
        body = absolute_links(text.strip(), path, site, repo)
        full += ["", "---", "", f"Source: {page_url(site, path)}", "", body, ""]
    return "\n".join(index).strip() + "\n", "\n".join(full).strip() + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="fail if the files are stale")
    args = parser.parse_args()
    llms, full = build()
    files = {LLMS: llms, LLMS_FULL: full}
    stale = [p for p, t in files.items() if not p.exists() or p.read_text(encoding="utf-8") != t]
    if args.check:
        if stale:
            print("llms.txt is out of date; run `python scripts/build_llms_txt.py`:")
            for p in stale:
                print(f"  {p.relative_to(ROOT)}")
            return 1
        print("llms.txt and llms-full.txt are up to date.")
        return 0
    for p, t in files.items():
        p.write_text(t, encoding="utf-8")
    print(f"Wrote {', '.join(str(p.relative_to(ROOT)) for p in files)}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
