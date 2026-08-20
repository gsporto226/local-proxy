#!/usr/bin/env pwsh
<#
.SYNOPSIS
  Instala o binário `local-proxy` a partir do último release do GitHub.

.DESCRIPTION
  Baixa o binário da plataforma/arquitetura atual do último release de
  gsporto226/local-proxy (ou de uma tag específica com -Tag), verifica o
  SHA256 publicado e instala em <InstallDir>/local-proxy(.exe).

.PARAMETER Repo
  Repositório "dono/nome" (padrão: gsporto226/local-proxy).

.PARAMETER Tag
  Tag específica do release. Se omitido, usa o último release.

.PARAMETER InstallDir
  Diretório de instalação (padrão: ~/.local/bin).

.PARAMETER AddToPath
  Se informado, adiciona InstallDir ao PATH do usuário (best-effort).

.PARAMETER SkipVerify
  Pula a verificação de SHA256.

.EXAMPLE
  ./install.ps1
#>
[CmdletBinding()]
param(
  [string]$Repo = "gsporto226/local-proxy",
  [string]$Tag,
  [string]$InstallDir,
  [switch]$AddToPath,
  [switch]$SkipVerify
)

$ErrorActionPreference = "Stop"

function Get-Os {
  $isWin = Get-Variable -Name IsWindows -ErrorAction SilentlyContinue
  $isLx = Get-Variable -Name IsLinux -ErrorAction SilentlyContinue
  $isMac = Get-Variable -Name IsOSX -ErrorAction SilentlyContinue
  if ($isWin -and $isWin.Value) { return "windows" }
  if ($isLx -and $isLx.Value) { return "linux" }
  if ($isMac -and $isMac.Value) { return "darwin" }

  try {
    if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Windows)) { return "windows" }
    if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Linux)) { return "linux" }
    if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::OSX)) { return "darwin" }
  } catch { }

  if ($env:OS -and $env:OS -like "Windows*") { return "windows" }

  if (Get-Command -Name uname -ErrorAction SilentlyContinue) {
    $s = (& uname -s).ToLowerInvariant()
    if ($s -like "*mingw*" -or $s -like "*msys*" -or $s -like "cygwin*") { return "windows" }
    if ($s -eq "darwin") { return "darwin" }
    if ($s -like "linux*") { return "linux" }
  }

  throw "Não foi possível detectar o sistema operacional."
}

function Get-Arch {
  if ($os -eq "windows") {
    $arch = $null
    try {
      $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    } catch { }
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    $arch = $arch.ToLowerInvariant()
    if ($arch -eq "x64" -or $arch -eq "amd64") { return "x86_64" }
    if ($arch -eq "arm64" -or $arch -eq "aarch64") { return "aarch64" }
    return $arch
  }

  $arch = (& uname -m).ToLowerInvariant()
  if ($arch -eq "x86_64") { return "x86_64" }
  if ($arch -eq "aarch64" -or $arch -eq "arm64") { return "aarch64" }
  return $arch
}

$os = Get-Os
$arch = Get-Arch
if ($arch -ne "x86_64") {
  throw "Ainda não publicamos binário para $os/$arch (apenas x86_64)."
}

$binName = if ($os -eq "windows") { "local-proxy.exe" } else { "local-proxy" }

if (-not $InstallDir) {
  $InstallDir = Join-Path $HOME ".local\bin"
}

$releaseUrl = if ($Tag) {
  "https://api.github.com/repos/$Repo/releases/tags/$Tag"
} else {
  "https://api.github.com/repos/$Repo/releases/latest"
}

Write-Host "> Buscando release: $releaseUrl" -ForegroundColor Cyan
$release = Invoke-RestMethod -Uri $releaseUrl -Headers @{ "User-Agent" = "local-proxy-installer"; "Accept" = "application/vnd.github+json" }
$tag = $release.tag_name

$asset = $release.assets | Where-Object { $_.name -eq $binName } | Select-Object -First 1
if (-not $asset) {
  throw "Binário '$binName' não encontrado no release $tag."
}

$tmpDir = Join-Path $env:TEMP "local-proxy-install-$PID"
New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null

try {
  $binPath = Join-Path $tmpDir $binName
  $shaUrl = $asset.browser_download_url + ".sha256"
  $shaPath = Join-Path $tmpDir ($binName + ".sha256")

  Write-Host "> Baixando $($asset.name) ($tag)..." -ForegroundColor Cyan
  Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $binPath

  if (-not $SkipVerify) {
    Write-Host "> Verificando SHA256..." -ForegroundColor Cyan
    Invoke-WebRequest -Uri $shaUrl -OutFile $shaPath
    $expected = ((Get-Content $shaPath -Raw) -split "\s+")[0].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $binPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
      throw "SHA256 não confere. esperado=$expected obtido=$actual"
    }
    Write-Host "  OK: $actual" -ForegroundColor Green
  }

  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  $dest = Join-Path $InstallDir $binName
  Copy-Item -LiteralPath $binPath -Destination $dest -Force

  if ($os -ne "windows") {
    & chmod +x $dest
  }

  Write-Host ""
  Write-Host "Instalado: $dest" -ForegroundColor Green

  if ($AddToPath) {
    if ($os -eq "windows") {
      $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
      if ($userPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
        Write-Host "> $InstallDir adicionado ao PATH do usuário (reabra o terminal)." -ForegroundColor Yellow
      }
    } else {
      $shell = Split-Path -Leaf $env:SHELL
      $rc = if ($shell -match "zsh") { "$HOME/.zshrc" } elseif ($shell -match "fish") { "$HOME/.config/fish/config.fish" } else { "$HOME/.bashrc" }
      $line = "export PATH=`"$InstallDir`:`$PATH`""
      if (-not (Select-String -Quiet -Path $rc -Pattern [regex]::Escape($InstallDir))) {
        Add-Content -Path $rc -Value $line
        Write-Host "> PATH atualizado em $rc" -ForegroundColor Yellow
      }
    }
  } else {
    Write-Host "> Para usar, adicione ao PATH: $InstallDir" -ForegroundColor Yellow
  }
}
finally {
  Remove-Item -Recurse -Force -LiteralPath $tmpDir -ErrorAction SilentlyContinue
}
