"""Tests of `validate_release_draft_analysis.py`: Claude's answer and its execution record."""

from __future__ import annotations

import json
import re
import shlex
from pathlib import Path
from typing import Any

import pytest
import validate_release_draft_analysis as validator
from helpers import ROOT, run_script
from release_draft_common import BREAKING_CHANGE_HEADINGS

PR = 42
VALID_CONTENT = (
    "### Configuration and CLI Changes\n"
    "* `--watch` is renamed - The option `--watch` is now `--follow`; scripts that pass it must change (#42)\n"
)


class Artifacts:
    """The trusted files of one analysis run, and an execution record that reads them."""

    def __init__(self, directory: Path) -> None:
        self.directory = directory
        self.marker = directory / "marker.txt"
        self.marker.write_text("canary-0123456789\n", encoding="utf-8")
        self.context = directory / "claude" / "analysis-context.md"
        self.context.parent.mkdir()
        self.context.write_text(
            "".join(f"context line {n}\n" for n in range(1, 701)), encoding="utf-8"
        )
        self.diff = directory / "claude" / "pr.diff"
        self.diff.write_text("-    --watch\n+    --follow\n", encoding="utf-8")
        self.deleted = directory / "deleted-lines.txt"
        self.deleted.write_text("    --watch\n", encoding="utf-8")
        self.messages: list[dict[str, Any]] = []
        self.denials: list[dict[str, Any]] = []
        self._next_id = 0

    def read(self, path: Path, start: int, count: int, total: int, **file: Any) -> None:
        self._next_id += 1
        tool_use_id = f"toolu_{self._next_id}"
        self.messages.append(
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tool_use_id,
                            "name": "Read",
                            "input": {"file_path": str(path)},
                        }
                    ]
                },
            }
        )
        self.messages.append(
            {
                "type": "user",
                "message": {"content": [{"type": "tool_result", "tool_use_id": tool_use_id}]},
                "tool_use_result": {
                    "type": "text",
                    "file": {
                        "filePath": str(path),
                        "content": "line\n" * (count - 1) + "line",
                        "startLine": start,
                        "numLines": count,
                        "totalLines": total,
                        **file,
                    },
                },
            }
        )

    def denied_marker_read(self) -> None:
        self._next_id += 1
        tool_use_id = f"toolu_{self._next_id}"
        self.messages.append(
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tool_use_id,
                            "name": "Read",
                            "input": {"file_path": str(self.marker)},
                        }
                    ]
                },
            }
        )
        self.messages.append(
            {
                "type": "user",
                "message": {
                    "content": [
                        {"type": "tool_result", "tool_use_id": tool_use_id, "is_error": True}
                    ]
                },
            }
        )
        self.denials.append(
            {
                "tool_name": "Read",
                "tool_use_id": tool_use_id,
                "tool_input": {"file_path": str(self.marker)},
            }
        )

    def complete_run(self) -> None:
        """Deny the marker once, then read the context in two pages and the diff in one."""
        self.denied_marker_read()
        self.read(self.context, 1, 500, 701)
        self.read(self.context, 501, 200, 701)
        self.read(self.diff, 1, 2, 3)

    def record(self) -> list[dict[str, Any]]:
        return [
            *self.messages,
            {
                "type": "result",
                "subtype": "success",
                "is_error": False,
                "permission_denials": self.denials,
            },
        ]

    def validate(self, output: object, *, record: object = None):
        execution = self.directory / "execution.json"
        execution.write_text(
            json.dumps(self.record() if record is None else record), encoding="utf-8"
        )
        analysis = self.directory / "analysis.json"
        environment = {"CLAUDE_OUTPUT": output if isinstance(output, str) else json.dumps(output)}
        result = run_script(
            "validate_release_draft_analysis.py",
            "--analysis-path",
            str(analysis),
            "--deleted-lines-path",
            str(self.deleted),
            "--execution-path",
            str(execution),
            "--marker-path",
            str(self.marker),
            "--analysis-context-path",
            str(self.context),
            "--diff-path",
            str(self.diff),
            "--pr-number",
            str(PR),
            env={"PATH": "/usr/bin:/bin", **environment},
        )
        return result, json.loads(
            analysis.read_text(encoding="utf-8")
        ) if analysis.exists() else None


@pytest.fixture
def artifacts(tmp_path: Path) -> Artifacts:
    return Artifacts(tmp_path)


def output(
    *,
    breaking: bool = True,
    content: str = VALID_CONTENT,
    reasoning: str = "The option is renamed.",
) -> dict:
    return {
        "has_breaking_changes": breaking,
        "breaking_changes_content": content,
        "reasoning": reasoning,
    }


def test_a_complete_breaking_analysis_is_written(artifacts: Artifacts) -> None:
    artifacts.complete_run()
    result, analysis = artifacts.validate(output())
    assert result.returncode == 0, result.stderr
    assert analysis == output()


def test_an_empty_result_is_written(artifacts: Artifacts) -> None:
    artifacts.complete_run()
    result, analysis = artifacts.validate(
        output(breaking=False, content="", reasoning="Only a test changed.")
    )
    assert result.returncode == 0, result.stderr
    assert analysis == output(breaking=False, content="", reasoning="Only a test changed.")


@pytest.mark.parametrize(
    ("raw", "message"),
    [
        pytest.param("", "structured output is empty", id="empty"),
        pytest.param("{not json", "not valid JSON", id="malformed JSON"),
        pytest.param("[]", "must be a JSON object", id="array"),
        pytest.param(json.dumps({**output(), "extra": 1}), "unexpected schema", id="extra field"),
        pytest.param(
            json.dumps({"has_breaking_changes": True, "reasoning": "x"}),
            "unexpected schema",
            id="missing field",
        ),
        pytest.param(
            json.dumps(output() | {"has_breaking_changes": "true"}),
            "invalid field types",
            id="string boolean",
        ),
        pytest.param(
            json.dumps(output() | {"reasoning": "x" * 4_001}),
            "exceeds its size limit",
            id="long reasoning",
        ),
        pytest.param(
            json.dumps(output() | {"reasoning": "Read canary-0123456789."}),
            "marker appeared",
            id="leaked marker",
        ),
    ],
)
def test_malformed_output_writes_nothing(artifacts: Artifacts, raw: str, message: str) -> None:
    artifacts.complete_run()
    result, analysis = artifacts.validate(raw)
    assert result.returncode == 1
    assert message in result.stderr
    assert analysis is None


@pytest.mark.parametrize(
    ("content", "message"),
    [
        pytest.param(
            "### Breaking Things\n* Title - Detail (#42)\n",
            "invalid breaking-change heading",
            id="unknown heading",
        ),
        pytest.param(
            VALID_CONTENT + VALID_CONTENT, "invalid breaking-change heading", id="repeated heading"
        ),
        pytest.param(
            "* Title - Detail (#42)\n", "invalid breaking-change item", id="item without heading"
        ),
        pytest.param(
            "### Default Rule Changes\n* Title - Detail (#41)\n",
            "invalid breaking-change item",
            id="other number",
        ),
        pytest.param(
            "### Default Rule Changes\n* Title without details (#42)\n",
            "malformed",
            id="no separator",
        ),
        pytest.param(
            "### Default Rule Changes\n* 1 title - Detail (#42)\n",
            "malformed",
            id="title starts with a digit",
        ),
        pytest.param(
            "### Default Rule Changes\n", "empty breaking-change section", id="heading only"
        ),
        pytest.param(
            "### Default Rule Changes\n#### More\n", "unexpected heading", id="deeper heading"
        ),
        pytest.param(
            "### Default Rule Changes\n* Title - See [docs](x) (#42)\n",
            "active Markdown",
            id="link",
        ),
        pytest.param(
            "### Default Rule Changes\n* Title - Ask @someone (#42)\n",
            "active Markdown",
            id="mention",
        ),
        pytest.param(
            "### Default Rule Changes\n* Title - Uses ``x`` (#42)\n",
            "inline code span",
            id="double backticks",
        ),
        pytest.param(
            "### Default Rule Changes\n* Title - ```x``` (#42)\n", "fenced code block", id="fence"
        ),
        pytest.param(
            "### Default Rule Changes\n* Title — now different - Detail (#42)\n",
            "em dash",
            id="em dash",
        ),
        pytest.param(
            "### Default Rule Changes\n* Title - Detail​ (#42)\n",
            "control characters",
            id="zero width",
        ),
        pytest.param("", "omitted release-note content", id="breaking without items"),
    ],
)
def test_malformed_release_notes_write_nothing(
    artifacts: Artifacts, content: str, message: str
) -> None:
    artifacts.complete_run()
    result, analysis = artifacts.validate(output(content=content))
    assert result.returncode == 1
    assert message in result.stderr
    assert analysis is None


def test_inline_code_may_hold_brackets(artifacts: Artifacts) -> None:
    artifacts.complete_run()
    content = (
        "### Output and Editor Integration Changes\n"
        "* JSON field renamed - The field `fix[0]` of `--output-format json` is now `fixes` (#42)\n"
    )
    result, _ = artifacts.validate(output(content=content))
    assert result.returncode == 0, result.stderr


def test_a_non_breaking_analysis_has_no_items(artifacts: Artifacts) -> None:
    artifacts.complete_run()
    result, _ = artifacts.validate(output(breaking=False))
    assert result.returncode == 1
    assert "Non-breaking analysis included release-note content" in result.stderr


@pytest.mark.parametrize(
    ("reasoning", "message"),
    [
        ("Two\nlines.", "one non-empty paragraph"),
        ("See #12.", "one non-empty paragraph"),
        (" Leading space.", "one non-empty paragraph"),
        ("A change — small.", "em dash"),
        (
            "Unable to read the prepared diff because a line was truncated",
            "could not read the prepared diff",
        ),
    ],
)
def test_malformed_reasoning_writes_nothing(
    artifacts: Artifacts, reasoning: str, message: str
) -> None:
    artifacts.complete_run()
    result, analysis = artifacts.validate(output(breaking=False, content="", reasoning=reasoning))
    assert result.returncode == 1
    assert message in result.stderr
    assert analysis is None


@pytest.mark.parametrize(
    ("item", "accepted"),
    [
        ("* Option removed - The option `--watch` was removed (#42)", True),
        ("* Option removed - The option `--follow` was removed (#42)", False),
        ("* Rule dropped - Dropped `MUT105` from the defaults (#42)", False),
        ("* Option gone - `--watch` is no longer supported (#42)", True),
    ],
)
def test_removal_claims_must_be_deleted_lines(
    artifacts: Artifacts, item: str, accepted: bool
) -> None:
    artifacts.complete_run()
    result, _ = artifacts.validate(output(content=f"### Configuration and CLI Changes\n{item}\n"))
    assert (result.returncode == 0) is accepted, result.stderr
    if not accepted:
        assert "claims removals that are not deleted lines" in result.stderr


def test_a_partial_read_writes_nothing(artifacts: Artifacts) -> None:
    artifacts.denied_marker_read()
    artifacts.read(artifacts.context, 1, 500, 701)
    artifacts.read(artifacts.diff, 1, 2, 3)
    result, analysis = artifacts.validate(output())
    assert result.returncode == 1
    assert "did not completely read" in result.stderr
    assert analysis is None


def test_a_token_capped_page_counts_only_when_it_matches_the_file(artifacts: Artifacts) -> None:
    artifacts.denied_marker_read()
    context = artifacts.context.read_text(encoding="utf-8")
    first_page = "".join(context.splitlines(keepends=True)[:300]).removesuffix("\n")
    artifacts.read(artifacts.context, 1, 300, 701, truncatedByTokenCap=True, content=first_page)
    artifacts.read(artifacts.context, 301, 400, 701)
    artifacts.read(artifacts.diff, 1, 2, 3)
    result, _ = artifacts.validate(output())
    assert result.returncode == 0, result.stderr


def test_a_read_of_another_file_does_not_count(artifacts: Artifacts) -> None:
    artifacts.denied_marker_read()
    artifacts.read(artifacts.directory / "elsewhere.md", 1, 701, 701)
    artifacts.read(artifacts.diff, 1, 2, 3)
    result, _ = artifacts.validate(output())
    assert result.returncode == 1
    assert "did not completely read" in result.stderr


@pytest.mark.parametrize("denials", [0, 2])
def test_the_marker_must_be_denied_exactly_once(artifacts: Artifacts, denials: int) -> None:
    for _ in range(denials):
        artifacts.denied_marker_read()
    artifacts.read(artifacts.context, 1, 701, 701)
    artifacts.read(artifacts.diff, 1, 2, 3)
    result, _ = artifacts.validate(output())
    assert result.returncode == 1
    assert "unexpected permission denials" in result.stderr


@pytest.mark.parametrize(
    "record",
    [
        pytest.param({"not": "a list"}, id="not a list"),
        pytest.param([], id="no result"),
        pytest.param(
            [{"type": "result", "subtype": "error_max_turns", "is_error": True}], id="failed run"
        ),
    ],
)
def test_an_invalid_execution_record_writes_nothing(artifacts: Artifacts, record: object) -> None:
    result, analysis = artifacts.validate(output(), record=record)
    assert result.returncode == 1
    assert "refusing to update the draft" in result.stderr
    assert analysis is None


def workflow_schema() -> dict[str, Any]:
    """The JSON schema the workflow passes to Claude with `--json-schema`."""
    workflow = (ROOT / ".github/workflows/release-draft.yml").read_text(encoding="utf-8")
    claude_args = re.search(r"claude_args: \|\n((?:            .*\n)+)", workflow)
    assert claude_args, "the Claude step passes claude_args"
    arguments = shlex.split(claude_args[1])
    return json.loads(arguments[arguments.index("--json-schema") + 1])


def test_the_schema_matches_the_validator() -> None:
    schema = workflow_schema()
    assert set(schema["required"]) == set(schema["properties"]) == validator.EXPECTED_FIELDS
    assert schema["additionalProperties"] is False
    assert (
        schema["properties"]["breaking_changes_content"]["maxLength"]
        == validator.MAX_BREAKING_CHANGES_CONTENT_LENGTH
    )
    assert schema["properties"]["reasoning"]["maxLength"] == validator.MAX_REASONING_LENGTH


@pytest.mark.parametrize(
    ("value", "valid"),
    [
        (output(), True),
        (output(breaking=False, content=""), True),
        ({**output(), "extra": True}, False),
        (output(reasoning=""), False),
        (output(reasoning="Two\nlines"), False),
        (output(reasoning="See #12"), False),
        (output(reasoning="A change — small"), False),
        (output(reasoning="Ask @someone"), False),
        (output(content="x" * 20_001), False),
        ({"has_breaking_changes": "yes", "breaking_changes_content": "", "reasoning": "x"}, False),
    ],
)
def test_the_schema_rejects_what_the_validator_rejects(value: dict[str, Any], valid: bool) -> None:
    jsonschema = pytest.importorskip("jsonschema")
    schema = workflow_schema()
    jsonschema.Draft202012Validator.check_schema(schema)
    assert jsonschema.Draft202012Validator(schema).is_valid(value) is valid


def test_the_prompt_names_every_heading_the_validator_accepts() -> None:
    workflow = (ROOT / ".github/workflows/release-draft.yml").read_text(encoding="utf-8")
    listed = re.findall(r"^            [1-9]\. ([A-Z][A-Za-z ]+): ", workflow, re.MULTILINE)
    assert tuple(listed) == BREAKING_CHANGE_HEADINGS
