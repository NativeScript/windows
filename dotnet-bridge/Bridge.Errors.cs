using System;
using System.Buffers;
using System.Threading.Tasks;

namespace NativeScriptBridge;

/// Thrown into managed code when a JavaScript callback threw; StackTrace leads with the JS stack.
public sealed class JsException(string message, string? jsStack = null) : Exception(message)
{
    public string? JsStack { get; } = jsStack;

    public override string? StackTrace =>
        string.IsNullOrEmpty(JsStack) ? base.StackTrace : JsStack + Environment.NewLine + base.StackTrace;
}

public static partial class Bridge
{
    // 0xFF, message, exception type, .NET stack trace, exception handle (`error.nativeException` in
    // JS). Older readers stop after the message.
    private static unsafe void WriteBinError(Exception ex, byte** outPtr, int* outLen)
    {
        var buf = new ArrayBufferWriter<byte>(256);
        var w = new BinWriter(buf);
        w.WriteByte(0xFF);
        w.WriteString32(ex.Message);
        w.WriteString16(ex.GetType().FullName ?? ex.GetType().Name);
        w.WriteString32(ex.StackTrace ?? "");
        int handle = -1;
        try { handle = HandleFor(ex); } catch { }
        w.WriteI32(handle);
        WriteUnmanaged(buf.WrittenSpan, outPtr, outLen);
    }

    // The handler JS registered for exceptions nothing in .NET caught.
    private static Action<string, object?>? s_unhandledHandler;

    /// Forwards unhandled .NET exceptions ("unhandled") and faulted tasks nobody awaited
    /// ("unobservedTask") to `handler`, with the exception.
    public static void RegisterUnhandledExceptionHandler(Action<string, object?> handler)
    {
        s_unhandledHandler = handler;
        AppDomain.CurrentDomain.UnhandledException -= OnUnhandled;
        AppDomain.CurrentDomain.UnhandledException += OnUnhandled;
        TaskScheduler.UnobservedTaskException -= OnUnobserved;
        TaskScheduler.UnobservedTaskException += OnUnobserved;
    }

    private static void OnUnhandled(object? sender, UnhandledExceptionEventArgs e)
    {
        try { s_unhandledHandler?.Invoke("unhandled", e.ExceptionObject); } catch { }
    }

    private static void OnUnobserved(object? sender, UnobservedTaskExceptionEventArgs e)
    {
        try { s_unhandledHandler?.Invoke("unobservedTask", (object?)e.Exception.InnerException ?? e.Exception); } catch { }
    }
}
