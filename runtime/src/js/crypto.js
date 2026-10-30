// Web Crypto's random source, filled from the OS RNG by `__nsRandomFill`. Hosts that already have
// `crypto` (Node, Deno, Bun) keep their own.
(function () {
    var fill = globalThis.__nsRandomFill;
    if (typeof fill !== 'function') return;
    var crypto = globalThis.crypto || (globalThis.crypto = {});

    function isIntegerArray(array) {
        return array instanceof Int8Array || array instanceof Uint8Array ||
            array instanceof Uint8ClampedArray || array instanceof Int16Array ||
            array instanceof Uint16Array || array instanceof Int32Array ||
            array instanceof Uint32Array ||
            (typeof BigInt64Array === 'function' &&
                (array instanceof BigInt64Array || array instanceof BigUint64Array));
    }

    if (typeof crypto.getRandomValues !== 'function') {
        crypto.getRandomValues = function getRandomValues(array) {
            var prefix = "Failed to execute 'getRandomValues' on 'Crypto': ";
            if (!isIntegerArray(array)) {
                throw new TypeError(prefix + 'The provided ArrayBufferView is not an integer array.');
            }
            if (array.byteLength > 65536) {
                var message = prefix + "The ArrayBufferView's byte length (" + array.byteLength +
                    ') exceeds the number of bytes of entropy available via this API (65536).';
                throw typeof DOMException === 'function'
                    ? new DOMException(message, 'QuotaExceededError')
                    : new RangeError(message);
            }
            fill(array);
            return array;
        };
    }

    if (typeof crypto.randomUUID !== 'function') {
        crypto.randomUUID = function randomUUID() {
            var b = crypto.getRandomValues(new Uint8Array(16));
            b[6] = (b[6] & 0x0f) | 0x40; // version 4
            b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
            var hex = '';
            for (var i = 0; i < 16; i++) {
                hex += (b[i] + 0x100).toString(16).slice(1);
                if (i === 3 || i === 5 || i === 7 || i === 9) hex += '-';
            }
            return hex;
        };
    }
})();
