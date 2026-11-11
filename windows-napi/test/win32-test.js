// NSWinRT.win32
const { win32, native } = require('../nswinrt.js');

let pass = 0, fail = 0;
function check(name, got, expected) {
  const ok = typeof expected === 'function' ? expected(got) : Object.is(got, expected);
  if (ok) pass++;
  else { console.log(`FAIL ${name}: got ${String(got)}, expected ${String(expected)}`); fail++; }
}

async function pumpUntil(cond, ms = 3000) {
  const start = Date.now();
  while (!cond() && Date.now() - start < ms) {
    native.pumpMessages();
    await new Promise((r) => setImmediate(r));
  }
  return cond();
}

const WM_HOTKEY = 0x0312;
const WM_APP = 0x8000;
const MOD_NOREPEAT = 0x4000;
const VK_F24 = 0x87;

const kernel32 = win32.define('kernel32.dll', {
  GetCurrentProcessId: { args: [], ret: 'u32' },
  GetTickCount64: { args: [], ret: 'u64' },
  lstrcpyW: { args: ['pointer', 'wstr'], ret: 'pointer' },
});
const user32 = win32.define('user32.dll', {
  EnumWindows: { args: ['pointer', 'pointer'], ret: 'bool' },
  FindWindowW: { args: ['wstr', 'wstr'], ret: 'pointer' },
  IsWindow: { args: ['pointer'], ret: 'bool' },
  PostMessageW: { args: ['pointer', 'u32', 'pointer', 'pointer'], ret: 'bool' },
  RegisterHotKey: { args: ['pointer', 'i32', 'u32', 'u32'], ret: 'bool' },
  UnregisterHotKey: { args: ['pointer', 'i32'], ret: 'bool' },
});

(async () => {
  // Calls
  check('call returns u32', win32.call('kernel32.dll', 'GetCurrentProcessId', 'u32'), process.pid);
  check('define returns u32', kernel32.GetCurrentProcessId(), process.pid);
  check('bind auto-types a string', win32.bind('kernel32.dll').lstrlenW('hello'), 5);
  check('u64 result', kernel32.GetTickCount64(), (v) => typeof v === 'number' && v > 0);

  // VER_MAJORVERSION = VER_GREATER_EQUAL, keeping the high bit.
  const mask = win32.call('kernel32.dll', 'VerSetConditionMask', 'u64',
    { type: 'u64', value: 0x8000000000000000n }, { type: 'u32', value: 0x2 }, { type: 'u8', value: 3 });
  check('BigInt in, BigInt out beyond 2^53', mask, 0x8000000000000018n);

  const buf = new Uint16Array(8);
  check('buffer pointer result', kernel32.lstrcpyW(buf, 'abc'), (p) => p !== 0);
  check('native code wrote the buffer', String.fromCharCode(...buf.subarray(0, 3)), 'abc');

  check('null wstr is a null pointer', user32.FindWindowW(null, `ns-win32-test-${process.pid}-${Date.now()}`), 0);

  // Bound functions: floats, stack arguments, lock-in and errors
  const msvcrt = win32.define('msvcrt.dll', {
    pow: { args: ['f64', 'f64'], ret: 'f64' },
    abs: { args: ['i32'], ret: 'i32' },
    memchr: { args: ['pointer', 'i32', 'u64'], ret: 'pointer' },
  });
  check('f64 arguments and result', msvcrt.pow(2, 10), 1024);
  const ucrt = win32.define('ucrtbase.dll', { sqrtf: { args: ['f32'], ret: 'f32' } });
  check('f32 argument and result', ucrt.sqrtf(2.25), 1.5);
  check('i32 from a bool', msvcrt.abs(true), 1);
  check('i32 from a numeric string', msvcrt.abs('-7'), 7);
  const haystack = new Uint8Array([1, 2, 3, 4]);
  check('pointer result arithmetic', msvcrt.memchr(haystack, 3, 4) - msvcrt.memchr(haystack, 1, 4), 2);
  check('pointer from an object throws', (() => {
    try { msvcrt.memchr({}, 1, 4); return false; } catch (e) { return /argument 1/.test(e.message); }
  })(), true);
  const mb = win32.define('kernel32.dll', {
    MultiByteToWideChar: { args: ['u32', 'u32', 'pointer', 'i32', 'pointer', 'i32'], ret: 'i32' },
  });
  const wide = new Uint16Array(8);
  check('six arguments (two on the stack)', mb.MultiByteToWideChar(65001, 0, new Uint8Array([104, 105]), 2, wide, 8), 2);
  check('stack arguments reached the callee', String.fromCharCode(wide[0], wide[1]), 'hi');
  const locked = win32.bind('kernel32.dll').lstrlenW;
  check('bind locks the first call types in', locked('abc'), 3);
  check('a locked wstr takes null', locked(null), 0);
  check('define returns the same function each time', kernel32.GetCurrentProcessId, kernel32.GetCurrentProcessId);

  check('unknown export throws', (() => {
    try { win32.call('kernel32.dll', 'NoSuchExport', 'i32'); return false; } catch { return true; }
  })(), true);

  // Callbacks
  let seen = 0;
  let lparam = -1;
  const enumProc = win32.callback('bool', ['pointer', 'pointer'], (hwnd, lp) => {
    seen++;
    lparam = lp;
    return seen < 3;
  });
  check('callback pointer', enumProc.pointer, (p) => typeof p === 'number' && p !== 0);
  user32.EnumWindows(enumProc.pointer, 42);
  check('callback runs during the call', seen, (n) => n >= 1 && n <= 3);
  check('callback receives arguments', lparam, 42);
  enumProc.release();
  enumProc.release();
  check('release twice is harmless', true, true);

  const throwing = win32.callback('bool', ['pointer', 'pointer'], () => { throw new Error('boom'); });
  user32.EnumWindows(throwing.pointer, 0);
  check('a throwing callback returns 0 and records the error', native.lastError(), (e) => /boom/.test(e || ''));
  throwing.release();

  // Message windows
  const received = [];
  const win = win32.messageWindow((msg, wParam, lParam) => received.push([msg, wParam, lParam]));
  check('message window', user32.IsWindow(win.hwnd), true);

  user32.PostMessageW(win.hwnd, WM_APP + 1, 7, 9);
  check('posted message reaches the listener', await pumpUntil(() => received.some((m) => m[0] === WM_APP + 1)), true);
  const app = received.find((m) => m[0] === WM_APP + 1);
  check('message arguments', app && `${app[1]},${app[2]}`, '7,9');

  check('RegisterHotKey on the window', user32.RegisterHotKey(win.hwnd, 1, MOD_NOREPEAT, VK_F24), true);
  user32.PostMessageW(win.hwnd, WM_HOTKEY, 1, 0);
  check('WM_HOTKEY reaches the listener', await pumpUntil(() => received.some((m) => m[0] === WM_HOTKEY && m[1] === 1)), true);
  user32.UnregisterHotKey(win.hwnd, 1);

  win.close();
  check('close destroys the window', user32.IsWindow(win.hwnd), false);
  win.close();

  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch((e) => {
  console.log(`FAIL unexpected: ${e && e.stack || e}`);
  console.log(`\n${pass} passed, ${fail + 1} failed`);
  process.exit(1);
});
