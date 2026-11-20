//! `heki check --profile`: which project functions a profile dump shows hot, and the dumps the
//! readers refuse.

mod common;

use common::Project;

/// A function with a size-dependent `OPT110` rewrite (`allowed` becomes a set).
fn member_body(name: &str) -> String {
    format!(
        "    def {name}(self, n):\n        allowed = list(range(0, 50, 3))\n        hits = []\n        for p in range(n):\n            if p in allowed:\n                hits.append(p)\n        return hits\n"
    )
}

#[test]
fn profile_weights_that_overflow_are_a_malformed_dump() {
    let p = Project::new("prof_overflow");
    p.write("app.py", &format!("class A:\n{}", member_body("run")))
        .write(
            "p.txt",
            "app.py:A.run:3 9223372036854775808\nidle 9223372036854775808\n",
        );
    let (code, _, stderr) = p.check(&["--profile", "p.txt"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("unsupported or malformed profile dump: sample weights overflow"),
        "{stderr}"
    );
}
