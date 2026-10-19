//! Writes a reader's samples to InfluxDB.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

#[cfg_attr(
    not(feature = "sim"),
    expect(unreachable_pub, reason = "only the sim feature exports it")
)]
mod batch;
#[cfg(feature = "sim")]
#[doc(hidden)]
pub mod bench;
pub mod gap;
pub mod line;
#[cfg_attr(
    not(feature = "sim"),
    expect(unreachable_pub, reason = "only the sim feature exports it")
)]
mod send;
#[cfg(feature = "sim")]
pub mod sim;

use connector::kind::{self, Channels, Context, Error};
use connector::{reader, secret};
use document::diagnostic::{Code, Diagnostic};
use document::schema::{Attribute, Body, Presence};
use document::value::{self, Value};
use document::{Document, read};
use http::Uri;
use http::uri::Scheme;

const BAD_ADDRESS: Code = Code::new("influx.bad-address");
const BAD_BUCKET: Code = Code::new("influx.bad-bucket");
const BAD_DATABASE: Code = Code::new("influx.bad-database");
const BAD_MEASUREMENT: Code = Code::new("influx.bad-measurement");
const BAD_ORG: Code = Code::new("influx.bad-org");
const CA_WITH_HTTP: Code = Code::new("influx.ca-with-http");
const ORG_WITH_DATABASE: Code = Code::new("influx.org-with-database");
const TWO_TARGETS: Code = Code::new("influx.two-targets");
/// The body of the config.
const BODY: Body = Body {
    summary: "Writes the samples of the channels that it reads to InfluxDB 1, 2, or 3. \
              It takes a `database`, or a `bucket` and an `org`, never both.",
    attributes: &[
        reader::SELECT,
        Attribute {
            name: "address",
            value: "an `http` or `https` URI with no path",
            presence: Presence::Required,
            summary: "The InfluxDB endpoint.",
        },
        Attribute {
            name: "database",
            value: "a string",
            presence: Presence::Unless(&["bucket", "org"]),
            summary: "The InfluxDB 1 database.",
        },
        Attribute {
            name: "bucket",
            value: "a string",
            presence: Presence::Unless(&["database"]),
            summary: "The InfluxDB 2 bucket, or the InfluxDB 3 database.",
        },
        Attribute {
            name: "org",
            value: "a string",
            presence: Presence::Unless(&["database"]),
            summary: "The organization of the `bucket`. InfluxDB 3 takes any name.",
        },
        Attribute {
            name: "measurement",
            value: "a string",
            presence: Presence::Required,
            summary: "The measurement of each line.",
        },
        Attribute {
            name: "token",
            value: "a secret",
            presence: Presence::Default("no `Authorization` header"),
            summary: "The token that each request sends as `Authorization: Token \
                      <value>`.",
        },
        Attribute {
            name: "ca",
            value: "a secret",
            presence: Presence::Default("only the roots of the node"),
            summary: "PEM certificates that an `https` connect trusts in addition to \
                      the roots of the node. Only an `https` address takes it.",
        },
    ],
    blocks: &[reader::BLOCK],
    examples: &[DATABASE, BUCKET],
};

/// A connector to InfluxDB 1.
const DATABASE: &str = "\
address     = \"http://influx:8086\"
database    = \"edge\"
measurement = \"telemetry\"
select      = \"edge.*\"
";

/// A connector to InfluxDB 2 or 3, with a token, a CA, and a reader block.
const BUCKET: &str = "\
address     = \"https://influx:8086\"
bucket      = \"edge\"
org         = \"plant\"
measurement = \"telemetry\"
token       = env(\"INFLUX_TOKEN\")
ca          = file(\"/etc/foundation/influx.pem\")
select      = \"edge.**\"

reader {
  mode = \"complete\"
  hold = \"2h\"
}
";

/// The count of the samples of the frames that the run acks, which InfluxDB confirmed
/// with a 204, across the runs of one supervisor call. A run that ends before it acks
/// a frame counts none of its samples, also those that landed.
const CONFIRMED: &str = "confirmed";
/// The count of the seqs that the gap lines of the frames that the run acks count,
/// which InfluxDB took with a 204, across the runs of one supervisor call.
const LOST: &str = "lost";
/// The count of the `string` and `bytes` samples left out as a field of over 65,536
/// bytes as written, in the frames that the run acks, across the runs of one
/// supervisor call.
const OVERSIZED: &str = "oversized";

/// The `influx` kind: writes a reader's samples to InfluxDB.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Kind;

/// One influx connector's config. Only
/// [`Kind::parse`](connector::kind::Kind::parse) of [`Kind`] makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The InfluxDB endpoint: a URI that [`connector::http::uri`] takes, with the
    /// scheme `http` or `https`, a path of `/` or none, and no query.
    pub(crate) address: Uri,
    /// Where it writes.
    pub(crate) target: Target,
    /// The measurement of each line, other than [`gap::MEASUREMENT`].
    pub(crate) measurement: line::Measurement,
    /// Where the InfluxDB token comes from, sent as `Authorization: Token <value>`
    /// on each request. `None` sends no `Authorization` header. An `http` address
    /// sends it in clear, and an `https` one keeps it from the network.
    pub(crate) token: Option<secret::Source>,
    /// Where the PEM certificates come from that an `https` connect trusts in
    /// addition to the roots of the node, such as a private CA, or the certificate of
    /// a self-signed server with `CA:FALSE`. `None` trusts only the roots of the node.
    /// An `http` address with a `ca` is refused.
    pub(crate) ca: Option<secret::Source>,
    /// The reader whose samples it writes.
    pub(crate) reader: reader::Settings,
}

/// Where a connector writes. Each name has 1 to 4,096 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Target {
    /// The database of InfluxDB 1, written with `POST /write?db=<database>`.
    Database(String),
    /// The bucket of InfluxDB 2, or the database of InfluxDB 3, in `org`, written
    /// with `POST /api/v2/write?org=<org>&bucket=<name>`. InfluxDB 3 takes any `org`.
    Bucket {
        /// The bucket, or the InfluxDB 3 database.
        name: String,
        /// The organization.
        org: String,
    },
}

impl Target {
    /// Whether the run writes a `u` integer: to a `bucket`, which stands for InfluxDB 2
    /// or 3, and not to a `database`, which stands for InfluxDB 1 (LINE TEXT).
    pub(crate) const fn unsigned(&self) -> bool {
        match self {
            Self::Database(_) => false,
            Self::Bucket { .. } => true,
        }
    }
}

impl kind::Kind for Kind {
    type Config = Config;
    const SCHEMA: Body = BODY;

    /// Reads `address`, `database` or `bucket` and `org`, `measurement`, an optional
    /// `token` and an optional `ca` that [`secret::Source::read`] takes, the settings
    /// of [`reader::read`], and no other key. It refuses a `ca` with an `http`
    /// address.
    fn parse(&self, config: &Document) -> Result<Config, Vec<Diagnostic>> {
        let reader = reader::read(config, &BODY);
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
        let target = target(config);
        let measurement = read::required(
            config,
            kind::NOUN,
            None,
            "measurement",
            measurement,
            "Add a `measurement` attribute with the name of each line, such as \
             \"telemetry\""
                .into(),
        );
        let token = source(config, "token");
        let ca = ca(config, &address);
        match (reader, address, target, measurement, token, ca) {
            (
                Ok(reader),
                Ok(address),
                Ok(target),
                Ok(measurement),
                Ok(token),
                Ok(ca),
            ) => Ok(Config {
                address,
                target,
                measurement,
                token,
                ca,
                reader,
            }),
            (reader, address, target, measurement, token, ca) => {
                let mut diagnostics = reader.err().unwrap_or_default();
                diagnostics.extend(address.err());
                diagnostics.extend(target.err().into_iter().flatten());
                diagnostics.extend(measurement.err());
                diagnostics.extend(token.err());
                diagnostics.extend(ca.err());
                Err(diagnostics)
            }
        }
    }

    /// Reads the channels of its reader settings, writes none, and names three
    /// counts: `confirmed`, the samples that InfluxDB took with a 204; `lost`, the
    /// seqs that the gap lines of this connector count, which InfluxDB took with a
    /// 204; and `oversized`, the `string` and `bytes` samples left out as a field of
    /// over 65,536 bytes as written, when their frame is acked.
    fn check(&self, config: &Config) -> Result<Channels, Vec<Diagnostic>> {
        let count = |text: &str| text.parse().expect("invariant: a count is a name");
        Ok(Channels {
            reader: Some(config.reader.clone()),
            counts: vec![count(CONFIRMED), count(LOST), count(OVERSIZED)],
            ..Channels::default()
        })
    }

    /// Writes the samples of its reader to InfluxDB, one line for each sample time with
    /// a field for each channel, keyed by the channel's name. Before the lines of a
    /// frame after seqs that the reader did not get, it writes their `live` gap line
    /// ([`gap::MEASUREMENT`]) at the frame's first sample, also when it leaves that
    /// sample's line out. It keeps one request in flight, of at most 5,000 lines, and
    /// of at most 4 MiB of them or one line, so the lines of a frame can span requests.
    /// It acks a frame only after InfluxDB answers 204 to the request of its last line,
    /// or with no request when no line waits for one. At the ack it adds the frame's
    /// fields to the count `confirmed`, the seqs of its gap lines to `lost`, and the
    /// samples it left out as over 65,536 bytes to `oversized`. A part of a frame that
    /// lands before the run ends, with the frame not acked, adds to no count, and a
    /// restart within the `hold` of the reader sends the whole frame again. It shows
    /// each other answer as its error, with the fix for a 400, a 404, a 413, or a 422,
    /// and sends the request again after a backoff. It reads the token at its start,
    /// and again after a 401 or a 403 before the next attempt. It reads the `ca` at its
    /// start, and again after a TLS failure before the next attempt. A read after the
    /// start that fails shows its error, keeps the request and the old token or `ca`,
    /// and reads again after the next backoff, with no request until the read gives a
    /// value.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with `influx.sample-type` for a channel of an `array`, `list`,
    /// or `matrix` type, with `influx.unsigned` for a `u64` channel to a
    /// [`Target::Database`], and with `influx.bad-channel` for a channel name that is
    /// not a valid field key. The error of [`hub::reader::Ended`] when the reader
    /// ends. From the reads at the start only: [`Error::Retry`] or [`Error::Config`]
    /// from [`Context::secret`] when a read fails, and [`Error::Retry`] when the token
    /// holds a byte that an HTTP header cannot hold, and when the `ca` holds no PEM
    /// certificate or one that does not parse.
    fn run(&self, ctx: Context<Config>) -> impl Future<Output = Result<(), Error>> {
        send::run(ctx)
    }
}

/// Reads the source of the secret at `key`, when there is one.
fn source(config: &Document, key: &str) -> Result<Option<secret::Source>, Diagnostic> {
    let attribute = config.attributes.get(key);
    attribute
        .map(|attribute| secret::Source::read(&attribute.value))
        .transpose()
}

/// Reads the `ca`, which only an `https` address takes.
fn ca(
    config: &Document,
    address: &Result<Uri, Diagnostic>,
) -> Result<Option<secret::Source>, Diagnostic> {
    let Some(ca) = config.attributes.get("ca") else {
        return Ok(None);
    };
    let source = secret::Source::read(&ca.value)?;
    match address {
        Ok(address) if address.scheme() != Some(&Scheme::HTTPS) => {
            Err(Diagnostic::new(
                CA_WITH_HTTP,
                ca.key_span,
                "the connector has a `ca`, which only an `https` address takes".into(),
                "Use an `https` address, or remove `ca`".into(),
            ))
        }
        _ => Ok(Some(source)),
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

/// Reads `database`, or `bucket` and `org`.
fn target(config: &Document) -> Result<Target, Vec<Diagnostic>> {
    let get = |key| config.attributes.get(key);
    match (get("database"), get("bucket"), get("org")) {
        (Some(_), Some(bucket), _) => Err(vec![Diagnostic::new(
            TWO_TARGETS,
            bucket.key_span,
            "the connector has a `database` and a `bucket`".into(),
            "Keep `database` for InfluxDB 1, or `bucket` and `org` for InfluxDB 2 and 3"
                .into(),
        )]),
        (Some(_), None, Some(org)) => Err(vec![Diagnostic::new(
            ORG_WITH_DATABASE,
            org.key_span,
            "the connector has an `org`, which only a `bucket` takes".into(),
            "Remove it, or write a `bucket` in place of the `database`".into(),
        )]),
        (Some(database), None, None) => {
            let database = name(&database.value, BAD_DATABASE, "database");
            database.map(Target::Database).map_err(|error| vec![error])
        }
        (None, None, _) => Err(vec![read::missing(
            kind::NOUN,
            None,
            &["database", "bucket"],
            "Add a `bucket` attribute and an `org` attribute, such as \"plant\", or a \
             `database` attribute for InfluxDB 1"
                .into(),
        )]),
        (None, Some(bucket), _) => {
            let bucket = name(&bucket.value, BAD_BUCKET, "bucket");
            let org = read::required(
                config,
                kind::NOUN,
                None,
                "org",
                |value| name(value, BAD_ORG, "org"),
                "Add an `org` attribute with the InfluxDB organization, such as \
                 \"plant\". InfluxDB 3 takes any name"
                    .into(),
            );
            match (bucket, org) {
                (Ok(name), Ok(org)) => Ok(Target::Bucket { name, org }),
                (bucket, org) => {
                    Err(bucket.err().into_iter().chain(org.err()).collect())
                }
            }
        }
    }
}

/// Reads the name of an InfluxDB `noun`, such as a database: a string of 1 to
/// [`send::NAME_MAX`] bytes.
fn name(value: &Value, code: Code, noun: &str) -> Result<String, Diagnostic> {
    let bad = |message: String| {
        Diagnostic::new(
            code,
            value.span,
            message,
            format!("Write the name of an InfluxDB {noun}, such as \"plant\""),
        )
    };
    match &value.kind {
        value::Kind::String(text) if text.is_empty() => {
            Err(bad(format!("the {noun} name is empty")))
        }
        value::Kind::String(text) if text.len() > send::NAME_MAX => Err(bad(format!(
            "the {noun} name has {} bytes, over {}",
            text.len(),
            send::NAME_MAX
        ))),
        value::Kind::String(text) => Ok(text.to_string()),
        kind => Err(bad(format!("`{noun}` is a string, not {}", kind.noun()))),
    }
}

/// Reads a measurement name: a string that [`line::Measurement::new`] takes with no
/// tags, other than [`gap::MEASUREMENT`].
fn measurement(value: &Value) -> Result<line::Measurement, Diagnostic> {
    let bad = |message: String| {
        Diagnostic::new(
            BAD_MEASUREMENT,
            value.span,
            message,
            "Write a measurement name, such as \"telemetry\"".into(),
        )
    };
    let value::Kind::String(text) = &value.kind else {
        return Err(bad(format!(
            "a measurement is a string, not {}",
            value.kind.noun()
        )));
    };
    if **text == *gap::MEASUREMENT {
        return Err(bad(format!("{text:?} is the measurement of the gap lines")));
    }
    line::Measurement::new(text, &[]).map_err(|error| bad(error.to_string()))
}

#[cfg(test)]
mod tests;
