//! Top-N audio selection across a 50-node overlay (design section 8, spike 4), using the real
//! selector from `khef-core`.

// Helpers in an integration test file are test code, but clippy only exempts `#[test]` functions.
#![expect(
    clippy::cast_possible_truncation,
    reason = "test arithmetic on frame counts and node numbers, all far below the narrower type's range"
)]

use palaver_khef_core::TopNConfig;
use palaver_proto::Timestamp;
use palaver_sim::NodeId;
use palaver_sim::audio::{AudioOverlay, PACKET_INTERVAL, Spurt, Voices, transit_topology};
use std::time::Duration;

const NODES: u32 = 50;
const LISTENERS: u32 = NODES - 1;
/// A leaf on the last transit khef: the far end of both topologies.
const FAR: NodeId = 15;

/// Milliseconds in one packet interval.
const FRAME_MS: u64 = 20;

/// How many whole packet intervals fit in `span`.
const fn frames(span: Duration) -> u64 {
    (span.as_micros() / PACKET_INTERVAL.as_micros()) as u64
}

/// The time `k` packet intervals after `start`.
fn nth(start: Timestamp, k: u64) -> Timestamp {
    start + PACKET_INTERVAL * k as u32
}

/// A spurt aligned to packet intervals, as speech is once it has been cut into frames.
fn spurt(who: NodeId, start_ms: u64, len_ms: u64) -> Spurt {
    let align = |ms: u64| ms / FRAME_MS * FRAME_MS;
    Spurt {
        who,
        start: Timestamp::from_millis(align(start_ms)),
        len: Duration::from_millis(align(len_ms).max(FRAME_MS)),
    }
}

struct Run {
    overlay: AudioOverlay,
    script: Vec<Spurt>,
}

fn run_with(
    chain: bool,
    script: Vec<Spurt>,
    seconds: u64,
    setup: impl FnOnce(&mut AudioOverlay, &mut Voices),
) -> Run {
    let mut overlay = AudioOverlay::new(
        NODES,
        &transit_topology(NODES, chain),
        TopNConfig::default(),
        42,
    );
    let mut voices = Voices::new(NODES, 7);
    setup(&mut overlay, &mut voices);
    let ticks = seconds * 1000 / FRAME_MS;
    for tick in 0..ticks {
        overlay.run_until(nth(Timestamp::ZERO, tick));
        voices.tick(&mut overlay, &script);
    }
    overlay.run_until(nth(Timestamp::ZERO, ticks + 20));
    Run { overlay, script }
}

fn run(chain: bool, script: Vec<Spurt>, seconds: u64) -> Run {
    run_with(chain, script, seconds, |_, _| {})
}

/// For one spurt: packets that reached every listener, packets sent, and how many at the
/// start were missed by someone.
fn heard(run: &Run, s: &Spurt) -> (u64, u64, u64) {
    let sent: Vec<Timestamp> = (0..frames(s.len)).map(|k| nth(s.start, k)).collect();
    let full = |at: &Timestamp| run.overlay.delivery(s.who, *at).listeners == LISTENERS;
    let complete = sent.iter().filter(|at| full(at)).count() as u64;
    let clipped = sent.iter().take_while(|at| !full(at)).count() as u64;
    (complete, sent.len() as u64, clipped)
}

#[test]
fn a_one_word_answer_reaches_everyone_from_its_first_packet_with_no_added_delay() {
    for (chain, hops) in [(false, 7), (true, 9)] {
        // Three people talk, everyone is silent for five seconds, then a fourth says "yes".
        let script = vec![
            spurt(8, 500, 2000),
            spurt(9, 500, 2000),
            spurt(10, 500, 2000),
            spurt(FAR, 8000, 300),
        ];
        let run = run(chain, script, 9);
        let yes = run.script[3];
        assert_eq!(heard(&run, &yes), (15, 15, 0), "chain: {chain}");
        // The farthest listener gets the first packet after exactly the path's hop count.
        assert_eq!(
            run.overlay.delivery(FAR, yes.start).worst_latency,
            PACKET_INTERVAL * hops,
            "chain: {chain}"
        );
    }
}

#[test]
fn an_answer_200_ms_after_a_question_is_complete() {
    let script = vec![
        spurt(8, 500, 1000),
        spurt(9, 500, 1000),
        spurt(10, 3000, 3000),
        spurt(FAR, 6200, 300),
    ];
    let run = run(true, script, 7);
    assert_eq!(heard(&run, &run.script[3]), (15, 15, 0));
}

#[test]
fn a_fourth_voice_gets_in_during_a_rapid_three_way_exchange() {
    for turn in [200, 400] {
        let mut script = vec![];
        let mut at = 500;
        let mut i = 0;
        while at < 6000 {
            script.push(spurt(8 + i % 3, at, turn));
            at += turn;
            i += 1;
        }
        script.push(spurt(FAR, at + 40, 300));
        let run = run(true, script, 7);
        assert_eq!(
            heard(&run, run.script.last().unwrap()),
            (15, 15, 0),
            "turns of {turn} ms"
        );
    }
}

/// One speaker at a time for 2 to 5 seconds, the next starting up to 200 ms early or 600 ms
/// late, with short interjections from others.
fn meeting(seconds: u64) -> Vec<Spurt> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    let mut script = vec![];
    let mut at = 1000;
    while at < seconds * 1000 {
        let who = next(u64::from(NODES)) as NodeId;
        let len = 2000 + next(3000);
        script.push(spurt(who, at, len));
        let mut interject = at + 300 + next(2000);
        while interject < at + len {
            let other = next(u64::from(NODES)) as NodeId;
            if other != who {
                script.push(spurt(other, interject, 200 + next(300)));
            }
            interject += 1000 + next(3000);
        }
        at = at + len + next(800) - 200;
    }
    script.retain(|s| s.start + s.len < Timestamp::from_millis(seconds * 1000));
    script
}

#[test]
fn in_a_meeting_no_word_is_clipped_and_links_stay_within_the_bound() {
    let seconds = 20;
    let run = run(false, meeting(seconds), seconds);
    assert!(
        run.script.len() >= 8,
        "only {} spurts: too little to prove anything",
        run.script.len()
    );
    let (mut sent, mut complete) = (0, 0);
    for s in &run.script {
        let (ok, n, clipped) = heard(&run, s);
        assert_eq!(clipped, 0, "onset of {s:?} was clipped");
        sent += n;
        complete += ok;
    }
    // Syllable dips below the minimum level cannot cost a selected speaker any packets.
    assert_eq!(complete, sent);
    // No link direction carries more than N tracks' worth of packets.
    let bound = 3 * (seconds + 1) * 1000 / FRAME_MS;
    assert!(
        run.overlay.busiest_link_packets() <= bound,
        "{} > {bound}",
        run.overlay.busiest_link_packets()
    );
    assert!(run.overlay.client_switches() > 0);
}

#[test]
fn a_talker_that_crashes_mid_speech_does_not_block_later_speakers() {
    let script = vec![
        spurt(8, 500, 3000),
        spurt(9, 500, 3000),
        spurt(10, 500, 3000),
        spurt(20, 6000, 1000),
        spurt(21, 6200, 1000),
        spurt(22, 6400, 1000),
    ];
    let run = run_with(true, script, 8, |_, voices| {
        voices.crashed.push((10, Timestamp::from_millis(2000)));
    });
    for s in &run.script[3..] {
        assert_eq!(heard(&run, s), (50, 50, 0), "{s:?}");
    }
}

#[test]
fn with_loss_on_every_hop_speech_still_gets_through_and_nothing_gets_stuck() {
    let seconds = 20;
    let run = run_with(false, meeting(seconds), seconds, |overlay, _| {
        overlay.loss_per_million = 20_000;
    });
    let mut reached = 0u64;
    let mut possible = 0u64;
    for s in &run.script {
        for k in 0..frames(s.len) {
            reached += u64::from(run.overlay.delivery(s.who, nth(s.start, k)).listeners);
            possible += u64::from(LISTENERS);
        }
    }
    // 2% loss per hop over about four hops loses roughly 8%; selection must not add much.
    let percent = reached * 100 / possible;
    assert!(
        (85..=96).contains(&percent),
        "{percent}% of speech deliveries"
    );
}

#[test]
fn the_same_seed_gives_the_same_run() {
    let go = |loss| {
        let run = run_with(false, meeting(10), 10, |overlay, _| {
            overlay.loss_per_million = loss;
        });
        (
            run.overlay.trace(),
            run.overlay.busiest_link_packets(),
            run.overlay.client_switches(),
        )
    };
    assert_eq!(go(20_000), go(20_000));
    assert_ne!(go(20_000).0, go(0).0);
}
