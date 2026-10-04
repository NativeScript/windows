using System;
using System.IO;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading.Tasks;
using WinRT.Interop;
using Windows.ApplicationModel.Core;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;
using Windows.System;
using Windows.UI.Popups;

namespace __PROJECT_NAME__
{
    internal static class CrashDiagnostics
    {
        private static bool _installed;

        public static void InstallGlobalHandlers()
        {
            if (_installed) return;
            _installed = true;

            AppDomain.CurrentDomain.UnhandledException += (_, args) =>
            {
                var ex = args.ExceptionObject as Exception;
                WriteExceptionReport("AppDomain.UnhandledException", ex, "IsTerminating=" + args.IsTerminating);
            };

            TaskScheduler.UnobservedTaskException += (_, args) =>
            {
                // Always mark the exception as observed to prevent the finalizer
                // from rethrowing it on the finalizer thread.
                try { args.SetObserved(); } catch { }

                // Log the exception but never allow logging failures to propagate
                // (this handler runs on the finalizer thread in some cases).
                try
                {
                    WriteExceptionReport("TaskScheduler.UnobservedTaskException", args.Exception, null);
                }
                catch (Exception logEx)
                {
                    try { WriteToTraceLog("[CrashDiagnostics] Failed to write unobserved exception: " + logEx); } catch { }
                }
            };
        }

        public static void WriteExceptionReport(string source, Exception ex, string details)
        {
            var sb = new StringBuilder();
            sb.AppendLine("============================================================");
            sb.AppendLine("Timestamp: " + DateTimeOffset.UtcNow.ToString("o"));
            sb.AppendLine("Source: " + source);
            if (!string.IsNullOrWhiteSpace(details)) sb.AppendLine("Details: " + details);
            if (ex != null) { sb.AppendLine("Exception:"); sb.AppendLine(ex.ToString()); }
            else sb.AppendLine("Exception: <null>");
            sb.AppendLine("Managed stack snapshot:");
            sb.AppendLine(Environment.StackTrace);
            sb.AppendLine();
            AppendToLog(sb.ToString());
        }

        public static void WriteMessage(string source, string message)
        {
            var sb = new StringBuilder();
            sb.AppendLine("============================================================");
            sb.AppendLine("Timestamp: " + DateTimeOffset.UtcNow.ToString("o"));
            sb.AppendLine("Source: " + source);
            sb.AppendLine("Message: " + message);
            sb.AppendLine();
            AppendToLog(sb.ToString());
        }

        public static string BuildErrorReport(Exception ex, string jsError = null, string extraDetails = null)
        {
            var sb = new StringBuilder();

            if (!string.IsNullOrWhiteSpace(jsError))
            {
                sb.AppendLine("── JavaScript Error ──────────────────────────────────────");
                sb.AppendLine(jsError.Trim());
                sb.AppendLine();
            }

            if (ex != null)
            {
                sb.AppendLine("── Native Exception ──────────────────────────────────────");
                sb.AppendLine(ex.GetType().Name + ": " + ex.Message);
                if (ex.StackTrace != null) sb.AppendLine(ex.StackTrace);
                var inner = ex.InnerException;
                while (inner != null)
                {
                    sb.AppendLine("Caused by: " + inner.GetType().Name + ": " + inner.Message);
                    if (inner.StackTrace != null) sb.AppendLine(inner.StackTrace);
                    inner = inner.InnerException;
                }
                sb.AppendLine();
            }

            // Include panic log from previous or current run
            try
            {
                var panicLog = Path.Combine(RuntimeHost.LocalFolderPath, "nativescript-panic.log");
                if (File.Exists(panicLog))
                {
                    var content = File.ReadAllText(panicLog, Encoding.UTF8).Trim();
                    if (!string.IsNullOrEmpty(content))
                    {
                        sb.AppendLine("── Rust Panic Log ────────────────────────────────────────");
                        sb.AppendLine(content);
                        sb.AppendLine();
                    }
                }
            }
            catch { }

            if (!string.IsNullOrWhiteSpace(extraDetails))
            {
                sb.AppendLine("── Additional Info ───────────────────────────────────────");
                sb.AppendLine(extraDetails);
                sb.AppendLine();
            }

            sb.AppendLine("──────────────────────────────────────────────────────────");
            sb.AppendLine("Timestamp: " + DateTimeOffset.UtcNow.ToString("o"));

            return sb.ToString();
        }

        public static string CrashLogPath()
        {
            try { return Path.Combine(RuntimeHost.LocalFolderPath, "nativescript-crash.log"); }
            catch { return null; }
        }

        /// Parses the first "path:line:col" source location from a JS stack trace.
        /// Returns null if none found. Handles Windows drive-letter paths (C:\...).
        public static (string file, int line, int col)? TryExtractSourceLocation(string errorReport)
        {
            if (string.IsNullOrEmpty(errorReport)) return null;

            // Match the contents of the first (path:line:col) group in the stack trace.
            var m = Regex.Match(errorReport, @"\(([^)]+)\)");
            if (!m.Success) return null;

            var inner = m.Groups[1].Value; // e.g. "C:\app\bundle.js:42:13"

            // Parse col from the right (last :digits).
            var lastColon = inner.LastIndexOf(':');
            if (lastColon < 0 || !int.TryParse(inner.Substring(lastColon + 1), out int col)) return null;
            inner = inner.Substring(0, lastColon);

            // Parse line from the right (second-to-last :digits).
            lastColon = inner.LastIndexOf(':');
            if (lastColon < 0 || !int.TryParse(inner.Substring(lastColon + 1), out int line)) return null;
            var file = inner.Substring(0, lastColon);

            return string.IsNullOrEmpty(file) ? null : (file, line, col);
        }

        public static async Task ShowCrashDialogAsync(string heading, string errorReport)
        {
            try
            {
                // Write full details to log so developer can access them even if the
                // dialog summary is truncated.
                var logPath = CrashLogPath();
                if (logPath != null)
                    File.WriteAllText(logPath, errorReport, Encoding.UTF8);

                // Truncate for the dialog (MessageDialog has practical limits).
                const int MaxLen = 800;
                var summary = errorReport.Length > MaxLen
                    ? errorReport.Substring(0, MaxLen) + "\n\n[truncated — see log for full details]"
                    : errorReport;

                if (logPath != null)
                    summary += "\n\nFull log: " + logPath;

                var dialog = new MessageDialog(summary, "NativeScript Runtime Error");
                try
                {
                    var window = App.CurrentWindow;
                    if (window != null)
                    {
                        InitializeWithWindow.Initialize(dialog, WindowNative.GetWindowHandle(window));
                    }
                }
                catch { }

                // MessageDialog supports at most 3 commands. When a source location is
                // available, swap "Copy Details" for "Open in VS Code" so the user can
                // choose between jumping to the error or restarting. Full details are
                // always written to the log file shown in the dialog text.
                var srcLocation = TryExtractSourceLocation(errorReport);
                if (srcLocation.HasValue)
                {
                    var (srcFile, srcLine, srcCol) = srcLocation.Value;
                    dialog.Commands.Add(new UICommand("Open Source File", async _ =>
                    {
                        try
                        {
                            var normalizedPath = srcFile.Replace('\\', '/');
                            var uriStr = $"file:///{normalizedPath}";
                            if (Uri.TryCreate(uriStr, UriKind.Absolute, out var fileUri))
                                await Launcher.LaunchUriAsync(fileUri,
                                    new LauncherOptions { DisplayApplicationPicker = true });
                        }
                        catch { }
                    }));
                }
                else
                {
                    dialog.Commands.Add(new UICommand("Copy Details", _ =>
                    {
                        try
                        {
                            var dp = new DataPackage();
                            dp.SetText(errorReport);
                            Clipboard.SetContent(dp);
                        }
                        catch { }
                    }));
                }

                dialog.Commands.Add(new UICommand("Restart App", async _ =>
                {
                    try { await CoreApplication.RequestRestartAsync("crash-restart"); }
                    catch { }
                }));

                dialog.Commands.Add(new UICommand("Dismiss"));
                dialog.DefaultCommandIndex = 0;
                dialog.CancelCommandIndex = 2;

                await dialog.ShowAsync();
            }
            catch (Exception ex)
            {
                System.Diagnostics.Debug.WriteLine("[NativeScript] Failed to show crash dialog: " + ex.Message);
            }
        }

        /// <summary>
        /// Appends <paramref name="message"/> to the runtime trace file in the Win32 temp path.
        /// Prefer `console.log`; fall back to `ns_trace.log` for CLI/legacy compatibility.
        /// In a packaged Windows app, GetTempPath() resolves inside the app container temp area,
        /// matching Rust's std::env::temp_dir() so the CLI still sees the same log stream.
        /// </summary>
        public static void WriteToTraceLog(string message)
        {
            try
            {
            // System.IO.Path.GetTempPath() calls Win32 GetTempPathW() which for a packaged
            // desktop app resolves into the container temp folder, matching Rust.
                var temp = System.IO.Path.GetTempPath();
                var consolePath = Path.Combine(temp, "console.log");
                File.AppendAllText(consolePath, message, Encoding.UTF8);

                // Maintain legacy compatibility: also append to ns_trace.log if it already exists
                var legacyPath = Path.Combine(temp, "ns_trace.log");
                if (File.Exists(legacyPath))
                {
                    File.AppendAllText(legacyPath, message, Encoding.UTF8);
                }
            }
            catch { }
        }

        private static void AppendToLog(string content)
        {
            try
            {
                var logPath = Path.Combine(RuntimeHost.LocalFolderPath, "nativescript-crash.log");
                File.AppendAllText(logPath, content, Encoding.UTF8);
            }
            catch { }
        }
    }
}
