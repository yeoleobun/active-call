use crate::event::EventSender;
use crate::event::SessionEvent;
use crate::media::{Sample, TrackId};
#[cfg(feature = "offline")]
use crate::offline::get_offline_models;
#[cfg(feature = "offline")]
use crate::offline::sensevoice::{FeaturePipeline, FrontendConfig, language_id_from_code};
use crate::transcription::{TranscriptionClient, TranscriptionOption};
use anyhow::{Result, anyhow};
#[cfg(feature = "offline")]
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

type TranscriptionClientFuture =
    Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>>;

struct SensevoiceAsrClientInner {
    audio_tx: mpsc::UnboundedSender<Vec<Sample>>,
    _option: TranscriptionOption,
}

pub struct SensevoiceAsrClient {
    inner: Arc<SensevoiceAsrClientInner>,
}

pub struct SensevoiceAsrClientBuilder;

impl SensevoiceAsrClientBuilder {
    pub fn create(
        track_id: TrackId,
        _token: CancellationToken,
        option: TranscriptionOption,
        event_sender: EventSender,
    ) -> TranscriptionClientFuture {
        Box::pin(async move {
            #[cfg(feature = "offline")]
            {
                // Ensure offline models are initialized
                if get_offline_models().is_none() {
                    return Err(anyhow!(
                        "Offline models not initialized. Please initialize with init_offline_models()"
                    ));
                }

                let models = get_offline_models().unwrap();

                // Initialize SenseVoice if not already initialized
                models.init_sensevoice().await?;

                let (audio_tx, audio_rx) = mpsc::unbounded_channel::<Vec<Sample>>();

                let language = option
                    .language
                    .clone()
                    .unwrap_or_else(|| "auto".to_string());
                let lang_id = language_id_from_code(&language);
                let track_id_clone = track_id.clone();
                // Default input sample rate to 8000Hz (standard SIP) if not specified
                let input_rate = option.samplerate.unwrap_or(8000);

                let silence_threshold = option
                    .extra
                    .as_ref()
                    .and_then(|m| m.get("silence_threshold"))
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(0.01);

                // Spawn audio processing task
                tokio::spawn(process_stream(
                    audio_rx,
                    track_id_clone,
                    lang_id,
                    event_sender,
                    input_rate,
                    silence_threshold,
                ));

                let inner = SensevoiceAsrClientInner {
                    audio_tx,
                    _option: option,
                };

                Ok(Box::new(SensevoiceAsrClient {
                    inner: Arc::new(inner),
                }) as Box<dyn TranscriptionClient>)
            }
            #[cfg(not(feature = "offline"))]
            {
                Err(anyhow!("Offline feature is not enabled"))
            }
        })
    }
}

#[cfg(feature = "offline")]
async fn process_stream(
    mut audio_rx: mpsc::UnboundedReceiver<Vec<Sample>>,
    track_id: TrackId,
    lang_id: i32,
    event_sender: EventSender,
    input_rate: u32,
    silence_threshold: f32,
) {
    // Buffer for accumulating audio samples (target rate 16000)
    let mut buffer: Vec<f32> = Vec::with_capacity(16000 * 10);

    // VAD/Segmentation parameters
    let sample_rate: usize = 16000;
    let max_segment_len = sample_rate * 15; // 15 seconds
    // let silence_threshold = 0.01; // RMS threshold
    let min_silence_duration = sample_rate / 2; // 0.5 seconds
    let min_segment_len = sample_rate; // 1 second

    // Resampler setup
    let chunk_size = 128;
    let mut resampler = if input_rate != sample_rate as u32 {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
            oversampling_factor: 128,
        };
        match SincFixedIn::<f32>::new(
            sample_rate as f64 / input_rate as f64,
            2.0,
            params,
            chunk_size,
            1,
        ) {
            Ok(r) => Some(r),
            Err(e) => {
                warn!(error = %e, "Failed to create resampler, will process without resampling");
                None
            }
        }
    } else {
        None
    };
    let mut input_accumulator: Vec<f32> = Vec::with_capacity(chunk_size * 2);

    let mut silence_samples = 0;

    // Create frontend feature pipeline
    let mut frontend = FeaturePipeline::new(FrontendConfig::default());

    debug!(track_id = %track_id, "SenseVoice processing loop started");

    while let Some(samples) = audio_rx.recv().await {
        // Convert i16 samples to f32
        let samples_f32: Vec<f32> = samples.iter().map(|&x| x as f32 / 32768.0).collect();

        // Resample or pass through
        let processed_samples = if let Some(resampler) = &mut resampler {
            input_accumulator.extend_from_slice(&samples_f32);
            let mut output = Vec::new();
            while input_accumulator.len() >= chunk_size {
                let input_chunk: Vec<f32> = input_accumulator.drain(..chunk_size).collect();
                let wave_in = vec![input_chunk];
                if let Ok(wave_out) = resampler.process(&wave_in, None) {
                    output.extend_from_slice(&wave_out[0]);
                }
            }
            output
        } else {
            samples_f32
        };

        buffer.extend_from_slice(&processed_samples);
        if processed_samples.is_empty() {
            continue;
        }

        let sum_sq: f32 = processed_samples.iter().map(|&x| x * x).sum();
        let rms = (sum_sq / processed_samples.len() as f32).sqrt();

        if rms < silence_threshold {
            silence_samples += processed_samples.len();
        } else {
            silence_samples = 0;
        }

        let should_transcribe = (silence_samples >= min_silence_duration
            && buffer.len() >= min_segment_len)
            || (buffer.len() >= max_segment_len);

        if !should_transcribe {
            continue;
        }
        // Take all (since satisfied silence condition)
        let segment: Vec<f32> = buffer.drain(..).collect();

        // Reset silence counter because we drained the buffer (including silence)
        silence_samples = 0;

        let models = match get_offline_models() {
            Some(m) => m,
            None => {
                continue;
            }
        };
        let feats = match frontend.compute_features(&segment, sample_rate as u32) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "Feature extraction failed");
                continue;
            }
        };

        let encoder_lock = models.get_sensevoice().await.unwrap();
        let mut encoder_guard = encoder_lock.write().await;
        if let Some(encoder) = encoder_guard.as_mut() {
            let feats = feats.insert_axis(ndarray::Axis(0));
            // Run inference
            match encoder.run_and_decode(feats.view(), lang_id, true) {
                Ok(text) => {
                    let clean_text = text.trim();
                    if !clean_text.is_empty() {
                        info!(track_id = %track_id, text = %clean_text, "SenseVoice transcription");
                        // Send event
                        let event = SessionEvent::AsrFinal {
                            track_id: track_id.clone(),
                            index: 0,
                            text: clean_text.to_string(),
                            timestamp: crate::media::get_timestamp(),
                            start_time: None,
                            end_time: None,
                            is_filler: None,
                            confidence: Some(1.0),
                        };

                        if let Err(e) = event_sender.send(event) {
                            warn!(error = %e, "Failed to send transcription event");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "SenseVoice inference failed");
                }
            }
        }
    }
}

impl TranscriptionClient for SensevoiceAsrClient {
    fn send_audio(&self, samples: &[Sample]) -> Result<()> {
        self.inner
            .audio_tx
            .send(samples.to_vec())
            .map_err(|_| anyhow!("Failed to send audio data"))?;
        Ok(())
    }
}
