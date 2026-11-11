'use strict';

const path = require('path');
const net = require('net');
const child_process = require('child_process');
const EventEmitter = require('events');

const STUBS_DIR = path.resolve(__dirname, '..', 'stubs');

function log() {
  const args = Array.prototype.slice.call(arguments);
  args.unshift(new Date().toISOString());
  console.log.apply(console, args);
}

function loadService(serviceDir) {
  const mainPath = path.join(path.resolve(serviceDir), 'main.js');
  const existing = process.env.NODE_PATH
    ? process.env.NODE_PATH.split(path.delimiter).filter(Boolean)
    : [];
  process.env.NODE_PATH = [STUBS_DIR].concat(existing).join(path.delimiter);
  require('module').Module._initPaths();
  require(mainPath);
  const Service = require('webos-service');
  const service = Service.lastInstance();
  if (!service) {
    throw new Error('main.js did not construct the webos-service stub');
  }
  return service;
}

function subscribe(serviceInstance) {
  const handler = serviceInstance.handlers['service_channel'];
  if (!handler) {
    throw new Error('service_channel is not registered');
  }

  const events = [];
  const emitter = new EventEmitter();
  let cursor = 0;

  function respond(obj) {
    events.push(obj);
    emitter.emit('event', obj);
  }

  handler.callback({
    isSubscription: true,
    payload: { subscriptionId: 'e2e' },
    respond: respond,
    cancel: function () {}
  });

  function waitFor(eventName, predicate, timeoutMs) {
    return new Promise(function (resolve, reject) {
      let timer = null;

      function matches(obj) {
        if (!obj || obj.event !== eventName) {
          return false;
        }
        if (typeof predicate !== 'function') {
          return true;
        }
        try {
          return !!predicate(obj.value);
        } catch (err) {
          return false;
        }
      }

      function finish() {
        emitter.removeListener('event', onEvent);
        if (timer) {
          clearTimeout(timer);
        }
      }

      function check() {
        for (let i = cursor; i < events.length; i++) {
          if (matches(events[i])) {
            cursor = i + 1;
            finish();
            resolve(events[i]);
            return true;
          }
        }
        return false;
      }

      function onEvent() {
        check();
      }

      if (check()) {
        return;
      }

      emitter.on('event', onEvent);
      timer = setTimeout(function () {
        finish();
        reject(new Error('timed out after ' + timeoutMs + ' ms waiting for event "' + eventName + '"'));
      }, timeoutMs);
    });
  }

  return {
    events: events,
    emitter: emitter,
    waitFor: waitFor
  };
}

function appRequest(serviceInstance, event, value) {
  return new Promise(function (resolve, reject) {
    const handler = serviceInstance.handlers['app_channel'];
    if (!handler) {
      reject(new Error('app_channel is not registered'));
      return;
    }

    let settled = false;
    const timer = setTimeout(function () {
      settle(null, null);
    }, 2000);

    function settle(err, obj) {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      if (err) {
        reject(err);
      } else {
        resolve(obj);
      }
    }

    let ret;
    try {
      ret = handler.callback({
        payload: { event: event, value: value },
        respond: function (obj) {
          settle(null, obj);
        }
      });
    } catch (err) {
      settle(err);
      return;
    }
    if (ret && typeof ret.then === 'function') {
      ret.then(null, function (err) {
        settle(err || new Error('app_channel handler rejected'));
      });
    }
  });
}

function runProcess(exe, args, timeoutMs) {
  const child = child_process.spawn(exe, args, {
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true
  });

  let stdout = '';
  let stderr = '';
  let timedOut = false;
  let settled = false;
  let timer = null;
  let killTimer = null;

  const result = new Promise(function (resolve) {
    function finish(code, signal) {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      clearTimeout(killTimer);
      resolve({
        code: code,
        signal: signal,
        stdout: stdout,
        stderr: stderr,
        timedOut: timedOut
      });
    }

    child.stdout.on('data', function (d) {
      stdout += d.toString();
    });
    child.stderr.on('data', function (d) {
      stderr += d.toString();
    });
    child.on('error', function (err) {
      stderr += err.message + '\n';
      finish(null, null);
    });
    child.on('close', function (code, signal) {
      finish(code, signal);
    });

    if (timeoutMs > 0) {
      timer = setTimeout(function () {
        timedOut = true;
        try {
          child.kill();
        } catch (e) {
          // the child may already have exited
        }
        killTimer = setTimeout(function () {
          finish(null, 'KILL-TIMEOUT');
        }, 3000);
      }, timeoutMs);
    }
  });

  result.child = child;
  return result;
}

function waitForPort(port, host, timeoutMs) {
  const deadline = Date.now() + timeoutMs;

  return new Promise(function (resolve, reject) {
    function retry() {
      if (Date.now() >= deadline) {
        reject(new Error('port ' + host + ':' + port + ' not listening after ' + timeoutMs + ' ms'));
        return;
      }
      setTimeout(attempt, 100);
    }

    function attempt() {
      const socket = net.connect({ port: port, host: host });
      let done = false;

      function failed() {
        if (done) {
          return;
        }
        done = true;
        socket.destroy();
        retry();
      }

      socket.once('connect', function () {
        if (done) {
          return;
        }
        done = true;
        socket.destroy();
        resolve();
      });
      socket.once('error', failed);
      socket.once('close', failed);
    }

    attempt();
  });
}

const RAW_PING = 12;
const RAW_PONG = 13;

function encodeRawFrame(opcode, value) {
  const body = value === undefined || value === null
    ? Buffer.alloc(0)
    : Buffer.from(JSON.stringify(value), 'utf8');
  const frame = Buffer.alloc(5 + body.length);
  frame.writeUIntLE(1 + body.length, 0, 4);
  frame.writeUInt8(opcode, 4);
  body.copy(frame, 5);
  return frame;
}

// Minimal FCast TCP client that speaks the raw wire format, with no SDK in between.
// Frames are size (u32 LE, opcode + body length) | opcode (u8) | UTF-8 JSON body (may be empty).
function rawConnect(host, port) {
  return new Promise(function (resolve, reject) {
    const socket = net.connect({ port: port, host: host });
    const emitter = new EventEmitter();
    const packets = [];
    const cursors = {};
    let pending = Buffer.alloc(0);
    let connected = false;

    function onFrame(opcode, body) {
      let value = null;
      if (body.length) {
        try {
          value = JSON.parse(body);
        } catch (e) {
          value = null;
        }
      }
      const pkt = { opcode: opcode, body: body, value: value };
      packets.push(pkt);
      if (opcode === RAW_PING) {
        socket.write(encodeRawFrame(RAW_PONG));
      }
      emitter.emit('packet', pkt);
    }

    socket.on('data', function (chunk) {
      pending = Buffer.concat([pending, chunk]);
      while (pending.length >= 4) {
        const size = pending.readUInt32LE(0);
        if (pending.length < 4 + size) {
          break;
        }
        if (size >= 1) {
          onFrame(pending.readUInt8(4), pending.slice(5, 4 + size).toString('utf8'));
        }
        pending = pending.slice(4 + size);
      }
    });

    socket.on('connect', function () {
      connected = true;
      resolve(client);
    });

    socket.on('error', function (err) {
      if (!connected) {
        reject(err);
      }
    });

    function waitPacket(opcode, predicate, timeoutMs) {
      return new Promise(function (resolveWait, rejectWait) {
        let timer = null;

        function matches(pkt) {
          if (pkt.opcode !== opcode) {
            return false;
          }
          if (typeof predicate !== 'function') {
            return true;
          }
          try {
            return !!predicate(pkt.value);
          } catch (err) {
            return false;
          }
        }

        function finish() {
          emitter.removeListener('packet', check);
          if (timer) {
            clearTimeout(timer);
          }
        }

        function check() {
          const start = cursors[opcode] || 0;
          for (let i = start; i < packets.length; i++) {
            if (matches(packets[i])) {
              cursors[opcode] = i + 1;
              finish();
              resolveWait(packets[i]);
              return true;
            }
          }
          return false;
        }

        if (check()) {
          return;
        }

        emitter.on('packet', check);
        timer = setTimeout(function () {
          finish();
          rejectWait(new Error('timed out after ' + timeoutMs + ' ms waiting for opcode ' + opcode));
        }, timeoutMs);
      });
    }

    const client = {
      packets: packets,
      send: function (opcode, value) {
        if (!socket.destroyed) {
          socket.write(encodeRawFrame(opcode, value));
        }
      },
      waitPacket: waitPacket,
      close: function () {
        socket.destroy();
      }
    };
  });
}

module.exports = {
  loadService: loadService,
  subscribe: subscribe,
  appRequest: appRequest,
  runProcess: runProcess,
  waitForPort: waitForPort,
  rawConnect: rawConnect,
  log: log
};
