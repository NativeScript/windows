// interop.Array.create(type, count): a zero-filled array of a primitive WinRT element type,
// backed by JS memory a `T[]` parameter reads, or a callee fills, in place.
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

    function create(type, count) {
        var name = typeof type === 'function' && type.BYTES_PER_ELEMENT ? type.name : String(type);
        var kind = Object.prototype.hasOwnProperty.call(kinds, name) ? kinds[name] : name;
        var ctor = globalThis[kind];
        if (!/Array$/.test(kind) || typeof ctor !== 'function' || !ctor.BYTES_PER_ELEMENT) {
            throw new TypeError('interop.Array.create: unsupported element type ' + name +
                '; expected one of ' + Object.keys(kinds).join(', '));
        }
        if (typeof count !== 'number' || !(count >= 0) || count % 1 !== 0 || count > 4294967295) {
            throw new RangeError('interop.Array.create: count must be an integer from 0 to 4294967295');
        }
        return new ctor(count);
    }

    interop.Array = { create: create };
})();
