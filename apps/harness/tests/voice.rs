//! A call on made-up devices: a microphone that gives a made-up voice and a speaker that
//! takes sound at its own pace, on an in-memory network.

#![expect(clippy::unwrap_used, reason = "a test may stop where it stands")]

use palaver_harness::voice::{Sink, Source, Voice as Live};
use palaver_media::process::Settings;
use palaver_media::synthetic::{Voice, hears, pitch_of};
use palaver_media::{Channels, FRAME_SAMPLES};
use palaver_net::memory::MemoryNetwork;
use palaver_proto::Secret;
use palaver_sdk::{Options, Participant};
use std::time::Duration;

/// A microphone at which someone talks, or nobody: one frame for each frame interval.
struct Mic {
    voice: Option<Voice>,
    due: usize,
}

impl Source for Mic {
    fn pop(&mut self, frame: &mut [f32]) -> bool {
        if self.due == 0 {
            return false;
        }
        self.due -= 1;
        match &mut self.voice {
            Some(voice) => voice.frame(true, frame),
            None => frame.fill(0.0),
        }
        true
    }
}

/// A speaker: it keeps everything it was given, and says how much it has not yet taken.
#[derive(Default)]
struct Speaker {
    heard: Vec<f32>,
    waiting: usize,
    most_waiting: usize,
    frames: usize,
}

impl Speaker {
    /// The device takes `samples`; what it finds missing it would have played as silence.
    const fn take(&mut self, samples: usize) -> usize {
        let short = samples.saturating_sub(self.waiting);
        self.waiting = self.waiting.saturating_sub(samples);
        short
    }
}

impl Sink for Speaker {
    fn waiting(&self) -> usize {
        self.waiting
    }

    fn push(&mut self, frame: &[f32]) {
        self.frames += 1;
        self.waiting += FRAME_SAMPLES;
        self.most_waiting = self.most_waiting.max(self.waiting);
        self.heard.extend_from_slice(frame);
    }
}

fn member(
    network: &MemoryNetwork,
    secret: &Secret<[u8; 32]>,
    speaker: Option<u32>,
    cleaning: Option<Settings>,
    playout: Channels,
) -> (Participant, Live<Mic, Speaker>) {
    let options = Options {
        playout,
        ..Options::named("someone")
    };
    let (participant, call) = Participant::on_memory(network, secret, options).unwrap();
    let mic = Mic {
        voice: speaker.map(|speaker| Voice::new(pitch_of(speaker), 0.4)),
        due: 0,
    };
    // Two frames are kept waiting for the speaker.
    let live = Live::new(
        call,
        mic,
        Speaker::default(),
        cleaning,
        playout,
        2 * FRAME_SAMPLES,
    );
    (participant, live)
}

/// Run `members` for `seconds`. Every 20 ms each microphone has a frame, each speaker
/// takes `taken` samples, and each voice has its turn. Returns how many samples the
/// second member's speaker found missing after the first second.
async fn run(members: &mut [Live<Mic, Speaker>], seconds: u64, taken: usize) -> usize {
    let mut short = 0;
    for tick in 0..seconds * 50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        for (n, live) in members.iter_mut().enumerate() {
            let missing = live.speaker().take(taken);
            if n == 1 && tick >= 50 {
                short += missing;
            }
            live.microphone().due += 1;
            live.turn().unwrap();
        }
    }
    short
}

#[tokio::test(start_paused = true)]
async fn what_is_said_at_one_microphone_comes_out_of_the_others_speaker() {
    let network = MemoryNetwork::new();
    let secret = Secret::new([7; 32]);
    let (talker, a) = member(&network, &secret, Some(2), None, Channels::Mono);
    let (listener, b) = member(&network, &secret, None, None, Channels::Mono);
    talker.create_meeting().await;
    listener.join(vec![talker.address().clone()]).await;

    let mut members = vec![a, b];
    let short = run(&mut members, 4, FRAME_SAMPLES).await;
    let (mut b, a) = (members.pop().unwrap(), members.pop().unwrap());
    let heard = &b.speaker().heard;
    assert!(hears(&heard[heard.len() - 48_000..], pitch_of(2)));
    // The speaker never ran dry once things were going, and never had more waiting than
    // the two frames asked for and the one just made.
    assert_eq!(short, 0);
    assert!(b.speaker().most_waiting <= 3 * FRAME_SAMPLES);
    // The listener, who said nothing, sent nothing; the talker hears nobody.
    let mut a = a;
    assert!(!hears(&a.speaker().heard, pitch_of(2)));
    a.into_call().stop().await;
    b.into_call().stop().await;
}

#[tokio::test(start_paused = true)]
async fn the_speaker_keeps_time_so_a_slow_one_is_given_less_and_loses_nothing() {
    let network = MemoryNetwork::new();
    let secret = Secret::new([7; 32]);
    let (talker, a) = member(&network, &secret, Some(2), None, Channels::Mono);
    let (listener, b) = member(&network, &secret, None, None, Channels::Mono);
    talker.create_meeting().await;
    listener.join(vec![talker.address().clone()]).await;

    // A speaker that takes 3% too little: 931 samples where 960 are due.
    let mut members = vec![a, b];
    run(&mut members, 4, 931).await;
    let (mut b, a) = (members.pop().unwrap(), members.pop().unwrap());
    let speaker = b.speaker();
    // It was given about 3% fewer frames than a timer would have made (200), and what
    // waited for it never grew: nothing had to be thrown away.
    assert!((190..=197).contains(&speaker.frames), "{}", speaker.frames);
    assert!(speaker.most_waiting <= 3 * FRAME_SAMPLES);
    assert!(hears(
        &speaker.heard[speaker.heard.len() - 48_000..],
        pitch_of(2)
    ));
    a.into_call().stop().await;
    b.into_call().stop().await;
}

#[tokio::test(start_paused = true)]
async fn cleaning_and_two_channels_of_playout_go_together() {
    let network = MemoryNetwork::new();
    let secret = Secret::new([7; 32]);
    let cleaning = Some(Settings::default());
    let (talker, a) = member(&network, &secret, Some(2), cleaning, Channels::Stereo);
    let (listener, b) = member(&network, &secret, None, cleaning, Channels::Stereo);
    talker.create_meeting().await;
    listener.join(vec![talker.address().clone()]).await;

    let mut members = vec![a, b];
    run(&mut members, 3, FRAME_SAMPLES).await;
    let (mut b, a) = (members.pop().unwrap(), members.pop().unwrap());
    let speaker = b.speaker();
    // Two channels: twice the samples, the same voice on both sides.
    assert_eq!(speaker.heard.len(), speaker.frames * 2 * FRAME_SAMPLES);
    let left: Vec<f32> = speaker.heard.iter().step_by(2).copied().collect();
    assert!(hears(&left[left.len() - 48_000..], pitch_of(2)));
    a.into_call().stop().await;
    b.into_call().stop().await;
}
