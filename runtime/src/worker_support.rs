use v8::{ContextScope, HandleScope};

/// Installs the `Worker` global class.
///
/// Requires `message_port::install_message_port_runtime` to have run first so that
/// `globalThis.MessagePort`, `globalThis.MessageChannel`, and
/// `globalThis.__nsMessagePortDispatch` are already defined.
pub fn install_worker_runtime(scope: &mut ContextScope<HandleScope>) {
    let source = r#"
    (function () {
        if (typeof globalThis.Worker === 'function') {
            return;
        }

        var MessagePort    = globalThis.MessagePort;
        var MessageChannel = globalThis.MessageChannel;

        function __resolveWorkerSource(specifier, options) {
            var evalMode = !!(options && options.eval);
            var input = String(specifier || '');
            if (!input) {
                throw new Error('Worker requires a non-empty source string or file path');
            }

            if (evalMode) {
                return { source: input, filename: '[worker:eval]' };
            }

            if (typeof globalThis.__nsResolveModulePath !== 'function' || typeof globalThis.__nsReadTextFile !== 'function') {
                throw new Error('Worker file loading is unavailable: missing runtime host hooks');
            }

            var resolved = globalThis.__nsResolveModulePath(
                input,
                '',
                globalThis.__nsAppRoot || ''
            );

            if (!resolved) {
                throw new Error('Unable to resolve worker path: ' + input);
            }

            return {
                source: globalThis.__nsReadTextFile(resolved),
                filename: resolved
            };
        }

        function __createWorkerGlobal(worker, workerPort) {
            var selfRef = {};

            selfRef.postMessage = function (data) { workerPort.postMessage(data); };
            selfRef.close = function () { worker.terminate(); };
            selfRef.addEventListener = function (type, listener) { workerPort.addEventListener(type, listener); };
            selfRef.removeEventListener = function (type, listener) { workerPort.removeEventListener(type, listener); };

            Object.defineProperty(selfRef, 'onmessage', {
                get: function () { return workerPort.onmessage; },
                set: function (handler) { workerPort.onmessage = handler; },
                enumerable: true,
                configurable: true
            });

            selfRef.MessagePort    = MessagePort;
            selfRef.MessageChannel = MessageChannel;
            selfRef.globalThis     = selfRef;
            return selfRef;
        }

        function __evaluateWorkerScript(workerGlobal, source, filename) {
            var executor = new Function(
                'self', 'globalThis', 'postMessage', 'MessagePort', 'MessageChannel', '__filename',
                source
            );
            executor(workerGlobal, workerGlobal, workerGlobal.postMessage, MessagePort, MessageChannel, filename);
        }

        var __threadedWorkers = {};

        function Worker(specifierOrSource, options) {
            if (!(this instanceof Worker)) {
                throw new TypeError('Class constructor Worker cannot be invoked without new');
            }

            var resolved = __resolveWorkerSource(specifierOrSource, options || {});
            var canUseThreadedHost =
                typeof globalThis.__nsWorkerCreateThreaded === 'function' &&
                typeof globalThis.__nsWorkerPostMessage === 'function' &&
                typeof globalThis.__nsWorkerPollMessages === 'function' &&
                typeof globalThis.__nsWorkerPollMessagesBlocking === 'function' &&
                typeof globalThis.__nsWorkerTerminate === 'function';

            var channel    = new MessageChannel();
            var mainPort   = channel.port1;
            var workerPort = channel.port2;
            var terminated = false;
            var workerId   = -1;

            if (canUseThreadedHost) {
                workerId = globalThis.__nsWorkerCreateThreaded(
                    resolved.source,
                    resolved.filename,
                    String(globalThis.__nsAppRoot || '')
                );
                if (workerId >= 0) {
                    // The runtime calls this whenever the worker has posted messages.
                    __threadedWorkers[workerId] = function () { dispatchThreadedMessages(); };
                }
            }

            function dispatchThreadedMessages(messages) {
                if (terminated || workerId < 0 || !canUseThreadedHost) {
                    return 0;
                }

                if (!messages) {
                    messages = globalThis.__nsWorkerPollMessages(workerId);
                }
                if (!messages || !messages.length) {
                    return 0;
                }

                var delivered = 0;
                var dispatch = typeof globalThis.__nsMessagePortDispatch === 'function'
                    ? globalThis.__nsMessagePortDispatch
                    : null;

                for (var i = 0; i < messages.length; i++) {
                    var value = messages[i];

                    if (value && typeof value === 'object' && value.__workerError) {
                        var errorEvent = {
                            type: 'messageerror',
                            error: new Error(String(value.__workerError)),
                            target: mainPort,
                            currentTarget: mainPort
                        };
                        if (typeof mainPort.onmessage === 'function') {
                            mainPort.onmessage.call(mainPort, errorEvent);
                        }
                        delivered += 1;
                        continue;
                    }

                    if (value && typeof value === 'object' && value.__workerExit) {
                        terminated = true;
                        delete __threadedWorkers[workerId];
                        delivered += 1;
                        continue;
                    }

                    if (dispatch) {
                        dispatch(mainPort, value);
                    } else {
                        mainPort.postMessage(value);
                    }
                    delivered += 1;
                }

                return delivered;
            }

            // Replies arrive through __nsWorkerDeliver where this thread has a dispatcher; without one
            // (console hosts, tests) postMessage waits a little for them.
            var pushesMessages = typeof globalThis.__nsWorkerPushesMessages === 'function' &&
                globalThis.__nsWorkerPushesMessages();

            this.postMessage = function (data, transfer) {
                if (terminated) { return; }

                if (canUseThreadedHost && workerId >= 0) {
                    globalThis.__nsWorkerPostMessage(workerId, data, transfer);
                    if (pushesMessages) {
                        return;
                    }

                    for (var attempt = 0; attempt < 100; attempt++) {
                        var blockingMessages = globalThis.__nsWorkerPollMessagesBlocking(workerId, 5);
                        var delivered = dispatchThreadedMessages(blockingMessages);
                        if (delivered === 0) {
                            delivered = dispatchThreadedMessages();
                        }
                        if (delivered > 0 || terminated) {
                            break;
                        }
                    }
                    return;
                }

                mainPort.postMessage(data);
            };

            this.terminate = function () {
                if (terminated) { return; }
                terminated = true;
                delete __threadedWorkers[workerId];

                if (canUseThreadedHost && workerId >= 0) {
                    try { globalThis.__nsWorkerTerminate(workerId); } catch (_) {}
                }

                mainPort.close();
                workerPort.close();
            };

            this.addEventListener    = function (type, listener) { mainPort.addEventListener(type, listener); };
            this.removeEventListener = function (type, listener) { mainPort.removeEventListener(type, listener); };

            Object.defineProperty(this, 'onmessage', {
                get: function () { return mainPort.onmessage; },
                set: function (handler) { mainPort.onmessage = handler; },
                enumerable: true,
                configurable: true
            });

            if (canUseThreadedHost && workerId >= 0) {
                dispatchThreadedMessages();
                return;
            }

            // In-process synchronous fallback (no threaded host).
            var workerGlobal = __createWorkerGlobal(this, workerPort);
            var schedule = typeof globalThis.queueMicrotask === 'function'
                ? globalThis.queueMicrotask.bind(globalThis)
                : function (cb) { Promise.resolve().then(cb); };

            schedule(function () {
                if (terminated) { return; }

                try {
                    __evaluateWorkerScript(workerGlobal, resolved.source, resolved.filename);
                } catch (err) {
                    if (typeof mainPort.onmessage === 'function') {
                        mainPort.onmessage.call(mainPort, {
                            type: 'messageerror',
                            error: err,
                            target: mainPort,
                            currentTarget: mainPort
                        });
                    } else if (typeof globalThis.__ns__setTimeout === 'function') {
                        globalThis.__ns__setTimeout(function () { throw err; }, 0);
                    } else if (typeof globalThis.setTimeout === 'function') {
                        globalThis.setTimeout(function () { throw err; }, 0);
                    } else {
                        throw err;
                    }
                }
            });
        }

        globalThis.Worker = Worker;
        globalThis.__nsWorkerDeliver = function (workerId) {
            var deliver = __threadedWorkers[workerId];
            if (deliver) { deliver(); }
        };
    })();
    "#;

    let Some(source) = v8::String::new(scope, source) else {
        return;
    };

    if let Some(script) = v8::Script::compile(scope, source, None) {
        script.run(scope);
    }
}
