use wavy::{MicrophoneId, SpeakerId};

fn main() {
    for speaker in SpeakerId::query() {
        println!("Found speaker: {}", speaker);
    }
    for microphone in MicrophoneId::query() {
        println!("Found microphone: {}", microphone);
    }
}