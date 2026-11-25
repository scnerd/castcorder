# Run the whisper.cpp bench matrix. Usage: matrix.ps1 <cpu|vulkan|cuda|igpu>
param([string]$Dev)
$root = Split-Path $PSScriptRoot
$run = "$root\run.ps1"
$m = "$root\models\ggml"
$exe = @{ cpu = "D:\sttb\cpu\Release\wbench.exe"; vulkan = "D:\sttb\vulkan\Release\wbench.exe"; igpu = "D:\sttb\vulkan\Release\wbench.exe"; cuda = "D:\sttb\cuda\Release\wbench.exe" }[$Dev]
$gpuArgs = @{ cpu = @(); vulkan = @('--gpu', '0'); igpu = @('--gpu', '1'); cuda = @('--gpu', '0') }[$Dev]
$models = @('tiny.en', 'tiny.en-q5_1', 'tiny.en-q8_0', 'base.en', 'base.en-q5_1', 'base.en-q8_0', 'small.en', 'small.en-q5_1', 'small.en-q8_0', 'distil-small.en', 'large-v3-turbo-q5_0', 'large-v3-turbo-q8_0')
if ($Dev -eq 'cpu' -or $Dev -eq 'igpu') { $models = $models | Where-Object { $_ -notlike 'large*' } }
foreach ($mod in $models) {
    $f = "$m\ggml-$mod.bin"
    if (-not (Test-Path $f)) { "SKIP $mod (missing)"; continue }
    & $run $exe $f "wcpp $Dev $mod" @gpuArgs --threads 6
}
# Encoder context scaled to chunk length (live-chunk mode) for the small models.
foreach ($mod in @('base.en', 'small.en')) {
    & $run $exe "$m\ggml-$mod.bin" "wcpp $Dev $mod audio_ctx" @gpuArgs --threads 6 --audio-ctx
}
