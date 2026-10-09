//! Keeps each `sim` feature test-only: only a dev-dependency, the fuzz crate, or
//! another `sim` feature turns it on (rule 9 in `docs/decisions/crate-map.md`).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::field;

const RULE: &str = "A `sim` feature is test-only: only a dev-dependency, the fuzz \
                    crate, or another `sim` feature turns it on. A product feature \
                    turns on `dep:sim` and `simulate` features. See rule 9 in \
                    docs/decisions/crate-map.md.";

/// The problems where a local crate of `graph`, the resolved `cargo metadata` of a
/// workspace, turns on a `sim` feature from a feature other than `sim` or from a
/// dependency that is not a dev-dependency. A local crate is a member, also a tool or
/// a benchmark, or a path crate outside the workspace: a build unifies the features
/// of each. A field that is missing or of another shape is a problem too.
pub(crate) fn check(graph: &Value) -> Vec<String> {
    let packages = match field::list(graph, "packages") {
        Ok(packages) => packages,
        Err(e) => return vec![e],
    };
    let local = packages
        .iter()
        .filter(|package| package["source"].is_null());
    local
        .flat_map(|package| problems(package).unwrap_or_else(|e| vec![e]))
        .collect()
}

/// The problems of one package of `cargo metadata`, as [`check`] states.
fn problems(package: &Value) -> Result<Vec<String>, String> {
    let name = field::text(package, "name")?;
    let features: BTreeMap<String, Vec<String>> =
        serde_json::from_value(package["features"].clone())
            .map_err(|e| format!("`{name}`: JSON field `features`: {e}"))?;
    let mut problems = Vec::new();
    for (feature, values) in features.iter().filter(|(feature, _)| *feature != "sim") {
        for value in values.iter().filter(|v| *v == "sim" || v.ends_with("/sim")) {
            problems.push(format!(
                "`{name}` turns on `{value}` in its feature `{feature}`. {RULE}"
            ));
        }
    }
    for dep in field::list(package, "dependencies")? {
        if dep["kind"] == "dev" {
            continue;
        }
        if field::list(dep, "features")?.iter().any(|f| f == "sim") {
            let dep = field::text(dep, "name")?;
            problems.push(format!(
                "`{name}` turns on the `sim` feature of its dependency `{dep}`. Move \
                 it to the dev-dependencies. {RULE}"
            ));
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn of(name: &str) -> Vec<String> {
        let graph = crate::graph(&crate::fixture().join("features")).unwrap();
        let prefix = format!("`{name}` ");
        check(&graph)
            .into_iter()
            .filter(|p| p.starts_with(&prefix))
            .collect()
    }

    #[test]
    fn rejects_a_product_feature_that_turns_on_sim() {
        for (name, feature, value) in [
            ("own", "default", "sim"),
            ("weak", "simulate", "own?/sim"),
            ("strong", "simulate", "own/sim"),
        ] {
            assert_eq!(
                of(name),
                [format!(
                    "`{name}` turns on `{value}` in its feature `{feature}`. A `sim` \
                     feature is test-only: only a dev-dependency, the fuzz crate, or \
                     another `sim` feature turns it on. A product feature turns on \
                     `dep:sim` and `simulate` features. See rule 9 in \
                     docs/decisions/crate-map.md."
                )],
                "`{name}`"
            );
        }
    }

    #[test]
    fn rejects_a_normal_build_or_platform_dependency_that_turns_on_sim() {
        for name in ["normal", "build", "platform"] {
            assert_eq!(
                of(name),
                [format!(
                    "`{name}` turns on the `sim` feature of its dependency `own`. Move \
                     it to the dev-dependencies. A `sim` feature is test-only: only a \
                     dev-dependency, the fuzz crate, or another `sim` feature turns \
                     it on. A product feature turns on `dep:sim` and `simulate` \
                     features. See rule 9 in docs/decisions/crate-map.md."
                )],
                "`{name}`"
            );
        }
    }

    #[test]
    fn accepts_sim_from_sim_and_from_dev_dependencies() {
        assert_eq!(of("pass"), Vec::<String>::new());
    }

    #[test]
    fn names_a_field_that_is_missing_or_of_another_shape() {
        for (package, error) in [
            (json!({}), "JSON has no string field `name`"),
            (
                json!({ "name": "a", "features": { "default": "sim" } }),
                "`a`: JSON field `features`: invalid type: string \"sim\", expected a \
                 sequence",
            ),
            (
                json!({ "name": "a", "features": {} }),
                "JSON has no array field `dependencies`",
            ),
            (
                json!({ "name": "a", "features": {}, "dependencies": [{}] }),
                "JSON has no array field `features`",
            ),
            (
                json!({
                    "name": "a",
                    "features": {},
                    "dependencies": [{ "features": ["sim"] }],
                }),
                "JSON has no string field `name`",
            ),
        ] {
            assert_eq!(problems(&package), Err(error.to_string()), "{package}");
        }
    }

    #[test]
    fn checks_only_local_crates_and_names_a_malformed_graph() {
        assert_eq!(check(&json!({})), ["JSON has no array field `packages`"]);
        let registry = json!({
            "name": "a",
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "features": { "default": ["sim"] },
            "dependencies": [],
        });
        assert_eq!(
            check(&json!({ "packages": [registry, {}] })),
            ["JSON has no string field `name`"]
        );
    }
}
