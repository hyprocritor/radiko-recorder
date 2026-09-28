param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version,
    [string]$BinaryDir = 'target/release',
    [string]$OutputDir = 'target/release-assets'
)

$ErrorActionPreference = 'Stop'
$taskRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$taskBinaryDir = [IO.Path]::GetFullPath((Join-Path $taskRoot $BinaryDir))
$taskOutputDir = [IO.Path]::GetFullPath((Join-Path $taskRoot $OutputDir))
$taskName = "radiko-recorder-v$Version-windows-x86_64"
$taskStage = Join-Path $taskOutputDir $taskName
$taskZip = Join-Path $taskOutputDir ($taskName + '.zip')
if ((Test-Path -LiteralPath $taskStage) -or (Test-Path -LiteralPath $taskZip)) {
    throw 'Release output already exists; choose a new OutputDir to avoid overwriting it.'
}
$taskExe = Join-Path $taskBinaryDir 'radiko-recorder.exe'
$taskReportedVersion = & $taskExe --version
if ($LASTEXITCODE -ne 0 -or $taskReportedVersion -ne "radiko-recorder $Version") {
    throw "Unexpected executable version: $taskReportedVersion"
}
New-Item -ItemType Directory -Path (Join-Path $taskStage 'scripts') -Force | Out-Null
Copy-Item -LiteralPath $taskExe -Destination $taskStage
Copy-Item -LiteralPath (Join-Path $taskBinaryDir 'radiko_recorder.pdb') -Destination $taskStage
foreach ($taskFile in @('README.md', 'LICENSE')) {
    Copy-Item -LiteralPath (Join-Path $taskRoot $taskFile) -Destination $taskStage
}
Copy-Item -LiteralPath (Join-Path $taskRoot 'scripts/setup-ffmpeg.ps1') -Destination (Join-Path $taskStage 'scripts')
Copy-Item -LiteralPath (Join-Path $taskRoot "docs/releases/v$Version.md") -Destination (Join-Path $taskStage 'RELEASE-NOTES.md')
Compress-Archive -LiteralPath $taskStage -DestinationPath $taskZip -CompressionLevel Optimal
$taskHash = (Get-FileHash -LiteralPath $taskZip -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path $taskOutputDir 'SHA256SUMS.txt'), "$taskHash  $taskName.zip`n", [Text.UTF8Encoding]::new($false))
Get-Item -LiteralPath $taskZip, (Join-Path $taskOutputDir 'SHA256SUMS.txt') | Select-Object Name, Length
