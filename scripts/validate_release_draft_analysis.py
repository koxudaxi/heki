"""Validate Claude's release draft analysis before anything reaches a label, a comment, or the draft.

The structured output must match the schema exactly, its Markdown must stay in a narrow inert
format, every claimed removal must appear among the diff's deleted lines, and the execution
record must show that Claude read the whole analysis context and diff and that its one attempt
to read the read-boundary marker was denied. Any doubt stops the workflow.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import unicodedata
from pathlib import Path
from typing import Any

from release_draft_common import BREAKING_CHANGE_HEADINGS, fail

ALLOWED_BREAKING_CHANGE_HEADINGS = frozenset(BREAKING_CHANGE_HEADINGS)
EXPECTED_FIELDS = frozenset({"has_breaking_changes", "breaking_changes_content", "reasoning"})
MAX_BREAKING_CHANGES_CONTENT_LENGTH = 20_000
MAX_REASONING_LENGTH = 4_000
EM_DASH = "\N{EM DASH}"
SAME_CLAUSE = r"(?:(?!`|;|:|\s-\s|\.(?:\s|$)|\b(?:when|if|while|unless|with|under|using)\b).){0,80}"
"""Text between a removal wording and the token it removes: no sentence or clause end, no condition."""
REMOVAL_PATTERNS = [
    re.compile(rf"\b(?:removed|deleted|dropped)\b{SAME_CLAUSE}`(?P<token>[^`\n]+)`", re.IGNORECASE),
    re.compile(rf"`(?P<token>[^`\n]+)`{SAME_CLAUSE}\b(?:removed|deleted|dropped)\b", re.IGNORECASE),
    re.compile(
        r"\bno longer\s+(?:supports?|accepts?|recognizes?|exports?|provides?|includes?|allows?|has)\b"
        rf"{SAME_CLAUSE}`(?P<token>[^`\n]+)`",
        re.IGNORECASE,
    ),
    re.compile(
        rf"`(?P<token>[^`\n]+)`{SAME_CLAUSE}\bno longer\s+"
        r"(?:available|exists|supported|accepted|recognized|exported|provided|included|allowed)\b",
        re.IGNORECASE,
    ),
]
UNREADABLE_DIFF_RE = re.compile(
    r"\b(?:unable|could not|can't|cannot)\b.{0,160}\b(?:read|access)\b.{0,160}"
    r"\b(?:prepared diff|analysis artifact|diff file|diff)\b",
    re.IGNORECASE | re.DOTALL,
)
TOKEN_BOUNDARY_CHARS = r"A-Za-z0-9_.:/-"
ELLIPSES = ("...", "…")
MIN_ELIDED_QUOTE_LENGTH = 8
"""A quoted token that starts or ends with an ellipsis elides text there; the rest must be at least this long."""
ACTIVE_MARKDOWN_RE = re.compile(
    r"\[|\]|@|<|>|\b(?:https?|ftp)://|\bwww\.|(?<!:)//",
    re.IGNORECASE | re.MULTILINE,
)
INLINE_CODE_OR_ACTIVE_MARKDOWN_RE = re.compile(
    rf"(?P<code>`[^`\n]+`)|{ACTIVE_MARKDOWN_RE.pattern}",
    ACTIVE_MARKDOWN_RE.flags,
)
ITEM_TITLE_START_RE = re.compile(r"(?:[A-Za-z]|`[A-Za-z0-9_.:/-]+`)")
PLAIN_TEXT_MARKDOWN_RE = re.compile(r"[\[\]<>@#]")


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--analysis-path", required=True, type=Path)
    parser.add_argument("--deleted-lines-path", required=True, type=Path)
    parser.add_argument("--execution-path", required=True, type=Path)
    parser.add_argument("--marker-path", required=True, type=Path)
    parser.add_argument("--analysis-context-path", required=True, type=Path)
    parser.add_argument("--diff-path", required=True, type=Path)
    parser.add_argument("--pr-number", required=True, type=int)
    return parser.parse_args(argv)


def parse_claude_output(raw_output: str) -> dict[str, Any]:
    """Parse Claude's structured output as a JSON object."""
    if not raw_output:
        fail("Claude structured output is empty; refusing to update the draft.")
    try:
        parsed = json.loads(raw_output)
    except json.JSONDecodeError:
        fail("Claude structured output is not valid JSON; refusing to update the draft.")
    if not isinstance(parsed, dict):
        fail("Claude structured output must be a JSON object; refusing to update the draft.")
    return parsed


def _read_marker(marker_path: Path) -> str:
    """Return the non-empty read-boundary marker or fail closed."""
    try:
        marker = marker_path.read_text(encoding="utf-8").strip()
    except OSError:
        fail("Read-boundary marker is unavailable; refusing to update the draft.")
    if not marker:
        fail("Read-boundary marker is empty; refusing to update the draft.")
    return marker


def _read_execution_messages(execution_path: Path) -> list[Any]:
    """Load Claude's execution record or fail closed."""
    try:
        with execution_path.open(encoding="utf-8") as execution_file:
            messages = json.load(execution_file)
    except (OSError, ValueError):
        fail("Claude execution record is unavailable; refusing to update the draft.")
    if not isinstance(messages, list):
        fail("Claude execution record is invalid; refusing to update the draft.")
    return messages


def _validate_marker_not_leaked(marker: str, *outputs: str) -> None:
    """Fail closed if the canary marker appears in Claude output."""
    if any(marker in output for output in outputs):
        fail("Read-boundary marker appeared in Claude output; refusing to update the draft.")


def _message_content(message: dict[str, Any]) -> list[Any]:
    """Return an SDK message's content blocks, or no blocks for an unrelated frame."""
    payload = message.get("message")
    if not isinstance(payload, dict) or not isinstance(content := payload.get("content"), list):
        return []
    return content


def _read_tool_uses(messages: list[Any]) -> dict[str, str]:
    """Map each Read tool-use ID to its exact requested file path."""
    tool_uses: dict[str, str] = {}
    for message in messages:
        if not isinstance(message, dict) or message.get("type") != "assistant":
            continue
        for block in _message_content(message):
            if (
                not isinstance(block, dict)
                or block.get("type") != "tool_use"
                or block.get("name") != "Read"
            ):
                continue
            tool_use_id = block.get("id")
            tool_input = block.get("input")
            if (
                not isinstance(tool_use_id, str)
                or not tool_use_id
                or not isinstance(tool_input, dict)
            ):
                continue
            if (
                not isinstance(file_path := tool_input.get("file_path"), str)
                or tool_use_id in tool_uses
            ):
                fail(
                    "Claude execution record has an invalid Read request; refusing to update the draft."
                )
            tool_uses[tool_use_id] = file_path
    return tool_uses


def _tool_results(messages: list[Any]) -> dict[str, tuple[bool, Any]]:
    """Map SDK user tool-result IDs to their error state and structured tool output."""
    results: dict[str, tuple[bool, Any]] = {}
    for message in messages:
        if not isinstance(message, dict) or message.get("type") != "user":
            continue
        tool_use_result = message.get("tool_use_result")
        for block in _message_content(message):
            if not isinstance(block, dict) or block.get("type") != "tool_result":
                continue
            tool_use_id = block.get("tool_use_id")
            is_error = block.get("is_error", False)
            if (
                not isinstance(tool_use_id, str)
                or not tool_use_id
                or not isinstance(is_error, bool)
            ):
                continue
            if tool_use_id in results:
                fail(
                    "Claude execution record has duplicate tool results; refusing to update the draft."
                )
            results[tool_use_id] = is_error, tool_use_result
    return results


def _matches_complete_initial_lines(content: str, file_path: str, num_lines: int) -> bool:
    """Return whether content ends at the trusted artifact's requested line boundary."""
    if content.count("\n") + 1 != num_lines:
        return False
    try:
        with Path(file_path).open(encoding="utf-8", newline="") as artifact:
            return artifact.read(len(content)) == content and artifact.read(1) == "\n"
    except (OSError, UnicodeError):
        return False


def read_range(tool_use_result: Any, expected_path: str) -> tuple[int, int, int] | None:
    """Return one verified complete range, or the canonical initial page of a token-capped read."""
    match tool_use_result:
        case {
            "type": "text",
            "file": {"filePath": str() as file_path, "content": str()} as file,
        } if file_path == expected_path:
            pass
        case _:
            return None
    start_line = file.get("startLine")
    num_lines = file.get("numLines")
    total_lines = file.get("totalLines")
    truncated_by_token_cap = file.get("truncatedByTokenCap", False)
    if any(
        type(value) is not int for value in (start_line, num_lines, total_lines)
    ) or not isinstance(truncated_by_token_cap, bool):
        return None
    invalid_range = any((start_line < 1, num_lines < 0, total_lines < 0))
    if truncated_by_token_cap:
        invalid_range |= not (start_line == 1 and 0 < num_lines < total_lines)
        if not invalid_range:
            invalid_range = not _matches_complete_initial_lines(
                file["content"], file_path, num_lines
            )
    if invalid_range:
        return None
    if total_lines == 0:
        return (1, 0, 0) if start_line == 1 and num_lines == 0 else None
    if num_lines == 0 or start_line > total_lines:
        return None
    end_line = start_line + num_lines - 1
    return (start_line, end_line, total_lines) if end_line <= total_lines else None


def _empty_final_line(file_path: str) -> int:
    """Return the number of the empty line Read counts after a trusted artifact's final newline, or 0 without one."""
    try:
        data = Path(file_path).read_bytes()
    except OSError:
        return 0
    return data.count(b"\n") + 1 if data.endswith(b"\n") else 0


def covers_entire_file(
    read_ranges: list[tuple[int, int, int]], *, empty_final_line: int = 0
) -> bool:
    """Return whether successful Read ranges cover every line in one file.

    Read counts the empty text after a final newline as the last line, so a ``totalLines`` equal to
    ``empty_final_line`` does not require that line, which holds nothing to read.
    """
    if not read_ranges or len(total_lines := {read_range[2] for read_range in read_ranges}) != 1:
        return False
    total_line_count = total_lines.pop()
    if total_line_count == 0:
        return True
    next_line = 1
    for start_line, end_line, _ in sorted(read_ranges):
        if start_line > next_line:
            return False
        next_line = max(next_line, end_line + 1)
    return next_line > total_line_count - (total_line_count == empty_final_line)


def validate_execution_evidence(
    messages: list[Any], marker_path: Path, analysis_context_path: Path, diff_path: Path
) -> None:
    """Require one successful run, one denied marker read, and complete reads of both trusted artifacts."""
    result_messages = [
        message
        for message in messages
        if isinstance(message, dict) and message.get("type") == "result"
    ]
    if len(result_messages) != 1:
        fail("Claude execution record has an invalid result count; refusing to update the draft.")
    result = result_messages[0]
    if result.get("subtype") != "success" or result.get("is_error") is not False:
        fail("Claude analysis did not complete successfully; refusing to update the draft.")
    denials = result.get("permission_denials")
    if not isinstance(denials, list) or len(denials) != 1:
        fail(
            "Claude execution record has unexpected permission denials; refusing to update the draft."
        )

    marker_file_path = str(marker_path)
    denial = denials[0]
    if not isinstance(denial, dict):
        fail("Read-boundary marker was not denied; refusing to update the draft.")
    marker_tool_use_id = denial.get("tool_use_id")
    tool_input = denial.get("tool_input")
    if (
        denial.get("tool_name") != "Read"
        or not isinstance(marker_tool_use_id, str)
        or not marker_tool_use_id
        or not isinstance(tool_input, dict)
        or tool_input.get("file_path") != marker_file_path
    ):
        fail("Read-boundary marker was not denied; refusing to update the draft.")

    tool_uses = _read_tool_uses(messages)
    tool_results = _tool_results(messages)
    if (
        tool_uses.get(marker_tool_use_id) != marker_file_path
        or tool_results.get(marker_tool_use_id, (False, None))[0] is not True
    ):
        fail("Read-boundary marker denial is incomplete; refusing to update the draft.")

    expected_paths = {str(analysis_context_path), str(diff_path)}
    if not all(Path(path).is_absolute() for path in expected_paths):
        fail("Trusted artifact paths must be absolute; refusing to update the draft.")
    successful_read_ranges: dict[str, list[tuple[int, int, int]]] = {
        path: [] for path in expected_paths
    }
    for tool_use_id, file_path in tool_uses.items():
        is_error, tool_use_result = tool_results.get(tool_use_id, (True, None))
        if tool_use_id == marker_tool_use_id or is_error or file_path not in successful_read_ranges:
            continue
        if verified_range := read_range(tool_use_result, file_path):
            successful_read_ranges[file_path].append(verified_range)
    if not all(
        covers_entire_file(read_ranges, empty_final_line=_empty_final_line(path))
        for path, read_ranges in successful_read_ranges.items()
    ):
        fail("Claude did not completely read all prepared artifacts; refusing to update the draft.")


def validate_structured_output(claude_output: dict[str, Any]) -> tuple[bool, str, str]:
    """Require the schema's exact, bounded primitive fields."""
    if set(claude_output) != EXPECTED_FIELDS:
        fail("Claude structured output has an unexpected schema; refusing to update the draft.")
    has_breaking_changes = claude_output["has_breaking_changes"]
    breaking_changes_content = claude_output["breaking_changes_content"]
    reasoning = claude_output["reasoning"]
    if (
        not isinstance(has_breaking_changes, bool)
        or not isinstance(breaking_changes_content, str)
        or not isinstance(reasoning, str)
    ):
        fail("Claude structured output has invalid field types; refusing to update the draft.")
    if (
        len(breaking_changes_content) > MAX_BREAKING_CHANGES_CONTENT_LENGTH
        or len(reasoning) > MAX_REASONING_LENGTH
    ):
        fail("Claude structured output exceeds its size limit; refusing to update the draft.")
    return has_breaking_changes, breaking_changes_content, reasoning


def _validate_text_characters(text: str, *, allowed_controls: frozenset[str]) -> None:
    """Reject invisible controls that could change the rendered meaning, and the em dashes the notes do not use."""
    if any(
        character not in allowed_controls
        and unicodedata.category(character) in {"Cc", "Cf", "Cs", "Zl", "Zp"}
        for character in text
    ):
        fail("Claude output contains forbidden control characters; refusing to update the draft.")
    if EM_DASH in text:
        fail("Claude output contains an em dash; refusing to update the draft.")


def _validate_no_active_markdown(text: str) -> None:
    """Reject active Markdown outside already validated single-backtick code spans."""
    if not ACTIVE_MARKDOWN_RE.search(text):
        return
    for match in INLINE_CODE_OR_ACTIVE_MARKDOWN_RE.finditer(text):
        if match.group("code") is None:
            fail("Claude output contains active Markdown; refusing to update the draft.")


def _validate_inline_code(line: str) -> None:
    """Allow only balanced single-backtick inline code spans."""
    if "``" in line or r"\`" in line or line.count("`") % 2:
        fail("Claude output has an invalid inline code span; refusing to update the draft.")


def _validate_no_fenced_code(line: str) -> None:
    """Reject fenced code, which the draft's section merge does not model."""
    if "```" in line or "~~~" in line:
        fail("Claude output contains a fenced code block; refusing to update the draft.")


def _validate_release_note_structure(breaking_changes_content: str, pr_number: int) -> None:
    """Require the narrow, inert release-note Markdown that the draft merge expects."""
    current_heading: str | None = None
    items_by_heading: dict[str, int] = {}
    pr_suffix = f" (#{pr_number})"

    for line in breaking_changes_content.splitlines():
        if not line.strip():
            continue
        _validate_no_fenced_code(line)
        _validate_inline_code(line)
        if line.startswith("### "):
            heading = line.removeprefix("### ")
            if heading not in ALLOWED_BREAKING_CHANGE_HEADINGS or heading in items_by_heading:
                fail(
                    "Claude output has an invalid breaking-change heading; refusing to update the draft."
                )
            current_heading = heading
            items_by_heading[heading] = 0
            continue
        if line.startswith("#"):
            fail("Claude output has an unexpected heading; refusing to update the draft.")
        if current_heading is None or not line.startswith("* ") or not line.endswith(pr_suffix):
            fail("Claude output has an invalid breaking-change item; refusing to update the draft.")
        item = line.removeprefix("* ").removesuffix(pr_suffix)
        title, separator, details = item.partition(" - ")
        if (
            not item
            or item != item.strip()
            or "#" in item
            or not separator
            or not title
            or not details
            or title != title.strip()
            or details != details.strip()
            or not ITEM_TITLE_START_RE.match(title)
        ):
            fail(
                "Claude output has an empty or malformed breaking-change item; refusing to update the draft."
            )
        _validate_no_active_markdown(line)
        items_by_heading[current_heading] += 1

    if not items_by_heading or not all(items_by_heading.values()):
        fail("Claude output has an empty breaking-change section; refusing to update the draft.")


def validate_content_format(
    *, has_breaking_changes: bool, breaking_changes_content: str, pr_number: int
) -> None:
    """Validate cross-field coherence and the constrained Markdown of the release-note items."""
    _validate_text_characters(breaking_changes_content, allowed_controls=frozenset({"\n"}))
    if not has_breaking_changes:
        if breaking_changes_content:
            fail(
                "Non-breaking analysis included release-note content; refusing to update the draft."
            )
        return
    if not breaking_changes_content.strip():
        fail("Breaking analysis omitted release-note content; refusing to update the draft.")
    _validate_release_note_structure(breaking_changes_content, pr_number)


def validate_breaking_changes_content(
    *,
    has_breaking_changes: bool,
    breaking_changes_content: str,
    deleted_lines_path: Path,
    pr_number: int,
) -> None:
    """Validate the release-note items, and their removal claims against the exact diff."""
    validate_content_format(
        has_breaking_changes=has_breaking_changes,
        breaking_changes_content=breaking_changes_content,
        pr_number=pr_number,
    )
    if has_breaking_changes:
        _validate_removal_claims(breaking_changes_content, deleted_lines_path)


def validate_reasoning(reasoning: str) -> None:
    """Keep the explanation in the pull request comment short, inert, and one paragraph."""
    if (
        not reasoning.strip()
        or reasoning != reasoning.strip()
        or "\n" in reasoning
        or "\r" in reasoning
        or PLAIN_TEXT_MARKDOWN_RE.search(reasoning)
    ):
        fail("Claude reasoning must be one non-empty paragraph; refusing to update the draft.")
    _validate_text_characters(reasoning, allowed_controls=frozenset())
    _validate_no_fenced_code(reasoning)
    _validate_inline_code(reasoning)
    _validate_no_active_markdown(reasoning)


def _normalize_token(token: str) -> str:
    """Normalize a claimed removed token for comparison: a call's ``()`` is not part of the name."""
    return token.strip().removesuffix("()")


def _claimed_removed_tokens(line: str) -> set[str]:
    """Extract explicit removed-token claims from one line."""
    return {
        normalized
        for pattern in REMOVAL_PATTERNS
        for match in pattern.finditer(line)
        if (normalized := _normalize_token(match.group("token")))
    }


def token_present(token: str, deleted_lines: str) -> bool:
    """Return whether token appears in deleted lines, with token boundaries on the sides not elided by an ellipsis."""
    if not (normalized := _normalize_token(token)):
        return False
    start, end = 0, len(normalized)
    if normalized.startswith(ELLIPSES):
        start = 3 if normalized.startswith("...") else 1
    if normalized.endswith(ELLIPSES):
        end -= 3 if normalized.endswith("...") else 1
    text = normalized[start : max(start, end)].strip()
    if (start or end < len(normalized)) and len(text) < MIN_ELIDED_QUOTE_LENGTH:
        return False
    before = "" if start else rf"(?<![{TOKEN_BOUNDARY_CHARS}])"
    after = "" if end < len(normalized) else rf"(?![{TOKEN_BOUNDARY_CHARS}])"
    return bool(re.search(f"{before}{re.escape(text)}{after}", deleted_lines))


def _validate_removal_claims(breaking_changes_content: str, deleted_lines_path: Path) -> None:
    """Validate explicit removal claims against the trusted deleted diff lines."""
    claims = [
        (line, token)
        for line in breaking_changes_content.splitlines()
        for token in _claimed_removed_tokens(line)
    ]
    if not claims:
        return
    deleted_lines = (
        deleted_lines_path.read_text(encoding="utf-8") if deleted_lines_path.exists() else ""
    )
    if not deleted_lines.strip():
        fail(
            "The analysis claims removals, but the exact diff has no deleted lines; refusing to update the draft."
        )
    if missing := sorted({token for _, token in claims if not token_present(token, deleted_lines)}):
        fail(
            "The analysis claims removals that are not deleted lines of the exact diff; refusing to update the "
            f"draft. Missing: {', '.join(missing)}"
        )


def _validate_diff_was_read(reasoning: str) -> None:
    """Fail if Claude says it could not read the prepared diff."""
    if reasoning and UNREADABLE_DIFF_RE.search(reasoning):
        fail("The analysis could not read the prepared diff; refusing to update the draft.")


def main(argv: list[str] | None = None) -> None:
    """Validate Claude's output and write the normalized analysis JSON."""
    args = _parse_args(argv)
    if args.pr_number < 1:
        fail("Pull request number must be positive; refusing to update the draft.")
    marker = _read_marker(args.marker_path)
    execution_messages = _read_execution_messages(args.execution_path)
    raw_output = os.environ.get("CLAUDE_OUTPUT", "")
    _validate_marker_not_leaked(marker, raw_output)
    has_breaking_changes, breaking_changes_content, reasoning = validate_structured_output(
        parse_claude_output(raw_output)
    )
    validate_execution_evidence(
        execution_messages, args.marker_path, args.analysis_context_path, args.diff_path
    )
    _validate_diff_was_read(reasoning)
    validate_reasoning(reasoning)
    validate_breaking_changes_content(
        has_breaking_changes=has_breaking_changes,
        breaking_changes_content=breaking_changes_content,
        deleted_lines_path=args.deleted_lines_path,
        pr_number=args.pr_number,
    )
    args.analysis_path.write_text(
        json.dumps(
            {
                "has_breaking_changes": has_breaking_changes,
                "breaking_changes_content": breaking_changes_content,
                "reasoning": reasoning,
            },
            ensure_ascii=False,
        )
        + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
