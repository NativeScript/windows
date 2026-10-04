using System;
using System.Buffers;
using System.Buffers.Binary;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading.Tasks;
using NativeScriptBridge;
using Xunit;

namespace DotNetBridgeTests;

// Bridge side of extending .NET classes from JS (class X extends SomeType / SomeType.extend()):
// constructor arguments, protected constructors, abstract members, base calls for `super`,
// identity of JS-backed instances, results converted to the member's return type, and JS errors
// surfacing as JsException. The JS half is covered by test-app/app/tests/extend-dotnet.js.
[Collection("Bridge")]
public sealed class JsSubclassTests : IDisposable
{
    private static readonly List<(string Member, object?[] Args)> s_calls = new();
    private static Func<string, object?[], object?>? s_handler;

    [UnmanagedCallersOnly(CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    private static unsafe void Invoker(int id, byte* argsPtr, int argsLen, byte** respPtr, int* respLen)
    {
        var (member, args) = DecodeCall(new ReadOnlySpan<byte>(argsPtr, argsLen));
        s_calls.Add((member, args));
        object? result;
        try { result = s_handler?.Invoke(member, args); }
        catch (Exception e) { result = new JsError(e.Message); }
        WriteResponse(result, respPtr, respLen);
    }

    private sealed record JsError(string Message);

    public unsafe JsSubclassTests()
    {
        Bridge.ClearCaches();
        s_calls.Clear();
        s_handler = null;
        s_onDispatch = null;
        Bridge.s_jsInvoker = &Invoker;
    }

    public unsafe void Dispose()
    {
        Bridge.s_jsInvoker = null;
        Bridge.ClearCaches();
    }

    [Fact]
    public void ConstructorArguments_SelectAndFeedTheBaseConstructor()
    {
        var one = CreateSubclass(typeof(Pet).FullName!, [], ["Speak"], 1, "rex");
        var two = CreateSubclass(typeof(Pet).FullName!, [], ["Speak"], 2, "fido", 12);
        var pet1 = Assert.IsAssignableFrom<Pet>(Instance(one));
        var pet2 = Assert.IsAssignableFrom<Pet>(Instance(two));
        Assert.Equal("rex", pet1.Name);
        Assert.Equal(0, pet1.Age);
        Assert.Equal("fido", pet2.Name);
        Assert.Equal(12, pet2.Age);
    }

    [Fact]
    public void ConstructorArguments_NoMatchingConstructor_Throws()
    {
        Assert.ThrowsAny<Exception>(() => CreateSubclass(typeof(Pet).FullName!, [], [], 3, "a", 1, "extra"));
    }

    [Fact]
    public void VirtualCalledByBaseConstructor_ReachesJs()
    {
        s_handler = (member, _) => member == "Speak" ? "js-speak" : null;
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["Speak"], 4, "rex");
        var pet = Assert.IsAssignableFrom<Pet>(Instance(handle));
        Assert.Equal("js-speak", pet.SpokeInConstructor);
    }

    [Fact]
    public void AbstractBase_ProtectedConstructorAndAbstractMember()
    {
        s_handler = (member, _) => member == "Area" ? 4.0 : null;
        var handle = CreateSubclass(typeof(AbstractShape).FullName!, [], ["Area"], 5);
        var shape = Assert.IsAssignableFrom<AbstractShape>(Instance(handle));
        Assert.Equal("area=4", shape.Report());
    }

    [Fact]
    public void AbstractMember_NotImplementedInJs_StillCreatesTheType()
    {
        s_handler = (member, _) => throw new InvalidOperationException(member + " is not implemented");
        var handle = CreateSubclass(typeof(AbstractShape).FullName!, [], [], 6);
        var shape = Assert.IsAssignableFrom<AbstractShape>(Instance(handle));
        var error = Assert.Throws<JsException>(() => shape.Area());
        Assert.Contains("Area is not implemented", error.Message);
    }

    [Fact]
    public void BaseCall_RunsTheBaseImplementationOfAnOverriddenMember()
    {
        s_handler = (member, args) => member == "Greet" ? "js:" + args[0] : null;
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["Greet"], 7, "rex");

        Assert.Equal("js:bob", Invoke(handle, "Greet", "bob").PrimitiveValue());
        Assert.Equal("rex greets bob", Invoke(handle, Bridge.BaseCallPrefix + "Greet", "bob").PrimitiveValue());
    }

    [Fact]
    public void BaseCall_PropertyGetter()
    {
        s_handler = (member, _) => member == "get_Legs" ? 2 : null;
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["get_Legs"], 8, "tweety");
        var pet = Assert.IsAssignableFrom<Pet>(Instance(handle));
        Assert.Equal(2, pet.Legs);
        Assert.Equal(4, Invoke(handle, Bridge.BaseCallPrefix + "get_Legs").PrimitiveValue());
    }

    [Fact]
    public void JsBackedInstance_BoxesToItsOriginalHandle()
    {
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["Speak"], 9, "rex");
        var keep = InvokeStatic(typeof(PetStore).FullName!, "Keep", new HandleArg(handle));
        var back = InvokeStatic(typeof(PetStore).FullName!, "Kept");
        Assert.Equal(DispatchKind.Handle, back.Kind());
        Assert.Equal(handle, back.HandleId());
        _ = keep;
    }

    [Fact]
    public void OverrideResult_IsConvertedToTheReturnType()
    {
        // JS numbers arrive as double; the override returns int.
        s_handler = (member, _) => member == "get_Legs" ? 6.0 : null;
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["get_Legs"], 10, "ant");
        Assert.Equal(6, Assert.IsAssignableFrom<Pet>(Instance(handle)).Legs);
    }

    [Fact]
    public void StructArgumentsAndResults_TravelAsJson()
    {
        s_handler = (member, args) =>
        {
            if (member != "Measure") return null;
            var json = (string)args[0]!;
            Assert.Contains("\"Width\":10", json);
            return new JsJson("{\"width\":20,\"height\":5}");
        };
        var handle = CreateSubclass(typeof(Measurer).FullName!, [], ["Measure"], 11);
        var measurer = Assert.IsAssignableFrom<Measurer>(Instance(handle));
        var size = measurer.Measure(new Extent { Width = 10, Height = 3 });
        Assert.Equal(20, size.Width);
        Assert.Equal(5, size.Height);
    }

    [Fact]
    public void JsException_InAMemberWithAResult_SurfacesAsJsException()
    {
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["Speak"], 12, "rex");
        var pet = Assert.IsAssignableFrom<Pet>(Instance(handle));
        s_handler = (_, _) => throw new InvalidOperationException("boom");
        var error = Assert.Throws<JsException>(() => pet.Speak());
        Assert.Equal("boom", error.Message);
    }

    [Fact]
    public void InterfaceAsBase_ImplementsTheInterfaceOnSystemObject()
    {
        s_handler = (member, args) => member == "Compute" ? (int)args[0]! * (int)args[1]! : member == "get_Name" ? "mul" : null;
        var handle = CreateSubclass(typeof(ICalc).FullName!, [], ["Compute", "get_Name"], 13);
        var calc = Assert.IsAssignableFrom<ICalc>(Instance(handle));
        Assert.Equal(42, calc.Compute(6, 7));
        Assert.Equal("mul", calc.Name);
    }

    [Fact]
    public void TypeKind_DescribesTypes()
    {
        Assert.Equal("interface", Bridge.TypeKind(typeof(ICalc).FullName!));
        Assert.Equal("abstract", Bridge.TypeKind(typeof(AbstractShape).FullName!));
        Assert.Equal("class", Bridge.TypeKind(typeof(Pet).FullName!));
        Assert.Equal("sealed", Bridge.TypeKind(typeof(string).FullName!));
        Assert.Equal("", Bridge.TypeKind("No.Such.Type"));
    }

    [Fact]
    public void IsInstanceOf_UsesTheRuntimeType()
    {
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["Speak"], 14, "rex");
        Assert.True(Bridge.IsInstanceOf(Instance(handle), typeof(Pet).FullName!));
        Assert.False(Bridge.IsInstanceOf(Instance(handle), typeof(AbstractShape).FullName!));
        Assert.False(Bridge.IsInstanceOf(null, typeof(Pet).FullName!));
    }

    [Fact]
    public void TaskResults_AreReturnedAsAwaitableHandles_WithoutBlocking()
    {
        var res = InvokeStatic(typeof(PetStore).FullName!, "SlowAsync", 5000);
        Assert.Equal(DispatchKind.TaskHandle, res.Kind());
        Assert.True(Bridge.s_handles.TryGetValue(res.HandleId(), out var task));
        Assert.False(((Task)task!).IsCompleted);
    }

    private static readonly List<int> s_released = new();

    [UnmanagedCallersOnly(CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    private static void RecordRelease(int callbackId)
    {
        lock (s_released) s_released.Add(callbackId);
    }

    [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)]
    private static void CreateAndDrop(int callbackId)
    {
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["Speak"], callbackId, "temp");
        Release(handle);
    }

    [Fact]
    public unsafe void CollectedInstance_ReleasesItsJsDispatcher()
    {
        var previous = Bridge.s_jsCallbackRelease;
        Bridge.s_jsCallbackRelease = &RecordRelease;
        try
        {
            CreateAndDrop(31);
            for (int i = 0; i < 5; i++)
            {
                GC.Collect();
                GC.WaitForPendingFinalizers();
                lock (s_released) if (s_released.Contains(31)) break;
            }
            lock (s_released) Assert.Contains(31, s_released);
        }
        finally
        {
            Bridge.s_jsCallbackRelease = previous;
        }
    }

    [Fact]
    public void ReleasedHandle_InstanceGetsANewHandle_UsedForDispatch()
    {
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["Speak"], 32, "rex");
        InvokeStatic(typeof(PetStore).FullName!, "Keep", new HandleArg(handle));
        Release(handle);

        var back = InvokeStatic(typeof(PetStore).FullName!, "Kept");
        Assert.Equal(DispatchKind.Handle, back.Kind());
        Assert.NotEqual(handle, back.HandleId());

        int dispatchedHandle = -1;
        s_handler = (member, _) => member == "Speak" ? "js" : null;
        s_onDispatch = h => dispatchedHandle = h;
        Assert.Equal("js", Invoke(back.HandleId(), "Speak").PrimitiveValue());
        Assert.Equal(back.HandleId(), dispatchedHandle);
    }

    [Fact]
    public void JsSelfMember_AsksTheDispatcherForTheJsObject()
    {
        var handle = CreateSubclass(typeof(Pet).FullName!, [], ["Speak"], 33, "rex");
        s_calls.Clear();
        Invoke(handle, Bridge.JsSelfMember);
        Assert.Contains(s_calls, c => c.Member == Bridge.JsSelfMember);
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    private static Action<int>? s_onDispatch;

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

    private sealed record HandleArg(int Handle);
    private sealed record JsJson(string Json);

    private static object? Instance(int handle)
    {
        Assert.True(Bridge.s_handles.TryGetValue(handle, out var instance));
        return instance;
    }

    private static int CreateSubclass(string typeName, string[] interfaceNames, string[] memberNames, int callbackId, params object[] ctorArgs)
    {
        var pkt = BuildPacket(w =>
        {
            w.WriteByte(0x0A);
            w.WriteString16("");
            w.WriteString16(typeName);
            w.WriteI32(interfaceNames.Length);
            foreach (var n in interfaceNames) w.WriteString16(n);
            w.WriteI32(memberNames.Length);
            foreach (var n in memberNames) w.WriteString16(n);
            w.WriteI32(callbackId);
            WriteArgs(w, ctorArgs);
        });
        var r = new BinReader(pkt.AsSpan());
        var res = Bridge.DispatchBin(ref r);
        Assert.Equal(DispatchKind.Handle, res.Kind());
        return res.HandleId();
    }

    private static DispatchResult Invoke(int handle, string method, params object[] args)
    {
        var pkt = BuildPacket(w =>
        {
            w.WriteByte(0x01);
            w.WriteI32(handle);
            w.WriteString16(method);
            WriteArgs(w, args);
        });
        var r = new BinReader(pkt.AsSpan());
        return Bridge.DispatchBin(ref r);
    }

    private static DispatchResult InvokeStatic(string typeName, string method, params object[] args)
    {
        var pkt = BuildPacket(w =>
        {
            w.WriteByte(0x02);
            w.WriteString16(typeName);
            w.WriteString16("");
            w.WriteString16(method);
            WriteArgs(w, args);
        });
        var r = new BinReader(pkt.AsSpan());
        return Bridge.DispatchBin(ref r);
    }

    private static void WriteArgs(BinWriter w, object[] args)
    {
        w.WriteByte((byte)args.Length);
        foreach (var a in args)
        {
            switch (a)
            {
                case string s: w.WriteByte(0x05); w.WriteString16(s); break;
                case int i: w.WriteByte(0x03); w.WriteI32(i); break;
                case HandleArg h: w.WriteByte(0x06); w.WriteI32(h.Handle); break;
                default: throw new NotSupportedException(a.GetType().Name);
            }
        }
    }

    private static byte[] BuildPacket(Action<BinWriter> build)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        build(w);
        return buf.WrittenSpan.ToArray();
    }

    private static (string Member, object?[] Args) DecodeCall(ReadOnlySpan<byte> span)
    {
        var r = new BinReader(span);
        var count = r.ReadByte();
        // instance handle
        Assert.Equal(0x06, r.ReadByte());
        var selfHandle = r.ReadI32();
        s_onDispatch?.Invoke(selfHandle);
        r.ReadString16();
        if (r.ReadByte() == 1) r.ReadI64();
        Assert.Equal(0x05, r.ReadByte());
        var member = r.ReadString32();
        var args = new object?[count - 2];
        for (int i = 0; i < args.Length; i++)
        {
            var tag = r.ReadByte();
            args[i] = tag switch
            {
                0x00 => null,
                0x03 => r.ReadI32(),
                0x04 => r.ReadF64(),
                0x05 => r.ReadString32(),
                0x0D => r.ReadString32(),
                _ => throw new InvalidOperationException($"unexpected tag 0x{tag:X2}"),
            };
        }
        return (member, args);
    }

    private static unsafe void WriteResponse(object? result, byte** respPtr, int* respLen)
    {
        byte[] bytes;
        switch (result)
        {
            case null:
                *respPtr = null;
                *respLen = 0;
                return;
            case string s:
                bytes = Tagged(0x05, Encoding.UTF8.GetBytes(s));
                break;
            case JsJson j:
                bytes = Tagged(0x0D, Encoding.UTF8.GetBytes(j.Json));
                break;
            case JsError e:
                bytes = Tagged(0x0F, Encoding.UTF8.GetBytes(e.Message));
                break;
            case int i:
                bytes = new byte[5];
                bytes[0] = 0x03;
                BinaryPrimitives.WriteInt32LittleEndian(bytes.AsSpan(1), i);
                break;
            case double d:
                bytes = new byte[9];
                bytes[0] = 0x04;
                BinaryPrimitives.WriteDoubleLittleEndian(bytes.AsSpan(1), d);
                break;
            default:
                throw new NotSupportedException(result.GetType().Name);
        }
        var mem = (byte*)Marshal.AllocHGlobal(bytes.Length);
        bytes.CopyTo(new Span<byte>(mem, bytes.Length));
        *respPtr = mem;
        *respLen = bytes.Length;
    }

    private static byte[] Tagged(byte tag, byte[] utf8)
    {
        var bytes = new byte[5 + utf8.Length];
        bytes[0] = tag;
        BinaryPrimitives.WriteUInt32LittleEndian(bytes.AsSpan(1), (uint)utf8.Length);
        utf8.CopyTo(bytes, 5);
        return bytes;
    }
}

public class Pet
{
    public Pet(string name) : this(name, 0) { }

    public Pet(string name, int age)
    {
        Name = name;
        Age = age;
        SpokeInConstructor = Speak();
    }

    public string Name { get; }

    public int Age { get; }

    public string SpokeInConstructor { get; }

    public virtual string Speak() => "...";

    public virtual int Legs => 4;

    public virtual string Greet(string other) => $"{Name} greets {other}";
}

public abstract class AbstractShape
{
    protected AbstractShape() { }

    public abstract double Area();

    public string Report() => $"area={Area()}";
}

public interface ICalc
{
    int Compute(int a, int b);

    string Name { get; }
}

public struct Extent
{
    public double Width;
    public double Height;
}

public class Measurer
{
    public virtual Extent Measure(Extent available) => available;
}

public static class PetStore
{
    private static object? s_kept;

    public static void Keep(object value) => s_kept = value;

    public static object? Kept() => s_kept;

    public static async Task<int> SlowAsync(int ms)
    {
        await Task.Delay(ms);
        return 1;
    }
}
