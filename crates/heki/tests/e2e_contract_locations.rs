//! Where a contract report points in the configuration (`contract_locations`): the contract
//! table and the entry the report names, written in the main file, in an included file, or in a
//! group, or the places `source` and `entry-sources` name instead; the same places as SARIF
//! related locations; and a baseline that the places leave as it was (end to end, through the
//! real binary).

mod common;

use common::Project;
use serde_json::{Value, json};

const MODEL: &str = "\
class Model:
    def __init__(self) -> None:
        self.fields: list[int] = []


def outside(m: Model) -> None:
    m.fields = [1]
";

/// A project of `pkg/m.py` with `config` after the `[project]` table of its `pyproject.toml`
/// (3 lines).
fn project(name: &str, config: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/m.py", MODEL);
    p
}

/// `contract_locations` of the one `MUT201` report, as `(path, line, column, message)`.
fn locations(p: &Project) -> Vec<(String, u64, u64, String)> {
    let (code, stdout, stderr) = p.check(&["--select", "MUT201", "--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let [d] = diags.as_slice() else {
        panic!("{stdout}")
    };
    (d["contract_locations"].as_array().unwrap().iter())
        .map(|c| {
            (
                c["path"].as_str().unwrap().to_owned(),
                c["line"].as_u64().unwrap(),
                c["column"].as_u64().unwrap(),
                c["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn at(path: &str, line: u64, column: u64, message: &str) -> (String, u64, u64, String) {
    (path.to_owned(), line, column, message.to_owned())
}

const OWNER: &str = "\
[[tool.heki.write-owners]]
name = \"fields\"
targets = [\"pkg.m.Model.fields\"]
allowed-writers = [\"pkg.store.*\"]
";

#[test]
fn a_report_points_at_its_contract_and_the_entry_it_names() {
    let p = project("locations_main", OWNER);
    assert_eq!(
        locations(&p),
        [
            at("pyproject.toml", 4, 1, "write-owners contract `fields`"),
            at(
                "pyproject.toml",
                6,
                12,
                "`targets` entry `pkg.m.Model.fields`"
            ),
        ]
    );
    // A contract without a name is named by its position.
    let p = project(
        "locations_unnamed",
        &OWNER.replace("name = \"fields\"\n", ""),
    );
    assert_eq!(
        locations(&p)[0],
        at(
            "pyproject.toml",
            4,
            1,
            "the `write-owners` contract at position 1"
        )
    );
}

#[test]
fn a_report_points_into_the_included_file_and_the_group() {
    let main = "[tool.heki]\ninclude = [\"contracts/fields.toml\"]\n";
    let p = project("locations_included", main);
    p.write(
        "contracts/fields.toml",
        "[groups]\nfields = [\"pkg.m.Model.fields\"]\n\n[[write-owners]]\nname = \"fields\"\ntargets = [\"@fields\"]\nallowed-writers = [\"pkg.store.*\"]\n",
    );
    assert_eq!(
        locations(&p),
        [
            at(
                "contracts/fields.toml",
                4,
                1,
                "write-owners contract `fields`"
            ),
            at(
                "contracts/fields.toml",
                2,
                11,
                "`targets` entry `pkg.m.Model.fields` from group `fields`"
            ),
        ]
    );
}

#[test]
fn source_and_entry_sources_name_the_places_shown_instead() {
    let named = format!("{OWNER}source = \"gen/tables.py:12\"\n");
    assert_eq!(
        locations(&project("locations_source", &named)),
        [
            at("gen/tables.py", 12, 1, "write-owners contract `fields`"),
            at(
                "pyproject.toml",
                6,
                12,
                "`targets` entry `pkg.m.Model.fields`"
            ),
        ]
    );
    let entries =
        format!("{OWNER}entry-sources = {{ \"pkg.m.Model.fields\" = \"gen/tables.py:20\" }}\n");
    assert_eq!(
        locations(&project("locations_entry_sources", &entries)),
        [
            at("pyproject.toml", 4, 1, "write-owners contract `fields`"),
            at(
                "gen/tables.py",
                20,
                1,
                "`targets` entry `pkg.m.Model.fields`"
            ),
        ]
    );
}

#[test]
fn sarif_gives_the_places_as_related_locations() {
    let p = project("locations_sarif", OWNER);
    let (code, stdout, stderr) = p.check(&["--select", "MUT201", "--output-format", "sarif"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let sarif: Value = serde_json::from_str(&stdout).unwrap();
    let related = &sarif["runs"][0]["results"][0]["relatedLocations"];
    let place = |id: u64, line: u64, column: u64, text: &str| {
        json!({
            "id": id,
            "physicalLocation": {
                "artifactLocation": { "uri": "pyproject.toml" },
                "region": { "startLine": line, "startColumn": column }
            },
            "message": { "text": text }
        })
    };
    assert_eq!(
        related,
        &json!([
            place(0, 4, 1, "write-owners contract `fields`"),
            place(1, 6, 12, "`targets` entry `pkg.m.Model.fields`"),
        ])
    );
}

#[test]
fn the_places_leave_the_baseline_as_it_was() {
    let update = ["--baseline", "heki-baseline.toml", "--update-baseline"];
    let plain = project("locations_baseline_plain", OWNER);
    plain.check(&update);
    let named = format!(
        "{OWNER}source = \"gen/tables.py:12\"\nentry-sources = {{ \"pkg.m.Model.fields\" = \"gen/tables.py:20\" }}\n"
    );
    let with_sources = project("locations_baseline_named", &named);
    with_sources.check(&update);
    assert_eq!(
        with_sources.read("heki-baseline.toml"),
        plain.read("heki-baseline.toml")
    );
    let (code, stdout, stderr) = with_sources.check(&["--baseline", "heki-baseline.toml"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}

#[test]
fn a_place_that_is_no_path_and_line_is_an_error() {
    for (key, value, message) in [
        (
            "source",
            "\"gen/tables.py\"",
            "pyproject.toml:8:10: `source` value `gen/tables.py` is not `path:line`",
        ),
        (
            "source",
            "\"/gen/tables.py:3\"",
            "pyproject.toml:8:10: `source` value `/gen/tables.py:3` is not `path:line`",
        ),
        (
            "entry-sources",
            "{ \"pkg.m.Model.fields\" = \"gen/tables.py:0\" }",
            "pyproject.toml:8:19: `entry-sources` value `gen/tables.py:0` is not `path:line`",
        ),
        (
            "entry-sources",
            "{ \"pkg.m.Model.gone\" = \"gen/tables.py:3\" }",
            "pyproject.toml:8:19: `entry-sources` names `pkg.m.Model.gone`, which no list of the contract holds",
        ),
    ] {
        let p = project("locations_errors", &format!("{OWNER}{key} = {value}\n"));
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{stdout}{stderr}");
        assert!(stderr.contains(message), "{message}\n{stderr}");
    }
    // A defaults table holds neither key: each names one table's places.
    let defaults =
        format!("[tool.heki.write-owners-defaults]\nsource = \"gen/tables.py:1\"\n\n{OWNER}");
    let (code, stdout, stderr) = project("locations_defaults", &defaults).check(&[]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(stderr.contains("source"), "{stderr}");
}

/// An entry that matches nothing is warned about once, with each place it is written and the
/// place of its contract, also when the contract holds it twice.
#[test]
fn a_warning_names_every_place_of_the_entry() {
    let config = "\
[tool.heki.groups]
more = [\"pkg.m.Model.gone\"]

[[tool.heki.write-owners]]
name = \"fields\"
targets = [\"pkg.m.Model.gone\", \"@more\"]
allowed-writers = [\"pkg.m.outside\"]
";
    let (_, stdout, stderr) = project("locations_unmatched", config).check(&[]);
    let warned: Vec<&str> = (stderr.lines())
        .filter_map(|l| l.strip_prefix("warning: "))
        .collect();
    let [only] = warned.as_slice() else {
        panic!("{stdout}{stderr}")
    };
    assert!(
        only.starts_with("`targets` entry `pkg.m.Model.gone` of write-owners `fields` (from pyproject.toml:9, group `more` at pyproject.toml:5; contract at pyproject.toml:7) names no "),
        "{stdout}{stderr}"
    );
}

/// A write-owners contract of `entries` targets, one per line, in the main file (`included`
/// false) or in an included file.
fn many_entries(name: &str, entries: usize, included: bool) -> Project {
    let list: String = (0..entries)
        .map(|k| format!("    \"pkg.m.Model.field_{k:06}\",\n"))
        .collect();
    let table = if included {
        "write-owners"
    } else {
        "tool.heki.write-owners"
    };
    let owner = format!(
        "[[{table}]]\nname = \"fields\"\ntargets = [\n{list}]\nallowed-writers = [\"pkg.m.outside\"]\n"
    );
    let p = Project::new(name);
    p.write("pkg/__init__.py", "").write("pkg/m.py", MODEL);
    if included {
        p.write("heki-owners.toml", &owner).write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki]\ninclude = [\"heki-owners.toml\"]\n",
        );
    } else {
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\n\n{owner}"),
        );
    }
    p
}

/// The seconds `heki check` takes on the project.
fn seconds(p: &Project) -> f64 {
    let started = std::time::Instant::now();
    let (_, stdout, stderr) = p.check(&["--select", "MUT201"]);
    assert!(!stderr.contains("error"), "{stdout}{stderr}");
    started.elapsed().as_secs_f64()
}

#[test]
fn loading_the_places_of_many_entries_takes_linear_time() {
    for included in [false, true] {
        let small = seconds(&many_entries("locations_scale_small", 5_000, included));
        let large = seconds(&many_entries("locations_scale_large", 40_000, included));
        // Eight times the entries: linear growth is about 8, quadratic about 64. The floor keeps
        // a fast small run from making the ratio noisy.
        assert!(
            large < 24.0 * small.max(0.25),
            "included {included}: {small:.2}s for 5000 entries, {large:.2}s for 40000"
        );
    }
}

#[test]
fn contracts_of_one_name_each_get_their_place_in_the_warning() {
    let config = "\
[[tool.heki.write-owners]]
name = \"fields\"
targets = [\"pkg.m.Model.gone\"]
allowed-writers = [\"pkg.m.outside\"]

[[tool.heki.write-owners]]
name = \"fields\"
targets = [\"pkg.m.Model.gone\"]
allowed-writers = [\"pkg.m.outside\"]
";
    let (_, stdout, stderr) = project("locations_same_name", config).check(&[]);
    assert!(
        stderr.contains("contracts at pyproject.toml:4, pyproject.toml:9)"),
        "{stdout}{stderr}"
    );
}
