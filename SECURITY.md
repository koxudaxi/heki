# Security Policy

## Supported versions

Security fixes are released for the latest version of heki.

| Version | Supported |
| ------- | --------- |
| Latest release on PyPI | Yes |
| Older releases | No |

If a security fix is needed, update to the latest release listed on PyPI or in the GitHub releases page. Security
advisories may list the exact affected and patched version ranges for each issue.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through GitHub Private Vulnerability Reporting:

https://github.com/koxudaxi/heki/security/advisories/new

Do not publish exploit details in a public issue, pull request, discussion, or social media post before a fix is
available and the advisory has been published.

If GitHub Private Vulnerability Reporting is unavailable, contact @koxudaxi on GitHub without details and ask for a
private channel.

Useful reports include:

- the heki version and the platform;
- the command involved: `heki check`, `heki check --fix`, `heki check --watch`, or the language server;
- the configuration (`[tool.heki]` in `pyproject.toml`, or `heki.toml`);
- a minimal Python input that shows the problem;
- expected and actual behavior;
- whether the issue affects confidentiality, integrity, or availability.

## Coordinated disclosure

After a report is received, the maintainer will triage the issue and, when appropriate, create or continue a GitHub
Security Advisory. Fixes may be developed in the advisory's temporary private fork before being merged into the public
repository.

The usual workflow is:

1. Acknowledge and triage the report.
2. Reproduce the issue and determine affected versions.
3. Prepare and test a patch privately when needed.
4. Release a patched version.
5. Publish or update the GitHub Security Advisory.

Please wait to publish write-ups, proof-of-concept details, or exploit analysis until the patched version is available
and the advisory is public.

## Threat model

heki parses and analyzes Python source. It never imports or runs the code it analyzes. Whether a report is a
vulnerability depends on what heki does with its input.

What heki reads and writes:

- the Python files it analyzes, which it only parses;
- the configuration: `[tool.heki]` in `pyproject.toml`, or `heki.toml`;
- baseline and snapshot files, which it reads and writes where the configuration says;
- with `heki check --fix`, the analyzed files, which it rewrites for the fixes it applies;
- the language server, which talks to the editor over stdio.

### What is a vulnerability

- Input that makes heki run code, such as Python source, a configuration file or a baseline that gets executed.
- Input that makes heki read or write outside the project, beyond the paths the configuration names.
- Input that makes `heki check --fix` rewrite a file it was not asked to change.
- Input that makes analysis exceed its bounds (a crash, a hang, or memory without limit) on code a project could hold.
  The bounds are described under "Analysis cost" in [docs/development.md](../docs/development.md).

### What is an ordinary bug

These are not vulnerabilities. Please report them in the public issue tracker, with the
[issue templates](https://github.com/koxudaxi/heki/issues/new/choose):

- a wrong report, or a report heki misses;
- slow analysis that stays within the documented bounds;
- the behavior of code that heki only reads.

Non-security bugs should be reported through the public issue tracker.
