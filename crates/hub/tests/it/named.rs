//! Named reader sessions: one session for each subject and name.

use std::path::Path as FilePath;
use std::pin::pin;
use std::sync::atomic::Ordering;
use std::task::Poll;

use env::files::Operation;
use hub::reader::{self, Ended, Mode};
use types::channel::Key;
use types::name::Selector;
use types::time::Span;

use super::{
    RING, SETTLE, name, poll_flagged, run, samples, unnamed, unsynced, without, write,
    write_series, write_wide,
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
fn resumes_a_named_complete_reader_past_a_frame_released_before_its_position() {
    run(16, |test| async move {
        let mut keep = test
            .hub
            .reader(unnamed(&["value"], Mode::Complete))
            .await
            .expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let first = test.hub.reader(open).await.expect("opens");
        let received = keep.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [7]);
        test.clock.sleep(SETTLE).await;
        drop(first);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        write(&mut writer, &[test.now()], &[8]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [8]);
    });
}

#[test]
fn ends_a_named_complete_reader_behind_on_a_frame_dropped_before_it_resumes() {
    for (seed, queued) in [(20, true), (21, false)] {
        run(seed, move |test| async move {
            let open = named("a", "r", Mode::Complete, Span::SECOND);
            let first = test.hub.reader(open).await.expect("opens");
            let mut writer = test.writer("w", &["value"]).await;
            test.clock.sleep(SETTLE).await;
            test.paused.pause();
            if queued {
                write(&mut writer, &[test.now()], &[7]);
                drop(first);
            } else {
                drop(first);
                write(&mut writer, &[test.now()], &[7]);
            }
            let open = named("a", "r", Mode::Complete, Span::SECOND);
            let mut reader = test.hub.reader(open).await.expect("opens");
            test.paused.resume();
            let ended = reader.next().await.expect_err("behind");
            assert_eq!(ended, Ended::Behind, "queued: {queued}");
        });
    }
}

#[test]
fn resumes_a_named_complete_reader_past_a_frame_lost_while_held() {
    run(22, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let first = test.hub.reader(open).await.expect("opens");
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        let mut writer = test.writer("w", &["value"]).await;
        test.clock.sleep(SETTLE).await;
        drop(first);
        // A frame that the pool cannot freeze never reaches the readers. Frees one
        // block at a time until a write freezes the frame and still loses it, which
        // the latest reader shows.
        let mut blocks = super::fill(&test.pool);
        let lost = loop {
            drop(blocks.pop().expect("a write that freezes its frame"));
            let draft = super::draft(&writer, &[(1, &[test.now()]), (2, &[7])]);
            let outcomes = writer.write(super::LIVE, draft).expect("taken").to_vec();
            assert!(
                matches!(outcomes[..], [hub::home::Outcome::Lost { .. }]),
                "{outcomes:?}"
            );
            if let Poll::Ready(received) = super::poll_once(latest.next()) {
                break received.expect("a frame");
            }
        };
        assert_eq!(samples(&lost, 2), [7]);
        drop(blocks);
        test.clock.sleep(SETTLE).await;
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        write(&mut writer, &[test.now()], &[8]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [8]);
    });
}

#[test]
fn resumes_each_later_open_of_a_named_complete_reader_at_the_same_position() {
    run(17, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut first = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        first.next().await.expect("a frame");
        test.clock.sleep(SETTLE).await;
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let second = test.hub.reader(open).await.expect("opens");
        test.clock.sleep(SETTLE).await;
        drop(second);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        write(&mut writer, &[test.now()], &[8]);
        assert_eq!(reader.next().await.expect_err("behind"), Ended::Behind);
    });
}

#[test]
fn holds_a_named_complete_reader_for_its_hold_and_no_longer() {
    for (seed, slept, held) in [
        (15, Span::from_nanos(999_999_999), true),
        (18, Span::SECOND, false),
    ] {
        run(seed, move |test| async move {
            let open = named("a", "r", Mode::Complete, Span::SECOND);
            let mut first = test.hub.reader(open).await.expect("opens");
            let mut writer = test.writer("w", &["value"]).await;
            write(&mut writer, &[test.now()], &[7]);
            first.next().await.expect("a frame");
            drop(first);
            test.clock.sleep(slept).await;
            let open = named("a", "r", Mode::Complete, Span::SECOND);
            let mut reader = test.hub.reader(open).await.expect("opens");
            write(&mut writer, &[test.now()], &[8]);
            let next = reader.next().await;
            if held {
                assert_eq!(next.expect_err("behind"), Ended::Behind);
            } else {
                assert_eq!(samples(&next.expect("a frame"), 2), [8]);
            }
        });
    }
}

#[test]
fn ends_a_named_complete_reader_behind_when_it_resumes_at_an_ack_before_a_frame() {
    run(23, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut first = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        let mut acked = None;
        for n in 0..10 {
            write(&mut writer, &[now + n], &[n]);
            let received = first.next().await.expect("a frame");
            assert_eq!(samples(&received, 2), [n]);
            if n == 4 {
                acked = Some(received.position());
            }
        }
        first.ack(acked.expect("a fifth frame"));
        drop(first);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        assert_eq!(reader.next().await.expect_err("behind"), Ended::Behind);
    });
}

#[test]
fn changes_nothing_on_the_ack_of_a_reader_that_ended_behind() {
    run(31, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut lagger = test.hub.reader(open).await.expect("opens");
        let mut taker = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        let mut position = None;
        for n in 0..400 {
            write_wide(&mut writer, now, n);
            taker.next().await.expect("a frame");
            if n < 2 {
                position = Some(lagger.next().await.expect("a frame").position());
            }
        }
        let ended = loop {
            match lagger.next().await {
                Ok(received) => position = Some(received.position()),
                Err(ended) => break ended,
            }
        };
        assert_eq!(ended, Ended::Behind);
        lagger.ack(position.expect("a frame"));
        drop(lagger);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        assert_eq!(reader.next().await.expect_err("behind"), Ended::Behind);
    });
}

#[test]
fn resumes_a_named_complete_reader_that_acked_each_frame_at_each_later_frame() {
    run(24, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut first = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        for n in 0..3 {
            write(&mut writer, &[now + n], &[n]);
            let position = first.next().await.expect("a frame").position();
            first.ack(position);
        }
        drop(first);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        for n in 3..6 {
            write(&mut writer, &[now + n], &[n]);
        }
        for n in 3..6 {
            let received = reader.next().await.expect("a frame");
            assert_eq!(samples(&received, 2), [n]);
        }
    });
}

#[test]
fn changes_nothing_on_an_ack_below_the_last() {
    run(25, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now, now + 1], &[7, 8]);
        write(&mut writer, &[now + 2], &[9]);
        let first = reader.next().await.expect("a frame").position();
        let second = reader.next().await.expect("a frame").position();
        reader.ack(second);
        reader.ack(first);
        drop(reader);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        write(&mut writer, &[now + 3], &[10]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [10]);
    });
}

#[test]
fn opens_a_named_complete_reader_at_the_live_tail_once_its_hold_ends() {
    run(26, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut first = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        let position = first.next().await.expect("a frame").position();
        first.ack(position);
        drop(first);
        test.clock.sleep(Span::SECOND).await;
        write(&mut writer, &[test.now()], &[8]);
        test.clock.sleep(SETTLE).await;
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        write(&mut writer, &[test.now()], &[9]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [9]);
    });
}

#[test]
fn changes_nothing_on_the_ack_of_a_latest_reader() {
    run(27, |test| async move {
        let open = named("a", "r", Mode::Latest, Span::ZERO);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[7]);
        let first = reader.next().await.expect("a frame").position();
        write(&mut writer, &[now + 1], &[8]);
        let second = reader.next().await.expect("a frame").position();
        reader.ack(second);
        reader.ack(first);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        write(&mut writer, &[now + 2], &[9]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [9]);
    });
}

#[test]
fn changes_nothing_on_the_ack_of_a_reader_that_ended_on_a_failed_sync() {
    run(42, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[7]);
        write(&mut writer, &[now + 1], &[8]);
        test.clock.sleep(SETTLE).await;
        let first = reader.next().await.expect("a frame").position();
        reader.next().await.expect("a frame");
        test.node.fail_file(FilePath::new(RING), Operation::Sync);
        write(&mut writer, &[now + 2], &[9]);
        test.clock.sleep(SETTLE).await;
        let ended = reader.next().await.expect_err("the sync failed");
        assert!(matches!(ended, Ended::Buffer(_)), "{ended:?}");
        reader.ack(first);
        drop(reader);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        assert_eq!(reader.next().await.expect_err("behind"), Ended::Behind);
    });
}

#[test]
fn changes_nothing_on_an_ack_after_a_failed_sync_of_another_index() {
    run(44, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        let mut other = test.writer("o", &["value-b"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[7]);
        write(&mut writer, &[now + 1], &[8]);
        test.clock.sleep(SETTLE).await;
        reader.next().await.expect("a frame");
        let last = reader.next().await.expect("a frame").position();
        test.node.fail_file(FilePath::new(RING), Operation::Sync);
        write_series(&mut other, &[(3, &[now + 2]), (4, &[9])]);
        test.clock.sleep(SETTLE).await;
        let ended = reader.next().await.expect_err("the sync failed");
        assert!(matches!(ended, Ended::Buffer(_)), "{ended:?}");
        reader.ack(last);
        drop(reader);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        assert_eq!(reader.next().await.expect_err("behind"), Ended::Behind);
    });
}

#[test]
fn changes_nothing_on_the_ack_of_a_replaced_reader() {
    run(28, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut replaced = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        let position = replaced.next().await.expect("a frame").position();
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let reader = test.hub.reader(open).await.expect("opens");
        assert_eq!(
            replaced.next().await.expect_err("replaced"),
            Ended::Replaced
        );
        replaced.ack(position);
        drop(reader);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        assert_eq!(reader.next().await.expect_err("behind"), Ended::Behind);
    });
}

#[test]
fn changes_nothing_on_the_ack_of_a_reader_whose_index_was_removed() {
    run(29, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        let position = reader.next().await.expect("a frame").position();
        drop(writer);
        test.hub
            .set_definitions(&without(&["time", "value", "value-c"]));
        let ended = reader.next().await.expect_err("removed");
        assert_eq!(ended, Ended::Removed(Key::from_u128(2)));
        reader.ack(position);
    });
}

#[test]
#[should_panic(expected = "the position is of another index than the reader's")]
fn panics_on_the_ack_of_a_position_of_another_index() {
    run(30, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut other = test.reader(&["value-b"], Mode::Complete).await;
        let mut writer = test.writer("w", &["value-b"]).await;
        write_series(&mut writer, &[(3, &[test.now()]), (4, &[7])]);
        let position = other.next().await.expect("a frame").position();
        reader.ack(position);
    });
}

#[test]
#[should_panic(expected = "the position is past each position that this reader gave")]
fn panics_on_the_ack_of_a_position_past_each_position_it_gave() {
    run(40, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut other = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        for n in 0..3 {
            write(&mut writer, &[now + n], &[n]);
        }
        let mut ahead = None;
        for _ in 0..3 {
            ahead = Some(other.next().await.expect("a frame").position());
        }
        let own = reader.next().await.expect("a frame").position();
        reader.ack(own);
        reader.ack(ahead.expect("a frame"));
    });
}

#[test]
#[should_panic(expected = "the position is past each position that this reader gave")]
fn panics_on_an_ack_past_the_samples_that_it_received() {
    run(41, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut slow = test.hub.reader(open).await.expect("opens");
        let mut other = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        for n in 0..3 {
            write(&mut writer, &[now + n], &[n]);
        }
        let first = other.next().await.expect("a frame").position();
        let mut ahead = first;
        for _ in 1..3 {
            ahead = other.next().await.expect("a frame").position();
        }
        let received = slow.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [0]);
        assert_eq!(received.position(), first);
        slow.ack(ahead);
    });
}

#[test]
#[should_panic(expected = "the position is past each position that this reader gave")]
fn panics_on_an_ack_before_it_gave_a_frame() {
    run(33, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut other = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        let position = other.next().await.expect("a frame").position();
        reader.ack(position);
    });
}

#[test]
#[should_panic(expected = "the position is past each position that this reader gave")]
fn panics_on_an_ack_past_each_position_it_gave_but_not_past_each_frame_it_received() {
    run(43, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut other = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        for n in 0..3 {
            write(&mut writer, &[now + n], &[n]);
        }
        let own = reader.next().await.expect("a frame").position();
        reader.next().await.expect("a frame");
        reader.next().await.expect("a frame");
        other.next().await.expect("a frame");
        let second = other.next().await.expect("a frame").position();
        reader.ack(own);
        reader.ack(second);
    });
}

#[test]
fn resumes_at_the_ack_of_a_position_of_another_reader_at_or_below_its_own() {
    run(34, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut other = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("w", &["value"]).await;
        let now = test.now();
        let mut own = None;
        for n in 0..3 {
            write(&mut writer, &[now + n], &[n]);
            own = Some(reader.next().await.expect("a frame").position());
        }
        let first = other.next().await.expect("a frame").position();
        let mut last = first;
        for _ in 1..3 {
            last = other.next().await.expect("a frame").position();
        }
        assert_eq!(own, Some(last));
        reader.ack(first);
        reader.ack(last);
        drop(reader);
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        write(&mut writer, &[now + 3], &[3]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [3]);
    });
}

#[test]
fn changes_nothing_on_the_ack_of_a_reader_whose_index_was_removed_before_next() {
    run(35, |test| async move {
        let open = named("a", "r", Mode::Complete, Span::SECOND);
        let mut reader = test.hub.reader(open).await.expect("opens");
        let mut writer = test.writer("w", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        let position = reader.next().await.expect("a frame").position();
        drop(writer);
        test.hub
            .set_definitions(&without(&["time", "value", "value-c"]));
        reader.ack(position);
        let ended = reader.next().await.expect_err("removed");
        assert_eq!(ended, Ended::Removed(Key::from_u128(2)));
    });
}

#[test]
fn resumes_a_named_reader_whose_index_is_not_the_first_group_of_a_frame() {
    run(36, |test| async move {
        // It takes the first slot, so `time` is the first group of each frame.
        let _latest = test.reader(&["value"], Mode::Latest).await;
        let open = || {
            let mut open = named("a", "r", Mode::Complete, Span::SECOND);
            open.select = Selector::new(["value-b"]).expect("a selector");
            open
        };
        let mut first = test.hub.reader(open()).await.expect("opens");
        let mut writer = test.writer("w", &["value", "value-b"]).await;
        let now = test.now();
        let mut frame = |n: i64| {
            let stamps: Vec<_> = (now + 5 * n..now + 5 * n + 5).collect();
            let series = [
                (1, &stamps[..]),
                (2, &[0; 5][..]),
                (3, &[now + n]),
                (4, &[n]),
            ];
            write_series(&mut writer, &series);
        };
        for n in 0..3 {
            frame(n);
            let position = first.next().await.expect("a frame").position();
            first.ack(position);
        }
        drop(first);
        let mut reader = test.hub.reader(open()).await.expect("opens");
        frame(3);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 4), [3]);
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
