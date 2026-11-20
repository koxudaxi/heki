//! `heki check --profile`: which project functions a profile dump shows hot, and the dumps the
//! readers refuse.

mod common;

use common::Project;
use serde_json::Value;

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

#[test]
fn gecko_references_to_missing_rows_are_a_malformed_dump() {
    let p = Project::new("prof_gecko_refs");
    p.write("app.py", &format!("class A:\n{}", member_body("run")))
        .write(
            "p.json",
            r#"{"threads": [{"samples": {"stack": [7]}, "stackTable": {"frame": [], "prefix": []}, "frameTable": {"func": []}, "funcTable": {"name": [], "fileName": []}}], "shared": {"stringArray": []}}"#,
        );
    let (code, _, stderr) = p.check(&["--profile", "p.json"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("gecko `samples.stack` entry 0 refers to no row"),
        "{stderr}"
    );
}

/// The `profile:` evidence of each diagnostic, by qualname.
fn profile_notes(p: &Project, dump: &str) -> Vec<(String, String)> {
    let (code, stdout, stderr) = p.check(&["--profile", dump, "--output-format", "json"]);
    assert_eq!(code, 1, "{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .filter_map(|d| {
            let evidence = d["evidence"].as_str()?;
            let note = evidence.split("; ").find(|e| e.starts_with("profile:"))?;
            Some((d["qualname"].as_str()?.to_owned(), note.to_owned()))
        })
        .collect()
}

#[test]
fn profile_frames_name_functions_on_dotted_and_path_boundaries() {
    let p = Project::new("prof_boundaries");
    let two_classes = format!(
        "class A:\n{}\n\nclass BA:\n{}",
        member_body("run"),
        member_body("run")
    );
    p.write("app.py", &two_classes)
        .write(
            "one/models.py",
            &format!("class M:\n{}", member_body("build")),
        )
        .write(
            "two/models.py",
            &format!("class M:\n{}", member_body("build")),
        )
        // `A.run` is not `BA.run`; a line inside `BA.run` picks it for a plain `run`.
        .write(
            "qualified.txt",
            "tid:1;app.py:A.run:4 95\ntid:1;other.py:idle:1 5\n",
        )
        .write("by_line.txt", "run (app.py:14) 60\nidle 40\n")
        // Without a line `run` could be either method, and `models.py` either package.
        .write(
            "ambiguous.txt",
            "run (app.py) 60\nmodels.py:M.build:4 30\nidle 10\n",
        );
    assert_eq!(
        profile_notes(&p, "qualified.txt"),
        [(
            "A.run".to_owned(),
            "profile: `A.run` is on the stack in 95.0% of samples".to_owned()
        )]
    );
    assert_eq!(
        profile_notes(&p, "by_line.txt"),
        [(
            "BA.run".to_owned(),
            "profile: `BA.run` is on the stack in 60.0% of samples".to_owned()
        )]
    );
    assert!(profile_notes(&p, "ambiguous.txt").is_empty());

    // Only the function the profile shows hot gets its size-dependent fix.
    let (_, _, stderr) = p.check(&["--fix", "--profile", "qualified.txt"]);
    assert!(
        stderr.contains("Fixed 1 issue(s) in 1 file(s)."),
        "{stderr}"
    );
    assert_eq!(
        p.read("app.py")
            .matches("allowed_set = set(allowed)")
            .count(),
        1
    );
    let (_, _, stderr) = p.check(&["--fix", "--profile", "ambiguous.txt"]);
    assert!(stderr.contains("Fixed 0 issue(s)"), "{stderr}");
}
