//! Copies a release of open62541 into `patches/open62541/`, and checks a copy: each
//! C file that our options compile and each header that it includes.

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
    "-DUA_MULTITHREADING=100",
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

/// The only (file, function) pairs that may call a function of [`CLOCKS`].
const CLOCK_CALLS: [(&str, &str); 5] = [
    // The build date of a server config, for the test server only.
    ("plugins/ua_config_default.c", "setDefaultConfig"),
    // `UA_Server_runUntilInterrupt`, which we never call.
    ("plugins/ua_config_default.c", "interruptServer"),
    // The stdout logger, which we replace with our own.
    ("plugins/ua_log_stdout.c", "UA_Log_Stdout_log"),
    // ECC user tokens, which need encryption, which is off.
    (
        "src/util/ua_encryptedsecret.c",
        "encryptUserIdentityTokenEcc",
    ),
    // The seed, which `UA_ENABLE_DETERMINISTIC_RNG` keeps from the clock.
    ("src/util/ua_util.c", "UA_random_seed"),
];

/// The system headers that a file of the copy may include: the C standard library and
/// the POSIX headers of the plugins. A header that one of them includes is not checked.
const SYSTEM_HEADERS: [&str; 18] = [
    "ctype.h",
    "errno.h",
    "float.h",
    "inttypes.h",
    "limits.h",
    "pthread.h",
    "signal.h",
    "stdarg.h",
    "stdatomic.h",
    "stdbool.h",
    "stddef.h",
    "stdint.h",
    "stdio.h",
    "stdlib.h",
    "string.h",
    "sys/socket.h",
    "syslog.h",
    "unistd.h",
];

/// Clones `tag` of `url` into `target/open62541/`, builds it with [`OPTIONS`], and
/// replaces `patches/open62541/` with its compiled sources, the headers in the clone
/// that they include, `LICENSE`, `sources.txt` (each `.c` file), `flags.txt` (the
/// `-D`, `-I`, and `-std` flags of each compile), and `VERSION` (tag and commit).
/// Then it gives what [`check`] gives for the new copy. Needs Linux, `git`, `cmake`,
/// Python 3, a C compiler `cc`, and GNU `objdump`.
///
/// # Errors
///
/// A step that fails, and each error of [`check`]. On an error,
/// `patches/open62541/` does not change.
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
    inspect(&stage, &work.join("check"))?;
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
/// with its `flags.txt` and then `-g -O0`, so a call stays in the function that holds
/// it in the source, except in a function that the compiler inlines.
///
/// # Errors
///
/// A line of `flags.txt` other than a `-D`, `-I`, or `-std` flag with its value, a
/// build that fails, an `#include` or `#import` of a header outside the copy other
/// than one of [`SYSTEM_HEADERS`] in a system directory, a call of a clock function
/// from a pair that [`CLOCK_CALLS`] does not list, a listed pair with no call, any
/// other reference to a clock function, such as its address in code or data,
/// through which any code can call it, each inlined function, an `#include_next`,
/// and a `#line` directive or line marker in a `.c` or `.h` file of the copy.
pub(crate) fn check(root: &Path) -> Result<(), Vec<String>> {
    inspect(&root.join(DEST), &root.join("target/open62541/check"))
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
    /// Each (from, to): each source of the library and each header in the clone that
    /// it includes. A header outside the clone is left out, and [`check`] fails on
    /// one that a source needs.
    files: BTreeSet<(PathBuf, PathBuf)>,
    /// The `-D`, `-I`, and `-std` flags of each compile, with each `-I` relative to
    /// the copy.
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
        let these = trees.flags(&entry.arguments)?;
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

    /// The `-D`, `-I`, and `-std` flags of a compile, with each `-I` relative to the
    /// copy.
    fn flags(&self, arguments: &[String]) -> Result<Vec<String>, String> {
        let mut flags = Vec::new();
        for argument in arguments {
            if let Some(dir) = argument.strip_prefix("-I") {
                let dir = self.relative(Path::new(dir))?;
                // A bare `-I` takes the next flag as its directory; `-I-` is a flag.
                let dir = if dir.as_os_str().is_empty()
                    || dir.as_os_str().as_encoded_bytes().starts_with(b"-")
                {
                    Path::new(".").join(dir)
                } else {
                    dir
                };
                flags.push(format!("-I{}", dir.display()));
            } else if argument.starts_with("-D") || argument.starts_with("-std=") {
                flags.push(argument.clone());
            }
        }
        Ok(flags)
    }
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

/// Builds the copy in `copy` into `out`, and gives each error of [`check`].
fn inspect(copy: &Path, out: &Path) -> Result<(), Vec<String>> {
    let read = |name: &str| {
        std::fs::read_to_string(copy.join(name))
            .map_err(|e| vec![format!("{}: {e}", copy.join(name).display())])
    };
    let (sources, flags) = (read("sources.txt")?, read("flags.txt")?);
    let other: Vec<String> = flags
        .lines()
        // A value that starts with `-` makes a flag such as `-I-`, which changes how
        // `cc` finds a header.
        .filter(|flag| {
            !["-D", "-I", "-std="].iter().any(|p| {
                flag.strip_prefix(p)
                    .is_some_and(|value| !value.is_empty() && !value.starts_with('-'))
            })
        })
        .map(|flag| {
            format!("flags.txt: `{flag}` is not a -D, -I, or -std flag with its value")
        })
        .collect();
    if !other.is_empty() {
        return Err(other);
    }
    remove(out)
        .and_then(|()| std::fs::create_dir_all(out).map_err(|e| format!("{e}")))
        .map_err(|e| vec![e])?;
    let mut cc = Command::new("cc");
    let verbose = spawn(cc.args(["-xc", "-E", "-v", "/dev/null"])).and_then(wait);
    let dirs = system_dirs(&verbose.map_err(|e| vec![e])?.1);
    let objects = build(copy, &sources, &flags, out).map_err(|e| vec![e])?;
    let mut calls = BTreeSet::new();
    let mut problems = line_directives(copy, Path::new("")).map_err(|e| vec![e])?;
    for (source, object, preprocessed) in objects {
        let disassembly = exec(Command::new("objdump").arg("-dr").arg(&object))
            .map_err(|e| vec![e])?;
        for function in clock_calls(&disassembly) {
            calls.insert((source.to_owned(), function));
        }
        let relocations = exec(Command::new("objdump").arg("-r").arg(&object));
        let relocations = relocations.map_err(|e| vec![e])?;
        for (section, symbol) in clock_addresses(&relocations, &disassembly) {
            problems.push(format!(
                "{source}: the section `{section}` takes the address of `{symbol}`, \
                 so a call through it escapes CLOCK_CALLS"
            ));
        }
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
    problems.extend(mismatches(&calls));
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
/// the copy, as `preprocessed` (the output
/// of `cc -E -dI` in `copy`) shows it, that finds a header outside the copy, other
/// than one of [`SYSTEM_HEADERS`] in one of the system directories `dirs`. It finds
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
            Some((_, true)) if SYSTEM_HEADERS.contains(&name) => continue,
            Some((_, true)) => format!(
                "includes the system header `{name}`, which SYSTEM_HEADERS does not \
                 list"
            ),
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

/// Compiles and preprocesses each source of `sources` in `copy` into `out` at once,
/// and gives each (source, object, the output of `cc -E -dI`).
fn build<'a>(
    copy: &Path,
    sources: &'a str,
    flags: &str,
    out: &Path,
) -> Result<Vec<(&'a str, PathBuf, String)>, String> {
    let mut children = Vec::new();
    for (index, source) in sources.lines().enumerate() {
        let object = out.join(format!("{index}.o"));
        let cc = |mode: &[&str]| {
            let mut cc = Command::new("cc");
            // `./` keeps a source such as `-x.c` or `@x.c` from being an option. Else
            // GCC gives cc1 the base name as `-dumpbase`, which cc1 reads as a
            // response file when it starts with `@`.
            cc.current_dir(copy)
                .args(flags.lines())
                .args(["-g", "-O0", "-dumpbase", &index.to_string()])
                .args(mode);
            spawn(cc.arg(Path::new(".").join(source)))
        };
        let compile = cc(&["-c", "-o", &object.to_string_lossy()])?;
        children.push((source, object, compile, cc(&["-E", "-dI"])?));
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

/// The functions in the output of `objdump -dr` that call a function of [`CLOCKS`].
/// A name loses the suffix of a compiler clone, such as `.isra.0`.
fn clock_calls(text: &str) -> BTreeSet<String> {
    let mut calls = BTreeSet::new();
    let mut function = "";
    for line in text.lines() {
        if let Some(name) = line.strip_suffix(">:").and_then(|l| l.split_once(" <")) {
            function = name.1.split('.').next().unwrap_or(name.1);
        } else if let Some((kind, _)) = clock(line)
            && CALLS.contains(&kind)
        {
            calls.insert(function.to_owned());
        }
    }
    calls
}

/// Each (section, clock function) in `relocations`, the output of `objdump -r`,
/// where the section takes the address of a function of [`CLOCKS`]: any reference
/// other than a call from a section that `disassembly`, the output of `objdump -dr`,
/// shows. Each pair comes once, also when one address takes two relocations, as
/// `adrp` and `add` do on 64-bit Arm.
fn clock_addresses(
    relocations: &str,
    disassembly: &str,
) -> Vec<(String, &'static str)> {
    let code: BTreeSet<&str> = disassembly
        .lines()
        .filter_map(|line| line.strip_prefix("Disassembly of section "))
        .map(|name| name.trim_end_matches(':'))
        .collect();
    let mut found = Vec::new();
    let mut section = "";
    for line in relocations.lines() {
        if let Some(name) = line.strip_prefix("RELOCATION RECORDS FOR [") {
            section = name.trim_end_matches("]:");
        } else if let Some((kind, symbol)) = clock(line)
            && !(CALLS.contains(&kind) && code.contains(section))
            && !found.contains(&(section.to_owned(), symbol))
        {
            found.push((section.to_owned(), symbol));
        }
    }
    found
}

/// The type and the clock function of a relocation line that refers to one.
fn clock(line: &str) -> Option<(&str, &'static str)> {
    let mut words = line.split_whitespace();
    let kind = words.find(|word| word.starts_with("R_"))?;
    let symbol = words.next_back()?.split(['+', '-']).next()?;
    CLOCKS
        .into_iter()
        .find(|&clock| clock == symbol)
        .map(|clock| (kind, clock))
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

/// An error for each call in `found` that [`CLOCK_CALLS`] does not list, and for each
/// listed call that `found` does not hold.
fn mismatches(found: &BTreeSet<(String, String)>) -> Vec<String> {
    let listed: BTreeSet<(String, String)> = CLOCK_CALLS
        .iter()
        .map(|&(file, function)| (file.to_owned(), function.to_owned()))
        .collect();
    let new = found.difference(&listed).map(|(file, function)| {
        format!(
            "{file}: `{function}` calls a global clock function. Find whether a node \
             runs it; if not, add it to CLOCK_CALLS with the reason"
        )
    });
    let gone = listed.difference(found).map(|(file, function)| {
        format!(
            "{file}: `{function}` no longer calls a clock. Remove it from CLOCK_CALLS"
        )
    });
    new.chain(gone).collect()
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

    #[test]
    fn clock_calls_names_each_function_that_refers_to_a_clock() {
        let text = "\
Disassembly of section .text.setDefaultConfig:

0000000000000000 <setDefaultConfig>:
   0:\tpush   %rbx
\t\t\t1: R_X86_64_PLT32\tUA_DateTime_now-0x4
0000000000000040 <other>:
\t\t\t40: R_X86_64_REX_GOTPCRELX\tUA_DateTime_now-0x4
\t\t\t41: R_X86_64_PLT32\tUA_DateTime_nowMore-0x4
\t\t\t42: R_X86_64_PC32\tUA_DateTime_now_x+0x4
0000000000000080 <seed.isra.0>:
\t\t\t81: R_AARCH64_CALL26\tUA_DateTime_nowMonotonic
00000000000000c0 <log>:
\t\t\tc1: R_X86_64_PLT32\tUA_DateTime_localTimeUtcOffset-0x4
";
        let calls: Vec<_> = clock_calls(text).into_iter().collect();
        assert_eq!(calls, ["log", "seed", "setDefaultConfig"]);
    }

    #[test]
    fn clock_addresses_names_each_reference_other_than_a_call() {
        let text = "\
RELOCATION RECORDS FOR [.text]:
OFFSET           TYPE              VALUE
0000000000000005 R_X86_64_PLT32    UA_DateTime_now-0x0000000000000004
0000000000000009 R_X86_64_REX_GOTPCRELX  UA_DateTime_now-0x0000000000000004

RELOCATION RECORDS FOR [.text.log]:
0000000000000005 R_AARCH64_CALL26  UA_DateTime_nowMonotonic
0000000000000009 R_AARCH64_JUMP26  UA_DateTime_nowMonotonic

RELOCATION RECORDS FOR [.data.rel]:
OFFSET           TYPE              VALUE
0000000000000000 R_X86_64_64       UA_DateTime_now
0000000000000008 R_X86_64_64       UA_DateTime_nowMore

RELOCATION RECORDS FOR [.rodata]:
0000000000000000 R_AARCH64_ABS64   UA_DateTime_localTimeUtcOffset+0x8

RELOCATION RECORDS FOR [.data.rel.ro]:
0000000000000000 R_X86_64_PLT32    UA_DateTime_now
";
        let disassembly = "\
Disassembly of section .text:
Disassembly of section .text.log:
";
        assert_eq!(
            clock_addresses(text, disassembly),
            [
                (".text".to_owned(), "UA_DateTime_now"),
                (".data.rel".to_owned(), "UA_DateTime_now"),
                (".rodata".to_owned(), "UA_DateTime_localTimeUtcOffset"),
                (".data.rel.ro".to_owned(), "UA_DateTime_now"),
            ]
        );
    }

    #[test]
    fn clock_addresses_names_an_address_in_two_relocations_once() {
        let text = "\
RELOCATION RECORDS FOR [.text]:
OFFSET           TYPE              VALUE
0000000000000008 R_AARCH64_ADR_PREL_PG_HI21  UA_DateTime_now
000000000000000c R_AARCH64_ADD_ABS_LO12_NC  UA_DateTime_now
0000000000000010 R_AARCH64_ADR_GOT_PAGE  UA_DateTime_nowMonotonic
0000000000000014 R_AARCH64_LD64_GOT_LO12_NC  UA_DateTime_nowMonotonic
";
        assert_eq!(
            clock_addresses(text, "Disassembly of section .text:\n"),
            [
                (".text".to_owned(), "UA_DateTime_now"),
                (".text".to_owned(), "UA_DateTime_nowMonotonic"),
            ]
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
#import <time.h>
#include_next <stdio.h>
#includes <time.h>
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
        let unlisted = |name| {
            format!(
                "includes the system header `{name}`, which SYSTEM_HEADERS does not \
                 list"
            )
        };
        assert_eq!(
            includes(&preprocessed, &root.join("copy"), flags, &dirs),
            [
                "includes <local.h>, which no include directory holds".to_owned(),
                "includes src/../../out.h, which is outside the copy".to_owned(),
                unlisted("time.h"),
                unlisted("sys/time.h"),
                unlisted("openssl/ssl.h"),
                "includes UA_X, which is not a file name".to_owned(),
                unlisted("time.h"),
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
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
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
            "-O3",
            "-o",
            "x.o",
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
                "-std=c99"
            ]
            .map(str::to_owned)
            .to_vec())
        );
        assert_eq!(
            trees.flags(&["-I/w/build".to_owned()]),
            Err("/w/build is outside the clone".to_owned())
        );
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

    #[test]
    fn mismatches_names_a_new_call_and_a_listed_call_that_is_gone() {
        let mut found: BTreeSet<(String, String)> = CLOCK_CALLS
            .iter()
            .map(|&(file, function)| (file.to_owned(), function.to_owned()))
            .collect();
        assert_eq!(mismatches(&found), Vec::<String>::new());
        found.remove(&("src/util/ua_util.c".to_owned(), "UA_random_seed".to_owned()));
        found.insert(("src/ua_types.c".to_owned(), "UA_new".to_owned()));
        assert_eq!(
            mismatches(&found),
            [
                "src/ua_types.c: `UA_new` calls a global clock function. Find whether \
                 a node runs it; if not, add it to CLOCK_CALLS with the reason",
                "src/util/ua_util.c: `UA_random_seed` no longer calls a clock. Remove \
                 it from CLOCK_CALLS",
            ]
        );
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

    /// C text that includes `clock.h` and a system header, with each function of
    /// `functions` calling a clock function.
    fn calls(functions: &[&str]) -> String {
        let mut text = "#include <stdio.h>\n#include \"clock.h\"\n".to_owned();
        for function in functions {
            text = text
                + "long long "
                + function
                + "(void) { return UA_DateTime_now(); }\n";
        }
        text
    }

    /// A project with the layout of open62541: the `open62541` library from two
    /// object libraries, with a call of a clock function at each place that
    /// [`CLOCK_CALLS`] lists, a generated header, and a header that is not UTF-8.
    fn create_project(repo: &Path) {
        exec(Command::new("git").arg("init").arg("-q").arg(repo)).unwrap();
        let util = "#include \"open62541/config.h\"\n".to_owned()
            + &calls(&["UA_random_seed"]);
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
                     file(GLOB more src/more/*.c)\n\
                     add_library(open62541-object OBJECT src/util/ua_util.c \
                     src/util/ua_encryptedsecret.c ${more})\n\
                     add_library(open62541-plugins OBJECT plugins/ua_config_default.c \
                     plugins/ua_log_stdout.c)\n\
                     add_library(open62541 STATIC $<TARGET_OBJECTS:open62541-object> \
                     $<TARGET_OBJECTS:open62541-plugins>)\n\
                     add_executable(tool tools/tool.c)\n",
                ),
                ("config.h.in", "#define CONFIG 1\n"),
                ("src/util/ua_util.c", &util),
                (
                    "src/util/ua_encryptedsecret.c",
                    &calls(&["encryptUserIdentityTokenEcc"]),
                ),
                (
                    "plugins/ua_config_default.c",
                    &calls(&["setDefaultConfig", "interruptServer"]),
                ),
                ("plugins/ua_log_stdout.c", &calls(&["UA_Log_Stdout_log"])),
                ("tools/tool.c", "int main(void) { return 0; }\n"),
            ],
        );
        let clock = b"/* Andr\xe9 */\nlong long UA_DateTime_now(void);\n";
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
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
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
                "flags.txt",
                "include/clock.h",
                "plugins/ua_config_default.c",
                "plugins/ua_log_stdout.c",
                "sources.txt",
                "src/util/ua_encryptedsecret.c",
                "src/util/ua_util.c",
                "src_generated/open62541/config.h",
            ]
        );
        let read = |path| std::fs::read_to_string(dest.join(path)).unwrap();
        assert_eq!(
            read("sources.txt"),
            "plugins/ua_config_default.c\nplugins/ua_log_stdout.c\n\
             src/util/ua_encryptedsecret.c\nsrc/util/ua_util.c\n"
        );
        assert_eq!(
            read("flags.txt"),
            "-DNAME=\"a b\"\n-Iinclude\n-Isrc_generated\n-DNDEBUG\n-std=gnu99\n"
        );
        let mut git = Command::new("git");
        let commit = exec(git.arg("-C").arg(&repo).args(["rev-parse", "v1"])).unwrap();
        assert_eq!(read("VERSION"), format!("v1\n{commit}\n"));
        #[expect(clippy::disallowed_methods, reason = "a test reads its files")]
        for path in ["src/util/ua_util.c", "include/clock.h"] {
            assert_eq!(
                std::fs::read(dest.join(path)).unwrap(),
                std::fs::read(repo.join(path)).unwrap()
            );
        }
        assert_eq!(check(&root), Ok(()));
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
    fn run_refuses_each_problem_of_the_staged_copy() {
        let (root, repo, result) = run_on("refuses", |repo| {
            create_files(
                repo,
                &[
                    (
                        "plugins/ua_config_default.c",
                        "#include \"clock.h\"\nlong long (*UA_clockFn)(void);\n\
                         long long setDefaultConfig(void) {\n\
                         UA_clockFn = UA_DateTime_now;\n\
                         return UA_DateTime_now();\n}\n\
                         long long interruptServer(void) {\n\
                         return UA_DateTime_now();\n}\n",
                    ),
                    (
                        "src/more/ua_types.c",
                        &("#include \"../../../build/other.h\"\n\
                           #include <sys/stat.h>\n"
                            .to_owned()
                            + &calls(&["UA_new"])),
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
                "plugins/ua_config_default.c: the section `.text` takes the address of \
                 `UA_DateTime_now`, so a call through it escapes CLOCK_CALLS"
                    .to_owned(),
                "src/more/ua_clock.c: the section `.data.rel` takes the address of \
                 `UA_DateTime_now`, so a call through it escapes CLOCK_CALLS"
                    .to_owned(),
                "src/more/ua_text.c: the section `.text_ptr` takes the address of \
                 `UA_DateTime_now`, so a call through it escapes CLOCK_CALLS"
                    .to_owned(),
                "src/more/ua_types.c: includes ./src/more/../../../build/other.h, \
                 which is outside the copy"
                    .to_owned(),
                "src/more/ua_types.c: includes the system header `sys/stat.h`, which \
                 SYSTEM_HEADERS does not list"
                    .to_owned(),
                "src/more/ua_types.c: `UA_new` calls a global clock function. Find \
                 whether a node runs it; if not, add it to CLOCK_CALLS with the reason"
                    .to_owned(),
            ])
        );
        assert!(root.join("patches/open62541/kept.c").exists());
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
    fn run_names_a_static_helper_that_calls_a_clock() {
        let (root, repo, result) = run_on("helper", |repo| {
            let log = "#include \"clock.h\"\n\
                       static long long helper(void) { return UA_DateTime_now(); }\n\
                       long long UA_Log_Stdout_log(void) { return helper(); }\n";
            create_files(repo, &[("plugins/ua_log_stdout.c", log)]);
        });
        assert_eq!(
            result,
            Err(vec![
                "plugins/ua_log_stdout.c: `helper` calls a global clock function. Find \
                 whether a node runs it; if not, add it to CLOCK_CALLS with the reason"
                    .to_owned(),
                "plugins/ua_log_stdout.c: `UA_Log_Stdout_log` no longer calls a clock. \
                 Remove it from CLOCK_CALLS"
                    .to_owned(),
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
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
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
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
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
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
        assert_eq!(
            result,
            Err(vec![
                "@gen.c: `UA_gen` calls a global clock function. Find whether a node \
                 runs it; if not, add it to CLOCK_CALLS with the reason"
                    .to_owned()
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
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
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
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
            "-O2\n-include\nsys/stat.h\n-I-\n-D\n-save-temps\n",
        );
        let refused = |flag: &str| {
            format!("flags.txt: `{flag}` is not a -D, -I, or -std flag with its value")
        };
        assert_eq!(
            check(&root),
            Err(
                ["-O2", "-include", "sys/stat.h", "-I-", "-D", "-save-temps"]
                    .map(refused)
                    .to_vec()
            )
        );
        std::fs::write(copy.join("flags.txt"), flags).unwrap();
        append(
            "src/util/ua_util.c",
            "static long long hidden(void) { return UA_DateTime_now(); }\n\
             long long later(void) { return hidden(); }\n",
        );
        assert_eq!(
            check(&root),
            Err(vec![
                "src/util/ua_util.c: `hidden` calls a global clock function. Find \
                 whether a node runs it; if not, add it to CLOCK_CALLS with the reason"
                    .to_owned(),
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
    fn check_finds_an_unlisted_header_that_a_listed_header_included_first() {
        let (root, repo, result) = run_after("order", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        let copy = root.join("patches/open62541");
        let append = |path: &str, text: &str| {
            let old = std::fs::read_to_string(copy.join(path)).unwrap();
            std::fs::write(copy.join(path), old + text).unwrap();
        };
        append("src/util/ua_encryptedsecret.c", "#include <time.h>\n");
        append(
            "src/util/ua_util.c",
            "#include <pthread.h>\n#include <time.h>\n",
        );
        assert_eq!(
            check(&root),
            Err(vec![
                "src/util/ua_encryptedsecret.c: includes the system header `time.h`, \
                 which SYSTEM_HEADERS does not list"
                    .to_owned(),
                "src/util/ua_util.c: includes the system header `time.h`, which \
                 SYSTEM_HEADERS does not list"
                    .to_owned(),
            ])
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
    fn check_finds_a_clock_call_that_a_listed_function_inlines() {
        let (root, repo, result) = run_after("inline", |_| {}, &[]);
        assert_eq!(result, Ok(()));
        std::fs::write(
            root.join("patches/open62541/plugins/ua_log_stdout.c"),
            "#include \"clock.h\"\n\
             static inline __attribute__((always_inline)) long long helper(void) {\n\
             return UA_DateTime_now();\n}\n\
             long long UA_Log_Stdout_log(void) { return helper(); }\n",
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
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
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
