# Installation

heki is a single executable: its Python package ships only that executable and installs no
Python dependencies into your environment. It is published as prebuilt wheels on PyPI for the
platforms listed below, so `uvx heki`, `pip install heki`, and the pre-commit hook need no Rust
toolchain and compile nothing.

## With uv, pipx, or pip

=== "uv"

    ```console
    $ uv tool install heki
    $ uvx heki check            # or run it without installing
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

Wheels are published for these platforms:

| OS | Architectures |
| -- | ------------- |
| Linux | x86-64, AArch64, x86-64 musl |
| macOS | Apple silicon, Intel |
| Windows | x86-64 |

## Check the installation

```console
$ heki --version
$ heki rule --all
```

## Supported Python versions

- heki analyzes code written for Python 3.11 to 3.15.
- It does not need a Python interpreter to run.
- The version of the checked code comes from
  [`target-version`](configuration.md#top-level-settings), or from `requires-python`; see
  [Scope](scope.md#the-target-python-version) for what it changes.

## From source

??? note "Only for a platform without a wheel, or for development on heki"

    heki needs Rust 1.99 or newer:

    ```console
    $ git clone https://github.com/koxudaxi/heki
    $ cd heki
    $ cargo install --path crates/heki
    ```

    `pip install` of the source distribution builds the same executable with maturin.
