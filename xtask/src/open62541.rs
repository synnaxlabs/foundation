//! Copies a release of open62541 into `patches/open62541/`, and checks a copy: each
//! C file that our options compile, each header that it includes, and each file of
//! [`EXTRA`].

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::Value;

use crate::files;

/// The upstream repository.
pub(crate) const URL: &str = "https://github.com/open62541/open62541.git";

/// The place of the copy, under the workspace root.
const DEST: &str = "patches/open62541";

/// The `cmake` options of our build: no architecture, so the event loop and the clock
/// are ours, and no feature the connector does not use.
const OPTIONS: [&str; 11] = [
    "-DUA_ARCHITECTURE=none",
    "-DUA_MULTITHREADING=0",
    "-DUA_ENABLE_ENCRYPTION=OFF",
    "-DUA_ENABLE_PUBSUB=OFF",
    "-DUA_ENABLE_XML_ENCODING=OFF",
    "-DUA_ENABLE_JSON_ENCODING=OFF",
    "-DUA_ENABLE_SUBSCRIPTIONS_EVENTS=OFF",
    "-DUA_ENABLE_HISTORIZING=OFF",
    "-DUA_ENABLE_DA=OFF",
    "-DUA_NAMESPACE_ZERO=MINIMAL",
    "-DUA_ENABLE_DETERMINISTIC_RNG=ON",
];

/// The global clock functions. Our shim gives each a fixed time, so a call reads no
/// clock, and one on a path that runs gives a wrong time with no error.
const CLOCKS: [&str; 3] = [
    "UA_DateTime_now",
    "UA_DateTime_nowMonotonic",
    "UA_DateTime_localTimeUtcOffset",
];

/// The only (file, function, symbol, access) keys with which a function references a
/// symbol outside the copy that neither [`SYMBOLS`] nor [`FILE_SYMBOLS`] admits. A
/// clock counts only as a call.
const FUNCTION_SYMBOLS: [(&str, &str, &str, Access); 13] = [
    // `isdigit` reads the locale, which stays C: nothing calls `setlocale`.
    (
        "deps/musl_inet_pton.c",
        "musl_inet_pton",
        "__ctype_b_loc",
        Access::Call,
    ),
    // Writes `errno` and never reads it.
    (
        "deps/musl_inet_pton.c",
        "musl_inet_pton",
        "__errno_location",
        Access::Call,
    ),
    // Sets `errno` to 0 before `strtod` and reads only the error of that call.
    (
        "deps/parse_num.c",
        "parseDouble",
        "__errno_location",
        Access::Call,
    ),
    // `strtod` reads the decimal point of the locale, which stays C.
    ("deps/parse_num.c", "parseDouble", "strtod", Access::Call),
    // `UA_Server_runUntilInterrupt`, which we never call.
    (
        "plugins/ua_config_default.c",
        "interruptServer",
        "UA_DateTime_nowMonotonic",
        Access::Call,
    ),
    // The build date of a server config, for the test server only.
    (
        "plugins/ua_config_default.c",
        "setDefaultConfig",
        "UA_DateTime_now",
        Access::Call,
    ),
    // The stdout logger, which we replace with our own.
    (
        "plugins/ua_log_stdout.c",
        "UA_Log_Stdout_log",
        "UA_DateTime_localTimeUtcOffset",
        Access::Call,
    ),
    (
        "plugins/ua_log_stdout.c",
        "UA_Log_Stdout_log",
        "UA_DateTime_now",
        Access::Call,
    ),
    // ECC user tokens, which need encryption, which is off.
    (
        "src/util/ua_encryptedsecret.c",
        "encryptUserIdentityTokenEcc",
        "UA_DateTime_now",
        Access::Call,
    ),
    // The start value of the random state, which `UA_ENABLE_DETERMINISTIC_RNG` keeps
    // from the clock.
    (
        "src/util/ua_util.c",
        "UA_random_seed",
        "UA_DateTime_now",
        Access::Call,
    ),
    // Our change: a draw on a thread with no start value prints its name and aborts.
    (
        "src/util/ua_util.c",
        "UA_rng_require",
        "abort",
        Access::Call,
    ),
    (
        "src/util/ua_util.c",
        "UA_rng_require",
        "fprintf",
        Access::Call,
    ),
    (
        "src/util/ua_util.c",
        "UA_rng_require",
        "stderr",
        Access::Address,
    ),
];

/// The release files that the copy holds and the library of our options does not
/// compile. A `.c` file goes in `sources.txt`, so it builds with `flags.txt`.
const EXTRA: [&str; 2] = [
    // The timer of the event loop, which takes the time as an input.
    "arch/common/timer.c",
    // The shim includes it to hold a `UA_Timer` in the loop.
    "arch/common/timer.h",
];

/// The flags of the upstream compile, other than `-D`, `-I`, and `-std`, that change
/// the code. `flags.txt` keeps them.
const CODE_FLAGS: [&str; 7] = [
    "-fno-strict-aliasing",
    "-fexceptions",
    "-ffunction-sections",
    "-fdata-sections",
    "-fno-unwind-tables",
    "-fno-asynchronous-unwind-tables",
    "-fno-math-errno",
];

/// The flags of the upstream compile that `flags.txt` leaves out, other than warnings.
const LEFT_OUT: [&str; 4] = [
    // Only the speed of the build.
    "-pipe",
    // The profile of `build.rs` gives the level, and the check needs -O0.
    "-O3",
    // Objects of GCC's own form, which the Rust linker and objdump cannot read.
    "-flto=auto",
    "-fno-fat-lto-objects",
];

/// The symbols outside the copy that any file of it may reference. Each reads no clock,
/// file, network, randomness, or process state, except the allocator, with its reason.
/// A new symbol outside the copy also goes in `OUTSIDE` in `connector-opcua`, which
/// lists the symbols of the production build.
const SYMBOLS: [&str; 16] = [
    // The allocator of libc, since the check builds without `alloc.h`. It returns
    // addresses that the OS places at random, so no result of the copy may depend on
    // an address.
    "calloc",
    "free",
    "malloc",
    "realloc",
    // Memory and string functions, which read only the memory that they are given.
    "memcmp",
    "memcpy",
    "memmove",
    "memset",
    "strcmp",
    "strlen",
    "strncmp",
    // `shim.c` defines each to print its name and abort.
    "UA_ConnectionManager_new_POSIX_Ethernet",
    "UA_ConnectionManager_new_POSIX_TCP",
    "UA_ConnectionManager_new_POSIX_UDP",
    "UA_EventLoop_new_POSIX",
    "UA_InterruptManager_new_POSIX",
];

/// The only (file, symbol) pairs that may reference a symbol outside the copy that
/// [`SYMBOLS`] does not list, other than a clock, from anywhere in the file. Each file
/// is one that no node runs.
const FILE_SYMBOLS: [(&str, &str); 7] = [
    // The stdout logger, which we replace with our own.
    ("plugins/ua_log_stdout.c", "fflush"),
    ("plugins/ua_log_stdout.c", "printf"),
    ("plugins/ua_log_stdout.c", "puts"),
    ("plugins/ua_log_stdout.c", "stdout"),
    // The syslog logger, which we never set.
    ("plugins/ua_log_syslog.c", "syslog"),
    // `UA_fileExists`, for the semaphore file of a discovery server, which no node
    // runs.
    ("src/server/ua_discovery.c", "access"),
    ("src/server/ua_services_discovery.c", "access"),
];

/// Clones `tag` of `url` into `target/open62541/`, builds it with [`OPTIONS`], and
/// replaces `patches/open62541/` with its compiled sources, the headers in the clone
/// that they include, each file of [`EXTRA`], `LICENSE`, `sources.txt` (each `.c`
/// file), `flags.txt` (the `-D`, `-I`, and `-std` flags and the [`CODE_FLAGS`] of each
/// compile), and `VERSION` (tag and commit).
/// Then it gives what [`check`] gives for the new copy. Needs Linux, `git`, `cmake`,
/// Python 3, GCC as `cc`, and GNU `objdump` and `nm`.
///
/// # Errors
///
/// A step that fails, a flag of a compile that is neither [`kept`] nor [`left_out`],
/// a file of [`EXTRA`] that the release does not have or the library holds, and each
/// error of [`check`]. On an error, `patches/open62541/` does not change.
pub(crate) fn run(root: &Path, url: &str, tag: &str) -> Result<(), Vec<String>> {
    let work = root.join("target/open62541");
    let (src, build, stage) = (work.join("src"), work.join("build"), work.join("copy"));
    let trees = Trees {
        src: &src,
        build: &build,
    };
    let commit = compile(&work, &trees, url, tag).map_err(|e| vec![e])?;
    let found = collect(&trees).map_err(|e| vec![e])?;
    write(&stage, &src, &found, &format!("{tag}\n{commit}")).map_err(|e| vec![e])?;
    inspect(&stage, &work.join("check"), Path::new("cc"))?;
    let dest = root.join(DEST);
    remove(&dest)
        .and_then(|()| {
            let parent = dest.parent().ok_or("a copy with no directory")?;
            std::fs::create_dir_all(parent)
                .and_then(|()| std::fs::rename(&stage, &dest))
                .map_err(|e| format!("{DEST}: {e}"))
        })
        .map_err(|e| vec![e])
}

/// Builds each file of `sources.txt` in `patches/open62541/` from the copy alone,
/// with its `flags.txt` and then `-g -O0 -fno-stack-protector`, so a call stays in the
/// function that holds it in the source, except in a function that the compiler
/// inlines, and the compiler adds no reference to the canary of the stack.
///
/// # Errors
///
/// A line of `flags.txt` other than a `-D`, `-I`, or `-std` flag with its value or one
/// of [`CODE_FLAGS`], a build that fails, an `#include` or `#import` of a header
/// outside both the copy and the system directories, a reference to a symbol outside
/// the copy that neither [`SYMBOLS`], [`FILE_SYMBOLS`], nor [`FUNCTION_SYMBOLS`]
/// admits, a listed pair or key with no reference, a reference outside a function
/// that [`SYMBOLS`] and [`FILE_SYMBOLS`] do not admit, any reference to a clock
/// function other than a call, such as its address in code or data, through which any
/// code can call it, each inlined function, an `#include_next`, and a `#line`
/// directive or line marker in a `.c` or `.h` file of the copy.
pub(crate) fn check(root: &Path) -> Result<(), Vec<String>> {
    let out = root.join("target/open62541/check");
    inspect(&root.join(DEST), &out, Path::new("cc"))
}

/// Clones and builds `tag` of `url` in `work`, and gives its commit.
fn compile(
    work: &Path,
    trees: &Trees<'_>,
    url: &str,
    tag: &str,
) -> Result<String, String> {
    remove(work)?;
    let clone = ["clone", "-q", "--depth", "1", "--branch", tag, url];
    exec(Command::new("git").args(clone).arg(trees.src))?;
    let mut cmake = Command::new("cmake");
    cmake.arg("-S").arg(trees.src).arg("-B").arg(trees.build);
    let build = [
        "-DCMAKE_BUILD_TYPE=Release",
        "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON",
    ];
    exec(cmake.args(OPTIONS).args(build))?;
    let mut make = Command::new("cmake");
    make.arg("--build").arg(trees.build);
    exec(make.args(["--target", "open62541", "--parallel"]))?;
    exec(
        Command::new("git")
            .arg("-C")
            .arg(trees.src)
            .args(["rev-parse", "HEAD"]),
    )
}

/// What a built clone gives to the copy.
struct Found {
    /// Each (from, to): each source of the library, each header in the clone that
    /// it includes, and each file of [`EXTRA`]. A header outside the clone is left
    /// out, and [`check`] fails on one that a source needs.
    files: BTreeSet<(PathBuf, PathBuf)>,
    /// The `-D`, `-I`, and `-std` flags and the [`CODE_FLAGS`] of each compile, with
    /// each `-I` relative to the copy.
    flags: Vec<String>,
}

/// The [`Found`] of a built clone.
fn collect(trees: &Trees<'_>) -> Result<Found, String> {
    let commands = std::fs::read_to_string(trees.build.join("compile_commands.json"))
        .map_err(|e| format!("compile_commands.json: {e}"))?;
    let mut files = BTreeSet::new();
    let mut flags: Option<Vec<String>> = None;
    for entry in entries(&commands, trees.build)? {
        let file = trees.relative(&entry.source)?;
        let these = trees
            .flags(&entry.arguments)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        match &flags {
            Some(first) if *first != these => {
                return Err(format!("{} compiles with other flags", file.display()));
            }
            _ => flags = Some(these),
        }
        let depfile = std::fs::read_to_string(entry.object.with_extension("o.d"))
            .map_err(|e| format!("{}.d: {e}", entry.object.display()))?;
        for header in headers(&depfile) {
            if let Ok(path) = trees.relative(&header) {
                files.insert((header, path));
            }
        }
        files.insert((entry.source, file));
    }
    for extra in EXTRA {
        let from = trees.src.join(extra);
        if files.iter().any(|(_, to)| to == Path::new(extra)) {
            return Err(format!(
                "{extra}: EXTRA lists it, and the library already holds it"
            ));
        }
        if !from.is_file() {
            return Err(format!(
                "{extra}: EXTRA lists it, and the release has no such file"
            ));
        }
        files.insert((from, PathBuf::from(extra)));
    }
    Ok(Found {
        files,
        flags: flags.ok_or("compile_commands.json has no file of the library")?,
    })
}

/// The source and build trees of one clone.
struct Trees<'a> {
    src: &'a Path,
    build: &'a Path,
}

impl Trees<'_> {
    /// `path` relative to the copy: a path in the source tree keeps its place, and a
    /// generated file goes under `src_generated/`.
    fn relative(&self, path: &Path) -> Result<PathBuf, String> {
        let path = files::normalize(path);
        if let Ok(generated) = path.strip_prefix(self.build.join("src_generated")) {
            return Ok(files::normalize(
                &Path::new("src_generated").join(generated),
            ));
        }
        path.strip_prefix(self.src)
            .map(Path::to_path_buf)
            .map_err(|_outside| format!("{} is outside the clone", path.display()))
    }

    /// The `-D`, `-I`, and `-std` flags and the [`CODE_FLAGS`] of a compile, with each
    /// `-I` relative to the copy. `arguments` starts with the compiler.
    ///
    /// # Errors
    ///
    /// An `-I` directory outside the clone, and a flag that is neither [`kept`] nor
    /// [`left_out`].
    fn flags(&self, arguments: &[String]) -> Result<Vec<String>, String> {
        let mut flags = Vec::new();
        let mut arguments = arguments.iter().skip(1);
        while let Some(argument) = arguments.next() {
            if ["-o", "-c"].contains(&argument.as_str()) {
                // `cmake` writes `-o <object> -c <source>`; `build.rs` and the check
                // give their own.
                arguments.next();
                continue;
            }
            let flag = if let Some(dir) = argument.strip_prefix("-I") {
                let dir = self.relative(Path::new(dir))?;
                // A bare `-I` takes the next flag as its directory; `-I-` is a flag.
                let dir = if dir.as_os_str().is_empty()
                    || dir.as_os_str().as_encoded_bytes().starts_with(b"-")
                {
                    Path::new(".").join(dir)
                } else {
                    dir
                };
                format!("-I{}", dir.display())
            } else {
                argument.clone()
            };
            if kept(&flag) {
                flags.push(flag);
            } else if !left_out(&flag) {
                return Err(format!(
                    "`{argument}` is in neither CODE_FLAGS nor LEFT_OUT"
                ));
            }
        }
        Ok(flags)
    }
}

/// An error for each line of `flags`, the text of `flags.txt`, that [`kept`] refuses.
fn unkept(flags: &str) -> Vec<String> {
    flags
        .lines()
        .filter(|flag| !kept(flag))
        .map(|flag| {
            format!(
                "flags.txt: `{flag}` is not a -D, -I, or -std flag with its value, or \
                 one of CODE_FLAGS"
            )
        })
        .collect()
}

/// Whether `flags.txt` may hold `flag`: a `-D`, `-I`, or `-std` flag with its value,
/// or one of [`CODE_FLAGS`].
fn kept(flag: &str) -> bool {
    CODE_FLAGS.contains(&flag)
        // A value that starts with `-` makes a flag such as `-I-`, which changes how
        // `cc` finds a header.
        || ["-D", "-I", "-std="].iter().any(|p| {
            flag.strip_prefix(p)
                .is_some_and(|value| !value.is_empty() && !value.starts_with('-'))
        })
}

/// Whether `flags.txt` leaves out `flag`: one of [`LEFT_OUT`], or a warning.
fn left_out(flag: &str) -> bool {
    LEFT_OUT.contains(&flag)
        // A warning, which changes no code. A `-W` flag with a `,`, such as `-Wl,`,
        // passes flags to another tool.
        || (flag.starts_with("-W") && !flag.contains(','))
}

/// One compile of the `open62541` library in `compile_commands.json`.
#[derive(Debug, PartialEq)]
struct Entry {
    source: PathBuf,
    object: PathBuf,
    arguments: Vec<String>,
}

/// Each [`Entry`] of the `open62541` library in `compile_commands.json`.
fn entries(commands: &str, build: &Path) -> Result<Vec<Entry>, String> {
    let commands: Value =
        serde_json::from_str(commands).map_err(|e| format!("compile_commands: {e}"))?;
    let list = commands
        .as_array()
        .ok_or("compile_commands is not a list")?;
    let mut entries = Vec::new();
    for entry in list {
        let fields = (
            entry["file"].as_str(),
            entry["output"].as_str(),
            entry["command"].as_str(),
        );
        let (Some(file), Some(output), Some(command)) = fields else {
            return Err(format!(
                "compile_commands entry with no file, output, or command: {entry}"
            ));
        };
        if output.contains("/open62541-object.dir/")
            || output.contains("/open62541-plugins.dir/")
        {
            entries.push(Entry {
                source: PathBuf::from(file),
                object: build.join(output),
                arguments: arguments(command),
            });
        }
    }
    Ok(entries)
}

/// The arguments of a shell command as `cmake` writes one: words split on whitespace,
/// with `"` quotes and `\` escapes.
fn arguments(command: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut argument = None::<String>;
    let mut quoted = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                argument.get_or_insert_default();
            }
            '\\' => argument.get_or_insert_default().extend(chars.next()),
            c if c.is_whitespace() && !quoted => arguments.extend(argument.take()),
            c => argument.get_or_insert_default().push(c),
        }
    }
    arguments.extend(argument);
    arguments
}

/// Builds the copy in `copy` into `out` with the compiler `cc`, a name that `PATH`
/// finds or an absolute path, and gives each error of [`check`].
fn inspect(copy: &Path, out: &Path, cc: &Path) -> Result<(), Vec<String>> {
    let read = |name: &str| {
        std::fs::read_to_string(copy.join(name))
            .map_err(|e| vec![format!("{}: {e}", copy.join(name).display())])
    };
    let (sources, flags) = (read("sources.txt")?, read("flags.txt")?);
    let other = unkept(&flags);
    if !other.is_empty() {
        return Err(other);
    }
    remove(out)
        .and_then(|()| std::fs::create_dir_all(out).map_err(|e| format!("{e}")))
        .map_err(|e| vec![e])?;
    let (macros, verbose) =
        spawn(Command::new(cc).args(["-xc", "-E", "-dM", "-v", "/dev/null"]))
            .and_then(wait)
            .map_err(|e| vec![e])?;
    gcc(&macros).map_err(|e| vec![e])?;
    let dirs = system_dirs(&verbose);
    let objects = build(copy, &sources, &flags, out, cc).map_err(|e| vec![e])?;
    let exported = symbols(
        objects.iter().map(|(_, object, _)| object.as_path()),
        &["--defined-only", "--extern-only"],
    )
    .map_err(|e| vec![e])?;
    let mut uses = Uses::default();
    let mut problems = line_directives(copy, Path::new("")).map_err(|e| vec![e])?;
    for (source, object, preprocessed) in objects {
        let disassembly = exec(Command::new("objdump").arg("-dr").arg(&object))
            .map_err(|e| vec![e])?;
        let relocations = exec(Command::new("objdump").arg("-r").arg(&object));
        let outside = symbols([object.as_path()], &["--undefined-only"])
            .map_err(|e| vec![e])?
            .difference(&exported)
            .cloned()
            .collect();
        let found =
            references(&disassembly, &relocations.map_err(|e| vec![e])?, &outside);
        problems.extend(
            found
                .into_iter()
                .filter_map(|found| uses.add(source, found)),
        );
        let info = exec(Command::new("objdump").arg("--dwarf=info").arg(&object));
        for function in inlined(&info.map_err(|e| vec![e])?) {
            problems.push(format!(
                "{source}: inlines `{function}`, so a clock call in it hides in its \
                 caller"
            ));
        }
        for problem in includes(&preprocessed, copy, &flags, &dirs) {
            problems.push(format!("{source}: {problem}"));
        }
    }
    problems.extend(uses.mismatches());
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// An error for each `#line` directive or line marker in a `.c` or `.h` file under
/// `dir` of the copy in `copy`, by path and line. Either moves the file that
/// [`includes`] reads.
fn line_directives(copy: &Path, dir: &Path) -> Result<Vec<String>, String> {
    let error = |path: &Path, e| format!("{}: {e}", copy.join(path).display());
    let mut paths = std::fs::read_dir(copy.join(dir))
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| dir.join(entry.file_name())))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| error(dir, e))?;
    paths.sort();
    let mut problems = Vec::new();
    for path in paths {
        if copy.join(&path).is_dir() {
            problems.extend(line_directives(copy, &path)?);
            continue;
        }
        if !path.extension().is_some_and(|e| e == "c" || e == "h") {
            continue;
        }
        // A release file need not be UTF-8, and `cc` reads a comment in any bytes.
        #[expect(clippy::disallowed_methods, reason = "a dev tool reads the copy")]
        let bytes = std::fs::read(copy.join(&path)).map_err(|e| error(&path, e))?;
        let text = String::from_utf8_lossy(&bytes);
        // `cc` skips a byte order mark at the start of a file.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        // `cc` also ends a line at a lone carriage return.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        for (index, line) in text.lines().enumerate() {
            let Some(rest) = line.trim_start().strip_prefix('#').map(str::trim_start)
            else {
                continue;
            };
            let word = rest.split(|c: char| !c.is_ascii_alphanumeric()).next();
            if word.is_some_and(|w| {
                w == "line" || w.starts_with(|c: char| c.is_ascii_digit())
            }) {
                problems.push(format!(
                    "{}:{}: holds a line directive, which moves the file that the \
                     include check reads",
                    path.display(),
                    index + 1
                ));
            }
        }
    }
    Ok(problems)
}

/// Whether `path`, relative to the copy, stays inside it.
fn inside(path: &Path) -> bool {
    let mut depth = 0_usize;
    path.components().all(|component| match component {
        Component::Normal(_) => {
            depth += 1;
            true
        }
        Component::CurDir => true,
        Component::ParentDir => depth.checked_sub(1).map(|d| depth = d).is_some(),
        Component::RootDir | Component::Prefix(_) => false,
    })
}

/// An error for each `#include_next`, and each `#include` or `#import` in a file of
/// the copy, as `preprocessed` (the output of `cc -E -dI` in `copy`) shows it, that
/// finds a header outside both the copy and the system directories `dirs`. It finds
/// the header as `cc` does, through the `-I` directories `flags` gives. Unlike
/// `cc -H`, `-dI` also shows an `#include` of a header that the unit included before.
fn includes(
    preprocessed: &str,
    copy: &Path,
    flags: &str,
    dirs: &[PathBuf],
) -> Vec<String> {
    let quoted: Vec<&Path> = flags
        .lines()
        .filter_map(|f| f.strip_prefix("-I"))
        .map(Path::new)
        .collect();
    let mut problems = Vec::new();
    let mut file = Path::new("");
    for line in preprocessed.lines() {
        if let Some(marker) = line.strip_prefix("# ")
            && let Some((_, name)) = marker.split_once(" \"")
        {
            file = Path::new(name.rsplit_once('"').map_or(name, |(name, _)| name));
            continue;
        }
        let Some((keyword, directive)) = line.split_once(' ') else {
            continue;
        };
        if !matches!(keyword, "#include" | "#import" | "#include_next") || !inside(file)
        {
            continue;
        }
        if keyword == "#include_next" {
            problems.push(format!("uses {line}, which the check cannot follow"));
            continue;
        }
        let directive = directive.trim();
        let (name, local) = match directive.as_bytes() {
            [b'"', .., b'"'] => (&directive[1..directive.len() - 1], true),
            [b'<', .., b'>'] => (&directive[1..directive.len() - 1], false),
            _ => {
                problems
                    .push(format!("includes {directive}, which is not a file name"));
                continue;
            }
        };
        let parent = file.parent().filter(|_| local);
        let user = parent
            .into_iter()
            .chain(quoted.iter().copied())
            .map(|dir| (dir.join(name), false));
        let system = dirs.iter().map(|dir| (dir.join(name), true));
        let problem = match user
            .chain(system)
            .find(|(path, _)| copy.join(path).is_file())
        {
            Some((_, true)) => continue,
            Some((path, false)) if inside(&path) => continue,
            Some((path, false)) => {
                format!("includes {}, which is outside the copy", path.display())
            }
            None => format!("includes {directive}, which no include directory holds"),
        };
        problems.push(problem);
    }
    problems
}

/// Refuses a `cc` that is not GCC, from `macros`, the output of `cc -dM -E`. The
/// check passes GCC's `-dumpbase`, whose value clang reads as a source file.
fn gcc(macros: &str) -> Result<(), String> {
    let defined = |name: &str| {
        let prefix = format!("#define {name} ");
        macros.lines().any(|line| line.starts_with(&prefix))
    };
    if defined("__GNUC__") && !defined("__clang__") {
        Ok(())
    } else {
        Err("cc is not GCC, which the check needs".to_owned())
    }
}

/// The directories of `#include <...>` in `verbose`, the standard error of
/// `cc -E -v`.
fn system_dirs(verbose: &str) -> Vec<PathBuf> {
    verbose
        .lines()
        .skip_while(|line| *line != "#include <...> search starts here:")
        .skip(1)
        .take_while(|line| *line != "End of search list.")
        .map(|line| files::normalize(Path::new(line.trim())))
        .collect()
}

/// Compiles and preprocesses each source of `sources` in `copy` into `out` at once
/// with `cc`, and gives each (source, object, the output of `cc -E -dI`).
fn build<'a>(
    copy: &Path,
    sources: &'a str,
    flags: &str,
    out: &Path,
    cc: &Path,
) -> Result<Vec<(&'a str, PathBuf, String)>, String> {
    let mut children = Vec::new();
    for (index, source) in sources.lines().enumerate() {
        let object = out.join(format!("{index}.o"));
        let compiler = |mode: &[&str]| {
            let mut cc = Command::new(cc);
            // `./` keeps a source such as `-x.c` or `@x.c` from being an option. Else
            // GCC gives cc1 the base name as `-dumpbase`, which cc1 reads as a
            // response file when it starts with `@`. With no stack protector, a
            // reference to its random canary is one that the C makes.
            cc.current_dir(copy)
                .args(flags.lines())
                .args(["-g", "-O0", "-fno-stack-protector", "-dumpbase"])
                .arg(index.to_string())
                .args(mode);
            spawn(cc.arg(Path::new(".").join(source)))
        };
        let compile = compiler(&["-c", "-o", &object.to_string_lossy()])?;
        children.push((source, object, compile, compiler(&["-E", "-dI"])?));
    }
    children
        .into_iter()
        .map(|(source, object, compile, preprocess)| {
            wait(compile)?;
            wait(preprocess).map(|(preprocessed, _)| (source, object, preprocessed))
        })
        .collect()
}

/// The name of each function that the output of `objdump --dwarf=info` shows inlined.
/// An `always_inline` function is inlined also at `-O0`.
fn inlined(info: &str) -> Vec<String> {
    let mut names = std::collections::BTreeMap::new();
    let mut origins = Vec::new();
    let (mut entry, mut inline) = ("", false);
    for line in info.lines() {
        if let Some((key, tag)) = line.split_once(">: Abbrev Number:") {
            entry = key.rsplit('<').next().unwrap_or(key);
            inline = tag.ends_with("(DW_TAG_inlined_subroutine)");
            continue;
        }
        // An attribute follows the offset of its own entry, as `<64>   DW_AT_name`.
        let line = line.split_once("> ").map_or("", |(_, rest)| rest.trim());
        if let Some(value) = line.strip_prefix("DW_AT_name") {
            names.insert(entry, value.rsplit(": ").next().unwrap_or(value));
        } else if inline && let Some(value) = line.strip_prefix("DW_AT_abstract_origin")
        {
            let origin = value.trim_start_matches([' ', ':']);
            origins.push(origin.trim_start_matches("<0x").trim_end_matches('>'));
        }
    }
    origins
        .into_iter()
        .map(|origin| names.get(origin).map_or(origin, |&name| name).to_owned())
        .collect()
}

/// The relocation types of a call or a tail call. Any other relocation takes an
/// address.
const CALLS: [&str; 3] = ["R_X86_64_PLT32", "R_AARCH64_CALL26", "R_AARCH64_JUMP26"];

/// A relocation against a symbol outside the copy or a function of [`CLOCKS`].
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Reference {
    /// The section that holds it.
    section: String,
    /// The function symbol that labels it in `objdump -d`, whole, such as `parse.0`
    /// for a nested function, or `None` when no function symbol labels it.
    function: Option<String>,
    access: Access,
    symbol: String,
}

/// How a reference uses its symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Access {
    /// A relocation of a type of [`CALLS`].
    Call,
    /// Any other relocation, which takes the address of the symbol.
    Address,
}

impl Access {
    /// What a function does to the symbol, as the verb of an error.
    fn verb(self) -> &'static str {
        match self {
            Access::Call => "calls",
            Access::Address => "takes the address of",
        }
    }
}

/// Each relocation against a symbol of `outside` or a function of [`CLOCKS`]: from
/// `disassembly`, the output of `objdump -dr`, in each section that it shows, and from
/// `relocations`, the output of `objdump -r`, in each other section. Equal ones come
/// once, as the two relocations of one address on 64-bit Arm do.
fn references(
    disassembly: &str,
    relocations: &str,
    outside: &BTreeSet<String>,
) -> BTreeSet<Reference> {
    let mut found = BTreeSet::new();
    let mut code = BTreeSet::new();
    let mut section = "";
    let mut function = None;
    let mut add = |section: &str, function: Option<&str>, line: &str| {
        if let Some((kind, symbol)) = relocation(line)
            && (outside.contains(symbol) || CLOCKS.contains(&symbol))
        {
            found.insert(Reference {
                section: section.to_owned(),
                function: function.map(str::to_owned),
                access: if CALLS.contains(&kind) {
                    Access::Call
                } else {
                    Access::Address
                },
                symbol: symbol.to_owned(),
            });
        }
    };
    for line in disassembly.lines() {
        if let Some(name) = line.strip_prefix("Disassembly of section ") {
            section = name.trim_end_matches(':');
            function = None;
            code.insert(section);
        } else if let Some(name) =
            line.strip_suffix(">:").and_then(|l| l.split_once(" <"))
        {
            // A section with no symbol at its start shows as `<.text>`.
            function = Some(name.1).filter(|name| !name.starts_with('.'));
        } else {
            add(section, function, line);
        }
    }
    for line in relocations.lines() {
        if let Some(name) = line.strip_prefix("RELOCATION RECORDS FOR [") {
            section = name.trim_end_matches("]:");
        } else if !code.contains(section) {
            add(section, None, line);
        }
    }
    found
}

/// The references to symbols outside the copy that [`SYMBOLS`] does not list, read so
/// far, as the lists key them.
#[derive(Default)]
struct Uses {
    /// Each pair of [`FILE_SYMBOLS`] with a reference.
    pairs: BTreeSet<(String, String)>,
    /// Each key of a reference that [`Uses::add`] gives to [`FUNCTION_SYMBOLS`].
    keys: BTreeSet<(String, String, String, Access)>,
}

impl Uses {
    /// Adds `found`, a reference of `file`, and gives its error when no list can admit
    /// it: the address of a clock function, or a reference outside a function that
    /// neither [`SYMBOLS`] nor [`FILE_SYMBOLS`] admits.
    fn add(&mut self, file: &str, found: Reference) -> Option<String> {
        let Reference {
            section,
            function,
            access,
            symbol,
        } = found;
        if CLOCKS.contains(&symbol.as_str()) && access == Access::Address {
            let site = function
                .map_or(format!("the section `{section}`"), |function| {
                    format!("`{function}`")
                });
            return Some(format!(
                "{file}: {site} takes the address of `{symbol}`, so a call through it \
                 escapes FUNCTION_SYMBOLS"
            ));
        }
        if SYMBOLS.contains(&symbol.as_str()) {
            return None;
        }
        if FILE_SYMBOLS.contains(&(file, &symbol)) {
            self.pairs.insert((file.to_owned(), symbol));
            return None;
        }
        let Some(function) = function else {
            return Some(format!(
                "{file}: the section `{section}` references `{symbol}` outside a \
                 function, so a use through it escapes FUNCTION_SYMBOLS"
            ));
        };
        self.keys
            .insert((file.to_owned(), function, symbol, access));
        None
    }

    /// An error for each key that [`FUNCTION_SYMBOLS`] does not list, for each listed
    /// key with no reference, and for each pair of [`FILE_SYMBOLS`] with no reference.
    fn mismatches(&self) -> Vec<String> {
        let listed: BTreeSet<(String, String, String, Access)> = FUNCTION_SYMBOLS
            .iter()
            .map(|&(file, function, symbol, access)| {
                (
                    file.to_owned(),
                    function.to_owned(),
                    symbol.to_owned(),
                    access,
                )
            })
            .collect();
        let new = self.keys.difference(&listed).map(|(file, function, symbol, access)| {
            let verb = access.verb();
            if CLOCKS.contains(&symbol.as_str()) {
                format!(
                    "{file}: `{function}` {verb} `{symbol}`, a global clock function. \
                     Find whether a node runs it; if not, add it to FUNCTION_SYMBOLS \
                     with the reason"
                )
            } else {
                format!(
                    "{file}: `{function}` {verb} `{symbol}`, which no list admits. \
                     Find whether a node runs it; if it reads no clock, file, network, \
                     randomness, or process state, add it with the reason, to \
                     FILE_SYMBOLS for a file that no node runs, else to \
                     FUNCTION_SYMBOLS"
                )
            }
        });
        let gone =
            listed
                .difference(&self.keys)
                .map(|(file, function, symbol, access)| {
                    format!(
                        "{file}: `{function}` no longer {} `{symbol}`. Remove it from \
                 FUNCTION_SYMBOLS",
                        access.verb()
                    )
                });
        let files = FILE_SYMBOLS
            .iter()
            .filter(|&&(file, symbol)| {
                !self.pairs.contains(&(file.to_owned(), symbol.to_owned()))
            })
            .map(|(file, symbol)| {
                format!(
                    "{file}: no longer references `{symbol}`. Remove it from \
                     FILE_SYMBOLS"
                )
            });
        new.chain(gone).chain(files).collect()
    }
}

/// The type and the symbol of a line of `objdump -r` that holds a relocation.
fn relocation(line: &str) -> Option<(&str, &str)> {
    let mut words = line.split_whitespace();
    let kind = words.find(|word| word.starts_with("R_"))?;
    Some((kind, words.next_back()?.split(['+', '-']).next()?))
}

/// Each header that a Make depfile names, with its escapes read: `\ ` for a space,
/// `\#` for `#`, `$$` for `$`, and `\` at the end of a line to go on.
fn headers(depfile: &str) -> Vec<PathBuf> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = depfile.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(&(' ' | '#'))) | ('$', Some('$')) => word.extend(chars.next()),
            ('\\', Some('\n')) => {
                chars.next();
                words.push(std::mem::take(&mut word));
            }
            (c, _) if c.is_whitespace() => words.push(std::mem::take(&mut word)),
            (c, _) => word.push(c),
        }
    }
    words.push(word);
    words
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.extension().is_some_and(|e| e == "h"))
        .collect()
}

/// The names of the symbols that `nm -P` with `flags` gives for `objects`.
fn symbols<'a>(
    objects: impl IntoIterator<Item = &'a Path>,
    flags: &[&str],
) -> Result<BTreeSet<String>, String> {
    let text = exec(Command::new("nm").arg("-P").args(flags).args(objects))?;
    Ok(text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

/// Writes the copy of `found` to `stage`, with `LICENSE` from `src`.
fn write(stage: &Path, src: &Path, found: &Found, version: &str) -> Result<(), String> {
    let mut sources = String::new();
    let license = (src.join("LICENSE"), PathBuf::from("LICENSE"));
    for (from, to) in found.files.iter().chain([&license]) {
        if to.extension().is_some_and(|e| e == "c") {
            sources.push_str(&to.display().to_string());
            sources.push('\n');
        }
        let to = stage.join(to);
        let dir = to.parent().ok_or("a file with no directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::copy(from, &to).map_err(|e| format!("{}: {e}", from.display()))?;
    }
    let flags = found.flags.join("\n") + "\n";
    std::fs::write(stage.join("sources.txt"), sources)
        .and_then(|()| std::fs::write(stage.join("flags.txt"), flags))
        .and_then(|()| std::fs::write(stage.join("VERSION"), format!("{version}\n")))
        .map_err(|e| format!("{}: {e}", stage.display()))
}

/// Removes `dir` when it exists.
fn remove(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", dir.display()))
        }
        _ => Ok(()),
    }
}

/// Runs `command` and gives its trimmed standard output.
fn exec(command: &mut Command) -> Result<String, String> {
    let (out, _) = wait(spawn(command)?)?;
    Ok(out.trim().to_owned())
}

/// Starts `command` with no input, and with its output captured.
fn spawn(command: &mut Command) -> Result<(String, Child), String> {
    let name = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|e| format!("{name}: {e}"))?;
    Ok((name, child))
}

/// Waits for a command from [`spawn`], and gives its standard output and error.
fn wait((name, child): (String, Child)) -> Result<(String, String), String> {
    let output = child
        .wait_with_output()
        .map_err(|e| format!("{name}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{name} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    Ok((text(&output.stdout), text(&output.stderr)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reference of `function` from `section`.
    fn labeled(
        section: &str,
        function: &str,
        access: Access,
        symbol: &str,
    ) -> Reference {
        Reference {
            function: Some(function.to_owned()),
            ..unlabeled(section, access, symbol)
        }
    }

    /// A reference from `section` that no function labels.
    fn unlabeled(section: &str, access: Access, symbol: &str) -> Reference {
        Reference {
            section: section.to_owned(),
            function: None,
            access,
            symbol: symbol.to_owned(),
        }
    }

    #[test]
    fn references_keys_each_reference_by_its_section_and_function() {
        let disassembly = "\
Disassembly of section .text:

0000000000000000 <.text>:
\t\t\t0: R_X86_64_PC32\tstrtod-0x4

Disassembly of section .text.setDefaultConfig:

0000000000000000 <setDefaultConfig>:
   0:\tpush   %rbx
\t\t\t1: R_X86_64_PLT32\tUA_DateTime_now-0x4
0000000000000040 <other>:
\t\t\t40: R_X86_64_REX_GOTPCRELX\tUA_DateTime_now-0x4
\t\t\t41: R_X86_64_PLT32\tUA_DateTime_nowMore-0x4
\t\t\t42: R_X86_64_PC32\tUA_DateTime_now_x+0x4
\t\t\t43: R_X86_64_PLT32\tstrtod-0x4
\t\t\t44: R_X86_64_PLT32\tUA_inside-0x4
0000000000000080 <seed.0>:
\t\t\t81: R_AARCH64_CALL26\tUA_DateTime_nowMonotonic
00000000000000c0 <log>:
\t\t\tc1: R_X86_64_PLT32\tUA_DateTime_localTimeUtcOffset-0x4
";
        let relocations = "\
RELOCATION RECORDS FOR [.text]:
OFFSET           TYPE              VALUE
0000000000000005 R_X86_64_PLT32    UA_DateTime_now-0x0000000000000004

RELOCATION RECORDS FOR [.text.setDefaultConfig]:
0000000000000001 R_X86_64_PLT32    UA_DateTime_now-0x0000000000000004

RELOCATION RECORDS FOR [.data.rel]:
OFFSET           TYPE              VALUE
0000000000000000 R_X86_64_64       UA_DateTime_now
0000000000000008 R_X86_64_64       UA_DateTime_nowMore
0000000000000010 R_X86_64_64       strtod

RELOCATION RECORDS FOR [.rodata]:
0000000000000000 R_AARCH64_ABS64   UA_DateTime_localTimeUtcOffset+0x8

RELOCATION RECORDS FOR [.data.rel.ro]:
0000000000000000 R_X86_64_PLT32    UA_DateTime_now
";
        let outside = BTreeSet::from(["strtod".to_owned()]);
        let config = |function, access, symbol| {
            labeled(".text.setDefaultConfig", function, access, symbol)
        };
        assert_eq!(
            Vec::from_iter(references(disassembly, relocations, &outside)),
            [
                unlabeled(".data.rel", Access::Address, "UA_DateTime_now"),
                unlabeled(".data.rel", Access::Address, "strtod"),
                unlabeled(".data.rel.ro", Access::Call, "UA_DateTime_now"),
                unlabeled(".rodata", Access::Address, "UA_DateTime_localTimeUtcOffset"),
                unlabeled(".text", Access::Address, "strtod"),
                config("log", Access::Call, "UA_DateTime_localTimeUtcOffset"),
                config("other", Access::Call, "strtod"),
                config("other", Access::Address, CLOCKS[0]),
                config("seed.0", Access::Call, "UA_DateTime_nowMonotonic"),
                config("setDefaultConfig", Access::Call, CLOCKS[0]),
            ]
        );
    }

    #[test]
    fn references_names_an_address_in_two_relocations_once() {
        let disassembly = "\
Disassembly of section .text:

0000000000000000 <now>:
\t\t\t8: R_AARCH64_ADR_PREL_PG_HI21\tUA_DateTime_now
\t\t\tc: R_AARCH64_ADD_ABS_LO12_NC\tUA_DateTime_now
\t\t\t10: R_AARCH64_ADR_GOT_PAGE\tstrtod
\t\t\t14: R_AARCH64_LD64_GOT_LO12_NC\tstrtod
";
        let outside = BTreeSet::from(["strtod".to_owned()]);
        let found = references(disassembly, "", &outside);
        assert_eq!(
            Vec::from_iter(found.iter().map(|found| found.symbol.as_str())),
            ["UA_DateTime_now", "strtod"]
        );
    }

    #[test]
    fn includes_finds_each_header_as_cc_does() {
        let root = temp("includes");
        create_files(
            &root,
            &[
                ("copy/src/local.h", ""),
                ("copy/include/a.h", ""),
                ("copy/deps/d.h", ""),
                ("out.h", ""),
                ("sys/x/time.h", ""),
                ("sys/sys/time.h", ""),
                ("sys/stdio.h", ""),
                ("sys/stdint.h", ""),
                ("sys/openssl/ssl.h", ""),
            ],
        );
        let dirs = [root.join("sys/x"), root.join("sys")];
        let out = root.join("out.h");
        let preprocessed = format!(
            "\
# 0 \"src/a.c\"
# 1 \"src/a.c\"
#include \"local.h\"
#include \"a.h\"
#include <d.h>
#include <local.h>
#include \"../../out.h\"
#include <stdio.h>
#include \"stdint.h\"
#include <time.h>
#include <sys/time.h>
#include <openssl/ssl.h>
#include UA_X
#import <none.h>
#include_next <stdio.h>
#includes <none.h>
# 1 \"{sys}/stdio.h\" 1 3 4
#include <sys/stat.h>
# 9 \"src/a.c\" 2
#pragma once
# 1 \"include/a.h\" 1
#include \"../src/local.h\"
#include \"{out}\"
",
            sys = root.join("sys").display(),
            out = out.display(),
        );
        let flags = "-DX\n-Iinclude\n-Ideps\n-std=c99\n";
        assert_eq!(
            includes(&preprocessed, &root.join("copy"), flags, &dirs),
            [
                "includes <local.h>, which no include directory holds".to_owned(),
                "includes src/../../out.h, which is outside the copy".to_owned(),
                "includes UA_X, which is not a file name".to_owned(),
                "includes <none.h>, which no include directory holds".to_owned(),
                "uses #include_next <stdio.h>, which the check cannot follow"
                    .to_owned(),
                format!("includes {}, which is outside the copy", out.display()),
            ]
        );
        remove(&root).unwrap();
    }

    #[test]
    fn line_directives_names_each_line_directive_and_marker() {
        let copy = temp("lines");
        create_files(
            &copy,
            &[
                (
                    "src/a.c",
                    "#line 5 \"x.y\"\n  #  12 \"b\"\n#lines\n// #line 1\n",
                ),
                ("src/b.txt", "#line 1\n"),
                ("src/d.h", "\u{feff}#line 2\n"),
                ("src/m.h", "#line 3\n"),
                ("src/q.c", "#line 4\n"),
                ("src/r.c", "int a;\r#line 7\r\n#line 8\r\n"),
                ("src/z.c", "#line 5\n"),
                ("src/k.c", "#line 6\n"),
                ("include/c.h", "#define line 1\n# 1 \"c.y\" 1\n#line\n"),
            ],
        );
        std::fs::write(copy.join("src/e.c"), b"/* Andr\xe9 */\n#line 2\n").unwrap();
        let held = |at: &str| {
            format!(
                "{at}: holds a line directive, which moves the file that the include \
                 check reads"
            )
        };
        assert_eq!(
            line_directives(&copy, Path::new("")),
            Ok(vec![
                held("include/c.h:2"),
                held("include/c.h:3"),
                held("src/a.c:1"),
                held("src/a.c:2"),
                held("src/d.h:1"),
                held("src/e.c:2"),
                held("src/k.c:1"),
                held("src/m.h:1"),
                held("src/q.c:1"),
                held("src/r.c:2"),
                held("src/r.c:3"),
                held("src/z.c:1")
            ])
        );
        let missing = copy.join("missing");
        assert_eq!(
            line_directives(&missing, Path::new("")),
            Err(format!(
                "{}/: No such file or directory (os error 2)",
                missing.display()
            ))
        );
        remove(&copy).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_refuses_a_line_directive_in_the_copy() {
        let (root, repo, result) = run_after("line", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let path = root.join("patches/open62541/src/util/ua_util.c");
        let old = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("#line 1 \"gen.y\"\n{old}")).unwrap();
        assert_eq!(
            check(&root),
            Err(vec![
                "src/util/ua_util.c:1: holds a line directive, which moves the file \
                 that the include check reads"
                    .to_owned(),
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    fn inlined_names_each_inlined_function() {
        let info = "\
 <1><41>: Abbrev Number: 4 (DW_TAG_subprogram)
    <42>   DW_AT_name        : (indirect string, offset: 0xbd): UA_Log_Stdout_log
 <2><63>: Abbrev Number: 5 (DW_TAG_inlined_subroutine)
    <64>   DW_AT_abstract_origin: <0xc0>
    <68>   DW_AT_low_pc      : 0x21
 <2><70>: Abbrev Number: 6 (DW_TAG_formal_parameter)
    <71>   DW_AT_abstract_origin: <0x41>
 <2><75>: Abbrev Number: 5 (DW_TAG_inlined_subroutine)
    <76>   DW_AT_abstract_origin: <0xd0>
 <1><c0>: Abbrev Number: 10 (DW_TAG_subprogram)
    <c1>   DW_AT_name        : (indirect string, offset: 0xa0): helper
 <1><c8>: Abbrev Number: 7 (DW_TAG_variable)
    <c9>   DW_AT_name        : y
";
        assert_eq!(inlined(info), ["helper", "d0"]);
    }

    #[test]
    fn gcc_refuses_a_cc_that_is_not_gcc() {
        let gnu = "#define __STDC__ 1\n#define __GNUC__ 13\n";
        assert_eq!(gcc(gnu), Ok(()));
        let refused = Err("cc is not GCC, which the check needs".to_owned());
        assert_eq!(gcc(&format!("{gnu}#define __clang__ 1\n")), refused);
        assert_eq!(
            gcc("#define __STDC__ 1\n#define __GNUC_MINOR__ 2\n"),
            refused
        );
    }

    #[test]
    #[cfg(unix)]
    fn check_refuses_a_cc_that_is_not_gcc() {
        let dir = temp("clang");
        create_files(
            &dir,
            &[("copy/sources.txt", "a.c\n"), ("copy/flags.txt", "")],
        );
        // GCC's macros on standard error make a check that reads the wrong stream
        // pass.
        let script = "#!/bin/sh\n\
                      echo '#define __GNUC__ 4'\n\
                      echo '#define __clang__ 1'\n\
                      echo '#define __GNUC__ 13' >&2\n";
        let cc = dir.join("cc");
        // A child writes it: while this process holds it open for writing, a process
        // that another test forks holds it open too, and running it fails with
        // ETXTBSY.
        exec(
            Command::new("sh")
                .args(["-c", r#"printf %s "$1" > "$0" && chmod 755 "$0""#])
                .arg(&cc)
                .arg(script),
        )
        .unwrap();
        assert_eq!(
            inspect(&dir.join("copy"), &dir.join("out"), &cc),
            Err(vec!["cc is not GCC, which the check needs".to_owned()])
        );
        remove(&dir).unwrap();
    }

    #[test]
    fn system_dirs_reads_the_search_list_of_angle_includes() {
        let verbose = "\
#include \"...\" search starts here:
 /q
#include <...> search starts here:
 /usr/lib/gcc/x86_64-linux-gnu/13/include
 /usr/lib/gcc/x86_64-linux-gnu/13/../../../../include
End of search list.
 /after
";
        assert_eq!(
            system_dirs(verbose),
            [
                PathBuf::from("/usr/lib/gcc/x86_64-linux-gnu/13/include"),
                PathBuf::from("/usr/include"),
            ]
        );
    }

    #[test]
    fn headers_reads_each_header_of_a_depfile_with_its_escapes() {
        let depfile = "a\\ b/0.o: a\\ b/a.c a\\ b/a.h \\\n /s/../deps/b.h \
                       /s/c\\#$$.h\\\n/s/d.h\n";
        assert_eq!(
            headers(depfile),
            [
                PathBuf::from("a b/a.h"),
                PathBuf::from("/s/../deps/b.h"),
                PathBuf::from("/s/c#$.h"),
                PathBuf::from("/s/d.h"),
            ]
        );
    }

    #[test]
    fn arguments_splits_a_command_on_whitespace_outside_quotes() {
        let command =
            "/usr/bin/cc  -DA=\"x y\" \"-I/a b/include\" -I/c\\ d\t-o \"\" x.c";
        assert_eq!(
            arguments(command),
            [
                "/usr/bin/cc",
                "-DA=x y",
                "-I/a b/include",
                "-I/c d",
                "-o",
                "",
                "x.c",
            ]
        );
    }

    #[test]
    fn inside_refuses_a_path_that_leaves_the_copy() {
        for path in ["include/a.h", "./src/../src/a.h", "src/../a.h"] {
            assert!(inside(Path::new(path)), "{path}");
        }
        for path in ["/usr/include/a.h", "../a.h", "src/../../a.h", "src/../.."] {
            assert!(!inside(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn relative_keeps_a_source_path_and_moves_a_generated_one() {
        let trees = Trees {
            src: Path::new("/w/src"),
            build: Path::new("/w/build"),
        };
        assert_eq!(
            trees.relative(Path::new("/w/src/src/server/../ua_types.c")),
            Ok(PathBuf::from("src/ua_types.c"))
        );
        assert_eq!(
            trees.relative(Path::new("/w/build/src_generated/open62541/config.h")),
            Ok(PathBuf::from("src_generated/open62541/config.h"))
        );
        assert_eq!(
            trees.relative(Path::new("/w/build/src_generated/")),
            Ok(PathBuf::from("src_generated"))
        );
        assert_eq!(
            trees.relative(Path::new("/w/build/other.h")),
            Err("/w/build/other.h is outside the clone".to_owned())
        );
    }

    #[test]
    fn flags_keeps_each_define_include_and_standard() {
        let trees = Trees {
            src: Path::new("/w/src"),
            build: Path::new("/w/build"),
        };
        let arguments = [
            "/usr/bin/cc",
            "-DUA_X",
            "-I/w/src/include",
            "-I/w/build/src_generated",
            "-I/w/src",
            "-I/w/src/-gen",
            "-std=c99",
            "-fno-strict-aliasing",
            "-O3",
            "-Wall",
            "-Wno-cast-qual",
            "-Wformat=2",
            "-o",
            "x.o",
            "-c",
            "/w/src/-x.c",
        ]
        .map(str::to_owned);
        assert_eq!(
            trees.flags(&arguments),
            Ok([
                "-DUA_X",
                "-Iinclude",
                "-Isrc_generated",
                "-I./",
                "-I./-gen",
                "-std=c99",
                "-fno-strict-aliasing"
            ]
            .map(str::to_owned)
            .to_vec())
        );
        assert_eq!(
            trees.flags(&["cc", "-I/w/build"].map(str::to_owned)),
            Err("/w/build is outside the clone".to_owned())
        );
        for flag in ["-fplugin=x", "-Wl,-z", "x.c", "-include", "-D"] {
            assert_eq!(
                trees.flags(&["cc", flag].map(str::to_owned)),
                Err(format!("`{flag}` is in neither CODE_FLAGS nor LEFT_OUT"))
            );
        }
    }

    #[test]
    fn collect_refuses_a_flag_in_no_list() {
        let dir = temp("collect");
        let (src, build) = (dir.join("src"), dir.join("build"));
        let commands = serde_json::json!([{
            "file": src.join("src/ua_types.c"),
            "output": "o/open62541-object.dir/ua_types.c.o",
            "command": "cc -DA -fplugin=x -o o.o -c x.c",
        }]);
        create_files(&build, &[("compile_commands.json", &commands.to_string())]);
        let trees = Trees {
            src: &src,
            build: &build,
        };
        assert_eq!(
            collect(&trees).map(|found| found.flags),
            Err(
                "src/ua_types.c: `-fplugin=x` is in neither CODE_FLAGS nor LEFT_OUT"
                    .to_owned()
            )
        );
        remove(&dir).unwrap();
    }

    #[test]
    fn entries_keeps_the_library_compiles_only() {
        let commands = r#"[
            {"file": "/w/src/src/ua_types.c", "command": "cc -DA -c x",
             "output": "CMakeFiles/open62541-object.dir/src/ua_types.c.o"},
            {"file": "/w/src/plugins/ua_log_stdout.c", "command": "cc",
             "output": "CMakeFiles/open62541-plugins.dir/plugins/ua_log_stdout.c.o"},
            {"file": "/w/src/tools/x.c", "command": "cc",
             "output": "CMakeFiles/x.dir/tools/x.c.o"}
        ]"#;
        let build = Path::new("/w/build");
        assert_eq!(
            entries(commands, build),
            Ok(vec![
                Entry {
                    source: PathBuf::from("/w/src/src/ua_types.c"),
                    object: build
                        .join("CMakeFiles/open62541-object.dir/src/ua_types.c.o"),
                    arguments: ["cc", "-DA", "-c", "x"].map(str::to_owned).to_vec(),
                },
                Entry {
                    source: PathBuf::from("/w/src/plugins/ua_log_stdout.c"),
                    object: build.join(
                        "CMakeFiles/open62541-plugins.dir/plugins/ua_log_stdout.c.o"
                    ),
                    arguments: vec!["cc".to_owned()],
                },
            ])
        );
        assert_eq!(
            entries(r#"[{"file": "a.c", "output": "a.o"}]"#, build),
            Err("compile_commands entry with no file, output, or command: \
                 {\"file\":\"a.c\",\"output\":\"a.o\"}"
                .to_owned())
        );
    }

    /// A call of `symbol` from `function`.
    fn call(function: &str, symbol: &str) -> Reference {
        labeled(".text", function, Access::Call, symbol)
    }

    /// The address of `symbol` in the section `.data.rel`.
    fn data(symbol: &str) -> Reference {
        unlabeled(".data.rel", Access::Address, symbol)
    }

    /// The error of [`Uses::mismatches`] for a reference that no list admits.
    fn unlisted(file: &str, function: &str, access: Access, symbol: &str) -> String {
        let verb = access.verb();
        format!(
            "{file}: `{function}` {verb} `{symbol}`, which no list admits. Find \
             whether a node runs it; if it reads no clock, file, network, randomness, \
             or process state, add it with the reason, to FILE_SYMBOLS for a file \
             that no node runs, else to FUNCTION_SYMBOLS"
        )
    }

    /// The error of [`Uses::mismatches`] for a clock call that no list admits.
    fn unlisted_clock(file: &str, function: &str) -> String {
        format!(
            "{file}: `{function}` calls `UA_DateTime_now`, a global clock function. \
             Find whether a node runs it; if not, add it to FUNCTION_SYMBOLS with the \
             reason"
        )
    }

    /// A [`Uses`] with each reference that the lists admit, other than those of
    /// `left_out`.
    fn listed(left_out: &[&str]) -> Uses {
        let mut uses = Uses::default();
        for &(file, function, symbol, access) in &FUNCTION_SYMBOLS {
            if !left_out.contains(&function) {
                let found = labeled(".text", function, access, symbol);
                assert_eq!(uses.add(file, found), None);
            }
        }
        for &(file, symbol) in &FILE_SYMBOLS {
            if !left_out.contains(&symbol) {
                assert_eq!(uses.add(file, data(symbol)), None);
            }
        }
        uses
    }

    #[test]
    fn uses_names_a_new_reference_and_a_listed_one_that_is_gone() {
        let mut uses = listed(&[]);
        assert_eq!(uses.add("src/ua_types.c", call("UA_new", "memcpy")), None);
        assert_eq!(uses.add("src/ua_types.c", data("memcpy")), None);
        assert_eq!(uses.mismatches(), Vec::<String>::new());
        let mut uses = listed(&["UA_random_seed", "syslog"]);
        let found = [
            ("deps/parse_num.c", call("UA_parse", "__errno_location")),
            ("plugins/ua_log_stdout.c", call("UA_print", "puts")),
            ("src/ua_types.c", call("UA_new", CLOCKS[0])),
            ("src/ua_types.c", call("UA_open", "socket")),
        ];
        for (file, found) in found {
            assert_eq!(uses.add(file, found), None);
        }
        assert_eq!(
            uses.mismatches(),
            [
                unlisted(
                    "deps/parse_num.c",
                    "UA_parse",
                    Access::Call,
                    "__errno_location"
                ),
                unlisted_clock("src/ua_types.c", "UA_new"),
                unlisted("src/ua_types.c", "UA_open", Access::Call, "socket"),
                "src/util/ua_util.c: `UA_random_seed` no longer calls \
                 `UA_DateTime_now`. Remove it from FUNCTION_SYMBOLS"
                    .to_owned(),
                "plugins/ua_log_syslog.c: no longer references `syslog`. Remove it \
                 from FILE_SYMBOLS"
                    .to_owned(),
            ]
        );
    }

    /// A listed function that takes the address of a symbol that it calls lets other
    /// code call the symbol through that address. `check` needs GCC and GNU binutils,
    /// so this test also runs the rule on macOS.
    #[test]
    fn uses_names_the_address_of_a_symbol_that_its_function_may_only_call() {
        let mut uses = listed(&[]);
        let found = labeled(".text", "parseDouble", Access::Address, "strtod");
        assert_eq!(uses.add("deps/parse_num.c", found), None);
        assert_eq!(
            uses.mismatches(),
            [unlisted(
                "deps/parse_num.c",
                "parseDouble",
                Access::Address,
                "strtod"
            )]
        );
    }

    /// No clock is in [`SYMBOLS`] or [`FILE_SYMBOLS`], so only a key of
    /// [`FUNCTION_SYMBOLS`] admits a clock call.
    #[test]
    fn no_list_but_function_symbols_admits_a_clock() {
        for clock in CLOCKS {
            assert!(!SYMBOLS.contains(&clock));
            assert!(FILE_SYMBOLS.iter().all(|&(_, symbol)| symbol != clock));
        }
    }

    /// The address of a clock lets other code call the clock, and a reference outside
    /// a function has no function to key, so no list bounds its use. `check` needs GCC
    /// and GNU binutils, so this test also runs the rule on macOS.
    #[test]
    fn uses_refuses_a_reference_that_no_function_holds_or_no_call_makes() {
        let mut uses = Uses::default();
        let address = |site: &str| {
            Some(format!(
                "src/ua_types.c: {site} takes the address of `UA_DateTime_now`, so a \
                 call through it escapes FUNCTION_SYMBOLS"
            ))
        };
        let found = labeled(".text", "UA_new", Access::Address, CLOCKS[0]);
        assert_eq!(uses.add("src/ua_types.c", found), address("`UA_new`"));
        assert_eq!(
            uses.add("src/ua_types.c", data(CLOCKS[0])),
            address("the section `.data.rel`")
        );
        assert_eq!(
            uses.add(
                "src/ua_types.c",
                unlabeled(".data.rel.ro", Access::Call, CLOCKS[0])
            ),
            Some(
                "src/ua_types.c: the section `.data.rel.ro` references \
                 `UA_DateTime_now` outside a function, so a use through it escapes \
                 FUNCTION_SYMBOLS"
                    .to_owned()
            )
        );
        assert_eq!(
            uses.add("deps/parse_num.c", data("strtod")),
            Some(
                "deps/parse_num.c: the section `.data.rel` references `strtod` outside \
                 a function, so a use through it escapes FUNCTION_SYMBOLS"
                    .to_owned()
            )
        );
        assert_eq!(uses.add("plugins/ua_log_syslog.c", data("syslog")), None);
        assert_eq!(uses.add("src/ua_types.c", data("memcpy")), None);
    }

    /// A directory of the test `name`, empty, in the temporary directory, with a
    /// space in its path.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("xtask open62541-{name}-{}", std::process::id()));
        remove(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes each (path, text) of `files` under `dir`.
    fn create_files(dir: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    /// Commits each file in `repo` and tags the commit `tag`.
    fn tag(repo: &Path, tag: &str) {
        let git = |args: &[&str]| {
            let mut git = Command::new("git");
            git.arg("-C")
                .arg(repo)
                .args(["-c", "user.name=x", "-c", "user.email=x@x"]);
            exec(git.args(args)).unwrap()
        };
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", tag]);
        git(&["tag", tag]);
    }

    /// A statement that references `symbol`, in a function with an `int n`.
    fn statement(symbol: &str) -> String {
        match symbol {
            "__ctype_b_loc" => "(void)isdigit(n);".to_owned(),
            "__errno_location" => "errno = n;".to_owned(),
            "strtod" => "(void)strtod(\"1\", 0);".to_owned(),
            "abort" => "if (n) abort();".to_owned(),
            "fprintf" | "stderr" => "fprintf(stderr, \"%d\", n);".to_owned(),
            "fflush" | "stdout" => "fflush(stdout);".to_owned(),
            "printf" => "printf(\"%d\", n);".to_owned(),
            "puts" => "puts(\"x\");".to_owned(),
            "syslog" => "syslog(LOG_INFO, \"x\");".to_owned(),
            "access" => "(void)access(\"x\", 0);".to_owned(),
            clock if CLOCKS.contains(&clock) => format!("(void){clock}();"),
            _ => panic!("no statement references {symbol}"),
        }
    }

    /// C text of `file` that references each symbol at its place in
    /// [`FUNCTION_SYMBOLS`], and each that [`FILE_SYMBOLS`] lists for `file` in a
    /// function of its own.
    fn uses(file: &str) -> String {
        let mut functions = std::collections::BTreeMap::<_, Vec<_>>::new();
        for &(f, function, symbol, _) in &FUNCTION_SYMBOLS {
            if f == file {
                functions
                    .entry(function)
                    .or_default()
                    .push(statement(symbol));
            }
        }
        for &(f, symbol) in &FILE_SYMBOLS {
            if f == file {
                functions
                    .entry("UA_use")
                    .or_default()
                    .push(statement(symbol));
            }
        }
        let mut text = "#include <ctype.h>\n#include <errno.h>\n#include <stdio.h>\n\
                        #include <stdlib.h>\n#include <syslog.h>\n#include <unistd.h>\n\
                        #include \"clock.h\"\n"
            .to_owned();
        text.extend(functions.into_iter().map(|(function, statements)| {
            format!("void {function}(int n) {{\n{}\n}}\n", statements.join("\n"))
        }));
        text
    }

    /// A project with the layout of open62541: the `open62541` library from two
    /// object libraries, with a reference to each symbol at its place in
    /// [`FUNCTION_SYMBOLS`] and [`FILE_SYMBOLS`], a generated header, a header that
    /// is not UTF-8, and each file of [`EXTRA`], which the library does not compile.
    fn create_project(repo: &Path) {
        exec(Command::new("git").arg("init").arg("-q").arg(repo)).unwrap();
        let util = "#include \"open62541/config.h\"\n".to_owned()
            + &uses("src/util/ua_util.c");
        create_files(
            repo,
            &[
                ("LICENSE", "license\n"),
                (
                    "CMakeLists.txt",
                    "cmake_minimum_required(VERSION 3.20)\nproject(fixture C)\n\
                     set(CMAKE_C_STANDARD 99)\n\
                     configure_file(config.h.in src_generated/open62541/config.h)\n\
                     file(WRITE ${CMAKE_BINARY_DIR}/other.h \"\")\n\
                     include_directories(include ${CMAKE_BINARY_DIR}/src_generated)\n\
                     add_compile_definitions(NAME=\"a b\")\n\
                     add_compile_options(-fno-strict-aliasing -Wall -pipe -flto=auto \
                     -fno-fat-lto-objects)\n\
                     file(GLOB more src/more/*.c)\n\
                     add_library(open62541-object OBJECT src/util/ua_util.c \
                     src/util/ua_encryptedsecret.c deps/musl_inet_pton.c \
                     deps/parse_num.c src/server/ua_discovery.c \
                     src/server/ua_services_discovery.c ${more})\n\
                     add_library(open62541-plugins OBJECT plugins/ua_config_default.c \
                     plugins/ua_log_stdout.c plugins/ua_log_syslog.c)\n\
                     add_library(open62541 STATIC $<TARGET_OBJECTS:open62541-object> \
                     $<TARGET_OBJECTS:open62541-plugins>)\n\
                     add_executable(tool tools/tool.c)\n",
                ),
                ("config.h.in", "#define CONFIG 1\n"),
                ("src/util/ua_util.c", &util),
                ("tools/tool.c", "int main(void) { return 0; }\n"),
                ("arch/common/timer.c", "#include \"timer.h\"\nint timer;\n"),
                ("arch/common/timer.h", "#include <stdio.h>\n"),
            ],
        );
        for path in [
            "src/util/ua_encryptedsecret.c",
            "plugins/ua_config_default.c",
            "plugins/ua_log_stdout.c",
            "plugins/ua_log_syslog.c",
            "deps/musl_inet_pton.c",
            "deps/parse_num.c",
            "src/server/ua_discovery.c",
            "src/server/ua_services_discovery.c",
        ] {
            create_files(repo, &[(path, &uses(path))]);
        }
        let clock = b"/* Andr\xe9 */\nlong long UA_DateTime_now(void);\n\
                      long long UA_DateTime_nowMonotonic(void);\n\
                      long long UA_DateTime_localTimeUtcOffset(void);\n";
        std::fs::create_dir(repo.join("include")).unwrap();
        std::fs::write(repo.join("include/clock.h"), clock).unwrap();
    }

    /// Runs [`run`] on the tag `v1` of a project from [`create_project`], changed by
    /// `change`, with a file `kept.c` in the copy before it. Gives the root, the
    /// repository, and what [`run`] gave.
    fn run_on(
        name: &str,
        change: impl FnOnce(&Path),
    ) -> (PathBuf, PathBuf, Result<(), Vec<String>>) {
        run_after(name, change, &[("patches/open62541/kept.c", "")])
    }

    /// [`run_on`] with the files `before` under the root, in place of `kept.c`.
    fn run_after(
        name: &str,
        change: impl FnOnce(&Path),
        before: &[(&str, &str)],
    ) -> (PathBuf, PathBuf, Result<(), Vec<String>>) {
        let (root, repo) =
            (temp(&format!("{name}-root")), temp(&format!("{name}-repo")));
        create_project(&repo);
        change(&repo);
        tag(&repo, "v1");
        create_files(&root, before);
        let url = format!("file://{}", repo.display());
        let result = run(&root, &url, "v1");
        (root, repo, result)
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_copies_each_compiled_source_and_its_headers() {
        let (root, repo, result) = run_on("copies", |_| {});
        assert_eq!(result, Ok(()));
        let dest = root.join("patches/open62541");
        let mut copied: Vec<_> = walk(&dest)
            .iter()
            .map(|path| path.strip_prefix(&dest).unwrap().display().to_string())
            .collect();
        copied.sort();
        assert_eq!(
            copied,
            [
                "LICENSE",
                "VERSION",
                "arch/common/timer.c",
                "arch/common/timer.h",
                "deps/musl_inet_pton.c",
                "deps/parse_num.c",
                "flags.txt",
                "include/clock.h",
                "plugins/ua_config_default.c",
                "plugins/ua_log_stdout.c",
                "plugins/ua_log_syslog.c",
                "sources.txt",
                "src/server/ua_discovery.c",
                "src/server/ua_services_discovery.c",
                "src/util/ua_encryptedsecret.c",
                "src/util/ua_util.c",
                "src_generated/open62541/config.h",
            ]
        );
        let read = |path| std::fs::read_to_string(dest.join(path)).unwrap();
        assert_eq!(
            read("sources.txt"),
            "arch/common/timer.c\ndeps/musl_inet_pton.c\ndeps/parse_num.c\n\
             plugins/ua_config_default.c\nplugins/ua_log_stdout.c\n\
             plugins/ua_log_syslog.c\nsrc/server/ua_discovery.c\n\
             src/server/ua_services_discovery.c\nsrc/util/ua_encryptedsecret.c\n\
             src/util/ua_util.c\n"
        );
        assert_eq!(
            read("flags.txt"),
            "-DNAME=\"a b\"\n-Iinclude\n-Isrc_generated\n-DNDEBUG\n-std=gnu99\n\
             -fno-strict-aliasing\n"
        );
        let mut git = Command::new("git");
        let commit = exec(git.arg("-C").arg(&repo).args(["rev-parse", "v1"])).unwrap();
        assert_eq!(read("VERSION"), format!("v1\n{commit}\n"));
        #[expect(clippy::disallowed_methods, reason = "a test reads its files")]
        for path in [
            "src/util/ua_util.c",
            "include/clock.h",
            "arch/common/timer.h",
        ] {
            assert_eq!(
                std::fs::read(dest.join(path)).unwrap(),
                std::fs::read(repo.join(path)).unwrap()
            );
        }
        assert_eq!(check(&root), Ok(()));
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_refuses_a_release_with_no_file_of_extra() {
        let (root, repo, result) = run_on("extra", |repo| {
            std::fs::remove_file(repo.join("arch/common/timer.h")).unwrap();
        });
        assert_eq!(
            result,
            Err(vec![
                "arch/common/timer.h: EXTRA lists it, and the release has no such \
                 file"
                    .to_owned()
            ])
        );
        assert!(root.join("patches/open62541/kept.c").exists());
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_refuses_a_file_of_extra_that_the_library_holds() {
        let (root, repo, result) = run_on("stale", |repo| {
            let cmake = repo.join("CMakeLists.txt");
            let text = std::fs::read_to_string(&cmake).unwrap()
                + "target_sources(open62541-plugins PRIVATE arch/common/timer.c)\n";
            std::fs::write(&cmake, text).unwrap();
        });
        assert_eq!(
            result,
            Err(vec![
                "arch/common/timer.c: EXTRA lists it, and the library already holds it"
                    .to_owned()
            ])
        );
        assert!(root.join("patches/open62541/kept.c").exists());
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_refuses_each_problem_of_the_staged_copy() {
        let (root, repo, result) = run_on("refuses", |repo| {
            create_files(
                repo,
                &[
                    (
                        "plugins/ua_config_default.c",
                        &(uses("plugins/ua_config_default.c")
                            + "long long (*UA_clockFn)(void);\n\
                               void UA_keep(void) { UA_clockFn = UA_DateTime_now; }\n"),
                    ),
                    (
                        "src/more/ua_types.c",
                        "#include \"../../../build/other.h\"\n\
                           #include <sys/stat.h>\n\
                           int UA_stat(const char *path) {\n\
                           struct stat s;\nreturn stat(path, &s);\n}\n\
                           #include \"clock.h\"\n\
                           long long UA_new(void) { return UA_DateTime_now(); }\n",
                    ),
                    (
                        "src/more/ua_clock.c",
                        "#include \"clock.h\"\n\
                         long long (*UA_clock)(void) = UA_DateTime_now;\n",
                    ),
                    (
                        "src/more/ua_text.c",
                        "#include \"clock.h\"\n\
                         long long (*UA_text)(void) \
                         __attribute__((section(\".text_ptr\"))) = UA_DateTime_now;\n",
                    ),
                    ("src/more/ua_gen.c", "#line 1 \"gen.y\"\nint gen;\n"),
                ],
            );
        });
        assert_eq!(
            result,
            Err(vec![
                "src/more/ua_gen.c:1: holds a line directive, which moves the file \
                 that the include check reads"
                    .to_owned(),
                "plugins/ua_config_default.c: `UA_keep` takes the address of \
                 `UA_DateTime_now`, so a call through it escapes FUNCTION_SYMBOLS"
                    .to_owned(),
                "src/more/ua_clock.c: the section `.data.rel` takes the address of \
                 `UA_DateTime_now`, so a call through it escapes FUNCTION_SYMBOLS"
                    .to_owned(),
                "src/more/ua_text.c: the section `.text_ptr` takes the address of \
                 `UA_DateTime_now`, so a call through it escapes FUNCTION_SYMBOLS"
                    .to_owned(),
                "src/more/ua_types.c: includes ./src/more/../../../build/other.h, \
                 which is outside the copy"
                    .to_owned(),
                unlisted_clock("src/more/ua_types.c", "UA_new"),
                unlisted("src/more/ua_types.c", "UA_stat", Access::Call, "stat"),
            ])
        );
        assert!(root.join("patches/open62541/kept.c").exists());
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_names_a_static_helper_that_calls_a_clock() {
        let (root, repo, result) = run_on("helper", |repo| {
            let log = "#include \"clock.h\"\n\
                       static long long helper(void) { return UA_DateTime_now(); }\n"
                .to_owned()
                + &uses("plugins/ua_log_stdout.c")
                    .replace("(void)UA_DateTime_now();", "(void)helper();");
            create_files(repo, &[("plugins/ua_log_stdout.c", &log)]);
        });
        assert_eq!(
            result,
            Err(vec![
                unlisted_clock("plugins/ua_log_stdout.c", "helper"),
                "plugins/ua_log_stdout.c: `UA_Log_Stdout_log` no longer calls \
                 `UA_DateTime_now`. Remove it from FUNCTION_SYMBOLS"
                    .to_owned(),
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_refuses_a_source_with_other_flags() {
        let (root, repo, result) = run_on("flags", |repo| {
            let cmake = repo.join("CMakeLists.txt");
            let text = std::fs::read_to_string(&cmake).unwrap()
                + "set_source_files_properties(plugins/ua_log_stdout.c PROPERTIES \
                   COMPILE_DEFINITIONS LOG=1)\n";
            std::fs::write(&cmake, text).unwrap();
        });
        assert_eq!(
            result,
            Err(vec![
                "plugins/ua_log_stdout.c compiles with other flags".to_owned()
            ])
        );
        assert!(root.join("patches/open62541/kept.c").exists());
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_refuses_a_flag_in_no_list() {
        let (root, repo, result) = run_on("unsorted", |repo| {
            let cmake = repo.join("CMakeLists.txt");
            let text = std::fs::read_to_string(&cmake).unwrap()
                + "set_source_files_properties(plugins/ua_log_stdout.c PROPERTIES \
                   COMPILE_OPTIONS -fwrapv)\n";
            std::fs::write(&cmake, text).unwrap();
        });
        assert_eq!(
            result,
            Err(vec![
                "plugins/ua_log_stdout.c: `-fwrapv` is in neither CODE_FLAGS nor \
                 LEFT_OUT"
                    .to_owned()
            ])
        );
        assert!(root.join("patches/open62541/kept.c").exists());
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_copies_a_source_whose_name_starts_with_a_dash() {
        let (root, repo, result) = run_on("dash-source", |repo| {
            create_files(repo, &[("-gen.c", "int gen;\n")]);
            let cmake = repo.join("CMakeLists.txt");
            let text = std::fs::read_to_string(&cmake).unwrap()
                + "target_sources(open62541-object PRIVATE -gen.c)\n";
            std::fs::write(&cmake, text).unwrap();
        });
        assert_eq!(result, Ok(()));
        assert_eq!(check(&root), Ok(()));
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_names_a_clock_call_in_a_source_whose_name_starts_with_an_at_sign() {
        let (root, repo, result) = run_on("at-source", |repo| {
            let text = "long long UA_DateTime_now(void);\n\
                        long long UA_gen(void) { return UA_DateTime_now(); }\n";
            create_files(repo, &[("@gen.c", text), ("gen.c", "int gen;\n")]);
            let cmake = repo.join("CMakeLists.txt");
            let text = std::fs::read_to_string(&cmake).unwrap()
                + "target_sources(open62541-object PRIVATE @gen.c gen.c)\n";
            std::fs::write(&cmake, text).unwrap();
        });
        assert_eq!(result, Err(vec![unlisted_clock("@gen.c", "UA_gen")]));
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn run_keeps_the_copy_when_a_file_is_missing() {
        let (root, repo, result) = run_on("license", |repo| {
            std::fs::remove_file(repo.join("LICENSE")).unwrap();
        });
        let license = root.join("target/open62541/src/LICENSE");
        assert_eq!(
            result,
            Err(vec![format!(
                "{}: No such file or directory (os error 2)",
                license.display()
            )])
        );
        assert!(root.join("patches/open62541/kept.c").exists());
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_finds_a_clock_call_added_to_the_copy() {
        let (root, repo, result) = run_after("check", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let copy = root.join("patches/open62541");
        let append = |path: &str, text: &str| {
            let old = std::fs::read_to_string(copy.join(path)).unwrap();
            std::fs::write(copy.join(path), old + text).unwrap();
        };
        let flags = std::fs::read_to_string(copy.join("flags.txt")).unwrap();
        append(
            "flags.txt",
            "-O2\n-include\nsys/stat.h\n-I-\n-D\n-save-temps\n-pthread\n",
        );
        let refused = |flag: &str| {
            format!(
                "flags.txt: `{flag}` is not a -D, -I, or -std flag with its value, or \
                 one of CODE_FLAGS"
            )
        };
        assert_eq!(
            check(&root),
            Err([
                "-O2",
                "-include",
                "sys/stat.h",
                "-I-",
                "-D",
                "-save-temps",
                "-pthread"
            ]
            .map(refused)
            .to_vec())
        );
        std::fs::write(copy.join("flags.txt"), flags).unwrap();
        append(
            "src/util/ua_util.c",
            "static long long hidden(void) { return UA_DateTime_now(); }\n\
             long long later(void) { return hidden(); }\n",
        );
        assert_eq!(
            check(&root),
            Err(vec![unlisted_clock("src/util/ua_util.c", "hidden")])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_finds_an_os_call_through_a_header_that_includes_its_own() {
        let (root, repo, result) = run_after("os", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let copy = root.join("patches/open62541");
        let append = |path: &str, text: &str| {
            let old = std::fs::read_to_string(copy.join(path)).unwrap();
            std::fs::write(copy.join(path), old + text).unwrap();
        };
        append(
            "src/util/ua_encryptedsecret.c",
            "#include <pthread.h>\nlong long UA_time(void) { return time(0); }\n",
        );
        append(
            "src/util/ua_util.c",
            "#include <pthread.h>\n\
             int UA_lock(pthread_mutex_t *m) { return pthread_mutex_lock(m); }\n",
        );
        assert_eq!(
            check(&root),
            Err(vec![
                unlisted(
                    "src/util/ua_encryptedsecret.c",
                    "UA_time",
                    Access::Call,
                    "time"
                ),
                unlisted(
                    "src/util/ua_util.c",
                    "UA_lock",
                    Access::Call,
                    "pthread_mutex_lock"
                ),
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_finds_an_os_call_whose_name_another_file_defines_as_static() {
        let (root, repo, result) = run_after("static", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let copy = root.join("patches/open62541");
        let append = |path: &str, text: &str| {
            let old = std::fs::read_to_string(copy.join(path)).unwrap();
            std::fs::write(copy.join(path), old + text).unwrap();
        };
        append(
            "src/util/ua_encryptedsecret.c",
            "static int socket(void) { return 0; }\n\
             int UA_open(void) { return socket(); }\n",
        );
        append(
            "src/util/ua_util.c",
            "#include <sys/socket.h>\n\
             int UA_connect(void) { return socket(2, 1, 0); }\n",
        );
        assert_eq!(
            check(&root),
            Err(vec![unlisted(
                "src/util/ua_util.c",
                "UA_connect",
                Access::Call,
                "socket"
            )])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_finds_a_read_of_errno_that_no_call_of_the_copy_set() {
        // `errno` holds the error of the last OS call of the thread, also one that
        // code outside the copy made.
        let (root, repo, result) = run_after("errno", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let path = root.join("patches/open62541/src/util/ua_encryptedsecret.c");
        let old = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            old + "#include <errno.h>\nint UA_lastError(void) { return errno; }\n",
        )
        .unwrap();
        assert_eq!(
            check(&root),
            Err(vec![unlisted(
                "src/util/ua_encryptedsecret.c",
                "UA_lastError",
                Access::Call,
                "__errno_location"
            )])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_finds_a_listed_symbol_at_a_place_that_the_list_does_not_name() {
        let (root, repo, result) = run_after("place", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let copy = root.join("patches/open62541");
        let append = |path: &str, text: &str| {
            let old = std::fs::read_to_string(copy.join(path)).unwrap();
            std::fs::write(copy.join(path), old + text).unwrap();
        };
        append(
            "deps/parse_num.c",
            "int UA_lastError(void) { return errno; }\n",
        );
        append("src/util/ua_util.c", "void (*UA_stop)(void) = abort;\n");
        assert_eq!(
            check(&root),
            Err(vec![
                "src/util/ua_util.c: the section `.data.rel` references `abort` \
                 outside a function, so a use through it escapes FUNCTION_SYMBOLS"
                    .to_owned(),
                unlisted(
                    "deps/parse_num.c",
                    "UA_lastError",
                    Access::Call,
                    "__errno_location"
                ),
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_finds_a_listed_symbol_in_a_nested_function_of_a_new_function() {
        let (root, repo, result) = run_after("nested", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let copy = root.join("patches/open62541");
        let append = |path: &str, text: &str| {
            let old = std::fs::read_to_string(copy.join(path)).unwrap();
            std::fs::write(copy.join(path), old + text).unwrap();
        };
        append(
            "deps/parse_num.c",
            "int UA_lastError(void) {\n\
             int parseDouble(void) { return errno; }\n\
             return parseDouble();\n}\n",
        );
        append(
            "src/util/ua_util.c",
            "void UA_report(int n) {\n\
             void UA_rng_require(void) { fprintf(stderr, \"%d\", n); }\n\
             UA_rng_require();\n}\n",
        );
        assert_eq!(
            check(&root),
            Err(vec![
                unlisted(
                    "deps/parse_num.c",
                    "parseDouble.0",
                    Access::Call,
                    "__errno_location"
                ),
                unlisted(
                    "src/util/ua_util.c",
                    "UA_rng_require.0",
                    Access::Call,
                    "fprintf"
                ),
                unlisted(
                    "src/util/ua_util.c",
                    "UA_rng_require.0",
                    Access::Address,
                    "stderr"
                ),
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    /// A listed function that stores the address of a symbol that it may only call
    /// lets a new function call the symbol with no reference to it.
    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_finds_the_address_of_a_listed_symbol_in_its_function() {
        let (root, repo, result) = run_after("address", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let copy = root.join("patches/open62541");
        let replace = |path: &str, from: &str, to: &str| {
            let old = std::fs::read_to_string(copy.join(path)).unwrap();
            assert_eq!(old.matches(from).count(), 1, "{path}: {from}");
            std::fs::write(copy.join(path), old.replace(from, to)).unwrap();
        };
        replace(
            "deps/parse_num.c",
            "void parseDouble(",
            "double (*UA_conv)(const char *, char **);\n\
             double UA_parseOther(const char *s) { return UA_conv(s, 0); }\n\
             void parseDouble(",
        );
        replace(
            "deps/parse_num.c",
            "(void)strtod(",
            "UA_conv = strtod;\n(void)strtod(",
        );
        assert_eq!(
            check(&root),
            Err(vec![unlisted(
                "deps/parse_num.c",
                "parseDouble",
                Access::Address,
                "strtod"
            )])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_passes_an_undefined_symbol_that_no_relocation_names() {
        // As the assembler of x86-64 makes `_GLOBAL_OFFSET_TABLE_` for each object
        // that reads the table.
        let (root, repo, result) = run_after("unnamed", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let path = root.join("patches/open62541/src/util/ua_encryptedsecret.c");
        let old = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, old + "__asm__(\".globl UA_outside\");\n").unwrap();
        assert_eq!(check(&root), Ok(()));
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_finds_a_read_of_the_table_of_the_linker_that_the_c_makes() {
        // Its entries are addresses of imported functions, so a call through one
        // names no symbol.
        let (root, repo, result) = run_after("got", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let path = root.join("patches/open62541/src/util/ua_encryptedsecret.c");
        let old = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            old + "extern long long (*_GLOBAL_OFFSET_TABLE_[])(void);\n\
                   long long UA_now(int i) { return _GLOBAL_OFFSET_TABLE_[i](); }\n",
        )
        .unwrap();
        assert_eq!(
            check(&root),
            Err(vec![unlisted(
                "src/util/ua_encryptedsecret.c",
                "UA_now",
                Access::Address,
                "_GLOBAL_OFFSET_TABLE_"
            )])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_finds_a_clock_call_that_a_listed_function_inlines() {
        let (root, repo, result) = run_after("inline", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        std::fs::write(
            root.join("patches/open62541/plugins/ua_log_stdout.c"),
            "#include \"clock.h\"\n\
             static inline __attribute__((always_inline)) long long helper(void) {\n\
             return UA_DateTime_now();\n}\n"
                .to_owned()
                + &uses("plugins/ua_log_stdout.c")
                    .replace("(void)UA_DateTime_now();", "(void)helper();"),
        )
        .unwrap();
        assert_eq!(
            check(&root),
            Err(vec![
                "plugins/ua_log_stdout.c: inlines `helper`, so a clock call in it \
                 hides in its caller"
                    .to_owned(),
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "needs GCC, GNU objdump, and GNU nm"
    )]
    fn check_passes_on_the_committed_copy() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        // A root of its own: `cargo xtask open62541 <tag>` removes `target/open62541/`.
        let root = temp("committed");
        std::os::unix::fs::symlink(workspace.join("patches"), root.join("patches"))
            .unwrap();
        assert_eq!(check(&root), Ok(()));
        remove(&root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC")]
    fn each_thread_of_the_committed_copy_draws_from_its_own_random_state_or_aborts() {
        use std::os::unix::process::ExitStatusExt;
        /// The signal number of `SIGABRT` on Linux.
        const SIGABRT: i32 = 6;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let (copy, out) = (root.join(DEST), temp("rng"));
        let read = |name| std::fs::read_to_string(copy.join(name)).unwrap();
        let (sources, flags) = (read("sources.txt"), read("flags.txt"));
        let objects = build(&copy, &sources, &flags, &out, Path::new("cc")).unwrap();
        let library = out.join("libopen62541.a");
        let mut ar = Command::new("ar");
        ar.arg("rcs").arg(&library);
        exec(ar.args(objects.iter().map(|(_, object, _)| object))).unwrap();
        let driver = out.join("rng");
        let mut cc = Command::new("cc");
        cc.current_dir(&copy)
            .args(flags.lines())
            .arg("-pthread")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/open62541/rng.c"))
            .arg(&library)
            .arg("-o")
            .arg(&driver);
        exec(&mut cc).unwrap();
        let from_1 = "3795398737 17903413 3545275701 194195274 2326030198 2354257974 \
                      2697798104 3102124240";
        for draw in ["UA_UInt32_random", "UA_Guid_random"] {
            let output = Command::new(&driver).arg(draw).output().unwrap();
            assert_eq!(
                (
                    String::from_utf8(output.stdout).unwrap(),
                    String::from_utf8(output.stderr).unwrap(),
                    output.status.signal(),
                ),
                (
                    format!("after another thread: {from_1}\nalone: {from_1}\n"),
                    format!("{draw}: no start value on this thread\n"),
                    Some(SIGABRT),
                )
            );
        }
        remove(&out).unwrap();
    }

    #[test]
    fn remove_fails_on_an_error_other_than_a_missing_directory() {
        let dir = temp("remove");
        std::fs::write(dir.join("file"), "").unwrap();
        let path = dir.join("file/child");
        assert_eq!(remove(&dir.join("missing")), Ok(()));
        assert_eq!(
            remove(&path),
            Err(format!("{}: Not a directory (os error 20)", path.display()))
        );
        remove(&dir).unwrap();
    }

    /// Each file under `dir`.
    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(walk(&path));
            } else {
                found.push(path);
            }
        }
        found
    }
}
