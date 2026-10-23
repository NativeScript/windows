// JS subclasses of .NET and WinRT classes (extend, @NativeClass, NSWinRT.proxy).
// Shared by both engines: global_fns.rs HELPER_SOURCE (classic) and napi_engine/dotnet.rs.
// Runs inside the caller's function scope, so its vars stay private.

var proxyExtensions = [];
var proxyInstances = new Map();
var nextProxyId = 1;

function ctorName(ctor) {
    return (ctor && (ctor.__winrtProxyName__ || ctor.__typeName__ || ctor.name)) || 'Object';
}

// This IIFE runs before the NSWinRT.dotnet block below in script order, but by the
// time createManagedSubclass is actually CALLED (well after the whole bootstrap has
// finished evaluating) NSWinRT.dotnet.wrap already exists whenever
// __nsDotNetCreateJsSubclass does (both are registered together). Wraps a raw tagged
// handle value into a live DotNet instance proxy exactly like the dotnet block's own
// private `_wrap`, without needing access to that other IIFE's closure.
function _wrapDotNetHandle(value) {
    var dotnet = globalThis.NSWinRT && globalThis.NSWinRT.dotnet;
    return (dotnet && typeof dotnet.wrap === 'function') ? dotnet.wrap(value) : value;
}

var typeDescriptorCache = Object.create(null);

function describeWinRTType(typeName) {
    if (!typeName || typeof globalThis.__nsDescribeWinRTType !== 'function') {
        return null;
    }
    if (Object.prototype.hasOwnProperty.call(typeDescriptorCache, typeName)) {
        return typeDescriptorCache[typeName];
    }
    try {
        var raw = globalThis.__nsDescribeWinRTType(typeName);
        typeDescriptorCache[typeName] = raw ? JSON.parse(raw) : null;
    } catch (_) {
        typeDescriptorCache[typeName] = null;
    }
    return typeDescriptorCache[typeName];
}

function buildFallbackParameterMetadata(fn) {
    var params = [];
    var count = typeof fn === 'function' && Number.isFinite(fn.length) ? fn.length : 0;
    for (var i = 0; i < count; i++) {
        params.push({ name: 'arg' + i, type: 'Object' });
    }
    return params;
}

function normalizeMethodMetadata(name, value, descriptors) {
    for (var i = 0; i < descriptors.length; i++) {
        var descriptor = descriptors[i];
        if (!descriptor || !Array.isArray(descriptor.methods)) {
            continue;
        }
        for (var j = 0; j < descriptor.methods.length; j++) {
            var method = descriptor.methods[j];
            if (method && method.name === name) {
                return {
                    name: method.name,
                    returnType: method.returnType || method.return_type || 'Void',
                    parameters: Array.isArray(method.parameters) ? method.parameters : [],
                };
            }
        }
    }

    return {
        name: name,
        returnType: name === 'init' ? 'Void' : 'Object',
        parameters: buildFallbackParameterMetadata(value),
    };
}

function normalizePropertyMetadata(name, descriptors) {
    for (var i = 0; i < descriptors.length; i++) {
        var descriptor = descriptors[i];
        if (!descriptor || !Array.isArray(descriptor.properties)) {
            continue;
        }
        for (var j = 0; j < descriptor.properties.length; j++) {
            var property = descriptor.properties[j];
            if (property && property.name === name) {
                return {
                    name: property.name,
                    propType: property.propType || property.prop_type || 'Object',
                    readable: property.readable !== false,
                    writable: property.writable !== false,
                };
            }
        }
    }

    return {
        name: name,
        propType: 'Object',
        readable: true,
        writable: true,
    };
}

function collectProxyMethods(overrides, descriptors) {
    var methods = [];
    for (var key in overrides) {
        if (!Object.prototype.hasOwnProperty.call(overrides, key)) {
            continue;
        }
        if (key === 'interfaces') {
            continue;
        }
        if (typeof overrides[key] === 'function') {
            methods.push(normalizeMethodMetadata(key, overrides[key], descriptors));
        }
    }
    methods.sort(function (left, right) {
        return String(left && left.name || '').localeCompare(String(right && right.name || ''));
    });
    return methods;
}

function collectProxyProperties(overrides, descriptors) {
    var props = [];
    for (var key in overrides) {
        if (!Object.prototype.hasOwnProperty.call(overrides, key)) {
            continue;
        }
        if (key === 'interfaces') {
            continue;
        }
        if (typeof overrides[key] !== 'function') {
            props.push(normalizePropertyMetadata(key, descriptors));
        }
    }
    props.sort(function (left, right) {
        return String(left && left.name || '').localeCompare(String(right && right.name || ''));
    });
    return props;
}

function safeIdentifier(name) {
    return String(name || '')
        .replace(/[^A-Za-z0-9_]/g, '_')
        .replace(/^([^A-Za-z_])/, '_$1') || 'ProxyType';
}

function autoProxyTypeName(baseCtor) {
    var baseType = ctorName(baseCtor) || 'Object';
    var baseShort = safeIdentifier(baseType.split('.').pop());
    return 'NativeScript.Gen.' + baseShort + '_' + (proxyExtensions.length + 1);
}

function renderProxyCSharp(meta) {
    var typeName = meta.typeName || ('GeneratedProxy' + (proxyExtensions.length + 1));
    var safeTypeName = safeIdentifier(typeName.split('.').pop());
    var baseType = meta.baseType || 'object';
    var methodStubs = '';
    for (var i = 0; i < meta.methods.length; i++) {
        var methodMeta = meta.methods[i];
        var methodName = safeIdentifier((methodMeta && methodMeta.name) || methodMeta);
        methodStubs +=
            '    public object __ns_' + methodName + '(params object[] args)\\n' +
            '    {\\n' +
            '        return ProxyDispatcher.Invoke(this.__proxyId, "' + methodName + '", args);\\n' +
            '    }\\n\\n';
    }

    return (
        'using System;\\n\\n' +
        'namespace NativeScriptGeneratedProxies\\n' +
        '{\\n' +
        '    public static class ProxyDispatcher\\n' +
        '    {\\n' +
        '        public static Func<int, string, object[], object> JsInvoke;\\n' +
        '        public static object Invoke(int id, string method, object[] args)\\n' +
        '        {\\n' +
        '            var cb = JsInvoke;\\n' +
        '            if (cb == null) throw new InvalidOperationException("JsInvoke callback is not registered.");\\n' +
        '            return cb(id, method, args);\\n' +
        '        }\\n' +
        '    }\\n\\n' +
        '    public class ' + safeTypeName + ' : ' + baseType + '\\n' +
        '    {\\n' +
        '        private readonly int __proxyId;\\n\\n' +
        '        public ' + safeTypeName + '(int proxyId)\\n' +
        '        {\\n' +
        '            this.__proxyId = proxyId;\\n' +
        '        }\\n\\n' +
        methodStubs +
        '    }\\n' +
        '}\\n'
    );
}

function renderProxyCsproj(meta) {
    var asmName = safeIdentifier((meta.typeName || 'GeneratedProxy').split('.').pop());
    return (
        '<Project Sdk="Microsoft.NET.Sdk">\\n' +
        '  <PropertyGroup>\\n' +
        '    <TargetFramework>net8.0-windows10.0.19041.0</TargetFramework>\\n' +
        '    <AssemblyName>' + asmName + '</AssemblyName>\\n' +
        '    <RootNamespace>NativeScriptGeneratedProxies</RootNamespace>\\n' +
        '    <ImplicitUsings>enable</ImplicitUsings>\\n' +
        '    <Nullable>disable</Nullable>\\n' +
        '    <LangVersion>latest</LangVersion>\\n' +
        '  </PropertyGroup>\\n' +
        '</Project>\\n'
    );
}

function buildProxyMetadata(baseCtor, typeName, overrides, Extended) {
    var baseType = ctorName(baseCtor);
    var interfaceNames = Array.isArray(overrides.interfaces)
        ? overrides.interfaces.map(function (iface) { return ctorName(iface); })
        : [];
    var descriptors = [describeWinRTType(baseType)];
    for (var i = 0; i < interfaceNames.length; i++) {
        descriptors.push(describeWinRTType(interfaceNames[i]));
    }
    var namespace = '';
    var className = typeName || '';
    if (typeName) {
        var splitIndex = typeName.lastIndexOf('.');
        if (splitIndex >= 0) {
            namespace = typeName.slice(0, splitIndex);
            className = typeName.slice(splitIndex + 1);
        }
    }
    var meta = {
        kind: 'windows-proxy',
        typeName: typeName || '',
        className: className || safeIdentifier((typeName || baseType || 'GeneratedProxy').split('.').pop()),
        namespace: namespace || null,
        baseType: baseType,
        baseClass: baseType,
        interfaces: interfaceNames,
        methods: collectProxyMethods(overrides, descriptors.filter(Boolean)),
        properties: collectProxyProperties(overrides, descriptors.filter(Boolean)),
        isAutoGeneratedName: !typeName,
        registeredAt: new Date().toISOString(),
        registered: false,
        generated: null,
    };
    try {
        Object.defineProperty(Extended, '__proxyMetadata__', {
            value: meta,
            writable: true,
            configurable: true,
            enumerable: false,
        });
    } catch (_) {
        Extended.__proxyMetadata__ = meta;
    }
    proxyExtensions.push(meta);
    if (typeof globalThis.__nsProxyAutoCapture === 'function') {
        try {
            globalThis.__nsProxyAutoCapture(JSON.stringify(proxyExtensions));
        } catch (_) {
            // Capture is best-effort; runtime behavior should remain unaffected.
        }
    }
   
    if (meta.interfaces.length > 0) {
        if (typeof globalThis.__nsProxyCompileProject === 'function') {
            try { registerProxy(meta); } catch (_) { }
        } else if (typeof globalThis.__nsProxyWriteTextFile === 'function') {
            try { emitProxy(meta); } catch (_) { }
        }
    }
    return meta;
}

function ensureProxyInstance(instance, overrides, ctor) {
    if (!instance || typeof instance !== 'object') {
        return -1;
    }

    var proxyId = instance.__proxyId;
    if (typeof proxyId !== 'number' || !Number.isFinite(proxyId)) {
        proxyId = nextProxyId++;
        try {
            Object.defineProperty(instance, '__proxyId', {
                value: proxyId,
                writable: false,
                configurable: true,
                enumerable: false,
            });
        } catch (_) {
            instance.__proxyId = proxyId;
        }
    }

    proxyInstances.set(proxyId, {
        instance: instance,
        overrides: overrides,
        constructor: ctor,
    });

    return proxyId;
}

function makeExtendedConstructor(baseCtor, nameOrOverrides, maybeOverrides) {
    var hasName = typeof nameOrOverrides === 'string';
    var explicitTypeName = hasName ? nameOrOverrides : '';
    var typeName = explicitTypeName || autoProxyTypeName(baseCtor);
    var overrides = hasName ? maybeOverrides : nameOrOverrides;
    if (!overrides || typeof overrides !== 'object') {
        overrides = {};
    }

    function Extended() {
        var instance;
        var args = Array.prototype.slice.call(arguments);
        try {
            instance = Reflect.construct(baseCtor, args);
        } catch (_) {
            instance = {};
        }

        for (var key in overrides) {
            if (key === 'interfaces') {
                continue;
            }
            var value = overrides[key];
            if (typeof value === 'function') {
                try {
                    Object.defineProperty(instance, key, {
                        value: value,
                        writable: true,
                        configurable: true,
                        enumerable: true,
                    });
                } catch (_) {
                    instance[key] = value;
                }
            } else {
                instance[key] = value;
            }
        }

        if (typeof overrides.init === 'function') {
            try {
                var initResult = overrides.init.apply(instance, args);
                if (initResult && typeof initResult === 'object') {
                    instance = initResult;
                }
            } catch (_) {
                // Keep constructor resilient; init errors should not crash runtime.
            }
        }

        if (Array.isArray(overrides.interfaces)) {
            try {
                Object.defineProperty(instance, '__interfaces__', {
                    value: overrides.interfaces.slice(),
                    writable: false,
                    configurable: true,
                    enumerable: false,
                });
            } catch (_) {
                instance.__interfaces__ = overrides.interfaces.slice();
            }
        }

        ensureProxyInstance(instance, overrides, Extended);

        return instance;
    }

    Extended.prototype = Object.create((baseCtor && baseCtor.prototype) || Object.prototype);
    Extended.prototype.constructor = Extended;

    for (var protoKey in overrides) {
        if (protoKey === 'interfaces') {
            continue;
        }
        Extended.prototype[protoKey] = overrides[protoKey];
    }

    if (typeName) {
        try {
            Object.defineProperty(Extended, 'name', {
                value: typeName,
                configurable: true,
            });
        } catch (_) {
            // Non-critical metadata assignment.
        }
    }

    Extended.__typeName__ = typeName || ctorName(baseCtor);
    Extended.__nsWinRTClass__ = true;
    var metadata = buildProxyMetadata(baseCtor, typeName, overrides, Extended);

    Extended.extend = function (nextNameOrOverrides, nextMaybeOverrides) {
        return makeExtendedConstructor(Extended, nextNameOrOverrides, nextMaybeOverrides);
    };

    try {
        Object.defineProperty(Extended, 'emitProxy', {
            value: function (outDir) {
                return NSWinRT.proxy.emit(metadata, outDir);
            },
            writable: true,
            configurable: true,
            enumerable: false,
        });
    } catch (_) {
        Extended.emitProxy = function (outDir) {
            return NSWinRT.proxy.emit(metadata, outDir);
        };
    }

    return Extended;
}

if (typeof Function.prototype.extend !== 'function') {
    Object.defineProperty(Function.prototype, 'extend', {
        value: function (nameOrOverrides, maybeOverrides) {
            // A WinRT class (or a JS class deriving from one): an ES2015 subclass, so
            // instances get the composed-object construction of `class X extends Y`.
            if (globalThis.NSWinRT && typeof globalThis.NSWinRT.extendClass === 'function') {
                if (typeof this.__typeName__ === 'string')
                    return globalThis.NSWinRT.extendClass(this, nameOrOverrides, maybeOverrides);
                // Object.extend({ interfaces: [...], ... }): a subclass of System.Object.
                if (this === Object && globalThis.System)
                    return globalThis.NSWinRT.extendClass(globalThis.System.Object, nameOrOverrides, maybeOverrides);
            }
            return makeManagedConstructor(this, nameOrOverrides, maybeOverrides);
        },
        writable: true,
        configurable: true,
        enumerable: false,
    });
}

if (typeof Object.extend !== 'function') {
    Object.defineProperty(Object, 'extend', {
        value: function (nameOrOverrides, maybeOverrides) {
            return makeManagedConstructor(Object, nameOrOverrides, maybeOverrides);
        },
        writable: true,
        configurable: true,
        enumerable: false,
    });
}

// Create a managed-backed subclass factory. This returns a constructor
// that, when invoked, asks the managed bridge to instantiate a real
// C# subclass whose vtable forwards virtual calls back into the
// provided JS `overrides` object via the DotNet bridge.
function makeManagedConstructor(baseCtor, nameOrOverrides, maybeOverrides) {
    var hasName = typeof nameOrOverrides === 'string';
    var explicitTypeName = hasName ? nameOrOverrides : '';
    var typeName = explicitTypeName || autoProxyTypeName(baseCtor);
    var overrides = hasName ? maybeOverrides : nameOrOverrides;
    if (!overrides || typeof overrides !== 'object') overrides = {};

    function Managed() {
        var args = Array.prototype.slice.call(arguments);
        // Pass the actual base ctor type name as the assembly param so the
        // bridge can fall back to it when no static proxy exists for typeName.
        var baseTypeName = ctorName(baseCtor) || '';
        var obj = NSWinRT.proxy.createManagedSubclass(baseTypeName, typeName || '', overrides);
        if (typeof overrides.init === 'function') {
            try {
                var initResult = overrides.init.apply(obj, args);
                if (initResult && typeof initResult === 'object') obj = initResult;
            } catch (_) { }
        }
        return obj;
    }

    Managed.prototype = Object.create((baseCtor && baseCtor.prototype) || Object.prototype);
    Managed.prototype.constructor = Managed;
    Managed.extend = function (nextNameOrOverrides, nextMaybeOverrides) {
        return makeManagedConstructor(Managed, nextNameOrOverrides, nextMaybeOverrides);
    };
    try {
        Object.defineProperty(Managed, 'name', { value: typeName || ctorName(baseCtor), configurable: true });
    } catch (_) { }
    Managed.__typeName__ = typeName || ctorName(baseCtor);
    Managed.__nsWinRTClass__ = true;
    var meta = buildProxyMetadata(baseCtor, typeName, overrides, Managed);
    try {
        Object.defineProperty(Managed, 'emitProxy', {
            value: function (outDir) { return NSWinRT.proxy.emit(meta, outDir); },
            writable: true,
            configurable: true,
            enumerable: false,
        });
    } catch (_) {
        Managed.emitProxy = function (outDir) { return NSWinRT.proxy.emit(meta, outDir); };
    }
    return Managed;
}

if (typeof Function.prototype.extendManaged !== 'function') {
    Object.defineProperty(Function.prototype, 'extendManaged', {
        value: function (nameOrOverrides, maybeOverrides) {
            return makeManagedConstructor(this, nameOrOverrides, maybeOverrides);
        },
        writable: true,
        configurable: true,
        enumerable: false,
    });
}

if (typeof Object.extendManaged !== 'function') {
    Object.defineProperty(Object, 'extendManaged', {
        value: function (nameOrOverrides, maybeOverrides) {
            return makeManagedConstructor(Object, nameOrOverrides, maybeOverrides);
        },
        writable: true,
        configurable: true,
        enumerable: false,
    });
}

// Convenience alias for hosts that expect a global BaseClass helper.
globalThis.BaseClass = globalThis.BaseClass || {};
if (typeof globalThis.BaseClass.extend !== 'function') {
    globalThis.BaseClass.extend = function (baseCtor, nameOrOverrides, maybeOverrides) {
        return makeManagedConstructor(baseCtor || Object, nameOrOverrides, maybeOverrides);
    };
}

// Registry mapping each WinRT-extended prototype to its lazy factory.
// Keyed by the new Child.prototype created in __extends_winrt so that
// the Parent.apply override can look up the right factory per subclass.
var __nsExtendRegistry__ = new WeakMap();

function __extends_winrt(Child, Parent) {
    var extendStatics = Object.setPrototypeOf || function (d, b) { d.__proto__ = b; };
    extendStatics(Child, Parent);

    // Lazy factory: deferred until first `new Child()` so that TypeScript
    // has had a chance to populate Child.prototype with override methods.
    var _extended = null;
    var getExtended = function () {
        if (!_extended) {
            var overrides = {};
            var interfaces = [];
            var proto = Child.prototype;
            if (proto) {
                Object.getOwnPropertyNames(proto).forEach(function (key) {
                    if (key === 'constructor') return;
                    if (key === 'interfaces') {
                        var iv = proto[key];
                        if (Array.isArray(iv)) interfaces = iv;
                        return;
                    }
                    overrides[key] = proto[key];
                });
            }
            if (interfaces.length) overrides.interfaces = interfaces;
            var name = Child.__winrtProxyName__ || Child.name || '';
            var hasBridge = typeof globalThis.__nsDotNetCreateJsSubclass === 'function';
            if (Child.__winrtProxyName__ || (interfaces.length > 0 && hasBridge)) {
                _extended = makeManagedConstructor(Parent, name, overrides);
            } else {
                // No bridge or no interfaces: pure JS proxy via Reflect.construct.
                _extended = makeExtendedConstructor(Parent, name, overrides);
            }
        }
        return _extended;
    };

    var newProto = Object.create((Parent && Parent.prototype) || Object.prototype);
    newProto.constructor = Child;
    Child.prototype = newProto;
    __nsExtendRegistry__.set(newProto, getExtended);

    // Patch Parent.apply/call once per parent class.  The WeakMap lookup
    // inside the override picks the correct factory per subclass so that
    // multiple classes extending the same parent coexist correctly.
    if (!Parent.__nsApplyPatched__) {
        Parent.__nsApplyPatched__ = true;

        Parent.apply = function (thiz, args) {
            if (thiz && thiz.__nsContainer__) return;
            var factory = __nsExtendRegistry__.get(Object.getPrototypeOf(thiz));
            if (!factory) return;
            thiz.__nsContainer__ = true;
            try {
                var Extended = factory();
                var argArr = Array.isArray(args) ? args : (args ? Array.prototype.slice.call(args) : []);
                if (argArr.length > 0) {
                    thiz.__proto__ = new (Function.prototype.bind.apply(Extended, [null].concat(argArr)))();
                } else {
                    thiz.__proto__ = new Extended();
                }
                return thiz.__proto__;
            } finally {
                delete thiz.__nsContainer__;
            }
        };

        Parent.call = function (thiz) {
            return Parent.apply(thiz, Array.prototype.slice.call(arguments, 1));
        };
    }
}

// Global __extends that TypeScript compiled output resolves via
// `var __extends = (this && this.__extends) || ...`.
// Intercepts WinRT parent classes; falls back to standard TS behaviour.
if (typeof globalThis.__extends !== 'function') {
    var __extends_ts_inner = function (d, b) {
        if (b !== null && typeof b !== 'function') {
            throw new TypeError('Class extends value ' + String(b) + ' is not a constructor or null');
        }
        var extendStatics = Object.setPrototypeOf || function (d, b) { d.__proto__ = b; };
        extendStatics(d, b);
        function __() { this.constructor = d; }
        d.prototype = b === null ? Object.create(b) : (__.prototype = b.prototype, new __());
    };

    globalThis.__extends = function (d, b) {
        if (b && b.__nsWinRTClass__) {
            __extends_winrt(d, b);
        } else {
            __extends_ts_inner(d, b);
        }
    };
}

// TypeScript decorator helpers, as the iOS/Android runtimes provide them: compiled
// (`experimentalDecorators`) code calls a bare `__decorate(...)`. Core's globals module
// binds tslib's copies too, but an ES-module bundle can evaluate decorated core modules
// before it runs.
if (typeof globalThis.__decorate !== 'function') {
    globalThis.__decorate = function (decorators, target, key, desc) {
        var c = arguments.length;
        var r = c < 3 ? target : desc === null ? (desc = Object.getOwnPropertyDescriptor(target, key)) : desc, d;
        if (typeof Reflect === 'object' && typeof Reflect.decorate === 'function') {
            r = Reflect.decorate(decorators, target, key, desc);
        } else {
            for (var i = decorators.length - 1; i >= 0; i--) {
                if ((d = decorators[i])) r = (c < 3 ? d(r) : c > 3 ? d(target, key, r) : d(target, key)) || r;
            }
        }
        return c > 3 && r && Object.defineProperty(target, key, r), r;
    };
}
if (typeof globalThis.__param !== 'function') {
    globalThis.__param = function (paramIndex, decorator) {
        return function (target, key) { decorator(target, key, paramIndex); };
    };
}
if (typeof globalThis.__metadata !== 'function') {
    globalThis.__metadata = function (metadataKey, metadataValue) {
        if (typeof Reflect === 'object' && typeof Reflect.metadata === 'function') {
            return Reflect.metadata(metadataKey, metadataValue);
        }
    };
}

// `return global.__native(this)` ends TypeScript subclass constructors written for
// Android/iOS; here `this` already is the native-backed object.
if (typeof globalThis.__native !== 'function') {
    globalThis.__native = function (obj) { return obj; };
}

// @NativeClass / @NativeClass(): marks a class that extends a native (WinRT/.NET) class.
// Extending works without it; it exists so code written for iOS/Android, where the build
// transforms decorated classes, runs unchanged.
if (typeof globalThis.NativeClass !== 'function') {
    Object.defineProperty(globalThis, 'NativeClass', {
        value: function NativeClass(target) {
            if (typeof target === 'function') return target;
            return function (cls) { return cls; };
        },
        writable: true,
        configurable: true,
        enumerable: false,
    });
}

// @Interfaces([IFoo, IBar]): attach WinRT interface list to a class.
if (typeof globalThis.Interfaces !== 'function') {
    Object.defineProperty(globalThis, 'Interfaces', {
        value: function Interfaces(interfacesList) {
            return function (target) {
                target.prototype.interfaces = Array.isArray(interfacesList) ? interfacesList.slice() : [];
                return target;
            };
        },
        writable: true,
        configurable: true,
        enumerable: false,
    });
}


if (typeof globalThis.CSharpProxy !== 'function') {
    Object.defineProperty(globalThis, 'CSharpProxy', {
        value: function CSharpProxy(name) {
            return function (target) {
                target.__winrtProxyName__ = typeof name === 'string' ? name : '';
                return target;
            };
        },
        writable: true,
        configurable: true,
        enumerable: false,
    });
}

function defaultProxyOutDir(meta) {
    var typeName = (meta && meta.typeName) ? meta.typeName : 'GeneratedProxy';
    var safe = safeIdentifier(typeName.split('.').pop());
    return './generated-proxies/' + safe;
}

function emitProxy(meta, outDir) {
    if (!meta || typeof meta !== 'object') {
        throw new Error('NSWinRT.proxy.emit(meta[, outDir]) expects a proxy metadata object');
    }
    if (typeof globalThis.__nsProxyWriteTextFile !== 'function') {
        throw new Error('Host proxy file emitter is not available');
    }

    var dir = outDir || defaultProxyOutDir(meta);
    var csprojPath = dir + '/Proxy.csproj';
    var csPath = dir + '/Proxy.g.cs';
    var csproj = renderProxyCsproj(meta);
    var source = renderProxyCSharp(meta);

    globalThis.__nsProxyWriteTextFile(csprojPath, csproj);
    globalThis.__nsProxyWriteTextFile(csPath, source);

    meta.generated = {
        dir: dir,
        csprojPath: csprojPath,
        csPath: csPath,
    };

    return meta.generated;
}

function compileProxy(meta, outDir, configuration) {
    if (typeof globalThis.__nsProxyCompileProject !== 'function') {
        throw new Error('Host proxy compiler is not available');
    }
    var generated = emitProxy(meta, outDir);
    var result = globalThis.__nsProxyCompileProject(generated.csprojPath, configuration || 'Debug');
    generated.build = result;
    return generated;
}

function registerProxy(meta, outDir, configuration) {
    var generated = compileProxy(meta, outDir, configuration);
    var manifest = {
        kind: 'windows-proxy',
        typeName: meta.typeName,
        baseType: meta.baseType,
        interfaces: meta.interfaces,
        methods: meta.methods,
        properties: meta.properties,
        generated: generated,
        registration: {
            hostCanLoadAssemblies: false,
            note: 'Assembly build succeeded, but runtime CLR proxy activation is not wired yet. Dynamic JS fallback remains active.',
        },
    };
    if (typeof globalThis.__nsProxyRegisterManifest === 'function') {
        globalThis.__nsProxyRegisterManifest(JSON.stringify(manifest));
    }
    meta.registered = true;
    meta.registration = manifest.registration;
    return manifest;
}

function invokeProxyById(proxyId, methodName, argsArray) {
    var entry = proxyInstances.get(proxyId);
    if (!entry) {
        throw new Error('Proxy instance not found for id ' + proxyId);
    }

    var target = entry.instance;
    var method = target && target[methodName];
    if (typeof method !== 'function') {
        throw new Error('Proxy method "' + methodName + '" is not defined on proxy id ' + proxyId);
    }

    return method.apply(target, Array.isArray(argsArray) ? argsArray : []);
}

globalThis.__nsInvokeProxyJs = invokeProxyById;

globalThis.NSWinRT.proxy = {
    getExtensions: function () {
        return proxyExtensions.slice();
    },
    emit: emitProxy,
    compile: compileProxy,
    register: registerProxy,
    createManagedSubclass: function (assembly, typeName, overrides) {
        if (typeof globalThis.__nsDotNetCreateJsSubclass !== 'function') {
            throw new Error('__nsDotNetCreateJsSubclass is not available in this runtime');
        }
        if (!overrides || typeof overrides !== 'object') overrides = {};

        var interfaceNames = Array.isArray(overrides.interfaces)
            ? overrides.interfaces.map(function (iface) { return ctorName(iface); })
            : [];

        // Literal CLR member names this JS object actually overrides. The bridge only
        // emits an IL override for base virtuals named here (everything else falls
        // through to the real base implementation) and implements exactly these
        // interface members from JS (others fall back to default/no-op). Plain
        // functions are method overrides; accessor descriptors become get_/set_-
        // prefixed property overrides, matching the literal CLR accessor names the
        // dynamic proxy already dispatches by. Read via getOwnPropertyDescriptor
        // throughout so collecting names never invokes a JS getter as a side effect.
        var memberNames = [];
        for (var mkey in overrides) {
            if (!Object.prototype.hasOwnProperty.call(overrides, mkey)) continue;
            if (mkey === 'interfaces' || mkey === 'init') continue;
            var mdesc = Object.getOwnPropertyDescriptor(overrides, mkey);
            if (!mdesc) continue;
            if (typeof mdesc.get === 'function' || typeof mdesc.set === 'function') {
                if (typeof mdesc.get === 'function') memberNames.push('get_' + mkey);
                if (typeof mdesc.set === 'function') memberNames.push('set_' + mkey);
            } else if (typeof mdesc.value === 'function') {
                memberNames.push(mkey);
            }
        }

        var dispatcher = function () {
            var a = Array.prototype.slice.call(arguments);
            try {
                var target = _wrapDotNetHandle(a[0]);
                var method = a[1];
                var margs = a.slice(2).map(_wrapDotNetHandle);
                if (target && typeof method === 'string') {
                    var fn = overrides[method];
                    if (typeof fn === 'function') return fn.apply(target, Array.isArray(margs) ? margs : []);
                }
            } catch (_) { }
        };
        var handle = globalThis.__nsDotNetCreateJsSubclass(assembly || '', typeName || '', interfaceNames, memberNames, dispatcher);
        var obj = _wrapDotNetHandle(handle);
        ensureProxyInstance(obj, overrides, function () { });
        return obj;
    },
    invokeById: invokeProxyById,
    listRegisteredManifests: function () {
        if (typeof globalThis.__nsProxyListManifests === 'function') {
            return globalThis.__nsProxyListManifests();
        }
        return [];
    },
};
