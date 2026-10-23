using System;
using System.Buffers;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using BenchmarkDotNet.Attributes;
using NativeScriptBridge;

public class CallbackBase
{
    public virtual int Compute(int value) => value;

    public int Run(int value) => Compute(value);
}

/// <summary>
/// Managed cost of .NET calling into JS: a JS function passed as a delegate, and a JS subclass
/// override called from C#. The "JS side" is an unmanaged function that answers at once, so
/// what's measured is the bridge's own work (argument encoding, response handling).
///
///   dotnet run -c Release -- --filter *CallbackBenchmarks* --job short
/// </summary>
[MemoryDiagnoser]
public unsafe class CallbackBenchmarks
{
    private Func<int, int> _func = null!;
    private Action<string> _action = null!;
    private CallbackBase _override = null!;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    private static void FakeJs(int id, byte* args, int len, byte** resp, int* respLen)
    {
        // Every callback answers with the int 42, allocated the way the runtime allocates it.
        var p = (byte*)Marshal.AllocHGlobal(5);
        p[0] = 0x03;
        *(int*)(p + 1) = 42;
        *resp = p;
        *respLen = 5;
    }

    [GlobalSetup]
    public void Setup()
    {
        Bridge.ClearCaches();
        Bridge.s_jsInvoker = &FakeJs;
        _func = (Func<int, int>)Bridge.MakeJsDelegate(typeof(Func<int, int>), 1);
        _action = (Action<string>)Bridge.MakeJsDelegate(typeof(Action<string>), 2);

        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        w.WriteByte(0x0A);
        w.WriteString16("");
        w.WriteString16(typeof(CallbackBase).FullName!);
        w.WriteI32(0);
        w.WriteI32(1);
        w.WriteString16("Compute");
        w.WriteI32(3);
        w.WriteByte(0);
        var r = new BinReader(buf.WrittenSpan);
        var handle = Bridge.DispatchBin(ref r).HandleId();
        _override = (CallbackBase)Bridge.s_handles[handle]!;
        if (_override.Run(1) != 42) throw new InvalidOperationException("override did not reach the fake JS side");
    }

    [Benchmark] public int DelegateWithResult() => _func(5);

    [Benchmark] public void VoidDelegate() => _action("hello");

    [Benchmark] public int OverrideFromCSharp() => _override.Run(5);
}
