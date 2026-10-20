use std::cell::RefCell;
use std::time::Duration;

use herdr_mcp::matcher::{MatchOutcome, Matcher, tail, wait_for_match};

fn numbered(range: std::ops::Range<u32>) -> String {
    range.map(|n| format!("line {n}\n")).collect()
}

#[test]
fn old_line_in_the_baseline_does_not_match_but_the_same_text_added_later_does() {
    let baseline = "listening on 3000\n$ ";
    let matcher = Matcher::new(baseline, "listening on", "npm run dev");

    assert_eq!(matcher.find(baseline), None);
    assert_eq!(matcher.find("listening on 3000\n$ npm run dev\n"), None);

    let after = "listening on 3000\n$ npm run dev\nlistening on 4000\n";
    let excerpt = matcher.find(after).expect("the new line matches");
    assert!(excerpt.ends_with("listening on 4000"), "{excerpt}");
}

#[test]
fn diff_is_by_content_and_position_not_by_membership() {
    let baseline = "ready\nbuild ok\n$ ";
    let matcher = Matcher::new(baseline, "ready", "make");

    let repeated = "ready\nbuild ok\n$ make\nready\n";
    assert!(matcher.find(repeated).is_some());
    assert_eq!(
        matcher.new_lines(repeated),
        ["$ make".to_string(), "ready".to_string()]
    );
}

#[test]
fn unchanged_last_baseline_line_is_old_and_a_changed_one_is_new() {
    let matcher = Matcher::new("a\nlistening on 3000", "listening on", "x");
    assert!(matcher.new_lines("a\nlistening on 3000").is_empty());
    assert_eq!(matcher.find("a\nlistening on 3000"), None);

    let matcher = Matcher::new("a\n$ ", "ok", "echo ok");
    assert_eq!(matcher.new_lines("a\n$ echo ok\nok\n"), ["$ echo ok", "ok"]);
}

#[test]
fn echoed_command_line_is_skipped_even_when_it_contains_the_needle() {
    let matcher = Matcher::new("$ ", "dev", "npm run dev");
    assert_eq!(matcher.find("$ npm run dev\n"), None);
    assert_eq!(
        matcher.find("$ npm run dev\n> dev\n> vite\n"),
        Some("> dev".into())
    );
}

#[test]
fn multi_line_command_skips_every_echoed_line() {
    let matcher = Matcher::new("$ ", "two", "echo one\necho two");
    assert_eq!(matcher.find("$ echo one\n> echo two\n"), None);
    assert!(matcher.find("$ echo one\n> echo two\ntwo\n").is_some());
}

#[test]
fn reused_pane_with_an_old_match_waits_for_the_new_one() {
    let old = format!("{}listening on 3000\n$ ", numbered(0..30));
    let matcher = Matcher::new(&old, "listening on", "npm run dev");

    let mid = format!("{old}npm run dev\n> vite\n");
    assert_eq!(matcher.find(&mid), None);

    let done = format!("{mid}listening on 5173\n");
    let excerpt = matcher.find(&done).unwrap();
    assert!(
        excerpt.contains("5173") && !excerpt.contains("3000"),
        "{excerpt}"
    );
}

#[test]
fn ready_line_far_past_a_small_window_still_matches() {
    let baseline = numbered(0..80);
    let matcher = Matcher::new(&baseline, "listening on", "serve");

    let current = format!(
        "{baseline}{}listening on 8080\n{}",
        numbered(100..200),
        numbered(200..260)
    );
    assert!(matcher.find(&current).is_some());
}

#[test]
fn match_survives_the_buffer_sliding_past_the_baseline_start() {
    let baseline = numbered(0..80);
    let matcher = Matcher::new(&baseline, "listening on", "serve");

    let current = format!(
        "{}{}listening on 8080\n",
        numbered(40..80),
        numbered(500..540)
    );
    assert!(matcher.find(&current).is_some());
    assert_eq!(matcher.new_lines(&current).len(), 41);
}

#[test]
fn nothing_in_common_with_the_baseline_means_everything_is_new() {
    let matcher = Matcher::new(&numbered(0..10), "ready", "x");
    assert!(matcher.find("scrolled\nready\n").is_some());
}

#[test]
fn trailing_blank_lines_do_not_count_as_output() {
    let matcher = Matcher::new("$ \n\n\n", "ok", "x");
    assert!(matcher.new_lines("$ \n").is_empty());
}

#[tokio::test]
async fn wait_returns_the_match_when_a_later_poll_shows_it() {
    let matcher = Matcher::new("$ ", "ready", "serve");
    let polls = RefCell::new(vec![
        "$ serve\nbuilding\n",
        "$ serve\nbuilding\nready now\n",
    ]);
    let outcome = wait_for_match(
        &matcher,
        Duration::from_secs(2),
        Duration::from_millis(5),
        || async {
            let mut polls = polls.borrow_mut();
            let text = if polls.len() > 1 {
                polls.remove(0)
            } else {
                polls[0]
            };
            Ok::<_, ()>(text.to_string())
        },
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        MatchOutcome::Matched {
            excerpt: "building\nready now".into()
        }
    );
}

#[tokio::test]
async fn wait_times_out_with_the_tail_and_never_matches_stale_text() {
    let matcher = Matcher::new("ready\n$ ", "ready", "serve");
    let started = std::time::Instant::now();
    let outcome = wait_for_match(
        &matcher,
        Duration::from_millis(60),
        Duration::from_millis(5),
        || async { Ok::<_, ()>("ready\n$ serve\nbuilding\n".to_string()) },
    )
    .await
    .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(60));
    match outcome {
        MatchOutcome::TimedOut { tail } => assert!(tail.contains("building"), "{tail}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn wait_propagates_a_read_error() {
    let matcher = Matcher::new("", "x", "y");
    let result = wait_for_match(
        &matcher,
        Duration::from_secs(1),
        Duration::from_millis(5),
        || async { Err::<String, _>("gone") },
    )
    .await;
    assert_eq!(result, Err("gone"));
}

#[test]
fn tail_skips_blank_lines_and_tui_rules() {
    let screen = "answer line\n\n\n────────────\n❯ \n\n────────────\n  status bar\n";
    assert_eq!(tail(screen), "answer line\n❯\n  status bar");
}
