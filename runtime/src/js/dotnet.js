// NSWinRT.dotnet: .NET objects, namespaces and delegates as JS values.
// Shared by both engines: global_fns.rs HELPER_SOURCE (classic) and napi_engine/dotnet.rs.

// Requires the dotnet-bridge project to be published into
//   <app-root>/dotnet-bridge/publish/DotNetBridge.dll
(function () {
    if (typeof globalThis.__nsDotNetInvokeBin !== 'function') return;
    globalThis.NSWinRT = globalThis.NSWinRT || {};

    // Binary protocol: no JSON.stringify / JSON.parse.
    // __nsDotNetInvokeBin(handle, typeName, assembly, method, ...args)
    // throws on error, returns the result value directly.
    function _invoke(req) {
        var handle = (req.handle !== undefined && req.handle !== null) ? req.handle : -1;
        var args   = req.args;
        var typeName = req.typeName || '', assembly = req.assembly || '', method = req.method || '';
        try {
            if (!args || args.length === 0) return globalThis.__nsDotNetInvokeBin(handle, typeName, assembly, method);
            if (args.length === 1) return globalThis.__nsDotNetInvokeBin(handle, typeName, assembly, method, args[0]);
            return globalThis.__nsDotNetInvokeBin(handle, typeName, assembly, method, ...args);
        } catch (e) {
            throw _dotnetError(e);
        }
    }

    // _invoke with a function's own `arguments`, unwrapped straight into the native call (no
    // request object or intermediate arrays).
    function _invokeWith(handle, typeName, assembly, method, args) {
        var invoke = globalThis.__nsDotNetInvokeBin;
        try {
            for (var s = 0; s < args.length; s++) {
                var a = args[s];
                if (a !== null && typeof a === 'object' && (a instanceof _NamedArgs || _isOutParam(a)))
                    return _invokeSpecial(handle, typeName, assembly, method, args);
            }
            switch (args.length) {
                case 0: return invoke(handle, typeName, assembly, method);
                case 1: return invoke(handle, typeName, assembly, method, _unwrap(args[0]));
                case 2: return invoke(handle, typeName, assembly, method, _unwrap(args[0]), _unwrap(args[1]));
                case 3: return invoke(handle, typeName, assembly, method, _unwrap(args[0]), _unwrap(args[1]), _unwrap(args[2]));
            }
            // Built by push so the array stays packed: apply on a holey array takes V8's slow path.
            var call = [handle, typeName, assembly, method];
            for (var i = 0; i < args.length; i++) call.push(_unwrap(args[i]));
            return invoke.apply(null, call);
        } catch (e) {
            throw _dotnetError(e);
        }
    }

    // `NSWinRT.dotnet.named({ b: 2 })` as the last argument: arguments by parameter name.
    function _NamedArgs(values) { this.values = values; }

    // `NSWinRT.interop.out()` (an OutParam): a reference for a ref or out parameter.
    function _isOutParam(a) { return Object.prototype.hasOwnProperty.call(a, '__nswinrt_out_param__'); }

    // A call with references or named arguments: `&Name` returns [result, ...arguments after the
    // call] to fill the references in; `Name#a,b` names the trailing arguments.
    function _invokeSpecial(handle, typeName, assembly, method, args) {
        var call = [handle, typeName, assembly, ''];
        var refs = false, named = null;
        for (var i = 0; i < args.length; i++) {
            var a = args[i];
            if (a !== null && typeof a === 'object' && a instanceof _NamedArgs) {
                named = Object.keys(a.values || {});
                for (var k = 0; k < named.length; k++) call.push(_unwrap(a.values[named[k]]));
                break;
            }
            if (a !== null && typeof a === 'object' && _isOutParam(a)) {
                refs = true;
                call.push(_unwrap(a.value === undefined ? null : a.value));
            } else {
                call.push(_unwrap(a));
            }
        }
        call[3] = (refs ? '&' : '') + method + (named && named.length ? '#' + named.join(',') : '');
        var result = globalThis.__nsDotNetInvokeBin.apply(null, call);
        if (!refs) return result;
        for (var j = 0; j < args.length; j++) {
            if (args[j] !== null && typeof args[j] === 'object' && _isOutParam(args[j])) args[j].value = _wrap(result[1 + j]);
        }
        return result[0];
    }

    // Turns a bridge error's raw `nativeException` into a proxy and appends the .NET stack.
    function _dotnetError(e) {
        if (!e || typeof e !== 'object' || e.__nsDotNet === true) return e;
        var raw = e.nativeException;
        if (!raw || typeof raw !== 'object') return e;
        try { Object.defineProperty(e, '__nsDotNet', { value: true }); } catch (_) {}
        e.nativeException = typeof raw.__handle === 'number' ? _wrap(raw) : raw;
        if (typeof e.dotnetStack === 'string' && e.dotnetStack && typeof e.stack === 'string') e.stack += '\n' + e.dotnetStack;
        return e;
    }

    // An Error for a .NET exception passed to JS as a value (a faulted Task, an unhandled exception).
    function _errorFromException(value) {
        var ex = _wrap(value);
        if (!ex || typeof ex !== 'object' || typeof ex.__handle !== 'number')
            return value instanceof Error ? value : new Error(String(value));
        var message = '';
        try { message = String(ex.Message); } catch (_) {}
        var err = new Error(message);
        err.dotnetType = ex.__type;
        try { err.dotnetStack = ex.StackTrace || ''; } catch (_) { err.dotnetStack = ''; }
        err.nativeException = ex;
        try { Object.defineProperty(err, '__nsDotNet', { value: true }); } catch (_) {}
        if (err.dotnetStack && typeof err.stack === 'string') err.stack += '\n' + err.dotnetStack;
        return err;
    }

    // Populated lazily on first access; avoids repeated bridge round-trips.
    var _typeInfoCache = {};
    // A type's member lists get `kinds` (member name -> _K_* flags), so the proxy traps look a name up
    // instead of scanning the lists on every access.
    var _K_PROP = 1, _K_WO = 2, _K_RO = 4, _K_SPROP = 8, _K_SMETHOD = 16, _K_SWO = 32, _K_SRO = 64,
        _K_EVENT = 128, _K_SEVENT = 256, _K_METHOD = 512;
    function _withKinds(info) {
        var kinds = Object.create(null);
        function add(list, flag) {
            if (!list) return;
            for (var i = 0; i < list.length; i++) kinds[list[i]] = (kinds[list[i]] | 0) | flag;
        }
        add(info.properties, _K_PROP);
        add(info.writeonlyProperties, _K_WO);
        add(info.readonlyProperties, _K_RO);
        add(info.staticProperties, _K_SPROP);
        add(info.staticMethods, _K_SMETHOD);
        add(info.writeonlyStaticProperties, _K_SWO);
        add(info.readonlyStaticProperties, _K_SRO);
        add(info.events, _K_EVENT);
        add(info.staticEvents, _K_SEVENT);
        add(info.methods, _K_METHOD);
        info.kinds = kinds;
        return info;
    }
    function _kindsOf(info) { return info.kinds || _withKinds(info).kinds; }

    // "get_X" / "set_X" accessor names, made once per property name.
    var _getterNames = Object.create(null), _setterNames = Object.create(null);
    function _getterName(prop) { return _getterNames[prop] || (_getterNames[prop] = 'get_' + prop); }
    function _setterName(prop) { return _setterNames[prop] || (_setterNames[prop] = 'set_' + prop); }

    // The native COM pointer of a handle's object as the BigInt the runtime reads from `__native_ptr`,
    // or undefined when it has none. The bridge makes it for a plain .NET object only on request.
    function _nativePtrOf(handle) {
        try {
            var p = _invoke({ assembly: '', typeName: 'NativeScriptBridge.Bridge', method: 'GetNativePtrForHandle', args: [handle] });
            if (typeof p !== 'number' || p === 0) return undefined;
            return typeof BigInt === 'function' ? BigInt(p) : p;
        } catch (_) {
            return undefined;
        }
    }
    var _emptyInfo = _withKinds({ methods: [], properties: [], staticMethods: [], staticProperties: [], readonlyProperties: [], readonlyStaticProperties: [], writeonlyProperties: [], writeonlyStaticProperties: [] });
    // Optional mapping for namespace prefixes -> assembly simple-name.
    // Exact namespaces are preferred first, then progressively shorter
    // prefixes are tried as a fallback.
    var _namespaceAssemblyMap = Object.create(null);

    // _resolveAssembly results by type name, cleared whenever the namespace map changes.
    var _assemblyCache = Object.create(null);

    function _resolveAssembly(typeName) {
        if (!typeName || typeof typeName !== 'string') return '';
        var cached = _assemblyCache[typeName];
        if (cached !== undefined) return cached;
        var resolved = '';
        // A closed generic (`MyLib.Box`1[System.String]`) lives where its definition does.
        var cut = typeName.search(/[`\[]/);
        var probe = cut > 0 ? typeName.substring(0, cut) : typeName;
        while (probe) {
            var assembly = _namespaceAssemblyMap[probe];
            if (typeof assembly === 'string' && assembly) { resolved = assembly; break; }
            var lastDot = probe.lastIndexOf('.');
            if (lastDot < 0) break;
            probe = probe.substring(0, lastDot);
        }
        _assemblyCache[typeName] = resolved;
        return resolved;
    }

    // One proxy per live .NET object. The bridge counts each time it sends a handle; `seen` is how
    // many of those this proxy took, given back together when it is collected or released.
    var _proxyByHandle = new Map();
    var _canWeakRefProxies = typeof WeakRef === 'function';
    var _dotNetFinalizers = typeof FinalizationRegistry === 'function'
        ? new FinalizationRegistry(function (entry) { _releaseEntry(entry); })
        : null;

    function _releaseEntry(entry) {
        if (_proxyByHandle.get(entry.handle) === entry) _proxyByHandle.delete(entry.handle);
        var seen = entry.seen;
        if (seen === 0) return;
        entry.seen = 0;
        try {
            if (seen < 0) globalThis.__nsDotNetInvokeBin(entry.handle, '', '', '__release');
            else globalThis.__nsDotNetInvokeBin(entry.handle, '', '', '__release', seen);
        } catch (e) {}
    }

    // Registers `proxy` as the proxy of `handle`; seen < 0 releases the handle outright.
    function _track(proxy, handle, seen) {
        var entry = { handle: handle, seen: seen, ref: _canWeakRefProxies ? new WeakRef(proxy) : null };
        if (_canWeakRefProxies) _proxyByHandle.set(handle, entry);
        if (_dotNetFinalizers) _dotNetFinalizers.register(proxy, entry, entry);
        return entry;
    }

    // Auto-populate namespace→assembly map from the managed side.
    // GetNamespaceAssemblyMapJson scans the app directory for assemblies and
    // returns a JSON map of { rootNamespace: assemblySimpleName }.
    // Runs once at IIFE init; any failure is silently ignored.
    try {
        var _autoNsJson = _invoke({ assembly: '', typeName: 'NativeScriptBridge.Bridge', method: 'GetNamespaceAssemblyMapJson', args: [] });
        if (typeof _autoNsJson === 'string' && _autoNsJson) {
            var _parsed = JSON.parse(_autoNsJson);
            if (_parsed && typeof _parsed === 'object') {
                for (var _k in _parsed) {
                    if (Object.prototype.hasOwnProperty.call(_parsed, _k) && typeof _parsed[_k] === 'string') {
                        _namespaceAssemblyMap[_k] = _parsed[_k];
                    }
                }
            }
        }
    } catch (_e) {}

    // `handle`, when given, is an instance of the type: its members are read from it when the type
    // can't be found by name.
    function _getTypeInfo(assembly, typeName, handle) {
        if (!typeName) return _emptyInfo;
        var cached = _typeInfoCache[typeName];
        if (cached !== undefined) return cached;
        // A namespace of the app's assemblies isn't a type; asking the bridge would scan them all.
        if (typeof _namespaceAssemblyMap[typeName] === 'string' && handle === undefined) {
            _typeInfoCache[typeName] = _emptyInfo;
            return _emptyInfo;
        }
        var info = _emptyInfo;
        try {
            // Respect an explicitly-provided assembly name. When empty,
            // let the managed side attempt resolution (BCL types via Type.GetType).
            var asm = (typeof assembly === 'string') ? assembly : '';
            var members = _invoke({ assembly: asm, typeName: typeName, method: '__members__', args: [] });
            if (members && typeof members === 'object') info = _withKinds(members);
        } catch (e) {
            if (typeof handle === 'number' && handle >= 0) {
                try {
                    var own = _invoke({ handle: handle, method: '__members__', args: [] });
                    if (own && typeof own === 'object') info = _withKinds(own);
                } catch (_) {}
            }
        }
        _typeInfoCache[typeName] = info;
        return info;
    }

    function _unwrap(v) {
        if (v && typeof v === 'object' && typeof v.__handle === 'number') return { __handle: v.__handle };
        if (Array.isArray(v)) return v.map(_unwrap);
        // A JS Date reaches DateTime / DateTimeOffset parameters as its ISO 8601 string.
        if (v instanceof Date) return isNaN(v.getTime()) ? null : v.toISOString();
        // A JS function passed where .NET expects a delegate gets .NET objects as arguments, the same
        // proxies a method result would be, rather than raw handles.
        if (typeof v === 'function' && typeof v.__dotnetType__ !== 'string' && typeof v.__handle !== 'number')
            return _callbackFor(v);
        return v;
    }

    // The wrapper sent for a JS callback: one per function, so passing it again sends the same one.
    var _callbacks = typeof WeakMap === 'function' ? new WeakMap() : null;
    function _callbackFor(fn) {
        var wrapped = _callbacks && _callbacks.get(fn);
        if (wrapped) return wrapped;
        wrapped = function () { return fn.apply(this, Array.prototype.map.call(arguments, _wrap)); };
        if (_callbacks) _callbacks.set(fn, wrapped);
        return wrapped;
    }

    // .NET collections (List<T>, Dictionary, ObservableCollection) act like JS arrays: `length`,
    // `list[i]`, for...of, and the array methods (map, filter, ...) on a copy of their items.
    // _collectionKind is the bridge's CollectionKind flags (1 enumerable, 2 count, 4 indexed) by type.
    var _collectionKinds = Object.create(null);
    var _arrayMethods = Object.create(null);
    ['forEach', 'map', 'filter', 'reduce', 'reduceRight', 'some', 'every', 'find', 'findIndex', 'findLast',
        'findLastIndex', 'indexOf', 'lastIndexOf', 'includes', 'join', 'slice', 'concat', 'flat', 'flatMap',
        'entries', 'keys', 'values', 'at', 'toJSON'].forEach(function (name) { _arrayMethods[name] = 1; });

    function _bridgeCall(method, args) {
        return _invoke({ assembly: '', typeName: 'NativeScriptBridge.Bridge', method: method, args: args });
    }

    function _isIndex(prop) {
        var c = prop.charCodeAt(0);
        return c >= 48 && c <= 57 && /^(0|[1-9]\d*)$/.test(prop);
    }

    function _collectionKind(handle, typeName) {
        var kind = typeName ? _collectionKinds[typeName] : undefined;
        if (kind !== undefined) return kind;
        try { kind = _bridgeCall('CollectionKind', [{ __handle: handle }]) | 0; } catch (_) { kind = 0; }
        if (typeName) _collectionKinds[typeName] = kind;
        return kind;
    }

    function _collectionItems(handle) {
        return _wrap(_bridgeCall('CollectionItems', [{ __handle: handle }])) || [];
    }

    // `prop` of a collection proxy when it is one of the array-like members, else undefined.
    function _collectionMember(handle, typeName, prop) {
        var kind = _collectionKind(handle, typeName);
        if (prop === 'length') return (kind & 2) ? _bridgeCall('CollectionCount', [{ __handle: handle }]) : undefined;
        if (_arrayMethods[prop] === 1) {
            if (!(kind & 1)) return undefined;
            return function () {
                var items = _collectionItems(handle);
                return prop === 'toJSON' ? items : Array.prototype[prop].apply(items, arguments);
            };
        }
        return (kind & 4) ? _wrap(_bridgeCall('ItemAt', [{ __handle: handle }, +prop])) : undefined;
    }

    // A token per JS function subscribed to a .NET event, so remove_X(fn) finds its delegate.
    var _fnTokens = typeof WeakMap === 'function' ? new WeakMap() : null;
    var _nextFnToken = 1;
    function _fnToken(fn) {
        var token = _fnTokens && _fnTokens.get(fn);
        if (!token) {
            token = _nextFnToken++;
            if (_fnTokens) _fnTokens.set(fn, token);
        }
        return token;
    }

    // The event name of `add_X` / `remove_X` when X is an event with flag `flag` in `kinds`.
    function _eventOf(prop, kinds, flag) {
        var name = prop.charCodeAt(0) === 97 && prop.indexOf('add_') === 0 ? prop.substring(4)
            : prop.charCodeAt(0) === 114 && prop.indexOf('remove_') === 0 ? prop.substring(7) : null;
        return name !== null && ((kinds[name] | 0) & flag) ? name : null;
    }

    // `add_X(fn)` / `remove_X(fn)` of an instance (handle) or a type's static event (typeName).
    function _eventAccessor(add, handle, typeName, name) {
        return function (fn) {
            var target = handle >= 0 ? { __handle: handle } : null;
            // A delegate made with asDelegate (or one .NET returned) is added and removed as itself.
            if (typeof fn !== 'function' || typeof fn.__handle === 'number' || typeof fn.__dotnetType__ === 'string')
                return _wrap(_invoke({ handle: handle, typeName: typeName, assembly: _resolveAssembly(typeName), method: (add ? 'add_' : 'remove_') + name, args: [_unwrap(fn)] }));
            if (add) _bridgeCall('AddEventHandler', [target, typeName, name, _fnToken(fn), _callbackFor(fn)]);
            else _bridgeCall('RemoveEventHandler', [target, typeName, name, _fnToken(fn)]);
        };
    }

    // `obj.X = fn` for an event X: replaces the handler assigned before; null removes it.
    function _assignEvent(handle, typeName, name, fn) {
        if (fn !== null && fn !== undefined && typeof fn !== 'function')
            throw new TypeError('Assign a function (or null) to the .NET event \'' + name + '\'');
        _bridgeCall('SetEventHandler', [handle >= 0 ? { __handle: handle } : null, typeName, name, fn ? _callbackFor(fn) : null]);
    }

    // The .NET name of a type argument for `.of(...)`: a .NET type proxy, a type name, or the JS
    // constructor of a primitive.
    function _typeArgName(t) {
        if (typeof t === 'string') return t;
        if (t === String) return 'System.String';
        if (t === Number) return 'System.Double';
        if (t === Boolean) return 'System.Boolean';
        if (t === Object) return 'System.Object';
        if (t === Date) return 'System.DateTime';
        if (typeof BigInt === 'function' && t === BigInt) return 'System.Int64';
        var name = t && (typeof t === 'function' || typeof t === 'object') ? t.__dotnetType__ : undefined;
        if (typeof name === 'string' && name) return name;
        throw new TypeError('Not a .NET type: ' + String(t));
    }

    function _typeArgList(args) {
        return Array.prototype.map.call(args, _typeArgName).join(',');
    }

    // Adds `.of(T1, ...)` to a method function, for explicit generic type arguments.
    function _withTypeArgs(fn, call) {
        fn.of = function () {
            var suffix = '<' + _typeArgList(arguments) + '>';
            return function () { return call(suffix, arguments); };
        };
        return fn;
    }

    // Makes sw.Stop() and sw.Elapsed both work naturally.
    // The proxy is registered with _dotNetFinalizers so the CLR reference
    // is released automatically when JS GC collects the proxy.
    function _makeDotNetInstance(handle, assembly, typeName, isTask, nativePtr) {
        _getTypeInfo(assembly, typeName, handle);
        // Method functions by name, made on first access.
        var methods = null;
        // Handlers assigned to events (`obj.Click = fn`), by event name.
        var assigned = null;
        var ptrKnown = nativePtr !== undefined;
        // The proxy target holds the plain JS fields set on the object.
        var proxy = new Proxy({}, {
            get: function (target, prop) {
                if (typeof prop === 'symbol') {
                    if (prop === Symbol.iterator && (_collectionKind(handle, typeName) & 1))
                        return function () { return _collectionItems(handle)[Symbol.iterator](); };
                    return target[prop];
                }
                if (prop === '__handle') return handle;
                if (prop === '__type')   return typeName;
                if (prop === '__isTask') return isTask === true;
                if (prop === '__native_ptr') {
                    if (!ptrKnown) { ptrKnown = true; nativePtr = _nativePtrOf(handle); }
                    return nativePtr;
                }
                if (prop === 'release') return function () {
                    var entry = _proxyByHandle.get(handle);
                    if (entry && _dotNetFinalizers) _dotNetFinalizers.unregister(entry);
                    _releaseEntry(entry || { handle: handle, seen: -1 });
                };
                // Like a plain object, so `'' + obj` and comparisons fall through to toString.
                if (prop === 'valueOf') return function () { return proxy; };
                // The object's .NET ToString().
                if (prop === 'toString') return function () {
                    return String(_invoke({ handle: handle, method: 'ToString', args: [] }));
                };
                // A Task/ValueTask (awaitable) result is thenable, so `await obj.SomethingAsync()` works;
                // any other .NET object must not look like a promise.
                if (prop === 'then') {
                    if (isTask !== true) return undefined;
                    return function (onFulfilled, onRejected) {
                        return globalThis.NSWinRT.dotnet.taskToPromise({ __handle: handle, __isTask: true }).then(onFulfilled, onRejected);
                    };
                }
                if (Object.prototype.hasOwnProperty.call(target, prop)) return target[prop];
                // Re-read info in case it was populated after construction.
                var kind = _kindsOf(_typeInfoCache[typeName] || _emptyInfo)[prop] | 0;
                // Write-only (setter, no getter): reading it is an error.
                if (kind & _K_WO)
                    throw new TypeError('Cannot read write-only property \'' + prop + '\' of .NET type \'' + typeName + '\'');
                if (kind & _K_PROP)
                    return _wrap(_invoke({ handle: handle, method: _getterName(prop), args: [] }));
                if (kind & _K_EVENT) return (assigned && assigned[prop]) || null;
                if (prop === 'length' || _arrayMethods[prop] === 1 || _isIndex(prop)) {
                    var member = _collectionMember(handle, typeName, prop);
                    if (member !== undefined) return member;
                }
                if (methods !== null && methods[prop]) return methods[prop];
                // A key of a string-keyed dictionary (`dict.key`, `dict['key']`).
                if (!(kind & _K_METHOD) && (_collectionKind(handle, typeName) & 8)) {
                    var hit = _bridgeCall('DictionaryTryGet', [{ __handle: handle }, prop]);
                    if (hit && hit[0] === true) return _wrap(hit[1]);
                }
                // Not a native property: a function for method dispatch.
                if (methods === null) methods = Object.create(null);
                var ev = _eventOf(prop, _kindsOf(_typeInfoCache[typeName] || _emptyInfo), _K_EVENT);
                if (ev !== null) return methods[prop] = _eventAccessor(prop.charCodeAt(0) === 97, handle, '', ev);
                return methods[prop] = _withTypeArgs(function () {
                    return _wrap(_invokeWith(handle, '', '', prop, arguments));
                }, function (suffix, args) {
                    return _wrap(_invokeWith(handle, '', '', prop + suffix, args));
                });
            },
            set: function (target, prop, value) {
                if (typeof prop !== 'symbol') {
                    var kind = _kindsOf(_typeInfoCache[typeName] || _emptyInfo)[prop] | 0;
                    // Read-only (getter, no setter): assigning it is an error.
                    if (kind & _K_RO)
                        throw new TypeError('Cannot assign to read-only property \'' + prop + '\' of .NET type \'' + typeName + '\'');
                    // Writable (read-write or write-only): invoke the setter.
                    if (kind & (_K_PROP | _K_WO)) {
                        _invoke({ handle: handle, method: _setterName(prop), args: [_unwrap(value)] });
                        return true;
                    }
                    if (kind & _K_EVENT) {
                        _assignEvent(handle, '', prop, value);
                        (assigned || (assigned = Object.create(null)))[prop] = value || null;
                        return true;
                    }
                    if (_isIndex(prop) && (_collectionKind(handle, typeName) & 4)) {
                        _bridgeCall('SetItemAt', [{ __handle: handle }, +prop, _unwrap(value)]);
                        return true;
                    }
                    if (!(kind & _K_METHOD) && (_collectionKind(handle, typeName) & 8)) {
                        _bridgeCall('DictionarySet', [{ __handle: handle }, prop, _unwrap(value)]);
                        return true;
                    }
                }
                // Not a native property: a plain JS field of this object.
                target[prop] = value;
                return true;
            },
        });
        return proxy;
    }

    function _wrap(value) {
        if (value == null) return null;
        if (Array.isArray(value)) return value.map(_wrap);
        if (typeof value === 'object' && typeof value.__handle === 'number') {
            var typeName = value.__type || '';
            var jsSelf = _jsSelfByHandle && _deref(_jsSelfByHandle.get(value.__handle));
            if (jsSelf) return jsSelf;
            // A JS subclass instance whose JS object was collected: its dispatcher revives one.
            if (typeName.indexOf('NSWinRTDynamicProxies.') === 0) {
                try { _invoke({ handle: value.__handle, method: '__ns_js_self', args: [] }); } catch (_) {}
                jsSelf = _deref(_jsSelfByHandle.get(value.__handle));
                if (jsSelf) return jsSelf;
            }
            var entry = _proxyByHandle.get(value.__handle);
            var existing = entry && entry.ref && entry.ref.deref();
            if (existing) {
                entry.seen++;
                return existing;
            }
            var assembly = _resolveAssembly(typeName);
            var proxy = _makeDotNetInstance(value.__handle, assembly, typeName, value.__isTask === true, value.__native_ptr);
            _track(proxy, value.__handle, 1);
            return proxy;
        }
        return value;
    }

    globalThis.NSWinRT.dotnet = {
        invoke: function (assembly, typeName, method, args) {
            return _wrap(_invoke({ assembly: assembly, typeName: typeName, method: method, args: (args || []).map(_unwrap) }));
        },
        get: function (assembly, typeName, prop) {
            return _wrap(_invoke({ assembly: assembly, typeName: typeName, method: 'get_' + prop, args: [] }));
        },
        fromHandle: function (handle, typeName) {
            var entry = _proxyByHandle.get(handle);
            var existing = entry && entry.ref && entry.ref.deref();
            if (existing) return existing;
            var assembly = _resolveAssembly(typeName || '');
            var proxy = _makeDotNetInstance(handle, assembly, typeName || '');
            _track(proxy, handle, -1);
            return proxy;
        },
        // Arguments by parameter name, as the last argument: `F(x, named({ digits: 2 }))`.
        named: function (values) { return new _NamedArgs(values || {}); },
        // Loads an assembly (a path relative to the app, or absolute) and makes its namespaces globals.
        loadAssembly: function (path) {
            var map = JSON.parse(_bridgeCall('LoadAssembly', [String(path)]) || '{}');
            _assemblyCache = Object.create(null);
            for (var cached in _typeInfoCache) if (_typeInfoCache[cached] === _emptyInfo) delete _typeInfoCache[cached];
            for (var kind in _typeKindCache) if (_typeKindCache[kind] === '') delete _typeKindCache[kind];
            for (var ns in map) {
                if (typeof map[ns] !== 'string') continue;
                _namespaceAssemblyMap[ns] = map[ns];
                if (ns.indexOf('.') < 0 && /^[A-Za-z_$][\w$]*$/.test(ns)) this.registerNamespace(ns);
            }
            return Object.keys(map);
        },
        // Exposed so other IIFEs in this bootstrap (e.g. the managed-subclass/proxy block,
        // which runs in its own closure earlier in the script and has no direct access to
        // this IIFE's private `_wrap`) can wrap a raw tagged handle value the same way.
        wrap: _wrap,
        // Lazily register a namespace prefix and optional assembly mapping.
        // Example: NSWinRT.dotnet.registerNamespace('com', 'NativeScript');
        registerNamespace: function(rootName, assemblyName) {
            if (!rootName || typeof rootName !== 'string') return;
            var name = String(rootName);
            var root = name.split('.')[0];
            if (assemblyName && typeof assemblyName === 'string') {
                _assemblyCache = Object.create(null);
                _namespaceAssemblyMap[name] = assemblyName;
                if (!_namespaceAssemblyMap[root]) {
                    _namespaceAssemblyMap[root] = assemblyName;
                }
            }
            if (root in globalThis) return;
            try {
                Object.defineProperty(globalThis, root, {
                    configurable: true,
                    enumerable: true,
                    get: function() {
                        var proxy = _makeNamespaceProxy(root);
                        try { Object.defineProperty(globalThis, root, { value: proxy, writable: true, configurable: true, enumerable: true }); } catch (_) {}
                        return proxy;
                    },
                    // A namespace root never blocks app code from using the name for its own value
                    // (`var App = ...` in a classic script assigns through this accessor).
                    set: function(value) {
                        try { Object.defineProperty(globalThis, root, { value: value, writable: true, configurable: true, enumerable: true }); } catch (_) {}
                    }
                });
            } catch (_) {
                try { globalThis[root] = _makeNamespaceProxy(root); } catch (_) {}
            }
        },
        registerNamespaces: function(arr) {
            if (!Array.isArray(arr)) return;
            for (var i = 0; i < arr.length; i++) this.registerNamespace(arr[i]);
        },
    };

    // .NET types as JS classes
    // A .NET type proxy (System.Text.StringBuilder, MyApp.Native.Greeter) is a constructor with a
    // stable `prototype`, so it can be extended the way Java and Objective-C classes are on Android
    // and iOS:
    //   class Cat extends MyApp.Animal { constructor(n) { super(n); } Speak() { return super.Speak(); } }
    //   const Dog = MyApp.Animal.extend('Dog', { Speak() { return this.super.Speak(); } });
    //   const calc = new MyApp.ICalculator({ Compute(a, b) { return a * b; } });
    // Constructing a subclass creates the JS object first, then a managed subclass of the .NET type
    // whose overridden virtuals (and implemented interface members) call back into that object.
    // `super.X()` reaches the base implementation through the non-virtual `__ns_base_X` entry point
    // the bridge emits for every overridden member. Native code handing the instance back to JS gets
    // the same JS object.
    var _nsProxyCache = Object.create(null);
    var _namespaceExtras = { System: Object.create(null) };
    var _typeKindCache = Object.create(null);
    var _typeProtoCache = Object.create(null);
    var _subclassInfoCache = new WeakMap();
    var _jsSelfByHandle = new Map();

    // Lifetime of JS subclass instances
    // An instance is a JS object plus the managed object it drives. The JS object holds the managed
    // one through its handle; the managed object reaches the JS object only through its dispatcher,
    // which holds it weakly. So once JS drops the JS object, its handle is released and the managed
    // object goes as soon as .NET/native code drops it too (the bridge then unpins the dispatcher).
    // When native code calls into an instance whose JS object was collected, a new JS object of the
    // same class is made for it: fields its constructor set are gone, as on Android when a JS object
    // used only from native code is collected. Subclasses of WinRT UI classes are kept strongly while
    // they are in a live XAML tree.
    var _canWeakRef = typeof WeakRef === 'function';
    var _selfFinalizers = typeof FinalizationRegistry === 'function'
        ? new FinalizationRegistry(function (handle) {
            if (!_deref(_jsSelfByHandle.get(handle))) _jsSelfByHandle.delete(handle);
            try { _invoke({ handle: handle, method: '__release', args: [] }); } catch (_) {}
        })
        : null;

    function _weak(obj) { return _canWeakRef ? new WeakRef(obj) : obj; }
    function _deref(ref) { return ref && _canWeakRef && ref instanceof WeakRef ? ref.deref() : ref; }

    // Gives `self` the bridge identity in `raw` and registers it as the JS object of `entry`'s instance.
    function _adopt(entry, self, raw) {
        Object.defineProperty(self, '__handle', { value: raw.__handle });
        Object.defineProperty(self, '__nsOverrides', { value: entry.overrides });
        if (!entry.winrt) {
            Object.defineProperty(self, '__type', { value: raw.__type || entry.typeName });
            if (raw.__native_ptr !== undefined) Object.defineProperty(self, '__native_ptr', { value: raw.__native_ptr });
        }
        entry.ref = _weak(self);
        _jsSelfByHandle.set(raw.__handle, _weak(self));
        if (_selfFinalizers) _selfFinalizers.register(self, raw.__handle);
        if (entry.winrt) {
            entry.strong = self;
            _attachedEntries.add(entry);
        }
        _sweepAttached();
        return self;
    }

    // The JS object of `entry`'s instance, made anew when the previous one was collected.
    function _selfFor(entry, raw) {
        var self = _deref(entry.ref);
        if (self) return self;
        if (entry.constructing) throw new Error(entry.typeName + ' called into its JS subclass before the instance existed');
        if (!raw || typeof raw.__handle !== 'number') throw new Error('The ' + entry.typeName + ' subclass instance is gone');
        if (entry.winrt) {
            self = globalThis.__nsWrapWinRTPointer(raw.__native_ptr, entry.typeName);
            Object.setPrototypeOf(self, entry.proto);
        } else {
            self = Object.create(entry.proto);
        }
        return _adopt(entry, self, raw);
    }

    // The dispatcher native code calls the instance's overridden and interface members through.
    function _makeDispatcher(entry) {
        return function (selfHandle, member) {
            var self = _selfFor(entry, selfHandle);
            if (member === '__ns_js_self') return undefined;
            _sweepAttached();
            var margs = Array.prototype.slice.call(arguments, 2).map(_wrap);
            return _dispatchToJs(self, entry.baseProto, entry.typeName, member, margs);
        };
    }

    // Subclasses of WinRT classes held strongly (entry.strong) while in a live XAML tree, re-checked at
    // most every two seconds when subclass instances are created or called into.
    var _attachedEntries = new Set();
    var _lastAttachedSweep = 0;
    function _sweepAttached() {
        var now = Date.now();
        if (now - _lastAttachedSweep < 2000) return;
        _lastAttachedSweep = now;
        _attachedEntries.forEach(function (entry) {
            var self = _deref(entry.ref);
            if (!self) {
                _attachedEntries.delete(entry);
                return;
            }
            var attached = false;
            try { attached = self.XamlRoot != null; } catch (_) {}
            entry.strong = attached ? self : undefined;
        });
    }
    var _baseCallPrefix = '__ns_base_';

    function _has(list, name) {
        return !!list && list.indexOf(name) >= 0;
    }

    function _typeKind(path) {
        var kind = _typeKindCache[path];
        if (kind !== undefined) return kind;
        try {
            kind = _invoke({ assembly: '', typeName: 'NativeScriptBridge.Bridge', method: 'TypeKind', args: [path] }) || '';
        } catch (_) {
            kind = '';
        }
        _typeKindCache[path] = kind;
        return kind;
    }

    // Full name of a .NET type proxy, or of the .NET base of a JS class deriving from one.
    function _dotnetTypeName(type) {
        if (typeof type === 'string') return type;
        if (!type || (typeof type !== 'function' && typeof type !== 'object')) return '';
        var name = type.__dotnetType__;
        return typeof name === 'string' ? name : '';
    }

    // The handle of an instance of a JS subclass (an own property); -1 for anything else, such as
    // a class prototype further down the chain, without running any property trap.
    function _ownHandle(o) {
        if (!o || (typeof o !== 'object' && typeof o !== 'function')) return -1;
        var d = Object.getOwnPropertyDescriptor(o, '__handle');
        return d && typeof d.value === 'number' ? d.value : -1;
    }

    function _baseCallName(self, member) {
        var overrides = self.__nsOverrides;
        return overrides && overrides.has(member) ? _baseCallPrefix + member : member;
    }

    // A native member of `self` (an instance of a JS subclass), reached through the .NET type's
    // prototype: properties are read through their getter, methods become functions. Members the JS
    // class overrides resolve to the base implementation: only `super.X` gets this far for them.
    function _baseMember(path, self, prop) {
        if (prop === 'toString') return function () { return String(_invoke({ handle: self.__handle, method: 'ToString', args: [] })); };
        if (prop === 'valueOf') return function () { return this; };
        var info = _getTypeInfo(_resolveAssembly(path), path);
        var overrides = self.__nsOverrides;
        if (_has(info.properties, prop) || _has(info.protectedProperties, prop) || (overrides && overrides.has('get_' + prop))) {
            return _wrap(_invoke({ handle: self.__handle, method: _baseCallName(self, 'get_' + prop), args: [] }));
        }
        if (_has(info.writeonlyProperties, prop))
            throw new TypeError('Cannot read write-only property \'' + prop + '\' of .NET type \'' + path + '\'');
        if (_has(info.methods, prop) || _has(info.protectedMethods, prop) || (overrides && overrides.has(prop))) {
            var method = _baseCallName(self, prop);
            return function () {
                var args = Array.prototype.slice.call(arguments).map(_unwrap);
                return _wrap(_invoke({ handle: self.__handle, method: method, args: args }));
            };
        }
        return undefined;
    }

    // The `prototype` of a .NET type: the end of every JS subclass's prototype chain. Reads resolve
    // native members against the receiving instance; writes go to native properties, and anything
    // else becomes a plain JS field of the instance.
    function _typePrototype(path) {
        var cached = _typeProtoCache[path];
        if (cached) return cached;
        var proto = new Proxy(Object.create(null), {
            get: function (_, prop, receiver) {
                if (typeof prop === 'symbol') return undefined;
                if (_ownHandle(receiver) < 0) {
                    // Read off the prototype itself (TypeScript's ES5 super call
                    // `_super.prototype.Method.call(this, ...)`): a function that runs the base
                    // implementation on whichever instance it is called with.
                    var typeInfo = _getTypeInfo(_resolveAssembly(path), path);
                    if (!_has(typeInfo.methods, prop)) return undefined;
                    return function () {
                        if (_ownHandle(this) < 0) throw new TypeError(path + '.prototype.' + prop + ' called on an object that is not a ' + path);
                        return _baseMember(path, this, prop).apply(this, arguments);
                    };
                }
                return _baseMember(path, receiver, prop);
            },
            set: function (_, prop, value, receiver) {
                if (typeof prop !== 'symbol' && _ownHandle(receiver) >= 0) {
                    var info = _getTypeInfo(_resolveAssembly(path), path);
                    if (_has(info.readonlyProperties, prop))
                        throw new TypeError('Cannot assign to read-only property \'' + prop + '\' of .NET type \'' + path + '\'');
                    if (_has(info.properties, prop) || _has(info.writeonlyProperties, prop) || _has(info.protectedProperties, prop)) {
                        _invoke({ handle: receiver.__handle, method: _baseCallName(receiver, 'set_' + prop), args: [_unwrap(value)] });
                        return true;
                    }
                }
                Object.defineProperty(receiver, prop, { value: value, writable: true, enumerable: true, configurable: true });
                return true;
            },
        });
        _typeProtoCache[path] = proto;
        return proto;
    }

    // Members (literal CLR names: methods, get_X/set_X accessors) and interfaces a JS class adds on
    // top of its .NET base, from every class between it and the base.
    function _subclassInfo(proto, baseProto) {
        var cached = _subclassInfoCache.get(proto);
        if (cached) return cached;
        var members = [];
        var overrides = new Set();
        var interfaces = [];
        var seen = Object.create(null);
        function addInterfaces(list) {
            if (!Array.isArray(list)) return;
            for (var i = 0; i < list.length; i++) {
                var name = _dotnetTypeName(list[i]) || (list[i] && (list[i].__typeName__ || list[i].name)) || '';
                if (name && interfaces.indexOf(name) < 0) interfaces.push(name);
            }
        }
        for (var p = proto; p && p !== baseProto; p = Object.getPrototypeOf(p)) {
            var keys = Object.getOwnPropertyNames(p);
            for (var i = 0; i < keys.length; i++) {
                var key = keys[i];
                if (key === 'constructor' || key === 'interfaces' || key === 'super' || key === 'init' || seen[key]) continue;
                seen[key] = true;
                var d = Object.getOwnPropertyDescriptor(p, key);
                if (typeof d.get === 'function') { members.push('get_' + key); overrides.add('get_' + key); }
                if (typeof d.set === 'function') { members.push('set_' + key); overrides.add('set_' + key); }
                if (typeof d.value === 'function') { members.push(key); overrides.add(key); }
            }
            if (Object.prototype.hasOwnProperty.call(p, 'interfaces')) addInterfaces(p.interfaces);
            var ctor = Object.prototype.hasOwnProperty.call(p, 'constructor') ? p.constructor : null;
            if (ctor && Object.prototype.hasOwnProperty.call(ctor, 'interfaces')) addInterfaces(ctor.interfaces);
        }
        var info = { members: members, overrides: overrides, interfaces: interfaces };
        _subclassInfoCache.set(proto, info);
        return info;
    }

    // The JS-defined member `key` of `self` (an own field, or a member of one of its JS classes).
    function _jsMember(self, key, baseProto) {
        for (var o = self; o && o !== baseProto; o = Object.getPrototypeOf(o)) {
            var d = Object.getOwnPropertyDescriptor(o, key);
            if (d) return d;
        }
        return null;
    }

    // Native code calling an overridden member (or an implemented interface member) of `self`.
    function _dispatchToJs(self, baseProto, typeName, member, args) {
        var accessor = /^(get|set)[_:](.+)$/.exec(member);
        if (accessor) {
            var ad = _jsMember(self, accessor[2], baseProto);
            if (ad) {
                if (accessor[1] === 'get') {
                    if (typeof ad.get === 'function') return ad.get.call(self);
                    if ('value' in ad && typeof ad.value !== 'function') return ad.value;
                } else if (typeof ad.set === 'function') {
                    ad.set.call(self, args[0]);
                    return undefined;
                } else if ('value' in ad && ad.writable) {
                    self[accessor[2]] = args[0];
                    return undefined;
                }
            }
        }
        var md = _jsMember(self, member, baseProto);
        if (md && typeof md.value === 'function') return md.value.apply(self, args);
        throw new Error((self.constructor && self.constructor.name || typeName) + ' does not implement ' + member);
    }

    function _constructSubclass(path, args, proto) {
        if (typeof globalThis.__nsDotNetCreateJsSubclass !== 'function')
            throw new Error('Extending .NET types is not available in this runtime');
        var baseProto = _typePrototype(path);
        var info = _subclassInfo(proto, baseProto);
        var self = Object.create(proto);
        // A virtual the base constructor calls reaches `self` before it has its handle.
        var entry = { proto: proto, baseProto: baseProto, typeName: path, overrides: info.overrides, winrt: false, ref: self };
        var raw = globalThis.__nsDotNetCreateJsSubclass('', path, info.interfaces, info.members, _makeDispatcher(entry), args.map(_unwrap));
        if (!raw || typeof raw.__handle !== 'number') throw new Error('Could not create an instance of a subclass of ' + path);
        return _adopt(entry, self, raw);
    }

    // `new SomeInterface({ ... })`: an object implementing the interface with the given members.
    function _implementInterface(path, impl) {
        var proto = Object.create(_typePrototype('System.Object'));
        Object.defineProperties(proto, Object.getOwnPropertyDescriptors(impl));
        Object.defineProperty(proto, 'interfaces', { value: [path] });
        return _constructSubclass('System.Object', [], proto);
    }

    // `this.super` inside a class made by extend(): the parent class's members bound to `self`.
    function _superOf(self, Cls) {
        var parent = Object.getPrototypeOf(Cls.prototype);
        return new Proxy(Object.create(null), {
            get: function (_, prop) {
                var value = Reflect.get(parent, prop, self);
                return typeof value === 'function' ? value.bind(self) : value;
            },
        });
    }

    // Base.extend([name,] members): an ES2015 subclass of Base with the given members; `init`, when
    // present, runs after construction with the constructor arguments.
    function _extendClass(Base, nameOrMembers, maybeMembers) {
        var hasName = typeof nameOrMembers === 'string';
        var members = (hasName ? maybeMembers : nameOrMembers) || {};
        var Extended = class extends Base {
            constructor() {
                super(...arguments);
                if (typeof members.init === 'function') members.init.apply(this, arguments);
            }
        };
        var descriptors = Object.getOwnPropertyDescriptors(members);
        delete descriptors.init;
        if (descriptors.interfaces) {
            Object.defineProperty(Extended.prototype, 'interfaces', { value: members.interfaces });
            delete descriptors.interfaces;
        }
        Object.defineProperties(Extended.prototype, descriptors);
        Object.defineProperty(Extended.prototype, 'super', {
            get: function () { return _superOf(this, Extended); },
        });
        if (hasName) Object.defineProperty(Extended, 'name', { value: nameOrMembers.split('.').pop() });
        return Extended;
    }

    // `_super.call(this, ...)` / `_super.apply(this, args)` in TypeScript's ES5 output (what the
    // @NativeClass transform emits): construct the subclass instance `thisArg` was created for.
    function _es5SuperCall(path, thisArg, args) {
        var proto = thisArg && Object.getPrototypeOf(thisArg);
        if (!proto || !Object.prototype.isPrototypeOf.call(_typePrototype(path), proto))
            throw new TypeError('Class constructor ' + path + ' cannot be invoked without \'new\'');
        return _constructSubclass(path, args, proto);
    }

    function _hasInstance(path, value) {
        if (value === null || (typeof value !== 'object' && typeof value !== 'function')) return false;
        if (Object.prototype.isPrototypeOf.call(_typePrototype(path), value)) return true;
        if (typeof value.__handle !== 'number') return false;
        try {
            return _invoke({ assembly: '', typeName: 'NativeScriptBridge.Bridge', method: 'IsInstanceOf', args: [{ __handle: value.__handle }, path] }) === true;
        } catch (_) {
            return false;
        }
    }

    // WinRT classes as JS base classes
    // `class FixedPanel extends Microsoft.UI.Xaml.Controls.Panel { MeasureOverride(size) { ... } }`:
    // the WinRT constructor hands `super(...)` here. The instance is a composed object created by
    // the .NET bridge from the C#/WinRT projection of the class (the way a C# subclass is), so XAML
    // calling an overridable member (MeasureOverride, ArrangeOverride, OnApplyTemplate, ...) lands in
    // the JS override. The JS object is the ordinary WinRT wrapper of that object, with the subclass
    // prototype spliced in front of the class's native members.
    var _linkedWinRTPrototypes = new WeakSet();

    // The WinRT class a JS class derives from: the nearest constructor with its own __typeName__.
    function _winrtBaseOf(ctor) {
        for (var c = ctor; c; c = Object.getPrototypeOf(c)) {
            if (Object.prototype.hasOwnProperty.call(c, '__typeName__')) return c;
        }
        return null;
    }

    // `super.X(...)` for a member a JS subclass of a WinRT class overrides: the base implementation,
    // through the bridge (the native member would dispatch back into the override).
    function _winrtBaseCall(handle, prop) {
        return function () {
            var args = Array.prototype.slice.call(arguments).map(_unwrap);
            return _wrap(_invoke({ handle: handle, method: _baseCallPrefix + prop, args: args }));
        };
    }

    // Puts a layer under the JS subclass prototypes that resolves the class's native members and
    // sends `super.X()` for overridden members to the base implementation. On the classic engine the
    // class's `prototype` is separate from its instances' native prototype, so the layer goes under the
    // class's `prototype` once; on the napi engines they're the same object, so it goes under the
    // subclass prototype that sits directly on it.
    function _linkWinRTSubclassPrototype(base, nativeProto, proto) {
        if (!base || !nativeProto) return;
        var target = base.prototype;
        if (target === nativeProto) {
            target = proto;
            while (target && Object.getPrototypeOf(target) !== nativeProto) target = Object.getPrototypeOf(target);
        }
        if (!target || _linkedWinRTPrototypes.has(target)) return;
        _linkedWinRTPrototypes.add(target);
        var layer = new Proxy(Object.create(nativeProto), {
            get: function (_, prop, receiver) {
                var handle = _ownHandle(receiver);
                if (typeof prop !== 'string') return Reflect.get(nativeProto, prop, receiver);
                if (handle >= 0) {
                    if (receiver.__nsOverrides && receiver.__nsOverrides.has(prop)) return _winrtBaseCall(handle, prop);
                    return Reflect.get(nativeProto, prop, receiver);
                }
                // Read off a prototype (TypeScript's ES5 `_super.prototype.X.call(this, ...)`): decide
                // between the base implementation and the native member when it's called.
                var native = Reflect.get(nativeProto, prop, receiver);
                if (typeof native !== 'function') return native;
                return function () {
                    var h = _ownHandle(this);
                    if (h >= 0 && this.__nsOverrides && this.__nsOverrides.has(prop))
                        return _winrtBaseCall(h, prop).apply(this, arguments);
                    return native.apply(this, arguments);
                };
            },
            has: function (_, prop) { return prop in nativeProto; },
        });
        Object.setPrototypeOf(target, layer);
    }

    // napi engines: the class `prototype` is the native prototype itself, so TypeScript's ES5
    // `_super.prototype.X.call(this, ...)` reaches the native member directly. Wrap each native member a
    // subclass overrides (once) so that, called on a JS subclass instance, it runs the base
    // implementation instead of dispatching back into the override.
    function _guardNativeMembers(nativeProto, overrides) {
        overrides.forEach(function (name) {
            for (var p = nativeProto; p; p = Object.getPrototypeOf(p)) {
                var d = Object.getOwnPropertyDescriptor(p, name);
                if (!d) continue;
                if (typeof d.value === 'function' && !d.value.__nsGuarded && d.writable) {
                    var native = d.value;
                    var guarded = function () {
                        var h = _ownHandle(this);
                        if (h >= 0 && this.__nsOverrides && this.__nsOverrides.has(name))
                            return _winrtBaseCall(h, name).apply(this, arguments);
                        return native.apply(this, arguments);
                    };
                    Object.defineProperty(guarded, '__nsGuarded', { value: true });
                    Object.defineProperty(p, name, { value: guarded, writable: true, configurable: true, enumerable: d.enumerable });
                }
                return;
            }
        });
    }

    // Called by the WinRT class constructors for `super(...)` from a JS subclass (newTarget) or
    // TypeScript's ES5 `_super.call(this, ...)` (es5This). Returns undefined when this isn't a subclass
    // construction or the .NET bridge isn't available; the constructor then builds a plain instance.
    globalThis.__nsConstructWinRTSubclass = function (typeName, newTarget, args, es5This) {
        var proto;
        var base;
        if (newTarget) {
            proto = newTarget.prototype;
            base = _winrtBaseOf(Object.getPrototypeOf(newTarget));
        } else {
            proto = es5This && Object.getPrototypeOf(es5This);
            base = proto && proto.constructor ? _winrtBaseOf(Object.getPrototypeOf(proto.constructor)) : null;
            if (!base || base.__typeName__ !== typeName || Object.prototype.hasOwnProperty.call(proto.constructor, '__typeName__'))
                return undefined;
        }
        if (typeof globalThis.__nsDotNetCreateJsSubclass !== 'function' || typeof globalThis.__nsWrapWinRTPointer !== 'function')
            return undefined;
        var baseProto = base ? base.prototype : null;
        var info = _subclassInfo(proto, baseProto);
        var entry = { proto: proto, baseProto: baseProto, typeName: typeName, overrides: info.overrides, winrt: true, constructing: true };
        var raw = globalThis.__nsDotNetCreateJsSubclass('', typeName, info.interfaces, info.members, _makeDispatcher(entry), (args || []).map(_unwrap));
        entry.constructing = false;
        if (!raw || raw.__native_ptr === undefined || raw.__native_ptr === null)
            throw new Error('Could not create a WinRT object for a subclass of ' + typeName);
        var self = globalThis.__nsWrapWinRTPointer(raw.__native_ptr, typeName);
        var nativeProto = Object.getPrototypeOf(self);
        _linkWinRTSubclassPrototype(base, nativeProto, proto);
        if (base && base.prototype === nativeProto) _guardNativeMembers(nativeProto, info.overrides);
        Object.setPrototypeOf(self, proto);
        return _adopt(entry, self, raw);
    };

    globalThis.NSWinRT.extendClass = _extendClass;

    // System.Diagnostics.Stopwatch.StartNew()    →  static method call
    // System.Environment.MachineName             →  static property get
    // new System.Text.StringBuilder(64)          →  constructor
    // sw.Stop()                                  →  instance method
    // sw.Elapsed                                 →  instance property
    function _makeNamespaceProxy(path) {
        var cached = _nsProxyCache[path];
        if (cached) return cached;
        // Static method functions and child namespace/type proxies by name, made on first access.
        var members = Object.create(null);
        // Handlers assigned to static events, by event name.
        var assigned = null;
        // Plain JS fields assigned to the namespace or type, made on first assignment.
        var fields = null;
        function _node() {}
        var proxy = new Proxy(_node, {
            get: function (_, prop) {
                if (prop === Symbol.hasInstance) return function (value) { return _hasInstance(path, value); };
                if (typeof prop === 'symbol') return undefined;
                // Prevent string-coercion methods from descending into sub-proxies.
                // V8's console.log / JSON.stringify call toString/valueOf on unknown objects;
                // returning a sub-proxy causes the apply trap to fire with a non-existent
                // type name, producing a spurious "Type not found" bridge error.
                if (prop === 'toString' || prop === 'valueOf')
                    return function() { return '[.NET ' + path + ']'; };
                var extras = _namespaceExtras[path];
                if (extras && Object.prototype.hasOwnProperty.call(extras, prop)) return extras[prop];
                if (fields !== null && prop in fields) return fields[prop];
                var member = members[prop];
                if (member !== undefined) return member;
                if (prop === '__dotnetType__') return path;
                if (prop === 'prototype') return _typePrototype(path);
                // Not a WinRT class: TypeScript's __extends must take the standard path.
                if (prop === '__nsWinRTClass__' || prop === 'then') return undefined;
                var assembly = _resolveAssembly(path);
                var kind = _kindsOf(_getTypeInfo(assembly, path))[prop] | 0;
                // Write-only static property: reading it is an error.
                if (kind & _K_SWO)
                    throw new TypeError('Cannot read write-only property \'' + prop + '\' of .NET type \'' + path + '\'');
                // Readable static property: resolve value immediately.
                if (kind & _K_SPROP)
                    return _wrap(_invoke({ assembly: assembly, typeName: path, method: _getterName(prop), args: [] }));
                if (kind & _K_SEVENT) return (assigned && assigned[prop]) || null;
                // Static method: a function, made once.
                if (kind & _K_SMETHOD) {
                    return members[prop] = _withTypeArgs(function () {
                        return _wrap(_invokeWith(-1, path, _resolveAssembly(path), prop, arguments));
                    }, function (suffix, args) {
                        return _wrap(_invokeWith(-1, path, _resolveAssembly(path), prop + suffix, args));
                    });
                }
                var ev = _eventOf(prop, _kindsOf(_getTypeInfo(assembly, path)), _K_SEVENT);
                if (ev !== null) return members[prop] = _eventAccessor(prop.charCodeAt(0) === 97, -1, path, ev);
                // `List.of(System.String)`: the closed generic type.
                if (prop === 'of') return function () {
                    var args = _typeArgList(arguments);
                    return _makeNamespaceProxy(path + '`' + arguments.length + '[' + args + ']');
                };
                if (prop === 'extend') return function (nameOrMembers, maybeMembers) { return _extendClass(this, nameOrMembers, maybeMembers); };
                if (prop === 'call') return function (thisArg) { return _es5SuperCall(path, thisArg, Array.prototype.slice.call(arguments, 1)); };
                if (prop === 'apply') return function (thisArg, args) { return _es5SuperCall(path, thisArg, args ? Array.prototype.slice.call(args) : []); };
                // Namespace / sub-type: keep descending.
                return members[prop] = _makeNamespaceProxy(path + '.' + prop);
            },
            set: function (_, prop, value) {
                if (typeof prop === 'symbol') return true;
                var assembly = _resolveAssembly(path);
                var kind = _kindsOf(_getTypeInfo(assembly, path))[prop] | 0;
                // Read-only static property: assignment is an error.
                if (kind & _K_SRO)
                    throw new TypeError('Cannot assign to read-only property \'' + prop + '\' of .NET type \'' + path + '\'');
                // Writable (read-write or write-only): invoke the setter.
                if (kind & (_K_SPROP | _K_SWO)) {
                    _invoke({ assembly: assembly, typeName: path, method: _setterName(prop), args: [_unwrap(value)] });
                    return true;
                }
                if (kind & _K_SEVENT) {
                    _assignEvent(-1, path, prop, value);
                    (assigned || (assigned = Object.create(null)))[prop] = value || null;
                    return true;
                }
                // Not a native property: a plain JS field.
                (fields || (fields = Object.create(null)))[prop] = value;
                return true;
            },
            apply: function (_, _this, args) {
                var lastDot  = path.lastIndexOf('.');
                // No dot means this is a top-level name being called directly:
                // there is no type+method pair to dispatch, so bail out.
                if (lastDot <= 0) return undefined;
                var typeName = path.substring(0, lastDot);
                var method   = path.substring(lastDot + 1);
                var assembly = _resolveAssembly(typeName);
                return _wrap(_invoke({ assembly: assembly, typeName: typeName, method: method, args: args.map(_unwrap) }));
            },
            construct: function (_, args, newTarget) {
                // `super(...)` from a JS subclass.
                if (newTarget && newTarget !== proxy) return _constructSubclass(path, args, newTarget.prototype);
                if (args.length === 1 && args[0] && typeof args[0] === 'object' && typeof args[0].__handle !== 'number'
                    && _typeKind(path) === 'interface')
                    return _implementInterface(path, args[0]);
                return _wrap(_invokeWith(-1, path, _resolveAssembly(path), '.ctor', args));
            },
        });
        _nsProxyCache[path] = proxy;
        return proxy;
    }
    // `System` stays the .NET namespace when app code assigns it: @nativescript/core sets
    // `global.System = { import() }` (a SystemJS shim). Assigned members are kept and take
    // precedence (`System.import(...)` keeps working); everything else is .NET.
    (function () {
        var systemProxy = _makeNamespaceProxy('System');
        Object.defineProperty(globalThis, 'System', {
            configurable: true,
            enumerable: true,
            get: function () { return systemProxy; },
            set: function (value) {
                if (value && (typeof value === 'object' || typeof value === 'function'))
                    Object.keys(value).forEach(function (k) { _namespaceExtras.System[k] = value[k]; });
            },
        });
    })();
    globalThis.Microsoft      = _makeNamespaceProxy('Microsoft');
    globalThis.NativeScript   = _makeNamespaceProxy('NativeScript');

    // Every other root namespace of the app's assemblies (C# sources compiled into the app,
    // plugin and NuGet assemblies) is a lazy global too, so `new MyCompany.Native.Foo()` works
    // with no registration step, the way Java packages and Objective-C classes are globals on
    // Android and iOS. WinRT owns Windows/Microsoft/NativeScript; the rest are implementation
    // namespaces of the toolchain that no app reaches for.
    (function () {
        var reserved = { Windows: 1, Microsoft: 1, NativeScript: 1, System: 1, NativeScriptBridge: 1, NSWinRTDynamicProxies: 1,
            WinRT: 1, ABI: 1, XamlGeneratedNamespace: 1, FxResources: 1, Internal: 1, Interop: 1, MS: 1, Mono: 1 };
        for (var ns in _namespaceAssemblyMap) {
            if (ns.indexOf('.') !== -1 || reserved[ns] === 1 || !/^[A-Za-z_$][\w$]*$/.test(ns)) continue;
            globalThis.NSWinRT.dotnet.registerNamespace(ns);
        }
    })();

    // Creates a typed .NET BCL delegate (not a WinRT COM delegate).
    // Use this when the API expects a managed System.Action,
    // System.EventHandler, etc. rather than a WinRT delegate interface.
    // For WinRT delegate types use NSWinRT.asDelegate instead.
    if (typeof globalThis.__nsDotNetAwaitTask === 'function') {
        globalThis.NSWinRT.dotnet.taskToPromise = function (obj) {
            var h = obj && typeof obj.__handle === 'number' && obj.__isTask === true ? obj.__handle : -1;
            if (h < 0) return Promise.resolve(obj);
            return new Promise(function (resolve, reject) {
                globalThis.__nsDotNetAwaitTask(h, function (value) { resolve(_wrap(value)); }, function (error) { reject(_errorFromException(error)); });
            });
        };
    }

    if (typeof globalThis.__nsDotNetCreateDelegate === 'function') {
        globalThis.NSWinRT.dotnet.asDelegate = function(typeNameOrFn, fn) {
            var typeName, callback;
            if (typeof typeNameOrFn === 'function') {
                typeName = '';
                callback = typeNameOrFn;
            } else {
                typeName = typeNameOrFn || '';
                callback = fn;
            }
            if (typeof callback !== 'function')
                throw new TypeError('NSWinRT.dotnet.asDelegate: callback must be a function');
            var wrapped = function() {
                var args = Array.prototype.slice.call(arguments).map(_wrap);
                return callback.apply(null, args);
            };
            return globalThis.__nsDotNetCreateDelegate(typeName, wrapped);
        };
    }

    // Uncaught .NET exceptions go to __onUncaughtError; faulted Tasks nobody awaited go to
    // __onDiscardedError.
    try {
        _bridgeCall('RegisterUnhandledExceptionHandler', [_callbackFor(function (kind, ex) {
            var err = _errorFromException(ex);
            var hook = kind === 'unobservedTask' ? globalThis.__onDiscardedError : globalThis.__onUncaughtError;
            if (typeof hook === 'function') hook(err);
            else if (typeof console !== 'undefined') console.error(err);
        })]);
    } catch (_) {}

    if (typeof globalThis.__nsRunOnUIThread === 'function') {
        globalThis.NSWinRT.runOnUIThread = function(fn) {
            if (typeof fn !== 'function') throw new TypeError('NSWinRT.runOnUIThread: expected a function');
            return globalThis.__nsRunOnUIThread(fn);
        };
    }
})();
