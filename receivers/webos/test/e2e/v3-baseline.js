#!/usr/bin/env node
'use strict';

const net = require('net');
const harness = require('./harness');

const PORT = 46899;
const HOST = '127.0.0.1';
const VIDEO_URL = 'http://127.0.0.1:9/e2e.mp4';
const RAW_URL = 'http://127.0.0.1:9/raw-v3.mp4';
const RAW_V2_URL = 'http://127.0.0.1:9/raw-v2.mp4';
const SENDER_TIMEOUT = 15000;
const EVENT_TIMEOUT = 15000;
const RAW_TIMEOUT = 5000;
const RAW_PING_TIMEOUT = 6000;
const PRE_NEGOTIATION_MIN_MS = 9000;
const PRE_NEGOTIATION_MAX_MS = 12000;
const FAST_TIMEOUT = 120000;
const FAST_CASES = ['connect_version_2', 'connect_version_3', 'heartbeat'];

const OP = {
  Play: 1,
  Pause: 2,
  Resume: 3,
  Stop: 4,
  Seek: 5,
  PlaybackUpdate: 6,
  SetVolume: 8,
  SetSpeed: 10,
  Version: 11,
  Initial: 14,
  Ping: 12
};

const SENDER_CASES = [
  { name: 'v3-pause', args: ['pause'], event: 'pause' },
  { name: 'v3-resume', args: ['resume'], event: 'resume' },
  {
    name: 'v3-seek',
    args: ['seek', '--timestamp', '12'],
    event: 'seek',
    check: function (v) { return !!v && v.time === 12; }
  },
  {
    name: 'v3-setvolume',
    args: ['set-volume', '--volume', '0.25'],
    event: 'setvolume',
    check: function (v) { return !!v && v.volume === 0.25; }
  },
  {
    name: 'v3-setspeed',
    args: ['set-speed', '--speed', '2'],
    event: 'setspeed',
    check: function (v) { return !!v && v.speed === 2; }
  },
  { name: 'v3-stop', args: ['stop'], event: 'stop' }
];

const tally = { passed: 0, total: 0 };

function usage() {
  return 'usage: node v3-baseline.js --service-dir <dir> --sender <fcast.exe> [--fast <fast.exe>]';
}

function parseArgs(argv) {
  const opts = { serviceDir: null, sender: null, fast: null };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg !== '--service-dir' && arg !== '--sender' && arg !== '--fast') {
      throw new Error('unknown argument: ' + arg);
    }
    if (i + 1 >= argv.length) {
      throw new Error(arg + ' needs a value');
    }
    i++;
    if (arg === '--service-dir') {
      opts.serviceDir = argv[i];
    } else if (arg === '--sender') {
      opts.sender = argv[i];
    } else {
      opts.fast = argv[i];
    }
  }
  return opts;
}

function tail(text) {
  return String(text || '').trim().slice(-300);
}

function noop() {}

// Connects without sending anything, reads the receiver's Version frame and waits for the receiver
// to close the connection. Resolves with the close time, the bytes that followed the Version frame,
// or timedOut when the receiver is still open at PRE_NEGOTIATION_MAX_MS.
function preNegotiationTimeout() {
  return new Promise(function (resolve, reject) {
    const socket = net.connect({ host: HOST, port: PORT });
    const start = Date.now();
    let received = Buffer.alloc(0);
    let timedOut = false;
    const cutoff = setTimeout(function () {
      timedOut = true;
      socket.destroy();
    }, PRE_NEGOTIATION_MAX_MS);

    socket.on('error', noop);
    socket.on('data', function (chunk) {
      received = Buffer.concat([received, chunk]);
    });
    socket.on('close', function () {
      clearTimeout(cutoff);
      const closedAt = Date.now();
      if (received.length < 5) {
        reject(new Error('no Version frame from the receiver'));
        return;
      }
      const frameEnd = 4 + received.readUInt32LE(0);
      if (received.length < frameEnd || received[4] !== OP.Version) {
        reject(new Error('first packet from the receiver is not Version'));
        return;
      }
      resolve({
        start: start,
        closedAt: closedAt,
        timedOut: timedOut,
        after: received.length - frameEnd
      });
    });
  });
}

function delay(ms) {
  return new Promise(function (resolve) {
    setTimeout(resolve, ms);
  });
}

function report(ok, name, detail) {
  tally.total++;
  if (ok) {
    tally.passed++;
  }
  let line;
  if (ok) {
    line = 'PASS ' + name + (detail ? ' ' + detail : '');
  } else {
    line = 'FAIL ' + name + ': ' + detail;
  }
  console.log(line);
  return ok;
}

async function attempt(name, fn) {
  try {
    const detail = await fn();
    return report(true, name, detail || '');
  } catch (err) {
    return report(false, name, err && err.message ? err.message : String(err));
  }
}

function runSender(opts, sub, args, eventName, predicate) {
  const waiting = sub.waitFor(eventName, predicate, EVENT_TIMEOUT);
  const proc = harness.runProcess(opts.sender, ['--host', HOST].concat(args), SENDER_TIMEOUT);
  const child = proc.child;

  return new Promise(function (resolve, reject) {
    let settled = false;

    function settle(fn, value) {
      if (settled) {
        return;
      }
      settled = true;
      try {
        child.kill();
      } catch (e) {
        // the sender may already have exited
      }
      fn(value);
    }

    waiting.then(
      function (evt) {
        settle(resolve, evt);
      },
      function (err) {
        settle(reject, err);
      }
    );

    proc.then(function (res) {
      if (res.code !== 0) {
        settle(reject, new Error(
          'sender exited with code ' + res.code + ' before event "' + eventName + '"' +
          (tail(res.stderr) ? ': ' + tail(res.stderr) : '')
        ));
      }
    });
  });
}

async function fastStep(opts) {
  const res = await harness.runProcess(
    opts.fast,
    ['--host', HOST, '--port', String(PORT), 'run',
      'connect_version_2', 'connect_version_3', 'heartbeat'],
    FAST_TIMEOUT
  );
  const output = (res.stdout + res.stderr).replace(/\x1b\[[0-9;]*m/g, '');
  const lines = output.split(/\r?\n/);
  console.log(lines.slice(-30).join('\n'));

  const problems = [];
  if (res.timedOut) {
    problems.push('timed out after ' + FAST_TIMEOUT + ' ms');
  }
  if (res.code !== 0) {
    problems.push('exit code ' + res.code + (res.signal ? ' signal ' + res.signal : ''));
  }
  const bad = [];
  const ignored = [];
  lines.forEach(function (l) {
    const m = /test (\S+) \.\.\. (FAILED|SKIPPED)/.exec(l);
    if (!m) {
      return;
    }
    if (FAST_CASES.indexOf(m[1]) !== -1) {
      bad.push(l.trim());
    } else {
      ignored.push(m[1] + ':' + m[2]);
    }
  });
  if (bad.length) {
    problems.push('failed or skipped: ' + bad.join(' | '));
  }
  const missing = FAST_CASES.filter(function (name) {
    const okLine = new RegExp('^\\s*test ' + name + ' \\.\\.\\. OK\\s*$');
    return !lines.some(function (l) {
      return okLine.test(l);
    });
  });
  if (missing.length) {
    problems.push('no OK line for: ' + missing.join(', '));
  }
  if (problems.length) {
    throw new Error(problems.join('; '));
  }
  return 'cases=' + FAST_CASES.join(',') + (ignored.length ? ' ignored=' + ignored.join('|') : '');
}

async function main(opts) {
  let service = null;
  let sub = null;
  let blocked = null;
  let maxVersion = null;
  let rawV3 = null;
  let rawV2 = null;
  let rawV3Ready = false;

  const step = async function (name, fn) {
    if (blocked) {
      return report(false, name, 'skipped: ' + blocked);
    }
    return attempt(name, fn);
  };

  const gate = function (ok, reason) {
    if (!ok && !blocked) {
      blocked = reason;
    }
    return ok;
  };

  gate(await step('service-load', async function () {
    service = harness.loadService(opts.serviceDir);
    return '';
  }), 'service-load failed');

  gate(await step('tcp-listen', function () {
    return harness.waitForPort(PORT, HOST, 10000).then(function () {
      return 'port ' + PORT;
    });
  }), 'tcp-listen failed');

  gate(await step('service-registered', async function () {
    const missing = ['service_channel', 'app_channel'].filter(function (n) {
      return !service.handlers[n];
    });
    if (missing.length) {
      throw new Error('not registered: ' + missing.join(', '));
    }
    return 'service_channel app_channel';
  }), 'service-registered failed');

  gate(await step('subscribe', async function () {
    sub = harness.subscribe(service);
    return '';
  }), 'subscribe failed');

  gate(await step('connection-info', async function () {
    const resp = await harness.appRequest(service, 'get_connection_info', {});
    const v = resp && resp.value ? resp.value.maxVersion : undefined;
    console.log('maxVersion ' + v);
    if (v !== 3 && v !== 4) {
      throw new Error('maxVersion is ' + v + ', expected 3 or 4');
    }
    maxVersion = v;
    return 'maxVersion=' + v;
  }), 'connection-info failed');

  if (maxVersion === 3) {
    let playEvent = null;
    const playOk = await step('v3-play', async function () {
      const evt = await runSender(
        opts,
        sub,
        ['play', '--mime-type', 'video/mp4', '--url', VIDEO_URL, '--timestamp', '5', '--speed', '1.5', '--volume', '0.5'],
        'play',
        function (v) { return !!v && !!v.rendererMessage && v.rendererMessage.url === VIDEO_URL; }
      );
      playEvent = evt;
      const rm = evt.value.rendererMessage;
      if (rm.container !== 'video/mp4') {
        throw new Error('container is ' + rm.container + ', expected video/mp4');
      }
      return 'time=' + rm.time + ' speed=' + rm.speed + ' volume=' + rm.volume;
    });

    if (!playOk || !playEvent) {
      report(false, 'simulated-playback-update', 'skipped: v3-play did not receive a play event');
    } else {
      await step('simulated-playback-update', async function () {
        const resp = await harness.appRequest(service, 'send_playback_update', {
          generationTime: Date.now(),
          time: 5,
          duration: 60,
          state: 1,
          speed: 1.5
        });
        return 'response=' + (resp ? 'yes' : 'none');
      });
    }

    for (const c of SENDER_CASES) {
      await step(c.name, async function () {
        await runSender(opts, sub, c.args, c.event, c.check);
        return '';
      });
    }
  } else if (maxVersion === 4) {
    console.log('SKIP terminal-sender-v3: receiver announces v4; the SDK sender requires the fingerprint (covered by the v4 e2e)');
  }

  function rawStep(name, opcode, body, event, predicate) {
    return step(name, async function () {
      if (!rawV3Ready) {
        throw new Error('no raw v3 session');
      }
      const waiting = sub.waitFor(event, predicate, EVENT_TIMEOUT);
      waiting.catch(noop);
      rawV3.send(opcode, body);
      await waiting;
      return '';
    });
  }

  try {
    await step('raw-v3-version', async function () {
      rawV3 = await harness.rawConnect(HOST, PORT);
      const versionPkt = await rawV3.waitPacket(OP.Version, null, EVENT_TIMEOUT);
      const receiverVersion = versionPkt.value ? versionPkt.value.version : undefined;
      const initialWait = rawV3.waitPacket(OP.Initial, null, EVENT_TIMEOUT);
      initialWait.catch(noop);
      rawV3.send(OP.Version, { version: 3 });
      const initial = await initialWait;
      rawV3Ready = true;
      return 'receiverVersion=' + receiverVersion + ' initial=' + JSON.stringify(initial.value);
    });

    await rawStep('raw-v3-play', OP.Play,
      { container: 'video/mp4', url: RAW_URL, time: 7, speed: 1.25, volume: 0.4, headers: { 'X-Test': '1' } },
      'play',
      function (v) { return !!v && !!v.rendererMessage && v.rendererMessage.url === RAW_URL; });

    // Commands sent while media is loading are held until the player reports it playing.
    await step('raw-v3-player-playing', async function () {
      if (!rawV3Ready) {
        throw new Error('no raw v3 session');
      }
      const waiting = rawV3.waitPacket(OP.PlaybackUpdate,
        function (v) { return !!v && v.state === 1 && v.time === 7; }, RAW_TIMEOUT);
      waiting.catch(noop);
      await harness.appRequest(service, 'send_playback_update', {
        generationTime: Date.now(),
        time: 7,
        duration: 60,
        state: 1,
        speed: 1.25
      });
      await waiting;
      return '';
    });

    await rawStep('raw-v3-pause', OP.Pause, null, 'pause', null);
    await rawStep('raw-v3-resume', OP.Resume, null, 'resume', null);
    await rawStep('raw-v3-seek', OP.Seek, { time: 21 }, 'seek',
      function (v) { return !!v && v.time === 21; });
    await rawStep('raw-v3-setvolume', OP.SetVolume, { volume: 0.3 }, 'setvolume',
      function (v) { return !!v && v.volume === 0.3; });
    await rawStep('raw-v3-setspeed', OP.SetSpeed, { speed: 0.75 }, 'setspeed',
      function (v) { return !!v && v.speed === 0.75; });
    await step('raw-v3-playback-update-relay', async function () {
      if (!rawV3Ready) {
        throw new Error('no raw v3 session');
      }
      const waiting = rawV3.waitPacket(OP.PlaybackUpdate,
        function (v) { return !!v && v.time === 9; }, RAW_TIMEOUT);
      waiting.catch(noop);
      await harness.appRequest(service, 'send_playback_update', {
        generationTime: Date.now(),
        time: 9,
        duration: 60,
        state: 1,
        speed: 1
      });
      const pkt = await waiting;
      return 'time=' + pkt.value.time;
    });

    await rawStep('raw-v3-stop', OP.Stop, null, 'stop', null);

    await step('raw-v2-legacy', async function () {
      rawV2 = await harness.rawConnect(HOST, PORT);
      const waiting = sub.waitFor('play',
        function (v) { return !!v && !!v.rendererMessage && v.rendererMessage.url === RAW_V2_URL; },
        EVENT_TIMEOUT);
      waiting.catch(noop);
      rawV2.send(OP.Play, { container: 'video/mp4', url: RAW_V2_URL, time: 2 });
      await waiting;
      await harness.appRequest(service, 'send_playback_update', {
        generationTime: Date.now(),
        time: 4,
        duration: 60,
        state: 1,
        speed: 1
      });
      const pkt = await rawV2.waitPacket(OP.PlaybackUpdate,
        function (v) { return !!v && v.time === 4; }, RAW_TIMEOUT);
      return 'time=' + pkt.value.time;
    });

    await step('raw-v2-legacy-heartbeat', async function () {
      if (!rawV2) {
        throw new Error('no raw v2 legacy session');
      }
      const ping = await rawV2.waitPacket(OP.Ping, null, RAW_PING_TIMEOUT);
      await delay(1000);
      const waitingPause = sub.waitFor('pause', null, RAW_TIMEOUT);
      waitingPause.catch(noop);
      rawV2.send(OP.Pause, null);
      await waitingPause;
      await harness.appRequest(service, 'send_playback_update', {
        generationTime: Date.now(),
        time: 6,
        duration: 60,
        state: 1,
        speed: 1
      });
      const pkt = await rawV2.waitPacket(OP.PlaybackUpdate,
        function (v) { return !!v && v.time === 6; }, RAW_TIMEOUT);
      return 'ping=' + ping.opcode + ' time=' + pkt.value.time;
    });
  } finally {
    if (rawV3) {
      rawV3.close();
    }
    if (rawV2) {
      rawV2.close();
    }
  }

  await step('pre-negotiation-timeout', async function () {
    const res = await preNegotiationTimeout();
    if (res.timedOut) {
      throw new Error('connection still open ' + PRE_NEGOTIATION_MAX_MS + ' ms after connect');
    }
    const at = res.closedAt - res.start;
    if (at < PRE_NEGOTIATION_MIN_MS || at > PRE_NEGOTIATION_MAX_MS) {
      throw new Error('closed ' + at + ' ms after connect, expected ' + PRE_NEGOTIATION_MIN_MS + ' to ' + PRE_NEGOTIATION_MAX_MS + ' ms');
    }
    if (res.after !== 0) {
      throw new Error(res.after + ' bytes received after the Version frame');
    }
    return 'after ' + at + ' ms';
  });

  if (opts.fast) {
    await step('fast-run', function () {
      return fastStep(opts);
    });
  }
}

let opts = null;
try {
  opts = parseArgs(process.argv.slice(2));
} catch (err) {
  console.error(err.message);
  console.error(usage());
  process.exit(2);
}
if (!opts.serviceDir || !opts.sender) {
  console.error(usage());
  process.exit(2);
}

console.log('node ' + process.version);

main(opts)
  .catch(function (err) {
    report(false, 'harness', err && err.message ? err.message : String(err));
  })
  .then(function () {
    const code = tally.total > 0 && tally.passed === tally.total ? 0 : 1;
    process.stdout.write('RESULT ' + tally.passed + '/' + tally.total + '\n', function () {
      process.exit(code);
    });
  });
