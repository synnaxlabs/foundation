//! Keeps each `sim` feature test-only: only a dev-dependency, the fuzz crate, or
//! another `sim` feature turns it on (rule 9 in `docs/decisions/crate-map.md`).

use serde_json::Value;

const RULE: &str = "A `sim` feature is test-only: only a dev-dependency, the fuzz \
                    crate, or another `sim` feature turns it on. A product feature \
                    turns on `dep:sim` and `simulate` features. See rule 9 in \
                    docs/decisions/crate-map.md.";

/// The problems where `package`, one package of `cargo metadata`, turns on a `sim`
/// feature from a feature other than `sim` or from a normal dependency.
pub(crate) fn check(package: &Value) -> Vec<String> {
    let name = package["name"].as_str().unwrap_or_default();
    let mut problems = Vec::new();
    let features = package["features"].as_object().into_iter().flatten();
    for (feature, values) in features.filter(|(feature, _)| *feature != "sim") {
        let values = values.as_array().into_iter().flatten();
        let values = values.filter_map(Value::as_str);
        for value in values.filter(|v| *v == "sim" || v.ends_with("/sim")) {
            problems.push(format!(
                "`{name}` turns on `{value}` in its feature `{feature}`. {RULE}"
            ));
        }
    }
    let deps = package["dependencies"].as_array().into_iter().flatten();
    for dep in deps.filter(|dep| dep["kind"].is_null()) {
        let features = dep["features"].as_array().into_iter().flatten();
        if features.filter_map(Value::as_str).any(|f| f == "sim") {
            let dep = dep["name"].as_str().unwrap_or_default();
            problems.push(format!(
                "`{name}` turns on the `sim` feature of its dependency `{dep}`. Move \
                 it to the dev-dependencies. {RULE}"
            ));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problems(name: &str) -> Vec<String> {
        let metadata = crate::metadata(&crate::fixture().join("features")).unwrap();
        let packages = metadata["packages"].as_array().unwrap();
        let package = packages.iter().find(|p| p["name"] == name).unwrap();
        check(package)
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
    fn rejects_a_normal_dependency_that_turns_on_sim() {
        assert_eq!(
            problems("normal"),
            [
                "`normal` turns on the `sim` feature of its dependency `own`. Move it \
                 to the dev-dependencies. A `sim` feature is test-only: only a \
                 dev-dependency, the fuzz crate, or another `sim` feature turns it \
                 on. A product feature turns on `dep:sim` and `simulate` features. \
                 See rule 9 in docs/decisions/crate-map.md."
            ]
        );
    }

    #[test]
    fn accepts_sim_from_sim_and_from_dev_and_build_dependencies() {
        assert_eq!(problems("pass"), Vec::<String>::new());
    }
}
