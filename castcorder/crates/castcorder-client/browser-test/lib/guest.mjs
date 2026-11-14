import { join } from 'node:path';
import { ctx as run } from './ctx.mjs';
import { check } from './helpers.mjs';

const FIREFOX_PREFS = {
  'media.navigator.streams.fake': true,
  'media.navigator.permission.disabled': true,
  // ICE and autoplay stay at Firefox's defaults, as on the owner's machine.
  // Automated runs stay silent: every output is scaled to 0 while the output
  // stream itself keeps running (the AudioWorklet's render clock needs it).
  'media.volume_scale': '0.0',
};

export async function openGuest(name, extraArgs, base, setup) {
  const { MODE, BASE_ARGS, host, logs, browsers, chromium, firefox } = run;
  logs[name] = logs[name] || [];
  let browser, ctx;
  if (MODE === 'firefox') {
    // Distinct fake tones: identical recordings would share one content root,
    // and the host library keys tracks by root (one entry for both guests).
    const prefs = { ...FIREFOX_PREFS, 'media.navigator.audio.fake_frequency': name === 'A' ? 1000 : 1500 };
    browser = await firefox.launch({ headless: true, firefoxUserPrefs: prefs });
    ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  } else {
    browser = await chromium.launch({ headless: true, args: [...BASE_ARGS, ...extraArgs] });
    ctx = await browser.newContext({ permissions: ['microphone', 'camera'], ignoreHTTPSErrors: true });
  }
  browsers.push(browser);
  if (MODE === 'fallback') {
    // A browser without MediaStreamTrackProcessor on window (Firefox, Safari,
    // or a Chrome that exposes it in workers only).
    await ctx.addInitScript(() => { delete window.MediaStreamTrackProcessor; });
  }
  // Instrumentation, not a mock: the real objects the app creates are kept so
  // the test can check them after Leave and cut a connection.
  await ctx.addInitScript(() => {
    const tracks = window.__tracks = [];
    const ctxs = window.__audioContexts = [];
    const pcs = window.__pcs = [];
    const gum = MediaDevices.prototype.getUserMedia;
    MediaDevices.prototype.getUserMedia = async function (...a) {
      const st = await gum.apply(this, a);
      for (const t of st.getTracks()) if (!tracks.includes(t)) tracks.push(t);
      return st;
    };
    // Every track the app reads from a stream, too (whatever created it).
    for (const m of ['getTracks', 'getAudioTracks', 'getVideoTracks']) {
      const f = MediaStream.prototype[m];
      MediaStream.prototype[m] = function () {
        const r = f.call(this);
        for (const t of r) if (!tracks.includes(t)) tracks.push(t);
        return r;
      };
    }
    const clone = MediaStreamTrack.prototype.clone;
    MediaStreamTrack.prototype.clone = function () { const t = clone.call(this); tracks.push(t); return t; };
    const AC = window.AudioContext;
    window.AudioContext = class extends AC { constructor(...a) { super(...a); ctxs.push(this); } };
    const PC = window.RTCPeerConnection;
    window.RTCPeerConnection = class extends PC { constructor(...a) { super(...a); pcs.push(this); } };
  });
  if (setup) await setup(ctx);
  const page = await ctx.newPage();
  page.on('console', (m) => logs[name].push(m.text()));
  page.on('pageerror', (e) => logs[name].push('[pageerror] ' + e.message));
  await page.goto(base + '/?name=' + name);
  await page.waitForSelector('#join');
  return page;
}
