// Checks that V8 platform tasks run (FinalizationRegistry cleanup is one).
//   nativescript-windows packages/test/finalization-check.js
'use strict';

var collected = false;
var registry = new FinalizationRegistry(function () { collected = true; });
(function () { registry.register({}, 'held'); })();

var tries = 0;
var timer = setInterval(function () {
    // Forces a scavenge.
    var junk = [];
    for (var i = 0; i < 100000; i++) junk.push({ i: i, s: 'x' + i });
    junk = null;
    if (collected || ++tries > 100) {
        clearInterval(timer);
        console.log('finalization-check: ' + (collected ? 'PASS' : 'FAIL (cleanup never ran)'));
        if (!collected) throw new Error('FinalizationRegistry cleanup never ran');
    }
}, 5);
