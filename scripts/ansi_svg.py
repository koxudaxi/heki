"""Draw text with ANSI color codes as a terminal window, in SVG.

`build_docs.py` draws `docs/assets/example-check.svg` with it, from what `heki check --color
always` prints for the README's examples. It reads the SGR codes of a 16-color terminal: bold,
dim, italic, underline, the foreground and background colors, and their resets. Any other escape
sequence or control character is an error, so that nothing is drawn other than a terminal would
show it.

Each run of one style is placed at its column and stretched to its width in cells
(`textLength`), so the columns line up whatever monospace font the viewer has.
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, replace
from xml.sax.saxutils import escape

SGR = re.compile(r"\x1b\[([0-9;]*)m")

# The geometry, in pixels: a cell is 0.6 em wide, as in most monospace fonts.
FONT_SIZE = 15
CELL_WIDTH = 9
LINE_HEIGHT = 21
# From the top of a line to the baseline of its text.
BASELINE = 16
PADDING = 18
TITLE_BAR = 34
RADIUS = 10

# A dark terminal for the site's teal theme (`docs/stylesheets/heki.css`): cyan is the snake's
# color in the logo, and the other colors keep their contrast on the background.
BACKGROUND = "#172328"
TITLE_BAR_COLOR = "#22323a"
FOREGROUND = "#d6e1e4"
BUTTONS = ("#ff5f57", "#febc2e", "#28c840")
PALETTE = (
    "#3b4a50",  # black
    "#f47174",  # red
    "#7fd18b",  # green
    "#f2c45a",  # yellow
    "#5aa9e6",  # blue
    "#c792ea",  # magenta
    "#5fd8c8",  # cyan
    "#d6e1e4",  # white
    "#6c7f86",  # bright black
    "#ff9496",  # bright red
    "#a5e6ae",  # bright green
    "#ffd97a",  # bright yellow
    "#7cc0ff",  # bright blue
    "#ddb3f5",  # bright magenta
    "#91ede1",  # bright cyan
    "#ffffff",  # bright white
)
FONT_FAMILY = 'ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace'


@dataclass(frozen=True)
class Style:
    bold: bool = False
    dim: bool = False
    italic: bool = False
    underline: bool = False
    # Indexes into `PALETTE`; `None` is the terminal's default.
    foreground: int | None = None
    background: int | None = None


@dataclass(frozen=True)
class Run:
    """Text of one style that starts at `column` of its row."""

    column: int
    text: str
    style: Style

    @property
    def width(self) -> int:
        return text_width(self.text)


def cell_width(char: str) -> int:
    return 2 if unicodedata.east_asian_width(char) in "WF" else 1


def text_width(text: str) -> int:
    return sum(cell_width(c) for c in text)


def with_sgr(style: Style, parameters: str) -> Style:
    """`style` after the codes of one `ESC [ ... m` sequence; no code at all resets it."""
    for code in [int(p) if p else 0 for p in parameters.split(";")]:
        match code:
            case 0:
                style = Style()
            case 1:
                style = replace(style, bold=True)
            case 2:
                style = replace(style, dim=True)
            case 3:
                style = replace(style, italic=True)
            case 4:
                style = replace(style, underline=True)
            case 22:
                style = replace(style, bold=False, dim=False)
            case 23:
                style = replace(style, italic=False)
            case 24:
                style = replace(style, underline=False)
            case 39:
                style = replace(style, foreground=None)
            case 49:
                style = replace(style, background=None)
            case _ if 30 <= code <= 37:
                style = replace(style, foreground=code - 30)
            case _ if 90 <= code <= 97:
                style = replace(style, foreground=code - 90 + 8)
            case _ if 40 <= code <= 47:
                style = replace(style, background=code - 40)
            case _ if 100 <= code <= 107:
                style = replace(style, background=code - 100 + 8)
            case _:
                raise ValueError(f"unsupported SGR code {code} in `ESC[{parameters}m`")
    return style


def styled_lines(text: str) -> list[list[tuple[str, Style]]]:
    """The characters of each line with their style; a style lasts across lines until it is
    reset, as in a terminal."""
    lines: list[list[tuple[str, Style]]] = [[]]
    style = Style()
    pieces = SGR.split(text)
    for i, piece in enumerate(pieces):
        if i % 2:
            style = with_sgr(style, piece)
            continue
        for char in piece:
            if char == "\n":
                lines.append([])
            elif unicodedata.category(char) == "Cc":
                raise ValueError(f"unsupported control character {char!r}")
            else:
                lines[-1].append((char, style))
    if not lines[-1]:
        lines.pop()
    return lines


def wrapped(line: list[tuple[str, Style]], columns: int) -> list[list[tuple[str, Style]]]:
    """`line` in rows of at most `columns` cells, broken at the last space that fits (the space
    is dropped), or inside a word longer than a row."""
    rows = []
    while text_width("".join(c for c, _ in line)) > columns:
        fits, used = 0, 0
        while used + cell_width(line[fits][0]) <= columns:
            used += cell_width(line[fits][0])
            fits += 1
        fits = max(fits, 1)
        space = next((i for i in range(fits, 0, -1) if line[i][0] == " "), None)
        if space is None:
            rows.append(line[:fits])
            line = line[fits:]
        else:
            rows.append(line[:space])
            line = line[space + 1 :]
    rows.append(line)
    return rows


def runs(row: list[tuple[str, Style]]) -> list[Run]:
    """The runs of one style in `row`, without the blank ones that draw nothing."""
    out: list[Run] = []
    column = 0
    for char, style in row:
        if out and out[-1].style == style:
            out[-1] = replace(out[-1], text=out[-1].text + char)
        else:
            out.append(Run(column, char, style))
        column += cell_width(char)
    return [
        r for r in out if r.text.strip() or r.style.background is not None or r.style.underline
    ]


EFFECTS = {
    "bold": ("b", "font-weight:700"),
    "dim": ("d", "fill-opacity:0.7"),
    "italic": ("i", "font-style:italic"),
    "underline": ("u", "text-decoration:underline"),
}


def classes(style: Style) -> list[str]:
    names = [name for effect, (name, _) in EFFECTS.items() if getattr(style, effect)]
    if style.foreground is not None:
        names.append(f"f{style.foreground}")
    return names


def stylesheet(styles: set[Style]) -> str:
    """The rules of the classes `styles` use."""
    used = {name for style in styles for name in classes(style)}
    rules = [
        f"text{{font-family:{FONT_FAMILY};font-size:{FONT_SIZE}px;white-space:pre;"
        f"fill:{FOREGROUND}}}",
        *(f".{name}{{{rule}}}" for name, rule in EFFECTS.values() if name in used),
        *(f".f{i}{{fill:{color}}}" for i, color in enumerate(PALETTE) if f"f{i}" in used),
    ]
    return "\n".join(rules)


def text_element(top: int, row: list[Run]) -> list[str]:
    """The backgrounds and the text of one row whose top is at `top`."""
    out = [
        f'<rect x="{PADDING + CELL_WIDTH * run.column}" y="{top}" width="{CELL_WIDTH * run.width}" '
        f'height="{LINE_HEIGHT}" fill="{PALETTE[run.style.background]}"/>'
        for run in row
        if run.style.background is not None
    ]
    spans = []
    for run in row:
        css = " ".join(classes(run.style))
        spans.append(
            f'<tspan x="{PADDING + CELL_WIDTH * run.column}" textLength="{CELL_WIDTH * run.width}"'
            + (f' class="{css}"' if css else "")
            + f">{escape(run.text)}</tspan>"
        )
    if spans:
        out.append(f'<text y="{top + BASELINE}" xml:space="preserve">{"".join(spans)}</text>')
    return out


def render(text: str, columns: int, title: str) -> str:
    """An SVG of a terminal window, at most `columns` cells wide, that shows `text`."""
    rows = [row for line in styled_lines(text) for row in wrapped(line, columns)]
    width = 2 * PADDING + CELL_WIDTH * max(text_width("".join(c for c, _ in r)) for r in rows)
    height = TITLE_BAR + 2 * PADDING + LINE_HEIGHT * len(rows)
    row_runs = [runs(row) for row in rows]
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img">',
        f"<title>{escape(title)}</title>",
        f"<style>\n{stylesheet({run.style for row in row_runs for run in row})}\n</style>",
        f'<rect width="{width}" height="{height}" rx="{RADIUS}" fill="{TITLE_BAR_COLOR}"/>',
        f'<path d="M0 {TITLE_BAR}H{width}V{height - RADIUS}A{RADIUS} {RADIUS} 0 0 1 '
        f'{width - RADIUS} {height}H{RADIUS}A{RADIUS} {RADIUS} 0 0 1 0 {height - RADIUS}Z" '
        f'fill="{BACKGROUND}"/>',
        *(
            f'<circle cx="{20 + 20 * i}" cy="{TITLE_BAR // 2}" r="6" fill="{color}"/>'
            for i, color in enumerate(BUTTONS)
        ),
    ]
    for number, row in enumerate(row_runs):
        out += text_element(TITLE_BAR + PADDING + LINE_HEIGHT * number, row)
    out.append("</svg>")
    return "\n".join(out) + "\n"
