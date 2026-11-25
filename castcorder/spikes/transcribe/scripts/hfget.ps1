# Download a URL, resolving every host via 1.1.1.1 (local DNS returns a bogus IP for huggingface.co).
param([string]$Url, [string]$Out)
$u = $Url
for ($i = 0; $i -lt 8; $i++) {
    $h = ([Uri]$u).Host
    $ip = (Resolve-DnsName $h -Server 1.1.1.1 -Type A | Where-Object { $_.IPAddress } | Select-Object -First 1).IPAddress
    $next = curl.exe -sS -o NUL -w "%{redirect_url}" --resolve "${h}:443:$ip" $u
    if (-not $next) {
        curl.exe -sS -L -o $Out --resolve "${h}:443:$ip" $u
        Write-Output ("{0} bytes -> {1}" -f (Get-Item $Out).Length, $Out)
        return
    }
    if ($next.StartsWith('/')) { $next = "https://$h$next" }
    $u = $next
}
Write-Error "too many redirects"
