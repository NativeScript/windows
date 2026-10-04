#Requires -Version 5.1
<#
.SYNOPSIS
    Builds the NativeScript Windows framework for a chosen JS engine and stages it into
    template/framework/ ready for npm publish.

.DESCRIPTION
    The framework is identical across engines — the same WinUI 3 app template, dotnet-bridge, and
    tools — differing only in the runtime `nativescript.dll` staged into framework/libs/. `-Engine`
    selects which runtime is built into that slot, so producing an engine variant is just a flag:

        build.ps1                     # @nativescript/windows        (classic V8, default)
        build.ps1 -Engine quickjs     # @nativescript/windows-quickjs
        build.ps1 -Engine hermes      # @nativescript/windows-hermes
        build.ps1 -Engine v8          # @nativescript/windows-v8
        build.ps1 -Engine jsc         # @nativescript/windows-jsc

    Classic (V8) builds the workspace `nativescript` cdylib for x64 + arm64, release + devtools:

        framework/libs/x64/nativescript.dll            # release, no devtools
        framework/libs/arm64/nativescript.dll
        framework/libs/devtools/x64/nativescript.dll   # release-with-devtools
        framework/libs/devtools/arm64/nativescript.dll

    A napi engine (quickjs/hermes/v8/jsc) builds that engine package's cdylib (its `host_dll`
    feature) and stages it — plus any engine runtime DLLs (hermes.dll, JavaScriptCore.dll, …) —
    as framework/libs/x64/nativescript.dll. Engine variants are x64-only, no devtools (the
    prebuilt/compiled engines are x64 and expose no inspector).

    Run from anywhere; the script locates the repo root via its own path.

.PARAMETER Engine
    Which JS engine to build into the runtime DLL: classic (default) | v8 | quickjs | hermes | jsc.

.PARAMETER SkipArm64
    Skip the arm64 cross-compilation steps (faster, x64-only output). Implied for engine variants.

.PARAMETER SkipRelease
    Skip the stripped release builds (devtools builds only).

.PARAMETER SkipDevtools
    Skip the devtools builds (release builds only).
#>
param(
    [ValidateSet('classic', 'v8', 'quickjs', 'hermes', 'jsc')]
    [string]$Engine = 'classic',
    [switch]$SkipArm64,
    [switch]$SkipRelease,
    [switch]$SkipDevtools,
    [switch]$SkipDotnet
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# Paths
$ScriptDir  = $PSScriptRoot
$RepoRoot   = (Resolve-Path (Join-Path $ScriptDir "..")).Path
Write-Host "Repo root  : $RepoRoot"

# Engine variants: which package produces the runtime DLL, its cdylib name, extra runtime DLLs to
# stage beside it, any extra cargo features, and the npm package name the framework publishes as.
# `classic` is the default V8 workspace crate; the rest are the napi engine packages.
$EngineMap = @{
    classic = @{ Package = 'nativescript'; NpmName = '@nativescript/windows' }
    v8       = @{ Dir = 'windows-v8';      Lib = 'windows_v8';      Dlls = @();                                                                Features = @('host_dll'); NpmName = '@nativescript/windows-v8' }
    quickjs  = @{ Dir = 'windows-quickjs'; Lib = 'windows_quickjs'; Dlls = @();                                                                Features = @('host_dll'); NpmName = '@nativescript/windows-quickjs' }
    hermes   = @{ Dir = 'windows-hermes';  Lib = 'windows_hermes';  Dlls = @('hermes.dll', 'hermes-icu.dll');                                  Features = @('host_dll'); NpmName = '@nativescript/windows-hermes' }
    jsc      = @{ Dir = 'windows-jsc';      Lib = 'windows_jsc';     Dlls = @('JavaScriptCore.dll', 'icuin77.dll', 'icuuc77.dll', 'icudt77.dll'); Features = @('host_dll', 'jsc_link'); NpmName = '@nativescript/windows-jsc' }
}
$EngineInfo = $EngineMap[$Engine]
if ($Engine -ne 'classic') {
    # The prebuilt/compiled engines are x64 with no inspector; engine variants are x64 release only.
    $SkipArm64 = $true
    $SkipDevtools = $true
}

# The framework is staged into the template for classic, or into the engine package for a variant
# (so each variant is a self-contained, publishable @nativescript/windows-<engine> with the same
# framework layout). Shared scaffolding (dotnet-bridge, tools, app template) is copied from the
# template at build time rather than duplicated in git — only the runtime DLL differs per engine.
$TemplateFramework = Join-Path $ScriptDir "framework"
if ($Engine -eq 'classic') {
    $FrameworkRoot = $TemplateFramework
} else {
    $FrameworkRoot = Join-Path $RepoRoot "packages\$($EngineInfo.Dir)\framework"
}
$FrameworkLibs = Join-Path $FrameworkRoot "libs"
$ToolsDir = Join-Path $FrameworkRoot "tools"

Write-Host "Engine     : $Engine  ->  $($EngineInfo.NpmName)"
Write-Host "Framework  : $FrameworkRoot"

# Helper
function Copy-Dll {
    param([string]$Src, [string]$Dest)
    $null = New-Item -ItemType Directory -Force -Path (Split-Path $Dest)
    Copy-Item -Force $Src $Dest
    Write-Host "  copied  $(Split-Path $Src -Leaf)  →  $(Resolve-Path $Dest -Relative 2>$null)"
}

function Build-Crate {
    param(
        [string]$Profile,       # e.g. "release" or "release-with-devtools"
        [string]$Target,        # e.g. "x86_64-pc-windows-msvc"
        [string[]]$ExtraArgs    # e.g. @("--features","devtools")
    )
    $args = @("build", "--profile", $Profile, "-p", "nativescript", "--target", $Target) + $ExtraArgs
    Write-Host ""
    Write-Host "cargo $($args -join ' ')"
    Push-Location $RepoRoot
    try {
        & cargo @args
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
}

# Engine variant: stage the shared framework from the template, build the engine cdylib as
# framework/libs/x64/nativescript.dll, and finish. The template must already have been built once
# (classic) so its dotnet-bridge/tools/app-template scaffolding exists to copy.
if ($Engine -ne 'classic') {
    Write-Host "`n=== Engine framework: $Engine ===" -ForegroundColor Cyan

    if (-not (Test-Path (Join-Path $TemplateFramework 'tools'))) {
        throw "Template framework not built yet. Run './build.ps1' (classic) once before building an engine variant."
    }

    # Copy the shared scaffolding (everything except libs/, which is the per-engine runtime DLL).
    Write-Host "Staging shared framework from $TemplateFramework -> $FrameworkRoot"
    if (Test-Path $FrameworkRoot) { Remove-Item -Recurse -Force $FrameworkRoot }
    $null = New-Item -ItemType Directory -Force -Path $FrameworkRoot
    Get-ChildItem -Path $TemplateFramework -Force | Where-Object { $_.Name -ne 'libs' } | ForEach-Object {
        Copy-Item -Recurse -Force $_.FullName (Join-Path $FrameworkRoot $_.Name)
    }

    # Build the engine cdylib (its `host_dll` feature) and stage it as the runtime DLL. `--lib`
    # scopes this to the [lib] target only — the package's `nativescript-windows` bin (dev/bench
    # only, never shipped; see packages/<engine>/README.md) would otherwise also build here since
    # its `required-features` are satisfied by `host_dll`.
    Write-Host "`n=== Engine runtime: $Engine (x64 release) ===" -ForegroundColor Cyan
    $engineManifest = Join-Path $RepoRoot "packages\$($EngineInfo.Dir)\Cargo.toml"
    $cargoArgs = @("build", "--release", "--lib", "--manifest-path", $engineManifest,
                   "--features", ($EngineInfo.Features -join ","))
    Write-Host "cargo $($cargoArgs -join ' ')"
    Push-Location $RepoRoot
    try {
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { throw "engine cdylib build failed (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }

    # Excluded-from-workspace packages build into their own target/ dir.
    $engineOut = Join-Path $RepoRoot "packages\$($EngineInfo.Dir)\target\release"
    $srcDll = Join-Path $engineOut "$($EngineInfo.Lib).dll"
    if (-not (Test-Path $srcDll)) { throw "engine cdylib not found: $srcDll" }
    Copy-Dll -Src $srcDll -Dest (Join-Path $FrameworkLibs "x64\nativescript.dll")
    foreach ($extra in $EngineInfo.Dlls) {
        $extraSrc = Join-Path $engineOut $extra
        if (Test-Path $extraSrc) {
            Copy-Dll -Src $extraSrc -Dest (Join-Path $FrameworkLibs "x64\$extra")
        } else {
            Write-Host "  WARN: expected engine DLL not found: $extra" -ForegroundColor Yellow
        }
    }

    Write-Host ""
    Write-Host "Done. $($EngineInfo.NpmName) framework staged at $FrameworkRoot" -ForegroundColor Green
    return
}

# DotNet Bridge
# Copies the dotnet-bridge source (csproj + .cs files) into
# template/framework/dotnet-bridge/ so it ships inside the npm package.
# The actual `dotnet publish` is deferred to the app's MSBuild process —
# see the PublishDotNetBridge target in __PROJECT_NAME__.csproj.

if (-not $SkipDotnet) {
    Write-Host "`n=== DotNet Bridge (source copy) ===" -ForegroundColor Cyan
    $BridgeSrc  = Join-Path $RepoRoot "dotnet-bridge"
    $BridgeDest = Join-Path $ScriptDir "framework\dotnet-bridge"

    # Wipe the destination so stale files don't linger between runs.
    if (Test-Path $BridgeDest) { Remove-Item -Recurse -Force $BridgeDest }
    $null = New-Item -ItemType Directory -Force -Path $BridgeDest

    # Copy only source files; exclude build/publish artefacts.
    Get-ChildItem -Path $BridgeSrc -Recurse |
        Where-Object {
            $rel = $_.FullName.Substring($BridgeSrc.Length + 1)
            $rel -notmatch '^(bin|obj|publish)(\\|$)'
        } |
        ForEach-Object {
            $dest = Join-Path $BridgeDest $_.FullName.Substring($BridgeSrc.Length + 1)
            if ($_.PSIsContainer) {
                $null = New-Item -ItemType Directory -Force -Path $dest
            } else {
                Copy-Item -Force $_.FullName $dest
                Write-Host "  copied  $($_.FullName.Substring($BridgeSrc.Length + 1))"
            }
        }
}

# ManifestMerger MSBuild task
Write-Host "`n=== ManifestMerger MSBuild task ===" -ForegroundColor Cyan
if (-not (Test-Path $ToolsDir)) { New-Item -ItemType Directory -Force -Path $ToolsDir | Out-Null }
$ManifestMergerProj = Join-Path $RepoRoot "tools\ManifestMerger\ManifestMerger.csproj"
$ManifestMergerOut = Join-Path $ToolsDir "ManifestMerger"
if (Test-Path $ManifestMergerProj) {
    & dotnet publish $ManifestMergerProj -c Release -o $ManifestMergerOut --nologo
    if ($LASTEXITCODE -ne 0) {
        Write-Host "dotnet publish for ManifestMerger failed (exit $LASTEXITCODE)." -ForegroundColor Yellow
    } else {
        Write-Host "  published ManifestMerger -> $(Resolve-Path $ManifestMergerOut -Relative)"
    }
} else {
    Write-Host "ManifestMerger project not found: $ManifestMergerProj" -ForegroundColor Yellow
}

# Targets
$Targets = @(
    @{ Arch = "x64";   RustTarget = "x86_64-pc-windows-msvc"  }
)
if (-not $SkipArm64) {
    $Targets += @{ Arch = "arm64"; RustTarget = "aarch64-pc-windows-msvc" }
}

# Build the runtime DLL (classic V8): the workspace `nativescript` cdylib, x64 + arm64,
# release + devtools. (Engine variants build + stage their DLL earlier and return.)
foreach ($t in $Targets) {
    $arch        = $t.Arch
    $rustTarget  = $t.RustTarget

    if (-not $SkipRelease) {
        Write-Host "`n=== Release ($arch) ===" -ForegroundColor Cyan
        $releaseArgs = @()
        Build-Crate -Profile "release" -Target $rustTarget -ExtraArgs $releaseArgs
        $dll = Join-Path $RepoRoot "target\$rustTarget\release\nativescript.dll"
        Copy-Dll -Src $dll -Dest (Join-Path $FrameworkLibs "$arch\nativescript.dll")
    }

    if (-not $SkipDevtools) {
        Write-Host "`n=== Release-with-devtools ($arch) ===" -ForegroundColor Cyan
        $devtoolsArgs = @("--features", "devtools")
        Build-Crate -Profile "release-with-devtools" -Target $rustTarget -ExtraArgs $devtoolsArgs
        $dll = Join-Path $RepoRoot "target\$rustTarget\release-with-devtools\nativescript.dll"
        Copy-Dll -Src $dll -Dest (Join-Path $FrameworkLibs "devtools\$arch\nativescript.dll")
    }
}

# dotnet-tool prebuilt binaries
Write-Host "`n=== Build dotnet-tool prebuilt binaries ===" -ForegroundColor Cyan
# Place prebuilt tools into the framework folder so they are included in the
# packaged framework (npm publish copies framework/* into the package root).
if (-not (Test-Path $ToolsDir)) { New-Item -ItemType Directory -Force -Path $ToolsDir | Out-Null }


foreach ($t in $Targets) {
    $arch = $t.Arch
    $rustTarget = $t.RustTarget
    Write-Host "Building dotnet-tool for $arch ($rustTarget)..."
    Push-Location $RepoRoot
    try {
        & cargo build -p dotnet-tool --release --target $rustTarget
        $buildExit = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($buildExit -ne 0) {
        Write-Host "cargo build for dotnet-tool failed for target $rustTarget (exit $buildExit). Skipping copy for $arch." -ForegroundColor Yellow
        continue
    }
    $candidate = Join-Path $RepoRoot "target\$rustTarget\release\dotnet-tool.exe"
    if (Test-Path $candidate) {
        $dest = Join-Path $ToolsDir "dotnet-tool-$arch.exe"
        Copy-Item -Force $candidate $dest
        Write-Host "  copied dotnet-tool -> $(Resolve-Path $dest -Relative)"
    } else {
        Write-Host "Expected build output not found: $candidate" -ForegroundColor Yellow
    }
}

# Provide a generic `dotnet-tool.exe` fallback (copy x64 if available)
$x64Path = Join-Path $ToolsDir "dotnet-tool-x64.exe"
if (Test-Path $x64Path) {
    Copy-Item -Force $x64Path (Join-Path $ToolsDir "dotnet-tool.exe")
    Write-Host "  copied dotnet-tool-x64.exe -> dotnet-tool.exe"
}

# nsbundle_pack prebuilt binaries (source-protection packer — seals an app's webpack output
# directory into an encrypted app.nsbundle. It's a [[bin]]
# target inside the `runtime` crate rather than its own package, unlike dotnet-tool, but is
# otherwise staged identically so nativescript-cli can resolve/invoke it the same way.
Write-Host "`n=== Build nsbundle_pack prebuilt binaries ===" -ForegroundColor Cyan
if (-not (Test-Path $ToolsDir)) { New-Item -ItemType Directory -Force -Path $ToolsDir | Out-Null }

foreach ($t in $Targets) {
    $arch = $t.Arch
    $rustTarget = $t.RustTarget
    Write-Host "Building nsbundle_pack for $arch ($rustTarget)..."
    Push-Location $RepoRoot
    try {
        & cargo build -p runtime --bin nsbundle_pack --release --target $rustTarget
        $buildExit = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($buildExit -ne 0) {
        Write-Host "cargo build for nsbundle_pack failed for target $rustTarget (exit $buildExit). Skipping copy for $arch." -ForegroundColor Yellow
        continue
    }
    $candidate = Join-Path $RepoRoot "target\$rustTarget\release\nsbundle_pack.exe"
    if (Test-Path $candidate) {
        $dest = Join-Path $ToolsDir "nsbundle_pack-$arch.exe"
        Copy-Item -Force $candidate $dest
        Write-Host "  copied nsbundle_pack -> $(Resolve-Path $dest -Relative)"
    } else {
        Write-Host "Expected build output not found: $candidate" -ForegroundColor Yellow
    }
}

# Provide a generic `nsbundle_pack.exe` fallback (copy x64 if available)
$nsbundlePackX64Path = Join-Path $ToolsDir "nsbundle_pack-x64.exe"
if (Test-Path $nsbundlePackX64Path) {
    Copy-Item -Force $nsbundlePackX64Path (Join-Path $ToolsDir "nsbundle_pack.exe")
    Write-Host "  copied nsbundle_pack-x64.exe -> nsbundle_pack.exe"
}

# typings-generator: shipped so `ns typings windows` can generate WinRT/.NET typings.
Write-Host "`n=== Build typings-generator prebuilt binaries ===" -ForegroundColor Cyan

foreach ($t in $Targets) {
    $arch = $t.Arch
    $rustTarget = $t.RustTarget
    Write-Host "Building typings-generator for $arch ($rustTarget)..."
    Push-Location $RepoRoot
    try {
        & cargo build -p typings-generator --release --target $rustTarget
        $buildExit = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($buildExit -ne 0) {
        Write-Host "cargo build for typings-generator failed for target $rustTarget (exit $buildExit). Skipping copy for $arch." -ForegroundColor Yellow
        continue
    }
    $candidate = Join-Path $RepoRoot "target\$rustTarget\release\typings-generator.exe"
    if (Test-Path $candidate) {
        $dest = Join-Path $ToolsDir "typings-generator-$arch.exe"
        Copy-Item -Force $candidate $dest
        Write-Host "  copied typings-generator -> $(Resolve-Path $dest -Relative)"
    } else {
        Write-Host "Expected build output not found: $candidate" -ForegroundColor Yellow
    }
}

$x64Path = Join-Path $ToolsDir "typings-generator-x64.exe"
if (Test-Path $x64Path) {
    Copy-Item -Force $x64Path (Join-Path $ToolsDir "typings-generator.exe")
    Write-Host "  copied typings-generator-x64.exe -> typings-generator.exe"
}

# dotnet-typings-gen: .NET sub-tool typings-generator shells to for managed/non-Windows assemblies.
$DotnetTypingsProj = Join-Path $RepoRoot "typings-generator\dotnet-src\dotnet-typings-gen.csproj"
if (Test-Path $DotnetTypingsProj) {
    Write-Host "`n=== Publish dotnet-typings-gen (.NET typings sub-tool) ===" -ForegroundColor Cyan
    $DotnetTypingsOut = Join-Path $ToolsDir "dotnet-typings-gen"
    & dotnet publish $DotnetTypingsProj -c Release -o $DotnetTypingsOut --nologo
    if ($LASTEXITCODE -ne 0) {
        Write-Host "dotnet publish for dotnet-typings-gen failed (exit $LASTEXITCODE)." -ForegroundColor Yellow
    } else {
        Write-Host "  published dotnet-typings-gen -> $(Resolve-Path $DotnetTypingsOut -Relative)"
    }
}

Write-Host ""
Write-Host "Done." -ForegroundColor Green
Write-Host ""
Write-Host "libs layout:"
Get-ChildItem -Recurse $FrameworkLibs -Filter "*.dll" |
    ForEach-Object { "  " + $_.FullName.Substring($FrameworkLibs.Length + 1) }
