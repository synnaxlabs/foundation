//! Writes a reader's samples to InfluxDB.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

pub mod gap;
pub mod line;
#[cfg(feature = "sim")]
pub mod sim;

use std::future;

use connector::kind::{self, Channels, Context, Error};
use connector::{cancel, reader};
use document::diagnostic::{Code, Diagnostic};
use document::value::Value;
use document::{Document, read};
use http::Uri;

const BAD_ADDRESS: Code = Code::new("influx.bad-address");
const NOT_YET: Code = Code::new("influx.not-yet");

/// The `influx` kind: writes a reader's samples to InfluxDB.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Kind;

/// One influx connector's config.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// The InfluxDB endpoint: a URI that [`connector::http::uri`] takes, with a path
    /// of `/` or none, and no query.
    pub address: Uri,
    /// The reader whose samples it writes.
    pub reader: reader::Settings,
}

impl kind::Kind for Kind {
    type Config = Config;

    /// Reads `address`, the settings of [`reader::read`], and no other key.
    fn parse(&self, config: &Document) -> Result<Config, Vec<Diagnostic>> {
        let reader = reader::read(config, &["address"], &[]);
        let address = read::required(
            config,
            kind::NOUN,
            None,
            "address",
            address,
            "Add an `address` attribute with the InfluxDB endpoint, such as \
             \"http://influx:8086\""
                .into(),
        );
        match (reader, address) {
            (Ok(reader), Ok(address)) => Ok(Config { address, reader }),
            (reader, address) => {
                let mut diagnostics = reader.err().unwrap_or_default();
                diagnostics.extend(address.err());
                Err(diagnostics)
            }
        }
    }

    /// An out connector reads and writes no device channel.
    fn check(&self, _: &Config) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    /// An InfluxDB has no devices to find.
    fn discover(
        &self,
        _: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, Error>> {
        future::ready(Ok(Vec::new()))
    }

    /// Fails with `influx.not-yet`: this build cannot run the kind yet.
    fn run(&self, _: Context<Config>) -> impl Future<Output = Result<(), Error>> {
        future::ready(Err(Error::Config(vec![Diagnostic::new(
            NOT_YET,
            None,
            "this build cannot run the influx kind yet".into(),
            "Remove the connector, or run it on a build that has the influx writer"
                .into(),
        )])))
    }
}

/// Reads an address: a URI that [`connector::http::uri`] takes, with a path of `/`
/// or none, and no query.
fn address(value: &Value) -> Result<Uri, Diagnostic> {
    let uri = connector::http::uri(value)?;
    let part = match (uri.path(), uri.query()) {
        (_, Some(_)) => "a query",
        ("/", None) => return Ok(uri),
        (_, None) => "a path",
    };
    Err(Diagnostic::new(
        BAD_ADDRESS,
        value.span,
        format!("the address has {part}, which the influx kind does not take"),
        "Remove it, as in \"http://influx:8086\"".into(),
    ))
}

#[cfg(test)]
mod tests;
