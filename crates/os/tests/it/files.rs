//! `os::files` on the real disk, through `env::files`.

use std::cell::Cell;
use std::future::poll_fn;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::task::Poll;

use block::{Block, Pool};
use env::files::{Error, File, Files, Mode, Operation};

use crate::disk::{KIB, Scratch, files, opened};

/// Runs `body` with the files of a scratch directory of its own and the path of
/// their data directory.
fn run<F: Future<Output = ()>>(body: impl FnOnce(Files, PathBuf) -> F) {
    let scratch = Scratch::new();
    let (files, thread) = files(&scratch.0, "files");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(body(files, scratch.0.join("data")));
    thread.join().unwrap();
}

fn pool() -> Pool {
    let config = block::Config { budget: 1 << 20 };
    let memory = block::Heap::new(config.reservation());
    Pool::new(config, memory)
}

fn block(pool: &Pool, bytes: &[u8]) -> Block {
    let mut unique = pool.alloc(bytes.len()).unwrap();
    unique.copy_from_slice(bytes);
    unique.freeze()
}

/// The `len` bytes of `file` at `offset`.
async fn read(file: &File, pool: &Pool, offset: u64, len: usize) -> Vec<u8> {
    let into = pool.alloc(len).unwrap();
    file.read_at(offset, into).await.unwrap().to_vec()
}

async fn create(files: &Files, path: &str, len: u64) -> File {
    let mode = Mode::Create { len };
    files.open(Path::new(path), mode).await.unwrap()
}

fn io(path: &str, operation: Operation, code: i32) -> Error {
    Error::Io {
        path: path.into(),
        operation,
        code,
    }
}

#[test]
fn a_read_sees_the_parts_of_a_write_back_to_back() {
    run(|files, _| async move {
        let pool = pool();
        let file = create(&files, "a", 4 * KIB).await;
        let parts = [block(&pool, b"abc"), block(&pool, b"defgh")];
        file.write_at(1_000, &parts).await.unwrap();
        assert_eq!(read(&file, &pool, 999, 10).await, b"\0abcdefgh\0");
    });
}

#[test]
fn a_write_of_more_parts_than_one_call_takes_keeps_their_order() {
    run(|files, _| async move {
        let pool = pool();
        let file = create(&files, "a", 8 * KIB).await;
        let bytes: Vec<[u8; 4]> = (0..1_500_u32).map(u32::to_le_bytes).collect();
        let parts: Vec<Block> = bytes.iter().map(|bytes| block(&pool, bytes)).collect();
        file.write_at(0, &parts).await.unwrap();
        assert_eq!(read(&file, &pool, 0, 6_000).await, bytes.concat());
    });
}

#[test]
fn a_dropped_write_ends_before_a_later_read() {
    run(|files, _| async move {
        let pool = pool();
        let file = create(&files, "a", 4 * KIB).await;
        {
            let parts = [block(&pool, b"kept")];
            let mut write = pin!(file.write_at(8, &parts));
            poll_fn(|context| {
                if let Poll::Ready(result) = write.as_mut().poll(context) {
                    result.unwrap();
                }
                Poll::Ready(())
            })
            .await;
        }
        assert_eq!(read(&file, &pool, 8, 4).await, b"kept");
    });
}

#[test]
fn create_makes_a_zeroed_file_of_its_length_in_the_data_directory() {
    run(|files, data| async move {
        let pool = pool();
        files.create_dir(Path::new("ring")).await.unwrap();
        let file = create(&files, "ring/0", 12 * KIB).await;
        assert_eq!(file.len(), 12 * KIB);
        assert_eq!(read(&file, &pool, 0, 12 * 1_024).await, vec![0; 12 * 1_024]);
        let found = std::fs::metadata(data.join("ring/0")).unwrap();
        assert_eq!(found.len(), 12 * KIB);
    });
}

#[test]
fn create_allocates_each_byte_of_the_file() {
    const LEN: u64 = 64 << 20;
    run(|files, data| async move {
        create(&files, "a", LEN).await.close().await;
        let allocated = std::fs::metadata(data.join("a")).unwrap().blocks() * 512;
        assert!(allocated >= LEN, "{allocated} of {LEN} bytes");
    });
}

#[test]
fn create_keeps_the_bytes_of_a_file_that_is_there() {
    run(|files, _| async move {
        let pool = pool();
        let file = create(&files, "a", 4 * KIB).await;
        file.write_at(0, &[block(&pool, b"old")]).await.unwrap();
        file.close().await;
        let file = create(&files, "a", 4 * KIB).await;
        assert_eq!(read(&file, &pool, 0, 3).await, b"old");
    });
}

#[test]
fn create_allocates_an_empty_file_that_is_there() {
    run(|files, data| async move {
        std::fs::write(data.join("a"), b"").unwrap();
        let file = create(&files, "a", 4 * KIB).await;
        assert_eq!(file.len(), 4 * KIB);
    });
}

#[cfg(target_os = "linux")]
#[test]
fn create_frees_the_blocks_past_the_end_of_an_empty_file_that_is_there() {
    run(|files, data| async move {
        crate::kept::check(&files, &data).await;
    });
}

#[test]
fn create_of_no_bytes_makes_an_empty_file() {
    run(|files, data| async move {
        let file = create(&files, "a", 0).await;
        assert_eq!(file.len(), 0);
        assert_eq!(std::fs::metadata(data.join("a")).unwrap().len(), 0);
    });
}

#[test]
fn create_of_another_length_gives_length() {
    run(|files, _| async move {
        create(&files, "a", 4 * KIB).await.close().await;
        let mode = Mode::Create { len: 8 * KIB };
        let found = files.open(Path::new("a"), mode).await.unwrap_err();
        let expected = Error::Length {
            path: "a".into(),
            expected: 8 * KIB,
            found: 4 * KIB,
        };
        assert_eq!(found, expected);
    });
}

#[cfg(target_os = "linux")]
#[test]
fn a_create_past_the_file_size_limit_leaves_no_file() {
    use rustix::process::{Resource, getrlimit};
    const LIMIT: u64 = 1 << 20;
    if getrlimit(Resource::Fsize).current != Some(LIMIT) {
        let thread = std::thread::current();
        let test = thread.name().expect("invariant: libtest names the thread");
        // The limit is on the whole process, so this test runs alone in a child. An
        // ignored `SIGXFSZ` stays ignored across `exec`, so it does not end the child.
        let script = r#"trap '' XFSZ && ulimit -f "$0" && exec "$1" --exact "$2""#;
        let status = std::process::Command::new("sh")
            .args(["-c", script])
            .arg((LIMIT / 512).to_string())
            .arg(std::env::current_exe().unwrap())
            .arg(test)
            .status()
            .unwrap();
        assert!(status.success(), "{status}");
        return;
    }
    run(|files, _| async move {
        let mode = Mode::Create { len: 8 * LIMIT };
        let found = files.open(Path::new("a"), mode).await.unwrap_err();
        assert_eq!(found, io("a", Operation::Open, libc::EFBIG));
        let found = files.open(Path::new("a"), Mode::Write).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "a".into() });
    });
}

#[test]
fn an_open_of_a_missing_file_gives_not_found() {
    run(|files, _| async move {
        for mode in [Mode::Read, Mode::Write] {
            let found = files.open(Path::new("a"), mode).await.unwrap_err();
            assert_eq!(found, Error::NotFound { path: "a".into() });
        }
        let found = files.open(Path::new("b/a"), Mode::Create { len: KIB });
        let expected = Error::NotFound { path: "b/a".into() };
        assert_eq!(found.await.unwrap_err(), expected);
    });
}

#[test]
fn a_write_open_of_a_held_file_gives_busy_and_a_read_open_does_not() {
    run(|files, data| async move {
        let held = create(&files, "a", 4 * KIB).await;
        let (other, thread) = self::files(data.parent().unwrap(), "other");
        for files in [&files, &other] {
            for mode in [Mode::Write, Mode::Create { len: 4 * KIB }] {
                let found = files.open(Path::new("a"), mode).await.unwrap_err();
                assert_eq!(found, Error::Busy { path: "a".into() });
            }
            files.open(Path::new("a"), Mode::Read).await.unwrap();
        }
        drop((held, other));
        thread.join().unwrap();
    });
}

#[test]
fn a_write_open_after_a_close_or_a_drop_of_the_holder_succeeds() {
    run(|files, _| async move {
        create(&files, "a", 4 * KIB).await.close().await;
        let file = files.open(Path::new("a"), Mode::Write).await.unwrap();
        drop(file);
        files.open(Path::new("a"), Mode::Write).await.unwrap();
    });
}

#[test]
fn a_rename_moves_the_file_and_the_handle_follows_it() {
    run(|files, _| async move {
        let pool = pool();
        files.create_dir(Path::new("d")).await.unwrap();
        let mut file = create(&files, "d/a", 4 * KIB).await;
        file.write_at(0, &[block(&pool, b"one")]).await.unwrap();
        file.rename(Path::new("d/b")).await.unwrap();
        let found = files.open(Path::new("d/a"), Mode::Read).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "d/a".into() });
        file.write_at(3, &[block(&pool, b"two")]).await.unwrap();
        let moved = files.open(Path::new("d/b"), Mode::Read).await.unwrap();
        assert_eq!(read(&moved, &pool, 0, 6).await, b"onetwo");
    });
}

#[test]
fn a_rename_to_a_taken_name_spelled_with_a_dot_gives_exists() {
    run(|files, _| async move {
        drop(create(&files, "b", 4 * KIB).await);
        let mut file = create(&files, "a", 4 * KIB).await;
        let found = file.rename(Path::new("./b")).await.unwrap_err();
        assert_eq!(found, Error::Exists { path: "./b".into() });
        let names = files.list(Path::new("")).await.unwrap();
        assert_eq!(names, [PathBuf::from("a"), PathBuf::from("b")]);
    });
}

#[test]
fn a_path_with_a_trailing_slash_names_only_a_directory() {
    run(|files, _| async move {
        drop(create(&files, "a", 4 * KIB).await);
        let mut results = Vec::new();
        for (path, mode) in [
            ("a/", Mode::Write),
            ("a/.", Mode::Read),
            ("a//", Mode::Read),
            ("a/", Mode::Create { len: 4 * KIB }),
            ("b/", Mode::Create { len: 4 * KIB }),
            ("b/", Mode::Read),
            ("b/.", Mode::Read),
        ] {
            results.push(files.open(Path::new(path), mode).await.map(drop));
        }
        results.push(files.remove(Path::new("a/")).await);
        let names = files.list(Path::new("")).await.unwrap();
        assert_eq!(
            names,
            [PathBuf::from("a")],
            "a refused remove keeps the file"
        );
        for path in ["b/", "a"] {
            results.push(files.remove(Path::new(path)).await);
        }
        let expected = [
            Err(io("a/", Operation::Open, 20)),
            Err(io("a/.", Operation::Open, 20)),
            Err(io("a//", Operation::Open, 20)),
            Err(io("a/", Operation::Open, 21)),
            Err(io("b/", Operation::Open, 21)),
            Err(Error::NotFound { path: "b/".into() }),
            Err(Error::NotFound { path: "b/.".into() }),
            Err(io("a/", Operation::Remove, 20)),
            Ok(()),
            Ok(()),
        ];
        assert_eq!(results, expected);
    });
}

#[test]
fn a_path_of_the_data_directory_names_no_file() {
    run(|files, _| async move {
        let mut results = Vec::new();
        for (path, mode) in [
            ("./", Mode::Read),
            ("./", Mode::Write),
            (".", Mode::Read),
            (".", Mode::Create { len: 1 }),
        ] {
            results.push(files.open(Path::new(path), mode).await.map(drop));
        }
        results.push(files.remove(Path::new(".")).await);
        let expected = [
            Err(io("./", Operation::Open, 21)),
            Err(io("./", Operation::Open, 21)),
            Err(io(".", Operation::Open, 21)),
            Err(io(".", Operation::Open, 21)),
            Err(io(".", Operation::Remove, 21)),
        ];
        assert_eq!(results, expected);
    });
}

#[test]
fn an_empty_path_names_no_file() {
    run(|files, _| async move {
        let mut results = Vec::new();
        for mode in [Mode::Read, Mode::Write, Mode::Create { len: 4 * KIB }] {
            results.push(files.open(Path::new(""), mode).await.map(drop));
        }
        results.push(files.remove(Path::new("")).await);
        let not_found = || Error::NotFound { path: "".into() };
        let expected = [Err(not_found()), Err(not_found()), Err(not_found()), Ok(())];
        assert_eq!(results, expected);
        assert_eq!(
            files.list(Path::new("")).await.unwrap(),
            Vec::<PathBuf>::new()
        );
    });
}

#[test]
fn a_rename_onto_a_file_that_is_there_gives_exists_and_changes_nothing() {
    run(|files, _| async move {
        let pool = pool();
        let mut file = create(&files, "a", 4 * KIB).await;
        file.write_at(0, &[block(&pool, b"one")]).await.unwrap();
        let other = create(&files, "b", 4 * KIB).await;
        other.write_at(0, &[block(&pool, b"two")]).await.unwrap();
        other.close().await;
        let found = file.rename(Path::new("b")).await.unwrap_err();
        assert_eq!(found, Error::Exists { path: "b".into() });
        for (path, bytes) in [("a", b"one"), ("b", b"two")] {
            let kept = files.open(Path::new(path), Mode::Read).await.unwrap();
            assert_eq!(read(&kept, &pool, 0, 3).await, bytes);
        }
        file.rename(Path::new("c")).await.unwrap();
        let moved = files.open(Path::new("c"), Mode::Read).await.unwrap();
        assert_eq!(read(&moved, &pool, 0, 3).await, b"one");
    });
}

#[test]
fn a_rename_of_a_removed_path_gives_not_found_and_changes_nothing() {
    run(|files, _| async move {
        let mut file = create(&files, "a", 4 * KIB).await;
        files.remove(Path::new("a")).await.unwrap();
        let found = file.rename(Path::new("b")).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "a".into() });
        let found = files.open(Path::new("b"), Mode::Read).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "b".into() });
    });
}

#[test]
fn a_rename_of_a_path_that_names_another_file_gives_not_found() {
    run(|files, _| async move {
        let pool = pool();
        let mut file = create(&files, "a", 4 * KIB).await;
        files.remove(Path::new("a")).await.unwrap();
        let other = create(&files, "a", 4 * KIB).await;
        other.write_at(0, &[block(&pool, b"new")]).await.unwrap();
        let found = file.rename(Path::new("b")).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "a".into() });
        let kept = files.open(Path::new("a"), Mode::Read).await.unwrap();
        assert_eq!(read(&kept, &pool, 0, 3).await, b"new");
        let found = files.open(Path::new("b"), Mode::Read).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "b".into() });
    });
}

#[test]
fn a_rename_of_a_path_that_is_a_link_to_the_file_gives_not_found() {
    run(|files, data| async move {
        std::fs::write(data.join("t"), [0; 4_096]).unwrap();
        std::os::unix::fs::symlink("t", data.join("a")).unwrap();
        let mut file = files.open(Path::new("a"), Mode::Write).await.unwrap();
        let found = file.rename(Path::new("b")).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "a".into() });
        assert!(data.join("t").is_file() && data.join("a").is_symlink());
        assert!(!data.join("b").exists());
    });
}

#[test]
fn a_write_open_of_the_new_name_gives_busy_until_the_handle_closes() {
    run(|files, data| async move {
        let mut file = create(&files, "a", 4 * KIB).await;
        file.rename(Path::new("b")).await.unwrap();
        let (other, thread) = self::files(data.parent().unwrap(), "other");
        for files in [&files, &other] {
            let found = files.open(Path::new("b"), Mode::Write).await.unwrap_err();
            assert_eq!(found, Error::Busy { path: "b".into() });
        }
        file.close().await;
        files.open(Path::new("b"), Mode::Write).await.unwrap();
        drop(other);
        thread.join().unwrap();
    });
}

#[test]
fn an_error_after_a_rename_names_the_new_path() {
    run(|files, data| async move {
        let pool = pool();
        let mut file = create(&files, "a", 4 * KIB).await;
        file.rename(Path::new("b")).await.unwrap();
        std::fs::write(data.join("b"), [7; 3_000]).unwrap();
        let found = file.read_at(2_000, pool.alloc(2_000).unwrap()).await;
        assert_eq!(found.unwrap_err(), io("b", Operation::ReadAt, 5));
    });
}

#[test]
fn a_read_past_the_end_of_a_file_cut_short_gives_eio() {
    run(|files, data| async move {
        let pool = pool();
        let file = create(&files, "a", 4 * KIB).await;
        std::fs::write(data.join("a"), [7; 3_000]).unwrap();
        let found = file.read_at(2_000, pool.alloc(2_000).unwrap()).await;
        assert_eq!(found.unwrap_err(), io("a", Operation::ReadAt, 5));
    });
}

#[test]
fn an_open_of_a_directory_gives_eisdir() {
    run(|files, _| async move {
        files.create_dir(Path::new("d")).await.unwrap();
        for mode in [Mode::Read, Mode::Write, Mode::Create { len: KIB }] {
            let found = files.open(Path::new("d"), mode).await.unwrap_err();
            assert_eq!(found, io("d", Operation::Open, 21));
        }
    });
}

#[test]
fn list_gives_the_names_in_a_directory() {
    run(|files, _| async move {
        files.create_dir(Path::new("d")).await.unwrap();
        create(&files, "d/b", KIB).await;
        create(&files, "d/a", KIB).await;
        files.create_dir(Path::new("d/c")).await.unwrap();
        create(&files, "d/c/x", KIB).await;
        let names = files.list(Path::new("d")).await.unwrap();
        assert_eq!(names, [PathBuf::from("a"), "b".into(), "c".into()]);
        assert_eq!(
            files.list(Path::new("")).await.unwrap(),
            [PathBuf::from("d")]
        );
        let found = files.list(Path::new("e")).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "e".into() });
    });
}

#[test]
fn create_dir_of_a_directory_that_is_there_succeeds() {
    run(|files, _| async move {
        files.create_dir(Path::new("d")).await.unwrap();
        files.create_dir(Path::new("d")).await.unwrap();
        assert_eq!(
            files.list(Path::new("")).await.unwrap(),
            [PathBuf::from("d")]
        );
    });
}

#[test]
fn create_dir_over_a_file_or_in_a_missing_parent_fails() {
    run(|files, _| async move {
        create(&files, "a", KIB).await;
        let found = files.create_dir(Path::new("a")).await.unwrap_err();
        assert_eq!(found, io("a", Operation::CreateDir, 17));
        let found = files.create_dir(Path::new("b/c")).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "b/c".into() });
    });
}

#[test]
fn remove_removes_a_file() {
    run(|files, _| async move {
        create(&files, "a", KIB).await.close().await;
        files.remove(Path::new("a")).await.unwrap();
        assert!(files.list(Path::new("")).await.unwrap().is_empty());
        files.remove(Path::new("a")).await.unwrap();
    });
}

#[test]
fn a_remove_through_the_handle_removes_the_file_and_a_create_at_its_path_opens() {
    run(|files, _| async move {
        let pool = pool();
        let file = create(&files, "a", 4 * KIB).await;
        file.write_at(0, &[block(&pool, b"old")]).await.unwrap();
        assert_eq!(file.remove().await, Ok(()));
        assert!(files.list(Path::new("")).await.unwrap().is_empty());
        let found = files.open(Path::new("a"), Mode::Write).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "a".into() });
        let made = create(&files, "a", KIB).await;
        assert_eq!(read(&made, &pool, 0, 3).await, [0; 3]);
    });
}

#[test]
fn a_remove_of_a_path_that_names_another_file_gives_not_found_and_keeps_it() {
    run(|files, _| async move {
        let pool = pool();
        let file = create(&files, "a", 4 * KIB).await;
        files.remove(Path::new("a")).await.unwrap();
        let other = create(&files, "a", 4 * KIB).await;
        other.write_at(0, &[block(&pool, b"new")]).await.unwrap();
        let found = file.remove().await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "a".into() });
        let kept = files.open(Path::new("a"), Mode::Read).await.unwrap();
        assert_eq!(read(&kept, &pool, 0, 3).await, b"new");
    });
}

#[test]
fn a_remove_of_a_path_that_is_a_link_to_the_file_gives_not_found() {
    run(|files, data| async move {
        std::fs::write(data.join("t"), [0; 4_096]).unwrap();
        std::os::unix::fs::symlink("t", data.join("a")).unwrap();
        let file = files.open(Path::new("a"), Mode::Write).await.unwrap();
        let found = file.remove().await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "a".into() });
        assert!(data.join("t").is_file() && data.join("a").is_symlink());
        files.open(Path::new("t"), Mode::Write).await.unwrap();
    });
}

#[test]
fn a_remove_through_the_handle_releases_the_lock_of_the_file() {
    run(|files, data| async move {
        let file = create(&files, "a", 4 * KIB).await;
        std::fs::hard_link(data.join("a"), data.join("b")).unwrap();
        let (other, thread) = self::files(data.parent().unwrap(), "other");
        for files in [&files, &other] {
            let found = files.open(Path::new("b"), Mode::Write).await.unwrap_err();
            assert_eq!(found, Error::Busy { path: "b".into() });
        }
        file.remove().await.unwrap();
        assert!(!data.join("a").exists() && data.join("b").is_file());
        other.open(Path::new("b"), Mode::Write).await.unwrap();
        drop(other);
        thread.join().unwrap();
    });
}

#[test]
#[should_panic(expected = "remove a, which was opened to read")]
fn a_remove_through_a_read_handle_panics() {
    run(|files, _| async move {
        create(&files, "a", KIB).await.close().await;
        let file = files.open(Path::new("a"), Mode::Read).await.unwrap();
        drop(file.remove().await);
    });
}

#[test]
fn sync_dir_syncs_a_directory_that_is_there() {
    run(|files, _| async move {
        files.create_dir(Path::new("d")).await.unwrap();
        files.sync_dir(Path::new("d")).await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
        let found = files.sync_dir(Path::new("e")).await.unwrap_err();
        assert_eq!(found, Error::NotFound { path: "e".into() });
    });
}

#[test]
fn a_sync_after_a_write_succeeds() {
    run(|files, _| async move {
        let pool = pool();
        let file = create(&files, "a", 4 * KIB).await;
        file.write_at(0, &[block(&pool, b"x")]).await.unwrap();
        file.sync().await.unwrap();
    });
}

#[test]
fn free_drops_by_the_bytes_of_a_created_file() {
    const LEN: u64 = 64 << 20;
    // Other jobs on the host write to the same disk, and an attempt counts their bytes
    // too.
    const ATTEMPTS: usize = 8;
    run(|files, _| async move {
        let mut seen = Vec::new();
        for attempt in 0..ATTEMPTS {
            let path = attempt.to_string();
            let before = files.free().await.unwrap();
            let file = create(&files, &path, LEN).await;
            let after = files.free().await.unwrap();
            file.close().await;
            let taken = before.saturating_sub(after);
            if taken.abs_diff(LEN) < LEN / 4 {
                return;
            }
            files.remove(Path::new(&path)).await.unwrap();
            seen.push(taken);
        }
        panic!("{seen:?} are not {LEN}");
    });
}

/// The error of `os::files` on `dir`.
#[test]
fn a_disk_shows_as_disk() {
    let scratch = Scratch::new();
    let (disk, thread) =
        os::files(&scratch.0, &os::threads().unwrap(), "files").unwrap();
    assert_eq!(format!("{disk:?}"), "Disk { .. }");
    drop(disk);
    thread.join().unwrap();
}

fn dir_error(dir: &Path) -> os::Error {
    os::files(dir, &os::threads().unwrap(), "files").unwrap_err()
}

#[test]
fn files_of_a_missing_dir_gives_dir_and_makes_nothing() {
    let scratch = Scratch::new();
    let found = dir_error(&scratch.0.join("a"));
    assert_eq!(
        found.to_string(),
        "cannot open the data directory: No such file or directory (os error 2)"
    );
    assert!(matches!(found, os::Error::Dir(_)));
    assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0);
}

#[test]
fn files_in_a_directory_under_a_file_gives_dir() {
    let scratch = Scratch::new();
    std::fs::write(scratch.0.join("a"), b"").unwrap();
    let found = dir_error(&scratch.0.join("a"));
    assert_eq!(
        found.to_string(),
        "cannot open the data directory: Not a directory (os error 20)"
    );
    assert!(matches!(found, os::Error::Dir(_)));
}

#[test]
fn a_write_open_while_another_create_fails_holds_the_file_at_its_path() {
    hold_while_another_create_fails(Mode::Write);
}

#[test]
fn a_create_while_another_create_fails_holds_the_file_at_its_path() {
    hold_while_another_create_fails(Mode::Create { len: 4 * KIB });
}

/// Opens `a` with `mode` while another handle's creates fail and unlink it, and
/// checks that the open holds the file at its path.
fn hold_while_another_create_fails(mode: Mode) {
    let scratch = Scratch::new();
    let (creator, creator_thread) = files(&scratch.0, "creator");
    let (writer, writer_thread) = files(&scratch.0, "writer");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        for _ in 0..500 {
            let stopped = Cell::new(false);
            // Fails with no allocation, so each create unlinks the file it made.
            let mut create = pin!(async {
                while !stopped.get() {
                    let mode = Mode::Create { len: u64::MAX };
                    creator.open(Path::new("a"), mode).await.unwrap_err();
                }
            });
            let mut write = pin!(async {
                loop {
                    if let Ok(file) = writer.open(Path::new("a"), mode).await {
                        stopped.set(true);
                        return file;
                    }
                }
            });
            let mut file = None;
            poll_fn(|context| {
                if file.is_none()
                    && let Poll::Ready(found) = write.as_mut().poll(context)
                {
                    file = Some(found);
                }
                match (file.is_some(), create.as_mut().poll(context)) {
                    (true, Poll::Ready(())) => Poll::Ready(()),
                    _ => Poll::Pending,
                }
            })
            .await;
            let found = writer.open(Path::new("a"), Mode::Write).await.unwrap_err();
            assert_eq!(found, Error::Busy { path: "a".into() });
            file.unwrap().close().await;
            writer.remove(Path::new("a")).await.unwrap();
        }
    });
    drop((creator, writer));
    creator_thread.join().unwrap();
    writer_thread.join().unwrap();
}

/// A remove that drops while it waits for room in the full queue of the I/O thread
/// never runs.
#[cfg(target_os = "linux")]
#[test]
fn a_remove_that_drops_while_it_waits_for_room_leaves_the_file() {
    run(|files, data| async move {
        create(&files, "a", KIB).await.close().await;
        let mode = rustix::fs::Mode::from_raw_mode(0o600);
        rustix::fs::mkfifoat(rustix::fs::CWD, data.join("p"), mode).unwrap();
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        // The I/O thread blocks in the open of the FIFO until a writer opens it.
        let mut blocker = Box::pin(files.open(Path::new("p"), Mode::Read));
        assert!(blocker.as_mut().poll(&mut context).is_pending());
        // 64 is the depth of the queue of the I/O thread.
        let mut frees: Vec<_> = (0..64).map(|_| Box::pin(files.free())).collect();
        for free in &mut frees {
            assert!(free.as_mut().poll(&mut context).is_pending());
        }
        let mut remove = Box::pin(files.remove(Path::new("a")));
        assert!(remove.as_mut().poll(&mut context).is_pending());
        drop(remove);
        let flags = rustix::fs::OFlags::WRONLY;
        let writer = rustix::fs::open(data.join("p"), flags, mode).unwrap();
        drop(blocker.await.unwrap());
        drop(writer);
        for free in frees {
            free.await.unwrap();
        }
        files.free().await.unwrap();
        let found = files.open(Path::new("a"), Mode::Read).await.map(drop);
        assert_eq!(found, Ok(()));
    });
}

#[test]
fn a_file_or_directory_that_is_there_keeps_its_mode() {
    let scratch = Scratch::new();
    let data = scratch.0.join("data");
    std::fs::create_dir_all(data.join("d")).unwrap();
    let len = usize::try_from(KIB).unwrap();
    std::fs::write(data.join("f"), vec![0; len]).unwrap();
    for (path, mode) in [("", 0o755), ("f", 0o644), ("d", 0o755)] {
        let mode = std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(data.join(path), mode).unwrap();
    }
    assert_eq!(opened(&scratch.0), [0o755, 0o644, 0o755]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_new_directory_takes_the_setgid_bit_of_its_parent() {
    let scratch = Scratch::new();
    let mode = std::fs::Permissions::from_mode(0o2750);
    std::fs::set_permissions(&scratch.0, mode).unwrap();
    assert_eq!(opened(&scratch.0), [0o2700, 0o600, 0o2700]);
}
