$s = Join-Path $PSScriptRoot 'hfget.ps1'
$m = 'D:\Python\castcorder-stt\castcorder\spikes\transcribe\models'
$hf = 'https://huggingface.co'
$ggml = @('tiny.en','tiny.en-q5_1','tiny.en-q8_0','base.en','base.en-q5_1','base.en-q8_0','small.en','small.en-q5_1','small.en-q8_0','large-v3-turbo-q5_0','large-v3-turbo-q8_0')
New-Item -ItemType Directory -Force "$m\ggml" | Out-Null
foreach ($g in $ggml) { if (-not (Test-Path "$m\ggml\ggml-$g.bin")) { & $s "$hf/ggerganov/whisper.cpp/resolve/main/ggml-$g.bin" "$m\ggml\ggml-$g.bin" } }
if (-not (Test-Path "$m\ggml\ggml-distil-small.en.bin")) { & $s "$hf/distil-whisper/distil-small.en/resolve/main/ggml-distil-small.en.bin" "$m\ggml\ggml-distil-small.en.bin" }
$st = @{ 'tiny.en'='openai/whisper-tiny.en'; 'base.en'='openai/whisper-base.en'; 'small.en'='openai/whisper-small.en'; 'distil-small.en'='distil-whisper/distil-small.en'; 'large-v3-turbo'='openai/whisper-large-v3-turbo' }
foreach ($k in $st.Keys) {
  $dir = "$m\st\$k"; New-Item -ItemType Directory -Force $dir | Out-Null
  foreach ($f in @('config.json','tokenizer.json','model.safetensors')) { if (-not (Test-Path "$dir\$f")) { & $s "$hf/$($st[$k])/resolve/main/$f" "$dir\$f" } }
}
