using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Text.Json;

namespace NativeScriptWindowsDemo
{
    internal sealed class RuntimeHost : IDisposable
    {
        private const string NativeScriptLibrary = "nativescript";

        [DllImport("kernel32.dll")]
        private static extern bool AttachConsole(int dwProcessId);

        private const int ATTACH_PARENT_PROCESS = -1;

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_init))]
        private static extern long runtime_init([MarshalAs(UnmanagedType.LPUTF8Str)] string appRoot);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_deinit))]
        private static extern void runtime_deinit(long runtime);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_runscript))]
        private static extern void runtime_runscript(long runtime, [MarshalAs(UnmanagedType.LPUTF8Str)] string script, [MarshalAs(UnmanagedType.LPUTF8Str)] string filename);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_install_ctrlc_handler))]
        private static extern void runtime_install_ctrlc_handler(int exitCode);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_set_local_folder))]
        private static extern void runtime_set_local_folder([MarshalAs(UnmanagedType.LPUTF8Str)] string localFolder);

        // Escape hatch for apps packed with `nsbundle_pack --key-hex <hex>` (custom-key
        // app.nsbundle containers). Must be called before runtime_init if used at all — apps
        // packed with the default pepper (no --key-hex) never need to call this. Not wired to a
        // default call site here; an app author supplies their own key material and calls this
        // from Initialize() before runtime_init(...).
        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_set_bundle_key))]
        private static extern int runtime_set_bundle_key([MarshalAs(UnmanagedType.LPUTF8Str)] string keyHex);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_read_protected_file))]
        private static extern IntPtr runtime_read_protected_file([MarshalAs(UnmanagedType.LPUTF8Str)] string virtualPath);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_free_protected_string))]
        private static extern void runtime_free_protected_string(IntPtr ptr);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_get_last_js_error))]
        private static extern IntPtr runtime_get_last_js_error();

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_free_js_error))]
        private static extern void runtime_free_js_error(IntPtr ptr);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_has_devtools))]
        private static extern bool runtime_has_devtools();

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_pump_timers))]
        private static extern void runtime_pump_timers();

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_notify_app_event))]
        private static extern void runtime_notify_app_event(long runtime, int kind, [MarshalAs(UnmanagedType.LPUTF8Str)] string message);

        public void NotifyAppEvent(int kind, string message = null)
        {
            if (!_initialized) return;
            try { runtime_notify_app_event(_runtime, kind, message); }
            catch { }
        }

    #if DEBUG
        [DllImport(NativeScriptLibrary, EntryPoint = nameof(ns_set_log_to_console))]
        private static extern int ns_set_log_to_console(int enabled);

        public static bool TrySetLogToConsole(bool enabled)
        {
            try { return ns_set_log_to_console(enabled ? 1 : 0) == 1; }
            catch { return false; }
        }
    #else
        public static bool TrySetLogToConsole(bool enabled) => false;
    #endif

        public void PumpTimers()
        {
            if (!_initialized) return;
            try { runtime_pump_timers(); }
            catch { }
        }

#if DEBUG
        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_devtools_start))]
        private static extern IntPtr runtime_devtools_start(long runtime, ushort port);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_devtools_pump))]
        private static extern void runtime_devtools_pump(long runtime);

        [DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_free_string))]
        private static extern void runtime_free_string(IntPtr ptr);

        public string DevtoolsFrontendUrl { get; private set; }

        private bool _devtoolsAvailable;

        public void PumpDevtools()
        {
            if (!_initialized || !_devtoolsAvailable) return;
            try { runtime_devtools_pump(_runtime); }
            catch (Exception ex)
            {
                System.Diagnostics.Debug.WriteLine($"[NativeScript DevTools] Pump failed: {ex.Message}");
            }
        }

        private void StartDevtoolsSafely()
        {
            if (!runtime_has_devtools()) return;
            IntPtr urlPtr = IntPtr.Zero;
            try
            {
                urlPtr = runtime_devtools_start(_runtime, 42000);
                if (urlPtr == IntPtr.Zero) return;
                var wsUrl = Marshal.PtrToStringUTF8(urlPtr);
                DevtoolsFrontendUrl = wsUrl != null
                    ? $"devtools://devtools/bundled/inspector.html?ws={wsUrl.Replace("ws://", "")}"
                    : null;
                if (DevtoolsFrontendUrl != null)
                {
                    _devtoolsAvailable = true;
                    System.Diagnostics.Debug.WriteLine($"[NativeScript DevTools] {DevtoolsFrontendUrl}");
                }
            }
            catch (Exception ex)
            {
                DevtoolsFrontendUrl = null;
                System.Diagnostics.Debug.WriteLine($"[NativeScript DevTools] Start failed: {ex.Message}");
            }
            finally
            {
                if (urlPtr != IntPtr.Zero) runtime_free_string(urlPtr);
            }
        }
#endif

        private long _runtime;
        private bool _initialized;

        public void Initialize()
        {
            if (_initialized) return;
            bool attached = false;
            try { attached = AttachConsole(ATTACH_PARENT_PROCESS); } catch { }
            runtime_install_ctrlc_handler(0);
            runtime_set_local_folder(Windows.Storage.ApplicationData.Current.LocalFolder.Path);
            _runtime = runtime_init(AppContext.BaseDirectory);
#if DEBUG
            var trySet = TrySetLogToConsole(true);
            System.Diagnostics.Debug.WriteLine($"[NativeScript] TrySetLogToConsole returned={trySet}");

            // Also notify the managed bridge (if present) so managed-side
            // diagnostics follow the runtime toggle. Use reflection to avoid a
            // hard compile-time dependency on the bridge assembly.
            try
            {
                foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
                {
                    try
                    {
                        var t = asm.GetType("NativeScriptBridge.Bridge");
                        if (t is null) continue;
                        var m = t.GetMethod("SetLogToConsole", BindingFlags.Public | BindingFlags.Static);
                        if (m is null) continue;
                        m.Invoke(null, new object[] { true });
                        System.Diagnostics.Debug.WriteLine($"[NativeScript] Invoked Bridge.SetLogToConsole on {asm.GetName().Name}");
                        break;
                    }
                    catch { }
                }
            }
            catch { }
            if (ConsumeDebugBreakMarker())
                StartDevtoolsSafely();
#endif
            _initialized = true;
        }

#if DEBUG
        /// <summary>
        /// Returns true and deletes the marker if the CLI wrote ns-debugbreak to LocalFolder,
        /// matching the Android sentinel-file pattern used by the NativeScript CLI.
        /// </summary>
        private static bool ConsumeDebugBreakMarker()
        {
            try
            {
                var markerPath = System.IO.Path.Combine(
                    Windows.Storage.ApplicationData.Current.LocalFolder.Path,
                    "ns-debugbreak");
                if (!System.IO.File.Exists(markerPath))
                    return false;
                System.IO.File.Delete(markerPath);
                return true;
            }
            catch { return false; }
        }
#endif

        /// Reads a file that may live inside a sealed app.nsbundle instead of on disk: tries the
        /// protected-VFS native call first (cheap no-op when no bundle is loaded), falls back to
        /// the real filesystem. `path` is whatever candidate the caller already built for
        /// `File.Exists`/`File.ReadAllText` — the native side strips it down to the bundle's
        /// virtual (app/App-relative) path itself, so no extra bookkeeping is needed here.
        private static string TryReadVirtual(string path)
        {
            IntPtr ptr = IntPtr.Zero;
            try
            {
                ptr = runtime_read_protected_file(path);
                return ptr == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(ptr);
            }
            catch { return null; }
            finally { if (ptr != IntPtr.Zero) runtime_free_protected_string(ptr); }
        }

        private static bool VExists(string path) => TryReadVirtual(path) != null || File.Exists(path);

        private static string VReadAllText(string path) => TryReadVirtual(path) ?? File.ReadAllText(path);

        public void RunMainScript()
        {
            if (!_initialized)
                throw new InvalidOperationException("Runtime must be initialized before running scripts.");

            var entryPath = ResolveEntryScriptPath();
            if (entryPath == null)
            {
                System.Diagnostics.Debug.WriteLine("[NativeScript Runtime] No entry script found — bundle missing from app directory.");
                return;
            }

            var dir = Path.GetDirectoryName(Path.GetFullPath(entryPath));
            var chunks = new List<string>();
            // An ES-module entry (Vite's bundle.mjs) imports its own chunks; webpack's classic
            // runtime.js/vendor.js prelude must not run in front of it (a stale pair can be left
            // in app/ after switching bundlers).
            var isModuleEntry = entryPath.EndsWith(".mjs", StringComparison.OrdinalIgnoreCase);
            foreach (var chunkName in isModuleEntry ? Array.Empty<string>() : new[] { "runtime.js", "vendor.js" })
            {
                var chunkPath = Path.Combine(dir, chunkName);
                if (VExists(chunkPath) &&
                    !string.Equals(chunkPath, Path.GetFullPath(entryPath), StringComparison.OrdinalIgnoreCase))
                {
                    chunks.Add(chunkPath);
                }
            }
            chunks.Add(entryPath);

            foreach (var scriptPath in chunks)
            {
                var script = VReadAllText(Path.GetFullPath(scriptPath));
                try
                {
                    // Modules resolve their imports (chunks, ~/ paths) relative to their own
                    // location, so they need the full path; classic scripts keep the short name
                    // their stack traces and code-cache keys have always used.
                    var name = isModuleEntry ? Path.GetFullPath(scriptPath) : Path.GetFileName(scriptPath);
                    runtime_runscript(_runtime, script, name);
                }
                catch (Exception ex)
                {
                    CrashDiagnostics.WriteExceptionReport("RuntimeHost.RunMainScript", ex, "Script=" + scriptPath);
                    System.Diagnostics.Debug.WriteLine($"[NativeScript Runtime] Script execution failed ({scriptPath}): {ex}");
                    throw;
                }
            }
        }

        private sealed class RuntimePackageConfig
        {
            public string Main { get; set; } = string.Empty;
            public string WindowsMain { get; set; } = string.Empty;
        }

        private static string ResolveEntryScriptPath()
        {
            var baseDir = AppContext.BaseDirectory;
            // EXE lives in <project>/bin/; webpack bundle lives in <project>/app/.
            var parentDir = Path.GetDirectoryName(
                baseDir.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar))
                ?? baseDir;

            // Candidate app directories: sibling of bin/ first, then directly under baseDir.
            var appDirCandidates = new[]
            {
                Path.Combine(parentDir, "app"),
                Path.Combine(parentDir, "App"),
                Path.Combine(baseDir, "app"),
                Path.Combine(baseDir, "App"),
            };

            string packageJsonPath = null;
            string resolvedBaseDir = null;
            foreach (var dir in appDirCandidates)
            {
                var candidate = Path.Combine(dir, "package.json");
                if (VExists(candidate))
                {
                    packageJsonPath = candidate;
                    resolvedBaseDir = dir;
                    break;
                }
            }

            // Also accept package.json at the project root (parent of bin/).
            if (packageJsonPath == null && VExists(Path.Combine(parentDir, "package.json")))
            {
                packageJsonPath = Path.Combine(parentDir, "package.json");
                resolvedBaseDir = parentDir;
            }

            string Fallback() =>
                appDirCandidates
                    .SelectMany(d => new[] { Path.Combine(d, "bundle.js"), Path.Combine(d, "bundle.mjs") })
                    .FirstOrDefault(VExists);

            if (packageJsonPath == null)
                return Fallback();

            try
            {
                var config = ParsePackageConfig(packageJsonPath);
                if (!string.IsNullOrWhiteSpace(config.WindowsMain))
                {
                    var p = ResolveScriptPath(resolvedBaseDir, config.WindowsMain);
                    if (p != null) return p;
                }
                if (!string.IsNullOrWhiteSpace(config.Main))
                {
                    var p = ResolveScriptPath(resolvedBaseDir, config.Main);
                    if (p != null) return p;
                }
            }
            catch { }

            return Fallback();
        }

        private static RuntimePackageConfig ParsePackageConfig(string packageJsonPath)
        {
            using var doc = JsonDocument.Parse(VReadAllText(packageJsonPath));
            var config = new RuntimePackageConfig();
            if (doc.RootElement.TryGetProperty("main", out var main) && main.ValueKind == JsonValueKind.String)
                config.Main = main.GetString();
            if (doc.RootElement.TryGetProperty("windows", out var win) && win.ValueKind == JsonValueKind.Object &&
                win.TryGetProperty("main", out var winMain) && winMain.ValueKind == JsonValueKind.String)
                config.WindowsMain = winMain.GetString();
            return config;
        }

        private static string ResolveScriptPath(string baseDir, string scriptPath)
        {
            if (string.IsNullOrWhiteSpace(scriptPath)) return null;
            var normalized = scriptPath.Replace('/', Path.DirectorySeparatorChar);
            foreach (var candidate in new[] { normalized, normalized + ".js", normalized + ".mjs" })
            {
                var direct = Path.IsPathRooted(candidate) ? candidate : Path.Combine(baseDir, candidate);
                if (VExists(direct)) return direct;
                var appLower = Path.Combine(baseDir, "app", candidate);
                if (VExists(appLower)) return appLower;
                var appUpper = Path.Combine(baseDir, "App", candidate);
                if (VExists(appUpper)) return appUpper;
            }
            return null;
        }

        /// Returns the last JavaScript error (message + stack trace) captured during
        /// script execution, or null if no error was recorded since the last call.
        public string GetLastJsError()
        {
            if (!_initialized) return null;
            IntPtr ptr = IntPtr.Zero;
            try
            {
                ptr = runtime_get_last_js_error();
                return ptr == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(ptr);
            }
            catch { return null; }
            finally
            {
                if (ptr != IntPtr.Zero) runtime_free_js_error(ptr);
            }
        }

        public void Dispose()
        {
            if (!_initialized) return;
            runtime_deinit(_runtime);
            _runtime = 0;
            _initialized = false;
        }
    }
}
