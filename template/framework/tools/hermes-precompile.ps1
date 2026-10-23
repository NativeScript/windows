# Compiles the app's JavaScript to Hermes bytecode beside each file, for Release builds of
# @nativescript/windows-hermes apps: <file>.hbc for scripts the host runs as is (runtime.js,
# vendor.js, bundle.js, the package.json "main"), <file>.cjs.hbc for modules, compiled inside the
# runtime's CommonJS wrapper. Up-to-date files are skipped; the runtime ignores bytecode older
# than its source. Runs on Windows PowerShell 5.1.
param(
    [Parameter(Mandatory = $true)][string]$AppDir,
    [Parameter(Mandatory = $true)][string]$Hermesc,
    [switch]$Optimize
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path $AppDir)) { return }
$AppDir = (Resolve-Path $AppDir).Path

$entries = @('runtime.js', 'vendor.js', 'bundle.js')
$packageJson = Join-Path $AppDir 'package.json'
if (Test-Path $packageJson) {
    try {
        $package = Get-Content -Raw $packageJson | ConvertFrom-Json
        if ($package.main) { $entries += [IO.Path]::GetFileName([string]$package.main) }
    } catch { }
}

$wrapperHead = '(function (module, exports, require, __filename, __dirname) {'
$utf8 = New-Object System.Text.UTF8Encoding($false)
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("ns-hermes-" + [Guid]::NewGuid().ToString('N'))
$compiled = 0

try {
    Get-ChildItem -Path $AppDir -Recurse -File -Filter *.js |
        Where-Object { $_.FullName -notmatch '\\node_modules\\' } |
        ForEach-Object {
            $isEntry = ($_.DirectoryName -eq $AppDir) -and ($entries -contains $_.Name)
            $out = $_.FullName + $(if ($isEntry) { '.hbc' } else { '.cjs.hbc' })
            if ((Test-Path $out) -and ((Get-Item $out).LastWriteTimeUtc -ge $_.LastWriteTimeUtc)) { return }

            $source = $_.FullName
            if (-not $isEntry) {
                # Same file name in a scratch folder, so the bytecode's debug info names the module.
                $dir = Join-Path $scratch ([Guid]::NewGuid().ToString('N'))
                $null = New-Item -ItemType Directory -Force -Path $dir
                $source = Join-Path $dir $_.Name
                $text = [IO.File]::ReadAllText($_.FullName)
                [IO.File]::WriteAllText($source, $wrapperHead + $text + "`n})", $utf8)
            }

            $hermescArgs = @('-emit-binary', '-w', '-out', $out, $source)
            if ($Optimize) { $hermescArgs = @('-O') + $hermescArgs }
            & $Hermesc @hermescArgs
            if ($LASTEXITCODE -ne 0) {
                Remove-Item -Force -ErrorAction SilentlyContinue $out
                Write-Warning "hermesc could not compile $($_.FullName); it will be compiled at run time"
            } else {
                $compiled++
            }
        }
} finally {
    if (Test-Path $scratch) { Remove-Item -Recurse -Force $scratch }
}

Write-Host "[NativeScript] Hermes bytecode: $compiled file(s) compiled"
