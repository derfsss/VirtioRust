param([int]$Seconds = 8)
# Per-thread CPU of the amigaone virtio-net QEMU (run-amigaone-virtionet.sh,
# -name base_a1_backup) over $Seconds: which threads are busy.
$p = Get-CimInstance Win32_Process -Filter "Name='qemu-system-ppc.exe'" | Where-Object { $_.CommandLine -match '-name base_a1_backup' }
if (-not $p) { "no qemu"; exit 1 }
$proc = Get-Process -Id $p.ProcessId
$a = @{}
foreach ($t in $proc.Threads) { try { $a[$t.Id] = $t.TotalProcessorTime.TotalMilliseconds } catch {} }
Start-Sleep -Seconds $Seconds
$proc = Get-Process -Id $p.ProcessId
$rows = foreach ($t in $proc.Threads) {
  try {
    $b = $t.TotalProcessorTime.TotalMilliseconds
    if ($a.ContainsKey($t.Id)) { [pscustomobject]@{ Thread = $t.Id; Pct = [math]::Round(($b - $a[$t.Id]) / ($Seconds * 10), 1) } }
  } catch {}
}
$rows | Sort-Object Pct -Descending | Select-Object -First 6 | Format-Table -AutoSize | Out-String
