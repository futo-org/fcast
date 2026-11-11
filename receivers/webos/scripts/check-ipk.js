#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

const APP_FILES = [
  'appinfo.json',
  'index.html',
  'main.js',
  'assets/fonts/inter.css',
  'assets/fonts/outfit.css',
  'assets/fonts/Inter/OFL.txt',
  'assets/fonts/Outfit/OFL.txt',
  'assets/video/background.mp4'
];

const FONT_CSS = ['assets/fonts/inter.css', 'assets/fonts/outfit.css'];

const FORBIDDEN_NAME = /\.(pem|key)$/i;
const FORBIDDEN_SEGMENT = 'identity';
const FORBIDDEN_MARKERS = [
  '-----BEGIN PRIVATE KEY-----',
  '-----BEGIN RSA PRIVATE KEY-----',
  '-----BEGIN EC PRIVATE KEY-----'
];

function usage() {
  console.error('usage: node check-ipk.js <file.ipk> [--extract <dir>]');
  process.exitCode = 2;
}

function parseAr(buf) {
  if (buf.length < 8 || buf.toString('latin1', 0, 8) !== '!<arch>\n') {
    throw new Error('not an ar archive');
  }
  const members = [];
  let off = 8;
  while (off + 60 <= buf.length) {
    const name = buf.toString('latin1', off, off + 16).replace(/\s+$/, '').replace(/\/$/, '');
    const size = parseInt(buf.toString('latin1', off + 48, off + 58).trim(), 10);
    const magic = buf.toString('latin1', off + 58, off + 60);
    if (magic !== '`\n' || isNaN(size) || size < 0) {
      throw new Error('bad ar member header at offset ' + off);
    }
    const dataStart = off + 60;
    const dataEnd = dataStart + size;
    if (dataEnd > buf.length) {
      throw new Error('truncated ar member ' + name);
    }
    members.push({ name: name, data: buf.slice(dataStart, dataEnd) });
    off = dataEnd + (size % 2);
  }
  return members;
}

function readStr(buf, start, len) {
  const slice = buf.slice(start, start + len);
  const nul = slice.indexOf(0);
  return slice.toString('utf8', 0, nul === -1 ? slice.length : nul);
}

function isZeroBlock(header) {
  for (let i = 0; i < header.length; i++) {
    if (header[i] !== 0) {
      return false;
    }
  }
  return true;
}

function parseSize(header) {
  if (header[124] & 0x80) {
    let value = 0;
    for (let i = 125; i < 136; i++) {
      value = value * 256 + header[i];
    }
    return value;
  }
  const s = readStr(header, 124, 12).trim();
  if (s === '') {
    return 0;
  }
  if (!/^[0-7]+$/.test(s)) {
    throw new Error('bad tar size field: ' + JSON.stringify(s));
  }
  return parseInt(s, 8);
}

function parsePax(data) {
  const out = {};
  let pos = 0;
  while (pos < data.length) {
    const sp = data.indexOf(0x20, pos);
    if (sp === -1) {
      break;
    }
    const len = parseInt(data.toString('utf8', pos, sp), 10);
    if (!(len > 0)) {
      break;
    }
    const rec = data.toString('utf8', sp + 1, pos + len - 1);
    const eq = rec.indexOf('=');
    if (eq !== -1) {
      out[rec.slice(0, eq)] = rec.slice(eq + 1);
    }
    pos += len;
  }
  return out;
}

function normalizePath(p) {
  let out = p;
  while (out.indexOf('./') === 0) {
    out = out.slice(2);
  }
  return out;
}

function parseTar(buf) {
  const entries = [];
  let off = 0;
  let longName = null;
  let paxPath = null;

  while (off + 512 <= buf.length) {
    const header = buf.slice(off, off + 512);
    if (isZeroBlock(header)) {
      break;
    }
    const size = parseSize(header);
    const type = header[156] === 0 ? '0' : String.fromCharCode(header[156]);
    const dataStart = off + 512;
    const data = buf.slice(dataStart, dataStart + size);
    off = dataStart + Math.ceil(size / 512) * 512;

    if (type === 'L') {
      longName = readStr(data, 0, data.length);
      continue;
    }
    if (type === 'x') {
      const rec = parsePax(data);
      if (rec.path) {
        paxPath = rec.path;
      }
      continue;
    }
    if (type === 'g' || type === 'K') {
      continue;
    }

    let name;
    if (paxPath) {
      name = paxPath;
    } else if (longName) {
      name = longName;
    } else {
      const base = readStr(header, 0, 100);
      const isPosixUstar = header.toString('latin1', 257, 263) === 'ustar\0';
      const prefix = isPosixUstar ? readStr(header, 345, 155) : '';
      name = prefix ? prefix + '/' + base : base;
    }
    longName = null;
    paxPath = null;

    if (type === '0') {
      entries.push({ path: normalizePath(name), size: size, data: data });
    }
  }
  return entries;
}

function isSafeRelPath(p) {
  if (!p || p.charAt(0) === '/' || p.indexOf('\\') !== -1 || /^[A-Za-z]:/.test(p)) {
    return false;
  }
  return p.split('/').every(function (seg) {
    return seg !== '..';
  });
}

function mkdirp(dir) {
  if (fs.existsSync(dir)) {
    return;
  }
  const parent = path.dirname(dir);
  if (parent !== dir) {
    mkdirp(parent);
  }
  fs.mkdirSync(dir);
}

function extractAll(entries, outDir) {
  const root = path.resolve(outDir);
  mkdirp(root);
  entries.forEach(function (e) {
    const target = path.resolve.apply(path, [root].concat(e.path.split('/')));
    if (target.indexOf(root + path.sep) !== 0) {
      throw new Error('path escapes extract dir: ' + e.path);
    }
    mkdirp(path.dirname(target));
    fs.writeFileSync(target, e.data);
  });
}

function cssRefs(text) {
  const refs = [];
  const re = /url\(\s*([^)]*?)\s*\)/gi;
  let m;
  while ((m = re.exec(text)) !== null) {
    const raw = m[1].replace(/^['"]|['"]$/g, '').trim();
    if (raw) {
      refs.push(raw);
    }
  }
  return refs;
}

function main() {
  const argv = process.argv.slice(2);
  let ipkPath = null;
  let extractDir = null;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--extract') {
      if (i + 1 >= argv.length) {
        usage();
        return;
      }
      extractDir = argv[++i];
    } else if (!ipkPath) {
      ipkPath = argv[i];
    } else {
      usage();
      return;
    }
  }
  if (!ipkPath) {
    usage();
    return;
  }

  let members;
  let entries;
  try {
    members = parseAr(fs.readFileSync(ipkPath));
    const dataMember = members.filter(function (m) {
      return m.name === 'data.tar.gz';
    })[0];
    if (!dataMember) {
      throw new Error('data.tar.gz member not found');
    }
    entries = parseTar(zlib.gunzipSync(dataMember.data));
  } catch (err) {
    console.error('cannot read ' + ipkPath + ': ' + err.message);
    process.exitCode = 1;
    return;
  }

  const byPath = new Map();
  entries.forEach(function (e) {
    byPath.set(e.path, e);
  });
  const paths = entries.map(function (e) {
    return e.path;
  });
  const pathSet = new Set(paths);

  console.log('IPK ' + ipkPath + ' files=' + entries.length);

  if (extractDir) {
    const bad = paths.filter(function (p) {
      return !isSafeRelPath(p);
    });
    if (bad.length) {
      console.error('refusing to extract unsafe path: ' + bad[0]);
      process.exitCode = 2;
      return;
    }
    extractAll(entries, extractDir);
    console.log('EXTRACTED ' + path.resolve(extractDir));
  }

  const problems = [];
  function ok(p) {
    console.log('OK ' + p);
  }
  function missing(desc) {
    problems.push(desc);
    console.log('MISSING ' + desc);
  }

  const appinfoPath = paths.filter(function (p) {
    return /(^|\/)appinfo\.json$/.test(p);
  })[0] || null;
  const appDir = appinfoPath === null
    ? null
    : (path.posix.dirname(appinfoPath) === '.' ? '' : path.posix.dirname(appinfoPath));

  function appPath(rel) {
    if (appDir !== null) {
      return appDir ? appDir + '/' + rel : rel;
    }
    const hit = paths.filter(function (p) {
      return p === rel || p.endsWith('/' + rel);
    })[0];
    return hit || null;
  }

  function resolveRef(cssPath, ref) {
    if (/^data:/i.test(ref) || /^https?:/i.test(ref) || /^\/\//.test(ref)) {
      return null;
    }
    const clean = ref.split('?')[0].split('#')[0];
    if (!clean) {
      return null;
    }
    if (clean.charAt(0) === '/') {
      const stripped = clean.replace(/^\/+/, '');
      return appDir ? appDir + '/' + stripped : stripped;
    }
    return path.posix.normalize(path.posix.join(path.posix.dirname(cssPath), clean));
  }

  // private key material must never be packaged
  entries.forEach(function (e) {
    const forbiddenName = FORBIDDEN_NAME.test(e.path) || e.path.split('/').indexOf(FORBIDDEN_SEGMENT) !== -1;
    const forbiddenContent = FORBIDDEN_MARKERS.some(function (marker) {
      return e.data.indexOf(marker) !== -1;
    });
    if (forbiddenName || forbiddenContent) {
      problems.push('forbidden ' + e.path);
      console.log('FORBIDDEN ' + e.path);
    }
  });

  // app files
  APP_FILES.forEach(function (rel) {
    const full = appPath(rel);
    if (full && pathSet.has(full)) {
      ok(full);
    } else {
      missing('app file ' + rel);
    }
  });

  // font css url() references
  FONT_CSS.forEach(function (rel) {
    const cssFull = appPath(rel);
    if (!cssFull || !byPath.has(cssFull)) {
      return;
    }
    const text = byPath.get(cssFull).data.toString('utf8');
    cssRefs(text).forEach(function (ref) {
      const target = resolveRef(cssFull, ref);
      if (!target) {
        return;
      }
      if (pathSet.has(target)) {
        ok(target);
      } else {
        missing(cssFull + ': ' + ref);
      }
    });
  });

  // icons
  [['assets/icons/app/', 'app icon .svg under assets/icons/app/'],
    ['assets/icons/player/', 'player icon .svg under assets/icons/player/']].forEach(function (pair) {
    const hit = paths.filter(function (p) {
      return p.indexOf(pair[0]) !== -1 && /\.svg$/i.test(p);
    })[0];
    if (hit) {
      ok(hit);
    } else {
      missing(pair[1]);
    }
  });

  // svg/png references from every packaged css file
  paths.filter(function (p) {
    return /\.css$/i.test(p);
  }).forEach(function (cssP) {
    const text = byPath.get(cssP).data.toString('utf8');
    cssRefs(text).forEach(function (ref) {
      const clean = ref.split(/[?#]/)[0];
      if (!/\.(svg|png)$/i.test(clean)) {
        return;
      }
      const target = resolveRef(cssP, ref);
      if (!target) {
        return;
      }
      if (pathSet.has(target)) {
        ok(target);
      } else {
        missing(cssP + ': ' + ref);
      }
    });
  });

  // service package
  const serviceMains = paths.filter(function (p) {
    if (path.posix.basename(p) !== 'main.js') {
      return false;
    }
    const dir = path.posix.dirname(p);
    return path.posix.basename(dir) === 'main' || p.indexOf('com.futo.fcast.receiver.service') !== -1;
  });
  let serviceMain = null;
  let servicePkg = null;
  for (let i = 0; i < serviceMains.length && !serviceMain; i++) {
    const dir = path.posix.dirname(serviceMains[i]);
    const pkg = path.posix.join(dir, 'package.json');
    const svc = path.posix.join(dir, 'services.json');
    if (pathSet.has(pkg) && pathSet.has(svc)) {
      serviceMain = serviceMains[i];
      servicePkg = pkg;
      ok(serviceMain);
      ok(pkg);
      ok(svc);
    }
  }
  if (!serviceMain) {
    missing('service main/main.js with package.json and services.json in the same directory');
  }

  // versions
  function readJson(p) {
    if (!p || !byPath.has(p)) {
      return null;
    }
    try {
      return JSON.parse(byPath.get(p).data.toString('utf8').replace(/^﻿/, ''));
    } catch (err) {
      return null;
    }
  }
  const appinfo = readJson(appinfoPath);
  const servicePackage = readJson(servicePkg);
  const appVersion = appinfo && appinfo.version ? appinfo.version : 'unknown';
  const serviceVersion = servicePackage && servicePackage.version ? servicePackage.version : 'unknown';
  console.log('VERSION app=' + appVersion + ' service=' + serviceVersion);

  if (problems.length === 0) {
    console.log('RESULT OK');
    process.exitCode = 0;
  } else {
    console.log('RESULT FAIL ' + problems.length);
    process.exitCode = 1;
  }
}

main();
