// Win32 call overhead micro benchmarks, for every engine:
//   classic:    PLAYGROUND_SCRIPT_PATH=packages/demo/win32-bench.js target/release/playground.exe
//   napi:       nativescript-windows packages/demo/win32-bench.js   (standalone engines)
//   Node:       node packages/demo/win32-bench.js
// An optional filter argument (NSWIN_BENCH_FILTER on the classic engine) selects benchmarks.
'use strict';

if (!globalThis.NSWinRT || !globalThis.NSWinRT.win32) {
    if (typeof require === 'function') require('../../windows-napi/nswinrt.js');
}
var win32 = globalThis.NSWinRT.win32;
var filter = (typeof process !== 'undefined' && process.argv && process.argv[2]) ||
    (typeof process !== 'undefined' && process.env && process.env.NSWIN_BENCH_FILTER) || '';
var N = 1000000;

function bench(name, fn) {
    if (filter && name.indexOf(filter) < 0) return;
    for (var i = 0; i < 10000; i++) fn(i); // warm up: the bound function and TurboFan
    var t0 = performance.now();
    for (var i = 0; i < N; i++) fn(i);
    var ms = performance.now() - t0;
    var ns = (ms * 1e6 / N).toFixed(1);
    while (name.length < 36) name += ' ';
    while (ns.length < 7) ns = ' ' + ns;
    console.log(name + ns + ' ns/call');
}

var kernel32 = win32.define('kernel32.dll', {
    GetCurrentThreadId: { args: [], ret: 'u32' },
    GetTickCount64: { args: [], ret: 'u64' },
    MulDiv: { args: ['i32', 'i32', 'i32'], ret: 'i32' },
    SetLastError: { args: ['u32'], ret: 'void' },
    lstrlenW: { args: ['wstr'], ret: 'i32' },
    lstrcmpW: { args: ['wstr', 'wstr'], ret: 'i32' },
    MultiByteToWideChar: { args: ['u32', 'u32', 'pointer', 'i32', 'pointer', 'i32'], ret: 'i32' },
});
var user32 = win32.define('user32.dll', {
    IsWindow: { args: ['pointer'], ret: 'bool' },
    GetSystemMetrics: { args: ['i32'], ret: 'i32' },
});
var msvcrt = win32.define('msvcrt.dll', {
    abs: { args: ['i32'], ret: 'i32' },
    labs: { args: ['i64'], ret: 'i64' },
    fabs: { args: ['f64'], ret: 'f64' },
    pow: { args: ['f64', 'f64'], ret: 'f64' },
    memcmp: { args: ['pointer', 'pointer', 'u64'], ret: 'i32' },
    memchr: { args: ['pointer', 'i32', 'u64'], ret: 'pointer' },
    wcslen: { args: ['wstr'], ret: 'u64' },
});
var auto = win32.bind('kernel32.dll', 'u32');

var utf8 = new Uint8Array(16);
for (var i = 0; i < 16; i++) utf8[i] = 65 + i;
var wide = new Uint16Array(32);

// integers and void
bench('GetCurrentThreadId()', function () { return kernel32.GetCurrentThreadId(); });
bench('SetLastError(u32) -> void', function (i) { kernel32.SetLastError(i); });
bench('abs(i32)', function (i) { return msvcrt.abs(-i); });
bench('MulDiv(i32, i32, i32)', function (i) { return kernel32.MulDiv(i, 3, 2); });
bench('GetSystemMetrics(i32)', function () { return user32.GetSystemMetrics(0); });
bench('IsWindow(null) -> bool', function () { return user32.IsWindow(null); });

// floats
bench('fabs(f64)', function (i) { return msvcrt.fabs(-i); });
bench('pow(f64, f64)', function (i) { return msvcrt.pow(i, 0.5); });

// 64-bit and pointers
bench('GetTickCount64() -> u64', function () { return kernel32.GetTickCount64(); });
bench('labs(i64)', function (i) { return msvcrt.labs(-i); });
bench('memcmp(buffer, buffer, u64)', function () { return msvcrt.memcmp(utf8, utf8, 16); });
bench('memchr(buffer, i32, u64) -> pointer', function () { return msvcrt.memchr(utf8, 70, 16); });

// strings
bench('lstrlenW(string)', function () { return kernel32.lstrlenW('nativescript'); });
bench('wcslen(string) -> u64', function () { return msvcrt.wcslen('nativescript'); });
bench('lstrcmpW(string, string)', function () {
    return kernel32.lstrcmpW('nativescript', 'nativescript');
});

// six arguments: four in registers, two on the stack
bench('MultiByteToWideChar(6 args)', function () {
    return kernel32.MultiByteToWideChar(65001, 0, utf8, 16, wide, 32);
});

// the auto-typed and generic paths
bench('bind().GetCurrentThreadId()', function () { return auto.GetCurrentThreadId(); });
bench('call(dll, name, ret)', function () {
    return win32.call('kernel32.dll', 'GetCurrentThreadId', 'u32');
});
bench('call(dll, name, ret, {i32})', function (i) {
    return win32.call('kernel32.dll', 'SetLastError', 'void', { type: 'u32', value: i });
});
