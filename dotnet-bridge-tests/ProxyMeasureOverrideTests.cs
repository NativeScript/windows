using System;
using System.Buffers;
using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Text;
using NativeScriptBridge;
using Xunit;

namespace DotNetBridgeTests;

[Collection("Bridge")]
public sealed class ProxyMeasureOverrideTests : IDisposable
{
    private static int    s_capturedId;
    private static byte[] s_capturedArgs = [];

    [UnmanagedCallersOnly(CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    private static unsafe void CaptureInvoker(
        int id, byte* argsPtr, int argsLen, byte** respPtr, int* respLen)
    {
        s_capturedId   = id;
        s_capturedArgs = new ReadOnlySpan<byte>(argsPtr, argsLen).ToArray();
        *respPtr = null;
        *respLen = 0;
    }

    public unsafe ProxyMeasureOverrideTests()
    {
        Bridge.ClearCaches();
        s_capturedId   = -1;
        s_capturedArgs = [];
        Bridge.s_jsInvoker = &CaptureInvoker;
    }

    public unsafe void Dispose()
    {
        Bridge.s_jsInvoker = null;
        Bridge.ClearCaches();
    }

    [Fact]
    public void MeasureOverride_CustomOverride_InvokesJsCallback()
    {
        // Create a JS-backed subclass for our test base type
        var pkt = BuildPacket(w =>
        {
            w.WriteByte(0x0A); // create_js_subclass
            // Binary protocol expects: assembly, typeName, interfaceCount+names,
            // memberCount+names, callbackId for opcode 0x0A.
            w.WriteString16("DotNetBridgeTests");
            w.WriteString16("DotNetBridgeTests.MeasureOverrideBase");
            w.WriteI32(0); // interfaceCount
            w.WriteI32(1); // memberCount
            w.WriteString16("MeasureOverride");
            w.WriteI32(777); // callbackId
        });

        var r = new BinReader(pkt.AsSpan());
        var res = Bridge.DispatchBin(ref r);
        Assert.Equal(DispatchKind.Handle, res.Kind());
        var handle = res.HandleId();

        // Call MeasureOverride on the instance; expect the override to dispatch to JS
        var call = BuildPacket(w =>
        {
            w.WriteByte(0x01); // instance call
            w.WriteI32(handle);
            w.WriteString16("MeasureOverride");
            w.WriteByte(1); // arg_count
            w.WriteByte(0x03); // i32
            w.WriteI32(123);
        });

        var r2 = new BinReader(call.AsSpan());
        Bridge.DispatchBin(ref r2);

        Assert.Equal(777, s_capturedId);
        Assert.True(s_capturedArgs.Length > 0);

        // Parse captured binary: [count=3][handle][string member][i32 argument]
        var span = s_capturedArgs.AsSpan();
        Assert.Equal(3, span[0]);
        var off = 1;
        Assert.Equal(0x06, span[off++]);
        var instHandle = BinaryPrimitives.ReadInt32LittleEndian(span.Slice(off, 4)); off += 4;
        // Skip the type-name that follows the handle (u16 len + bytes) and the native-pointer flag
        var typeNameLen = BinaryPrimitives.ReadUInt16LittleEndian(span.Slice(off, 2)); off += 2;
        off += typeNameLen;
        var nativeFlag = span[off++];
        if (nativeFlag == 1) off += 8;

        Assert.Equal(0x05, span[off++]);
        var strLen = BinaryPrimitives.ReadUInt32LittleEndian(span.Slice(off, 4)); off += 4;
        var name = Encoding.UTF8.GetString(span.Slice(off, (int)strLen)); off += (int)strLen;
        Assert.Equal("MeasureOverride", name);
        // The argument travels as its own value, not as a handle to an object[].
        Assert.Equal(0x03, span[off++]);
        Assert.Equal(123, BinaryPrimitives.ReadInt32LittleEndian(span.Slice(off, 4)));

        // Release created instance
        Bridge.Dispatch(new InvokeRequest(null, null, "__release", handle, null));
    }

    private static byte[] BuildPacket(Action<BinWriter> build)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w   = new BinWriter(buf);
        build(w);
        return buf.WrittenSpan.ToArray();
    }
}
