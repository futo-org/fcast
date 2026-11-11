'use strict';

// Serves the compiled webOS frontend to a desktop browser and connects it to the service loaded in
// this process. The page runs unchanged: only the two webOS globals it depends on are simulated.
// `PalmServiceBridge` forwards luna calls to the handlers the service registered on the
// webos-service stub, and `PalmSystem` reports a fixed device.

const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const url = require('url');
const child_process = require('child_process');

const SERVICE_URI = 'luna://com.futo.fcast.receiver.service/';

const CONTENT_TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'application/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.mp4': 'video/mp4',
  '.ttf': 'font/ttf',
  '.woff2': 'font/woff2',
  '.txt': 'text/plain; charset=utf-8'
};

// Runs in every frame before the page scripts.
function pageStub() {
  var nextId = 0;

  function PalmServiceBridge() {
    this.onservicecallback = null;
    this.source = null;
  }

  PalmServiceBridge.prototype.call = function (uri, params) {
    var self = this;
    var id = String(Date.now()) + '-' + String(nextId++);
    var query = '/__bridge/call?id=' + encodeURIComponent(id) +
      '&uri=' + encodeURIComponent(uri) + '&params=' + encodeURIComponent(params || '{}');
    var subscription = false;
    try {
      subscription = JSON.parse(params || '{}').subscribe === true;
    } catch (e) { /* not a subscription */ }

    var source = new EventSource(query);
    this.source = source;
    source.onmessage = function (event) {
      if (self.onservicecallback) {
        self.onservicecallback(event.data);
      }
    };
    source.addEventListener('done', function () {
      source.close();
    });
    source.onerror = function () {
      if (subscription) {
        // A subscription that loses its connection is opened again, as the platform does for
        // a request made with `resubscribe`. EventSource reconnects by itself.
        console.warn('bridge: subscription to ' + uri + ' interrupted, reconnecting');
      } else {
        source.close();
      }
    };
  };

  PalmServiceBridge.prototype.cancel = function () {
    if (this.source) {
      this.source.close();
      this.source = null;
    }
  };

  window.PalmServiceBridge = PalmServiceBridge;
  window.PalmSystem = {
    identifier: 'com.futo.fcast.receiver',
    launchParams: '{}',
    deviceInfo: JSON.stringify({
      modelName: 'TEST',
      platformVersion: '26.0.0',
      platformVersionMajor: 26,
      platformVersionMinor: 0,
      platformVersionDot: 0
    }),
    country: '{}',
    locale: 'en-US',
    isKeyboardVisible: false,
    activate: function () {},
    deactivate: function () {},
    stageReady: function () {},
    platformBack: function () {}
  };

  ['log', 'info', 'warn', 'error', 'debug'].forEach(function (level) {
    var original = console[level];
    console[level] = function () {
      var parts = [];
      for (var i = 0; i < arguments.length; i++) {
        var arg = arguments[i];
        try {
          parts.push(typeof arg === 'string' ? arg : JSON.stringify(arg));
        } catch (e) {
          parts.push(String(arg));
        }
      }
      try {
        navigator.sendBeacon('/__bridge/log', level + ' ' + location.pathname + ' ' + parts.join(' '));
      } catch (e) { /* logging must never break the page */ }
      original.apply(console, arguments);
    };
  });

  window.addEventListener('error', function (event) {
    console.error('uncaught: ' + event.message + ' at ' + event.filename + ':' + event.lineno);
  });
}

// Answers for the platform services the frontend asks besides the FCast service.
function platformResponse(uri) {
  if (uri.indexOf('connectionmanager') !== -1) {
    return {
      returnValue: true,
      isInternetConnectionAvailable: true,
      wired: { state: 'connected', ipAddress: '127.0.0.1' },
      wifi: { state: 'disconnected' },
      wifiDirect: { state: 'disconnected' }
    };
  }
  return { returnValue: true };
}

function startFrontendHost(serviceInstance, distDir, options) {
  const opts = options || {};
  const root = path.resolve(distDir);
  const logs = [];
  const stubSource = '(' + pageStub.toString() + ')();';

  function onLog(line) {
    logs.push(line);
    if (opts.onLog) {
      opts.onLog(line);
    }
  }

  function serveStatic(req, res, pathname) {
    let relative;
    try {
      relative = decodeURIComponent(pathname);
    } catch (e) {
      res.writeHead(400);
      res.end();
      return;
    }
    if (relative.charAt(relative.length - 1) === '/') {
      relative += 'index.html';
    }
    const file = path.resolve(root, '.' + relative);
    if (file !== root && file.indexOf(root + path.sep) !== 0) {
      res.writeHead(403);
      res.end();
      return;
    }

    fs.readFile(file, function (err, data) {
      if (err) {
        res.writeHead(404);
        res.end('not found');
        return;
      }
      const ext = path.extname(file).toLowerCase();
      let body = data;
      if (ext === '.html') {
        body = Buffer.from(data.toString('utf8').replace(/<head>/i, '<head><script src="/__bridge/palm.js"></script>'), 'utf8');
      }
      res.writeHead(200, {
        'Content-Type': CONTENT_TYPES[ext] || 'application/octet-stream',
        'Content-Length': body.length,
        'Cache-Control': 'no-store'
      });
      res.end(body);
    });
  }

  function serveCall(req, res, query) {
    const uri = String(query.uri || '');
    let params;
    try {
      params = JSON.parse(String(query.params || '{}'));
    } catch (e) {
      res.writeHead(400);
      res.end();
      return;
    }

    res.writeHead(200, {
      'Content-Type': 'text/event-stream',
      'Cache-Control': 'no-store',
      'Connection': 'keep-alive'
    });

    let open = true;
    const subscription = params.subscribe === true;
    // Keeps an idle subscription from being dropped as an inactive connection.
    const keepAlive = subscription ? setInterval(function () {
      if (open) {
        res.write(': keep-alive\n\n');
      }
    }, 10000) : null;
    res.on('close', function () {
      clearInterval(keepAlive);
      if (subscription) {
        onLog('warn bridge: subscription connection for ' + uri + ' closed');
      }
    });

    function respond(obj) {
      if (!open) {
        return;
      }
      res.write('data: ' + JSON.stringify(obj) + '\n\n');
      if (!subscription) {
        open = false;
        res.end('event: done\ndata: {}\n\n');
      }
    }

    if (uri.indexOf(SERVICE_URI) !== 0) {
      respond(platformResponse(uri));
      return;
    }

    const method = uri.slice(SERVICE_URI.length);
    const handler = serviceInstance.handlers[method];
    if (!handler) {
      respond({ returnValue: false, errorCode: -1, errorText: 'unknown method ' + method });
      return;
    }

    const message = {
      isSubscription: subscription,
      payload: params,
      respond: respond,
      cancel: function () {}
    };

    req.on('close', function () {
      if (open && subscription && typeof handler.cancelCallback === 'function') {
        open = false;
        handler.cancelCallback({ payload: params, respond: function () {} });
      }
      open = false;
    });

    try {
      handler.callback(message);
    } catch (err) {
      onLog('error bridge ' + method + ' threw ' + (err && err.stack ? err.stack : err));
      respond({ returnValue: false, errorCode: -1, errorText: String(err) });
    }
  }

  const server = http.createServer(function (req, res) {
    const parsed = url.parse(req.url, true);

    if (parsed.pathname === '/__bridge/palm.js') {
      res.writeHead(200, { 'Content-Type': CONTENT_TYPES['.js'], 'Cache-Control': 'no-store' });
      res.end(stubSource);
    } else if (parsed.pathname === '/__bridge/call') {
      serveCall(req, res, parsed.query);
    } else if (parsed.pathname === '/__bridge/log') {
      const chunks = [];
      req.on('data', function (chunk) { chunks.push(chunk); });
      req.on('end', function () {
        onLog(Buffer.concat(chunks).toString('utf8'));
        res.writeHead(204);
        res.end();
      });
    } else {
      serveStatic(req, res, parsed.pathname);
    }
  });

  // Subscriptions are long-lived responses; no inactivity limit applies to them.
  server.timeout = 0;
  server.keepAliveTimeout = 0;

  return new Promise(function (resolve, reject) {
    server.on('error', reject);
    server.listen(0, '127.0.0.1', function () {
      const port = server.address().port;
      resolve({
        port: port,
        url: 'http://127.0.0.1:' + port + '/index.html',
        logs: logs,
        close: function () {
          server.close();
        }
      });
    });
  });
}

function findBrowser() {
  const candidates = [
    process.env.FCAST_TEST_BROWSER,
    // Chrome first: with Edge, playback stopped advancing partway through a full run.
    'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
    'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
    'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser',
    '/usr/bin/google-chrome'
  ];
  for (let i = 0; i < candidates.length; i++) {
    if (candidates[i] && fs.existsSync(candidates[i])) {
      return candidates[i];
    }
  }
  return null;
}

function removeDirectory(dir) {
  fs.readdirSync(dir).forEach(function (name) {
    const entry = path.join(dir, name);
    if (fs.lstatSync(entry).isDirectory()) {
      removeDirectory(entry);
    } else {
      fs.unlinkSync(entry);
    }
  });
  fs.rmdirSync(dir);
}

// Opens the page in a headless Chromium browser with its own empty profile. `close` stops the
// browser and deletes the profile; it resolves when that is done.
function launchBrowser(pageUrl, browserPath) {
  const exe = browserPath || findBrowser();
  if (!exe) {
    throw new Error('no Chromium browser found; set FCAST_TEST_BROWSER');
  }
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'fcast-e2e-browser-'));
  const child = child_process.spawn(exe, [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    '--mute-audio',
    '--autoplay-policy=no-user-gesture-required',
    // Keep the page's timers and media at full rate although its window is not on screen.
    '--disable-background-timer-throttling',
    '--disable-renderer-backgrounding',
    '--disable-backgrounding-occluded-windows',
    '--disable-background-media-suspend',
    '--disable-features=IntensiveWakeUpThrottling,CalculateNativeWinOcclusion',
    '--window-size=1920,1080',
    '--user-data-dir=' + profile,
    pageUrl
  ], { stdio: 'ignore' });

  const exited = new Promise(function (resolve) {
    child.on('exit', resolve);
    child.on('error', resolve);
  });

  return {
    process: child,
    close: function () {
      try {
        child.kill();
      } catch (e) { /* already gone */ }

      // The browser's helper processes release the profile a moment after it exits.
      return exited.then(function () {
        return new Promise(function (resolve) {
          let attempts = 0;
          (function attempt() {
            try {
              removeDirectory(profile);
              resolve();
            } catch (e) {
              if (++attempts >= 20) {
                resolve();
              } else {
                setTimeout(attempt, 250);
              }
            }
          })();
        });
      });
    }
  };
}

module.exports = {
  startFrontendHost: startFrontendHost,
  launchBrowser: launchBrowser,
  findBrowser: findBrowser
};
