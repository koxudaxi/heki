# Installation

heki is a single executable. The Python package only ships that executable (no Python
dependencies are installed into your environment).

## With uv, pipx, or pip

=== "uv"

    ```console
    $ uv tool install heki
    ```

    Or run it without installing:

    ```console
    $ uvx heki check
    ```

=== "pipx"

    ```console
    $ pipx install heki
    ```

=== "pip"

    ```console
    $ pip install heki
    ```

As a development dependency of a project:

```console
$ uv add --dev heki
```

Wheels are published for Linux (x86-64, AArch64, musl), macOS (Apple silicon and Intel), and
Windows (x86-64).

## From source

heki needs Rust 1.99 or newer.

```console
$ git clone https://github.com/koxudaxi/heki
$ cd heki
$ cargo install --path crates/heki
```

## Check the installation

```console
$ heki --version
$ heki rule --all
```

## Supported Python versions

heki analyzes code written for Python 3.11 to 3.15. It reads the target version from
`target-version` in the configuration, or from `requires-python` in `pyproject.toml`; the
version decides language details such as comprehension inlining (PEP 709), deferred
annotations (PEP 649), and `lazy import` (PEP 810). heki itself does not need a Python
interpreter to run.
