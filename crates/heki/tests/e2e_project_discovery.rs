//! Project and module discovery end to end: source roots, which file owns a module name, the
//! target version, and how imports bind names (what `heki check` links across modules).

mod common;

use common::Project;

/// `import time` + a blocking helper.
const BLOCKING_WAIT: &str = "import time\n\n\ndef wait():\n    time.sleep(1)\n";

/// `heki check --output-format concise` of the project: `(exit code, stdout)`.
fn concise(p: &Project) -> (i32, String) {
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_ne!(code, 2, "{stderr}");
    (code, stdout)
}

#[test]
fn src_directories_are_source_roots_unless_they_are_packages() {
    // A namespace package under `src/` (no `__init__.py`): `acme.io` links.
    let p = Project::new("disc_src_namespace");
    p.write("src/acme/io.py", BLOCKING_WAIT).write(
        "src/acme/api.py",
        "from acme.io import wait\n\n\nasync def go():\n    wait()\n",
    );
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    assert!(stdout.contains("src/acme/api.py:5:5: EFF101 "), "{stdout}");
    assert!(stdout.contains("`acme.io.wait`"), "{stdout}");

    // Modules directly under `src/` are top-level modules.
    let p = Project::new("disc_src_modules");
    p.write("src/helpers.py", BLOCKING_WAIT).write(
        "src/main.py",
        "from helpers import wait\n\n\nasync def go():\n    wait()\n",
    );
    assert!(concise(&p).1.contains("src/main.py:5:5: EFF101 "));

    // `src` is a package itself: its modules are `src.*`.
    let p = Project::new("disc_src_package");
    p.write("src/__init__.py", "")
        .write("src/helpers.py", BLOCKING_WAIT)
        .write(
            "src/main.py",
            "from src.helpers import wait\n\n\nasync def go():\n    wait()\n",
        );
    assert!(concise(&p).1.contains("`src.helpers.wait`"));
    p.write(
        "src/main.py",
        "from helpers import wait\n\n\nasync def go():\n    wait()\n",
    );
    assert_eq!(concise(&p), (0, String::new()));
}

#[test]
fn a_package_owns_its_name_over_a_module_of_the_same_name() {
    let p = Project::new("disc_package_over_module");
    p.write("pkg/__init__.py", BLOCKING_WAIT)
        .write("pkg.py", "def wait():\n    pass\n")
        .write(
            "app.py",
            "from pkg import wait\n\n\nasync def go():\n    wait()\n",
        );
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    assert!(stdout.contains("app.py:5:5: EFF101 "), "{stdout}");
    // The other way round: the package's no-op wins over the module's blocking `wait`.
    p.write("pkg/__init__.py", "def wait():\n    pass\n")
        .write("pkg.py", BLOCKING_WAIT);
    assert_eq!(concise(&p), (0, String::new()));
    // A stub package still loses to the module implementation.
    std::fs::remove_file(p.dir.join("pkg/__init__.py")).unwrap();
    p.write("pkg/__init__.pyi", "def wait() -> None: ...\n");
    assert!(concise(&p).1.contains("app.py:5:5: EFF101 "));
}

#[test]
fn a_typing_stub_importing_builtins_patches_nothing() {
    let p = Project::new("disc_stub_builtins");
    p.write(
        "a.py",
        "def build(items):\n    xs = list()\n    for item in items:\n        xs = xs + [item]\n    return xs\n",
    )
    .write("m.pyi", "import builtins\n\n\ndef f(x: builtins.str) -> None: ...\n");
    assert!(concise(&p).1.contains("a.py:4:9: OPT101 "));
    // The same import in a module that runs may rebind `list`.
    p.write("m.py", "import builtins\n");
    assert!(!concise(&p).1.contains("OPT101"));
}
