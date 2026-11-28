# Installs Rojo Manager on Windows: downloads the latest release, checks it against the release's
# SHA256SUMS.txt, extracts it, starts it, and adds its MCP server to Claude Code and Codex when they
# are installed. Run again any time: an existing copy is kept and only started and added.
#
#   irm https://raw.githubusercontent.com/lClayk/Rojo-Manager/main/install.ps1 | iex
#
# Another folder: & ([scriptblock]::Create((irm <that url>))) -Path 'D:\Tools\RojoManager'
param([string]$Path)
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

# Without -Path: the running Rojo Manager's folder, so agents get the copy that is in use.
if (-not $Path) {
    $running = Get-Process RojoManager -ErrorAction SilentlyContinue | Where-Object Path | Select-Object -First 1
    $Path = if ($running) { Split-Path $running.Path } else { "$env:LOCALAPPDATA\Programs\RojoManager" }
}
# Used as a literal path from here on, so a [ or ] in it is not a wildcard.
$Path = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)

$app = Join-Path $Path 'RojoManager.exe'
$mcp = Join-Path $Path 'RojoManagerMCP.exe'
if (Test-Path -LiteralPath $app) {
    Write-Host "Rojo Manager is already in $Path. It updates itself."
} else {
    $release = Invoke-RestMethod 'https://api.github.com/repos/lClayk/Rojo-Manager/releases/latest'
    $zip = $release.assets | Where-Object name -like 'RojoManager-*-windows-x64.zip' | Select-Object -First 1
    $sums = $release.assets | Where-Object name -eq 'SHA256SUMS.txt' | Select-Object -First 1
    if (-not $zip -or -not $sums) { throw "Release $($release.tag_name) has no Windows download." }
    $temp = Join-Path ([IO.Path]::GetTempPath()) "rojo-manager-install-$PID"
    New-Item -ItemType Directory -Force $temp | Out-Null
    try {
        Write-Host "Downloading $($zip.name)..."
        $archive = Join-Path $temp $zip.name
        Invoke-WebRequest $zip.browser_download_url -OutFile $archive -UseBasicParsing
        $sumsFile = Join-Path $temp $sums.name
        Invoke-WebRequest $sums.browser_download_url -OutFile $sumsFile -UseBasicParsing
        $line = "^([0-9a-f]{64})\s+$([regex]::Escape($zip.name))$"
        $expected = Get-Content $sumsFile | ForEach-Object { if ($_.Trim() -match $line) { $Matches[1] } }
        $actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLower()
        if (-not $expected -or $actual -ne $expected) { throw "$($zip.name) does not match SHA256SUMS.txt." }
        Expand-Archive $archive $temp
        # The archive holds one RojoManager folder, which becomes $Path: an empty folder there is replaced.
        if (Test-Path -LiteralPath $Path) {
            if (Get-ChildItem -Force -LiteralPath $Path) { throw "$Path is not empty. Pass an empty or new folder with -Path." }
            Remove-Item -LiteralPath $Path
        }
        New-Item -ItemType Directory -Force (Split-Path $Path) | Out-Null
        Move-Item -LiteralPath (Join-Path $temp 'RojoManager') -Destination $Path
    } finally {
        Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
    }
    Write-Host "Installed Rojo Manager $($release.tag_name) in $Path"
}

Start-Process $app
# Each agent's CLI registers the server itself; an old entry is replaced so a moved app is found.
$added = @()
foreach ($agent in @(@{ Name = 'Claude Code'; Cli = 'claude'; Scope = @('--scope', 'user') },
        @{ Name = 'Codex'; Cli = 'codex'; Scope = @() })) {
    # Its .exe or .cmd, never an npm .ps1 shim, which a Restricted execution policy blocks.
    $cli = Get-Command $agent.Cli -CommandType Application -ErrorAction SilentlyContinue |
        Where-Object { $_.Extension -in '.exe', '.cmd', '.bat' } | Select-Object -First 1
    if (-not $cli) { continue }
    $scope = $agent.Scope
    & $cli.Source mcp remove @scope rojo-manager *> $null
    & $cli.Source mcp add @scope rojo-manager -- $mcp *> $null
    if ($LASTEXITCODE -eq 0) { $added += $agent.Name } else { Write-Host "Could not add Rojo Manager to $($agent.Name)." }
}
if ($added) { Write-Host "Added to $($added -join ' and '). Start a new agent session to load its tools." }
Write-Host "MCP server: $mcp"
Write-Host "Check the setup: & '$mcp' setup"
