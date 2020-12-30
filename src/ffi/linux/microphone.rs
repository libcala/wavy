// Copyright Jeron Aldaron Lau 2019 - 2020.
// Distributed under either the Apache License, Version 2.0
//    (See accompanying file LICENSE_APACHE_2_0.txt or copy at
//          https://apache.org/licenses/LICENSE-2.0),
// or the Boost Software License, Version 1.0.
//    (See accompanying file LICENSE_BOOST_1_0.txt or copy at
//          https://www.boost.org/LICENSE_1_0.txt)
// at your option. This file may not be copied, modified, or distributed except
// according to those terms.

#![allow(unsafe_code)]

use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use fon::{chan::Ch32, Frame, Stream};

use super::{asound, pcm_hw_params, Pcm, SndPcmState};

pub(crate) struct Microphone {
    // PCM I/O Handle
    pcm: Pcm,
    // Interleaved Audio Buffer.
    buffer: Vec<Ch32>,
    // The period of the microphone.
    period: u16,
    // Index to stop reading.
    endi: usize,
    // Number of channels on the Microphone.
    pub(crate) channels: u8,
    // Sample Rate of The Microphone (src)
    pub(crate) sample_rate: Option<f64>,
}

impl Microphone {
    pub(crate) fn new(id: crate::MicrophoneId) -> Option<Self> {
        // Create Capture PCM.
        let pcm = Pcm::new(id.0 .0)?;
        // Return successfully
        Some(Self {
            pcm,
            buffer: Vec::new(),
            period: 0,
            channels: 0,
            endi: 0,
            sample_rate: None,
        })
    }

    /// Attempt to configure the microphone for a specific number of channels.
    fn set_channels<F>(&mut self) -> Option<bool>
    where
        F: Frame<Chan = Ch32>,
    {
        if F::CHAN_COUNT != self.channels.into() {
            if !matches!(F::CHAN_COUNT, 1 | 2 | 6) {
                panic!("Unknown speaker configuration")
            }
            self.channels = F::CHAN_COUNT as u8;
            // Configure Hardware Parameters
            pcm_hw_params(
                &self.pcm,
                self.channels,
                &mut self.buffer,
                &mut self.sample_rate,
                &mut self.period,
            )?;
            Some(true)
        } else {
            Some(false)
        }
    }

    pub(crate) fn record<F: Frame<Chan = Ch32>>(
        &mut self,
    ) -> MicrophoneStream<'_, F> {
        // Change number of channels, if different than last call.
        self.set_channels::<F>()
            .expect("Microphone::record() called with invalid configuration");

        // Stream from microphone's buffer.
        MicrophoneStream(self, 0, PhantomData)
    }

    pub(crate) fn channels(&self) -> u8 {
        self.pcm.dev.supported
    }
}

impl Future for Microphone {
    type Output = ();

    #[allow(unsafe_code)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Get mutable reference to microphone.
        let this = self.get_mut();

        // If microphone is unconfigured, return Ready to configure and play.
        if this.channels == 0 {
            return Poll::Ready(());
        }

        // Check if not woken, then yield.
        if this.pcm.fd.should_yield() {
            return Poll::Pending;
        }

        // Attempt to overwrite the internal microphone buffer.
        let result = unsafe {
            asound::pcm::readi(
                this.pcm.dev.pcm,
                this.buffer.as_mut_slice().as_mut_ptr(),
                this.period,
            )
        };

        // Check if it succeeds, then return Ready.
        match result {
            Err(error) => {
                match error {
                    // Edge-triggered epoll should only go into pending mode if
                    // read/write call results in EAGAIN (according to epoll man
                    // page)
                    -11 => { /* Pending */ }
                    -77 => {
                        eprintln!(
                            "Incorrect state (-EBADFD): Report Bug to \
                        https://github.com/libcala/wavy/issues/new"
                        );
                        unreachable!()
                    }
                    -32 => {
                        match unsafe { asound::pcm::state(this.pcm.dev.pcm) } {
                            SndPcmState::Xrun => {
                                eprintln!("Microphone XRUN: Latency cause?");
                                unsafe {
                                    asound::pcm::prepare(this.pcm.dev.pcm)
                                        .unwrap();
                                }
                            }
                            st => {
                                eprintln!(
                                "Incorrect state = {:?} (XRUN): Report Bug \
                            to https://github.com/libcala/wavy/issues/new",
                                st
                            );
                                unreachable!()
                            }
                        }
                    }
                    -86 => {
                        eprintln!(
                        "Stream got suspended, trying to recover… (-ESTRPIPE)"
                    );
                        unsafe {
                            if asound::pcm::resume(this.pcm.dev.pcm).is_ok() {
                                // Prepare, so we keep getting samples.
                                asound::pcm::prepare(this.pcm.dev.pcm).unwrap();
                            }
                        }
                    }
                    _ => unreachable!(),
                }
                // Register waker
                this.pcm.fd.register_waker(cx.waker());
                // Not ready
                Poll::Pending
            }
            Ok(len) => {
                this.endi = len;
                // Ready, audio buffer has been filled!
                Poll::Ready(())
            }
        }
    }
}

pub(crate) struct MicrophoneStream<'a, F: Frame<Chan = Ch32>>(
    &'a mut Microphone,
    usize,
    PhantomData<F>,
);

impl<F: Frame<Chan = Ch32>> Iterator for MicrophoneStream<'_, F> {
    type Item = F;

    fn next(&mut self) -> Option<Self::Item> {
        if self.1 >= self.0.endi {
            return None;
        }
        let frame = F::from_channels(
            &self.0.buffer[self.1 * self.0.channels as usize..],
        );
        self.1 += 1;
        Some(frame)
    }
}

impl<F: Frame<Chan = Ch32>> Stream<F> for MicrophoneStream<'_, F> {
    fn sample_rate(&self) -> Option<f64> {
        self.0.sample_rate
    }

    fn len(&self) -> Option<usize> {
        Some(self.0.endi)
    }
}
