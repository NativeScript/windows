using System;
using System.Buffers;
using System.Runtime.InteropServices;
using BenchmarkDotNet.Attributes;
using NativeScriptBridge;

/// <summary>
/// Full managed cost of one bridge call as the runtime pays it: the request goes through the
/// [UnmanagedCallersOnly] InvokeBinary entry point (via an unmanaged function pointer, like Rust
/// calls it) and the unmanaged response buffer is freed through Free.
///
///   dotnet run -c Release -- --filter *AbiBenchmarks* --job short
/// </summary>
[MemoryDiagnoser]
public unsafe class AbiBenchmarks
{
    private static readonly delegate* unmanaged[Cdecl]<byte*, int, byte**, int*, int> s_invoke = &Bridge.InvokeBinary;
    private static readonly delegate* unmanaged[Cdecl]<byte*, void> s_free = &Bridge.Free;

    private byte[] _staticCbrt = null!;
    private byte[] _staticCbrtAsm = null!;
    private byte[] _overloadMax = null!;
    private byte[] _overloadGetExtension = null!;
    private byte[] _stringArg = null!;
    private byte[] _propGet = null!;
    private byte[] _propSet = null!;
    private byte[] _ctor = null!;
    private byte[] _returnHandle = null!;

    [GlobalSetup]
    public void Setup()
    {
        Bridge.ClearCaches();
        _staticCbrt = Static("System.Math", "", "Cbrt", w => { w.WriteByte(0x03); w.WriteI32(27); });
        _staticCbrtAsm = Static("System.Math", "System.Runtime", "Cbrt", w => { w.WriteByte(0x03); w.WriteI32(27); });
        _overloadMax = Static("System.Math", "", "Max", w => { w.WriteByte(0x03); w.WriteI32(300); w.WriteByte(0x03); w.WriteI32(7); }, 2);
        _overloadGetExtension = Static("System.IO.Path", "", "GetExtension", w => { w.WriteByte(0x05); w.WriteString16("file.txt"); });
        _stringArg = Static("System.String", "", "IsNullOrEmpty", w => { w.WriteByte(0x05); w.WriteString16("abc"); });
        _ctor = Packet(w => { w.WriteByte(0x03); w.WriteString16("System.Text.StringBuilder"); w.WriteString16(""); w.WriteByte(0); });
        _returnHandle = Static("System.Diagnostics.Stopwatch", "", "StartNew", null, 0);

        var sb = Bridge.Dispatch(new InvokeRequest(null, "System.Text.StringBuilder", ".ctor", null, null)).HandleId();
        _propGet = Packet(w => { w.WriteByte(0x01); w.WriteI32(sb); w.WriteString16("get_Length"); w.WriteByte(0); });
        _propSet = Packet(w => { w.WriteByte(0x01); w.WriteI32(sb); w.WriteString16("set_Length"); w.WriteByte(1); w.WriteByte(0x03); w.WriteI32(0); });

        foreach (var p in new[] { _staticCbrt, _staticCbrtAsm, _overloadMax, _overloadGetExtension, _stringArg, _propGet, _propSet })
        {
            var resp = Call(p);
            if (resp[0] == 0xFF) throw new InvalidOperationException("bridge error: " + System.Text.Encoding.UTF8.GetString(resp, 5, resp.Length - 5));
        }
    }

    [Benchmark] public int StaticCall() => CallLen(_staticCbrt);
    [Benchmark] public int StaticCallWithAssembly() => CallLen(_staticCbrtAsm);
    [Benchmark] public int OverloadedInt() => CallLen(_overloadMax);
    [Benchmark] public int OverloadedString() => CallLen(_overloadGetExtension);
    [Benchmark] public int StringArg() => CallLen(_stringArg);
    [Benchmark] public int PropertyGet() => CallLen(_propGet);
    [Benchmark] public int PropertySet() => CallLen(_propSet);

    [Benchmark]
    public void ConstructRelease() => Release(Call(_ctor));

    [Benchmark]
    public void ReturnHandleRelease() => Release(Call(_returnHandle));

    private void Release(byte[] handleResponse)
    {
        var id = BitConverter.ToInt32(handleResponse, 1);
        CallLen(Packet(w => { w.WriteByte(0x04); w.WriteI32(id); }));
    }

    private static int CallLen(byte[] packet)
    {
        byte* resp = null;
        int len = 0;
        fixed (byte* p = packet) s_invoke(p, packet.Length, &resp, &len);
        s_free(resp);
        return len;
    }

    private static byte[] Call(byte[] packet)
    {
        byte* resp = null;
        int len = 0;
        fixed (byte* p = packet) s_invoke(p, packet.Length, &resp, &len);
        var bytes = new ReadOnlySpan<byte>(resp, len).ToArray();
        s_free(resp);
        return bytes;
    }

    private static byte[] Static(string type, string assembly, string method, Action<BinWriter>? args, int argCount = 1)
        => Packet(w =>
        {
            w.WriteByte(0x02);
            w.WriteString16(type);
            w.WriteString16(assembly);
            w.WriteString16(method);
            w.WriteByte((byte)argCount);
            args?.Invoke(w);
        });

    private static byte[] Packet(Action<BinWriter> write)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        write(new BinWriter(buf));
        return buf.WrittenSpan.ToArray();
    }
}
