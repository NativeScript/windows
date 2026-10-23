using System;
using System.Collections.Generic;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Threading;

namespace NativeScriptBridge;

public static partial class Bridge
{
    // One handle per live object, so JS gets one proxy per object. Each handle sent out counts as
    // an export; a collected proxy gives back the exports it saw, and the handle goes when none are
    // left, so a release can't race a new export.
    private static readonly Dictionary<object, int> s_handleOf = new(ReferenceEqualityComparer.Instance);
    private static readonly Dictionary<int, int> s_exports = new();
    private static readonly object s_handleLock = new();

    // The handle for `value`, counting an export. Value types get a new one every time.
    internal static int HandleFor(object value)
    {
        var shared = !value.GetType().IsValueType;
        lock (s_handleLock)
        {
            if (shared && s_handleOf.TryGetValue(value, out var existing))
            {
                s_exports[existing] = s_exports.TryGetValue(existing, out var n) ? n + 1 : 1;
                return existing;
            }
            var id = Interlocked.Increment(ref s_nextHandle);
            s_handles[id] = value;
            if (shared)
            {
                s_handleOf[value] = id;
                s_exports[id] = 1;
            }
            return id;
        }
    }

    // Gives back `count` exports (all when negative); the handle goes when none remain.
    internal static void ReleaseHandle(int id, int count)
    {
        lock (s_handleLock)
        {
            if (count >= 0 && s_exports.TryGetValue(id, out var n))
            {
                if (n > count)
                {
                    s_exports[id] = n - count;
                    return;
                }
            }
            s_exports.Remove(id);
            if (!s_handles.TryRemove(id, out var obj)) return;
            if (obj is not null && s_handleOf.TryGetValue(obj, out var owner) && owner == id)
                s_handleOf.Remove(obj);
        }
        if (s_nativePtrs.TryRemove(id, out var nativePtr))
        {
            RemoveNativePtr(id, nativePtr);
            try { Marshal.Release(nativePtr); } catch { }
        }
    }

    internal static void ClearHandles()
    {
        lock (s_handleLock)
        {
            s_handleOf.Clear();
            s_exports.Clear();
            s_handles.Clear();
        }
    }
}
