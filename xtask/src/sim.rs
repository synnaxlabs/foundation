//! Keeps each `sim` feature test-only: only a dev-dependency, the fuzz crate, or
//! another `sim` feature turns it on (rule 9 in `docs/decisions/crate-map.md`).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::field;

const RULE: &str = "A `sim` feature is test-only: only a dev-dependency, the fuzz \
                    crate, or another `sim` feature turns it on. A product feature \
                    turns on `dep:sim` and `simulate` features. See rule 9 in \
                    docs/decisions/crate-map.md.";

/// The problems where `package`, one package of `cargo metadata`, turns on a `sim`
/// feature from a feature other than `sim` or from a dependency that is not a
/// dev-dependency.
///
/// # Errors
///
/// A field of `package` that is missing or of another shape.
pub(crate) fn check(package: &Value) -> Result<Vec<String>, String> {
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

    fn problems(name: &str) -> Vec<String> {
        let metadata = crate::metadata(&crate::fixture().join("features")).unwrap();
        let packages = metadata["packages"].as_array().unwrap();
        let package = packages.iter().find(|p| p["name"] == name).unwrap();
        check(package).unwrap()
    }

    #[test]
    fn rejects_a_product_feature_that_turns_on_sim() {
        for (name, feature, value) in [
            ("own", "default", "sim"),
            ("weak", "simulate", "own?/sim"),
            ("strong", "simulate", "own/sim"),
        ] {
            assert_eq!(
                problems(name),
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
    fn rejects_a_normal_or_build_dependency_that_turns_on_sim() {
        for name in ["normal", "build"] {
            assert_eq!(
                problems(name),
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
        assert_eq!(problems("pass"), Vec::<String>::new());
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
            assert_eq!(check(&package), Err(error.to_string()), "{package}");
        }
    }
}
