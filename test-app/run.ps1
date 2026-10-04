#!/usr/bin/env pwsh
<#
.SYNOPSIS
  Builds and runs the Windows runtime test app (the counterpart of the Android/iOS test-app suites).

.DESCRIPTION
  Generates a platform project from template/framework the way `ns platform add windows` +
  `ns prepare windows` do, then builds and launches it:
    - template/framework/__PROJECT_NAME__   -> platforms/windows/TestRunner (placeholders replaced)
    - test-app/app                          -> TestRunner/app             (JS specs + runner)
    - test-app/App_Resources/Windows        -> TestRunner/App_Resources/Windows (C# fixtures in src/)
    - test-app/plugins/<name>/platforms/windows -> TestRunner/plugins/<name> (plugin C# sources)
  The freshly built nativescript.dll and the dotnet-bridge sources from this repo are used, and the
  dotnet-tool runs from source via cargo, so the run reflects the working tree.

  The app writes JUnit XML and a plain log next to the executable, then exits with the number of
  failed specs. This script prints the log and exits non-zero when any spec failed.

.EXAMPLE
  pwsh test-app/run.ps1
  pwsh test-app/run.ps1 -Filter "Threading"
  pwsh test-app/run.ps1 -SkipRuntimeBuild
  pwsh test-app/run.ps1 -EngineDll packages/windows-v8/target/release/windows_v8.dll   # a napi engine
#>
param(
    [ValidateSet("Debug", "Release")]
    [string]$Configuration = "Debug",
    [ValidateSet("x64", "arm64")]
    [string]$Platform = "x64",
    [switch]$SkipRuntimeBuild,
    # Another runtime DLL to test instead of the classic one, e.g. an engine package's cdylib
    # (packages/windows-<engine>, `cargo build --release --features host_dll`); DLLs next to it
    # (engine sidecars) are deployed too.
    [string]$EngineDll = "",
    [switch]$Clean,
    [string]$Filter = "",
    [int]$TimeoutSeconds = 300
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$TestAppDir = $PSScriptRoot
$RepoRoot = Resolve-Path (Join-Path $TestAppDir "..")
$Framework = Join-Path $RepoRoot "template\framework"
$PlatformsDir = Join-Path $TestAppDir "platforms\windows"
$ProjectName = "TestRunner"
$AppId = "org.nativescript.windows.testrunner"
$ProjectDir = Join-Path $PlatformsDir $ProjectName

function Step([string]$msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# ── 1. runtime ───────────────────────────────────────────────────────────────
$rustTarget = if ($Platform -eq "arm64") { "aarch64-pc-windows-msvc" } else { "x86_64-pc-windows-msvc" }
$cargoProfile = if ($Configuration -eq "Debug") { "dev" } else { "release" }
$profileDir = if ($Configuration -eq "Debug") { "debug" } else { "release" }
$hostTarget = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "aarch64-pc-windows-msvc" } else { "x86_64-pc-windows-msvc" }
$runtimeDll = if ($rustTarget -eq $hostTarget) {
    Join-Path $RepoRoot "target\$profileDir\nativescript.dll"
} else {
    Join-Path $RepoRoot "target\$rustTarget\$profileDir\nativescript.dll"
}
if ($EngineDll) {
    $runtimeDll = (Resolve-Path $EngineDll).Path
    $SkipRuntimeBuild = $true
}
if (-not $SkipRuntimeBuild) {
    Step "Building nativescript.dll ($cargoProfile, $rustTarget)"
    Push-Location $RepoRoot
    try {
        if ($rustTarget -eq $hostTarget) { cargo build -p nativescript --profile $cargoProfile }
        else { cargo build -p nativescript --profile $cargoProfile --target $rustTarget }
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    } finally { Pop-Location }
}
if (-not (Test-Path $runtimeDll)) { throw "Runtime DLL not found at $runtimeDll (run without -SkipRuntimeBuild)" }

# ── 2. platform project (ns platform add) ────────────────────────────────────
if ($Clean -and (Test-Path $PlatformsDir)) {
    Step "Removing $PlatformsDir"
    Remove-Item -Recurse -Force $PlatformsDir
}
New-Item -ItemType Directory -Force -Path $PlatformsDir | Out-Null

function Copy-Tree([string]$src, [string]$dest, [string[]]$excludeDirs = @("bin", "obj")) {
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    Get-ChildItem -LiteralPath $src -Force | ForEach-Object {
        if ($_.PSIsContainer) {
            if ($excludeDirs -notcontains $_.Name) { Copy-Tree $_.FullName (Join-Path $dest $_.Name) $excludeDirs }
        } else {
            Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $dest $_.Name) -Force
        }
    }
}

Step "Generating platform project from template/framework"
Copy-Tree (Join-Path $Framework "__PROJECT_NAME__") $ProjectDir
$placeholderCsproj = Join-Path $ProjectDir "__PROJECT_NAME__.csproj"
$csproj = Join-Path $ProjectDir "$ProjectName.csproj"
if (Test-Path $placeholderCsproj) { Move-Item -Force $placeholderCsproj $csproj }
$textExt = @(".cs", ".csproj", ".xaml", ".xml", ".json", ".appxmanifest", ".props", ".targets")
Get-ChildItem -LiteralPath $ProjectDir -Recurse -File |
    Where-Object { $textExt -contains $_.Extension.ToLowerInvariant() -and $_.FullName -notmatch "\\(bin|obj)\\" } |
    ForEach-Object {
        $content = [IO.File]::ReadAllText($_.FullName)
        if ($content.Contains("__PROJECT_NAME__") -or $content.Contains("__APP_IDENTIFIER__")) {
            $content = $content.Replace("__PROJECT_NAME__", $ProjectName).Replace("__APP_IDENTIFIER__", $AppId)
            [IO.File]::WriteAllText($_.FullName, $content)
        }
    }

# Bridge from source (not the template's synced copy) and the freshly built runtime.
Copy-Tree (Join-Path $RepoRoot "dotnet-bridge") (Join-Path $PlatformsDir "dotnet-bridge") @("bin", "obj", "publish", "publish_build")
$bridgePublish = Join-Path $PlatformsDir "dotnet-bridge\publish"
if (Test-Path $bridgePublish) { Remove-Item -Recurse -Force $bridgePublish }
$libDir = Join-Path $PlatformsDir "libs\$Platform"
# Start from an empty libs dir and no deployed runtime: the csproj copies with PreserveNewest, and
# a switched DLL can be older than the one already in bin\.
if (Test-Path $libDir) { Remove-Item -Recurse -Force $libDir }
New-Item -ItemType Directory -Force -Path $libDir | Out-Null
Get-ChildItem -LiteralPath (Join-Path $ProjectDir "bin") -Filter *.dll -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -eq "nativescript.dll" -or ($EngineDll -and (Test-Path (Join-Path (Split-Path $runtimeDll) $_.Name))) } |
    Remove-Item -Force
Copy-Item -Force $runtimeDll (Join-Path $libDir "nativescript.dll")
if ($EngineDll) {
    Get-ChildItem -LiteralPath (Split-Path $runtimeDll) -Filter *.dll |
        Where-Object { $_.FullName -ne $runtimeDll } |
        ForEach-Object { Copy-Item -Force $_.FullName $libDir }
}
# The ManifestMerger task ships prebuilt; dotnet-tool runs from source (cargo) because the
# template's tools\*.exe are not copied, which makes the csproj fall back to `cargo run`.
Copy-Tree (Join-Path $Framework "tools\ManifestMerger") (Join-Path $PlatformsDir "tools\ManifestMerger")

# ── 3. prepare (app, App_Resources, plugins) ─────────────────────────────────
Step "Preparing app, App_Resources and plugins"
$appDest = Join-Path $ProjectDir "app"
if (Test-Path $appDest) { Remove-Item -Recurse -Force $appDest }
Copy-Tree (Join-Path $TestAppDir "app") $appDest
$resDest = Join-Path $ProjectDir "App_Resources\Windows"
if (Test-Path $resDest) { Remove-Item -Recurse -Force $resDest }
Copy-Tree (Join-Path $TestAppDir "App_Resources\Windows") $resDest

# Mirrors WindowsProjectService.preparePluginNativeCode/prepareProject in the CLI: stage each
# plugin's platforms/windows folder under plugins/<name>/ and import its plugin.props/targets.
$pluginsDest = Join-Path $ProjectDir "plugins"
if (Test-Path $pluginsDest) { Remove-Item -Recurse -Force $pluginsDest }
New-Item -ItemType Directory -Force -Path $pluginsDest | Out-Null
$propsLines = @('<?xml version="1.0" encoding="utf-8"?>', '<Project>')
$targetsLines = @('<?xml version="1.0" encoding="utf-8"?>', '<Project>')
$pluginsSrc = Join-Path $TestAppDir "plugins"
if (Test-Path $pluginsSrc) {
    Get-ChildItem -LiteralPath $pluginsSrc -Directory | ForEach-Object {
        $native = Join-Path $_.FullName "platforms\windows"
        if (-not (Test-Path $native)) { return }
        Copy-Tree $native (Join-Path $pluginsDest $_.Name)
        foreach ($f in @("plugin.props", "plugin.targets")) {
            $p = Join-Path $_.FullName $f
            if (Test-Path $p) { Copy-Item -Force $p (Join-Path $pluginsDest "$($_.Name)\$f") }
        }
        $propsLines += "  <Import Project=`"`$(MSBuildThisFileDirectory)$($_.Name)\plugin.props`" Condition=`"Exists('`$(MSBuildThisFileDirectory)$($_.Name)\plugin.props')`" />"
        $targetsLines += "  <Import Project=`"`$(MSBuildThisFileDirectory)$($_.Name)\plugin.targets`" Condition=`"Exists('`$(MSBuildThisFileDirectory)$($_.Name)\plugin.targets')`" />"
    }
}
$propsLines += '</Project>'
$targetsLines += '</Project>'
Set-Content -Encoding utf8 (Join-Path $pluginsDest "Plugins.props") ($propsLines -join "`n")
Set-Content -Encoding utf8 (Join-Path $pluginsDest "Plugins.targets") ($targetsLines -join "`n")

# ── 4. build ─────────────────────────────────────────────────────────────────
Step "Building $ProjectName ($Configuration|$Platform)"
$outDir = Join-Path $ProjectDir "bin"
$env:NS_WINDOWS_RUNTIME_ROOT = "$RepoRoot"
# Unpackaged + self-contained Windows App SDK so the runner starts straight from bin\ without
# registering an MSIX package (the CLI's `ns run windows` registers one instead).
$buildLog = Join-Path $PlatformsDir "build.log"
dotnet build $csproj -c $Configuration "-p:Platform=$Platform" --output $outDir `
    "-p:WindowsPackageType=None" "-p:WindowsAppSDKSelfContained=true" -nologo -v:minimal *> $buildLog
if ($LASTEXITCODE -ne 0) {
    Get-Content $buildLog | Select-String -Pattern "error" | Select-Object -First 40 | ForEach-Object { Write-Host $_ }
    throw "dotnet build failed (full log: $buildLog)"
}

# ── 5. run ───────────────────────────────────────────────────────────────────
$exe = Join-Path $outDir "$ProjectName.exe"
$resultsXml = Join-Path $outDir "test-results.xml"
$resultsLog = Join-Path $outDir "test-results.log"
Remove-Item -Force -ErrorAction SilentlyContinue $resultsXml, $resultsLog

Step "Running $exe"
$env:NS_TEST_RESULTS = $resultsXml
$env:NS_TEST_LOG = $resultsLog
$env:NS_TEST_FILTER = $Filter
$proc = Start-Process -FilePath $exe -WorkingDirectory $outDir -PassThru
if (-not $proc.WaitForExit($TimeoutSeconds * 1000)) {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    if (Test-Path $resultsLog) { Get-Content $resultsLog }
    throw "Test app did not finish within $TimeoutSeconds s"
}

if (Test-Path $resultsLog) { Get-Content $resultsLog }
if (-not (Test-Path $resultsXml)) {
    throw ("Test app exited with 0x{0:X8} without writing results (crash?). See {1}" -f $proc.ExitCode, $outDir)
}
Write-Host "JUnit results: $resultsXml"
$failed = $proc.ExitCode
if ($failed -ne 0) {
    Write-Host ("{0} spec(s) failed" -f $failed) -ForegroundColor Red
    exit 1
}
Write-Host "All specs passed" -ForegroundColor Green
