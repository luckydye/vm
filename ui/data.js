// State and everything that talks to serve. The UI reads state through
// subscribe and changes it only through the functions exported here.

const TOKEN_KEY = "varmint-token";
const VOLUME_KEY = "varmint-volume";
const MUTED_KEY = "varmint-muted";

// The token from the URL `varmint serve --ui` prints, kept for later visits
// and taken out of the address bar.
function initialToken() {
  const fromUrl = new URLSearchParams(location.hash.slice(1)).get("token");
  if (fromUrl) {
    localStorage.setItem(TOKEN_KEY, fromUrl);
    history.replaceState(null, "", location.pathname);
  }
  return localStorage.getItem(TOKEN_KEY);
}

// The view a URL shows: `/`, `/vm/NAME`, or `/vm/NAME/display` with the
// display expanded in place of the shell.
export function route(path) {
  const match = path.match(/^\/vm\/([^/]+)(\/display)?$/);
  return { selected: match ? decodeURIComponent(match[1]) : null, screenExpanded: Boolean(match?.[2]) };
}

function pathOf({ selected, screenExpanded }) {
  return selected ? `/vm/${encodeURIComponent(selected)}${screenExpanded ? "/display" : ""}` : "/";
}

export const state = {
  // Sent with every request; null until the user gives one.
  token: initialToken(),
  vms: [],
  // One sample of what the host and its machines use per listing, the oldest first.
  usage: [],
  // The machine shown and whether its display fills the page instead of the
  // shell, both kept in the URL.
  ...route(location.pathname),
  creating: false,
  starting: [],
  pending: new Set(),
  toasts: [],
  listError: null,
  // How loud the machine's audio plays, from 0 to 1, kept for later visits.
  volume: Number(localStorage.getItem(VOLUME_KEY) ?? 1),
  muted: localStorage.getItem(MUTED_KEY) === "true",
  // Machines whose tile plays audio; tiles start muted.
  audibleTiles: new Set(),
  // Key codes of the modifiers latched in the screen's key bar.
  modifiers: new Set(),
};

const listeners = new Set();

export function subscribe(listener) {
  listeners.add(listener);
  listener(state);
}

function changed() {
  for (const listener of listeners) listener(state);
}

export function setToken(token) {
  localStorage.setItem(TOKEN_KEY, token);
  state.token = token;
  changed();
}

async function call(op, fields = {}) {
  const response = await fetch("/api", { method: "POST", body: JSON.stringify({ op, token: state.token, ...fields }) });
  const reply = await response.json();
  if (reply.error === "invalid token") {
    localStorage.removeItem(TOKEN_KEY);
    state.token = null;
  }
  if (reply.error) throw new Error(reply.error);
  return reply;
}

// How long the addresses and ports of the open machine stay before they are asked again.
const NETWORK_INTERVAL = 10000;

// Five minutes at the poll interval.
const USAGE_SAMPLES = 150;

async function refresh() {
  const { vms, host } = await call("ls");
  state.vms = vms;
  const machines = Object.fromEntries(vms.filter((vm) => vm.running).map((vm) => [vm.name, { memory_bytes: vm.memory_bytes, cpu_ms: vm.cpu_ms }]));
  state.usage = [...state.usage.slice(1 - USAGE_SAMPLES), { host, machines }];
  const running = state.selected && vms.find((vm) => vm.name === state.selected)?.running;
  if (!running) state.network = null;
  else if (state.network?.name !== state.selected || Date.now() - state.network.at > NETWORK_INTERVAL) {
    // The guest may not have an address yet while it boots, so a failure is retried on the next poll.
    const name = state.selected;
    const reply = await call("network", { name }).then((reply) => reply.network, () => null);
    state.network = { name, reply, at: reply ? Date.now() : 0 };
  }
  if (state.selected && !state.vms.some((vm) => vm.name === state.selected)) {
    Object.assign(state, route("/"));
    history.replaceState(null, "", "/");
  }
  changed();
}

// Polls one request at a time, so slow replies never pile up.
export async function poll() {
  if (!state.token) return setTimeout(poll, 2000);
  try {
    await refresh();
    state.listError = null;
  } catch (error) {
    state.listError = error.message;
    changed();
  }
  setTimeout(poll, 2000);
}

let toastIds = 0;

function toast(text, tone) {
  const id = ++toastIds;
  state.toasts.push({ id, text, tone });
  setTimeout(() => dismissToast(id), tone === "error" ? 8000 : 4000);
}

export function dismissToast(id) {
  state.toasts = state.toasts.filter((toast) => toast.id !== id);
  changed();
}

async function act(key, op, fields, done) {
  state.pending.add(key);
  changed();
  try {
    toast(done(await call(op, fields)), "done");
  } catch (error) {
    toast(error.message, "error");
  }
  state.pending.delete(key);
  await refresh();
}

export const stop = (name) => act(`stop ${name}`, "stop", { name }, () => `Stopped ${name}`);
export const start = (name) => act(`start ${name}`, "run", { name }, () => `Started ${name}`);
// Makes the addresses and ports of the open machine be asked again at the next poll.
function refreshNetwork() {
  if (state.network) state.network.at = 0;
}

export const forward = (name, port) =>
  act(`forward ${name}:${port}`, "forward", { name, port }, (reply) => {
    refreshNetwork();
    return `Forwarded port ${port} to localhost:${reply.local_port}`;
  });
export const unforward = (name, port) =>
  act(`forward ${name}:${port}`, "unforward", { name, port }, () => {
    refreshNetwork();
    return `Stopped forwarding port ${port}`;
  });
export const remove = (name) => act(`rm ${name}`, "rm", { name }, () => `Deleted ${name}`);

export const PRESETS = [
  { id: "small", label: "Small", cpus: 2, memory_mib: 2048 },
  { id: "standard", label: "Standard", cpus: 4, memory_mib: 6144 },
  { id: "large", label: "Large", cpus: 8, memory_mib: 16384 },
];

// Shows the view of the current URL, after the browser went back or forward.
export function followLocation() {
  Object.assign(state, route(location.pathname));
  changed();
}

// Adds the view to the browser history, so back returns to the one before.
function navigate() {
  const path = pathOf(state);
  if (path !== location.pathname) history.pushState(null, "", path);
  changed();
}

export function select(name) {
  state.selected = name;
  state.screenExpanded = false;
  navigate();
}

export function setScreenExpanded(expanded) {
  state.screenExpanded = expanded;
  navigate();
}

// Every machine's audio plays through this gain, so volume and mute apply to all of them.
const playback = new AudioContext({ sampleRate: 48000 });
const speakers = playback.createGain();
speakers.connect(playback.destination);

function applyVolume() {
  speakers.gain.value = state.muted ? 0 : state.volume;
}
applyVolume();

// Browsers only play audio after the user interacted with the page.
export function resumeAudio() {
  if (playback.state === "suspended") playback.resume();
}

export function setVolume(volume) {
  state.volume = volume;
  state.muted = false;
  localStorage.setItem(VOLUME_KEY, volume);
  localStorage.setItem(MUTED_KEY, false);
  applyVolume();
  changed();
}

export function setMuted(muted) {
  state.muted = muted;
  localStorage.setItem(MUTED_KEY, muted);
  applyVolume();
  changed();
}

export function setTileAudible(name, audible) {
  if (audible) state.audibleTiles.add(name);
  else state.audibleTiles.delete(name);
  changed();
}

export function setCreating(creating) {
  state.creating = creating;
  changed();
}

let runs = 0;

// `form` holds the fields of the new VM sheet, as strings and booleans.
export async function create(form) {
  let fields;
  try {
    fields = runFields(form);
  } catch (error) {
    toast(error.message, "error");
    changed();
    return;
  }
  const run = { id: ++runs, name: fields.name ?? null };
  state.starting.push(run);
  state.creating = false;
  changed();
  try {
    const { name } = await call("run", fields);
    toast(`Started ${name}`, "done");
  } catch (error) {
    toast(error.message, "error");
  }
  state.starting = state.starting.filter((other) => other !== run);
  await refresh();
}

function runFields(form) {
  const preset = PRESETS.find((preset) => preset.id === form.preset);
  const fields = { cpus: preset.cpus, memory_mib: preset.memory_mib, persist: form.persist, wait_network: form.wait_network };
  if (form.name) fields.name = form.name;
  if (form.base) fields.base = form.base;
  if (form.disk_gib) fields.disk_gib = Number(form.disk_gib);
  if (form.size) {
    const size = form.size.split("x").map(Number);
    if (size.length !== 2 || size.some((value) => !Number.isInteger(value))) throw new Error(`size is not WIDTHxHEIGHT: ${form.size}`);
    fields.size = size;
  }
  const lines = form.env.split("\n").filter((line) => line.trim());
  if (lines.length) {
    fields.env = Object.fromEntries(lines.map((line) => {
      const at = line.indexOf("=");
      if (at < 1) throw new Error(`not NAME=value: ${line}`);
      return [line.slice(0, at).trim(), line.slice(at + 1)];
    }));
  }
  return fields;
}

const sum = (values) => values.reduce((total, value) => total + value, 0);

// Layers stacked from the bottom, one value per point: each machine in the
// points, by name, then the rest of the host, which is `host` less the machines.
// The chart has `slots` points, filled from the right.
function stacked(points, machine, host, slots) {
  const names = [...new Set(points.flatMap((point) => Object.keys(point.machines)))].sort();
  const values = (name) => points.map((point) => (point.machines[name] ? machine(point, name) : 0));
  const layers = names.map((name) => ({ name, values: values(name) }));
  // Clamped, since the host and the machines are not measured at the same instant.
  const rest = points.map((point, index) => Math.max(0, host(point) - sum(layers.map((layer) => layer.values[index]))));
  layers.push({ name: null, values: rest });
  let below = points.map(() => 0);
  for (const layer of layers) {
    layer.from = below;
    layer.to = below.map((value, index) => value + layer.values[index]);
    below = layer.to;
  }
  return { slots, layers, machines: sum(layers.slice(0, -1).map((layer) => layer.values.at(-1))), rest: rest.at(-1) };
}

const percent = (fraction) => `${Math.round(fraction * 100)}%`;

export function memoryHistory(samples) {
  const history = stacked(samples, (sample, name) => sample.machines[name].memory_bytes, (sample) => sample.host.used_bytes, USAGE_SAMPLES);
  const { memory_bytes, used_bytes } = samples.at(-1).host;
  return { ...history, capacity: memory_bytes, format: (bytes) => `${gib(bytes)} GiB`, summary: `${gib(used_bytes)} of ${gib(memory_bytes)} GiB used` };
}

// The share of all host cores busy between each sample and the one before it,
// so it starts with the second sample.
export function cpuHistory(samples) {
  if (samples.length < 2) return null;
  const points = samples.slice(1).map((sample, index) => {
    const before = samples[index];
    const elapsed = sample.host.cpu_total_ms - before.host.cpu_total_ms;
    const machines = Object.fromEntries(Object.entries(sample.machines).map(([name, { cpu_ms }]) => {
      // A machine that started since counts from zero; one that restarted, as idle.
      const used = cpu_ms - (before.machines[name]?.cpu_ms ?? 0);
      return [name, Math.max(0, used) / elapsed];
    }));
    return { machines, busy: (sample.host.cpu_busy_ms - before.host.cpu_busy_ms) / elapsed };
  });
  const history = stacked(points, (point, name) => point.machines[name], (point) => point.busy, USAGE_SAMPLES - 1);
  return { ...history, capacity: 1, format: percent, summary: `${percent(points.at(-1).busy)} busy` };
}

// The layer under `value` at point `index`, null for the unused room above them.
export function layerAt(history, index, value) {
  return history.layers.find((layer) => layer.from[index] <= value && value < layer.to[index]) ?? null;
}

export const gib = (bytes) => (bytes / 2 ** 30).toFixed(1);

export function specs(hardware) {
  const memory = hardware.memory_mib >= 1024 ? `${+(hardware.memory_mib / 1024).toFixed(1)} GiB` : `${hardware.memory_mib} MiB`;
  return [`${hardware.cpus} CPU`, memory, `${hardware.disk_gib} GiB disk`];
}

// Attach: connections that serve hands to a VM's control socket, through the
// relay's WebSocket bridge. Each starts with one serve reply line.

function attach(name, request) {
  const socket = new WebSocket(`${location.origin.replace(/^http/, "ws")}/attach`);
  socket.binaryType = "arraybuffer";
  socket.onopen = () => socket.send(JSON.stringify({ op: "attach", name, request, token: state.token }));
  let buffer = new Uint8Array(0);
  let wake = () => {};
  let closed = false;
  socket.onmessage = (event) => {
    const chunk = new Uint8Array(event.data);
    const joined = new Uint8Array(buffer.length + chunk.length);
    joined.set(buffer);
    joined.set(chunk, buffer.length);
    buffer = joined;
    wake();
  };
  socket.onclose = () => {
    closed = true;
    wake();
  };
  async function until(ready) {
    while (!ready()) {
      if (closed) throw new Error("connection closed");
      await new Promise((resolve) => (wake = resolve));
    }
  }
  async function bytes(count) {
    await until(() => buffer.length >= count);
    const taken = buffer.slice(0, count);
    buffer = buffer.subarray(count);
    return taken;
  }
  async function reply() {
    await until(() => buffer.includes(10));
    const line = JSON.parse(new TextDecoder().decode(await bytes(buffer.indexOf(10) + 1)));
    if (line.error) throw new Error(line.error);
    return line;
  }
  return { socket, bytes, reply, close: () => socket.close() };
}

// How far ahead of the speakers audio is queued, which absorbs network jitter.
const AUDIO_LATENCY = 0.08;
// Audio further ahead is dropped, so the delay cannot grow when clocks drift.
const AUDIO_MAX_AHEAD = 0.3;
const STREAM_KIND = { VIDEO: 0, AUDIO: 1 };

// Calls onFrame with { width, height, image } whenever the screen changes,
// `image` a decoded VideoFrame the callback must close, until the returned
// connection is closed; `done` settles when the stream ends. Frames arrive as
// H.264 and are decoded in hardware, so a busy screen costs a few Mbit/s.
// With `audio` the machine's sound plays too, arriving as Opus.
export function screenStream(name, fps, onFrame, audio) {
  const attached = attach(name, { op: "stream", fps, codec: "h264", audio });
  let failed = null;
  const fail = (error) => (failed = error);
  const decoder = new VideoDecoder({
    output: (image) => onFrame({ width: image.displayWidth, height: image.displayHeight, image }),
    error: fail,
  });
  const sound = audio ? soundDecoder(fail) : null;
  const done = (async () => {
    try {
      await attached.reply();
      await attached.reply();
      for (let timestamp = 0; ; timestamp++) {
        const kind = (await attached.bytes(1))[0];
        if (kind === STREAM_KIND.AUDIO) {
          const length = new DataView((await attached.bytes(4)).buffer).getUint32(0, true);
          const data = await attached.bytes(length);
          if (failed) throw failed;
          sound.decode(new EncodedAudioChunk({ type: "key", timestamp, data }));
          continue;
        }
        if (kind !== STREAM_KIND.VIDEO) throw new Error(`unknown stream message kind ${kind}`);
        const header = new DataView((await attached.bytes(13)).buffer);
        const keyframe = header.getUint8(8) === 1;
        const data = await attached.bytes(header.getUint32(9, true));
        if (failed) throw failed;
        if (keyframe) decoder.configure({ codec: h264Codec(data), optimizeForLatency: true, hardwareAcceleration: "prefer-hardware" });
        decoder.decode(new EncodedVideoChunk({ type: keyframe ? "key" : "delta", timestamp, data }));
      }
    } finally {
      if (decoder.state !== "closed") decoder.close();
      if (sound && sound.state !== "closed") sound.close();
    }
  })();
  return { close: attached.close, done };
}

// Decodes Opus and queues each piece right behind the one before on the speakers.
function soundDecoder(onError) {
  let at = 0;
  const decoder = new AudioDecoder({
    output: (decoded) => {
      if (at - playback.currentTime <= AUDIO_MAX_AHEAD) {
        const buffer = playback.createBuffer(decoded.numberOfChannels, decoded.numberOfFrames, decoded.sampleRate);
        for (let channel = 0; channel < decoded.numberOfChannels; channel++) {
          decoded.copyTo(buffer.getChannelData(channel), { planeIndex: channel, format: "f32-planar" });
        }
        const source = playback.createBufferSource();
        source.buffer = buffer;
        source.connect(speakers);
        at = Math.max(at, playback.currentTime + AUDIO_LATENCY);
        source.start(at);
        at += buffer.duration;
      }
      decoded.close();
    },
    error: onError,
  });
  decoder.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
  return decoder;
}

// The codec string of an Annex B keyframe, from its sequence parameter set.
function h264Codec(data) {
  for (let index = 0; index + 6 < data.length; index++) {
    if (data[index] === 0 && data[index + 1] === 0 && data[index + 2] === 1 && (data[index + 3] & 0x1f) === 7) {
      return "avc1." + [...data.subarray(index + 4, index + 7)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
    }
  }
  throw new Error("H.264 keyframe without a sequence parameter set");
}

export function inputStream(name) {
  const attached = attach(name, { op: "inputstream" });
  const ready = attached.reply().then(() => attached.reply());
  return {
    close: attached.close,
    ready,
    send: (event) => attached.socket.send(JSON.stringify(event) + "\n"),
  };
}

const FRAME = { DATA: 2, STDERR: 3, EXIT: 5, RESIZE: 6, CLIPBOARD_IMAGE: 12 };
// The largest frame payload the VM takes, protocol::MAX_PAYLOAD.
const MAX_PAYLOAD = 16 * 1024 * 1024;

function encodeFrame(kind, payload) {
  const bytes = new Uint8Array(9 + payload.length);
  const view = new DataView(bytes.buffer);
  view.setUint8(4, kind);
  view.setUint32(5, payload.length, true);
  bytes.set(payload, 9);
  return bytes;
}

// The machine's shared tmux session "main" on a terminal, created on first
// attach; `exited` resolves with the exit code of the tmux client.
export function shellSession(name, cols, rows, onOutput) {
  const attached = attach(name, {
    op: "open",
    session: { type: "pty", argv: ["tmux", "new-session", "-A", "-s", "main"], cols, rows, terminal: { TERM: "xterm-256color", COLORTERM: "truecolor" } },
  });
  const exited = (async () => {
    await attached.reply();
    for (;;) {
      const header = new DataView((await attached.bytes(9)).buffer);
      const kind = header.getUint8(4);
      const payload = await attached.bytes(header.getUint32(5, true));
      if (kind === FRAME.DATA || kind === FRAME.STDERR) onOutput(payload);
      if (kind === FRAME.EXIT) return new DataView(payload.buffer).getInt32(0, true);
    }
  })();
  return {
    close: attached.close,
    exited,
    write: (text) => attached.socket.send(encodeFrame(FRAME.DATA, new TextEncoder().encode(text))),
    // Puts the PNG into the guest clipboard and presses Ctrl+V, so programs like Claude Code paste it.
    pasteImage: (png) => {
      if (png.length > MAX_PAYLOAD) throw new Error(`The clipboard image is too large for the VM (${png.length} bytes).`);
      attached.socket.send(encodeFrame(FRAME.CLIPBOARD_IMAGE, png));
      attached.socket.send(encodeFrame(FRAME.DATA, new Uint8Array([0x16])));
    },
    resize: (cols, rows) => {
      const payload = new Uint8Array(4);
      const view = new DataView(payload.buffer);
      view.setUint16(0, cols, true);
      view.setUint16(2, rows, true);
      attached.socket.send(encodeFrame(FRAME.RESIZE, payload));
    },
  };
}

// Linux evdev codes for KeyboardEvent.code.
const KEY_CODES = {
  Escape: 1, Minus: 12, Equal: 13, Backspace: 14, Tab: 15, BracketLeft: 26, BracketRight: 27, Enter: 28,
  ControlLeft: 29, Semicolon: 39, Quote: 40, Backquote: 41, ShiftLeft: 42, Backslash: 43, Comma: 51, Period: 52,
  Slash: 53, ShiftRight: 54, AltLeft: 56, Space: 57, CapsLock: 58, F11: 87, F12: 88, ControlRight: 97,
  AltRight: 100, Home: 102, ArrowUp: 103, PageUp: 104, ArrowLeft: 105, ArrowRight: 106, End: 107,
  ArrowDown: 108, PageDown: 109, Insert: 110, Delete: 111, MetaLeft: 125, MetaRight: 126,
};
[..."1234567890"].forEach((digit, index) => (KEY_CODES[`Digit${digit}`] = 2 + index));
for (const [row, first] of [["QWERTYUIOP", 16], ["ASDFGHJKL", 30], ["ZXCVBNM", 44]]) {
  [...row].forEach((letter, index) => (KEY_CODES[`Key${letter}`] = first + index));
}
for (let index = 1; index <= 10; index++) KEY_CODES[`F${index}`] = 58 + index;

const BUTTONS = [0x110, 0x112, 0x111];

// `x` and `y` are fractions of the displayed screen.
export function pointerEvents(type, x, y, frame, detail) {
  const move = { move: { x: Math.floor(x * frame.width), y: Math.floor(y * frame.height) } };
  if (type === "pointermove") return [move];
  if (type === "wheel") return [move, { scroll: { value: -Math.sign(detail), horizontal: false } }];
  return [move, { button: { button: BUTTONS[detail], down: type === "pointerdown" } }];
}

export function keyEvent(event, held) {
  const code = KEY_CODES[event.code];
  if (code === undefined || event.repeat) return null;
  const down = event.type === "keydown";
  if (down) held.add(code);
  else held.delete(code);
  return { key: { code, down } };
}

export function releaseKeys(held) {
  const events = [...held].map((code) => ({ key: { code, down: false } }));
  held.clear();
  return events;
}

// The screen's key bar, for touch screens without these keys and for
// combinations the browser keeps for itself. A modifier stays latched until
// the next key sent from the bar or typed on the on-screen keyboard.
export const MODIFIERS = [
  { label: "Ctrl", code: KEY_CODES.ControlLeft },
  { label: "Alt", code: KEY_CODES.AltLeft },
  { label: "Shift", code: KEY_CODES.ShiftLeft },
  { label: "Super", code: KEY_CODES.MetaLeft },
];

export const KEYS = [
  ["Esc", "Escape"], ["Tab", "Tab"], ["←", "ArrowLeft"], ["↑", "ArrowUp"], ["↓", "ArrowDown"], ["→", "ArrowRight"],
  ["Home", "Home"], ["End", "End"], ["PgUp", "PageUp"], ["PgDn", "PageDown"], ["Ins", "Insert"], ["Del", "Delete"],
  ...Array.from({ length: 12 }, (_, index) => [`F${index + 1}`, `F${index + 1}`]),
].map(([label, name]) => ({ label, code: KEY_CODES[name] }));

export function toggleModifier(code) {
  if (!state.modifiers.delete(code)) state.modifiers.add(code);
  changed();
}

// One press of `codes` together with the latched modifiers, which it unlatches.
export function chord(codes) {
  const pressed = [...new Set([...state.modifiers, ...codes])];
  if (state.modifiers.size) {
    state.modifiers.clear();
    changed();
  }
  return [...pressed.map((code) => ({ key: { code, down: true } })), ...pressed.toReversed().map((code) => ({ key: { code, down: false } }))];
}

// The keys of each character on the guest's US layout, Shift first where it is needed.
const CHARACTERS = new Map([[" ", [KEY_CODES.Space]], ["\n", [KEY_CODES.Enter]], ["\t", [KEY_CODES.Tab]]]);
for (const letter of "abcdefghijklmnopqrstuvwxyz") {
  const code = KEY_CODES[`Key${letter.toUpperCase()}`];
  CHARACTERS.set(letter, [code]);
  CHARACTERS.set(letter.toUpperCase(), [KEY_CODES.ShiftLeft, code]);
}
const SYMBOL_KEYS = [
  "Backquote", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8", "Digit9", "Digit0", "Minus", "Equal",
  "BracketLeft", "BracketRight", "Backslash", "Semicolon", "Quote", "Comma", "Period", "Slash",
];
SYMBOL_KEYS.forEach((name, index) => {
  CHARACTERS.set("`1234567890-=[]\\;',./"[index], [KEY_CODES[name]]);
  CHARACTERS.set("~!@#$%^&*()_+{}|:\"<>?"[index], [KEY_CODES.ShiftLeft, KEY_CODES[name]]);
});

// The presses that turn `before`, the text in the on-screen keyboard's field,
// into `after`: Backspace for what was taken away, then what was typed.
export function typing(before, after) {
  let same = 0;
  while (same < before.length && same < after.length && before[same] === after[same]) same++;
  const typed = [...after.slice(same)].map((character) => {
    const codes = CHARACTERS.get(character);
    if (!codes) throw new Error(`“${character}” has no key on the machine's US keyboard`);
    return codes;
  });
  return [...[...before.slice(same)].map(() => [KEY_CODES.Backspace]), ...typed].flatMap(chord);
}

// The key of a KeyboardEvent.key that types no text into the on-screen
// keyboard's field, like Enter or an arrow, or null. Backspace edits the
// field, so `typing` sends it.
export function namedKey(key) {
  return key === "Backspace" ? null : KEY_CODES[key] ?? null;
}
