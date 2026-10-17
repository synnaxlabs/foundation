//! The cases of `env::paths` that `os` and `sim` both pass. The tests of each include
//! this file by `#[path]`, so it uses only `env` and `std`.

use std::path::{Path, PathBuf};
use std::slice;

use env::paths::{Error, Found, Kind, Paths};

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
    /// Renames the file, link, or directory at the first path to the second, as a
    /// program does: it keeps its key.
    pub(crate) move_path: &'a dyn Fn(&Path, &Path),
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
            within_refuses_a_new_name_by_a_hidden_folder_that_is_not_there,
            within_refuses_a_new_name_outside_by_a_hidden_folder_that_is_not_there,
            within_refuses_a_bad_link_beside_a_hidden_folder_that_is_not_there,
            within_hides_a_hidden_file_that_a_write_replaces,
            within_holds_nothing_in_a_file_that_a_write_replaces,
            within_holds_nothing_in_a_folder_made_again,
            within_holds_nothing_in_a_folder_that_a_link_replaces,
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
            within_checks_no_hidden_folder_past_its_limits,
            a_directory_through_a_file_fails,
            a_directory_of_the_root_is_ok,
            within_refuses_make_dir_on_a_folder_that_a_bad_link_replaces,
            within_gives_replaced_for_a_moved_folder_whatever_takes_its_place,
            within_gives_replaced_for_a_file_or_a_loop_at_a_moved_folder,
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
    assert_eq!(paths.read(&path, 6), Ok(Found::Large(Kind::File)));
    assert_eq!(paths.read(&path, 0), Ok(Found::Large(Kind::File)));
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
    let large = Ok(Found::Large(Kind::Directory));
    assert_eq!(paths.read(&config, size - 1), large);
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
        assert_eq!(within.read(&path, 1), Err(Error::NotFound), "{name}");
        assert_eq!(within.write(&path, b"x"), Err(Error::NotFound), "{name}");
        assert_eq!(host.paths.read(&path, 1), Err(Error::NotFound), "{name}");
    }
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
    let file = Ok(Found::File(b"x".to_vec()));
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

pub(crate) fn within_refuses_a_new_name_by_a_hidden_folder_that_is_not_there(
    host: &Host<'_>,
) {
    let (paths, dir) = (host.paths, host.dir);
    (host.make_dir)(&dir.join("work"));
    (host.make_dir)(&dir.join("work/sub"));
    let base = paths.resolve(dir).unwrap();
    let data = base.join("work/data");
    let within = paths
        .within(&[base.join("work")], slice::from_ref(&data))
        .unwrap();
    assert_eq!(within.make_dir(&data), Err(Error::Vacant));
    assert_eq!(within.write(&data, b"x"), Err(Error::Vacant));
    // Each new name beside it, so no name in other case or other form makes it.
    assert_eq!(within.make_dir(&base.join("work/DATA")), Err(Error::Vacant));
    assert_eq!(
        within.write(&base.join("work/b.hcl"), b"x"),
        Err(Error::Vacant)
    );
    // A read makes no name, and finds nothing.
    let read = within.read(&base.join("work/b.hcl"), ALL);
    assert_eq!(
        (read, within.read(&data, ALL)),
        (Err(Error::NotFound), Err(Error::NotFound))
    );
    assert_eq!(within.write(&base.join("work/sub/b.hcl"), b"x"), Ok(()));
    assert_eq!(paths.read(&data, ALL), Err(Error::NotFound));
    paths.make_dir(&data).unwrap();
    paths.write(&data.join("admin.key"), b"x").unwrap();
    let key = within.read(&data.join("admin.key"), 1);
    assert_eq!(key, Err(Error::Hidden));
}

/// A new name that no folder holds is outside, also beside a missing hidden folder.
pub(crate) fn within_refuses_a_new_name_outside_by_a_hidden_folder_that_is_not_there(
    host: &Host<'_>,
) {
    let (paths, dir) = (host.paths, host.dir);
    (host.make_dir)(&dir.join("work"));
    (host.make_dir)(&dir.join("work/sub"));
    let base = paths.resolve(dir).unwrap();
    let within = paths
        .within(&[base.join("work/sub")], &[base.join("work/data")])
        .unwrap();
    let written = within.write(&base.join("work/b.hcl"), b"x");
    assert_eq!(written, Err(Error::Outside));
    assert_eq!(within.write(&base.join("work/sub/b.hcl"), b"x"), Ok(()));
}

pub(crate) fn within_refuses_a_bad_link_beside_a_hidden_folder_that_is_not_there(
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
    assert_eq!(within.make_dir(&base.join("work/new")), Err(Error::Vacant));
    let names = ["work/loop", "work/to-none", "work/data", "work/to-data"];
    let made = names.map(|name| within.make_dir(&base.join(name)));
    assert_eq!(made, [Err(Error::Vacant); 4], "{names:?}");
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
/// `folders` that is a file holds nothing after it, and refuses it as replaced.
pub(crate) fn within_holds_nothing_in_a_file_that_a_write_replaces(host: &Host<'_>) {
    let (_, base) = within_work(host);
    let file = base.join("work/a.hcl");
    let within = host.paths.within(slice::from_ref(&file), &[]).unwrap();
    assert_eq!(within.read(&file, 1), Ok(Found::File(b"x".to_vec())));
    host.paths.write(&file, b"y").unwrap();
    assert_eq!(within.read(&file, 1), Err(Error::Replaced));
}

/// A program moves the folder away and makes it again. A new name in it, and a file of
/// the moved folder, which keeps its key, are refused as replaced.
pub(crate) fn within_holds_nothing_in_a_folder_made_again(host: &Host<'_>) {
    let (_, base) = within_work(host);
    let work = base.join("work");
    let within = host.paths.within(slice::from_ref(&work), &[]).unwrap();
    (host.move_path)(&work, &base.join("old"));
    (host.make_dir)(&work);
    let written = within.write(&work.join("b.hcl"), b"x");
    let moved = within.read(&base.join("old/a.hcl"), 1);
    assert_eq!(
        (written, moved),
        (Err(Error::Replaced), Err(Error::Replaced))
    );
}

/// A program replaces the folder with a link to a directory outside it. A path through
/// the link, and the same file by the name of that directory, resolve under the folder
/// by the path that it resolves to now, so the folder is replaced.
pub(crate) fn within_holds_nothing_in_a_folder_that_a_link_replaces(host: &Host<'_>) {
    let (_, base) = within_work(host);
    let work = base.join("work");
    let within = host.paths.within(slice::from_ref(&work), &[]).unwrap();
    (host.move_path)(&work, &base.join("old"));
    (host.make_link)(&work, &base.join("workshop"));
    let read = within.read(&work.join("a.hcl"), 1);
    let direct = within.read(&base.join("workshop/a.hcl"), 1);
    assert_eq!((read, direct), (Err(Error::Replaced), Err(Error::Replaced)));
}

/// A program replaces the folder with a link to nothing, or with a loop. `make_dir` at
/// the folder's path checks its place, which is the path that the folder resolved to
/// at `within`, so the folder is replaced.
pub(crate) fn within_refuses_make_dir_on_a_folder_that_a_bad_link_replaces(
    host: &Host<'_>,
) {
    let (_, base) = within_work(host);
    let work = base.join("work");
    let made =
        [("old", base.join("none")), ("old2", work.clone())].map(|(old, target)| {
            let within = host.paths.within(slice::from_ref(&work), &[]).unwrap();
            (host.move_path)(&work, &base.join(old));
            (host.make_link)(&work, &target);
            let made = within.make_dir(&work);
            (host.move_path)(&work, &base.join(format!("{old}-link")));
            (host.make_dir)(&work);
            made
        });
    assert_eq!(made, [Err(Error::Replaced); 2], "link to nothing, loop");
}

/// A program moves the folder's directory away, and puts nothing, a file, or a
/// loop at the old place. A read into the moved folder is replaced in each case.
pub(crate) fn within_gives_replaced_for_a_moved_folder_whatever_takes_its_place(
    host: &Host<'_>,
) {
    let base = host.paths.resolve(host.dir).unwrap();
    let got = ["nothing", "file", "loop"].map(|what| {
        let p = base.join(format!("p-{what}"));
        let q = base.join(format!("q-{what}"));
        (host.make_dir)(&p);
        (host.make_dir)(&p.join("work"));
        host.paths.write(&p.join("work/a.hcl"), b"x").unwrap();
        let within = host.paths.within(&[p.join("work")], &[]).unwrap();
        (host.move_path)(&p, &q);
        match what {
            "file" => host.paths.write(&p, b"f").unwrap(),
            "loop" => (host.make_link)(&p, &p),
            _ => {}
        }
        within.read(&q.join("work/a.hcl"), 1).map(|_| ())
    });
    assert_eq!(got, [Err(Error::Replaced); 3], "nothing, file, loop");
}

/// The folder moves away and a file or a loop takes its path. A read into the moved
/// folder, and `make_dir` at the folder's path, give the same error.
pub(crate) fn within_gives_replaced_for_a_file_or_a_loop_at_a_moved_folder(
    host: &Host<'_>,
) {
    let base = host.paths.resolve(host.dir).unwrap();
    let got = ["file", "loop"].map(|what| {
        let work = base.join(format!("work-{what}"));
        (host.make_dir)(&work);
        let within = host.paths.within(slice::from_ref(&work), &[]).unwrap();
        let old = base.join(format!("old-{what}"));
        (host.move_path)(&work, &old);
        match what {
            "file" => host.paths.write(&work, b"f").unwrap(),
            _ => (host.make_link)(&work, &work),
        }
        let read = within.read(&old.join("a.hcl"), 1).map(|_| ());
        (read, within.make_dir(&work))
    });
    let replaced = (Err(Error::Replaced), Err(Error::Replaced));
    assert_eq!(got, [replaced, replaced], "file, loop");
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

pub(crate) fn within_checks_no_hidden_folder_past_its_limits(host: &Host<'_>) {
    let (within, base) = within_work(host);
    for name in ["none", "b.hcl", "workshop/a.hcl", "work/data/none"] {
        let hidden = [base.join(name)];
        let narrowed = within.within(&[base.join("work")], &hidden).map(|_| ());
        assert_eq!(narrowed, Ok(()), "{name}");
    }
}
