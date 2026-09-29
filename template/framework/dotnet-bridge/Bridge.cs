using System;
using System.Diagnostics;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Threading;
using System.Threading.Tasks;
using System.Runtime.InteropServices;
using System.Runtime.Loader;
using System.Text.Json;

[assembly: System.Runtime.CompilerServices.InternalsVisibleTo("DotNetBridgeTests")]
[assembly: System.Runtime.CompilerServices.InternalsVisibleTo("DotNetBridgeBenchmarks")]

namespace NativeScriptBridge;

public static partial class Bridge
{
    internal static readonly ConcurrentDictionary<int, object?> s_handles = new();
    internal static int s_nextHandle;

    // Cached WinUI3 UI-thread DispatcherQueue. Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread()
    // returns null off the UI thread, so we capture the queue the first time we run on a thread that owns
    // one (the UI thread during init/dispatch) and reuse it to marshal back from background threads.
    private static object? s_uiDispatcherQueue;

    // Function pointer registered by the Rust runtime so managed delegates can
    // call back into V8 without a JSON round-trip.
    internal static unsafe delegate* unmanaged[Cdecl]<int, byte*, int, byte**, int*, void>
        s_jsInvoker;

    private static readonly ConcurrentDictionary<string, Type?> s_typeCache
        = new(StringComparer.Ordinal);
    private static readonly ConcurrentDictionary<MethodKey, DispatchEntry> s_methodCache = new();
    private static readonly ConcurrentDictionary<PropKey, PropertyInfo?> s_propCache = new();
    private static readonly ConcurrentDictionary<CtorKey, CtorEntry> s_ctorCache = new();
    // Cache of attempted assembly loads (simple name -> Assembly or null if not found).
    private static readonly ConcurrentDictionary<string, Assembly?> s_assemblyLoadCache
        = new(StringComparer.OrdinalIgnoreCase);
    // Directories to search for assemblies (initialized once in static ctor).
    private static readonly string[] s_assemblySearchDirs;

    private static readonly JsonSerializerOptions s_coerceOpts = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
    };

    internal static void ClearCaches()
    {
        s_typeCache.Clear();
        s_methodCache.Clear();
        s_propCache.Clear();
        s_ctorCache.Clear();
        s_handles.Clear();
        s_nativePtrs.Clear();
        s_nativePtrsReverse.Clear();
        s_nextHandle = 0;
    }

    // Optional mapping from exported handle id -> native IUnknown pointer (IntPtr)
    // Populated when a managed object can yield a native COM pointer via
    // Marshal.GetIUnknownForObject. Cleared and released on __release.
    internal static readonly ConcurrentDictionary<int, IntPtr> s_nativePtrs = new();

    // Reverse index kept in lockstep with s_nativePtrs so CoerceBin can
    // resolve identity round-trips without scanning every entry.
    internal static readonly ConcurrentDictionary<IntPtr, int> s_nativePtrsReverse = new();

    internal static void StoreNativePtr(int handleId, IntPtr ptr)
    {
        s_nativePtrs[handleId] = ptr;
        s_nativePtrsReverse[ptr] = handleId;
    }

    internal static void RemoveNativePtr(int handleId, IntPtr ptr)
    {
        // Only drop the reverse entry if it still points at this handle —
        // another handle may have re-registered the same pointer.
        if (s_nativePtrsReverse.TryGetValue(ptr, out var owner) && owner == handleId)
            s_nativePtrsReverse.TryRemove(ptr, out _);
    }

    // Runtime-configurable logging toggle. Default to true in DEBUG builds so
    // developers get verbose diagnostics without setting environment vars.
#if DEBUG
    internal static bool s_logToConsole = true;
#else
    internal static bool s_logToConsole = false;
#endif

    public static void SetLogToConsole(bool enabled) => s_logToConsole = enabled;

    internal static bool IsLogToConsole() => s_logToConsole;

    // Returns a JSON object mapping namespaces to the assembly simple-name that
    // most likely contains those types. Exact namespaces are included so the
    // runtime can prefer the longest matching prefix (for example
    // Microsoft.UI.Xaml.Controls -> Microsoft.UI.Xaml) instead of relying only
    // on a coarse top-level root such as Microsoft.
    public static string GetNamespaceAssemblyMapJson()
    {
        var map = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        try
        {
            foreach (var dir in s_assemblySearchDirs.Where(Directory.Exists))
            {
                try
                {
                    foreach (var file in Directory.EnumerateFiles(dir, "*.dll", SearchOption.TopDirectoryOnly))
                    {
                        try
                        {
                            var simple = Path.GetFileNameWithoutExtension(file);
                            if (string.IsNullOrEmpty(simple)) continue;

                            Assembly? asm = AppDomain.CurrentDomain.GetAssemblies()
                                .FirstOrDefault(a => string.Equals(a.GetName().Name, simple, StringComparison.OrdinalIgnoreCase));
                            if (asm is null)
                            {
                                try { asm = AssemblyLoadContext.Default.LoadFromAssemblyPath(file); }
                                catch { continue; }
                            }

                            Type[] types;
                            try { types = asm.GetExportedTypes(); }
                            catch { types = Array.Empty<Type>(); }

                            var assemblyName = asm.GetName().Name;
                            if (string.IsNullOrEmpty(assemblyName)) continue;

                            foreach (var t in types)
                            {
                                if (string.IsNullOrEmpty(t.Namespace)) continue;
                                map[t.Namespace] = assemblyName;
                                var root = t.Namespace.Split('.')[0];
                                if (!map.ContainsKey(root)) map[root] = assemblyName;
                            }
                        }
                        catch { }
                    }
                }
                catch { }
            }
        }
        catch { }
        try { return JsonSerializer.Serialize(map); } catch { return "{}"; }
    }

    static Bridge()
    {
        try
        {
            var baseDir = AppContext.BaseDirectory ?? AppDomain.CurrentDomain.BaseDirectory;
            var dirs = new List<string> { baseDir };

            // libs/ subtree relative to the bridge's own directory.
            var libs = Path.Combine(baseDir, "libs");
            if (Directory.Exists(libs))
            {
                dirs.Add(libs);
                try { dirs.AddRange(Directory.GetDirectories(libs, "*", SearchOption.AllDirectories)); } catch { }
            }

            // The bridge DLL lives at dotnet-bridge/publish/ inside the app output root.
            // Plugin assemblies (added via plugin.props) land at plugins/**/*.dll and NuGet
            // assemblies land at the app output root itself — both outside the bridge subtree.
            // Use the host process directory to reach them.
            try
            {
                var processDir = Path.GetDirectoryName(
                    System.Diagnostics.Process.GetCurrentProcess().MainModule?.FileName);
                if (!string.IsNullOrEmpty(processDir))
                {
                    dirs.Add(processDir);

                    // plugins/ subtree: CLI-managed plugin DLLs live here.
                    // Check both {processDir}/plugins and {processDir}/../plugins because
                    // the NS CLI places plugins/ as a sibling of bin/, not inside it.
                    var pluginsDir = Path.Combine(processDir, "plugins");
                    if (Directory.Exists(pluginsDir))
                        try { dirs.AddRange(Directory.GetDirectories(pluginsDir, "*", SearchOption.AllDirectories)); } catch { }

                    var parentDir = Path.GetDirectoryName(processDir);
                    if (!string.IsNullOrEmpty(parentDir))
                    {
                        var parentPlugins = Path.Combine(parentDir, "plugins");
                        if (Directory.Exists(parentPlugins))
                        {
                            dirs.Add(parentPlugins);
                            try { dirs.AddRange(Directory.GetDirectories(parentPlugins, "*", SearchOption.AllDirectories)); } catch { }
                        }
                    }

                    // libs/ relative to the app root (alternative convention).
                    var processLibs = Path.Combine(processDir, "libs");
                    if (Directory.Exists(processLibs))
                        try { dirs.AddRange(Directory.GetDirectories(processLibs, "*", SearchOption.AllDirectories)); } catch { }
                }
            }
            catch { }

            s_assemblySearchDirs = dirs.Where(d => !string.IsNullOrEmpty(d)).Distinct(StringComparer.OrdinalIgnoreCase).ToArray();
            AppDomain.CurrentDomain.AssemblyResolve += OnAssemblyResolve;
        }
        catch
        {
            s_assemblySearchDirs = Array.Empty<string>();
        }
    }

    // Runs `action` via the WinUI3 (Windows App SDK) DispatcherQueue when this is a WinUI3 app.
    // Returns true if it handled the call — either by running inline (already on the UI thread) or by
    // enqueuing to the UI thread's queue from a background thread and waiting. Returns false when no
    // DispatcherQueue type exists (classic UWP/WPF app) so the caller can try its other dispatchers.
    // A single probed path keeps one bridge binary working for both classic UWP and WinUI3 — there is
    // no separate method or "is this WinUI3?" flag; whichever dispatcher is actually present wins.
    private static bool TryDispatcherQueueInvoke(Action action)
    {
        try
        {
            Type? dqType = null;
            foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
            {
                dqType = asm.GetType("Microsoft.UI.Dispatching.DispatcherQueue");
                if (dqType != null) break;
            }
            if (dqType == null) return false; // not a WinUI3 app

            // Capture the UI queue whenever we're on a thread that owns one (the UI thread).
            var current = dqType.GetMethod("GetForCurrentThread", BindingFlags.Public | BindingFlags.Static)?.Invoke(null, null);
            if (current != null) s_uiDispatcherQueue = current;

            var queue = current ?? s_uiDispatcherQueue;
            if (queue == null) return false; // WinUI3 present, but the UI queue hasn't been seen yet

            // On the UI thread → run inline.
            if (queue.GetType().GetProperty("HasThreadAccess")?.GetValue(queue) is true)
            {
                action();
                return true;
            }

            // Background thread → enqueue onto the UI queue and block until it runs.
            var tryEnqueue = queue.GetType().GetMethods()
                .FirstOrDefault(m => m.Name == "TryEnqueue" && m.GetParameters().Length == 1);
            if (tryEnqueue == null) return false;
            var handlerType = tryEnqueue.GetParameters()[0].ParameterType; // DispatcherQueueHandler
            using var mre = new ManualResetEventSlim(false);
            var wrapped = new Action(() => { try { action(); } finally { mre.Set(); } });
            var del = Delegate.CreateDelegate(handlerType, wrapped.Target, wrapped.Method);
            if (tryEnqueue.Invoke(queue, new object[] { del }) is false) return false;
            mre.Wait();
            return true;
        }
        catch { return false; }
    }

    public static object? RunOnUIThread(int callbackId)
    {
        var action = new Action(() =>
        {
            unsafe
            {
                if (s_jsInvoker == null) return;
                byte* respPtr = null;
                int respLen = 0;
                s_jsInvoker(callbackId, null, 0, &respPtr, &respLen);
                if (respPtr != null && respLen > 0) Marshal.FreeHGlobal((IntPtr)respPtr);
            }
        });

        try
        {
            foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
            {
                var coreAppType = asm.GetType("Windows.ApplicationModel.Core.CoreApplication");
                if (coreAppType == null) continue;
                var mainViewProp = coreAppType.GetProperty("MainView", BindingFlags.Public | BindingFlags.Static);
                var mainView = mainViewProp?.GetValue(null);
                if (mainView == null) continue;
                var dispatcherProp = mainView.GetType().GetProperty("Dispatcher", BindingFlags.Public | BindingFlags.Instance);
                var dispatcher = dispatcherProp?.GetValue(mainView);
                if (dispatcher == null) continue;

                var hasAccessProp = dispatcher.GetType().GetProperty("HasThreadAccess", BindingFlags.Public | BindingFlags.Instance);
                if (hasAccessProp?.GetValue(dispatcher) is true)
                {
                    action();
                    return null;
                }

                foreach (var m in dispatcher.GetType().GetMethods().Where(m => m.Name == "RunAsync"))
                {
                    var parameters = m.GetParameters();
                    if (parameters.Length != 2) continue;
                    var enumType = parameters[0].ParameterType;
                    object priority = enumType.IsEnum ? Enum.ToObject(enumType, 0) : Activator.CreateInstance(enumType)!;
                    var handlerType = parameters[1].ParameterType;
                    var mre = new ManualResetEventSlim(false);
                    var wrapped = new Action(() => { try { action(); } finally { mre.Set(); } });
                    try
                    {
                        var d = Delegate.CreateDelegate(handlerType, wrapped.Target, wrapped.Method);
                        m.Invoke(dispatcher, new object[] { priority, d });
                        mre.Wait();
                        return null;
                    }
                    catch { }
                }
                break;
            }
        }
        catch { }

        try { if (TryDispatcherQueueInvoke(action)) return null; } catch { }

        try
        {
            var wpfDispatcherType = AppDomain.CurrentDomain.GetAssemblies()
                .Select(a => a.GetType("System.Windows.Threading.Dispatcher"))
                .FirstOrDefault(t => t != null);
            if (wpfDispatcherType != null)
            {
                var currentDispatcherProp = wpfDispatcherType.GetProperty("CurrentDispatcher", BindingFlags.Public | BindingFlags.Static);
                var dispatcher = currentDispatcherProp?.GetValue(null);
                if (dispatcher != null)
                {
                    var checkAccess = dispatcher.GetType().GetMethod("CheckAccess");
                    if (checkAccess?.Invoke(dispatcher, null) is true)
                    {
                        action();
                        return null;
                    }
                    var beginInvoke = dispatcher.GetType().GetMethod("BeginInvoke", new[] { typeof(Action) })
                        ?? dispatcher.GetType().GetMethods().FirstOrDefault(m => m.Name == "BeginInvoke" && m.GetParameters().Length == 1);
                    if (beginInvoke != null)
                    {
                        var mre = new ManualResetEventSlim(false);
                        beginInvoke.Invoke(dispatcher, new object[] { new Action(() => { try { action(); } finally { mre.Set(); } }) });
                        mre.Wait();
                        return null;
                    }
                }
            }
        }
        catch { }

        action();
        return null;
    }

    public static object? InvokeOnUIThread(Func<object?> callback)
    {
        var mre = new ManualResetEventSlim(false);
        object? result = null;
        var wrapped = new Action(() => { try { result = callback(); } finally { mre.Set(); } });

        try
        {
            foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
            {
                var coreAppType = asm.GetType("Windows.ApplicationModel.Core.CoreApplication");
                if (coreAppType == null) continue;
                var mainViewProp = coreAppType.GetProperty("MainView", BindingFlags.Public | BindingFlags.Static);
                var mainView = mainViewProp?.GetValue(null);
                if (mainView == null) continue;
                var dispatcherProp = mainView.GetType().GetProperty("Dispatcher", BindingFlags.Public | BindingFlags.Instance);
                var dispatcher = dispatcherProp?.GetValue(mainView);
                if (dispatcher == null) continue;

                var hasAccessProp = dispatcher.GetType().GetProperty("HasThreadAccess", BindingFlags.Public | BindingFlags.Instance);
                if (hasAccessProp?.GetValue(dispatcher) is true)
                {
                    wrapped();
                    return result;
                }

                foreach (var m in dispatcher.GetType().GetMethods().Where(m => m.Name == "RunAsync"))
                {
                    var parameters = m.GetParameters();
                    if (parameters.Length != 2) continue;
                    var enumType = parameters[0].ParameterType;
                    object priority = enumType.IsEnum ? Enum.ToObject(enumType, 0) : Activator.CreateInstance(enumType)!;
                    var handlerType = parameters[1].ParameterType;
                    try
                    {
                        var d = Delegate.CreateDelegate(handlerType, wrapped.Target, wrapped.Method);
                        m.Invoke(dispatcher, new object[] { priority, d });
                        mre.Wait();
                        return result;
                    }
                    catch { }
                }
                break;
            }
        }
        catch { }

        try { if (TryDispatcherQueueInvoke(wrapped)) return result; } catch { }

        try
        {
            var wpfDispatcherType = AppDomain.CurrentDomain.GetAssemblies()
                .Select(a => a.GetType("System.Windows.Threading.Dispatcher"))
                .FirstOrDefault(t => t != null);
            if (wpfDispatcherType != null)
            {
                var currentDispatcherProp = wpfDispatcherType.GetProperty("CurrentDispatcher", BindingFlags.Public | BindingFlags.Static);
                var dispatcher = currentDispatcherProp?.GetValue(null);
                if (dispatcher != null)
                {
                    var checkAccess = dispatcher.GetType().GetMethod("CheckAccess");
                    if (checkAccess?.Invoke(dispatcher, null) is true)
                    {
                        wrapped();
                        return result;
                    }
                    var beginInvoke = dispatcher.GetType().GetMethod("BeginInvoke", new[] { typeof(Action) })
                        ?? dispatcher.GetType().GetMethods().FirstOrDefault(m => m.Name == "BeginInvoke" && m.GetParameters().Length == 1);
                    if (beginInvoke != null)
                    {
                        var mre2 = new ManualResetEventSlim(false);
                        beginInvoke.Invoke(dispatcher, new object[] { new Action(() => { try { wrapped(); } finally { mre2.Set(); } }) });
                        mre2.Wait();
                        return result;
                    }
                }
            }
        }
        catch { }

        wrapped();
        return result;
    }

    private static bool RequiresUiThread(Type t)
    {
        var ns = t.Namespace ?? string.Empty;
        return ns.StartsWith("Windows.UI.Xaml", StringComparison.Ordinal) || ns.StartsWith("Microsoft.UI.Xaml", StringComparison.Ordinal);
    }

    internal static bool IsMarshaledForDifferentThread(Exception? ex)
    {
        if (ex == null) return false;
        // Unwrap TargetInvocationException if present.
        if (ex is TargetInvocationException tie) return IsMarshaledForDifferentThread(tie.InnerException);
        if (ex is System.Runtime.InteropServices.COMException ce)
        {
            uint h = (uint)ce.HResult;
            // RPC_E_WRONG_THREAD = 0x8001010E, E_FAIL = 0x80004005
            if (h == 0x8001010E || h == 0x80004005) return true;
        }
        return IsMarshaledForDifferentThread(ex.InnerException);
    }

    private static Assembly? OnAssemblyResolve(object? sender, ResolveEventArgs args)
    {
        try
        {
            var requested = new AssemblyName(args.Name).Name;
            if (string.IsNullOrEmpty(requested)) return null;

            if (s_assemblyLoadCache.TryGetValue(requested, out var cached)) return cached;

            // Check already-loaded assemblies first (avoid recursive Assembly.Load).
            var loadedAsm = AppDomain.CurrentDomain.GetAssemblies()
                .FirstOrDefault(a => string.Equals(a.GetName().Name, requested, StringComparison.OrdinalIgnoreCase));
            if (loadedAsm is not null)
            {
                s_assemblyLoadCache[requested] = loadedAsm;
                return loadedAsm;
            }

            // Try to locate a matching dll in known search directories.
            foreach (var dir in s_assemblySearchDirs)
            {
                try
                {
                    var candidate = Path.Combine(dir, requested + ".dll");
                    if (File.Exists(candidate))
                    {
                        try
                        {
                            var asm = AssemblyLoadContext.Default.LoadFromAssemblyPath(candidate);
                            s_assemblyLoadCache[requested] = asm;
                            return asm;
                        }
                        catch { }
                    }
                }
                catch { }
            }

            s_assemblyLoadCache[requested] = null;
        }
        catch { }
        return null;
    }

    [UnmanagedCallersOnly(EntryPoint = "RegisterJsCallback",
        CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    public static unsafe int RegisterJsCallback(
        delegate* unmanaged[Cdecl]<int, byte*, int, byte**, int*, void> callback)
    {
        s_jsInvoker = callback;
        return 0;
    }

    // Callback id registered by the runtime/JS that should receive unhandled
    // managed exceptions and unobserved task exceptions.
    private static int s_unhandledExceptionCallbackId = -1;

    // Called from JS/Rust to request that managed unhandled exceptions be
    // forwarded to the registered JS callback id.  The callback id must have
    // been created on the Rust side and point to a JS function stored in the
    // runtime's `DOTNET_JS_CALLBACKS` map.
    public static int RegisterUnhandledExceptionCallback(int callbackId)
    {
        s_unhandledExceptionCallbackId = callbackId;

        // Subscribe once (idempotent).
        try
        {
            AppDomain.CurrentDomain.UnhandledException -= OnAppDomainUnhandledException;
            AppDomain.CurrentDomain.UnhandledException += OnAppDomainUnhandledException;

            TaskScheduler.UnobservedTaskException -= OnUnobservedTaskException;
            TaskScheduler.UnobservedTaskException += OnUnobservedTaskException;

            // Try to subscribe to Windows.CoreApplication.UnhandledErrorDetected if available
            foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
            {
                var coreAppType = asm.GetType("Windows.ApplicationModel.Core.CoreApplication");
                if (coreAppType == null) continue;
                var ev = coreAppType.GetEvent("UnhandledErrorDetected");
                if (ev == null) continue;
                try
                {
                    var handlerType = ev.EventHandlerType;
                    var method = typeof(Bridge).GetMethod(nameof(CoreApplicationUnhandledErrorHandler), BindingFlags.NonPublic | BindingFlags.Static);
                    if (handlerType != null && method != null)
                    {
                        var del = Delegate.CreateDelegate(handlerType, method);
                        ev.AddEventHandler(null, del);
                    }
                }
                catch { }
            }
        }
        catch { }

        return 0;
    }

    // IInspectable IID — the common interface for all WinRT objects.
    // WinRT APIs typically QI for IInspectable (not just IUnknown) when they
    // accept a polymorphic `object` parameter or need runtime class identity.
    private static Guid s_iidIInspectable = new Guid("AF86E2E0-B12D-4C6A-9C5A-D7AA65101E90");

    // Return a canonical native pointer (IUnknown / IInspectable) for an
    // exported handle id. Returns 0 when no pointer is available. If a
    // pointer isn't already cached in `s_nativePtrs`, attempt to obtain one
    // via `Marshal.GetIUnknownForObject` and cache it for subsequent calls.
    public static IntPtr GetNativePtrForHandle(int handleId)
    {
        if (handleId <= 0) return IntPtr.Zero;
        if (s_nativePtrs.TryGetValue(handleId, out var p)) {
            return p;
        }

        if (!s_handles.TryGetValue(handleId, out var obj) || obj == null)
            return IntPtr.Zero;

        try
        {
            var ip = ObtainNativePtr(obj);
            if (ip != IntPtr.Zero)
            {
                StoreNativePtr(handleId, ip);
                return ip;
            }
        }
        catch
        {
        }

        return IntPtr.Zero;
    }

    // Obtain a WinRT-usable native pointer for a managed object.
    // Prefers IInspectable over bare IUnknown because WinRT runtime class
    // identity and interface lookup go through IInspectable.  If the managed
    // object itself implements IWinRTObject (C#/WinRT projection), its inner
    // native object is used directly so that QI succeeds for all inherited
    // WinRT interfaces (e.g. IUIElement for a FlexboxLayout subclass).
    private static IntPtr ObtainNativePtr(object obj)
    {
        // Path 1 — C#/WinRT IWinRTObject: use the projected inner object's pointer.
        // This gives the real native WinRT pointer, not the managed CCW, so
        // QueryInterface will succeed for all WinRT interfaces inherited by the type.
        try
        {
            var winrtObjType = obj.GetType().GetInterface("WinRT.IWinRTObject");
            if (winrtObjType != null)
            {
                var nativeObjProp = winrtObjType.GetProperty("NativeObject");
                var nativeObj = nativeObjProp?.GetValue(obj);
                if (nativeObj != null)
                {
                    var thisPtrProp = nativeObj.GetType().GetProperty("ThisPtr");
                    if (thisPtrProp?.GetValue(nativeObj) is IntPtr thisPtr && thisPtr != IntPtr.Zero)
                    {
                        Marshal.AddRef(thisPtr);
                        return thisPtr;
                    }
                }
            }
        }
        catch { }

        IntPtr iunknown = IntPtr.Zero;
        try
        {
            iunknown = Marshal.GetIUnknownForObject(obj);
        }
        catch { }

        if (iunknown == IntPtr.Zero) return IntPtr.Zero;

        // Path 2 — QI for IInspectable so WinRT runtime class identity works.
        try
        {
            var iidLocal = s_iidIInspectable;
            var hr = Marshal.QueryInterface(iunknown, in iidLocal, out var inspectable);
            if (hr == 0 && inspectable != IntPtr.Zero)
            {
                Marshal.Release(iunknown);
                return inspectable;
            }
        }
        catch { }

        // Path 3 — bare IUnknown fallback.
        return iunknown;
    }

    private static void OnAppDomainUnhandledException(object? sender, UnhandledExceptionEventArgs e)
    {
        try
        {
            var ex = e.ExceptionObject as Exception;
            var msg = ex?.ToString() ?? e.ExceptionObject?.ToString() ?? "(unknown)";
            SendUnhandledToJs("unhandled", msg);
        }
        catch { }
    }

    private static void OnUnobservedTaskException(object? sender, UnobservedTaskExceptionEventArgs e)
    {
        try
        {
            var msg = e.Exception?.ToString() ?? "(unobserved task exception)";
            SendUnhandledToJs("unobservedTask", msg);
        }
        catch { }
    }

    // Fallback handler for platform-specific CoreApplication unhandled events.
    private static void CoreApplicationUnhandledErrorHandler(object? sender, object? args)
    {
        try
        {
            var msg = args?.ToString() ?? "(core unhandled)";
            SendUnhandledToJs("coreUnhandled", msg);
        }
        catch { }
    }

    private static unsafe void SendUnhandledToJs(string kind, string message)
    {
        try
        {
            if (s_jsInvoker == null || s_unhandledExceptionCallbackId <= 0) return;
            var payload = JsonSerializer.Serialize(new { kind = kind, message = message });
            var bytes = System.Text.Encoding.UTF8.GetBytes(payload);
            fixed (byte* p = bytes)
            {
                byte* respPtr = null;
                int respLen = 0;
                s_jsInvoker(s_unhandledExceptionCallbackId, p, bytes.Length, &respPtr, &respLen);
                if (respPtr != null && respLen > 0) Marshal.FreeHGlobal((IntPtr)respPtr);
            }
        }
        catch { }
    }

    [UnmanagedCallersOnly(EntryPoint = "Invoke",
        CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    public static unsafe int Invoke(
        byte* requestPtr, int requestLen,
        byte** responsePtr, int* responseLenPtr)
    {
        try
        {
            var span = new ReadOnlySpan<byte>(requestPtr, requestLen);
            var req  = JsonSerializer.Deserialize(span, BridgeJsonContext.Default.InvokeRequest)!;
            WriteResult(Dispatch(req), responsePtr, responseLenPtr);
        }
        catch (Exception ex)
        {
            WriteError(Unwrap(ex).Message, responsePtr, responseLenPtr);
        }
        return 0;
    }

    [UnmanagedCallersOnly(EntryPoint = "Free",
        CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    public static unsafe void Free(byte* ptr)
    {
        if (ptr != null) Marshal.FreeHGlobal((IntPtr)ptr);
    }
}
