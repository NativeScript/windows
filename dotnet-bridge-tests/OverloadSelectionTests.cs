using System;
using System.Buffers;
using System.Text;
using NativeScriptBridge;
using Xunit;

namespace DotNetBridgeTests;

public static class OverloadFixtures
{
    public static string Pick(int value) => "int";
    public static string Pick(long value) => "long";
    public static string Pick(double value) => "double";
    public static string Pick(string value) => "string";
    public static string Pick(object value) => "object";

    public static string Narrow(byte value) => "byte";
    public static string Narrow(short value) => "short";

    public static string Echo(string value) => value;
}

[Collection("Bridge")]
/// Overloads with the same parameter count resolve to the one whose parameter types best fit the
/// arguments, not the first one they happen to convert to.
public sealed class OverloadSelectionTests : IDisposable
{
    public OverloadSelectionTests() => Bridge.ClearCaches();
    public void Dispose()           => Bridge.ClearCaches();

    [Fact]
    public void MathAbs_Double_PicksDoubleOverload()
    {
        var result = Static("System.Math", "Abs", w => { w.WriteByte(0x04); w.WriteF64(-0.5); });
        Assert.Equal(0.5, result.PrimitiveValue());
    }

    [Fact]
    public void MathAbs_IntAtSByteMin_PicksIntOverload()
    {
        // Abs(sbyte) would throw OverflowException for -128.
        var result = Static("System.Math", "Abs", w => { w.WriteByte(0x03); w.WriteI32(-128); });
        Assert.Equal(128, result.PrimitiveValue());
    }

    [Fact]
    public void MathMax_Ints_PicksIntOverload()
    {
        var result = Static("System.Math", "Max", w =>
        {
            w.WriteByte(0x03); w.WriteI32(300);
            w.WriteByte(0x03); w.WriteI32(7);
        }, argCount: 2);
        Assert.Equal(300, result.PrimitiveValue());
    }

    [Theory]
    [InlineData((byte)0x03, "int")]
    [InlineData((byte)0x04, "double")]
    [InlineData((byte)0x05, "string")]
    [InlineData((byte)0x02, "object")]
    public void ExactArgumentTypeWins(byte tag, string expected)
    {
        var result = Static("DotNetBridgeTests.OverloadFixtures", "Pick", w =>
        {
            w.WriteByte(tag);
            if (tag == 0x03) w.WriteI32(5);
            else if (tag == 0x04) w.WriteF64(5.5);
            else if (tag == 0x05) w.WriteString16("five");
        });
        Assert.Equal(expected, result.PrimitiveValue());
    }

    [Fact]
    public void LargeIntegralDouble_PicksLong()
    {
        var result = Static("DotNetBridgeTests.OverloadFixtures", "Pick", w => { w.WriteByte(0x04); w.WriteF64(3e9); });
        // double is still the exact match for a double argument.
        Assert.Equal("double", result.PrimitiveValue());
    }

    [Fact]
    public void NarrowingPicksTheTypeTheValueFits()
    {
        var small = Static("DotNetBridgeTests.OverloadFixtures", "Narrow", w => { w.WriteByte(0x03); w.WriteI32(200); });
        Assert.Equal("byte", small.PrimitiveValue());
        var large = Static("DotNetBridgeTests.OverloadFixtures", "Narrow", w => { w.WriteByte(0x03); w.WriteI32(-200); });
        Assert.Equal("short", large.PrimitiveValue());
    }

    [Fact]
    public void StringOverload_SkipsSpanOverload()
    {
        var result = Static("System.IO.Path", "GetExtension", w => { w.WriteByte(0x05); w.WriteString16("file.txt"); });
        Assert.Equal(".txt", result.PrimitiveValue());
    }

    [Fact]
    public void Constructor_StringArgument_PicksStringOverload()
    {
        // StringBuilder(int) comes first by declaration; "q" must reach StringBuilder(string).
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        w.WriteByte(0x03);
        w.WriteString16("System.Text.StringBuilder");
        w.WriteString16("");
        w.WriteByte(1);
        w.WriteByte(0x05); w.WriteString16("q");
        var r = new BinReader(buf.WrittenSpan);
        var handle = Bridge.DispatchBin(ref r).HandleId();
        Assert.True(Bridge.s_handles.TryGetValue(handle, out var sb));
        Assert.Equal("q", sb!.ToString());
    }

    [Fact]
    public void StringArguments_AreNotInterned()
    {
        var unique = "arg-" + Guid.NewGuid().ToString("N");
        var result = Static("DotNetBridgeTests.OverloadFixtures", "Echo", w => { w.WriteByte(0x05); w.WriteString16(unique); });
        Assert.Equal(unique, result.PrimitiveValue());
        Assert.Null(string.IsInterned(unique));
    }

    [Fact]
    public void NameCache_ReturnsTheDecodedName()
    {
        var a = Encoding.UTF8.GetBytes("System.Math");
        var b = Encoding.UTF8.GetBytes("System.Text");
        Assert.Equal("System.Math", NameCache.Get(a));
        Assert.Equal("System.Text", NameCache.Get(b));
        Assert.Same(NameCache.Get(a), NameCache.Get(a));
        Assert.Equal("Größe", NameCache.Get(Encoding.UTF8.GetBytes("Größe")));
        Assert.Equal("", NameCache.Get(ReadOnlySpan<byte>.Empty));
    }

    private static DispatchResult Static(string type, string method, Action<BinWriter> args, int argCount = 1)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        w.WriteByte(0x02);
        w.WriteString16(type);
        w.WriteString16("");
        w.WriteString16(method);
        w.WriteByte((byte)argCount);
        args(w);
        var r = new BinReader(buf.WrittenSpan);
        return Bridge.DispatchBin(ref r);
    }
}
