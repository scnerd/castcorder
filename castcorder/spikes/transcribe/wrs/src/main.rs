// whisper-rs (whisper.cpp) bench. Usage: stt-wrs <model.bin> <clip.wav> <label> [--gpu N] [--threads N] [--audio-ctx]
#[path = "../../common/bench.rs"]
mod bench;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

struct W {
    state: WhisperState,
    threads: i32,
    scale_ctx: bool,
}

impl bench::Engine for W {
    fn transcribe(&mut self, pcm: &[f32], chunk_secs: Option<f32>) -> String {
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(self.threads);
        p.set_language(Some("en"));
        p.set_translate(false);
        p.set_no_context(true);
        p.set_temperature(0.0);
        p.set_temperature_inc(0.0);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        if let (true, Some(s)) = (self.scale_ctx, chunk_secs) {
            // Encoder frames are 50 per second; pad a little.
            p.set_audio_ctx(((s * 50.0) as i32 + 64).min(1500));
        }
        self.state.full(p, pcm).expect("full");
        let mut s = String::new();
        for seg in self.state.as_iter() {
            s.push_str(&seg.to_string());
        }
        s
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (model, wav, label) = (a[1].clone(), a[2].clone(), a[3].clone());
    let mut gpu: Option<i32> = None;
    let mut threads = 6;
    let mut scale_ctx = false;
    let mut i = 4;
    while i < a.len() {
        match a[i].as_str() {
            "--gpu" => { gpu = Some(a[i + 1].parse().unwrap()); i += 1; }
            "--threads" => { threads = a[i + 1].parse().unwrap(); i += 1; }
            "--audio-ctx" => scale_ctx = true,
            x => panic!("bad arg {x}"),
        }
        i += 1;
    }
    bench::run(&label, &wav, move || {
        let mut cp = WhisperContextParameters::default();
        cp.use_gpu(gpu.is_some());
        cp.gpu_device(gpu.unwrap_or(0));
        let ctx = WhisperContext::new_with_params(&model, cp).expect("load model");
        let state = ctx.create_state().expect("state");
        // ctx must outlive state; leak it for the bench.
        std::mem::forget(ctx);
        W { state, threads, scale_ctx }
    });
}
