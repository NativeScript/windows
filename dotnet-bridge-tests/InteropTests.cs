using System;
using System.Buffers;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using NativeScriptBridge;
using Xunit;

namespace DotNetBridgeTests;

public static class InteropFixtures
{
    public const int Limit = 10;
    public static readonly string Fixed = "fixed";
    public static int Counter;

    public static string Optional(string a, string b = "B", int c = 3) => $"{a}{b}{c}";
    public static bool TryDouble(string text, out int result) => (result = int.TryParse(text, out var v) ? v * 2 : 0) != 0;
    public static void Swap(ref int a, ref int b) => (a, b) = (b, a);
    public static string Point(int x = 0, int y = 0) => $"({x},{y})";
    public static T Identity<T>(T value) => value;
    public static string NameOf<T>() => typeof(T).Name;
    public static int CountOf<T>(IEnumerable<T> items) => items.Count();
    public static long Echo(long value) => value;
    public static int Sum(byte[] data) => data.Sum(b => (int)b);
    public static int SumInts(int[] data) => data.Sum();
    public static long StreamLength(Stream s) => s.Length;
    public static object Shared { get; } = new object();

    public class Nested
    {
        public static string Where() => "nested";
    }
}

public sealed class FieldHolder
{
    public int Value = 7;
    public readonly int Fixed = 1;
}

public static class InteropExtensions
{
    public static int Doubled(this FieldHolder holder) => holder.Value * 2;
}

[Collection("Bridge")]
/// Handle identity, fields, the slow-path binder (optional, out/ref, named, generic, extension
/// methods), longs, binary data and type names.
public sealed class InteropTests : IDisposable
{
    public InteropTests() => Bridge.ClearCaches();
    public void Dispose() => Bridge.ClearCaches();

    private const string Fixtures = "DotNetBridgeTests.InteropFixtures";

    [Fact]
    public void SameObject_GetsTheSameHandle_UntilEveryExportIsGivenBack()
    {
        var first = Static(Fixtures, "get_Shared", null, 0).HandleId();
        var second = Static(Fixtures, "get_Shared", null, 0).HandleId();
        Assert.Equal(first, second);

        Release(first, 1);
        Assert.True(Bridge.s_handles.ContainsKey(first));
        Release(first, 1);
        Assert.False(Bridge.s_handles.ContainsKey(first));

        var third = Static(Fixtures, "get_Shared", null, 0).HandleId();
        Assert.NotEqual(first, third);
    }

    [Fact]
    public void ReleaseWithoutACount_DropsTheHandle()
    {
        var h = Static(Fixtures, "get_Shared", null, 0).HandleId();
        Static(Fixtures, "get_Shared", null, 0);
        Release(h, null);
        Assert.False(Bridge.s_handles.ContainsKey(h));
    }

    [Fact]
    public void Fields_ReadAndWrite()
    {
        Assert.Equal(10, Static(Fixtures, "get_Limit", null, 0).PrimitiveValue());
        Assert.Equal("fixed", Static(Fixtures, "get_Fixed", null, 0).PrimitiveValue());
        Static(Fixtures, "set_Counter", w => { w.WriteByte(0x03); w.WriteI32(4); });
        Assert.Equal(4, InteropFixtures.Counter);
    }

    [Fact]
    public void ReadOnlyField_RefusesAssignment()
    {
        var h = Construct(typeof(FieldHolder).FullName!);
        Assert.Throws<FieldAccessException>(() => Instance(h, "set_Fixed", w => { w.WriteByte(0x03); w.WriteI32(2); }));
        Instance(h, "set_Value", w => { w.WriteByte(0x03); w.WriteI32(9); });
        Assert.Equal(9, Instance(h, "get_Value", null, 0).PrimitiveValue());
    }

    [Fact]
    public void MembersList_IncludesFieldsAsProperties()
    {
        var result = Members(typeof(FieldHolder).FullName!);
        var buf = new ArrayBufferWriter<byte>();
        result.WriteAsBin(buf);
        var text = System.Text.Encoding.UTF8.GetString(buf.WrittenSpan);
        Assert.Contains("Value", text);
        Assert.Contains("Fixed", text);
        Assert.Contains("nestedTypes", text);
    }

    [Fact]
    public void OptionalParameters_AreFilledIn()
    {
        Assert.Equal("aB3", Static(Fixtures, "Optional", w => { w.WriteByte(0x05); w.WriteString16("a"); }).PrimitiveValue());
        Assert.Equal("ax3", Static(Fixtures, "Optional", w => { w.WriteByte(0x05); w.WriteString16("a"); w.WriteByte(0x05); w.WriteString16("x"); }, 2).PrimitiveValue());
    }

    [Fact]
    public void OmittedOutParameters_ComeBackWithTheResult()
    {
        var result = Static(Fixtures, "TryDouble", w => { w.WriteByte(0x05); w.WriteString16("21"); });
        Assert.Equal(new object?[] { true, 42 }, Items(result));
    }

    [Fact]
    public void ByRefCall_ReturnsTheArgumentsAfterTheCall()
    {
        var result = Static(Fixtures, "&Swap", w => { w.WriteByte(0x03); w.WriteI32(1); w.WriteByte(0x03); w.WriteI32(2); }, 2);
        Assert.Equal(new object?[] { null, 2, 1 }, Items(result));
    }

    [Fact]
    public void NamedArguments_GoToTheirParameters()
    {
        var result = Static(Fixtures, "Point#y", w => { w.WriteByte(0x03); w.WriteI32(5); });
        Assert.Equal("(0,5)", result.PrimitiveValue());
    }

    [Fact]
    public void GenericMethods_InferOrTakeTypeArguments()
    {
        Assert.Equal("same", Static(Fixtures, "Identity", w => { w.WriteByte(0x05); w.WriteString16("same"); }).PrimitiveValue());
        Assert.Equal("Guid", Static(Fixtures, "NameOf<System.Guid>", null, 0).PrimitiveValue());
        var count = Static(Fixtures, "CountOf", w =>
        {
            w.WriteByte(0x07); w.WriteU32(3);
            for (int i = 0; i < 3; i++) { w.WriteByte(0x03); w.WriteI32(i); }
        });
        Assert.Equal(3, count.PrimitiveValue());
    }

    [Fact]
    public void ExtensionMethods_AreCalledOnInstances()
    {
        var h = Construct(typeof(FieldHolder).FullName!);
        Assert.Equal(14, Instance(h, "Doubled", null, 0).PrimitiveValue());
    }

    [Fact]
    public void Int64Arguments_AndBigResults()
    {
        var result = Static(Fixtures, "Echo", w => { w.WriteByte(0x10); w.WriteI64(9007199254740993L); });
        var buf = new ArrayBufferWriter<byte>();
        result.WriteAsBin(buf);
        Assert.Equal(0x10, buf.WrittenSpan[0]);
        Assert.Equal(9007199254740993L, BitConverter.ToInt64(buf.WrittenSpan[1..]));

        var small = Static(Fixtures, "Echo", w => { w.WriteByte(0x10); w.WriteI64(5); });
        buf.ResetWrittenCount();
        small.WriteAsBin(buf);
        Assert.Equal(0x04, buf.WrittenSpan[0]);
    }

    [Fact]
    public void TypedArrays_BecomeArraysAndStreams()
    {
        Assert.Equal(6, Static(Fixtures, "Sum", w => Bytes(w, 0, [1, 2, 3])).PrimitiveValue());
        Assert.Equal(3, Static(Fixtures, "SumInts", w => Bytes(w, 4, BitConverter.GetBytes(1).Concat(BitConverter.GetBytes(2)).ToArray())).PrimitiveValue());
        Assert.Equal(4L, Static(Fixtures, "StreamLength", w => Bytes(w, 0, [1, 2, 3, 4])).PrimitiveValue());
    }

    [Fact]
    public void ByteArrayResults_AreBinary()
    {
        var result = Static("System.Convert", "FromBase64String", w => { w.WriteByte(0x05); w.WriteString16("AQL/"); });
        var buf = new ArrayBufferWriter<byte>();
        result.WriteAsBin(buf);
        Assert.Equal(new byte[] { 0x12, 0, 3, 0, 0, 0, 1, 2, 255 }, buf.WrittenSpan.ToArray());
    }

    [Fact]
    public void NestedAndGenericTypeNames_Resolve()
    {
        Assert.Equal("nested", Static(Fixtures + ".Nested", "Where", null, 0).PrimitiveValue());
        var list = Construct("System.Collections.Generic.List`1[System.String]");
        Assert.IsType<List<string>>(Bridge.s_handles[list]);
        var dict = Construct("System.Collections.Generic.Dictionary`2[System.String,DotNetBridgeTests.FieldHolder]");
        Assert.IsType<Dictionary<string, FieldHolder>>(Bridge.s_handles[dict]);
    }

    [Fact]
    public void CallNames_Parse()
    {
        var plain = Bridge.CallName.Parse("Add");
        Assert.False(plain.WantsByRef);
        Assert.Null(plain.TypeArgs);
        var full = Bridge.CallName.Parse("&Convert<System.Collections.Generic.List`1[System.Int32],System.String>#a,b");
        Assert.True(full.WantsByRef);
        Assert.Equal("Convert", full.Name);
        Assert.Equal(new[] { "System.Collections.Generic.List`1[System.Int32]", "System.String" }, full.TypeArgs);
        Assert.Equal(new[] { "a", "b" }, full.Named);
    }

    private static object?[] Items(DispatchResult r) => ((System.Collections.IEnumerable)r.PrimitiveValue()!).Cast<object?>().ToArray();

    private static void Bytes(BinWriter w, byte kind, byte[] data)
    {
        w.WriteByte(0x12);
        w.WriteByte(kind);
        w.WriteU32((uint)data.Length);
        w.WriteBytes(data);
    }

    private static DispatchResult Static(string type, string method, Action<BinWriter>? args, int argCount = 1)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        w.WriteByte(0x02);
        w.WriteString16(type);
        w.WriteString16("");
        w.WriteString16(method);
        w.WriteByte((byte)argCount);
        args?.Invoke(w);
        var r = new BinReader(buf.WrittenSpan);
        return Bridge.DispatchBin(ref r);
    }

    private static int Construct(string type)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        w.WriteByte(0x03);
        w.WriteString16(type);
        w.WriteString16("");
        w.WriteByte(0);
        var r = new BinReader(buf.WrittenSpan);
        return Bridge.DispatchBin(ref r).HandleId();
    }

    private static DispatchResult Members(string type)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        w.WriteByte(0x06);
        w.WriteString16(type);
        w.WriteString16("");
        var r = new BinReader(buf.WrittenSpan);
        return Bridge.DispatchBin(ref r);
    }

    private static DispatchResult Instance(int handle, string method, Action<BinWriter>? args, int argCount = 1)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        w.WriteByte(0x01);
        w.WriteI32(handle);
        w.WriteString16(method);
        w.WriteByte((byte)argCount);
        args?.Invoke(w);
        var r = new BinReader(buf.WrittenSpan);
        return Bridge.DispatchBin(ref r);
    }

    private static void Release(int handle, int? count)
    {
        var buf = new ArrayBufferWriter<byte>(16);
        var w = new BinWriter(buf);
        w.WriteByte(0x04);
        w.WriteI32(handle);
        if (count is { } n) w.WriteI32(n);
        var r = new BinReader(buf.WrittenSpan);
        Bridge.DispatchBin(ref r);
    }
}
