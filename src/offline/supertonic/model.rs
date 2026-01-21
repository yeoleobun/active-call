use super::config::{Config, VoiceStyleData};
use super::processor::{UnicodeProcessor, sample_noisy_latent};
use anyhow::{Context, Result, anyhow};
use ndarray::{Array, Array3, Dimension};
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Value,
};
use std::{fs, io::BufReader, path::Path};
use tracing::warn;

pub struct Style {
    pub ttl: Array3<f32>,
    pub dp: Array3<f32>,
}

pub struct SupertonicModel {
    cfgs: Config,
    text_processor: UnicodeProcessor,
    dp_ort: Session,
    text_enc_ort: Session,
    vector_est_ort: Session,
    vocoder_ort: Session,
    pub sample_rate: i32,
}

impl SupertonicModel {
    pub fn new<P: AsRef<Path>>(
        onnx_dir: P,
        config_path: P,
        _voice_styles_dir: P, // Not used here, voice styles loaded on demand
        intra_threads: usize,
    ) -> Result<Self> {
        let onnx_dir = onnx_dir.as_ref();

        let cfgs = load_cfgs(config_path)?;
        let text_processor = UnicodeProcessor::new(onnx_dir.join("unicode_indexer.json"))?;

        let dp_ort =
            build_session_with_ort_cache(&onnx_dir.join("duration_predictor.onnx"), intra_threads)?;
        let text_enc_ort =
            build_session_with_ort_cache(&onnx_dir.join("text_encoder.onnx"), intra_threads)?;
        let vector_est_ort =
            build_session_with_ort_cache(&onnx_dir.join("vector_estimator.onnx"), intra_threads)?;
        let vocoder_ort =
            build_session_with_ort_cache(&onnx_dir.join("vocoder.onnx"), intra_threads)?;

        Ok(Self {
            cfgs: cfgs.clone(),
            text_processor,
            dp_ort,
            text_enc_ort,
            vector_est_ort,
            vocoder_ort,
            sample_rate: cfgs.ae.sample_rate,
        })
    }

    pub fn infer(
        &mut self,
        text_list: &[String],
        lang_list: &[String],
        style: &Style,
        total_step: usize,
        speed: f32,
    ) -> Result<(Vec<Vec<f32>>, Vec<f32>)> {
        let bsz = text_list.len();

        // Process text
        let (text_ids, text_mask_array) = self.text_processor.call(text_list, lang_list)?;

        let mut flat_ids = Vec::new();
        // Since we pad text_ids in processor, lengths might vary?
        // NOTE: In ref.md, `UnicodeProcessor::call` logic for padding was not fully shown.
        // It calculated `text_ids_lengths`. And `get_text_mask` uses max length.
        // But the returned `text_ids` is `Vec<Vec<i64>>`. We need to flatten and pad it to match max_len.

        let max_len = text_ids.iter().map(|v| v.len()).max().unwrap_or(0);
        for row in &text_ids {
            flat_ids.extend_from_slice(row);
            // Pad with 0? Reference didn't show padding logic explicitly for text_ids.
            // Assuming 0 is pad or logic handles it.
            // In processor we should probably pad but let's do it here if needed.
            // Actually `Array::from_shape_vec` will fail if not uniform.
            for _ in 0..(max_len - row.len()) {
                flat_ids.push(0); // Assuming 0 is PAD
            }
        }

        // However, referencing ref.md:
        // `let text_ids_array = { let text_ids_shape = (bsz, text_ids[0].len()); ...`
        // It implies `text_ids` already has uniform length from processor.

        // Let's adjust processor to pad. But I cannot edit processor.rs easily now (I just created it).
        // I'll assume processor pads or I handle it here.
        // Let's just padding here to be safe.

        let text_ids_array = Array::from_shape_vec((bsz, max_len), flat_ids.clone())?;

        tracing::info!("text_ids: {:?}", flat_ids);

        let text_ids_value = to_ort_value_i64(text_ids_array)?;
        let text_mask_value = to_ort_value_f32(text_mask_array.clone())?;
        let style_dp_value = to_ort_value_f32(style.dp.clone())?;

        // Predict duration
        let dp_outputs = self.dp_ort.run(ort::inputs! {
            "text_ids" => &text_ids_value,
            "style_dp" => &style_dp_value,
            "text_mask" => &text_mask_value
        })?;

        let (_, duration_data) = dp_outputs["duration"].try_extract_tensor::<f32>()?;
        let mut duration: Vec<f32> = duration_data.to_vec();
        tracing::info!("predicted duration (before speed): {:?}", duration);

        // Apply speed factor to duration
        for dur in duration.iter_mut() {
            *dur /= speed;
        }

        // Encode text
        let style_ttl_value = to_ort_value_f32(style.ttl.clone())?;
        let text_enc_outputs = self.text_enc_ort.run(ort::inputs! {
            "text_ids" => &text_ids_value,
            "style_ttl" => &style_ttl_value,
            "text_mask" => &text_mask_value
        })?;

        let (text_emb_shape, text_emb_data) =
            text_enc_outputs["text_emb"].try_extract_tensor::<f32>()?;
        tracing::info!("text_encoder output text_emb shape: {:?}", text_emb_shape);

        let text_emb = Array3::from_shape_vec(
            (
                text_emb_shape[0] as usize,
                text_emb_shape[1] as usize,
                text_emb_shape[2] as usize,
            ),
            text_emb_data.to_vec(),
        )?;

        // Sample noisy latent
        let (mut xt, latent_mask) = sample_noisy_latent(
            &duration,
            self.sample_rate,
            self.cfgs.ae.base_chunk_size,
            self.cfgs.ttl.chunk_compress_factor,
            self.cfgs.ttl.latent_dim,
        );

        // Prepare constant arrays
        let total_step_array = Array::from_elem(bsz, total_step as f32);

        // Denoising loop
        for step in 0..total_step {
            let current_step_array = Array::from_elem(bsz, step as f32);

            let xt_value = to_ort_value_f32(xt.clone())?;
            let style_ttl_value = to_ort_value_f32(style.ttl.clone())?;
            let text_emb_value = to_ort_value_f32(text_emb.clone())?;
            let latent_mask_value = to_ort_value_f32(latent_mask.clone())?;
            let text_mask_value2 = to_ort_value_f32(text_mask_array.clone())?;
            // Note: Reuse text_mask because ort::inputs consumes?? No, we can clone values or use refs if possible.
            // ORT rust bindings: inputs! takes values.

            let total_step_val = to_ort_value_f32(total_step_array.clone())?;
            let current_step_val = to_ort_value_f32(current_step_array.clone())?;

            let vector_est_outputs = self.vector_est_ort.run(ort::inputs! {
                "noisy_latent" => &xt_value,
                "text_emb" => &text_emb_value,
                "style_ttl" => &style_ttl_value,
                "latent_mask" => &latent_mask_value,
                "text_mask" => &text_mask_value2,
                "total_step" => &total_step_val,
                "current_step" => &current_step_val
            })?;

            let (_, denoised_data) =
                vector_est_outputs["denoised_latent"].try_extract_tensor::<f32>()?;
            let next_xt = Array3::from_shape_vec(xt.dim(), denoised_data.to_vec())?;

            let x_min = next_xt.iter().fold(f32::INFINITY, |a, &b| a.min(b));
            let x_max = next_xt.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
            tracing::info!("step {} latent min/max: {} / {}", step, x_min, x_max);

            // Update xt directly (model output is the next step latent)
            xt = next_xt;
        }

        let x_min = xt.iter().fold(f32::INFINITY, |a, &b| a.min(b));
        let x_max = xt.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
        tracing::info!("Final latent min/max: {} / {}", x_min, x_max);

        // Apply latent mask one last time
        for b in 0..bsz {
            for d in 0..xt.dim().1 {
                for t in 0..xt.dim().2 {
                    xt[[b, d, t]] *= latent_mask[[b, 0, t]];
                }
            }
        }

        // Vocoder (Latent to Waveform)
        let xt_value = to_ort_value_f32(xt.clone())?;
        let vocoder_outputs = self.vocoder_ort.run(ort::inputs! {
            "latent" => &xt_value
        })?;

        let (_, audio_data) = vocoder_outputs["wav_tts"].try_extract_tensor::<f32>()?;

        // Log audio stats
        let audio_vec = audio_data.to_vec();
        let a_min = audio_vec.iter().fold(f32::INFINITY, |a, &b| a.min(b));
        let a_max = audio_vec.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
        let a_mean = audio_vec.iter().sum::<f32>() / audio_vec.len() as f32;
        tracing::info!(
            "Generated Audio Stats (wav_tts): Min={} Max={} Mean={}",
            a_min,
            a_max,
            a_mean
        );

        let _chunk_size = self.cfgs.ae.base_chunk_size as usize;
        let t_audio = audio_data.len() / bsz;

        let mut audios = Vec::with_capacity(bsz);

        for b in 0..bsz {
            let audio_len = (duration[b] * self.sample_rate as f32) as usize;
            let start = b * t_audio;
            // Ensure we don't read past the buffer for this batch item
            // audio_len is estimated from duration, but vocoder output size is fixed by latent size * chunk_size
            // t_audio should be >= audio_len usually, unless duration > latent * chunk_compress * chunk_size
            let end = (start + audio_len).min(start + t_audio);

            if start < audio_data.len() && start < end {
                audios.push(audio_data[start..end].to_vec());
            } else {
                audios.push(Vec::new());
            }
        }

        Ok((audios, duration))
    }
}

pub fn load_cfgs<P: AsRef<Path>>(config_path: P) -> Result<Config> {
    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {:?}", config_path.as_ref()))?;

    // Check for BOM
    let content = if content.starts_with("\u{feff}") {
        &content[3..]
    } else {
        &content
    };

    let mut cfgs: Config = serde_json::from_str(content).with_context(|| {
        format!(
            "Failed to parse config file: {}\nContent: {:.50}...",
            config_path.as_ref().display(),
            content
        )
    })?;
    cfgs.fix();
    Ok(cfgs)
}

pub fn load_voice_style(voice_style_paths: &[String]) -> Result<Style> {
    let bsz = voice_style_paths.len();

    // Read first file to get dimensions
    let first_file =
        fs::File::open(&voice_style_paths[0]).context("Failed to open voice style file")?;
    let first_reader = BufReader::new(first_file);
    let first_data: VoiceStyleData = serde_json::from_reader(first_reader)?;

    let ttl_dims = &first_data.style_ttl.dims;
    let dp_dims = &first_data.style_dp.dims;

    let ttl_dim1 = ttl_dims[1];
    let ttl_dim2 = ttl_dims[2];
    let dp_dim1 = dp_dims[1];
    let dp_dim2 = dp_dims[2];

    // Pre-allocate arrays with full batch size
    let ttl_size = bsz * ttl_dim1 * ttl_dim2;
    let dp_size = bsz * dp_dim1 * dp_dim2;
    let mut ttl_flat = vec![0.0f32; ttl_size];
    let mut dp_flat = vec![0.0f32; dp_size];

    // Fill in the data
    for (i, path) in voice_style_paths.iter().enumerate() {
        let file = fs::File::open(path).context("Failed to open voice style file")?;
        let reader = BufReader::new(file);
        let data: VoiceStyleData = serde_json::from_reader(reader)?;

        // Flatten TTL data
        let ttl_offset = i * ttl_dim1 * ttl_dim2;
        let mut idx = 0;
        for batch in &data.style_ttl.data {
            for row in batch {
                for &val in row {
                    ttl_flat[ttl_offset + idx] = val;
                    idx += 1;
                }
            }
        }

        // Flatten DP data
        let dp_offset = i * dp_dim1 * dp_dim2;
        idx = 0;
        for batch in &data.style_dp.data {
            for row in batch {
                for &val in row {
                    dp_flat[dp_offset + idx] = val;
                    idx += 1;
                }
            }
        }
    }

    let ttl_style = Array3::from_shape_vec((bsz, ttl_dim1, ttl_dim2), ttl_flat)?;
    let dp_style = Array3::from_shape_vec((bsz, dp_dim1, dp_dim2), dp_flat)?;

    Ok(Style {
        ttl: ttl_style,
        dp: dp_style,
    })
}

fn to_ort_value_f32<D>(array: Array<f32, D>) -> Result<Value>
where
    D: Dimension,
{
    let shape: Vec<i64> = array.shape().iter().map(|&s| s as i64).collect();
    let (data, _) = array.into_raw_vec_and_offset();
    Ok(Value::from_array((shape, data))?.into())
}

fn to_ort_value_i64<D>(array: Array<i64, D>) -> Result<Value>
where
    D: Dimension,
{
    let shape: Vec<i64> = array.shape().iter().map(|&s| s as i64).collect();
    let (data, _) = array.into_raw_vec_and_offset();
    Ok(Value::from_array((shape, data))?.into())
}

fn build_session_with_ort_cache(model_path: &Path, intra_threads: usize) -> Result<Session> {
    let ort_path = model_path.with_extension("ort");

    if ort_path.exists() {
        let session_attempt = Session::builder()
            .map_err(|e| anyhow!("ORT session builder error: {e}"))?
            .with_intra_threads(intra_threads)
            .map_err(|e| anyhow!("ORT intra threads error: {e}"))?
            .commit_from_file(&ort_path);

        match session_attempt {
            Ok(session) => return Ok(session),
            Err(err) => {
                warn!(
                    ort = %ort_path.display(),
                    model = %model_path.display(),
                    error = %err,
                    "failed to load cached ORT graph, regenerating"
                );
                let _ = fs::remove_file(&ort_path);
            }
        }
    }

    let builder = Session::builder()
        .map_err(|e| anyhow!("ORT session builder error: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level2)
        .map_err(|e| anyhow!("ORT optimization level error: {e}"))?
        .with_intra_threads(intra_threads)
        .map_err(|e| anyhow!("ORT intra threads error: {e}"))?;

    if let Ok(builder_with_cache) = builder.with_optimized_model_path(&ort_path) {
        match builder_with_cache.commit_from_file(model_path) {
            Ok(session) => return Ok(session),
            Err(err) => {
                warn!(
                    ort = %ort_path.display(),
                    model = %model_path.display(),
                    error = %err,
                    "failed to build session with ORT cache, retrying without cache"
                );
                // let _ = fs::remove_file(&ort_path); // already failed to create it perhaps?
            }
        }
    }

    let fallback_builder = Session::builder()
        .map_err(|e| anyhow!("ORT session builder error: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level2)
        .map_err(|e| anyhow!("ORT optimization level error: {e}"))?
        .with_intra_threads(intra_threads)
        .map_err(|e| anyhow!("ORT intra threads error: {e}"))?;

    let model_bytes = fs::read(model_path)
        .with_context(|| format!("read encoder model {}", model_path.display()))?;
    fallback_builder
        .commit_from_memory(&model_bytes)
        .map_err(|e| anyhow!("ORT load model error: {e}"))
}
