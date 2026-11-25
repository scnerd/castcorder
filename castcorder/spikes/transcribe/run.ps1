# Run one bench process, sample total GPU memory with nvidia-smi, append the RESULT json
# (plus vram_mb = peak minus baseline) to results.jsonl.
# Usage: run.ps1 <exe> <model> <label> [bench args...]
param([string]$Exe, [string]$Model, [string]$Label, [Parameter(ValueFromRemainingArguments)] [string[]]$Rest)
$root = $PSScriptRoot
$wav = "$root\data\clip.wav"
$smiLog = [IO.Path]::GetTempFileName()
$base = [int](nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | Select-Object -First 1)
$smi = Start-Process nvidia-smi -ArgumentList '--query-gpu=memory.used', '--format=csv,noheader,nounits', '-lms', '100' -RedirectStandardOutput $smiLog -NoNewWindow -PassThru
$errLog = "$root\logs\$($Label -replace '[^A-Za-z0-9_.-]','_').log"
New-Item -ItemType Directory -Force "$root\logs" | Out-Null
$out = & $Exe $Model $wav $Label @Rest 2> $errLog
Stop-Process -Id $smi.Id -Force
Start-Sleep -Milliseconds 200
$peak = (Get-Content $smiLog | Where-Object { $_ -match '^\d+' } | ForEach-Object { [int]$_ } | Measure-Object -Maximum).Maximum
Remove-Item $smiLog
$line = $out | Where-Object { $_ -like 'RESULT *' } | Select-Object -Last 1
if (-not $line) { Write-Output "FAILED $Label (see $errLog)"; return }
$json = $line.Substring(7).TrimEnd('}') + (',"vram_mb":{0}}}' -f ($peak - $base))
Add-Content -Path "$root\results.jsonl" -Value $json -Encoding ascii
$r = $json | ConvertFrom-Json
Write-Output ("{0}: load {1}s rtf {2} c5 {3}s c10 {4}s rss {5}MB vram {6}MB" -f $Label, $r.load_s, $r.rtf, $r.c5_med_s, $r.c10_med_s, $r.peak_rss_mb, $r.vram_mb)
