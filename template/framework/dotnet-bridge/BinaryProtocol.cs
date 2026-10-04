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
        // Intern so repeated method/type names reuse the same heap string.
        // Eliminates the allocation on every subsequent warm-path call.
        return string.Intern(s);
    }

    public string ReadString32()
    {
        var len = BinaryPrimitives.ReadUInt32LittleEndian(_buf.Slice(_pos, 4));
        _pos += 4;
        var s = Encoding.UTF8.GetString(_buf.Slice(_pos, (int)len));
        _pos += (int)len;
        return string.Intern(s);
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
        {
            var tag = ReadByte();
            args[i] = tag switch
            {
                0x00 => null,
                0x01 => (object)false,
                0x02 => (object)true,
                0x03 => (object)ReadI32(),
                0x04 => (object)ReadF64(),
                0x05 => (object)ReadString16(),
                0x06 => (object)new HandleRef(ReadI32()),
                0x0A => (object)new WinRtRef(ReadI64()),
                0x0C => (object)new JsFunctionRef(ReadI32()),
                0x0D => (object)new JsJsonValue(ReadString32()),
                _    => null,
            };
        }
        return args;
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
