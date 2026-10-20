- **SECRET SOURCES** A config names where the value of a secret comes from, never the
  value: `env("NAME")`, `file("/absolute/path")`, or `secret("full.name")`. `secret()`
  also takes its name as a reference, `secret(full.name)`, as each name in a config does
  (HCL REFERENCES). `secret::Source::read` reads each attribute that takes a secret,
  such as `token` of the `influx` kind. A bare string, another function, a count of
  arguments other than 1, an argument that is not a string, an empty variable name or
  one with `=` or NUL, a `file()` path that does not start with `/`, that has NUL, or
  that has a `..` segment between `/` or `\`, and a bad `secret()` name are
  `secret.source`. Its text
  never quotes what the user wrote, because that can be a pasted token. `Source`
  displays its call with the argument as a string, such as `secret("full.name")`, so
  `plan` and `status` show the source and never the value. The person decided this on
  2026-10-13T20:59:00Z: "Yeah that's fine", to the three forms that `laptop.monitor`
  gave in https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6155082412.
  Supersedes the bare secret name of K4 (`docs/decisions/access/k4.md`), item 2 of the
  ruling in the body of https://github.com/synnaxlabs/foundation/issues/2583 ("The
  token is a secret name"), and Q5 of
  https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6150705009, and
  the node environment variable or file adapters of SECRET STORES AS ADAPTERS
  (`docs/decisions/access/secret-stores-as-adapters.md`).
  `laptop.architect-2` approved the reference form and the display of the argument as a
  string on 2026-10-13T22:18:44Z
  (https://github.com/synnaxlabs/foundation/pull/2619#issuecomment-6156410171). The path
  rule is the same text rule on every host, because no node runs on Windows and a rule
  can widen later with no break. The trigger to take a drive letter is the first node
  that runs on Windows. Decided by `laptop.architect-2` on 2026-10-13T22:22:37Z
  (https://github.com/synnaxlabs/foundation/pull/2619#issuecomment-6156477687).
  `laptop.architect-2` approved the refusal of NUL in a path on 2026-10-13T22:29:57Z
  (https://github.com/synnaxlabs/foundation/pull/2619#issuecomment-6156590921). The rest
  was decided by `laptop.architect-2` on 2026-10-13T21:05:46Z
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6155189112).

  `secret::Resolver` gives the value of a source on one shard. `env()` reads the
  environment of the node process, which `node` main reads once at start
  (`std::env::vars_os()`) and gives in `node::Config::vars`, since the `token` of the
  `influx` kind. `laptop.architect-2` moved this read to that PR on
  2026-10-14T03:52:12Z
  (https://github.com/synnaxlabs/foundation/pull/2662#issuecomment-6160488141). No code
  below `node` reads the OS environment.
  To change the value, restart the node. A value that is not UTF-8 gives its bytes.
  `secret::Source::File` holds an `env::files::Absolute`, the one type of the path rule
  above, and `file()` reads it through `env::files::Files::open_absolute`, as
  `laptop.architect-2` ruled on 2026-10-13T22:35:35Z
  (https://github.com/synnaxlabs/foundation/pull/2621#issuecomment-6156672291).
  `secret()` reads through the store (SECRET STORES AS ADAPTERS).
  `secret::Resolver::new` makes the one turn that `file()` and the store share on a
  shard: one read at a time, in the order they come, in the erased pool, at most 64 KiB,
  less one trailing `\n` or `\r\n`. So the memory bound of SECRET STORES AS ADAPTERS
  holds for both. A variable of more than 64 KiB is `secret::Error::Unavailable`.
  `laptop.architect-2` decided the turn in `Resolver::new`, the one `secret::Error`, and
  this bound on 2026-10-14T03:11:41Z
  (https://github.com/synnaxlabs/foundation/pull/2662#issuecomment-6160027911). An
  empty value, a variable that is not set, and a file that is not there are
  `secret::Error::Missing`, with a text that says where to put the value.
  `Context::secret(&Source)` gives each error as `Error::Retry` with the source in its
  text, such as `the secret env("INFLUX_TOKEN"): the environment variable INFLUX_TOKEN
  of the node is not set. Start the node with it set to the value`, other than `BadName`
  of the store, which stays `Error::Config`. The mode check of #2596 is for the store of
  `secret()` only, because Docker and Kubernetes mount a secret file with mode 0444 or
  0644. A local `plan` reads each source of each connector of the planning node. A
  remote `plan` reads them only for each connector that its subject may apply (`apply`
  and `admin` on its name), through a clone of the resolver of the shard, as a run does,
  and drops the value. Until #2694, a subject with `apply` and `admin` on the name of a
  new connector but not on its status channels learns from a remote `plan` the error of
  each source that gives no value: whether it is set or there, whether it is empty, its
  size when over 64 KiB, the OS error of a failed file call, and, for `secret()`, the
  absolute path of the data directory. A remote `plan` reads no source of another
  connector, so a subject that may only plan learns nothing of the files and the
  environment of the node. Each source that gives no value, `BadName` too, is a
  `secret.unresolved` problem at its call, with the text `the secret <source>: <error>`.
  These problems come after those of `config::plan::plan` in one list: the connectors in
  name order, and the sources of each in the order of its config. A plan that
  `config::check` refuses reads no source. A plan file that a subject who may not apply
  a connector made has no read of its sources, also when a subject who may apply it
  applies the file, until `apply` reads them (#2694). This is the plan of item 7
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6161179051), with
  the filter by subject that `laptop.architect-2` approved on 2026-10-14T19:52:43Z
  (https://github.com/synnaxlabs/foundation/pull/2692#issuecomment-6177025658). For a
  remote plan, it supersedes "each connector that the planning node runs" in item 7 of
  https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6155189112. A
  connector on another node is not checked at plan until #239. A person who can apply a
  spec can make the node read any variable of its environment and any file that its user
  can read. The trigger for a limit that the operator of the node sets at start, such as
  directories and variable names: a spec applied by a subject that does not run the
  node. Decided by `laptop.architect-2` on 2026-10-13T21:05:46Z
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6155189112), with
  the surface that it approved on 2026-10-13T21:26:42Z
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6155552036).

  A kind names `Source` and `Value` through `connector::secret`, which re-exports them
  from `secret`. So a `connector-<kind>` crate does not depend on `secret`, and does
  not see `Resolver`, the stores, or the seal, which no kind may call. Lost: `secret`
  in the `connector-<kind>` line of the crate map. Decided by `laptop.architect-2` on
  2026-10-14T04:40:48Z
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6161042898).

  The status of a run hides the last value of each source that `Context::secret` read in
  that run: in each text of `Context::error`, of `Context::note`, and of the end of the
  run, it shows one `<secret>` in place of each run of chars that the
  `String::from_utf8_lossy` form of such a value covers, with no ASCII white space at
  its ends and not empty, before the cut at 1024 bytes. So a server that sends a token
  back in its answer does not put it in the status or in the log lines that the status
  writes. A kind adds no mark of its own for a value that `Context::secret` gave. A form
  of a value that a kind makes, such as the base64 of `user:password`, is the duty of
  that kind. The trigger to hide values in the browse answer of #2705 is a browse error
  that holds server text. Decided by `laptop.architect-2` on 2026-10-16T17:54:49Z
  (https://github.com/synnaxlabs/foundation/issues/2849#issuecomment-6221505945 and
  https://github.com/synnaxlabs/foundation/issues/3603#issuecomment-6295375095).
