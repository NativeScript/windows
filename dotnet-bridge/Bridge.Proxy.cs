using System;
using System.Buffers;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using System.Reflection.Emit;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Threading;
using System.Globalization;

namespace NativeScriptBridge;

public static partial class Bridge
{
    // The types behind JS subclasses, emitted at runtime. Keyed by base type PLUS the exact set of
    // interfaces/members a JS class asked for, so different JS subclasses of the same base type
    // (different override sets) get distinct emitted types.
    private readonly record struct ProxyTypeKey(Type BaseType, string InterfaceKey, string MemberKey);
    private static readonly ConcurrentDictionary<ProxyTypeKey, Type> s_dynamicProxyCache = new();
    private static int s_dynamicProxyTypeCounter = 0;
    private static readonly AssemblyBuilder? s_dynamicAssembly = CreateDynamicAssembly();
    private static readonly ModuleBuilder? s_dynamicModule   = CreateDynamicModule(s_dynamicAssembly);

    private static AssemblyBuilder? CreateDynamicAssembly()
    {
        try
        {
            var an = new AssemblyName("NSWinRTDynamicProxies");
            var asm = AssemblyBuilder.DefineDynamicAssembly(an, AssemblyBuilderAccess.Run);
            
            return asm;
        }
        catch { return null; }
    }

    private static ModuleBuilder? CreateDynamicModule(AssemblyBuilder? asm)
    {
            try {
                var m = asm?.DefineDynamicModule("NSWinRTDynamicProxiesModule");
                return m;
            } catch { return null; }
    }

    // Public helpers callable from runtime-generated types. Nested type so it can
    // access Bridge's private proxy helpers (ProxyInvokeMethod, etc.) while
    // exposing a stable public API for emitted proxies.
    public static class ProxyRuntime
    {
        public static T InvokeMethodTyped<T>(object instance, string methodName, object[] args)
        {
            var result = ProxyInvokeMethod(instance, methodName, args);
            return (T)ConvertJsResult(result, typeof(T))!;
        }

        public static object? InvokeMethod(object instance, string methodName, object[] args)
        {
            return ProxyInvokeMethod(instance, methodName, args);
        }

        public static void InvokeVoid(object instance, string methodName, object[] args)
        {
            ProxyInvokeVoid(instance, methodName, args);
        }

        public static T GetProperty<T>(object instance, string propertyName)
        {
            var result = ProxyGetProperty(instance, propertyName);
            return (T)ConvertJsResult(result, typeof(T))!;
        }

        public static void SetProperty(object instance, string propertyName, object? value)
        {
            ProxySetProperty(instance, propertyName, value);
        }
    }
    // Ties a JS-backed instance to its JS dispatcher (and the handle its JS object holds). Lives as
    // long as the instance (ConditionalWeakTable value); when the instance is collected the runtime
    // is told to unpin the dispatcher.
    private sealed class ProxyCallbackHolder(int callbackId)
    {
        public readonly int CallbackId = callbackId;
        public int HandleId;

        ~ProxyCallbackHolder() => ReleaseJsCallback(CallbackId);
    }

    private static readonly ConditionalWeakTable<object, ProxyCallbackHolder> s_proxyCallbacks = new();
    private static readonly ThreadLocal<int?> s_pendingProxyCallbackId = new(() => null);

    private static Type GetOrCreateDynamicProxyType(Type baseType, string[] interfaceNames, string[] memberNames)
    {
        var interfaceKey = string.Join(",", interfaceNames.OrderBy(n => n, StringComparer.Ordinal));
        var memberKey = string.Join(",", memberNames.OrderBy(n => n, StringComparer.Ordinal));
        var key = new ProxyTypeKey(baseType, interfaceKey, memberKey);
        return s_dynamicProxyCache.GetOrAdd(key, _ => CreateDynamicProxyType(baseType, interfaceNames, memberNames));
    }

    private static Type CreateDynamicProxyType(Type baseType, string[] interfaceNames, string[] memberNames)
    {

        if (s_dynamicModule == null)
            throw new InvalidOperationException("Dynamic proxy generation not available in this environment");

        if (baseType.IsSealed)
            throw new InvalidOperationException($"Cannot derive from sealed type {baseType.FullName}");

        var interfaceTypes = interfaceNames
            .Select(n => ResolveType(null, n) ?? throw new TypeLoadException($"Interface type not found: {n}"))
            .ToArray();

        // Members JS actually overrides. Only base virtuals in this set get an IL override
        // emitted — anything else is left alone so the real base implementation stays live in
        // the vtable (no "call base" IL needed, the base method was simply never touched).
        var memberSet = new HashSet<string>(memberNames, StringComparer.Ordinal);

        var safeName = (baseType.FullName ?? Guid.NewGuid().ToString()).Replace('.', '_');
        var uniqueSuffix = Interlocked.Increment(ref s_dynamicProxyTypeCounter);
        var typeName = $"NSWinRTDynamicProxies.{safeName}_JsProxy_{uniqueSuffix}";

        var tb = s_dynamicModule.DefineType(typeName, TypeAttributes.Public | TypeAttributes.Class, baseType);

        // Mirror the public and protected base constructors: a JS subclass can call either through
        // super(...), and abstract classes / composable WinRT classes (Panel, Control) only have
        // protected ones.
        var baseCtors = baseType.GetConstructors(BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Instance)
                        .Where(c => !c.IsGenericMethod && (c.IsPublic || c.IsFamily || c.IsFamilyOrAssembly))
                        .ToArray();

        foreach (var bc in baseCtors)
        {
            var parameters = bc.GetParameters();
            if (parameters.Any(p => p.ParameterType.IsByRef)) continue;

            // Define a matching ctor on the derived type that forwards args to base.
            var paramTypes = parameters.Select(p => p.ParameterType).ToArray();
            var derivedCtor = tb.DefineConstructor(MethodAttributes.Public, CallingConventions.Standard, paramTypes);
            var ilg = derivedCtor.GetILGenerator();
            // Load 'this'
            ilg.Emit(OpCodes.Ldarg_0);
            // Load each argument and forward
            for (int i = 0; i < paramTypes.Length; i++)
            {
                // ldarg_1..n
                switch (i + 1)
                {
                    case 1: ilg.Emit(OpCodes.Ldarg_1); break;
                    case 2: ilg.Emit(OpCodes.Ldarg_2); break;
                    case 3: ilg.Emit(OpCodes.Ldarg_3); break;
                    default: ilg.Emit(OpCodes.Ldarg, i + 1); break;
                }
            }
            ilg.Emit(OpCodes.Call, bc);
            ilg.Emit(OpCodes.Ret);
        }

        // Shared by both loops below, keyed by "Name(Param1FullName,Param2FullName,...)" so a
        // member that's both an overridden base virtual and a requested interface member (e.g.
        // the base class already implements that interface) reuses one emitted body instead of
        // tripping a "duplicate member" TypeBuilder error.
        var definedSignatures = new Dictionary<string, MethodBuilder>(StringComparer.Ordinal);
        static string SignatureKey(string name, Type[] paramTypes) =>
            name + "(" + string.Join(",", paramTypes.Select(p => p.FullName)) + ")";

        // Override only the base virtuals JS actually asked for (memberSet). Anything not in
        // that set is simply never touched, so the base implementation stays live in the vtable —
        // no "call base" IL needed. (Skip ref/out and generic methods — unsupported.)
        // Abstract members are always implemented (dispatching to JS, which reports a missing
        // implementation as an error) — a type with an unimplemented abstract member can't be created.
        var virtualMethods = baseType.GetMethods(BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Instance)
            .Where(m => m.IsVirtual && !m.IsFinal && !m.IsGenericMethod && !m.IsConstructor
                && (m.IsPublic || m.IsFamily || m.IsFamilyOrAssembly)
                && (memberSet.Contains(m.Name) || m.IsAbstract))
            .ToArray();

        foreach (var mi in virtualMethods)
        {
            var parameters = mi.GetParameters();
            if (parameters.Any(p => p.ParameterType.IsByRef)) continue;

            var paramTypes = parameters.Select(p => p.ParameterType).ToArray();
            var sigKey = SignatureKey(mi.Name, paramTypes);
            if (!definedSignatures.TryGetValue(sigKey, out var mb))
            {
                mb = EmitProxyMethodBody(tb, mi.Name,
                    MethodAttributes.Public | MethodAttributes.Virtual | MethodAttributes.HideBySig,
                    mi.CallingConvention, mi.ReturnType, paramTypes, mi.Name);
                definedSignatures[sigKey] = mb;

                // `super.Name(...)` from JS: a public, non-virtual entry point to the base
                // implementation (a plain call, so it can't dispatch back into the override).
                if (!mi.IsAbstract)
                    EmitBaseCallMethod(tb, mi, paramTypes);
            }

            tb.DefineMethodOverride(mb, mi);
        }

        // Interfaces: every requested interface (plus any interfaces *those* extend, since
        // AddInterfaceImplementation only registers the interface named, not its ancestors) must
        // have every member implemented — the CLR requires it, unlike base-class virtuals where
        // skipping is fine. Members JS doesn't override fall back to default/no-op via the same
        // generic dispatcher (ProxyRuntime.InvokeMethodTyped/InvokeVoid already do this).
        if (interfaceTypes.Length > 0)
        {
            var allInterfaceTypes = new List<Type>();
            void CollectInterfaces(Type t)
            {
                if (allInterfaceTypes.Contains(t)) return;
                allInterfaceTypes.Add(t);
                foreach (var parent in t.GetInterfaces()) CollectInterfaces(parent);
            }
            foreach (var ifaceType in interfaceTypes) CollectInterfaces(ifaceType);

            foreach (var ifaceType in allInterfaceTypes) tb.AddInterfaceImplementation(ifaceType);

            foreach (var ifaceType in allInterfaceTypes)
            {
                foreach (var mi in ifaceType.GetMethods())
                {
                    var parameters = mi.GetParameters();
                    if (parameters.Any(p => p.ParameterType.IsByRef)) continue;

                    var paramTypes = parameters.Select(p => p.ParameterType).ToArray();
                    var sigKey = SignatureKey(mi.Name, paramTypes);
                    if (!definedSignatures.TryGetValue(sigKey, out var mb))
                    {
                        mb = EmitProxyMethodBody(tb, mi.Name,
                            MethodAttributes.Public | MethodAttributes.Virtual | MethodAttributes.Final
                                | MethodAttributes.HideBySig | MethodAttributes.NewSlot,
                            mi.CallingConvention, mi.ReturnType, paramTypes, mi.Name);
                        definedSignatures[sigKey] = mb;
                    }

                    tb.DefineMethodOverride(mb, mi);
                }
            }
        }

        return tb.CreateTypeInfo()!.AsType();
    }

    internal const string BaseCallPrefix = "__ns_base_";
    internal const string JsSelfMember = "__ns_js_self";

    private static void EmitBaseCallMethod(TypeBuilder tb, MethodInfo baseMethod, Type[] paramTypes)
    {
        var mb = tb.DefineMethod(BaseCallPrefix + baseMethod.Name,
            MethodAttributes.Public | MethodAttributes.HideBySig, CallingConventions.HasThis,
            baseMethod.ReturnType, paramTypes);
        var il = mb.GetILGenerator();
        il.Emit(OpCodes.Ldarg_0);
        for (int i = 0; i < paramTypes.Length; i++) il.Emit(OpCodes.Ldarg, i + 1);
        il.Emit(OpCodes.Call, baseMethod);
        il.Emit(OpCodes.Ret);
    }

    // Shared IL body for both base-virtual overrides and interface-member implementations: box
    // args into an object[], forward to ProxyRuntime.InvokeVoid/InvokeMethodTyped<T> by name.
    private static MethodBuilder EmitProxyMethodBody(
        TypeBuilder tb, string emittedName, MethodAttributes attrs, CallingConventions callingConvention,
        Type returnType, Type[] paramTypes, string dispatchName)
    {
        var mb = tb.DefineMethod(emittedName, attrs, callingConvention, returnType, paramTypes);
        var ilg = mb.GetILGenerator();
        var argsLocal = ilg.DeclareLocal(typeof(object[]));
        ilg.Emit(OpCodes.Ldc_I4, paramTypes.Length);
        ilg.Emit(OpCodes.Newarr, typeof(object));
        ilg.Emit(OpCodes.Stloc, argsLocal);

        for (int i = 0; i < paramTypes.Length; i++)
        {
            ilg.Emit(OpCodes.Ldloc, argsLocal);
            ilg.Emit(OpCodes.Ldc_I4, i);
            ilg.Emit(OpCodes.Ldarg, i + 1);
            if (paramTypes[i].IsValueType) ilg.Emit(OpCodes.Box, paramTypes[i]);
            ilg.Emit(OpCodes.Stelem_Ref);
        }

        if (returnType == typeof(void))
        {
            var invokeVoid = typeof(Bridge).GetNestedType("ProxyRuntime", BindingFlags.Public | BindingFlags.Static)!.GetMethod("InvokeVoid", BindingFlags.Public | BindingFlags.Static)!;
            ilg.Emit(OpCodes.Ldarg_0);
            ilg.Emit(OpCodes.Ldstr, dispatchName);
            ilg.Emit(OpCodes.Ldloc, argsLocal);
            ilg.Emit(OpCodes.Call, invokeVoid);
            ilg.Emit(OpCodes.Ret);
        }
        else
        {
            var proxyRuntimeType = typeof(Bridge).GetNestedType("ProxyRuntime", BindingFlags.Public | BindingFlags.Static)!;
            var gm = proxyRuntimeType.GetMethod("InvokeMethodTyped", BindingFlags.Public | BindingFlags.Static)!.MakeGenericMethod(returnType);
            ilg.Emit(OpCodes.Ldarg_0);
            ilg.Emit(OpCodes.Ldstr, dispatchName);
            ilg.Emit(OpCodes.Ldloc, argsLocal);
            ilg.Emit(OpCodes.Call, gm);
            ilg.Emit(OpCodes.Ret);
        }

        return mb;
    }

    private static DispatchResult CreateJsSubclass(
        string? assemblyName, string typeName, int callbackId, string[] interfaceNames, string[] memberNames)
        => CreateJsSubclass(assemblyName, typeName, callbackId, interfaceNames, memberNames, []);

    // Creates an instance of a JS subclass: a dynamic subclass of the base type (or of System.Object
    // when the "base" is an interface being implemented) whose overridden virtuals and interface
    // members call the JS object registered under `callbackId`. `ctorArgs` select and feed the
    // base constructor, as `super(...)` arguments do on Android/iOS.
    private static DispatchResult CreateJsSubclass(
        string? assemblyName, string typeName, int callbackId, string[] interfaceNames, string[] memberNames, object?[] ctorArgs)
    {
        // `typeName` is the base type; older runtimes passed an auto-generated proxy name there and
        // the base type in `assemblyName`.
        var baseType = ResolveType(null, typeName)
            ?? (string.IsNullOrEmpty(assemblyName) ? null : ResolveType(null, assemblyName))
            ?? throw new TypeLoadException($"Type not found: {typeName}");

        var interfaces = interfaceNames.ToList();
        if (baseType.IsInterface)
        {
            interfaces.Insert(0, baseType.FullName!);
            baseType = typeof(object);
        }
        if (baseType.IsSealed)
            throw new InvalidOperationException($"{baseType.FullName} is sealed and can't be extended.");

        var proxyType = GetOrCreateDynamicProxyType(baseType, interfaces.ToArray(), memberNames);

        // Pick the constructor by argument count, preferring one whose parameters the arguments
        // convert to.
        var ctors = proxyType.GetConstructors().Where(c => c.GetParameters().Length == ctorArgs.Length).ToArray();
        if (ctors.Length == 0)
            throw new MissingMethodException($"{baseType.FullName} has no constructor taking {ctorArgs.Length} argument(s).");
        object? instance = null;
        Exception? lastError = null;
        s_pendingProxyCallbackId.Value = callbackId;
        try
        {
            foreach (var ctor in ctors)
            {
                object?[] built;
                try { built = BuildArgsBinExact(ctorArgs, ctor.GetParameters()); }
                catch (Exception e) { lastError = e; continue; }
                if (!ArgsFit(built, ctor.GetParameters())) continue;
                instance = ctor.Invoke(built);
                break;
            }
            if (instance is null)
                throw lastError ?? new MissingMethodException(
                    $"No constructor of {baseType.FullName} accepts the given {ctorArgs.Length} argument(s).");
            RegisterJsBacked(instance, callbackId);
        }
        catch (TargetInvocationException tie) when (tie.InnerException is not null)
        {
            throw tie.InnerException;
        }
        finally
        {
            s_pendingProxyCallbackId.Value = null;
        }

        // Force COM CCW vtable creation for any implemented WinRT interfaces now, rather than
        // relying on it happening implicitly the first time the instance crosses the ABI boundary.
        if (interfaces.Count > 0) TryActivateWinRTInterfaces(instance);

        return Box(instance);
    }

    private static bool ArgsFit(object?[] built, ParameterInfo[] parameters)
    {
        for (int i = 0; i < parameters.Length; i++)
        {
            var p = parameters[i].ParameterType;
            if (built[i] is null) { if (p.IsValueType && Nullable.GetUnderlyingType(p) is null) return false; continue; }
            if (!p.IsInstanceOfType(built[i])) return false;
        }
        return true;
    }

    private static void RegisterJsBacked(object instance, int callbackId)
    {
        if (!s_proxyCallbacks.TryGetValue(instance, out _))
            s_proxyCallbacks.Add(instance, new ProxyCallbackHolder(callbackId));
    }

    internal static bool IsJsBacked(object instance) => s_proxyCallbacks.TryGetValue(instance, out _);

    // The live handle of a JS-backed instance, or null when it has none (its JS object was collected
    // and released the handle); Box then gives it a new one.
    private static ProxyCallbackHolder? JsBackedHolder(object instance, out bool handleLive)
    {
        handleLive = false;
        if (!s_proxyCallbacks.TryGetValue(instance, out var holder)) return null;
        handleLive = holder.HandleId != 0
            && s_handles.TryGetValue(holder.HandleId, out var current) && ReferenceEquals(current, instance);
        return holder;
    }

    private static int EnsureJsHandle(object instance, ProxyCallbackHolder holder)
    {
        JsBackedHolder(instance, out var live);
        return live ? holder.HandleId : Box(instance).HandleId();
    }

    // Locates CsWinRT's ComWrappersSupport via reflection (same style as the WinRT.IWinRTObject
    // lookup in ObtainNativePtr — no compile-time reference needed, since DotNetBridge.csproj is a
    // dependency-free net9.0 library and CsWinRT is only ever loaded into the process by the WinUI
    // app itself) and asks it to build a real COM CCW for the instance's implemented WinRT
    // interfaces now, via CsWinRT's JIT reflection-fallback vtable path (works for types it never
    // saw at compile time — see plan doc for the CsWinRT version/AOT caveats). Best-effort: no-ops
    // when CsWinRT isn't loaded, e.g. the xunit test host has no WinUI/CsWinRT at all.
    private static void TryActivateWinRTInterfaces(object instance)
    {
        try
        {
            Type? supportType = null;
            foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
            {
                try
                {
                    supportType = asm.GetType("WinRT.ComWrappersSupport");
                    if (supportType != null) break;
                }
                catch { }
            }
            if (supportType == null) return;

            foreach (var name in new[] { "CreateCCWForObject", "GetOrCreateComInterfaceForObject" })
            {
                foreach (var candidate in supportType.GetMethods(BindingFlags.Public | BindingFlags.Static)
                                                      .Where(m => m.Name == name))
                {
                    try
                    {
                        var target = candidate.IsGenericMethodDefinition
                            ? candidate.MakeGenericMethod(instance.GetType())
                            : candidate;
                        var ps = target.GetParameters();
                        if (ps.Length == 1) { target.Invoke(null, [instance]); return; }
                        if (ps.Length == 2 && ps[1].ParameterType == typeof(Guid)) { target.Invoke(null, [instance, Guid.Empty]); return; }
                    }
                    catch { /* try the next overload/name */ }
                }
            }
        }
        catch { /* best-effort only */ }
    }

    // The holder for a JS-backed instance. A virtual the base constructor calls runs before
    // CreateJsSubclass has registered the instance, so fall back to the pending callback id.
    private static ProxyCallbackHolder JsHolderFor(object instance)
    {
        if (s_proxyCallbacks.TryGetValue(instance, out var holder)) return holder;
        var pending = s_pendingProxyCallbackId.Value;
        if (pending.HasValue)
        {
            RegisterJsBacked(instance, pending.Value);
            if (s_proxyCallbacks.TryGetValue(instance, out holder)) return holder;
        }
        throw new InvalidOperationException("No JS callback registered for proxy instance.");
    }

    // Payload for the JS dispatcher: (self handle, member name, ...args). The arguments are sent
    // individually so each arrives as its JS value (numbers, strings, structs as plain objects,
    // objects as handles).
    private static object?[] DispatchPayload(object instance, ProxyCallbackHolder holder, string memberName, object?[] args)
    {
        var payload = new object?[args.Length + 2];
        payload[0] = new HandleRef(EnsureJsHandle(instance, holder));
        payload[1] = memberName;
        Array.Copy(args, 0, payload, 2, args.Length);
        return payload;
    }

    private static object? ProxyInvokeMethod(object instance, string methodName, object[] args)
    {
        var holder = JsHolderFor(instance);
        return CallJsCallback(holder.CallbackId, DispatchPayload(instance, holder, methodName, args), expectsResult: true);
    }

    private static void ProxyInvokeVoid(object instance, string methodName, object[] args)
    {
        var holder = JsHolderFor(instance);
        CallJsCallback(holder.CallbackId, DispatchPayload(instance, holder, methodName, args), expectsResult: false);
    }

    private static object? ProxyGetProperty(object instance, string propertyName)
    {
        var holder = JsHolderFor(instance);
        return CallJsCallback(holder.CallbackId, DispatchPayload(instance, holder, "get_" + propertyName, []), expectsResult: true);
    }

    private static void ProxySetProperty(object instance, string propertyName, object? value)
    {
        var holder = JsHolderFor(instance);
        CallJsCallback(holder.CallbackId, DispatchPayload(instance, holder, "set_" + propertyName, [value]), expectsResult: false);
    }
}
