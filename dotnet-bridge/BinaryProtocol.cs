using System;
using System.Buffers;
using System.Buffers.Binary;
using System.Text;

namespace NativeScriptBridge;

internal readonly struct HandleRef(int id)
{
    public readonly int Id = id;
}

// Carries a raw IUnknown/IInspectable pointer from a WinRT proxy (tag 0x0A).
// CoerceBin calls Marshal.GetObjectForIUnknown to create a managed RCW.
internal readonly struct WinRtRef(long ptr)
{
    public readonly long Ptr = ptr;
}

// A JS function passed where a delegate is expected (tag 0x0C): the runtime's callback id, wrapped
// in a delegate of the parameter's type when the call's arguments are coerced.
internal readonly struct JsFunctionRef(int id)
{
    public readonly int Id = id;
}

// A plain JS object (tag 0x0D) as JSON, deserialized into the struct/record/class the parameter or
// return type expects (e.g. `{ Width: 120, Height: 40 }` for Windows.Foundation.Size).
internal sealed class JsJsonValue(string json)
{
    public readonly string Json = json;
}

// A JS array (tag 0x07): its items, each read like an argument, converted to the array or
// collection type the parameter expects.
internal sealed class JsArrayValue(object?[] items)
{
    public readonly object?[] Items = items;
}

internal ref struct BinReader(ReadOnlySpan<byte> buf)
{
    private readonly ReadOnlySpan<byte> _buf = buf;
    private int _pos = 0;

    public byte ReadByte() => _buf[_pos++];

    public bool HasMore => _pos < _buf.Length;

    public int ReadI32()
    {
        var v = BinaryPrimitives.ReadInt32LittleEndian(_buf[_pos..]);
        _pos += 4;
        return v;
    }

    public long ReadI64()
    {
        var v = BinaryPrimitives.ReadInt64LittleEndian(_buf[_pos..]);
        _pos += 8;
        return v;
    }

    public ushort ReadU16()
    {
        var v = BinaryPrimitives.ReadUInt16LittleEndian(_buf[_pos..]);
        _pos += 2;
        return v;
    }

    public double ReadF64()
    {
        var v = BinaryPrimitives.ReadDoubleLittleEndian(_buf[_pos..]);
        _pos += 8;
        return v;
    }

    public string ReadString16()
    {
        var len = ReadU16();
        var s   = Encoding.UTF8.GetString(_buf.Slice(_pos, len));
        _pos += len;
        return s;
    }

    // A type, assembly or member name: the same few names arrive on every call, so they come from
    // NameCache instead of being decoded (and allocated) again.
    public string ReadName16()
    {
        var len = ReadU16();
        var s   = NameCache.Get(_buf.Slice(_pos, len));
        _pos += len;
        return s;
    }

    public string ReadString32()
    {
        var len = BinaryPrimitives.ReadUInt32LittleEndian(_buf.Slice(_pos, 4));
        _pos += 4;
        var s = Encoding.UTF8.GetString(_buf.Slice(_pos, (int)len));
        _pos += (int)len;
        return s;
    }

    public uint ReadU32()
    {
        var v = BinaryPrimitives.ReadUInt32LittleEndian(_buf.Slice(_pos, 4));
        _pos += 4;
        return v;
    }

    public object?[] ReadArgs()
    {
        var count = ReadByte();
        if (count == 0) return [];
        var args = new object?[count];
        for (int i = 0; i < count; i++)
            args[i] = ReadArg();
        return args;
    }

    private object? ReadArg()
    {
        var tag = ReadByte();
        switch (tag)
        {
            case 0x00: return null;
            case 0x01: return false;
            case 0x02: return true;
            case 0x03: return ReadI32();
            case 0x04: return ReadF64();
            case 0x05: return ReadString16();
            case 0x06: return new HandleRef(ReadI32());
            case 0x07:
            {
                var items = new object?[ReadU32()];
                for (int i = 0; i < items.Length; i++) items[i] = ReadArg();
                return new JsArrayValue(items);
            }
            case 0x0A: return new WinRtRef(ReadI64());
            case 0x0C: return new JsFunctionRef(ReadI32());
            case 0x0D: return new JsJsonValue(ReadString32());
            default: return null;
        }
    }
}

// Decoded names by their UTF-8 bytes. Per thread and direct-mapped: a lookup takes no lock and a
// collision just decodes the name again. Only names land here, never argument values, so unlike
// string.Intern it can't grow without bound.
internal static class NameCache
{
    private const int Size = 512;

    [ThreadStatic]
    private static Entry[]? t_entries;

    private sealed class Entry(byte[] utf8, string value)
    {
        public readonly byte[] Utf8 = utf8;
        public readonly string Value = value;
    }

    public static string Get(ReadOnlySpan<byte> utf8)
    {
        if (utf8.IsEmpty) return string.Empty;
        var entries = t_entries ??= new Entry[Size];
        var hash = new HashCode();
        hash.AddBytes(utf8);
        ref var slot = ref entries[hash.ToHashCode() & (Size - 1)];
        var entry = slot;
        if (entry is not null && utf8.SequenceEqual(entry.Utf8)) return entry.Value;
        var value = Encoding.UTF8.GetString(utf8);
        slot = new Entry(utf8.ToArray(), value);
        return value;
    }
}

internal ref struct BinWriter(ArrayBufferWriter<byte> buf)
{
    private readonly ArrayBufferWriter<byte> _buf = buf;

    public void WriteByte(byte b)
    {
        _buf.GetSpan(1)[0] = b;
        _buf.Advance(1);
    }

    public void WriteI32(int v)
    {
        BinaryPrimitives.WriteInt32LittleEndian(_buf.GetSpan(4), v);
        _buf.Advance(4);
    }

    public void WriteU16(ushort v)
    {
        BinaryPrimitives.WriteUInt16LittleEndian(_buf.GetSpan(2), v);
        _buf.Advance(2);
    }

    public void WriteU32(uint v)
    {
        BinaryPrimitives.WriteUInt32LittleEndian(_buf.GetSpan(4), v);
        _buf.Advance(4);
    }

    public void WriteI64(long v)
    {
        BinaryPrimitives.WriteInt64LittleEndian(_buf.GetSpan(8), v);
        _buf.Advance(8);
    }

    public void WriteF64(double v)
    {
        BinaryPrimitives.WriteDoubleLittleEndian(_buf.GetSpan(8), v);
        _buf.Advance(8);
    }

    public void WriteString32(ReadOnlySpan<char> s)
    {
        var byteCount = Encoding.UTF8.GetByteCount(s);
        WriteU32((uint)byteCount);
        Encoding.UTF8.GetBytes(s, _buf.GetSpan(byteCount));
        _buf.Advance(byteCount);
    }

    public void WriteString16(ReadOnlySpan<char> s)
    {
        var byteCount = Encoding.UTF8.GetByteCount(s);
        WriteU16((ushort)byteCount);
        Encoding.UTF8.GetBytes(s, _buf.GetSpan(byteCount));
        _buf.Advance(byteCount);
    }
}
