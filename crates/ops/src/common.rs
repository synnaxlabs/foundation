//! Fixtures that the tests of `plan` and `apply` share.

use std::collections::BTreeMap;
use std::path::PathBuf;

use connector::cancel;
use connector::kind::{self, Channels, Context};
use document::diagnostic::Diagnostic;
use document::{Document, Source};
use types::name::Name;

use crate::front_end::{File, FrontEnd};

pub(crate) const PLANT: &str =
    include_str!("../../acceptance/tests/it/fixtures/plant.hcl");
pub(crate) const SITE: &str =
    include_str!("../../acceptance/tests/it/fixtures/site.hcl");

/// A kind whose channels are the labels of its `read` blocks, which it writes. It takes
/// each attribute, so it stands in for each kind of the fixtures.
pub(crate) struct Reader;

impl kind::Kind for Reader {
    type Config = Vec<Name>;

    fn parse(&self, config: &Document) -> Result<Vec<Name>, Vec<Diagnostic>> {
        let reads = config
            .blocks
            .iter()
            .filter(|block| &*block.keyword == "read");
        Ok(reads
            .map(|block| block.labels[0].text.parse().expect("a name"))
            .collect())
    }

    fn check(&self, writes: &Vec<Name>) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            reads: Vec::new(),
            writes: writes.clone(),
        })
    }

    fn discover(
        &self,
        _: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, kind::Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn run(
        &self,
        _: Context<Vec<Name>>,
    ) -> impl Future<Output = Result<(), kind::Error>> {
        std::future::ready(Ok(()))
    }
}

pub(crate) fn hcl(source: Source, text: &str) -> Result<Document, Vec<Diagnostic>> {
    config_hcl::read(source, text)
        .map_err(|errors| errors.iter().map(Diagnostic::from).collect())
}

pub(crate) fn front_ends() -> BTreeMap<&'static str, FrontEnd> {
    BTreeMap::from([("hcl", FrontEnd { read: hcl })])
}

pub(crate) fn name(text: &str) -> Name {
    text.parse().expect("a name")
}

pub(crate) fn files(files: &[(&str, &str)]) -> Vec<File> {
    files
        .iter()
        .map(|(path, text)| File {
            path: PathBuf::from(path),
            text: (*text).to_owned(),
        })
        .collect()
}

/// `site.hcl` with a placement that homes its index on `edge`.
pub(crate) fn placed_site() -> String {
    format!("{SITE}placement \"p\" {{\n  select = \"site.*\"\n  home = \"edge\"\n}}\n")
}
