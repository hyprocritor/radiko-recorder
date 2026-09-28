$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$taskRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$taskTools = Join-Path $taskRoot 'tools'
New-Item -ItemType Directory -Force -Path $taskTools | Out-Null
$taskArchive = Join-Path $taskTools 'ffmpeg-release-essentials.zip'
$taskUrl = 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip'
Write-Output 'Downloading FFmpeg essentials from the Windows distributor linked by ffmpeg.org...'
Invoke-WebRequest -Uri $taskUrl -OutFile $taskArchive
$taskHashText = (Invoke-WebRequest -Uri ($taskUrl + '.sha256')).Content
$taskExpected = [regex]::Match([string]$taskHashText, '[a-fA-F0-9]{64}').Value
$taskActual = (Get-FileHash -LiteralPath $taskArchive -Algorithm SHA256).Hash
if (-not $taskExpected -or $taskActual -ne $taskExpected) { throw 'FFmpeg checksum mismatch; archive retained for inspection.' }
$taskExtract = Join-Path $taskTools 'ffmpeg'
Expand-Archive -LiteralPath $taskArchive -DestinationPath $taskExtract -Force
$taskBinary = Get-ChildItem -LiteralPath $taskExtract -Filter ffmpeg.exe -Recurse | Select-Object -First 1
if (-not $taskBinary) { throw 'No ffmpeg.exe found in archive.' }
$taskBinDir = Join-Path $taskTools 'bin'
New-Item -ItemType Directory -Force -Path $taskBinDir | Out-Null
Copy-Item -LiteralPath $taskBinary.FullName -Destination (Join-Path $taskBinDir 'ffmpeg.exe') -Force
Copy-Item -LiteralPath (Join-Path $taskBinary.DirectoryName 'ffprobe.exe') -Destination (Join-Path $taskBinDir 'ffprobe.exe') -Force
Write-Output ('FFmpeg: ' + (Join-Path $taskBinDir 'ffmpeg.exe'))
Write-Output ('SHA256: ' + $taskActual)
& $taskBinary.FullName -version | Select-Object -First 1
