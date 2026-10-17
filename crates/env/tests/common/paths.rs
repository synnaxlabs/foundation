//! The cases of `env::paths` that `os` and `sim` both pass. The tests of each include
//! this file by `#[path]`, so it uses only `env` and `std`.

use std::path::{Path, PathBuf};
use std::slice;

use env::paths::{Error, Found, Paths};

/// What a case runs on.
pub(crate) struct Host<'a> {
    pub(crate) paths: &'a Paths,
    /// A directory that is there and empty. On `sim`, only `/` is there with no file
    /// under it.
    pub(crate) dir: &'a Path,
    /// Makes a directory in a directory that is there.
    pub(crate) make_dir: &'a dyn Fn(&Path),
    /// Makes a symbolic link at the first path that names the second.
    pub(crate) make_link: &'a dyn Fn(&Path, &Path),
}

/// `EEXIST`, `ENOTDIR`, and `EISDIR` on Linux and macOS.
pub(crate) const EXISTS: i32 = 17;
pub(crate) const NOT_DIRECTORY: i32 = 20;
pub(crate) const DIRECTORY: i32 = 21;

/// A bound that each directory of a case passes.
const ALL: usize = 1 << 12;

/// One `#[test]` for each case, which gives the case to `$run`. Call it where this
/// module is in scope as `cases`.
macro_rules! tests {
    ($run:ident) => {
        cases::tests!(
            @each $run,
            a_file_at_the_bound_reads_whole,
            a_file_over_the_bound_reads_large,
            a_directory_lists_in_name_order,
            a_directory_whose_paths_pass_the_bound_reads_large,
            a_path_with_nothing_is_not_found,
            a_write_replaces_the_file,
            a_path_through_a_file_fails,
            a_path_that_ends_in_a_slash_or_a_dot_names_a_directory,
            an_empty_path_is_not_found,
            a_resolve_gives_the_absolute_path_with_no_dots,
            a_resolve_keeps_a_last_name_that_is_not_there,
            a_resolve_follows_each_link,
            a_resolve_through_a_file_fails,
            a_resolve_through_a_directory_that_is_not_there_is_not_found,
            a_resolve_of_a_link_to_nothing_is_not_found,
            within_reaches_only_its_folders,
            within_follows_a_link_out,
            within_resolves_its_folders,
            within_gives_not_found_through_a_directory_that_is_not_there,
            a_last_link_to_nothing_through_a_link_is_not_found,
            a_link_to_a_file_with_a_slash_fails,
            a_resolve_of_a_link_to_nothing_with_a_slash_is_not_found,
            within_hides_a_hidden_folder_that_is_not_there,
            within_hides_a_bad_link_beside_a_hidden_folder_that_is_not_there,
            within_hides_a_hidden_file_that_a_write_replaces,
            within_holds_nothing_in_a_file_that_a_write_replaces,
            within_hides_a_hidden_folder_that_the_limits_refuse,
            a_directory_made_takes_files,
            a_directory_made_again_or_through_a_link_is_ok,
            a_directory_under_one_that_is_not_there_is_not_found,
            a_directory_on_a_file_or_a_link_to_nothing_fails,
            a_directory_outside_is_not_made,
            within_makes_no_directory_on_a_link_that_does_not_resolve,
            within_keeps_its_limits_on_a_path_that_does_not_resolve,
            within_agrees_with_free_on_each_end_of_a_link_that_does_not_resolve,
            a_directory_on_a_link_to_nothing_and_a_dot_is_not_found,
            a_directory_on_a_link_to_nothing_and_a_slash_fails,
            a_directory_on_a_file_and_a_slash_fails,
            a_directory_on_a_link_to_a_file_and_a_slash_fails,
            a_directory_on_a_file_and_a_dot_fails,
            a_directory_of_a_dot_and_a_slash_is_not_made,
            within_hides_a_hidden_folder_that_is_a_file,
            within_hides_a_hidden_file_on_each_end_from_make_dir,
            within_hides_a_hidden_file_on_each_end,
            within_checks_each_part_of_a_path_that_does_not_resolve,
            within_checks_each_step_of_a_path_that_resolves,
            within_stops_a_long_path_at_its_first_step_that_fails,
            within_makes_no_directory_on_a_step_that_fails,
            within_hides_each_name_of_a_hidden_folder_on_the_way,
            within_refuses_each_name_outside_on_the_way,
            within_checks_no_hidden_folder_past_its_limits,
            within_refuses_each_path_under_a_hidden_folder_outside_the_folders,
            within_beside_another_refuses_each_name_of_its_folders,
            a_directory_through_a_file_fails,
            a_directory_of_the_root_is_ok,
        );
    };
    (@each $run:ident, $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $run(cases::$case);
            }
        )*
    };
}
pub(crate) use tests;

pub(crate) fn a_file_at_the_bound_reads_whole(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let path = dir.join("plant.hcl");
    paths.write(&path, b"node {}").unwrap();
    assert_eq!(paths.read(&path, 7), Ok(Found::File(b"node {}".to_vec())));
    let empty = dir.join("empty");
    paths.write(&empty, b"").unwrap();
    assert_eq!(paths.read(&empty, 0), Ok(Found::File(Vec::new())));
}

pub(crate) fn a_file_over_the_bound_reads_large(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let path = dir.join("plant.hcl");
    paths.write(&path, b"node {}").unwrap();
    assert_eq!(paths.read(&path, 6), Ok(Found::Large));
    assert_eq!(paths.read(&path, 0), Ok(Found::Large));
}

pub(crate) fn a_directory_lists_in_name_order(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let config = dir.join("config");
    assert_eq!(paths.read(dir, 0), Ok(Found::Directory(Vec::new())));
    (host.make_dir)(&config);
    (host.make_dir)(&config.join("c"));
    for name in ["b.hcl", "a.hcl", "c/d.hcl", "a-b.hcl"] {
        paths.write(&config.join(name), b"x").unwrap();
    }
    let entries = ["a-b.hcl", "a.hcl", "b.hcl", "c"].map(|name| config.join(name));
    assert_eq!(
        paths.read(&config, ALL),
        Ok(Found::Directory(entries.to_vec()))
    );
    assert_eq!(paths.read(dir, ALL), Ok(Found::Directory(vec![config])));
}

pub(crate) fn a_directory_whose_paths_pass_the_bound_reads_large(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let config = dir.join("config");
    (host.make_dir)(&config);
    for name in ["a.hcl", "b.hcl", "c"] {
        paths.write(&config.join(name), b"x").unwrap();
    }
    let entries = ["a.hcl", "b.hcl", "c"].map(|name| config.join(name));
    let size = entries.iter().map(|entry| entry.as_os_str().len()).sum();
    let listed = Ok(Found::Directory(entries.to_vec()));
    assert_eq!(paths.read(&config, size), listed);
    assert_eq!(paths.read(&config, size - 1), Ok(Found::Large));
}

pub(crate) fn a_path_with_nothing_is_not_found(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    assert_eq!(paths.read(&dir.join("none"), 1), Err(Error::NotFound));
    assert_eq!(paths.read(&dir.join("none/a"), 1), Err(Error::NotFound));
}

pub(crate) fn a_write_replaces_the_file(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let path = dir.join("plant.plan");
    paths.write(&path, b"first plan").unwrap();
    paths.write(&path, b"second").unwrap();
    assert_eq!(paths.read(&path, ALL), Ok(Found::File(b"second".to_vec())));
    assert_eq!(paths.read(dir, ALL), Ok(Found::Directory(vec![path])));
}

pub(crate) fn a_path_through_a_file_fails(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let file = dir.join("plant.hcl");
    paths.write(&file, b"x").unwrap();
    let under = file.join("a");
    let through = Error::Io {
        code: NOT_DIRECTORY,
    };
    assert_eq!(paths.read(&under, 1), Err(through));
    assert_eq!(paths.write(&under, b"y"), Err(through));
    let config = dir.join("config");
    (host.make_dir)(&config);
    paths.write(&config.join("a.hcl"), b"x").unwrap();
    let on_dir = paths.write(&config, b"y");
    assert_eq!(on_dir, Err(Error::Io { code: DIRECTORY }));
    let entries = vec![config, file];
    assert_eq!(paths.read(dir, ALL), Ok(Found::Directory(entries)));
}

pub(crate) fn a_path_that_ends_in_a_slash_or_a_dot_names_a_directory(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let file = dir.join("plant.hcl");
    paths.write(&file, b"x").unwrap();
    let config = dir.join("config");
    (host.make_dir)(&config);
    paths.write(&config.join("a.hcl"), b"x").unwrap();
    let through = Error::Io {
        code: NOT_DIRECTORY,
    };
    for end in ["/", "/."] {
        let named = |path: &Path| PathBuf::from(format!("{}{end}", path.display()));
        assert_eq!(paths.read(&named(&file), 1), Err(through), "{end}");
        assert_eq!(paths.write(&named(&file), b"y"), Err(through), "{end}");
        let none = named(&dir.join("none"));
        assert_eq!(paths.read(&none, 1), Err(Error::NotFound), "{end}");
        assert_eq!(paths.write(&none, b"y"), Err(Error::NotFound), "{end}");
        let named = named(&config);
        let entries = Ok(Found::Directory(vec![named.join("a.hcl")]));
        assert_eq!(paths.read(&named, ALL), entries, "{end}");
        let on_dir = Err(Error::Io { code: DIRECTORY });
        assert_eq!(paths.write(&named, b"y"), on_dir, "{end}");
    }
    let dots = file.join("x/..");
    assert_eq!(paths.read(&dots, 1), Err(through));
    assert_eq!(paths.write(&dots, b"y"), Err(through));
    let none = dir.join("none/x/..");
    assert_eq!(paths.write(&none, b"y"), Err(Error::NotFound));
    assert_eq!(paths.read(&file, 1), Ok(Found::File(b"x".to_vec())));
    let entries = vec![config, file];
    assert_eq!(paths.read(dir, ALL), Ok(Found::Directory(entries)));
}

pub(crate) fn an_empty_path_is_not_found(host: &Host<'_>) {
    let paths = host.paths;
    assert_eq!(paths.read(Path::new(""), 0), Err(Error::NotFound));
    assert_eq!(paths.write(Path::new(""), b"x"), Err(Error::NotFound));
}

pub(crate) fn a_resolve_gives_the_absolute_path_with_no_dots(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let base = paths.resolve(dir).unwrap();
    assert!(base.is_absolute(), "{}", base.display());
    let config = dir.join("config");
    (host.make_dir)(&config);
    (host.make_dir)(&config.join("c"));
    paths.write(&config.join("a.hcl"), b"x").unwrap();
    let a = base.join("config/a.hcl");
    for path in ["config/a.hcl", "config/./a.hcl", "./config/../config/a.hcl"] {
        assert_eq!(paths.resolve(&dir.join(path)), Ok(a.clone()), "{path}");
    }
    for path in ["config", "config/", "config/.", "config/c/.."] {
        let resolved = paths.resolve(&dir.join(path));
        assert_eq!(resolved, Ok(base.join("config")), "{path}");
    }
    assert_eq!(paths.resolve(&dir.join("config/..")), Ok(base));
}

pub(crate) fn a_resolve_keeps_a_last_name_that_is_not_there(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let base = paths.resolve(dir).unwrap();
    (host.make_dir)(&dir.join("config"));
    for path in ["none", "none/", "config/none.plan", "config/../none"] {
        let resolved = paths.resolve(&dir.join(path)).unwrap();
        let name = Path::new(path).file_name().unwrap();
        assert_eq!(resolved.file_name(), Some(name), "{path}");
        assert!(resolved.starts_with(&base), "{path}");
    }
    assert_eq!(paths.read(&dir.join("none"), 1), Err(Error::NotFound));
}

pub(crate) fn a_resolve_follows_each_link(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let base = paths.resolve(dir).unwrap();
    let (config, plans) = (dir.join("config"), dir.join("plans"));
    (host.make_dir)(&config);
    (host.make_dir)(&plans);
    paths.write(&config.join("a.hcl"), b"x").unwrap();
    (host.make_link)(&dir.join("to-config"), &config);
    (host.make_link)(&plans.join("up"), Path::new(".."));
    (host.make_link)(&plans.join("a.hcl"), Path::new("../config/a.hcl"));
    let a = base.join("config/a.hcl");
    for path in ["to-config/a.hcl", "plans/a.hcl", "plans/up/config/a.hcl"] {
        assert_eq!(paths.resolve(&dir.join(path)), Ok(a.clone()), "{path}");
    }
    let above = paths.resolve(&dir.join("to-config/.."));
    assert_eq!(above, Ok(base.clone()));
    let new = paths.resolve(&dir.join("plans/up/to-config/none.plan"));
    assert_eq!(new, Ok(base.join("config/none.plan")));
    assert_eq!(
        paths.read(&dir.join("plans/a.hcl"), 1),
        Ok(Found::File(b"x".to_vec()))
    );
}

pub(crate) fn a_resolve_through_a_file_fails(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let file = dir.join("plant.hcl");
    paths.write(&file, b"x").unwrap();
    let through = Err(Error::Io {
        code: NOT_DIRECTORY,
    });
    for path in [
        "plant.hcl/a",
        "plant.hcl/a/b",
        "plant.hcl/",
        "plant.hcl/.",
        "plant.hcl/..",
    ] {
        assert_eq!(paths.resolve(&dir.join(path)), through, "{path}");
    }
    assert_eq!(paths.resolve(Path::new("")), Err(Error::NotFound));
}

pub(crate) fn a_resolve_through_a_directory_that_is_not_there_is_not_found(
    host: &Host<'_>,
) {
    let (paths, dir) = (host.paths, host.dir);
    for path in ["none/a", "none/x/..", "none/x/a", "none/./a"] {
        let resolved = paths.resolve(&dir.join(path));
        assert_eq!(resolved, Err(Error::NotFound), "{path}");
        let read = paths.read(&dir.join(path), 1);
        assert_eq!(read, Err(Error::NotFound), "{path}");
        let written = paths.write(&dir.join(path), b"x");
        assert_eq!(written, Err(Error::NotFound), "{path}");
    }
}

pub(crate) fn a_resolve_of_a_link_to_nothing_is_not_found(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    (host.make_link)(&dir.join("link.plan"), &dir.join("plant.plan"));
    (host.make_link)(&dir.join("next.plan"), &dir.join("link.plan"));
    for name in ["link.plan", "next.plan"] {
        let resolved = paths.resolve(&dir.join(name));
        assert_eq!(resolved, Err(Error::NotFound), "{name}");
        let written = paths.write(&dir.join(name), b"x");
        assert_eq!(written, Err(Error::NotFound), "{name}");
    }
}

/// `work` is the folder, `work/data` is hidden, and `workshop` is beside `work`.
pub(crate) fn within_work(host: &Host<'_>) -> (Paths, PathBuf) {
    let (paths, dir) = (host.paths, host.dir);
    for name in ["work", "work/data", "workshop"] {
        (host.make_dir)(&dir.join(name));
    }
    for name in [
        "work/a.hcl",
        "work/data/admin.key",
        "workshop/a.hcl",
        "b.hcl",
    ] {
        paths.write(&dir.join(name), b"x").unwrap();
    }
    let base = paths.resolve(dir).unwrap();
    let within = paths.within(&[base.join("work")], &[base.join("work/data")]);
    (within.unwrap(), base)
}

pub(crate) fn within_gives_not_found_through_a_directory_that_is_not_there(
    host: &Host<'_>,
) {
    let (within, base) = within_work(host);
    for name in ["none/x", "work/none/x", "work/data/none/x", "none/x/../.."] {
        let path = base.join(name);
        assert_eq!(host.paths.read(&path, 1), Err(Error::NotFound), "{name}");
    }
    let path = base.join("work/none/x");
    assert_eq!(within.read(&path, 1), Err(Error::NotFound));
    assert_eq!(within.write(&path, b"x"), Err(Error::NotFound));
}

pub(crate) fn within_reaches_only_its_folders(host: &Host<'_>) {
    let dir = host.dir;
    let (within, base) = within_work(host);
    let file = Ok(Found::File(b"x".to_vec()));
    assert_eq!(within.read(&dir.join("work/a.hcl"), 1), file);
    assert_eq!(within.read(&base.join("work/./a.hcl"), 1), file);
    within.write(&dir.join("work/new.plan"), b"y").unwrap();
    let entries = ["a.hcl", "data", "new.plan"].map(|name| dir.join("work").join(name));
    let listed = Ok(Found::Directory(entries.to_vec()));
    assert_eq!(within.read(&dir.join("work"), ALL), listed);
    for (path, error) in [
        ("b.hcl", Error::Outside),
        ("work/../b.hcl", Error::Outside),
        ("workshop/a.hcl", Error::Outside),
        ("work/data", Error::Hidden),
        ("work/data/admin.key", Error::Hidden),
        ("work/data/../data/admin.key", Error::Hidden),
        ("none.plan", Error::Outside),
    ] {
        let path = dir.join(path);
        assert_eq!(within.read(&path, 1), Err(error), "{}", path.display());
        let written = within.write(&path, b"y");
        assert_eq!(written, Err(error), "{}", path.display());
    }
    assert_eq!(host.paths.read(&dir.join("b.hcl"), 1), file);
    assert_eq!(
        host.paths.read(&dir.join("none.plan"), 1),
        Err(Error::NotFound)
    );
    assert_eq!(host.paths.read(&dir.join("work/data/admin.key"), 1), file);
}

pub(crate) fn within_follows_a_link_out(host: &Host<'_>) {
    let dir = host.dir;
    let (within, _) = within_work(host);
    (host.make_link)(&dir.join("work/out"), &dir.join("workshop"));
    (host.make_link)(&dir.join("work/b.hcl"), Path::new("../b.hcl"));
    (host.make_link)(&dir.join("work/key"), Path::new("data/admin.key"));
    for (path, error) in [
        ("work/out/a.hcl", Error::Outside),
        ("work/out/new.plan", Error::Outside),
        ("work/b.hcl", Error::Outside),
        ("work/key", Error::Hidden),
    ] {
        let path = dir.join(path);
        assert_eq!(within.read(&path, 1), Err(error), "{}", path.display());
        let written = within.write(&path, b"y");
        assert_eq!(written, Err(error), "{}", path.display());
    }
    (host.make_link)(&dir.join("workshop/in"), &dir.join("work"));
    let file = Ok(Found::File(b"x".to_vec()));
    assert_eq!(within.read(&dir.join("workshop/in/a.hcl"), 1), file);
    let key = dir.join("workshop/in/data/admin.key");
    assert_eq!(within.read(&key, 1), Err(Error::Hidden));
    assert_eq!(host.paths.read(&dir.join("b.hcl"), 1), file);
    assert_eq!(
        host.paths.read(&dir.join("workshop/new.plan"), 1),
        Err(Error::NotFound)
    );
}

pub(crate) fn within_resolves_its_folders(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let (_, base) = within_work(host);
    (host.make_link)(&dir.join("alias"), &dir.join("work"));
    let working = paths.resolve(Path::new(".")).unwrap();
    let up = working.components().skip(1).map(|_| Path::new(".."));
    let relative: PathBuf = up.collect();
    let relative = relative.join(base.strip_prefix("/").unwrap());
    let key = dir.join("work/data/admin.key");
    let work = [dir.join("work")];
    for hidden in [relative.join("work/data"), dir.join("alias/data")] {
        let within = paths.within(&work, slice::from_ref(&hidden)).unwrap();
        let read = within.read(&key, 1);
        assert_eq!(read, Err(Error::Hidden), "{}", hidden.display());
        let written = within.write(&key, b"y");
        assert_eq!(written, Err(Error::Hidden), "{}", hidden.display());
    }
    let none = [dir.join("none/work")];
    assert_eq!(paths.within(&none, &[]).err(), Some(Error::NotFound));
    assert_eq!(paths.within(&work, &none).err(), Some(Error::NotFound));
    let last = [dir.join("none")];
    assert_eq!(paths.within(&last, &[]).err(), Some(Error::NotFound));
    assert_eq!(paths.within(&work, &last).err(), None);
    let file = [dir.join("b.hcl/data")];
    let through = Some(Error::Io {
        code: NOT_DIRECTORY,
    });
    assert_eq!(paths.within(&work, &file).err(), through);
}

pub(crate) fn a_last_link_to_nothing_through_a_link_is_not_found(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    (host.make_dir)(&dir.join("real"));
    (host.make_dir)(&dir.join("work"));
    (host.make_link)(&dir.join("d"), &dir.join("real"));
    (host.make_link)(&dir.join("work/l"), &dir.join("d/none.plan"));
    let (base, link) = (paths.resolve(dir).unwrap(), dir.join("work/l"));
    assert_eq!(paths.resolve(&dir.join("d")), Ok(base.join("real")));
    let within = paths.within(&[dir.join("work")], &[]).unwrap();
    assert_eq!(within.read(&link, 1), Err(Error::NotFound));
    assert_eq!(within.write(&link, b"x"), Err(Error::NotFound));
    assert_eq!(paths.resolve(&link), Err(Error::NotFound));
    let made = paths.read(&dir.join("real/none.plan"), 1);
    assert_eq!(made, Err(Error::NotFound));
}

pub(crate) fn a_link_to_a_file_with_a_slash_fails(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    paths.write(&dir.join("plant.hcl"), b"x").unwrap();
    let mut target = dir.join("plant.hcl").into_os_string();
    target.push("/");
    (host.make_link)(&dir.join("l"), Path::new(&target));
    let through = Error::Io {
        code: NOT_DIRECTORY,
    };
    assert_eq!(paths.read(&dir.join("l"), 1), Err(through));
    assert_eq!(paths.resolve(&dir.join("l")), Err(through));
}

pub(crate) fn a_resolve_of_a_link_to_nothing_with_a_slash_is_not_found(
    host: &Host<'_>,
) {
    let (paths, dir) = (host.paths, host.dir);
    (host.make_link)(&dir.join("link.plan"), &dir.join("plant.plan"));
    (host.make_link)(&dir.join("data"), Path::new("store/"));
    for name in ["link.plan/", "link.plan/.", "data", "data/"] {
        assert_eq!(
            paths.resolve(&dir.join(name)),
            Err(Error::NotFound),
            "{name}"
        );
        let hidden = [dir.join(name)];
        let within = paths.within(slice::from_ref(&dir.to_path_buf()), &hidden);
        assert_eq!(within.err(), Some(Error::NotFound), "{name}");
    }
}

pub(crate) fn within_hides_a_hidden_folder_that_is_not_there(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    (host.make_dir)(&dir.join("work"));
    (host.make_dir)(&dir.join("work/sub"));
    let base = paths.resolve(dir).unwrap();
    let data = base.join("work/data");
    let within = paths
        .within(&[base.join("work")], slice::from_ref(&data))
        .unwrap();
    assert_eq!(within.make_dir(&data), Err(Error::Hidden));
    assert_eq!(within.write(&data, b"x"), Err(Error::Hidden));
    // Each new name beside it, so no name in other case or other form makes it.
    assert_eq!(within.make_dir(&base.join("work/DATA")), Err(Error::Hidden));
    assert_eq!(
        within.write(&base.join("work/b.hcl"), b"x"),
        Err(Error::Hidden)
    );
    assert_eq!(within.write(&base.join("work/sub/b.hcl"), b"x"), Ok(()));
    assert_eq!(paths.read(&data, ALL), Err(Error::NotFound));
    paths.make_dir(&data).unwrap();
    paths.write(&data.join("admin.key"), b"x").unwrap();
    let key = within.read(&data.join("admin.key"), 1);
    assert_eq!(key, Err(Error::Hidden));
}

pub(crate) fn within_hides_a_bad_link_beside_a_hidden_folder_that_is_not_there(
    host: &Host<'_>,
) {
    let (paths, dir) = (host.paths, host.dir);
    (host.make_dir)(&dir.join("work"));
    let base = paths.resolve(dir).unwrap();
    let data = base.join("work/data");
    let within = paths
        .within(&[base.join("work")], slice::from_ref(&data))
        .unwrap();
    (host.make_link)(&base.join("work/loop"), &base.join("work/loop"));
    (host.make_link)(&base.join("work/to-none"), &base.join("work/none"));
    (host.make_link)(&data, &base.join("work/none"));
    (host.make_link)(&base.join("work/to-data"), &data);
    assert_eq!(within.make_dir(&base.join("work/new")), Err(Error::Hidden));
    for name in [
        "work/loop",
        "work/loop/x",
        "work/to-none",
        "work/to-none/x",
        "work/data",
        "work/data/x",
        "work/to-data",
    ] {
        let path = base.join(name);
        let reached = [
            within.read(&path, 1).map(|_| ()),
            within.write(&path, b"x"),
            within.make_dir(&path),
        ];
        assert_eq!(reached, [Err(Error::Hidden); 3], "{name}");
    }
}

pub(crate) fn within_hides_a_hidden_file_that_a_write_replaces(host: &Host<'_>) {
    let (_, base) = within_work(host);
    let file = base.join("work/a.hcl");
    (host.make_link)(&base.join("work/link"), &file);
    let within = host
        .paths
        .within(&[base.join("work")], slice::from_ref(&file))
        .unwrap();
    host.paths.write(&file, b"new").unwrap();
    assert_eq!(within.read(&file, 3), Err(Error::Hidden));
    let link = base.join("work/link");
    assert_eq!(within.read(&link, 3), Err(Error::Hidden));
}

/// A write makes a new file, as the rename of a write of `os` does, so a folder of
/// `folders` that is a file holds nothing after it.
pub(crate) fn within_holds_nothing_in_a_file_that_a_write_replaces(host: &Host<'_>) {
    let (_, base) = within_work(host);
    let file = base.join("work/a.hcl");
    let within = host.paths.within(slice::from_ref(&file), &[]).unwrap();
    assert_eq!(within.read(&file, 1), Ok(Found::File(b"x".to_vec())));
    host.paths.write(&file, b"y").unwrap();
    assert_eq!(within.read(&file, 1), Err(Error::Outside));
}

/// A hidden folder that the limits of the handle refuse, but that is there, still
/// hides what a later folder holds.
pub(crate) fn within_hides_a_hidden_folder_that_the_limits_refuse(host: &Host<'_>) {
    let (_, base) = within_work(host);
    let data = base.join("work/data");
    let outer = host.paths.within(slice::from_ref(&data), &[]).unwrap();
    let inner = outer
        .within(slice::from_ref(&data), &[base.join("work")])
        .unwrap();
    let key = data.join("admin.key");
    assert_eq!(inner.read(&key, 1), Err(Error::Hidden));
    assert_eq!(outer.read(&key, 1), Ok(Found::File(b"x".to_vec())));
}

pub(crate) fn a_directory_made_takes_files(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    assert_eq!(paths.make_dir(&dir.join("slash/")), Ok(()));
    assert_eq!(
        paths.read(&dir.join("slash"), 0),
        Ok(Found::Directory(Vec::new()))
    );
    let data = dir.join("data");
    assert_eq!(paths.make_dir(&data), Ok(()));
    assert_eq!(paths.read(&data, ALL), Ok(Found::Directory(Vec::new())));
    paths.write(&data.join("a.plan"), b"x").unwrap();
    let entries = Ok(Found::Directory(vec![data.join("a.plan")]));
    assert_eq!(paths.read(&data, ALL), entries);
}

pub(crate) fn a_directory_made_again_or_through_a_link_is_ok(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let data = dir.join("data");
    paths.make_dir(&data).unwrap();
    paths.write(&data.join("a.plan"), b"x").unwrap();
    assert_eq!(paths.make_dir(&data), Ok(()));
    (host.make_link)(&dir.join("alias"), &data);
    assert_eq!(paths.make_dir(&dir.join("alias")), Ok(()));
    let entries = Ok(Found::Directory(vec![data.join("a.plan")]));
    assert_eq!(paths.read(&data, ALL), entries);
}

pub(crate) fn a_directory_under_one_that_is_not_there_is_not_found(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let made = paths.make_dir(&dir.join("none/data"));
    assert_eq!(made, Err(Error::NotFound));
    assert_eq!(paths.make_dir(&dir.join("none/.")), Err(Error::NotFound));
    assert_eq!(paths.read(&dir.join("none"), ALL), Err(Error::NotFound));
}

pub(crate) fn a_directory_on_a_file_or_a_link_to_nothing_fails(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    let file = dir.join("data");
    paths.write(&file, b"x").unwrap();
    (host.make_link)(&dir.join("to-file"), &file);
    (host.make_link)(&dir.join("to-none"), &dir.join("none"));
    (host.make_link)(&dir.join("to-far"), &dir.join("none/far"));
    for name in ["data", "to-file", "to-none", "to-far"] {
        let made = paths.make_dir(&dir.join(name));
        assert_eq!(made, Err(Error::Io { code: EXISTS }), "{name}");
    }
    assert_eq!(paths.read(&file, 1), Ok(Found::File(b"x".to_vec())));
    assert_eq!(paths.read(&dir.join("none"), 1), Err(Error::NotFound));
}

pub(crate) fn a_directory_outside_is_not_made(host: &Host<'_>) {
    let dir = host.dir;
    let (within, _) = within_work(host);
    for (name, error) in [("data", Error::Outside), ("work/data/new", Error::Hidden)] {
        let path = dir.join(name);
        assert_eq!(within.make_dir(&path), Err(error), "{name}");
        assert_eq!(host.paths.read(&path, 1), Err(Error::NotFound), "{name}");
    }
    assert_eq!(within.make_dir(&dir.join("work/new")), Ok(()));
}

pub(crate) fn within_makes_no_directory_on_a_link_that_does_not_resolve(
    host: &Host<'_>,
) {
    let (within, base) = within_work(host);
    for (name, target) in [
        ("to-none", "work/none"),
        ("loop", "work/loop"),
        ("to-bad", "work/a.hcl/x"),
    ] {
        let link = base.join("work").join(name);
        (host.make_link)(&link, &base.join(target));
        let free = host.paths.make_dir(&link);
        assert_eq!(free, Err(Error::Io { code: EXISTS }), "free {name}");
        assert_eq!(within.make_dir(&link), free, "within {name}");
    }
}

pub(crate) fn within_keeps_its_limits_on_a_path_that_does_not_resolve(host: &Host<'_>) {
    let (within, base) = within_work(host);
    for name in ["work/data/loop", "loop", "workshop/loop"] {
        (host.make_link)(&base.join(name), &base.join(name));
    }
    (host.make_link)(&base.join("work/out"), &base.join("workshop"));
    (host.make_link)(
        &base.join("work/data/to-none"),
        &base.join("work/data/none"),
    );
    (host.make_link)(&base.join("to-none"), &base.join("none"));
    for (name, limit) in [
        ("work/data/loop", Error::Hidden),
        ("work/data/loop/.", Error::Hidden),
        ("work/data/admin.key/x", Error::Hidden),
        ("work/data/to-none", Error::Hidden),
        ("loop", Error::Outside),
        ("to-none", Error::Outside),
        ("b.hcl/x", Error::Outside),
        ("work/out/loop", Error::Outside),
        ("work/out/loop/", Error::Outside),
    ] {
        assert_eq!(within.make_dir(&base.join(name)), Err(limit), "{name}");
    }
}

pub(crate) fn within_agrees_with_free_on_each_end_of_a_link_that_does_not_resolve(
    host: &Host<'_>,
) {
    let (within, base) = within_work(host);
    let work = base.join("work");
    for (name, target) in [
        ("loop", "loop"),
        ("to-bad", "a.hcl/x"),
        ("to-file-slash", "a.hcl/"),
    ] {
        (host.make_link)(&work.join(name), &work.join(target));
    }
    for name in [
        "loop",
        "loop/",
        "loop/.",
        "loop//",
        "loop/x",
        "to-bad",
        "to-bad/",
        "to-bad/.",
        "to-file-slash",
        "to-file-slash/",
        "a.hcl/",
        "a.hcl/.",
        "a.hcl/x",
    ] {
        let path = work.join(name);
        let free = host.paths.make_dir(&path);
        assert_eq!(within.make_dir(&path), free, "{name}");
    }
    let file = Error::Io {
        code: NOT_DIRECTORY,
    };
    // `ELOOP` of `os` is 62 on macOS.
    let looped = host.paths.read(&work.join("loop"), 1).unwrap_err();
    #[cfg(target_os = "linux")]
    assert_eq!(looped, Error::Io { code: 40 });
    for (name, error) in [
        ("none/..", Error::NotFound),
        ("a.hcl/..", file),
        ("loop/..", looped),
    ] {
        let path = work.join(name);
        let free = host.paths.make_dir(&path);
        assert_eq!(free, Err(error), "{name}");
        assert_eq!(within.make_dir(&path), free, "{name}");
    }
}

pub(crate) fn a_directory_through_a_file_fails(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    paths.write(&dir.join("data"), b"x").unwrap();
    let made = paths.make_dir(&dir.join("data/new"));
    assert_eq!(
        made,
        Err(Error::Io {
            code: NOT_DIRECTORY
        })
    );
}

pub(crate) fn a_directory_of_the_root_is_ok(host: &Host<'_>) {
    for root in ["/", "//"] {
        assert_eq!(host.paths.make_dir(Path::new(root)), Ok(()), "{root}");
    }
}

pub(crate) fn a_directory_on_a_link_to_nothing_and_a_dot_is_not_found(host: &Host<'_>) {
    let dir = host.dir;
    (host.make_link)(&dir.join("to-none"), &dir.join("none"));
    let made = host.paths.make_dir(&dir.join("to-none/."));
    assert_eq!(made, Err(Error::NotFound));
}

pub(crate) fn a_directory_on_a_link_to_nothing_and_a_slash_fails(host: &Host<'_>) {
    let dir = host.dir;
    (host.make_link)(&dir.join("to-none"), &dir.join("none"));
    let made = host.paths.make_dir(&dir.join("to-none/"));
    let none = host.paths.read(&dir.join("none"), ALL);
    assert_eq!(
        (made, none),
        (Err(Error::Io { code: EXISTS }), Err(Error::NotFound))
    );
}

pub(crate) fn a_directory_of_a_dot_and_a_slash_is_not_made(host: &Host<'_>) {
    let dir = host.dir;
    for name in ["new/./", "new/.//", "new/../"] {
        let made = host.paths.make_dir(&dir.join(name));
        let there = host.paths.read(&dir.join("new"), ALL);
        assert_eq!(
            (made, there),
            (Err(Error::NotFound), Err(Error::NotFound)),
            "{name}"
        );
    }
}

pub(crate) fn a_directory_on_a_file_and_a_slash_fails(host: &Host<'_>) {
    let dir = host.dir;
    host.paths.write(&dir.join("data"), b"x").unwrap();
    let made = host.paths.make_dir(&dir.join("data/"));
    assert_eq!(made, Err(Error::Io { code: EXISTS }));
}

pub(crate) fn a_directory_on_a_link_to_a_file_and_a_slash_fails(host: &Host<'_>) {
    let dir = host.dir;
    host.paths.write(&dir.join("data"), b"x").unwrap();
    (host.make_link)(&dir.join("to-file"), &dir.join("data"));
    let made = host.paths.make_dir(&dir.join("to-file/"));
    assert_eq!(made, Err(Error::Io { code: EXISTS }));
}

pub(crate) fn a_directory_on_a_file_and_a_dot_fails(host: &Host<'_>) {
    let dir = host.dir;
    host.paths.write(&dir.join("data"), b"x").unwrap();
    let made = host.paths.make_dir(&dir.join("data/."));
    assert_eq!(
        made,
        Err(Error::Io {
            code: NOT_DIRECTORY
        })
    );
}

pub(crate) fn within_hides_a_hidden_folder_that_is_a_file(host: &Host<'_>) {
    let (_, base) = within_work(host);
    let file = base.join("work/a.hcl");
    let within = host
        .paths
        .within(&[base.join("work")], slice::from_ref(&file))
        .unwrap();
    assert_eq!(within.read(&file, 1), Err(Error::Hidden));
    let beside = base.join("work/b.hcl");
    assert_eq!(within.write(&beside, b"x"), Ok(()));
}

pub(crate) fn within_hides_a_hidden_file_on_each_end_from_make_dir(host: &Host<'_>) {
    let (_, base) = within_work(host);
    let file = base.join("work/a.hcl");
    let within = host
        .paths
        .within(&[base.join("work")], slice::from_ref(&file))
        .unwrap();
    let names = ["work/a.hcl", "work/a.hcl/x", "work/a.hcl/", "work/a.hcl/."];
    let made = names.map(|name| within.make_dir(&base.join(name)));
    assert_eq!(made, [Err(Error::Hidden); 4], "{names:?}");
}

pub(crate) fn within_hides_a_hidden_file_on_each_end(host: &Host<'_>) {
    let (_, base) = within_work(host);
    let file = base.join("work/a.hcl");
    let within = host
        .paths
        .within(&[base.join("work")], slice::from_ref(&file))
        .unwrap();
    let ends = ["work/a.hcl/", "work/a.hcl/.", "work/a.hcl/x"];
    let read = ends.map(|name| within.read(&base.join(name), 1).map(|_| ()));
    let write = ends.map(|name| within.write(&base.join(name), b"x"));
    assert_eq!(
        (read, write),
        ([Err(Error::Hidden); 3], [Err(Error::Hidden); 3])
    );
}

pub(crate) fn within_checks_each_part_of_a_path_that_does_not_resolve(host: &Host<'_>) {
    let (within, base) = within_work(host);
    let through_a_file = Error::Io {
        code: NOT_DIRECTORY,
    };
    for (name, error) in [
        ("work/data/none/x", Error::Hidden),
        ("work/data/admin.key/", Error::Hidden),
        ("work/data/admin.key/x", Error::Hidden),
        ("none/x", Error::Outside),
        ("none/x/../..", Error::Outside),
        ("b.hcl/", Error::Outside),
        ("b.hcl/x", Error::Outside),
        ("work/none/x", Error::NotFound),
        ("work/a.hcl/", through_a_file),
        ("work/a.hcl/x", through_a_file),
    ] {
        let path = base.join(name);
        assert_eq!(within.read(&path, 1), Err(error), "{name}");
        assert_eq!(within.write(&path, b"x"), Err(error), "{name}");
    }
    (host.make_dir)(&base.join("work/sub"));
    let narrowed = within.within(&[base.join("work/sub")], &[]).unwrap();
    let path = base.join("work/sub/none/x");
    assert_eq!(narrowed.read(&path, 1), Err(Error::NotFound));
    let path = base.join("work/none/x");
    assert_eq!(narrowed.read(&path, 1), Err(Error::Outside));
}

pub(crate) fn within_checks_each_step_of_a_path_that_resolves(host: &Host<'_>) {
    let (within, base) = within_work(host);
    for (name, error) in [
        ("workshop/../work/a.hcl", Error::Outside),
        ("work/data/../a.hcl", Error::Hidden),
        ("x/../work/data/y", Error::Outside),
    ] {
        let path = base.join(name);
        assert_eq!(within.read(&path, 1), Err(error), "{name}");
        assert_eq!(within.write(&path, b"x"), Err(error), "{name}");
    }
    let path = base.join("workshop/sub/../../work/a.hcl");
    assert_eq!(within.read(&path, 1), Err(Error::Outside));
    (host.make_dir)(&base.join("workshop/sub"));
    assert_eq!(within.read(&path, 1), Err(Error::Outside));
    (host.make_dir)(&base.join("x"));
    let path = base.join("x/../work/data/y");
    assert_eq!(within.write(&path, b"x"), Err(Error::Outside));
    let path = base.join("work/../work/a.hcl");
    assert_eq!(within.write(&path, b"y"), Ok(()));
    assert_eq!(within.read(&path, 1), Ok(Found::File(b"y".to_vec())));
}

pub(crate) fn within_stops_a_long_path_at_its_first_step_that_fails(host: &Host<'_>) {
    let (within, base) = within_work(host);
    let steps = "/x".repeat(100_000);
    for (name, error) in [("none", Error::Outside), ("work/none", Error::NotFound)] {
        let path = base.join(format!("{name}{steps}"));
        assert_eq!(within.read(&path, 1), Err(error), "{name}");
    }
}

pub(crate) fn within_makes_no_directory_on_a_step_that_fails(host: &Host<'_>) {
    let (within, base) = within_work(host);
    for (name, error) in [
        ("work/data/new", Error::Hidden),
        ("work/data/none/x", Error::Hidden),
        ("none", Error::Outside),
        ("none/x/../..", Error::Outside),
        ("work/none/x", Error::NotFound),
    ] {
        let path = base.join(name);
        assert_eq!(within.make_dir(&path), Err(error), "{name}");
        assert_eq!(host.paths.read(&path, 1), Err(Error::NotFound), "{name}");
    }
}

pub(crate) fn within_hides_each_name_of_a_hidden_folder_on_the_way(host: &Host<'_>) {
    let (within, base) = within_work(host);
    (host.make_dir)(&base.join("work/data/dir"));
    let got = ["dir", "none"].map(|name| {
        let path = base.join("work/data").join(name).join("../../none/x");
        assert_eq!(host.paths.read(&path, 1), Err(Error::NotFound), "{name}");
        within.read(&path, 1).map(|_| ())
    });
    assert_eq!(got, [Err(Error::Hidden); 2]);
}

pub(crate) fn within_refuses_each_name_outside_on_the_way(host: &Host<'_>) {
    let (within, base) = within_work(host);
    let got = ["workshop", "none"].map(|name| {
        let path = base.join(name).join("../work/none/x");
        assert_eq!(host.paths.read(&path, 1), Err(Error::NotFound), "{name}");
        within.read(&path, 1).map(|_| ())
    });
    assert_eq!(got, [Err(Error::Outside); 2]);
}

pub(crate) fn within_checks_no_hidden_folder_past_its_limits(host: &Host<'_>) {
    let (within, base) = within_work(host);
    for name in ["none", "b.hcl", "workshop/a.hcl", "work/data/none"] {
        let hidden = [base.join(name)];
        let narrowed = within.within(&[base.join("work")], &hidden).map(|_| ());
        assert_eq!(narrowed, Ok(()), "{name}");
    }
}

/// A hidden folder outside the folders tells nothing of what it holds, or whether it
/// is there.
pub(crate) fn within_refuses_each_path_under_a_hidden_folder_outside_the_folders(
    host: &Host<'_>,
) {
    let (paths, dir) = (host.paths, host.dir);
    for name in ["work", "out", "out/h", "out/h/s"] {
        (host.make_dir)(&dir.join(name));
    }
    let base = paths.resolve(dir).unwrap();
    let hidden = [base.join("out/h"), base.join("out/none")];
    let within = paths.within(&[base.join("work")], &hidden).unwrap();
    for name in [
        "out/h",
        "out/h/a",
        "out/h/s/x",
        "out/none/a",
        "out/none/s/x",
    ] {
        let path = base.join(name);
        assert_eq!(within.read(&path, 1), Err(Error::Outside), "{name}");
    }
}

/// A hidden folder of the first handle tells nothing of the folders of the first that
/// a handle beside them refuses.
pub(crate) fn within_beside_another_refuses_each_name_of_its_folders(host: &Host<'_>) {
    let (paths, dir) = (host.paths, host.dir);
    for name in ["work", "work/x", "plans"] {
        (host.make_dir)(&dir.join(name));
    }
    let base = paths.resolve(dir).unwrap();
    let wide = paths
        .within(&[base.join("work")], &[base.join("work/data")])
        .unwrap();
    let beside = wide.within(&[base.join("plans")], &[]).unwrap();
    for name in ["work/x", "work/y", "work/x/a", "work/y/a", "work/data"] {
        let path = base.join(name);
        assert_eq!(beside.read(&path, 1), Err(Error::Outside), "{name}");
    }
}
