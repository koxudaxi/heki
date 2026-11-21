"""Tests of `ansi_svg.py`, which draws the colored output of `heki check` for the documentation."""

from __future__ import annotations

import ansi_svg
import pytest
from ansi_svg import Run, Style

BOLD_RED = Style(bold=True, foreground=1)


def row_runs(text: str, columns: int = 100) -> list[list[Run]]:
    lines = ansi_svg.styled_lines(text)
    return [ansi_svg.runs(row) for line in lines for row in ansi_svg.wrapped(line, columns)]


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        pytest.param(
            "\x1b[1m\x1b[31mEFF101\x1b[0m (x)",
            [[Run(0, "EFF101", BOLD_RED), Run(6, " (x)", Style())]],
            id="codes one by one, then a reset",
        ),
        pytest.param("\x1b[1;31mEFF101\x1b[m", [[Run(0, "EFF101", BOLD_RED)]], id="combined codes"),
        pytest.param(
            "\x1b[94;1m|\x1b[22m|\x1b[39m|",
            [
                [
                    Run(0, "|", Style(bold=True, foreground=12)),
                    Run(1, "|", Style(foreground=12)),
                    Run(2, "|", Style()),
                ]
            ],
            id="bright color, and partial resets",
        ),
        pytest.param(
            "\x1b[33ma\nb\x1b[0m\n",
            [[Run(0, "a", Style(foreground=3))], [Run(0, "b", Style(foreground=3))]],
            id="a style lasts across lines",
        ),
        pytest.param(
            "  \x1b[1;94m|\x1b[0m      \x1b[43m \x1b[0m",
            [[Run(2, "|", Style(bold=True, foreground=12)), Run(9, " ", Style(background=3))]],
            id="blank runs draw nothing unless they have a background",
        ),
    ],
)
def test_runs_take_the_style_of_the_codes_before_them(text: str, expected: list[list[Run]]) -> None:
    assert row_runs(text) == expected


@pytest.mark.parametrize(
    "text",
    [
        pytest.param("\x1b[38;5;208mx", id="256-color code"),
        pytest.param("\x1b]8;;https://example.com\x1b\\link", id="hyperlink"),
        pytest.param("a\tb", id="tab"),
    ],
)
def test_what_a_16_color_terminal_would_not_show_as_written_is_an_error(text: str) -> None:
    with pytest.raises(ValueError):
        ansi_svg.styled_lines(text)


def test_long_lines_wrap_at_the_last_space_that_fits_and_keep_their_style() -> None:
    text = "\x1b[1mbold words\x1b[0m and a verylongword"
    assert row_runs(text, columns=12) == [
        [Run(0, "bold words", Style(bold=True))],
        [Run(0, "and a", Style())],
        [Run(0, "verylongword", Style())],
    ]
    assert row_runs("abcdefgh", columns=3) == [
        [Run(0, "abc", Style())],
        [Run(0, "def", Style())],
        [Run(0, "gh", Style())],
    ]


def test_runs_are_placed_by_cell_and_escaped() -> None:
    svg = ansi_svg.render("\x1b[1;31m<a & b>\x1b[0m\n漢字 x\n", columns=100, title="t <1>")
    padding, cell = ansi_svg.PADDING, ansi_svg.CELL_WIDTH
    assert "<title>t &lt;1&gt;</title>" in svg
    assert f'<tspan x="{padding}" textLength="{7 * cell}" class="b f1">&lt;a &amp; b&gt;</tspan>' in svg
    # A wide character takes two cells.
    assert f'<tspan x="{padding}" textLength="{6 * cell}">漢字 x</tspan>' in svg
    # Only the classes the text uses get a rule.
    assert ".b{font-weight:700}" in svg and ".f1{" in svg and ".i{" not in svg and ".f2{" not in svg
    width = 2 * padding + 7 * cell
    height = ansi_svg.TITLE_BAR + 2 * padding + 2 * ansi_svg.LINE_HEIGHT
    assert svg.startswith(f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" ')
