'use strict';

// Runs the upstream FAST conformance cases that need a real player against the packaged service
// and the compiled frontend. The service runs in this process on the webos-service stub; the
// frontend runs unchanged in a headless Chromium browser with the webOS bridge simulated
// (see frontend-host.js).
//
// usage: node player-fast.js --service-dir <dir> --frontend-dir <dir> --fast <fast.exe> --media <dir>
//                            [--cases a,b,...] [--list]

const path = require('path');
const harness = require('./harness.js');
const frontendHost = require('./frontend-host.js');

const HOST = '127.0.0.1';
const PORT = 46899;

// Cases that exercise the player through the frontend.
const PLAYER_CASES = [
  // v2 and v3
  'cast_photo_v2', 'cast_photos_v2', 'cast_photo_v3', 'cast_photos_v3',
  'cast_gif_v2', 'cast_gif_v3',
  'cast_video_v2', 'cast_video_v3',
  'cast_pause_resume_v2', 'cast_pause_resume_v3', 'cast_pause_during_load_v2',
  'subscribe_media_item_start_1', 'subscribe_media_item_start_2', 'subscribe_media_item_end',
  'cast_simple_playlist',
  'cast_photo_with_headers_v2', 'cast_photos_with_headers_v2', 'cast_photo_with_headers_v3', 'cast_photos_with_headers_v3',
  'cast_video_with_headers_v2', 'cast_video_with_headers_v3', 'cast_simple_playlist_with_headers',
  'cast_video_with_start_speed_volume_v3',
  'initial_play_data_v3', 'initial_play_data_playlist_v3',
  'play_local_file_refused_v3',
  'seek_v3', 'unsubscribe_event_v3',
  'stop_reports_idle_v2', 'stop_reports_idle_v3', 'end_reports_paused_v3', 'photo_reports_playing_v3',
  // v4 playback
  'cast_gif_v4',
  'cast_video_v4', 'cast_video_hold_v4',
  'cast_fake_image_url_resource_not_found_v4', 'cast_fake_video_url_resource_not_found_v4',
  'play_local_file_refused_keeps_playing_v4',
  'cast_pause_resume_v4', 'cast_pause_during_load_v4',
  'cast_video_set_speed_v4', 'cast_seek_v4',
  'seek_while_paused_sends_progress_v4',
  'cast_video_progress_interval_v4', 'progress_interval_min_clamp_v4',
  'volume_clamped_high_v4', 'volume_clamped_low_v4', 'set_speed_extremes_v4', 'seek_far_beyond_duration_v4',
  // v4 queues
  'queue_start_out_of_range_refused_v4',
  'cast_queue_v4', 'cast_queue_with_headers_v4', 'cast_queue_insert_remove_v4',
  'cast_queue_select_no_load_v4', 'cast_queue_select_out_of_range_v4', 'cast_queue_remove_current_v4',
  'queue_load_no_start_index_v4', 'queue_full_v4', 'queue_insert_front_v4',
  'queue_select_prefetched_v4', 'queue_select_prefetched_video_v4',
  // v4 external subtitles
  'external_sub_add_while_playing_v4', 'external_sub_add_while_paused_v4', 'external_sub_add_unselected_v4',
  'external_sub_add_two_selected_v4', 'external_sub_switch_between_externals_v4',
  'external_sub_add_after_deselect_v4', 'external_sub_reselect_after_deselect_v4',
  'external_sub_seek_v4', 'external_sub_companion_v4', 'external_sub_companion_multipart_v4',
  'external_sub_empty_url_malformed_v4', 'external_sub_add_invalid_url_v4', 'external_sub_no_media_rejected_v4',
  'external_sub_add_during_load_v4', 'external_sub_add_during_load_unselected_v4',
  'external_sub_add_during_load_dropped_on_error_v4',
  // v4 companion resources
  'cast_companion_photo_v4', 'cast_companion_video_v4', 'cast_companion_video_and_seek_v4', 'cast_companion_audio_v4',
  'cast_companion_missing_image_resource_not_found_v4', 'cast_companion_missing_video_resource_not_found_v4',
  'cast_companion_empty_seek',
  // v4 with several senders
  'multi_sender_load_broadcast_v4',
  'multi_sender_load_metadata_broadcast_video_v4', 'multi_sender_load_metadata_broadcast_image_v4',
  'multi_sender_load_custom_metadata_broadcast_video_v4', 'multi_sender_load_custom_metadata_broadcast_image_v4',
  'multi_sender_volume_broadcast_v4', 'multi_sender_progress_isolation_v4',
  'multi_sender_queue_insert_broadcast_v4', 'multi_sender_queue_remove_broadcast_v4', 'multi_sender_queue_select_broadcast_v4',
  'multi_sender_stop_broadcast_v4', 'multi_sender_error_idle_broadcast_v4', 'multi_sender_state_idle_relayed_v4',
  'multi_sender_external_subs_v4', 'late_joiner_gets_queue_state_v4'
];

const DISABLED_UPSTREAM = 'commented out of the case list in tools/fast at the pinned upstream revision';
const EMBEDDED_TRACKS = 'needs the audio, video and subtitle tracks embedded in a Matroska file, which an HTML5 video element does not expose';

// Cases a receiver built on an HTML5 video element cannot satisfy, with the reason.
const NOT_APPLICABLE = {
  cast_video_set_volume_v2: DISABLED_UPSTREAM,
  cast_video_set_volume_v3: DISABLED_UPSTREAM,
  cast_video_set_volume_v4: DISABLED_UPSTREAM,
  cast_gif_then_video_v4: EMBEDDED_TRACKS,
  subtitle_change_and_disable_v4: EMBEDDED_TRACKS,
  subtitle_change_keeps_playing_v4: EMBEDDED_TRACKS,
  subtitle_bitmap_change_then_load_v4: 'needs bitmap (VobSub) subtitles, which a browser cannot render',
  subtitle_disable_while_paused_v4: EMBEDDED_TRACKS,
  subtitle_switch_while_paused_v4: EMBEDDED_TRACKS,
  video_disable_with_subs_v4: EMBEDDED_TRACKS,
  video_disable_while_paused_v4: EMBEDDED_TRACKS,
  video_disable_then_eos_v4: EMBEDDED_TRACKS,
  video_track_switch_v4: EMBEDDED_TRACKS,
  audio_track_switch_v4: EMBEDDED_TRACKS,
  audio_disable_enable_v4: EMBEDDED_TRACKS,
  track_change_rejections_v4: EMBEDDED_TRACKS,
  rapid_track_changes_v4: EMBEDDED_TRACKS,
  external_sub_keeps_embedded_selection_v4: EMBEDDED_TRACKS,
  external_sub_switch_embedded_external_v4: EMBEDDED_TRACKS,
  queue_autoplay_v4: EMBEDDED_TRACKS,
  gapless_queue_autoplay_v4: EMBEDDED_TRACKS
};

function parseArgs(argv) {
  const opts = { serviceDir: null, frontendDir: null, fast: null, media: null, cases: null, list: false };
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    if (flag === '--list') {
      opts.list = true;
      continue;
    }
    const value = argv[++i];
    if (value === undefined) {
      throw new Error(flag + ' needs a value');
    }
    switch (flag) {
      case '--service-dir': opts.serviceDir = value; break;
      case '--frontend-dir': opts.frontendDir = value; break;
      case '--fast': opts.fast = value; break;
      case '--media': opts.media = value; break;
      case '--cases': opts.cases = value.split(',').filter(Boolean); break;
      default: throw new Error('unknown argument: ' + flag);
    }
  }
  return opts;
}

function out(line) {
  process.stdout.write(line + '\n');
}

function sleep(ms) {
  return new Promise(function (resolve) { setTimeout(resolve, ms); });
}

function runCase(opts, fingerprint, name) {
  const args = ['--host', HOST, '--port', String(PORT), '--sample-media-dir', opts.media];
  if (fingerprint) {
    args.push('--fingerprint', fingerprint);
  }
  args.push('run', name);
  return harness.runProcess(opts.fast, args, 240000).then(function (res) {
    const text = (res.stdout + res.stderr).replace(/\x1b\[[0-9;]*m/g, '');
    const ok = new RegExp('^\\s*test ' + name + ' \\.\\.\\. OK\\s*$', 'm').test(text);
    return { ok: ok, missing: text.indexOf('no test case matches') !== -1, text: text };
  });
}

// The main window logs the connection URL it draws as a QR code. Checks that it is a v4 connection
// URL: fcast://r/ followed by base64url JSON with name, addresses, services and the TXT values.
async function checkConnectionUrl(pageLog, fingerprint) {
  const deadline = Date.now() + 15000;
  let url = null;
  let rendered = false;
  while (Date.now() < deadline && (url === null || !rendered)) {
    pageLog.forEach(function (line) {
      const match = /fcast:\/\/r\/([A-Za-z0-9_-]+)/.exec(line);
      if (match) {
        url = match[1];
      }
      if (line.indexOf('Rendered QR Code') !== -1) {
        rendered = true;
      }
    });
    await sleep(200);
  }
  if (url === null) {
    throw new Error('the main window did not log a connection URL');
  }
  if (!rendered) {
    throw new Error('the main window did not render the QR code');
  }

  const info = JSON.parse(Buffer.from(url.replace(/-/g, '+').replace(/_/g, '/'), 'base64').toString('utf8'));
  if (typeof info.name !== 'string' || info.name.length === 0) {
    throw new Error('connection URL has no name');
  }
  if (!Array.isArray(info.addresses) || info.addresses.length === 0) {
    throw new Error('connection URL has no addresses');
  }
  if (!Array.isArray(info.services) || info.services.length !== 1 || info.services[0].port !== PORT || info.services[0].type !== 0) {
    throw new Error('connection URL services are ' + JSON.stringify(info.services));
  }
  if (fingerprint !== null) {
    if (!info.txt || info.txt.v !== '4' || info.txt.fp !== fingerprint) {
      throw new Error('connection URL txt is ' + JSON.stringify(info.txt) + ', expected v=4 and fp=' + fingerprint);
    }
  } else if (info.txt !== undefined) {
    throw new Error('connection URL carries txt ' + JSON.stringify(info.txt) + ' on a runtime without v4');
  }
  return 'addresses=' + info.addresses.join(',') + (fingerprint !== null ? ' fp matches' : ' no txt');
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.list) {
    PLAYER_CASES.forEach(function (name) { out('RUN ' + name); });
    Object.keys(NOT_APPLICABLE).forEach(function (name) { out('N/A ' + name + ': ' + NOT_APPLICABLE[name]); });
    return 0;
  }
  if (!opts.serviceDir || !opts.frontendDir || !opts.fast || !opts.media) {
    throw new Error('usage: node player-fast.js --service-dir <dir> --frontend-dir <dir> --fast <fast.exe> --media <dir> [--cases a,b,...] [--list]');
  }

  const cases = opts.cases || PLAYER_CASES;
  const pageLog = [];
  const service = harness.loadService(opts.serviceDir);
  await harness.waitForPort(PORT, HOST, 10000);

  const info = await harness.appRequest(service, 'get_connection_info', {});
  const fingerprint = info && info.value && info.value.txt ? info.value.txt.fp : null;
  out('service node=' + process.version + ' v4=' + (fingerprint !== null));

  const host = await frontendHost.startFrontendHost(service, opts.frontendDir, {
    onLog: function (line) { pageLog.push(line); }
  });
  const browser = frontendHost.launchBrowser(host.url);

  // The main window reports its first render through the bridge log.
  const deadline = Date.now() + 30000;
  while (!pageLog.some(function (line) { return line.indexOf('Window created') !== -1; })) {
    if (Date.now() > deadline) {
      await browser.close();
      throw new Error('the frontend did not start in the browser');
    }
    await sleep(200);
  }

  let passed = 0;
  const failed = [];
  try {
    out('PASS connection-url ' + await checkConnectionUrl(pageLog, fingerprint));
    passed++;
  } catch (err) {
    failed.push('connection-url');
    out('FAIL connection-url: ' + (err && err.message ? err.message : err));
  }

  for (let i = 0; i < cases.length; i++) {
    const name = cases[i];
    if (fingerprint === null && /_v4$|^cast_companion_empty_seek$/.test(name)) {
      out('SKIP ' + name + ': the runtime has no v4');
      continue;
    }
    const logStart = pageLog.length;
    const result = await runCase(opts, fingerprint, name);
    if (result.ok) {
      passed++;
      out('PASS ' + name);
    } else if (result.missing) {
      failed.push(name);
      out('FAIL ' + name + ': this FAST build has no such case');
    } else {
      failed.push(name);
      out('FAIL ' + name);
      out(result.text.split('\n').slice(-25).map(function (line) { return '    ' + line; }).join('\n'));
      out(pageLog.slice(logStart).filter(function (line) { return !/^debug/.test(line); }).slice(-25)
        .map(function (line) { return '    [page] ' + line.slice(0, 300); }).join('\n'));
    }
    // Lets the frontend return to its main window before the next case connects.
    await sleep(500);
  }

  await browser.close();
  host.close();
  out('RESULT ' + passed + '/' + (passed + failed.length) + (failed.length > 0 ? ' FAILED: ' + failed.join(' ') : ''));
  out('NOT APPLICABLE ' + Object.keys(NOT_APPLICABLE).length);
  return failed.length === 0 ? 0 : 1;
}

main().then(function (code) {
  process.exit(code);
}, function (err) {
  out('ERROR ' + (err && err.stack ? err.stack : err));
  process.exit(2);
});
