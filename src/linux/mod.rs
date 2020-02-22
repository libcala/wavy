use std::ffi::CString;
use std::task::Poll;
use std::task::Context;
use std::pin::Pin;
use std::future::Future;
use std::convert::TryInto;
use std::iter::IntoIterator;
use std::borrow::Borrow;

// Update with: `dl_api ffi/asound,so,2.muon src/linux/gen.rs`
#[rustfmt::skip]
mod gen;

use self::gen::{AlsaPlayer, AlsaRecorder, AlsaDevice, SndPcmHwParams, SndPcmStream, SndPcm, SndPcmFormat, SndPcmAccess, SndPcmMode};
use crate::{AudioError, SampleRate, StereoS16Frame};

fn pcm_hw_params(
    device: &AlsaDevice,
    sr: u32,
    sound_device: &SndPcm,
    limit_buffer: bool,
) -> Result<SndPcmHwParams, AudioError> {
    let hw_params = device.snd_pcm_hw_params_malloc().map_err(|_|
        AudioError::InternalError(
            "Cannot allocate hardware parameter structure!".to_string(),
        )
    )?;

    device.snd_pcm_hw_params_any(sound_device, &hw_params).map_err(|a|
        AudioError::InternalError(
            format!("Cannot initialize hardware parameter structure: {}!", a),
        )
    )?;
    // Enable resampling.
    device.snd_pcm_hw_params_set_rate_resample(sound_device, &hw_params, 1).map_err(|_|
        AudioError::InternalError(
            "Resampling setup failed for playback!".to_string(),
        )
    )?;
    // Set access to RW noninterleaved.
    device.snd_pcm_hw_params_set_access(sound_device, &hw_params,  SndPcmAccess::RwInterleaved).map_err(|_|
        AudioError::InternalError(
            "Cannot set access type!".to_string(),
        )
    )?;
    //
    device.snd_pcm_hw_params_set_format(sound_device, &hw_params, SndPcmFormat::S16Le).map_err(|_|
        AudioError::InternalError(
            "Cannot set sample format!".to_string(),
        )
    )?;
    // Set channels to stereo (2).
    device.snd_pcm_hw_params_set_channels(sound_device, &hw_params, 2).map_err(|_|
        AudioError::InternalError(
            "Cannot set channel count!".to_string(),
        )
    )?;
    // Set Sample rate.
    let mut actual_rate = sr;
    device.snd_pcm_hw_params_set_rate_near(
        sound_device,
        &hw_params,
        &mut actual_rate,
        None,
    ).map_err(|_|
        AudioError::InternalError(
            "Cannot set sample rate!".to_string(),
        )
    )?;
    if actual_rate != sr {
        return Err(AudioError::InternalError(format!(
            "Failed to set rate: {}, Got: {} instead!",
            sr, actual_rate
        )));
    }
    // Period size must be a power of two
    // Currently only tries 1024
    let mut period_size = 1024;
    device.snd_pcm_hw_params_set_period_size_near(
        sound_device,
        &hw_params,
        &mut period_size,
        None,
    ).unwrap();
    if period_size != 1024 {
        return Err(AudioError::InternalError(format!(
            "Wavy: Tried to set period size: {}, Got: {}!", 1024, period_size
        )));
    }
    // Set buffer size to about 3 times the period (setting latency).
    if limit_buffer {
        let mut buffer_size = period_size * 4;
        device.snd_pcm_hw_params_set_buffer_size_near(
            sound_device,
            &hw_params,
            &mut buffer_size,
        ).unwrap();
        if buffer_size != period_size * 4 {
            eprintln!(
                "Wavy: Tried to set buffer size: {}, Got: {}!",
                period_size * 4, buffer_size
            );
        }
    }
    // Apply the hardware parameters that just got set.
    device.snd_pcm_hw_params(sound_device, &hw_params).map_err(|_|
        AudioError::InternalError(
            "Failed to set parameters!".to_string(),
        )
    )?;

    Ok(hw_params)
}

// Player/Recorder Shared Code for ALSA.
pub struct Pcm {
    device: AlsaDevice,
    sound_device: SndPcm,
    fd: smelling_salts::Device,
    period_size: usize,
}

impl Pcm {
    /// Create a new async PCM.
    fn new(direction: SndPcmStream, sr: u32) -> Result<Self, AudioError> {
        // Load shared alsa module.
        let device = AlsaDevice::new().ok_or_else(|| AudioError::InternalError(
            "Could not load AlsaDevice module in shared object!".to_string(),
        ))?;
        // FIXME: Currently only the default device is supported.
        let device_name = CString::new("default").unwrap();
        // Create the ALSA PCM.
        let sound_device: SndPcm = device.snd_pcm_open(
            &device_name, direction, SndPcmMode::Nonblock
        ).map_err(|_| AudioError::NoDevice)?;
        // Configure Hardware Parameters
        let mut hw_params = pcm_hw_params(&device, sr, &sound_device, direction == SndPcmStream::Playback)?;
        // Get the period size (in frames).
        let mut d = 0;
        let period_size = device.snd_pcm_hw_params_get_period_size(
            &hw_params,
            Some(&mut d),
        ).map_err(|_| AudioError::InternalError("Get Period Size".to_string()))?;
        // Free Hardware Parameters
        device.snd_pcm_hw_params_free(&mut hw_params);
        // Get file descriptor
        let fd_count = device.snd_pcm_poll_descriptors_count(&sound_device).unwrap();
        let mut fd_list = Vec::with_capacity(fd_count.try_into().unwrap());
        device.snd_pcm_poll_descriptors(&sound_device, &mut fd_list).unwrap();
        assert_eq!(fd_count, 1); // TODO: More?
        // Register file descriptor with OS's I/O Event Notifier
        let fd = smelling_salts::Device::new(
            fd_list[0].fd,
            #[allow(unsafe_code)]
            unsafe {
                smelling_salts::Watcher::from_raw(fd_list[0].events as u32)
            }
        );

        Ok(Pcm {
            device, sound_device, period_size, fd
        })
    }
}

impl Drop for Pcm {
    fn drop(&mut self) {
        // Unregister async file descriptor before closing the PCM.
        self.fd.old();
        // Should never fail here
        self.device.snd_pcm_close(&mut self.sound_device).unwrap();
    }
}

pub struct Player {
    player: AlsaPlayer,
    pcm: Pcm,
    buffer: Vec<StereoS16Frame>,
}

impl Player {
    pub fn new(sr: SampleRate) -> Result<Player, AudioError> {
        // Load Player ALSA module
        let player = AlsaPlayer::new().ok_or_else(|| AudioError::InternalError(
            "Could not load AlsaPlayer module in shared object!".to_string(),
        ))?;
        // Create Playback PCM.
        let pcm = Pcm::new(SndPcmStream::Playback, sr as u32)?;
        // Create buffer
        let buffer = Vec::with_capacity(pcm.period_size);

        Ok(Player {
            player,
            pcm,
            buffer,
        })
    }

    #[allow(unsafe_code)]
    pub async fn play_last<T>(&mut self, iter: impl IntoIterator<Item=T>) -> Result<usize, AudioError>
        where T: Borrow<crate::StereoS16Frame>
    {
        let mut iter = iter.into_iter();
        // If buffer is empty, fill it.
        if self.buffer.is_empty() {
            let mut gen = false;
            // Write # of frames equal to the period size into the buffer.
            for _ in 0..self.pcm.period_size {
                let f = match iter.next() {
                    Some(f) => f.borrow().clone(),
                    None => {
                        break;
                        gen = true;
                        StereoS16Frame::new(0, 0)
                    },
                };
                self.buffer.push(f);
            }
            if gen {
                // println!("DEBUG: Genereating..");
            }
        }
        // 
        let nframes = (&mut *self).await;
        Ok(nframes)
    }
}

impl Future for &mut Player {
    type Output = usize;

    #[allow(unsafe_code)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = Pin::into_inner(self);

        // Record into temporary buffer.
        let len = match this.player.snd_pcm_writei(
            &this.pcm.sound_device,
            unsafe { std::mem::transmute(this   .buffer.as_slice()) },
        ) {
        Err(error) => {
            // println!("{:?}", this.pcm.device.snd_pcm_state(&this.pcm.sound_device));
            match error {
                // Edge-triggered epoll should only go into pending mode if
                // read/write call results in EAGAIN (according to epoll man
                // page)
                -11 => {
                    this.pcm.fd.register_waker(cx.waker().clone());
                    return Poll::Pending;
                },
                -77 => panic!("Incorrect state (-EBADFD): FIXME"),
                -32 => {
                    panic!("Buffer Overrun (Underflow): FIXME") // FIXME
                },
                -86 => panic!("Stream got suspended, must be recovered (-ESTRPIPE): FIXME"),
                _ => unreachable!(),
            }
        }
        Ok(len) => {
            len as usize
        }
        };
        assert_eq!(len, this.buffer.len());
        // Clear the output buffer (Keeps capacity of 1 period the same)
        this.buffer.clear();
        // Return ready, successfully read some data into the buffer.
        Poll::Ready(len)
    }
}

pub struct Recorder {
    recorder: AlsaRecorder,
    pcm: Pcm,
    buffer: Vec<StereoS16Frame>,
}

impl Recorder {
    pub fn new(sr: SampleRate) -> Result<Recorder, AudioError> {
        // Load Recorder ALSA module
        let recorder = AlsaRecorder::new().ok_or_else(|| AudioError::InternalError(
            "Could not load AlsaRecorder module in shared object!".to_string(),
        ))?;
        // Create Capture PCM.
        let pcm = Pcm::new(SndPcmStream::Capture, sr as u32)?;
        // Create buffer (FIXME: do we need a buffer?)
        let buffer = Vec::with_capacity(pcm.period_size);
        // Return successfully
        Ok(Recorder {
            recorder,
            pcm,
            buffer,
        })
    }

    pub fn link(&self, player: &Player) {
        /*// Start at same time as player.
        self.pcm.device.snd_pcm_link(
            &self.pcm.sound_device,
            &player.pcm.sound_device,
        ).unwrap_or_else(|x| panic!("\"{}\"", self.pcm.device.snd_strerror(x)));*/

        // Start the PCM.
        self.pcm.device.snd_pcm_start(&self.pcm.sound_device).map_err(|_| 
            AudioError::InternalError("Could not start!".to_string())
        ).unwrap();

/*        self.pcm.device.snd_pcm_start(&player.pcm.sound_device).map_err(|_| 
            AudioError::InternalError("Could not start!".to_string())
        ).unwrap();*/
    }

    pub async fn record_last(&mut self) -> Result<&[StereoS16Frame], AudioError> {
        (&mut *self).await;
        Ok(&self.buffer)
    }
}

impl Future for &mut Recorder {
    type Output = ();

    #[allow(unsafe_code)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = Pin::into_inner(self);
        // Clear the output buffer (Keeps capacity of 1 period the same)
        this.buffer.clear();
        // Record into temporary buffer.
        if let Err(error) = this.recorder.snd_pcm_readi(
            &this.pcm.sound_device,
            unsafe { std::mem::transmute(&mut this.buffer) },
        )
        {
            // Len is garbage, this resets it to 0
            this.buffer.clear();
            // println!("{:?}", this.pcm.device.snd_pcm_state(&this.pcm.sound_device));
            match error {
                // Edge-triggered epoll should only go into pending mode if
                // read/write call results in EAGAIN (according to epoll man
                // page)
                -11 => {
                    this.pcm.fd.register_waker(cx.waker().clone());
                    return Poll::Pending;
                },
                -77 => panic!("Incorrect state (-EBADFD): FIXME"),
                -32 => panic!("Buffer Overrun (Underflow): FIXME"),
                -86 => panic!("Stream got suspended, must be recovered (-ESTRPIPE): FIXME"),
                _ => unreachable!(),
            }
        }
        // Return ready, successfully read some data into the buffer.
        Poll::Ready(())
    }
}
