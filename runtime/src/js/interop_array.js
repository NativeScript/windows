// interop.Array.create(type, count): an array for a WinRT `T[]` parameter to read, or a callee
// to fill, in place. Primitive element types get a zero-filled typed array backed by JS memory;
// strings, `Object` and WinRT types (by name or constructor) get a plain array of nulls.
(function () {
    var interop = globalThis.interop || (globalThis.NSWinRT && globalThis.NSWinRT.interop);
    if (!interop || (interop.Array && typeof interop.Array.create === 'function')) return;

    var kinds = {
        UInt8: 'Uint8Array', Byte: 'Uint8Array', Boolean: 'Uint8Array',
        Int8: 'Int8Array', SByte: 'Int8Array',
        UInt16: 'Uint16Array', Char16: 'Uint16Array', Int16: 'Int16Array',
        UInt32: 'Uint32Array', Int32: 'Int32Array',
        UInt64: 'BigUint64Array', Int64: 'BigInt64Array',
        Single: 'Float32Array', Double: 'Float64Array'
    };

    function isReferenceType(type, name) {
        return name === 'String' || name === 'Object' || name.indexOf('.') > 0 ||
            (typeof type === 'function' && !type.BYTES_PER_ELEMENT);
    }

    function create(type, count) {
        var name = typeof type === 'function' && type.BYTES_PER_ELEMENT ? type.name : String(type);
        if (typeof count !== 'number' || !(count >= 0) || count % 1 !== 0 || count > 4294967295) {
            throw new RangeError('interop.Array.create: count must be an integer from 0 to 4294967295');
        }
        if (type != null && isReferenceType(type, name)) {
            var array = new Array(count);
            for (var i = 0; i < count; i++) array[i] = null;
            return array;
        }
        var kind = Object.prototype.hasOwnProperty.call(kinds, name) ? kinds[name] : name;
        var ctor = globalThis[kind];
        if (!/Array$/.test(kind) || typeof ctor !== 'function' || !ctor.BYTES_PER_ELEMENT) {
            throw new TypeError('interop.Array.create: unsupported element type ' + name +
                '; expected one of ' + Object.keys(kinds).join(', ') +
                ', String, Object or a WinRT type');
        }
        return new ctor(count);
    }

    interop.Array = { create: create };
})();
