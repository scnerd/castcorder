param([string]$Name, [string[]]$Extra = @())
$src = 'D:\Python\castcorder-stt\castcorder\spikes\transcribe\wcpp'
$bld = "D:\sttb\$Name"  # short path: the Vulkan shader-gen sub-build exceeds MAX_PATH under the repo
$t = Get-Date
cmake -S $src -B $bld -G "Visual Studio 17 2022" -A x64 @Extra 2>&1 | Select-String -Pattern 'error|CUDA|Vulkan|Error' | Select-Object -First 15
cmake --build $bld --config Release -j 16 2>&1 | Select-String -Pattern 'error|wbench.vcxproj ->' | Select-Object -First 20
"BUILD $Name took {0:n0} s" -f ((Get-Date) - $t).TotalSeconds
