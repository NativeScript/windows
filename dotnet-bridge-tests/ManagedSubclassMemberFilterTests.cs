using System;
using System.Buffers;
using System.Buffers.Binary;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using NativeScriptBridge;
using Xunit;

namespace DotNetBridgeTests;

// Covers the fixes made to the dynamic managed-subclass proxy (Bridge.Proxy.cs):
//   1. Call-base fallback — a base virtual JS doesn't override must run the real base
//      implementation, not silently return default(T).
//   2. Member-set-scoped type cache — two differently-configured subclasses of the same
//      base type must not contaminate each other's dispatch.
//   3. Property-accessor ergonomics — get_/set_-prefixed member names round-trip correctly.
//   4. Interface implementation — AddInterfaceImplementation + dispatch is mechanically correct.
[Collection("Bridge")]
public sealed class ManagedSubclassMemberFilterTests : IDisposable
{
    private static readonly List<(string Method, object?[] Args)> s_calls = new();
    private static Func<string, object?[], object?>? s_handler;

    [UnmanagedCallersOnly(CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    private static unsafe void Invoker(int id, byte* argsPtr, int argsLen, byte** respPtr, int* respLen)
    {
        var (method, args) = DecodeCall(new ReadOnlySpan<byte>(argsPtr, argsLen));
        s_calls.Add((method, args));
        var result = s_handler?.Invoke(method, args);
        WriteResponse(result, respPtr, respLen);
    }

    public unsafe ManagedSubclassMemberFilterTests()
    {
        Bridge.ClearCaches();
        s_calls.Clear();
        s_handler = null;
        Bridge.s_jsInvoker = &Invoker;
    }

    public unsafe void Dispose()
    {
        Bridge.s_jsInvoker = null;
        Bridge.ClearCaches();
    }

    [Fact]
    public void CallBaseFallback_UnoverriddenVirtual_RunsRealBaseImplementation()
    {
        var handle = CreateSubclass(
            "DotNetBridgeTests.CallBaseFallbackBase",
            interfaceNames: [],
            memberNames: ["Describe"],
            callbackId: 1);

        s_handler = (method, args) => method == "Describe" ? "js-describe" : null;

        // Overridden: dispatches to JS.
        var describeResult = InvokeInstance(handle, "Describe");
        Assert.Equal("js-describe", describeResult.PrimitiveValue());
        Assert.Contains(s_calls, c => c.Method == "Describe");

        // NOT overridden: must run the real base method, never even asking JS.
        var greetResult = InvokeInstance(handle, "Greet", "World");
        Assert.Equal("base-greet:World", greetResult.PrimitiveValue());
        Assert.DoesNotContain(s_calls, c => c.Method == "Greet");

        Release(handle);
    }

    [Fact]
    public void DifferentMemberSets_SameBaseType_DoNotContaminateEachOther()
    {
        var handleA = CreateSubclass(
            "DotNetBridgeTests.CallBaseFallbackBase", [], ["Describe"], callbackId: 2);
        var handleB = CreateSubclass(
            "DotNetBridgeTests.CallBaseFallbackBase", [], ["Greet"], callbackId: 3);

        s_handler = (method, args) => "js:" + method;

        // A overrides Describe only -> Greet falls through to the base on A.
        Assert.Equal("js:Describe", InvokeInstance(handleA, "Describe").PrimitiveValue());
        Assert.Equal("base-greet:x", InvokeInstance(handleA, "Greet", "x").PrimitiveValue());

        // B overrides Greet only -> Describe falls through to the base on B.
        Assert.Equal("base-describe", InvokeInstance(handleB, "Describe").PrimitiveValue());
        Assert.Equal("js:Greet", InvokeInstance(handleB, "Greet", "x").PrimitiveValue());

        Release(handleA);
        Release(handleB);
    }

    [Fact]
    public void PropertyAccessors_GetSetPrefixedMemberNames_RoundTrip()
    {
        var handle = CreateSubclass(
            "DotNetBridgeTests.CallBaseFallbackBase", [], ["get_Text", "set_Text"], callbackId: 4);

        string? stored = null;
        s_handler = (method, args) =>
        {
            if (method == "set_Text") { stored = (string?)args[0]; return null; }
            if (method == "get_Text") return stored ?? "(unset)";
            return null;
        };

        InvokeInstance(handle, "set_Text", "hello");
        Assert.Contains(s_calls, c => c.Method == "set_Text" && (string?)c.Args[0] == "hello");

        var getResult = InvokeInstance(handle, "get_Text");
        Assert.Equal("hello", getResult.PrimitiveValue());

        Release(handle);
    }

    [Fact]
    public void Interfaces_RequestedInterface_IsImplementedAndDispatches()
    {
        var handle = CreateSubclass(
            "System.Object", ["DotNetBridgeTests.ITestNotify"], ["Notify"], callbackId: 5);

        s_handler = (method, args) => method == "Notify" ? "notified:" + args[0] : null;

        Assert.True(Bridge.s_handles.TryGetValue(handle, out var instance));
        Assert.IsAssignableFrom<ITestNotify>(instance);

        var result = InvokeInstance(handle, "Notify", "hi");
        Assert.Equal("notified:hi", result.PrimitiveValue());

        Release(handle);
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    private static int CreateSubclass(
        string typeName, string[] interfaceNames, string[] memberNames, int callbackId)
    {
        var pkt = BuildPacket(w =>
        {
            w.WriteByte(0x0A);
            w.WriteString16("DotNetBridgeTests");
            w.WriteString16(typeName);
            w.WriteI32(interfaceNames.Length);
            foreach (var n in interfaceNames) w.WriteString16(n);
            w.WriteI32(memberNames.Length);
            foreach (var n in memberNames) w.WriteString16(n);
            w.WriteI32(callbackId);
        });
        var r = new BinReader(pkt.AsSpan());
        var res = Bridge.DispatchBin(ref r);
        Assert.Equal(DispatchKind.Handle, res.Kind());
        return res.HandleId();
    }

    private static DispatchResult InvokeInstance(int handle, string method, params object[] args)
    {
        var pkt = BuildPacket(w =>
        {
            w.WriteByte(0x01);
            w.WriteI32(handle);
            w.WriteString16(method);
            w.WriteByte((byte)args.Length);
            foreach (var a in args)
            {
                if (a is string s) { w.WriteByte(0x05); w.WriteString16(s); }
                else if (a is int i) { w.WriteByte(0x03); w.WriteI32(i); }
                else throw new NotSupportedException("test helper only supports string/int args");
            }
        });
        var r = new BinReader(pkt.AsSpan());
        return Bridge.DispatchBin(ref r);
    }

    private static void Release(int handle)
    {
        var pkt = BuildPacket(w =>
        {
            w.WriteByte(0x04);
            w.WriteI32(handle);
        });
        var r = new BinReader(pkt.AsSpan());
        Bridge.DispatchBin(ref r);
    }

    private static byte[] BuildPacket(Action<BinWriter> build)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        build(w);
        return buf.WrittenSpan.ToArray();
    }

    // Decodes a dispatcher call built by Bridge.JsDelegate's WriteCallbackArg:
    // [HandleRef instance][string32 member][arg...], each argument tagged individually.
    private static (string Method, object?[] Args) DecodeCall(ReadOnlySpan<byte> span)
    {
        var r = new BinReader(span);
        var count = r.ReadByte();
        if (count < 2)
            throw new InvalidOperationException($"expected at least 2 dispatcher args, got {count}");

        ReadOutgoingHandleTag(ref r); // instance handle — unused by these tests

        var methodTag = r.ReadByte();
        if (methodTag != 0x05)
            throw new InvalidOperationException($"expected string tag 0x05 for method name, got 0x{methodTag:X2}");
        var method = r.ReadString32();

        var args = new object?[count - 2];
        for (int i = 0; i < args.Length; i++)
        {
            var tag = r.ReadByte();
            args[i] = tag switch
            {
                0x00 => null,
                0x01 => false,
                0x02 => true,
                0x03 => r.ReadI32(),
                0x04 => r.ReadF64(),
                0x05 => r.ReadString32(),
                0x0D => r.ReadString32(),
                _ => throw new InvalidOperationException($"unexpected argument tag 0x{tag:X2}"),
            };
        }
        return (method, args);
    }

    private static int ReadOutgoingHandleTag(ref BinReader r)
    {
        var tag = r.ReadByte();
        if (tag != 0x06)
            throw new InvalidOperationException($"expected handle tag 0x06, got 0x{tag:X2}");
        var handleId = r.ReadI32();
        _ = r.ReadString16(); // type name — unused
        var nativeFlag = r.ReadByte();
        if (nativeFlag == 1) _ = r.ReadI64();
        return handleId;
    }

    private static unsafe void WriteResponse(object? result, byte** respPtr, int* respLen)
    {
        if (result is not string s)
        {
            *respPtr = null;
            *respLen = 0;
            return;
        }

        var strBytes = Encoding.UTF8.GetBytes(s);
        var total = 1 + 4 + strBytes.Length;
        var mem = (byte*)Marshal.AllocHGlobal(total);
        mem[0] = 0x05;
        BinaryPrimitives.WriteUInt32LittleEndian(new Span<byte>(mem + 1, 4), (uint)strBytes.Length);
        strBytes.CopyTo(new Span<byte>(mem + 5, strBytes.Length));
        *respPtr = mem;
        *respLen = total;
    }
}
