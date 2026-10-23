using System;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using System.Runtime.CompilerServices;

namespace NativeScriptBridge;

// A property or field: JS reads and writes both as properties.
internal sealed class DataMember
{
    private readonly PropertyInfo? _property;
    private readonly FieldInfo? _field;

    public DataMember(PropertyInfo property) => _property = property;
    public DataMember(FieldInfo field) => _field = field;

    public Type PropertyType => _property?.PropertyType ?? _field!.FieldType;

    public string Name => _property?.Name ?? _field!.Name;

    public object? GetValue(object? target) => _property is not null ? _property.GetValue(target) : _field!.GetValue(target);

    public void SetValue(object? target, object? value)
    {
        if (_property is not null)
        {
            _property.SetValue(target, value);
            return;
        }
        if (_field!.IsInitOnly || _field.IsLiteral)
            throw new FieldAccessException($"Cannot assign to read-only field '{_field.Name}' of .NET type '{_field.DeclaringType?.FullName}'");
        _field.SetValue(target, value);
    }

    // The property or field `name` of `type`, or null.
    public static DataMember? Find(Type type, string name, BindingFlags flags)
    {
        PropertyInfo? property = null;
        try { property = type.GetProperty(name, flags); }
        catch (AmbiguousMatchException)
        {
            // A property redeclared with `new` in a derived class: the most derived one.
            property = type.GetProperties(flags).Where(p => p.Name == name && p.GetIndexParameters().Length == 0)
                .OrderByDescending(p => Depth(p.DeclaringType)).FirstOrDefault();
        }
        if (property is not null && property.GetIndexParameters().Length == 0) return new DataMember(property);
        var field = type.GetField(name, flags);
        return field is not null && !field.IsSpecialName ? new DataMember(field) : null;
    }

    private static int Depth(Type? t)
    {
        var depth = 0;
        for (; t is not null; t = t.BaseType) depth++;
        return depth;
    }
}

public static partial class Bridge
{
    internal static DispatchResult BuildMembersResult(Type t)
    {
        const BindingFlags inst = BindingFlags.Public | BindingFlags.Instance;
        const BindingFlags stat = BindingFlags.Public | BindingFlags.Static;
        const BindingFlags family = BindingFlags.NonPublic | BindingFlags.Instance;

        var instProps = Array.FindAll(t.GetProperties(inst), p => p.GetIndexParameters().Length == 0);
        var statProps = Array.FindAll(t.GetProperties(stat), p => p.GetIndexParameters().Length == 0);
        var instFields = Array.FindAll(t.GetFields(inst), f => !f.IsSpecialName);
        var statFields = Array.FindAll(t.GetFields(stat), f => !f.IsSpecialName);

        static string[] Names(IEnumerable<MemberInfo> members) => members.Select(m => m.Name).Distinct().ToArray();
        static bool ReadOnlyField(FieldInfo f) => f.IsInitOnly || f.IsLiteral;

        var instEvents = t.GetEvents(inst);
        var statEvents = t.GetEvents(stat);
        var protectedMethods = t.GetMethods(family).Where(m => (m.IsFamily || m.IsFamilyOrAssembly) && !m.IsSpecialName);
        var protectedProps = t.GetProperties(family).Where(p => p.GetIndexParameters().Length == 0
            && (p.GetMethod ?? p.SetMethod) is { } a && (a.IsFamily || a.IsFamilyOrAssembly));

        return DispatchResult.Members(
            methods:               Names(t.GetMethods(inst).Where(m => !m.IsSpecialName)),
            props:                 Names(instProps.Where(p => p.GetGetMethod() != null).Concat<MemberInfo>(instFields)),
            staticMethods:         Names(t.GetMethods(stat).Where(m => !m.IsSpecialName)),
            staticProps:           Names(statProps.Where(p => p.GetGetMethod() != null).Concat<MemberInfo>(statFields)),
            readonlyProps:         Names(instProps.Where(p => p.GetGetMethod() != null && p.GetSetMethod() == null).Concat<MemberInfo>(instFields.Where(ReadOnlyField))),
            readonlyStaticProps:   Names(statProps.Where(p => p.GetGetMethod() != null && p.GetSetMethod() == null).Concat<MemberInfo>(statFields.Where(ReadOnlyField))),
            writeonlyProps:        Names(instProps.Where(p => p.GetGetMethod() == null && p.GetSetMethod() != null)),
            writeonlyStaticProps:  Names(statProps.Where(p => p.GetGetMethod() == null && p.GetSetMethod() != null)),
            extras:
            [
                ("events", Names(instEvents)),
                ("staticEvents", Names(statEvents)),
                ("protectedMethods", Names(protectedMethods)),
                ("protectedProperties", Names(protectedProps)),
                ("nestedTypes", Names(t.GetNestedTypes(BindingFlags.Public))),
            ]);
    }

    // Delegates added for JS functions, by function token, so remove_X(fn) removes the right one.
    private sealed class EventHandlers
    {
        public readonly Dictionary<(string Event, int Token), List<Delegate>> Added = new();
        // The handler of each event assigned with `obj.Event = fn`.
        public readonly Dictionary<string, Delegate> Assigned = new();
    }

    private static readonly ConditionalWeakTable<object, EventHandlers> s_instanceEvents = new();
    private static readonly Dictionary<Type, EventHandlers> s_staticEvents = new();

    private static (EventInfo Event, EventHandlers Handlers) EventOf(object? target, string typeName, string eventName)
    {
        var type = target?.GetType() ?? ResolveType(null, typeName)
            ?? throw new TypeLoadException($"Type not found: {typeName}");
        var flags = BindingFlags.Public | (target is null ? BindingFlags.Static : BindingFlags.Instance);
        var ev = type.GetEvent(eventName, flags)
            ?? throw new MissingMemberException($"Event '{eventName}' not found on {type.FullName}");
        EventHandlers handlers;
        if (target is null)
        {
            lock (s_staticEvents)
            {
                if (!s_staticEvents.TryGetValue(type, out handlers!)) s_staticEvents[type] = handlers = new EventHandlers();
            }
        }
        else
        {
            handlers = s_instanceEvents.GetValue(target, static _ => new EventHandlers());
        }
        return (ev, handlers);
    }

    /// `obj.add_Event(fn)` from JS: subscribes a delegate of the event's type calling `fn`.
    public static void AddEventHandler(object? target, string typeName, string eventName, int token, JsFunctionRef fn)
    {
        var (ev, handlers) = EventOf(target, typeName, eventName);
        var handler = MakeJsDelegate(ev.EventHandlerType!, fn.Id);
        ev.AddEventHandler(target, handler);
        lock (handlers)
        {
            if (!handlers.Added.TryGetValue((eventName, token), out var list))
                handlers.Added[(eventName, token)] = list = new List<Delegate>();
            list.Add(handler);
        }
    }

    /// `obj.remove_Event(fn)` from JS: unsubscribes the delegate added for `fn`.
    public static void RemoveEventHandler(object? target, string typeName, string eventName, int token)
    {
        var (ev, handlers) = EventOf(target, typeName, eventName);
        Delegate? handler = null;
        lock (handlers)
        {
            if (handlers.Added.TryGetValue((eventName, token), out var list) && list.Count > 0)
            {
                handler = list[^1];
                list.RemoveAt(list.Count - 1);
                if (list.Count == 0) handlers.Added.Remove((eventName, token));
            }
        }
        if (handler is not null) ev.RemoveEventHandler(target, handler);
    }

    /// `obj.Event = fn` from JS: replaces the handler assigned before (null removes it).
    public static void SetEventHandler(object? target, string typeName, string eventName, JsFunctionRef? fn)
    {
        var (ev, handlers) = EventOf(target, typeName, eventName);
        Delegate? previous;
        lock (handlers)
        {
            handlers.Assigned.Remove(eventName, out previous);
        }
        if (previous is not null) ev.RemoveEventHandler(target, previous);
        if (fn is not { } f) return;
        var handler = MakeJsDelegate(ev.EventHandlerType!, f.Id);
        ev.AddEventHandler(target, handler);
        lock (handlers)
        {
            handlers.Assigned[eventName] = handler;
        }
    }
}
