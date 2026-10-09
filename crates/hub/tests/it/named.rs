//! Named reader sessions: one session for each subject and name.

use std::pin::pin;
use std::sync::atomic::Ordering;

use hub::reader::{self, Ended, Mode};
use types::channel::Key;
use types::name::Selector;
use types::time::Span;

use super::{
    SETTLE, name, poll_flagged, run, samples, unnamed, unsynced, without, write,
};

/// A reader of `subject` named `reader` on `value`, with a hold of `hold`.
fn named(subject: &str, reader: &str, mode: Mode, hold: Span) -> reader::Config {
    reader::Config {
        select: Selector::new(["value"]).expect("a selector"),
        mode,
        subject: name(subject),
        name: Some(name(reader)),
        hold,
    }
}

#[test]
fn ends_the_session_of_a_named_reader_that_a_later_open_takes_over() {
    for (seed, (first, second)) in (0..).zip([
        (Mode::Complete, Mode::Complete),
        (Mode::Complete, Mode::Latest),
        (Mode::Latest, Mode::Complete),
        (Mode::Latest, Mode::Latest),
    ]) {
        run(seed, move |test| async move {
            let open = named("a", "r", first, Span::ZERO);
            let mut replaced = test.hub.reader(open).await.expect("opens");
            let open = named("a", "r", second, Span::ZERO);
            let mut reader = test.hub.reader(open).await.expect("opens");
            let mut writer = test.writer("w", &["value"]).await;
            let now = test.now();
            write(&mut writer, &[now], &[7]);
            let ended = replaced.next().await.expect_err("replaced");
            assert_eq!(ended, Ended::Replaced);
            assert_eq!(
                ended.to_string(),
                "a later open of the same named reader took over the session"
            );
            let ended = replaced.next().await.expect_err("replaced");
            assert_eq!(ended, Ended::Replaced, "on each later call");
            let received = reader.next().await.expect("a frame");
            assert_eq!(samples(&received, 2), [7]);
        });
    }
}

#[test]
fn wakes_a_named_reader_that_waits_when_a_later_open_takes_it_over() {
    run(4, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut replaced = test.hub.reader(open).await.expect("opens");
        let mut next = pin!(replaced.next());
        let (polled, woken) = poll_flagged(next.as_mut());
        assert!(polled.is_pending(), "no frame waits");
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let _reader = test.hub.reader(open).await.expect("opens");
        assert!(woken.0.load(Ordering::Relaxed), "the open wakes the reader");
        assert_eq!(next.await.expect_err("replaced"), Ended::Replaced);
    });
}

#[test]
fn ends_a_replaced_reader_with_replaced_before_the_frames_that_wait() {
    run(13, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::ZERO);
        let mut replaced = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        test.clock.sleep(SETTLE).await;
        let open = named("a", "r", Mode::Latest, Span::ZERO);
        let _reader = test.hub.reader(open).await.expect("opens");
        let ended = replaced.next().await.expect_err("replaced");
        assert_eq!(ended, Ended::Replaced);
    });
}

#[test]
fn opens_a_named_reader_on_each_index_on_its_own() {
    run(14, |test| async move {
        let open = named("a", "r", Mode::Latest, Span::ZERO);
        let mut first = test.hub.reader(open).await.expect("opens");
        let mut open = named("a", "r", Mode::Latest, Span::ZERO);
        open.select = Selector::new(["value-b"]).expect("a selector");
        let _other = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        let received = first.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [7]);
    });
}

#[test]
fn ends_a_replaced_reader_with_replaced_after_a_removal_of_its_channel() {
    run(10, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::ZERO);
        let mut replaced = test.hub.reader(open).await.expect("opens");
        let open = named("a", "r", Mode::Latest, Span::ZERO);
        let mut reader = test.hub.reader(open).await.expect("opens");
        test.hub.set_definitions(&without(&["value"]));
        let ended = replaced.next().await.expect_err("replaced");
        assert_eq!(ended, Ended::Replaced);
        let ended = reader.next().await.expect_err("removed");
        assert_eq!(ended, Ended::Removed(Key::from_u128(2)));
    });
}

#[test]
fn ends_a_named_complete_reader_behind_when_it_resumes_before_a_released_frame() {
    for (seed, closed) in [(11, false), (12, true)] {
        run(seed, move |test| async move {
            let open = named("a", "r", Mode::Complete, Span::SECOND);
            let mut first = test.hub.reader(open).await.expect("opens");
            let mut writer = test.writer("w", &["value"]).await;
            let now = test.now();
            write(&mut writer, &[now], &[7]);
            let received = first.next().await.expect("a frame");
            assert_eq!(samples(&received, 2), [7]);
            let first = (!closed).then_some(first);
            let open = named("a", "r", Mode::Complete, Span::SECOND);
            let mut reader = test.hub.reader(open).await.expect("opens");
            write(&mut writer, &[now + 1], &[8]);
            let ended = reader.next().await.expect_err("behind");
            assert_eq!(ended, Ended::Behind, "closed: {closed}");
            drop(first);
        });
    }
}

#[test]
fn opens_a_named_complete_reader_at_the_live_frames_once_its_hold_ends() {
    run(15, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut first = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        first.next().await.expect("a frame");
        drop(first);
        test.clock.sleep(Span::SECOND).await;
        test.clock.sleep(Span::SECOND).await;
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        write(&mut writer, &[test.now()], &[8]);
        let received = reader.next().await.expect("a frame after the hold");
        assert_eq!(samples(&received, 2), [8]);
    });
}

#[test]
fn opens_a_named_reader_of_each_subject_and_name_on_its_own() {
    run(5, |test| async move {
        let mut readers = Vec::new();
        for (subject, reader) in [("a", "r"), ("b", "r"), ("a", "s")] {
            let open = named(subject, reader, Mode::Complete, Span::ZERO);
            readers.push(test.hub.reader(open).await.expect("opens"));
        }
        let open = unnamed(&["value"], Mode::Complete);
        readers.push(test.hub.reader(open).await.expect("opens"));
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[7]);
        for reader in &mut readers {
            let received = reader.next().await.expect("a frame");
            assert_eq!(samples(&received, 2), [7]);
        }
    });
}

#[test]
fn opens_no_named_reader_before_mesh_time() {
    unsynced(6, |test| async move {
        for mode in [Mode::Complete, Mode::Latest] {
            let open = test.hub.reader(named("a", "r", mode, Span::ZERO)).await;
            let error = open.expect_err("no mesh time");
            assert_eq!(error, reader::Error::Unsynced);
            assert_eq!(
                error.to_string(),
                "the node has no mesh time yet: open the named reader again later"
            );
        }
        let open = test.hub.reader(unnamed(&["value"], Mode::Latest)).await;
        assert!(open.is_ok(), "an unnamed reader opens");
    });
}

#[test]
#[should_panic(expected = "a hold of 1s for a reader that is unnamed or latest")]
fn panics_on_a_hold_for_an_unnamed_reader() {
    run(7, |test| async move {
        let mut open = unnamed(&["value"], Mode::Complete);
        open.hold = Span::SECOND;
        drop(test.hub.reader(open).await);
    });
}

#[test]
#[should_panic(expected = "a hold of 1s for a reader that is unnamed or latest")]
fn panics_on_a_hold_for_a_latest_reader() {
    run(8, |test| async move {
        let open = named("a", "r", Mode::Latest, Span::SECOND);
        drop(test.hub.reader(open).await);
    });
}

#[test]
#[should_panic(expected = "the hold -1s of a reader is negative")]
fn panics_on_a_negative_hold() {
    run(9, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::from_nanos(-1_000_000_000));
        drop(test.hub.reader(open).await);
    });
}
