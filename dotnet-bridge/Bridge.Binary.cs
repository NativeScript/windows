using System;
using System.Buffers;
using System.IO;
using System.Runtime.InteropServices;

namespace NativeScriptBridge;

// A JS typed array, ArrayBuffer or DataView (tag 0x12). Kinds: 0 bytes (Uint8Array,
// Uint8ClampedArray, ArrayBuffer, DataView), 1 Int8, 2 Int16, 3 Uint16, 4 Int32, 5 Uint32,
// 6 Float32, 7 Float64, 8 BigInt64, 9 BigUint64.
internal sealed class JsTypedArray(byte kind, byte[] bytes)
{
    public readonly byte Kind = kind;
    public readonly byte[] Bytes = bytes;

    public Type ElementType => ElementTypeOf(Kind);

    public static Type ElementTypeOf(byte kind) => kind switch
    {
        1 => typeof(sbyte),
        2 => typeof(short),
        3 => typeof(ushort),
        4 => typeof(int),
        5 => typeof(uint),
        6 => typeof(float),
        7 => typeof(double),
        8 => typeof(long),
        9 => typeof(ulong),
        _ => typeof(byte),
    };

    // The typed-array kind for a .NET array element type, or -1 when JS has none.
    public static int KindOf(Type element) =>
        element == typeof(byte) ? 0 : element == typeof(sbyte) ? 1 : element == typeof(short) ? 2
        : element == typeof(ushort) ? 3 : element == typeof(int) ? 4 : element == typeof(uint) ? 5
        : element == typeof(float) ? 6 : element == typeof(double) ? 7 : element == typeof(long) ? 8
        : element == typeof(ulong) ? 9 : -1;

    // The items as an array of their own element type.
    public Array ToNativeArray()
    {
        var element = ElementType;
        if (element == typeof(byte)) return Bytes;
        var size = Marshal.SizeOf(element);
        var array = Array.CreateInstance(element, Bytes.Length / size);
        Buffer.BlockCopy(Bytes, 0, array, 0, array.Length * size);
        return array;
    }

    // The items as an array of `element`, converted when the types differ.
    public Array ToArray(Type element)
    {
        var native = ToNativeArray();
        if (native.GetType().GetElementType() == element) return native;
        var array = Array.CreateInstance(element, native.Length);
        for (int i = 0; i < native.Length; i++)
            array.SetValue(Convert.ChangeType(native.GetValue(i), element), i);
        return array;
    }
}

public static partial class Bridge
{
    private const double MaxSafeInteger = 9007199254740991;

    // How well a typed array fits a parameter type (see MatchScore).
    private static int TypedArrayScore(JsTypedArray t, Type p)
    {
        var element = t.ElementType;
        if (p.IsArray)
        {
            var pe = p.GetElementType()!;
            if (pe == element) return 10;
            if (pe == typeof(byte)) return 4;
            return JsTypedArray.KindOf(pe) >= 0 ? 5 : pe == typeof(object) ? 2 : -1;
        }
        if (p.IsGenericType)
        {
            var definition = p.GetGenericTypeDefinition();
            var arg = p.GetGenericArguments()[0];
            if (definition == typeof(Memory<>) || definition == typeof(ReadOnlyMemory<>) || definition == typeof(ArraySegment<>))
                return arg == element ? 9 : arg == typeof(byte) ? 4 : -1;
        }
        if (element == typeof(byte) && p.IsAssignableFrom(typeof(MemoryStream))) return p == typeof(object) ? 1 : 5;
        if (p.IsAssignableFrom(element.MakeArrayType())) return p == typeof(object) ? 1 : 7;
        return -1;
    }

    private static object? CoerceTypedArray(JsTypedArray t, Type targetType)
    {
        var p = Nullable.GetUnderlyingType(targetType) ?? targetType;
        if (p.IsArray)
        {
            var pe = p.GetElementType()!;
            return pe == typeof(byte) ? t.Bytes : pe == typeof(object) ? ToObjects(t.ToNativeArray()) : t.ToArray(pe);
        }
        if (p.IsGenericType)
        {
            var definition = p.GetGenericTypeDefinition();
            if (definition == typeof(Memory<>) || definition == typeof(ReadOnlyMemory<>) || definition == typeof(ArraySegment<>))
            {
                var arg = p.GetGenericArguments()[0];
                return Activator.CreateInstance(p, arg == typeof(byte) ? t.Bytes : t.ToArray(arg));
            }
        }
        if (t.ElementType == typeof(byte) && p.IsAssignableFrom(typeof(MemoryStream)) && p != typeof(object))
            return new MemoryStream(t.Bytes, writable: true);
        return t.ToNativeArray();
    }

    private static object?[] ToObjects(Array array)
    {
        var objects = new object?[array.Length];
        for (int i = 0; i < objects.Length; i++) objects[i] = array.GetValue(i);
        return objects;
    }

    // Values JS gets as a typed array: byte[], sbyte[], Memory<byte>, ArraySegment<byte>.
    internal static bool TryGetBytes(object value, out ReadOnlySpan<byte> bytes, out byte kind)
    {
        switch (value)
        {
            case byte[] b: bytes = b; kind = 0; return true;
            case sbyte[] sb: bytes = MemoryMarshal.AsBytes(sb.AsSpan()); kind = 1; return true;
            case Memory<byte> m: bytes = m.Span; kind = 0; return true;
            case ReadOnlyMemory<byte> rm: bytes = rm.Span; kind = 0; return true;
            case ArraySegment<byte> seg: bytes = seg.AsSpan(); kind = 0; return true;
        }
        bytes = default;
        kind = 0;
        return false;
    }

    internal static void WriteTypedArray(ref BinWriter w, ReadOnlySpan<byte> bytes, byte kind)
    {
        w.WriteByte(0x12);
        w.WriteByte(kind);
        w.WriteU32((uint)bytes.Length);
        w.WriteBytes(bytes);
    }

    // A number when the value fits a double exactly, else a BigInt (tags 0x10 / 0x11).
    internal static void WriteInt64(ref BinWriter w, long v)
    {
        if (v >= -MaxSafeInteger && v <= MaxSafeInteger) { w.WriteByte(0x04); w.WriteF64(v); }
        else { w.WriteByte(0x10); w.WriteI64(v); }
    }

    internal static void WriteUInt64(ref BinWriter w, ulong v)
    {
        if (v <= (ulong)MaxSafeInteger) { w.WriteByte(0x04); w.WriteF64(v); }
        else { w.WriteByte(0x11); w.WriteI64(unchecked((long)v)); }
    }
}
