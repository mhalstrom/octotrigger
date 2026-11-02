# Installs octotrigger on Windows, with no administrator:
#
#   irm https://raw.githubusercontent.com/mhalstrom/octotrigger/main/install.ps1 | iex
#
# It downloads the newest release for this computer (x64 or Arm64) from
# GitHub, checks its SHA-256, puts octotrigger.exe in
# %LOCALAPPDATA%\Programs\octotrigger, and adds that folder to your user PATH.
# Then run: octotrigger setup
#
# It runs in a script block of its own, so it changes no setting of your
# PowerShell window except its PATH.

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 on an older Windows 10 may offer no TLS 1.2 by itself.
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'mhalstrom/octotrigger'
    $arm = $env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -or $env:PROCESSOR_ARCHITEW6432 -eq 'ARM64'
    $target = if ($arm) { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
    $zipName = "octotrigger-$target.zip"

    $release = Invoke-RestMethod -UseBasicParsing `
        -Uri "https://api.github.com/repos/$repo/releases/latest" `
        -Headers @{ 'User-Agent' = 'octotrigger-install' }
    $zipAsset = $release.assets | Where-Object { $_.name -eq $zipName }
    $sumAsset = $release.assets | Where-Object { $_.name -eq "$zipName.sha256" }
    if (-not $zipAsset -or -not $sumAsset) {
        throw "octotrigger $($release.tag_name) has no build for $target."
    }

    $temp = Join-Path ([IO.Path]::GetTempPath()) ("octotrigger-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $temp | Out-Null
    try {
        $zip = Join-Path $temp $zipName
        $sum = "$zip.sha256"
        Invoke-WebRequest -UseBasicParsing -Uri $zipAsset.browser_download_url -OutFile $zip
        Invoke-WebRequest -UseBasicParsing -Uri $sumAsset.browser_download_url -OutFile $sum
        $want = ((Get-Content -Raw -Path $sum).Trim() -split '\s+')[0].ToLower()
        $have = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
        if ($have -ne $want) {
            throw "The download of $zipName does not match its SHA-256; nothing was installed."
        }

        $folder = Join-Path $env:LOCALAPPDATA 'Programs\octotrigger'
        New-Item -ItemType Directory -Force -Path $folder | Out-Null
        Expand-Archive -Force -Path $zip -DestinationPath $folder
    } finally {
        Remove-Item -Recurse -Force -Path $temp -ErrorAction SilentlyContinue
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | Where-Object { $_ } | ForEach-Object { $_.TrimEnd('\') })
    if ($entries -notcontains $folder) {
        $newPath = if ($userPath) { "$($userPath.TrimEnd(';'));$folder" } else { $folder }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    }
    # This window too, so the next command finds it.
    if (@($env:Path -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $folder) {
        $env:Path = "$env:Path;$folder"
    }

    Write-Host "octotrigger $($release.tag_name) is installed in $folder."
    Write-Host 'Next: octotrigger setup'
}
