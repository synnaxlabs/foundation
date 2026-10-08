//! Copies a release of open62541 into `patches/open62541/`: each C file that our
//! options compile and each header that it includes, unchanged.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::files;

/// The upstream repository.
pub(crate) const URL: &str = "https://github.com/open62541/open62541.git";

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

/// Options that change only the objects that [`clock_calls`] reads: no inlining, no
/// folding of identical functions, and no LTO, so each call keeps the function that
/// holds it in the source.
const ANALYSIS: [&str; 4] = [
    "-DCMAKE_BUILD_TYPE=Release",
    "-DCMAKE_INTERPROCEDURAL_OPTIMIZATION=OFF",
    "-DCMAKE_C_FLAGS=-fno-inline -fno-ipa-icf",
    "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON",
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

/// Clones `tag` of `url` into `target/open62541/`, builds it with [`OPTIONS`], and
/// replaces `patches/open62541/` with its compiled sources, their headers, `LICENSE`,
/// `sources.txt` (each `.c` file), and `VERSION` (tag and commit). Needs Linux,
/// `git`, `cmake`, Python 3, GCC, and GNU `objdump`.
///
/// # Errors
///
/// A step that fails, a header outside the source and build trees, or a call of a
/// clock function that [`CLOCK_CALLS`] does not list, or an entry it lists that no
/// call matches. On an error, `patches/open62541/` does not change.
pub(crate) fn run(root: &Path, url: &str, tag: &str) -> Result<(), Vec<String>> {
    let work = root.join("target/open62541");
    let (src, build) = (work.join("src"), work.join("build"));
    let trees = Trees {
        src: &src,
        build: &build,
    };
    let commit = compile(&work, &trees, url, tag).map_err(|e| vec![e])?;
    let (copy, mut problems) = collect(&trees).map_err(|e| vec![e])?;
    problems.extend(unlisted(&copy.calls));
    if !problems.is_empty() {
        return Err(problems);
    }
    write(root, &src, &copy.files, &format!("{tag}\n{commit}")).map_err(|e| vec![e])
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
    exec(cmake.args(OPTIONS).args(ANALYSIS))?;
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

/// What a built clone holds.
struct Copy {
    /// Each (from, to): each source of the library and each header that it includes,
    /// other than a system header.
    files: BTreeSet<(PathBuf, PathBuf)>,
    /// Each (file, function) that calls a function of [`CLOCKS`].
    calls: BTreeSet<(String, String)>,
}

/// The [`Copy`] of a built clone, and an error for each header outside the clone.
fn collect(trees: &Trees<'_>) -> Result<(Copy, Vec<String>), String> {
    let commands = std::fs::read_to_string(trees.build.join("compile_commands.json"))
        .map_err(|e| format!("compile_commands.json: {e}"))?;
    let mut copy = Copy {
        files: BTreeSet::new(),
        calls: BTreeSet::new(),
    };
    let mut outside = Vec::new();
    for (source, object) in objects(&commands, trees.build)? {
        let file = trees.relative(&source)?;
        let text = exec(Command::new("objdump").arg("-dr").arg(&object))?;
        for function in clock_calls(&text) {
            copy.calls.insert((file.display().to_string(), function));
        }
        let depfile = std::fs::read_to_string(object.with_extension("o.d"))
            .map_err(|e| format!("{}.d: {e}", object.display()))?;
        for header in headers(&depfile) {
            if header.starts_with("/usr") {
                continue;
            }
            match trees.relative(&header) {
                Ok(path) => {
                    copy.files.insert((header, path));
                }
                Err(e) => outside.push(e),
            }
        }
        copy.files.insert((source, file));
    }
    Ok((copy, outside))
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
            return Ok(Path::new("src_generated").join(generated));
        }
        path.strip_prefix(self.src)
            .map(Path::to_path_buf)
            .map_err(|_outside| format!("{} is outside the clone", path.display()))
    }
}

/// Each (source, object) of the `open62541` library in `compile_commands.json`.
fn objects(commands: &str, build: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let commands: Value =
        serde_json::from_str(commands).map_err(|e| format!("compile_commands: {e}"))?;
    let entries = commands
        .as_array()
        .ok_or("compile_commands is not a list")?;
    let mut objects = Vec::new();
    for entry in entries {
        let (Some(file), Some(output)) =
            (entry["file"].as_str(), entry["output"].as_str())
        else {
            return Err(format!(
                "compile_commands entry with no file or output: {entry}"
            ));
        };
        if output.contains("/open62541-object.dir/")
            || output.contains("/open62541-plugins.dir/")
        {
            objects.push((PathBuf::from(file), build.join(output)));
        }
    }
    Ok(objects)
}

/// The functions in the output of `objdump -dr` that refer to a function of
/// [`CLOCKS`]. A name loses the suffix of a compiler clone, such as `.isra.0`.
fn clock_calls(text: &str) -> BTreeSet<String> {
    let mut calls = BTreeSet::new();
    let mut function = "";
    for line in text.lines() {
        if let Some(name) = line.strip_suffix(">:").and_then(|l| l.split_once(" <")) {
            function = name.1.split('.').next().unwrap_or(name.1);
        } else if let Some((_, relocation)) = line.split_once(": R_") {
            let symbol = relocation.split_whitespace().last().unwrap_or_default();
            let symbol = symbol.split(['+', '-']).next().unwrap_or_default();
            if CLOCKS.contains(&symbol) {
                calls.insert(function.to_owned());
            }
        }
    }
    calls
}

/// Each header that a Make depfile names.
fn headers(depfile: &str) -> Vec<PathBuf> {
    depfile
        .split_whitespace()
        .map(PathBuf::from)
        .filter(|path| path.extension().is_some_and(|e| e == "h"))
        .collect()
}

/// Each call in `found` that [`CLOCK_CALLS`] does not list, and each listed call that
/// `found` does not hold.
fn unlisted(found: &BTreeSet<(String, String)>) -> Vec<String> {
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

/// Replaces `patches/open62541/` with each (from, to) of `copy`.
fn write(
    root: &Path,
    src: &Path,
    copy: &BTreeSet<(PathBuf, PathBuf)>,
    version: &str,
) -> Result<(), String> {
    let dest = root.join("patches/open62541");
    remove(&dest)?;
    let mut sources = String::new();
    let license = (src.join("LICENSE"), PathBuf::from("LICENSE"));
    for (from, to) in copy.iter().chain([&license]) {
        if to.extension().is_some_and(|e| e == "c") {
            sources.push_str(&to.display().to_string());
            sources.push('\n');
        }
        let to = dest.join(to);
        let dir = to.parent().ok_or("a file with no directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::copy(from, &to).map_err(|e| format!("{}: {e}", from.display()))?;
    }
    std::fs::write(dest.join("sources.txt"), sources)
        .and_then(|()| std::fs::write(dest.join("VERSION"), format!("{version}\n")))
        .map_err(|e| format!("{}: {e}", dest.display()))
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
    let name = command.get_program().to_string_lossy().into_owned();
    let output = command.output().map_err(|e| format!("{name}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{name} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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
    fn headers_reads_each_header_of_a_depfile() {
        let depfile = "a.c.o: /s/src/a.c /s/src/a.h \\\n /s/src/../deps/b.h /usr/x.h\n";
        assert_eq!(
            headers(depfile),
            [
                PathBuf::from("/s/src/a.h"),
                PathBuf::from("/s/src/../deps/b.h"),
                PathBuf::from("/usr/x.h"),
            ]
        );
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
            trees.relative(Path::new("/w/build/other.h")),
            Err("/w/build/other.h is outside the clone".to_owned())
        );
    }

    #[test]
    fn objects_keeps_the_library_objects_only() {
        let commands = r#"[
            {"file": "/w/src/src/ua_types.c",
             "output": "CMakeFiles/open62541-object.dir/src/ua_types.c.o"},
            {"file": "/w/src/plugins/ua_log_stdout.c",
             "output": "CMakeFiles/open62541-plugins.dir/plugins/ua_log_stdout.c.o"},
            {"file": "/w/src/tools/x.c", "output": "CMakeFiles/x.dir/tools/x.c.o"}
        ]"#;
        let build = Path::new("/w/build");
        assert_eq!(
            objects(commands, build),
            Ok(vec![
                (
                    PathBuf::from("/w/src/src/ua_types.c"),
                    build.join("CMakeFiles/open62541-object.dir/src/ua_types.c.o"),
                ),
                (
                    PathBuf::from("/w/src/plugins/ua_log_stdout.c"),
                    build.join(
                        "CMakeFiles/open62541-plugins.dir/plugins/ua_log_stdout.c.o"
                    ),
                ),
            ])
        );
        assert_eq!(
            objects(r#"[{"file": "a.c"}]"#, build),
            Err(
                r#"compile_commands entry with no file or output: {"file":"a.c"}"#
                    .to_owned()
            )
        );
    }

    #[test]
    fn unlisted_names_a_new_call_and_a_listed_call_that_is_gone() {
        let mut found: BTreeSet<(String, String)> = CLOCK_CALLS
            .iter()
            .map(|&(file, function)| (file.to_owned(), function.to_owned()))
            .collect();
        assert_eq!(unlisted(&found), Vec::<String>::new());
        found.remove(&("src/util/ua_util.c".to_owned(), "UA_random_seed".to_owned()));
        found.insert(("src/ua_types.c".to_owned(), "UA_new".to_owned()));
        assert_eq!(
            unlisted(&found),
            [
                "src/ua_types.c: `UA_new` calls a global clock function. Find whether \
                 a node runs it; if not, add it to CLOCK_CALLS with the reason",
                "src/util/ua_util.c: `UA_random_seed` no longer calls a clock. Remove \
                 it from CLOCK_CALLS",
            ]
        );
    }

    /// A directory of the test `name`, empty, in the temporary directory.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("xtask-open62541-{name}-{}", std::process::id()));
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
        git(&["add", "."]);
        git(&["commit", "-q", "-m", tag]);
        git(&["tag", tag]);
    }

    /// A project with the layout of open62541: the `open62541` library from two
    /// object libraries, with a call of a clock function at each place that
    /// [`CLOCK_CALLS`] lists, a generated header, and a system header.
    fn create_project(repo: &Path) {
        exec(Command::new("git").arg("init").arg("-q").arg(repo)).unwrap();
        let call = |functions: &[&str]| {
            let body = "(void) { return UA_DateTime_now(); }\n";
            let mut text = "#include <stdio.h>\n#include \"clock.h\"\n".to_owned();
            for function in functions {
                text = text + "long long " + function + body;
            }
            text
        };
        let util =
            "#include \"open62541/config.h\"\n".to_owned() + &call(&["UA_random_seed"]);
        create_files(
            repo,
            &[
                ("LICENSE", "license\n"),
                (
                    "CMakeLists.txt",
                    "cmake_minimum_required(VERSION 3.20)\nproject(fixture C)\n\
                     configure_file(config.h.in src_generated/open62541/config.h)\n\
                     file(WRITE ${CMAKE_BINARY_DIR}/other.h \"\")\n\
                     include_directories(include ${CMAKE_BINARY_DIR}/src_generated \
                     ${CMAKE_BINARY_DIR})\n\
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
                ("include/clock.h", "long long UA_DateTime_now(void);\n"),
                ("src/util/ua_util.c", &util),
                (
                    "src/util/ua_encryptedsecret.c",
                    &call(&["encryptUserIdentityTokenEcc"]),
                ),
                (
                    "plugins/ua_config_default.c",
                    &call(&["setDefaultConfig", "interruptServer"]),
                ),
                ("plugins/ua_log_stdout.c", &call(&["UA_Log_Stdout_log"])),
                ("tools/tool.c", "int main(void) { return 0; }\n"),
            ],
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
    fn run_copies_each_compiled_source_and_its_headers() {
        let (root, repo) = (temp("copies-root"), temp("copies-repo"));
        create_project(&repo);
        tag(&repo, "v1");
        create_files(&root, &[("patches/open62541/stale.c", "")]);
        let url = format!("file://{}", repo.display());
        assert_eq!(run(&root, &url, "v1"), Ok(()));
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
        let mut git = Command::new("git");
        let commit = exec(git.arg("-C").arg(&repo).args(["rev-parse", "v1"])).unwrap();
        assert_eq!(read("VERSION"), format!("v1\n{commit}\n"));
        assert_eq!(
            read("src/util/ua_util.c"),
            std::fs::read_to_string(repo.join("src/util/ua_util.c")).unwrap()
        );
        remove(&root).and_then(|()| remove(&repo)).unwrap();
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs GCC and GNU objdump")]
    fn run_refuses_a_new_clock_call_and_a_header_outside_the_clone() {
        let (root, repo) = (temp("refuses-root"), temp("refuses-repo"));
        create_project(&repo);
        create_files(
            &repo,
            &[(
                "src/more/ua_types.c",
                "#include \"clock.h\"\n#include \"other.h\"\n\
                     long long UA_new(void) { return UA_DateTime_now(); }\n",
            )],
        );
        tag(&repo, "v2");
        create_files(&root, &[("patches/open62541/kept.c", "")]);
        let url = format!("file://{}", repo.display());
        let build = root.join("target/open62541/build");
        assert_eq!(
            run(&root, &url, "v2"),
            Err(vec![
                format!("{} is outside the clone", build.join("other.h").display()),
                "src/more/ua_types.c: `UA_new` calls a global clock function. Find \
                 whether a node runs it; if not, add it to CLOCK_CALLS with the reason"
                    .to_owned(),
            ])
        );
        assert!(root.join("patches/open62541/kept.c").exists());
        remove(&root).and_then(|()| remove(&repo)).unwrap();
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
