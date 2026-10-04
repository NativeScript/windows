using System;
using System.Text.Json;
using Windows.ApplicationModel;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.Storage;

namespace __PROJECT_NAME__
{
    public interface INativeScriptApp
    {
        Microsoft.UI.Xaml.Window MainWindow { get; }
        Microsoft.UI.Xaml.Window Window { get; }
    }

    sealed partial class App : Application, INativeScriptApp
    {
        private const string LastLaunchArgsKey = "LastLaunchArgs";
        private readonly RuntimeHost _runtimeHost = new RuntimeHost();

        internal static Window CurrentWindow { get; private set; }
        public Window MainWindow => CurrentWindow;
        public Window Window => CurrentWindow;

         enum AppEventKind
        {
            Activated = 1,
            Deactivated = 2,
            Shown = 3,
            Hidden = 4,
            UncaughtError = 5,
            Exit = 6
        }

        public App()
        {
			this.InitializeComponent();
			CurrentWindow ??= new Window();

            CrashDiagnostics.InstallGlobalHandlers();
            CurrentWindow.Closed += OnWindowClosed;
            CurrentWindow.Activated += OnWindowActivated;
            CurrentWindow.VisibilityChanged += OnWindowVisibilityChanged;
            this.UnhandledException += OnUnhandledException;
        }

        protected override async void OnLaunched(LaunchActivatedEventArgs e)
        {
            _runtimeHost.Initialize();

            // Capture before any await — continuations may resume on a thread pool thread.
            var dispatcherQueue = DispatcherQueue.GetForCurrentThread();

            // Show crash report from the previous run if one exists.
            try
            {
                var panicLogPath = System.IO.Path.Combine(
                    RuntimeHost.LocalFolderPath, "nativescript-panic.log");
                if (System.IO.File.Exists(panicLogPath))
                {
                    var content = System.IO.File.ReadAllText(panicLogPath);
                    System.IO.File.Delete(panicLogPath);
                    if (!string.IsNullOrWhiteSpace(content))
                        await CrashDiagnostics.ShowCrashDialogAsync("Crash from previous run", content);
                }
            }
            catch { }

            string jsError = null;
            try
            {
                _runtimeHost.RunMainScript();
                jsError = _runtimeHost.GetLastJsError();
                if (!string.IsNullOrEmpty(jsError))
                {
                    CrashDiagnostics.WriteMessage("JS Error", jsError);
                    var report = CrashDiagnostics.BuildErrorReport(null, jsError);
                    CrashDiagnostics.WriteToTraceLog(report);
                    await CrashDiagnostics.ShowCrashDialogAsync("JavaScript Error", report);
                }
            }
            catch (Exception scriptEx)
            {
                jsError = _runtimeHost.GetLastJsError();
                System.Diagnostics.Debug.WriteLine($"[NativeScript] Script exception: {scriptEx.Message}");
                CrashDiagnostics.WriteExceptionReport("RunMainScript", scriptEx, null);
                var report = CrashDiagnostics.BuildErrorReport(scriptEx, jsError);
                CrashDiagnostics.WriteToTraceLog(report);
                await CrashDiagnostics.ShowCrashDialogAsync("Script Execution Error", report);
            }

            void ShowMainWindow()
            {
                StartPump();

                try
                {
                    if (CurrentWindow?.Content != null)
                    {
                        CurrentWindow.Activate();
                    }
                }
                catch { }
            }

            // After any await, the continuation may run on a thread pool thread.
            // Schedule all UI-thread-required operations through the dispatcher queue.
            if (dispatcherQueue?.HasThreadAccess == true)
            {
                ShowMainWindow();
            }
            else if (!(dispatcherQueue?.TryEnqueue(ShowMainWindow) ?? false))
            {
                ShowMainWindow();
            }
        }

        private void OnWindowClosed(object sender, WindowEventArgs e)
        {
            StopPump();
            _runtimeHost.NotifyAppEvent((int)AppEventKind.Exit, null);
            try { ApplicationData.Current.LocalSettings.Values[LastLaunchArgsKey] = string.Empty; }
            catch (InvalidOperationException) { } // unpackaged: no ApplicationData
            _runtimeHost.Dispose();
        }

        private void OnWindowActivated(object sender, WindowActivatedEventArgs e)
        {
             var kind = e.WindowActivationState == WindowActivationState.Deactivated ? AppEventKind.Deactivated : AppEventKind.Activated;
            _runtimeHost.NotifyAppEvent((int)kind, null);
        }

        private void OnWindowVisibilityChanged(object sender, WindowVisibilityChangedEventArgs e)
        {
            var kind = e.Visible ? AppEventKind.Shown : AppEventKind.Hidden;
            _runtimeHost.NotifyAppEvent((int)kind, null);
        }

        private void OnUnhandledException(object sender, Microsoft.UI.Xaml.UnhandledExceptionEventArgs e)
        {
            e.Handled = true;
            var jsError = _runtimeHost.GetLastJsError();

             var message = e.Message ?? jsError ?? "Unhandled exception";
            _runtimeHost.NotifyAppEvent((int)AppEventKind.UncaughtError, message);

            CrashDiagnostics.WriteExceptionReport(
                "Xaml.UnhandledException",
                e.Exception,
                "JsError=" + (jsError ?? "<none>"));

            var report = CrashDiagnostics.BuildErrorReport(e.Exception, jsError);
            CrashDiagnostics.WriteToTraceLog(report);
            var _ = CrashDiagnostics.ShowCrashDialogAsync(
                e.Message ?? "Unhandled exception", report);
        }

        // The V8 runtime pump (JS timers + microtask/promise continuations) must NOT run *inside*
        // CompositionTarget.Rendering: that callback executes within XAML's render walk, and the JS it
        // pumps frequently mutates the live XAML tree (e.g. setting Image.Source when an async image
        // load completes, or layout-invalidating writes from JS-driven animations). Mutating the tree
        // during the render walk re-enters the XAML core illegally and trips its re-entrancy guard,
        // which RoFailFastWithErrorContext's with E_UNEXPECTED (0x8000FFFF): a stowed exception
        // (0xC000027B) that faults in Microsoft.UI.Xaml.dll and bypasses every managed/native handler
        // (so it leaves no crash/panic log).
        //
        // Pump cadence comes from two sources, both routed through the guarded SchedulePump():
        //   1. CompositionTarget.Rendering — fires at the display's true refresh rate (120/144/240Hz,
        //      NOT capped at 60) while the compositor is producing frames, so JS-driven animations stay
        //      smooth on high-refresh displays.
        //   2. A low-frequency DispatcherQueueTimer heartbeat — keeps timers/promise continuations
        //      running even when Rendering stops firing (e.g. the window is minimised or fully occluded,
        //      so the compositor idles and stops raising Rendering).
        // Either way the actual pump runs as an ordinary dispatcher work item OUTSIDE the render walk,
        // where mutating the tree is legal. _pumpQueued keeps at most one pump in flight, so the two
        // sources never double-pump and work items can't pile up. All fields are touched only on the UI
        // thread, so no synchronization is needed.
        private DispatcherQueue _dispatcherQueue;
        private DispatcherQueueTimer _pumpHeartbeat;
        private bool _pumpQueued;

        private void StartPump()
        {
            _dispatcherQueue = DispatcherQueue.GetForCurrentThread();

            CompositionTarget.Rendering -= OnRenderFrame;
            CompositionTarget.Rendering += OnRenderFrame;

            if (_pumpHeartbeat == null && _dispatcherQueue != null)
            {
                _pumpHeartbeat = _dispatcherQueue.CreateTimer();
                _pumpHeartbeat.Interval = TimeSpan.FromMilliseconds(100);
                _pumpHeartbeat.IsRepeating = true;
                _pumpHeartbeat.Tick += OnPumpHeartbeat;
                _pumpHeartbeat.Start();
            }
        }

        private void StopPump()
        {
            CompositionTarget.Rendering -= OnRenderFrame;
            if (_pumpHeartbeat != null)
            {
                _pumpHeartbeat.Stop();
                _pumpHeartbeat.Tick -= OnPumpHeartbeat;
                _pumpHeartbeat = null;
            }
        }

        // Inside XAML's render walk — only schedule; never pump or touch the tree here.
        private void OnRenderFrame(object sender, object e) => SchedulePump();

        private void OnPumpHeartbeat(DispatcherQueueTimer sender, object args) => SchedulePump();

        private void SchedulePump()
        {
            if (_pumpQueued)
            {
                return;
            }

            var dispatcherQueue = _dispatcherQueue;
            if (dispatcherQueue == null)
            {
                return;
            }

            _pumpQueued = true;
            dispatcherQueue.TryEnqueue(() =>
            {
                _pumpQueued = false;
                _runtimeHost.PumpTimers();
#if DEBUG
                _runtimeHost.PumpDevtools();
#endif
            });
        }
    }
}
