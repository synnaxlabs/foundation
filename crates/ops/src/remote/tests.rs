use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::slice;
use std::task::{Context, Poll, Waker};

use document::diagnostic::Code;
use env::paths::{self, Found, Paths};
use serde_json::{Value, json};

use super::{Link, apply, browse, import, plan, status};
use crate::apply::Applied;
use crate::common::{self, create_browsing_node, create_node, name, placed_site, solo};
use crate::error::{Error, Problem, Unread, Unusable, Unwritten};
use crate::front_end::File;
use crate::operation::{self, DATA, Response};
use crate::plan::Output;
use crate::scope::{Limits, Scope, most};
use crate::{Failure, Node, Run, serve};

mod imports;

/// A failure of `reach`, or of a request.
fn lost() -> Failure {
    Failure {
        code: Code::new("node.not-running"),
        message: "no node runs in foundation-data".to_owned(),
        fix: "Start one".to_owned(),
    }
}

/// The paths of a host with `site.hcl`, which [`placed_site`] gives.
fn with_site(host: &sim::node::Node) -> Paths {
    host.make_dir(Path::new(WORK));
    host.set_working_dir(Path::new(WORK));
    let paths = host.paths();
    paths
        .write(Path::new("site.hcl"), placed_site().as_bytes())
        .expect("a write");
    paths
}

/// The text that [`Mode::Changes`] writes in most tests.
const CHANGED: &str = "# changed\n";

/// The working directory of the host of [`with_site`].
const WORK: &str = "/work";

/// What a `reach` of [`Remote`] does.
enum Mode {
    /// Serves each request on the node as `@admin`.
    Serve,
    /// Serves each request as `Serve` does, then writes these bytes to `plant.hcl`.
    Changes(Paths, Vec<u8>),
    /// Serves each request as `Serve` does, then moves `/work/plant.hcl` of this host
    /// to `/work/old.hcl`, and makes a directory at its path when the flag is set.
    Moves(sim::node::Node, bool),
    /// Serves each request as `Serve` does, then makes the next read of `plant.hcl`
    /// on this host fail with OS error 5.
    Fails(sim::node::Node),
    /// Gives this reply to each request.
    Reply(Vec<u8>),
    /// Fails with [`lost`].
    Unreached,
    /// Gives a request that fails with [`lost`].
    Unsent,
}

/// Runs `plan`, `apply`, `browse`, and the command line with a `reach` to `node`, and
/// counts what each call of `reach` saw.
struct Remote {
    node: Node,
    mode: Mode,
    reaches: Cell<usize>,
    /// The data directory of each call of `reach` by the command line.
    data: RefCell<Vec<PathBuf>>,
    requests: Cell<usize>,
    /// The body of each request.
    bodies: RefCell<Vec<Vec<u8>>>,
}

impl Remote {
    fn new(node: Node) -> Self {
        Self::with(node, Mode::Serve)
    }

    fn with(node: Node, mode: Mode) -> Self {
        Self {
            node,
            mode,
            reaches: Cell::new(0),
            data: RefCell::new(Vec::new()),
            requests: Cell::new(0),
            bodies: RefCell::new(Vec::new()),
        }
    }

    async fn send(&self, body: &[u8]) -> Result<Vec<u8>, Failure> {
        self.requests.set(self.requests.get() + 1);
        self.bodies.borrow_mut().push(body.to_vec());
        match &self.mode {
            Mode::Serve => Ok(serve::serve(&self.node, &name("@admin"), body).await),
            Mode::Changes(paths, bytes) => {
                let reply = serve::serve(&self.node, &name("@admin"), body).await;
                paths.write(Path::new("plant.hcl"), bytes).expect("a write");
                Ok(reply)
            }
            Mode::Moves(host, dir) => {
                let reply = serve::serve(&self.node, &name("@admin"), body).await;
                let plant = Path::new("/work/plant.hcl");
                host.move_path(plant, Path::new("/work/old.hcl"));
                if *dir {
                    host.make_dir(plant);
                }
                Ok(reply)
            }
            Mode::Fails(host) => {
                let reply = serve::serve(&self.node, &name("@admin"), body).await;
                host.fail_path(Path::new("plant.hcl"), 5);
                Ok(reply)
            }
            Mode::Reply(reply) => Ok(reply.clone()),
            Mode::Unreached | Mode::Unsent => Err(lost()),
        }
    }

    fn reached(&self) -> Result<(), Failure> {
        self.reaches.set(self.reaches.get() + 1);
        match self.mode {
            Mode::Unreached => Err(lost()),
            Mode::Serve
            | Mode::Changes(..)
            | Mode::Moves(..)
            | Mode::Fails(_)
            | Mode::Reply(_)
            | Mode::Unsent => Ok(()),
        }
    }

    async fn plan(
        &self,
        paths: &Paths,
        files: &[PathBuf],
        out: Option<&Path>,
    ) -> Result<Output, Error> {
        let reach = async || {
            self.reached()?;
            Ok(async |body: &[u8]| self.send(body).await)
        };
        let scope = Scope::free(paths, &common::front_ends());
        plan(files, out, &scope, &mut Link::new(reach)).await
    }

    async fn apply(&self, paths: &Paths, file: &Path) -> Result<Applied, Error> {
        let reach = async || {
            self.reached()?;
            Ok(async |body: &[u8]| self.send(body).await)
        };
        apply(
            file,
            &Scope::free(paths, &common::front_ends()),
            &mut Link::new(reach),
        )
        .await
    }

    /// The browse of the root of `plant` in `files`.
    async fn browse(
        &self,
        paths: &Paths,
        files: Vec<PathBuf>,
    ) -> Result<crate::browse::Output, Error> {
        let reach = async || {
            self.reached()?;
            Ok(async |body: &[u8]| self.send(body).await)
        };
        let input = operation::Browse {
            connector: "plant".to_owned(),
            files,
            start: None,
            pattern: None,
            cursor: None,
            values: false,
        };
        browse(
            input,
            &Scope::free(paths, &common::front_ends()),
            &mut Link::new(reach),
        )
        .await
    }

    async fn import(
        &self,
        paths: &Paths,
        input: operation::Import,
    ) -> Result<crate::import::Output, Error> {
        let reach = async || {
            self.reached()?;
            Ok(async |body: &[u8]| self.send(body).await)
        };
        import(
            input,
            &Scope::free(paths, &common::front_ends()),
            &mut Link::new(reach),
        )
        .await
    }

    async fn status(&self) -> Result<crate::status::Output, Error> {
        let reach = async || {
            self.reached()?;
            Ok(async |body: &[u8]| self.send(body).await)
        };
        status(&mut Link::new(reach)).await
    }

    /// The run, output, and errors of `foundation <args>`.
    async fn cli(&self, paths: &Paths, args: &[&str]) -> (Run, String, String) {
        let reach = async |data: &Path| {
            self.data.borrow_mut().push(data.to_owned());
            self.reached()?;
            Ok(async |body: &[u8]| self.send(body).await)
        };
        common::cli(paths, args, reach).await
    }

    /// The run of `foundation mcp <args>` on the lines of `messages`, and the JSON of
    /// each line of its output.
    async fn mcp(
        &self,
        paths: &Paths,
        args: &[&str],
        messages: &[Value],
    ) -> (Run, Vec<Value>) {
        let reach = async |data: &Path| {
            self.data.borrow_mut().push(data.to_owned());
            self.reached()?;
            Ok(async |body: &[u8]| self.send(body).await)
        };
        let input: String = messages.iter().map(|m| m.to_string() + "\n").collect();
        let args = ["foundation", "mcp"].iter().chain(args).map(Into::into);
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let mut input = input.as_bytes();
        let run = crate::cli(
            args,
            async || crate::line(&mut input),
            &mut output,
            &mut errors,
            paths,
            &common::front_ends(),
            &common::create_kinds(),
            reach,
        )
        .await;
        assert_eq!(String::from_utf8(errors).expect("UTF-8"), "");
        let lines = String::from_utf8(output).expect("UTF-8");
        let replies = lines
            .lines()
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect();
        (run, replies)
    }

    /// The path of each file of the last request, a request of `plan`.
    fn planned(&self) -> Vec<String> {
        let bodies = self.bodies.borrow();
        let body = bodies.last().expect("a request");
        let line = body.split(|&byte| byte == b'\n').next().expect("a line");
        let request: Value = serde_json::from_slice(line).expect("JSON");
        request["plan"]["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|file| file["path"].as_str().expect("a path").to_owned())
            .collect()
    }

    /// The count of calls of `reach`, and of requests.
    fn calls(&self) -> (usize, usize) {
        (self.reaches.get(), self.requests.get())
    }
}

/// The code and message of the one problem of `error`, and its status.
fn shown(error: &Error) -> (String, String, u8) {
    let problems = error.problems();
    let [problem] = &problems[..] else {
        panic!("one problem: {problems:?}");
    };
    (
        problem.code.clone(),
        problem.message.clone(),
        error.status(),
    )
}

fn read(code: &str, message: &str, status: u8) -> (String, String, u8) {
    (code.to_owned(), message.to_owned(), status)
}

#[test]
fn plans_writes_the_plan_file_and_applies_it() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let remote = Remote::new(create_node(&fixture, mesh.clone()));
        let files = vec![(PathBuf::from("site.hcl"), placed_site())];
        let (expected, output) = remote.node.plan(files).await.expect("a plan");

        let out = Path::new("plans/site.plan");
        fixture.sim.make_dir(Path::new("/work/plans"));
        let files = [PathBuf::from("site.hcl")];
        let planned = remote.plan(&paths, &files, Some(out)).await;
        let planned = planned.expect("a plan");
        assert_eq!(crate::value(&planned), output);
        assert_eq!(paths.read(out, most()), Ok(Found::File(expected)));
        assert_eq!(remote.calls(), (1, 1));

        let applied = remote.apply(&paths, out).await.expect("an apply");
        assert_eq!(applied.file, "plans/site.plan");
        assert_eq!(applied.effect.counts.added, 3);
        assert_eq!(mesh.pointer().version, 1);
        assert_eq!(remote.calls(), (2, 2));
    });
}

#[test]
fn plan_with_no_out_writes_no_file() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let remote = Remote::new(create_node(&fixture, mesh));
        let before = paths.read(Path::new("/"), most());
        let files = [PathBuf::from("site.hcl")];
        remote.plan(&paths, &files, None).await.expect("a plan");
        assert_eq!(paths.read(Path::new("/"), most()), before);
        assert_eq!(remote.calls(), (1, 1));
    });
}

#[test]
fn plan_gives_each_failed_read_by_its_cause_and_reaches_no_node() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        fixture.sim.make_dir(Path::new("/work/config/x.hcl"));
        paths
            .write(Path::new("config/a.hcl"), b"")
            .expect("a write");
        paths
            .write(Path::new("latin.hcl"), b"\xff")
            .expect("a write");
        fixture.sim.fail_path(Path::new("site.hcl"), 5);
        let remote = Remote::new(create_node(&fixture, mesh));
        let cases = [
            (
                "gone.hcl",
                "gone.hcl",
                Unread::NotFound,
                "no file is at gone.hcl",
                2,
            ),
            (
                "config",
                "config/x.hcl",
                Unread::Directory,
                "config/x.hcl is a directory, not a file",
                2,
            ),
            (
                "latin.hcl",
                "latin.hcl",
                Unread::NotUtf8,
                "the text of latin.hcl is not UTF-8",
                2,
            ),
            (
                "site.hcl",
                "site.hcl",
                Unread::Io { code: 5 },
                "the read of site.hcl failed with OS error 5",
                1,
            ),
        ];
        for (given, path, cause, message, status) in cases {
            let files = [PathBuf::from(given)];
            let planned = remote.plan(&paths, &files, None).await;
            let Err(error) = planned else {
                panic!("{given} does not read");
            };
            let path = PathBuf::from(path);
            assert_eq!(error, Error::Read { path, cause });
            assert_eq!(shown(&error), read("ops.read", message, status));
        }
        assert_eq!(remote.calls(), (0, 0));
    });
}

#[test]
fn plan_of_a_request_one_byte_over_the_most_reaches_no_node() {
    solo(|fixture, mesh| async move {
        let paths = fixture.sim.paths();
        let empty = [File {
            path: PathBuf::from("big.hcl"),
            text: String::new(),
        }];
        let around = serve::plan_request(&empty).expect("a request").len();
        let text = "#".repeat(most() - around + 1);
        paths
            .write(Path::new("big.hcl"), text.as_bytes())
            .expect("a write");
        let remote = Remote::new(create_node(&fixture, mesh));
        let files = [PathBuf::from("big.hcl")];
        let planned = remote.plan(&paths, &files, None).await;
        let Err(error) = planned else {
            panic!("a request one byte over");
        };
        assert_eq!(error, Error::LargeRequest);
        let message = format!(
            "the request has more than {} bytes, the most that a request holds",
            most()
        );
        assert_eq!(shown(&error), read("ops.large-request", &message, 2));
        assert_eq!(remote.calls(), (0, 0));

        let text = "#".repeat(most() - around);
        paths
            .write(Path::new("big.hcl"), text.as_bytes())
            .expect("a write");
        let remote = Remote::with(remote.node, Mode::Unsent);
        let planned = remote.plan(&paths, &files, None).await;
        assert_eq!(
            planned.err(),
            Some(Error::Node(lost())),
            "a request of the most"
        );
    });
}

/// The entry paths of the directory pass the most bytes of one read.
#[test]
fn plan_and_apply_of_a_directory_of_many_entries_give_read() {
    solo(|fixture, mesh| async move {
        let paths = fixture.sim.paths();
        let dir = "d".repeat(4000);
        paths.make_dir(Path::new(&dir)).expect("a directory");
        for n in 0..4200 {
            let path = PathBuf::from(format!("{dir}/f{n:04}"));
            paths.write(&path, b"").expect("a write");
        }
        let remote = Remote::with(create_node(&fixture, mesh), Mode::Unsent);
        let path = PathBuf::from(&dir);
        let planned = remote.plan(&paths, slice::from_ref(&path), None).await;
        let Err(error) = planned else {
            panic!("a plan of a directory");
        };
        let path = PathBuf::from(&dir);
        let cause = Unread::ManyEntries;
        assert_eq!(error, Error::Read { path, cause });
        let many = format!("{dir} holds more entries than one read lists");
        assert_eq!(shown(&error), read("ops.read", &many, 2), "plan");
        let path = PathBuf::from(&dir);
        let Err(error) = remote.apply(&paths, &path).await else {
            panic!("an apply of a directory");
        };
        let directory = format!("{dir} is a directory, not a file");
        assert_eq!(shown(&error), read("ops.read", &directory, 2), "apply");
        assert_eq!(remote.calls(), (0, 0));
    });
}

/// A directory gives each of its files that a front end reads and whose name does not
/// start with `.`, in name order and in the place of the directory, and no file of a
/// directory in it.
#[test]
fn plan_reads_the_config_files_of_a_directory_in_name_order() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        fixture.sim.make_dir(Path::new("/work/d/sub"));
        for (path, bytes) in [
            ("d/b.hcl", &b""[..]),
            ("d/a.hcl", b""),
            ("d/README.md", b"\xff"),
            ("d/.c.hcl", b"\xff"),
            ("d/sub/e.hcl", b"\xff"),
            ("x.hcl", b""),
        ] {
            paths.write(Path::new(path), bytes).expect("a write");
        }
        let remote = Remote::new(create_node(&fixture, mesh));
        let files = ["site.hcl", "d", "x.hcl"].map(PathBuf::from);
        remote.plan(&paths, &files, None).await.expect("a plan");
        let sent = ["site.hcl", "d/a.hcl", "d/b.hcl", "x.hcl"];
        assert_eq!(remote.planned(), sent);
        assert_eq!(remote.calls(), (1, 1));
    });
}

/// A directory that gives no config file is refused, since an empty config plans the
/// removal of each entry of the node.
#[test]
fn plan_of_a_directory_with_no_config_file_gives_no_config() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        fixture.sim.make_dir(Path::new("/work/d/sub.d"));
        for path in ["d/README.md", "d/.a.hcl", "d/sub.d/b.hcl"] {
            paths.write(Path::new(path), b"").expect("a write");
        }
        let remote = Remote::new(create_node(&fixture, mesh));
        let files = ["site.hcl", "d"].map(PathBuf::from);
        let Err(error) = remote.plan(&paths, &files, None).await else {
            panic!("a plan of a directory with no config file");
        };
        assert_eq!(
            error,
            Error::NoConfig {
                path: PathBuf::from("d"),
                extensions: "`.hcl`".to_owned(),
            }
        );
        assert_eq!(
            shown(&error),
            read("ops.no-config", "d holds no config file", 2)
        );
        assert_eq!(
            error.problems()[0].fix,
            "Add a file whose name ends in `.hcl` and does not start with `.` to d, or \
             give another path"
        );
        assert_eq!(remote.calls(), (0, 0));
    });
}

/// A span in a file of a directory names the path of that file.
#[test]
fn the_command_line_names_the_file_of_a_directory_in_a_span() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        fixture.sim.make_dir(Path::new("/work/d"));
        paths.write(Path::new("d/a.hcl"), b"").expect("a write");
        paths
            .write(Path::new("d/b.hcl"), b"channel {\n")
            .expect("a write");
        let remote = Remote::new(create_node(&fixture, mesh));
        let (status, output, errors) = remote.cli(&paths, &["plan", "d"]).await;
        assert_eq!((status, output.as_str()), (Run::Exit(2), ""));
        assert_eq!(
            errors,
            "error[hcl.syntax]: the file needs a key, a block, or the end of the body \
             here\n  --> d/b.hcl:2:1\nfix: Write it here, or correct the text here or \
             before it\n"
        );
    });
}

/// `b.hcl` is not UTF-8, so [`Unread::NotUtf8`] shows that `plan` checked its text.
#[test]
fn plan_gives_large_request_once_the_files_that_it_read_pass_the_most() {
    solo(|fixture, mesh| async move {
        let paths = fixture.sim.paths();
        let half = most() / 2 + 1;
        let b = vec![0xff; half];
        paths.write(Path::new("b.hcl"), &b).expect("a write");
        let remote = Remote::new(create_node(&fixture, mesh));
        let files = [PathBuf::from("a.hcl"), PathBuf::from("b.hcl")];
        let a = "#".repeat(half);
        paths
            .write(Path::new("a.hcl"), a.as_bytes())
            .expect("a write");
        let planned = remote.plan(&paths, &files, None).await;
        assert_eq!(
            planned.err(),
            Some(Error::LargeRequest),
            "over the most in all"
        );

        paths.write(Path::new("a.hcl"), b"##").expect("a write");
        let planned = remote.plan(&paths, &files, None).await;
        let path = PathBuf::from("b.hcl");
        let cause = Unread::NotUtf8;
        assert_eq!(
            planned.err(),
            Some(Error::Read { path, cause }),
            "within it"
        );
        assert_eq!(remote.calls(), (0, 0));
    });
}

/// `a.hcl` is not UTF-8, so [`Unread::NotUtf8`] shows that `plan` read all of its
/// `most()` bytes.
#[test]
fn plan_of_a_file_of_exactly_the_most_reads_it() {
    solo(|fixture, mesh| async move {
        let paths = fixture.sim.paths();
        paths
            .write(Path::new("a.hcl"), &vec![0xff; most()])
            .expect("a write");
        let remote = Remote::new(create_node(&fixture, mesh));
        let planned = remote.plan(&paths, &[PathBuf::from("a.hcl")], None).await;
        let path = PathBuf::from("a.hcl");
        let cause = Unread::NotUtf8;
        assert_eq!(planned.err(), Some(Error::Read { path, cause }));
    });
}

/// `a.hcl` leaves 100 bytes of a request, fewer than the entry paths of `config` take.
/// A directory after it lists with the bytes of a whole read, not the bytes that the
/// request has left.
#[test]
fn plan_of_a_directory_after_a_large_file_reads_its_files() {
    solo(|fixture, mesh| async move {
        let paths = fixture.sim.paths();
        fixture.sim.make_dir(Path::new("/config"));
        paths
            .write(Path::new("config/f.hcl"), b"")
            .expect("a write");
        for n in 0..10 {
            paths
                .write(&PathBuf::from(format!("config/n{n}.md")), b"")
                .expect("a write");
        }
        let a = "#".repeat(most() - 100);
        paths
            .write(Path::new("a.hcl"), a.as_bytes())
            .expect("a write");
        let remote = Remote::with(create_node(&fixture, mesh), Mode::Unsent);
        let alone = remote.plan(&paths, &[PathBuf::from("a.hcl")], None).await;
        assert_eq!(alone.err(), Some(Error::Node(lost())), "a.hcl fits");
        let files = [PathBuf::from("a.hcl"), PathBuf::from("config")];
        let planned = remote.plan(&paths, &files, None).await;
        assert_eq!(planned.err(), Some(Error::Node(lost())), "config fits");
    });
}

#[test]
fn plan_whose_write_of_out_fails_gives_write_and_leaves_no_file() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let out = Path::new("site.plan");
        fixture.sim.fail_path(out, 13);
        let remote = Remote::new(create_node(&fixture, mesh));
        let files = [PathBuf::from("site.hcl")];
        let planned = remote.plan(&paths, &files, Some(out)).await;
        let Err(error) = planned else {
            panic!("the write fails");
        };
        let expected = Error::Write {
            path: out.to_owned(),
            cause: Unwritten::Io { code: 13 },
        };
        assert_eq!(error, expected);
        let message = "the write of the plan file site.plan failed with OS error 13";
        assert_eq!(shown(&error), read("ops.write", message, 1));
        assert_eq!(paths.read(out, most()), Err(paths::Error::NotFound));
    });
}

#[test]
fn write_of_a_plan_file_whose_directory_is_not_there_names_the_directory() {
    let error = Error::Write {
        path: PathBuf::from("plans/site.plan"),
        cause: Unwritten::NotFound,
    };
    let message = "the directory of the plan file plans/site.plan is not there";
    assert_eq!(shown(&error), read("ops.write", message, 1));
}

#[test]
fn a_failure_of_reach_or_of_the_request_is_the_error_of_the_command() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let files = [PathBuf::from("site.hcl")];
        let remote = Remote::with(create_node(&fixture, mesh), Mode::Unreached);
        let planned = remote.plan(&paths, &files, None).await;
        let Err(error) = planned else {
            panic!("reach fails");
        };
        assert_eq!(error, Error::Node(lost()));
        let message = "no node runs in foundation-data";
        assert_eq!(shown(&error), read("node.not-running", message, 1));
        assert_eq!(remote.calls(), (1, 0));

        let remote = Remote::with(remote.node, Mode::Unsent);
        let planned = remote.plan(&paths, &files, None).await;
        assert_eq!(planned.err(), Some(Error::Node(lost())));
        assert_eq!(remote.calls(), (1, 1));
    });
}

#[test]
fn a_reply_that_is_no_reply_gives_bad_reply() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh);
        let paths = with_site(&fixture.sim);
        let files = [PathBuf::from("site.hcl")];
        let cases: [(&[u8], &str); 5] = [
            (b"{\"output\":{}}", "no newline ends the first line"),
            (
                b"{\"errors\":[],\"status\":1}\n",
                "an error has the exit status 0, or no problem",
            ),
            (
                b"{\"errors\":[{\"code\":\"a\",\"message\":\"b\",\"fix\":\"c\",\
                  \"notes\":[]}],\"status\":0}\n",
                "an error has the exit status 0, or no problem",
            ),
            (
                b"{}\n",
                "data did not match any variant of untagged enum Answer",
            ),
            (b"{\"output\":{}}\n", "missing field `base`"),
        ];
        let mut remote = Remote::new(node);
        for (reply, cause) in cases {
            remote = Remote::with(remote.node, Mode::Reply(reply.to_vec()));
            let planned = remote.plan(&paths, &files, None).await;
            let Err(error) = planned else {
                panic!("no reply");
            };
            let message = format!("the reply of the node is no reply: {cause}");
            assert_eq!(shown(&error), read("ops.bad-reply", &message, 1));
        }
    });
}

#[test]
fn plan_whose_reply_holds_no_plan_gives_bad_reply_and_writes_no_file() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let node = create_node(&fixture, mesh);
        let files = vec![(PathBuf::from("site.hcl"), placed_site())];
        let (_, output) = node.plan(files).await.expect("a plan");
        let reply = format!("{}\n", json!({ "output": output }));
        let remote = Remote::with(node, Mode::Reply(reply.into_bytes()));
        let out = Path::new("site.plan");
        let files = [PathBuf::from("site.hcl")];
        let planned = remote.plan(&paths, &files, Some(out)).await;
        let message =
            "no plan follows the first line of the reply of `plan`".to_owned();
        assert_eq!(planned.err(), Some(Error::Reply { message }));
        assert_eq!(paths.read(out, most()), Err(paths::Error::NotFound));
    });
}

#[test]
fn apply_whose_reply_has_bytes_after_its_first_line_gives_bad_reply() {
    solo(|fixture, mesh| async move {
        let paths = fixture.sim.paths();
        paths.write(Path::new("site.plan"), b"x").expect("a write");
        let reply = b"{\"output\":{\"pointer\":{\"version\":1,\"root\":\"00\"},\
                      \"added\":0,\"changed\":0,\"removed\":0,\"homes\":0}}\nx";
        let remote =
            Remote::with(create_node(&fixture, mesh), Mode::Reply(reply.to_vec()));
        let applied = remote.apply(&paths, Path::new("site.plan")).await;
        let message = "bytes follow the first line of the reply of `apply`".to_owned();
        assert_eq!(applied.err(), Some(Error::Reply { message }));
    });
}

#[test]
fn status_whose_reply_has_bytes_after_its_first_line_gives_bad_reply() {
    solo(|fixture, mesh| async move {
        let reply = b"{\"output\":{\"connectors\":[]}}\nx".to_vec();
        let remote = Remote::with(create_node(&fixture, mesh), Mode::Reply(reply));
        let message = "bytes follow the first line of the reply of `status`".to_owned();
        assert_eq!(remote.status().await, Err(Error::Reply { message }));
        assert_eq!(remote.calls(), (1, 1));
    });
}

#[test]
fn status_text_of_roots_with_the_largest_unlisted_count_gives_the_largest_count() {
    solo(|fixture, mesh| async move {
        let reply = format!(
            "{}\n",
            json!({ "output": { "connectors": [], "roots": {
                "source": "s", "failures": ["a"], "unlisted": u64::MAX,
            }}})
        );
        let remote =
            Remote::with(create_node(&fixture, mesh), Mode::Reply(reply.into_bytes()));
        let paths = fixture.sim.paths();
        let (run, output, errors) = remote.cli(&paths, &["status"]).await;
        let text = "You can read no connector on this node.\n\nOS roots: \
                    18446744073709551615 failures, from s\nOS roots: error: a\n\
                    OS roots: left out: 18446744073709551615\n";
        assert_eq!(
            (run, output.as_str(), errors.as_str()),
            (Run::Exit(0), text, "")
        );
    });
}

#[test]
fn status_that_reaches_no_node_gives_its_failure() {
    solo(|fixture, mesh| async move {
        for (mode, calls) in [(Mode::Unreached, (1, 0)), (Mode::Unsent, (1, 1))] {
            let remote = Remote::with(create_node(&fixture, mesh.clone()), mode);
            assert_eq!(remote.status().await, Err(Error::Node(lost())));
            assert_eq!(remote.calls(), calls);
        }
    });
}

#[test]
fn apply_of_a_plan_file_that_does_not_fit_a_request_reaches_no_node() {
    solo(|fixture, mesh| async move {
        let paths = fixture.sim.paths();
        let around = serve::apply_request(b"").expect("a request").len();
        let plan = vec![b'x'; most() - around + 1];
        paths.write(Path::new("big.plan"), &plan).expect("a write");
        let remote = Remote::new(create_node(&fixture, mesh));
        let applied = remote.apply(&paths, Path::new("big.plan")).await;
        assert_eq!(applied.err(), Some(Error::LargeRequest));
        let plan = [plan, b"x".to_vec()].concat();
        paths.write(Path::new("big.plan"), &plan).expect("a write");
        let applied = remote.apply(&paths, Path::new("big.plan")).await;
        assert_eq!(applied.err(), Some(Error::LargeRequest));
        let applied = remote.apply(&paths, Path::new("gone.plan")).await;
        let path = PathBuf::from("gone.plan");
        let cause = Unread::NotFound;
        assert_eq!(applied.err(), Some(Error::Read { path, cause }));
        assert_eq!(remote.calls(), (0, 0));
    });
}

#[test]
fn an_error_of_the_node_keeps_its_problems_and_status() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        paths.write(Path::new("bad.hcl"), b"{").expect("a write");
        let remote = Remote::new(create_node(&fixture, mesh));
        let files = [PathBuf::from("bad.hcl")];
        let planned = remote.plan(&paths, &files, None).await;
        let body = serve::plan_request(&[File {
            path: PathBuf::from("bad.hcl"),
            text: "{".to_owned(),
        }])
        .expect("a request");
        let reply = serve::serve(&remote.node, &name("@admin"), &body).await;
        let expected = serve::plan_reply(&reply).map(drop);
        let Err(Error::Remote { problems, status }) = expected.clone() else {
            panic!("the node refuses the file: {expected:?}");
        };
        assert_eq!(status, 2);
        assert_eq!(planned.err(), Some(Error::Remote { problems, status }));

        let problem = Problem {
            code: "ops.stopped".to_owned(),
            message: "a\u{1b}b\\c".to_owned(),
            fix: "c".to_owned(),
            place: None,
            notes: Vec::new(),
        };
        let reply = format!(
            "{}\n",
            json!({ "errors": [problem.clone(), problem.clone()], "status": 7 })
        );
        let remote = Remote::with(remote.node, Mode::Reply(reply.into_bytes()));
        let planned = remote.plan(&paths, &files, None).await;
        let Err(error) = planned else {
            panic!("an error of the node");
        };
        assert_eq!(error.status(), 7);
        assert_eq!(*error.problems(), [problem.clone(), problem.clone()]);
        let shown = "error[ops.stopped]: a\\u{1b}b\\c\nfix: c\n";
        assert_eq!(error.text(), format!("{shown}\n{shown}"));
        assert_eq!(error.to_string(), "the node gave 2 problems");

        let reply = format!("{}\n", json!({ "errors": [problem], "status": 7 }));
        let remote = Remote::with(remote.node, Mode::Reply(reply.into_bytes()));
        let planned = remote.plan(&paths, &files, None).await;
        let shown = planned.err().map(|error| error.to_string());
        assert_eq!(shown.as_deref(), Some("a\u{1b}b\\c"), "one problem");
    });
}

#[test]
fn plan_apply_and_browse_of_a_path_that_is_not_utf8_reach_no_node() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    solo(|fixture, mesh| async move {
        let paths = fixture.sim.paths();
        let remote = Remote::new(create_node(&fixture, mesh));
        let path = PathBuf::from(OsStr::from_bytes(b"a\xff.hcl"));
        let planned = remote.plan(&paths, std::slice::from_ref(&path), None).await;
        let Err(error) = planned else {
            panic!("a path that is not UTF-8");
        };
        assert_eq!(error.problems()[0].code, "ops.path-not-utf8");
        let applied = remote.apply(&paths, &path).await;
        let Err(error) = applied else {
            panic!("a path that is not UTF-8");
        };
        assert_eq!(error.problems()[0].code, "ops.path-not-utf8");
        let browsed = remote.browse(&paths, vec![path]).await;
        let Err(error) = browsed else {
            panic!("a path that is not UTF-8");
        };
        assert_eq!(error.problems()[0].code, "ops.path-not-utf8");
        assert_eq!(error.status(), 2);
        assert_eq!(remote.calls(), (0, 0));
    });
}

/// The filter runs on the bytes of a name, so only an entry that it keeps must be UTF-8.
#[test]
fn plan_of_a_directory_skips_a_name_that_is_not_utf8_and_refuses_a_kept_one() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        fixture.sim.make_dir(Path::new("/work/d"));
        let png = PathBuf::from("d").join(OsStr::from_bytes(b"\xff.png"));
        paths.write(&png, b"").expect("a write");
        paths.write(Path::new("d/a.hcl"), b"").expect("a write");
        let remote = Remote::with(create_node(&fixture, mesh), Mode::Unsent);
        let files = ["site.hcl", "d"].map(PathBuf::from);
        let planned = remote.plan(&paths, &files, None).await;
        assert_eq!(
            planned.err(),
            Some(Error::Node(lost())),
            "the png is skipped"
        );
        let hcl = PathBuf::from("d").join(OsStr::from_bytes(b"\xff.hcl"));
        paths.write(&hcl, b"").expect("a write");
        let Err(error) = remote.plan(&paths, &files, None).await else {
            panic!("an entry that is not UTF-8");
        };
        let problems = error.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].code, "ops.path-not-utf8");
        assert_eq!(
            problems[0].message,
            format!("the path {hcl:?} is not UTF-8")
        );
    });
}

#[test]
fn the_command_line_plans_and_applies_on_the_node() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let remote = Remote::new(create_node(&fixture, mesh));
        let (status, output, errors) = remote
            .cli(&paths, &["plan", "site.hcl", "--out", "site.plan"])
            .await;
        assert_eq!((status, errors.as_str()), (Run::Exit(0), ""));
        assert!(output.contains("3 to add"), "{output}");
        let (status, output, errors) =
            remote.cli(&paths, &["plan", "site.hcl", "--json"]).await;
        assert_eq!((status, errors.as_str()), (Run::Exit(0), ""));
        let output: serde_json::Value = serde_json::from_str(&output).expect("JSON");
        assert_eq!(output["added"], 3);
        let (status, output, errors) =
            remote.cli(&paths, &["apply", "site.plan", "--json"]).await;
        assert_eq!((status, errors.as_str()), (Run::Exit(0), ""));
        let output: serde_json::Value = serde_json::from_str(&output).expect("JSON");
        assert_eq!(output["file"], "site.plan");
        let (status, output, errors) = remote.cli(&paths, &["plan", "gone.hcl"]).await;
        assert_eq!((status, output.as_str()), (Run::Exit(2), ""));
        assert!(errors.contains("no file is at gone.hcl"), "{errors}");
        assert_eq!(remote.calls(), (3, 3));
    });
}

/// The line of an apply escapes the name of its plan file as an error does.
#[test]
fn the_command_line_escapes_the_name_of_the_plan_file() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let remote = Remote::new(create_node(&fixture, mesh));
        let plan = "it's \"b\\c\".plan";
        let (status, _, errors) = remote
            .cli(&paths, &["plan", "site.hcl", "--out", plan])
            .await;
        assert_eq!((status, errors.as_str()), (Run::Exit(0), ""));
        let (status, output, errors) = remote.cli(&paths, &["apply", plan]).await;
        let applied = "Applied it's \"b\\\\c\".plan: 3 added, 1 home listed.\n";
        assert_eq!(
            (status, output.as_str(), errors.as_str()),
            (Run::Exit(0), applied, "")
        );
    });
}

/// `plan`, `apply`, and `status` reach the node of `--data`, else of `foundation-data`.
#[test]
fn the_command_line_reaches_the_node_of_data() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let remote = Remote::new(create_node(&fixture, mesh.clone()));
        for args in [
            &["plan", "site.hcl", "--data", "other", "--out", "site.plan"][..],
            &["apply", "--data", "other", "site.plan"],
            &["plan", "site.hcl"],
        ] {
            let (status, _, errors) = remote.cli(&paths, args).await;
            assert_eq!((status, errors.as_str()), (Run::Exit(0), ""), "{args:?}");
        }
        let (other, data) = (PathBuf::from("other"), PathBuf::from(DATA));
        let reached = [other.clone(), other.clone(), data.clone()];
        assert_eq!(*remote.data.borrow(), reached);

        let reply = b"{\"output\":{\"connectors\":[]}}\n".to_vec();
        let remote = Remote::with(create_node(&fixture, mesh), Mode::Reply(reply));
        for args in [&["status", "--data", "other"][..], &["status"]] {
            let (status, _, errors) = remote.cli(&paths, args).await;
            assert_eq!((status, errors.as_str()), (Run::Exit(0), ""), "{args:?}");
        }
        assert_eq!(*remote.data.borrow(), [other, data]);
    });
}

/// A `tools/call` message of `name` with `arguments`.
fn call(id: u64, name: &str, arguments: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

/// The result of a tool call that failed with `error`.
fn failed(error: &Error) -> Value {
    json!({
        "content": [{ "type": "text", "text": error.json().to_string() }],
        "isError": true,
    })
}

/// `foundation mcp` reaches the node of its `--data` once, at the first call that
/// needs it, and each output parses back to the typed output.
#[test]
fn mcp_plans_applies_and_shows_status_on_one_reach_of_the_node_of_data() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let remote = Remote::new(create_node(&fixture, mesh.clone()));
        let messages = [
            call(1, "version", &json!({})),
            call(
                2,
                "plan",
                &json!({ "files": ["site.hcl"], "out": "site.plan" }),
            ),
            call(3, "apply", &json!({ "plan": "site.plan" })),
            call(4, "status", &json!({})),
        ];
        let (run, replies) = remote.mcp(&paths, &["--data", "other"], &messages).await;
        assert_eq!(run, Run::Exit(0));
        let names = ["version", "plan", "apply", "status"];
        assert_eq!(replies.len(), names.len());
        for (reply, name) in replies.iter().zip(names) {
            let content = &reply["result"]["structuredContent"];
            let response: Response = serde_json::from_value(json!({ name: content }))
                .unwrap_or_else(|e| panic!("{name}: {e}: {reply}"));
            assert_eq!(response.json(), *content);
            assert_eq!(
                reply["result"]["content"],
                json!([{ "type": "text", "text": content.to_string() }])
            );
        }
        assert_eq!(replies[1]["result"]["structuredContent"]["added"], 3);
        assert_eq!(
            replies[2]["result"]["structuredContent"]["file"],
            "site.plan"
        );
        assert_eq!(mesh.pointer().version, 1);
        assert_eq!(remote.calls(), (1, 3));
        assert_eq!(*remote.data.borrow(), [PathBuf::from("other")]);
    });
}

/// Asserts that `tools` holds `browse`, read-only, with its input.
fn assert_browse_tool(tools: &Value) {
    let tool = tools
        .as_array()
        .expect("a list")
        .iter()
        .find(|tool| tool["name"] == "browse")
        .expect("a browse tool");
    let annotations = &tool["annotations"];
    assert_eq!(annotations["readOnlyHint"], true);
    assert_eq!(annotations["destructiveHint"], false);
    let schema = &tool["inputSchema"];
    let properties: Vec<_> = schema["properties"]
        .as_object()
        .expect("an object")
        .keys()
        .collect();
    let names = ["connector", "cursor", "files", "match", "start", "values"];
    assert_eq!(properties, names);
    assert_eq!(schema["required"], json!(["connector", "files"]));
}

/// `tools/list` holds `browse` with its input, and a tool call of `browse` gives the
/// JSON of `foundation browse --json`, and takes `match`.
#[test]
fn mcp_browse_gives_the_json_of_the_command_line() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let plant = "connector \"plant\" {\n  kind = \"tree\"\n  node = \"edge\"\n}\n";
        paths
            .write(Path::new("plant.hcl"), plant.as_bytes())
            .expect("a write");
        let remote = Remote::new(create_browsing_node(&fixture, mesh));
        let level = json!({
            "connector": "plant",
            "files": ["plant.hcl"],
            "start": "Line3",
            "values": true,
        });
        let pattern = json!({
            "connector": "plant",
            "files": ["plant.hcl"],
            "start": "Types",
            "match": "b*",
        });
        let list = json!({ "jsonrpc": "2.0", "id": 0, "method": "tools/list" });
        let messages = [list, call(1, "browse", &level), call(2, "browse", &pattern)];
        let (run, mut replies) = remote.mcp(&paths, &[], &messages).await;
        assert_eq!(run, Run::Exit(0));
        assert_browse_tool(&replies.remove(0)["result"]["tools"]);
        let args = [
            "browse",
            "plant",
            "plant.hcl",
            "--start",
            "Line3",
            "--values",
            "--json",
        ];
        let (run, json, errors) = remote.cli(&paths, &args).await;
        assert_eq!((run, errors.as_str()), (Run::Exit(0), ""));
        let output: Value = serde_json::from_str(&json).expect("JSON");
        assert_eq!(replies[0]["result"]["structuredContent"], output);
        assert_eq!(output["end"], json!({ "next": "4" }));
        assert_eq!(output["entries"][1]["value"], 21.5);
        let found = &replies[1]["result"]["structuredContent"];
        let expected = json!({
            "entries": [{
                "path": "Types/bool",
                "tag": "types.bool",
                "shape": "point",
                "type": "bool",
                "unit": null,
                "unit_code": null,
                "access": "read",
            }],
            "end": "done",
        });
        assert_eq!(*found, expected);
    });
}

/// A failed reach or request is the error result of its call, and the next call
/// reaches the node again.
#[test]
fn mcp_reaches_again_after_a_failure() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        for (mode, calls) in [(Mode::Unreached, (2, 0)), (Mode::Unsent, (2, 2))] {
            let remote = Remote::with(create_node(&fixture, mesh.clone()), mode);
            let messages =
                [call(1, "status", &json!({})), call(2, "status", &json!({}))];
            let (run, replies) = remote.mcp(&paths, &[], &messages).await;
            assert_eq!(run, Run::Exit(0));
            let result = failed(&Error::Node(lost()));
            let results: Vec<_> = replies.iter().map(|r| &r["result"]).collect();
            assert_eq!(results, [&result, &result]);
            assert_eq!(remote.calls(), calls);
            assert_eq!(*remote.data.borrow(), vec![PathBuf::from(DATA); 2]);
        }
    });
}

/// The `docs` tool states each kind that `cli` takes, and reaches no node.
#[test]
fn mcp_docs_state_each_kind_of_the_cli() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let remote = Remote::new(create_node(&fixture, mesh));
        let messages = [call(1, "docs", &json!({}))];
        let (run, replies) = remote.mcp(&paths, &[], &messages).await;
        assert_eq!(run, Run::Exit(0));
        let docs = operation::docs(&common::create_kinds());
        assert!(docs.contains("\n## `influx`\n"), "{docs}");
        assert_eq!(replies[0]["result"]["structuredContent"]["markdown"], docs);
        assert_eq!(remote.calls(), (0, 0));
    });
}

/// A call that fails before its request is an error result, and reaches no node. A
/// plan of no file is a bad argument.
#[test]
fn mcp_of_a_file_that_does_not_read_reaches_no_node() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        let remote = Remote::new(create_node(&fixture, mesh));
        let messages = [
            call(1, "plan", &json!({ "files": ["gone.hcl"] })),
            call(2, "apply", &json!({ "plan": "gone.plan" })),
            call(3, "plan", &json!({ "files": [] })),
        ];
        let (run, replies) = remote.mcp(&paths, &[], &messages).await;
        assert_eq!(run, Run::Exit(0));
        let gone = |path: &str| Error::Read {
            path: PathBuf::from(path),
            cause: Unread::NotFound,
        };
        assert_eq!(replies[0]["result"], failed(&gone("gone.hcl")));
        assert_eq!(replies[1]["result"], failed(&gone("gone.plan")));
        let message = "invalid length 0, expected at least one path";
        assert_eq!(
            replies[2]["error"],
            json!({ "code": -32602, "message": message, "data": { "errors": [{
                "code": "ops.argument", "message": message,
                "fix": "Match the arguments to `foundation --help` or \
                    `foundation <operation> --help`",
                "notes": [] }] } })
        );
        assert_eq!(remote.calls(), (0, 0));
    });
}

/// A reply that holds problems is a reply: the next call sends on the same session.
#[test]
fn mcp_keeps_the_session_after_a_reply_with_problems() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        paths.write(Path::new("bad.hcl"), b"{").expect("a write");
        let remote = Remote::new(create_node(&fixture, mesh));
        let plan = |id| call(id, "plan", &json!({ "files": ["bad.hcl"] }));
        let (run, replies) = remote.mcp(&paths, &[], &[plan(1), plan(2)]).await;
        assert_eq!(run, Run::Exit(0));
        for reply in &replies {
            assert_eq!(reply["result"]["isError"], true, "{reply}");
        }
        assert_eq!(replies[0]["result"], replies[1]["result"]);
        assert_eq!(remote.calls(), (1, 2));
    });
}

/// The limits of `foundation mcp` with no `--files` on the host of [`with_site`].
fn work() -> Limits {
    Limits {
        folders: vec![PathBuf::from(WORK)],
        data: PathBuf::from("/work/foundation-data"),
        working: PathBuf::from(WORK),
    }
}

/// A tool of `foundation mcp` reaches no file outside its folders or in its data
/// directory, also through a link, and the first such call reaches no node.
#[test]
fn mcp_refuses_each_path_outside_its_folders() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        fixture.sim.make_dir(Path::new("/out"));
        paths.write(Path::new("/out/x.hcl"), b"").expect("a write");
        fixture
            .sim
            .link_path(Path::new("/work/link"), Path::new("/out"));
        let remote = Remote::new(create_node(&fixture, mesh));
        let messages = [
            call(1, "plan", &json!({ "files": ["../out/x.hcl"] })),
            call(2, "plan", &json!({ "files": ["/out/x.hcl"] })),
            call(3, "plan", &json!({ "files": ["link/x.hcl"] })),
            call(4, "apply", &json!({ "plan": "foundation-data/admin.key" })),
        ];
        let (run, replies) = remote.mcp(&paths, &[], &messages).await;
        assert_eq!(run, Run::Exit(0));
        let outside = |path: &str| Error::Outside {
            path: PathBuf::from(path),
            limits: work(),
        };
        let hidden = Error::Hidden {
            path: PathBuf::from("foundation-data/admin.key"),
            data: PathBuf::from("/work/foundation-data"),
        };
        let results: Vec<_> = replies.iter().map(|r| r["result"].clone()).collect();
        let mut errors: Vec<_> = ["../out/x.hcl", "/out/x.hcl", "link/x.hcl"]
            .map(|path| failed(&outside(path)))
            .into();
        errors.push(failed(&hidden));
        assert_eq!(results, errors);
        assert_eq!(
            shown(&hidden),
            (
                "ops.hidden".to_owned(),
                "`foundation-data/admin.key` is in the data directory \
                 /work/foundation-data of this MCP server, which no tool reads or writes"
                    .to_owned(),
                2
            )
        );
        assert_eq!(
            shown(&outside("/out/x.hcl")),
            (
                "ops.outside".to_owned(),
                "`/out/x.hcl` is not under a folder of this MCP server (/work)"
                    .to_owned(),
                2
            )
        );
        assert_eq!(remote.calls(), (0, 0));
    });
}

/// Each entry of a directory is read through the folders of `foundation mcp`, so an
/// entry that is a link out is refused with each folder.
#[test]
fn mcp_refuses_an_entry_of_a_directory_that_leads_out() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        for dir in ["/a/d", "/b", "/out"] {
            fixture.sim.make_dir(Path::new(dir));
        }
        paths.write(Path::new("/a/d/a.hcl"), b"").expect("a write");
        paths.write(Path::new("/out/x.hcl"), b"").expect("a write");
        fixture
            .sim
            .link_path(Path::new("/a/d/x.hcl"), Path::new("/out/x.hcl"));
        let remote = Remote::new(create_node(&fixture, mesh));
        let messages = [call(1, "plan", &json!({ "files": ["/a/d"] }))];
        let folders = ["--files", "/a", "--files", "/b"];
        let (run, replies) = remote.mcp(&paths, &folders, &messages).await;
        assert_eq!(run, Run::Exit(0));
        let outside = Error::Outside {
            path: PathBuf::from("/a/d/x.hcl"),
            limits: Limits {
                folders: vec![PathBuf::from("/a"), PathBuf::from("/b")],
                data: PathBuf::from("/work/foundation-data"),
                working: PathBuf::from(WORK),
            },
        };
        assert_eq!(replies[0]["result"], failed(&outside));
        assert_eq!(
            shown(&outside),
            (
                "ops.outside".to_owned(),
                "`/a/d/x.hcl` is not under a folder of this MCP server (/a, /b)"
                    .to_owned(),
                2
            )
        );
        assert_eq!(remote.calls(), (0, 0));
    });
}

/// An `out` of `foundation mcp` that does not resolve to a name that ends in `.plan`
/// is refused before a read, and one that leads out is not written.
#[test]
fn mcp_writes_a_plan_file_only_under_its_folders_and_with_its_end() {
    solo(|fixture, mesh| async move {
        let paths = with_site(&fixture.sim);
        fixture.sim.make_dir(Path::new("/out"));
        paths.write(Path::new("/out/key"), b"key").expect("a write");
        paths
            .write(Path::new("/out/x.plan"), b"plan")
            .expect("a write");
        fixture
            .sim
            .link_path(Path::new("/work/key.plan"), Path::new("/out/key"));
        fixture
            .sim
            .link_path(Path::new("/work/x.plan"), Path::new("/out/x.plan"));
        let remote = Remote::new(create_node(&fixture, mesh));
        let plan =
            |id, out| call(id, "plan", &json!({ "files": ["gone.hcl"], "out": out }));
        let messages = [plan(1, "site.txt"), plan(2, "key.plan")];
        let (_, replies) = remote.mcp(&paths, &[], &messages).await;
        let name = |path: &str, resolved: &str| Error::PlanName {
            path: PathBuf::from(path),
            resolved: PathBuf::from(resolved),
        };
        assert_eq!(
            replies[0]["result"],
            failed(&name("site.txt", "/work/site.txt"))
        );
        assert_eq!(replies[1]["result"], failed(&name("key.plan", "/out/key")));
        assert_eq!(
            shown(&name("key.plan", "/out/key")),
            (
                "ops.plan-name".to_owned(),
                "the plan file `key.plan` resolves to /out/key, which does not end in \
                 `.plan`"
                    .to_owned(),
                2
            )
        );
        assert_eq!(remote.calls(), (0, 0));
        let out = call(
            3,
            "plan",
            &json!({ "files": ["site.hcl"], "out": "x.plan" }),
        );
        let (_, replies) = remote.mcp(&paths, &[], &[out]).await;
        let outside = Error::Outside {
            path: PathBuf::from("x.plan"),
            limits: work(),
        };
        assert_eq!(replies[0]["result"], failed(&outside));
        let read = paths.read(Path::new("/out/x.plan"), 16).expect("a read");
        assert_eq!(read, Found::File(b"plan".to_vec()));
    });
}

/// The run of `foundation mcp <args>` that ends at its start, on a host whose working
/// directory is `working`, with the home `home`, a file `/work/f`, and an empty file
/// `/work/e`: its status and standard error. With one ping on its input, a start that
/// serves gives its pong.
fn start(working: &str, home: &str, args: &[&str]) -> (Run, String, String) {
    let (run, output, errors, _) = start_with(working, home, args, None);
    (run, output, errors)
}

/// `foundation mcp` with no `--files` refuses a working directory of `/` or the home
/// folder, serves there with `--files`, and refuses a folder that is not there.
#[test]
fn mcp_starts_only_on_a_folder_that_is_not_too_wide() {
    let wide = |working: &str| Error::WideFolder {
        working: PathBuf::from(working),
    };
    assert_eq!(
        start("/", "/home", &[]),
        (Run::Exit(2), String::new(), wide("/").text())
    );
    let home = start("/home", "/home", &["--data", "/home/d"]);
    assert_eq!(home, (Run::Exit(2), String::new(), wide("/home").text()));
    assert_eq!(
        shown(&wide("/home")),
        (
            "ops.wide-folder".to_owned(),
            "the working directory /home is the root or the home folder, which is too \
             wide for the tools of an MCP server"
                .to_owned(),
            2
        )
    );
    let pong = "{\"id\":1,\"jsonrpc\":\"2.0\",\"result\":{}}\n".to_owned();
    let served = start("/home", "/home", &["--files", "/home/plant"]);
    assert_eq!(served.0, Run::Exit(2));
    let folder = Error::Folder {
        path: PathBuf::from("/home/plant"),
        cause: Unusable::Failed(env::paths::Error::NotFound),
    };
    assert_eq!(served.2, folder.text());
    assert_eq!(
        shown(&folder),
        (
            "ops.folder".to_owned(),
            "the folder `/home/plant` of `--files` is not there".to_owned(),
            2
        )
    );
    let served = start("/home", "/home", &["--files", "."]);
    assert_eq!(served, (Run::Exit(0), pong.clone(), String::new()));
    let served = start("/home", "/home", &["--files", "/work"]);
    assert_eq!(served, (Run::Exit(0), pong, String::new()));
    let file = Error::Folder {
        path: PathBuf::from("/work/f"),
        cause: Unusable::File,
    };
    let refused = start("/home", "/home", &["--files", "/work/f"]);
    assert_eq!(refused, (Run::Exit(2), String::new(), file.text()));
    assert_eq!(
        shown(&file),
        (
            "ops.folder".to_owned(),
            "the folder `/work/f` of `--files` is a file".to_owned(),
            2
        )
    );
    let empty = Error::Folder {
        path: PathBuf::from("/work/e"),
        cause: Unusable::File,
    };
    let refused = start("/home", "/home", &["--files", "/work/e"]);
    assert_eq!(refused, (Run::Exit(2), String::new(), empty.text()));
}

/// The run, output, and errors of `foundation mcp <args>` that reads one ping, at
/// `working` with home folder `home`, and the data directory that it leaves there.
/// `/work` holds the file `f` and the empty file `e`, and `fault` fails a path with an
/// OS error code.
fn start_with(
    working: &str,
    home: &str,
    args: &[&str],
    fault: Option<(&str, i32)>,
) -> (Run, String, String, Option<Found>) {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = sim.node(sim::node::Config::default());
    for folder in [working, home] {
        if folder != "/" && folder != "/work" {
            host.make_dir(Path::new(folder));
        }
    }
    host.make_dir(Path::new("/work"));
    host.set_working_dir(Path::new(working));
    host.set_home(Path::new(home));
    let paths = host.paths();
    paths.write(Path::new("/work/f"), b"f").expect("a write");
    paths.write(Path::new("/work/e"), b"").expect("a write");
    if let Some((path, code)) = fault {
        host.fail_path(Path::new(path), code);
    }
    let request = async |_: &[u8]| -> Result<Vec<u8>, Failure> {
        panic!("a start sent a request")
    };
    let reach = async |_: &Path| Ok(request);
    let args = ["foundation", "mcp"].iter().chain(args).map(Into::into);
    let ping = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
    let (mut output, mut errors) = (Vec::new(), Vec::new());
    let mut input = ping.as_bytes();
    let front_ends = common::front_ends();
    let kinds = common::create_kinds();
    let cli = crate::cli(
        args,
        async || crate::line(&mut input),
        &mut output,
        &mut errors,
        &paths,
        &front_ends,
        &kinds,
        reach,
    );
    let Poll::Ready(run) = pin!(cli).poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("a start waited");
    };
    let text = |bytes| String::from_utf8(bytes).expect("UTF-8");
    let data = Path::new(working).join(DATA);
    let left = paths.read(&data, 1 << 10).ok();
    (run, text(output), text(errors), left)
}

/// A user who cannot write `/` starts `foundation mcp` at `/` with no `--files`, as a
/// client may. The decision says it refuses with the fix that names `--files`.
#[test]
fn mcp_at_the_root_that_cannot_make_data_is_too_wide() {
    let wide = Error::WideFolder {
        working: PathBuf::from("/"),
    };
    let (run, output, errors, _) =
        start_with("/", "/home", &[], Some(("/foundation-data", 13)));
    assert_eq!(
        (run, output, errors),
        (Run::Exit(2), String::new(), wide.text())
    );
}

/// A refused start at the home folder makes nothing there.
#[test]
fn mcp_refused_at_home_makes_no_data_directory() {
    let (run, _, _, left) = start_with("/home", "/home", &[], None);
    assert_eq!((run, left), (Run::Exit(2), None));
}

/// A folder of `--files` that is not there, or that is a file, also an empty one,
/// makes no data directory.
#[test]
fn mcp_refused_for_a_folder_makes_no_data_directory() {
    for folder in ["/work/missing", "/work/f", "/work/e"] {
        let (run, _, _, left) =
            start_with("/work", "/home", &["--files", folder], None);
        assert_eq!((run, left), (Run::Exit(2), None), "{folder}");
    }
}

/// `foundation mcp` makes its data directory, and refuses one whose directory above
/// is not there.
#[test]
fn mcp_makes_its_data_directory() {
    let unmade = Error::Data {
        path: PathBuf::from("gone/data"),
        error: env::paths::Error::NotFound,
    };
    let (run, _, errors) = start("/work", "/home", &["--data", "gone/data"]);
    assert_eq!((run, errors), (Run::Exit(1), unmade.text()));
    assert_eq!(
        shown(&unmade),
        (
            "ops.data".to_owned(),
            "the directory gone above the data directory `gone/data` is not there"
                .to_owned(),
            1
        )
    );
}
