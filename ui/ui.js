// Renders state from data.js and forwards user events to it. Live
// connections (tile previews, the machine's screen and shell) are opened and
// closed as the rendered state asks for them.

import * as data from "./data.js";

const $ = (id) => document.getElementById(id);

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

const selected = () => data.state.vms.find((vm) => vm.name === data.state.selected);

// A button that asks once more before `action` while `needed` says so.
function confirming(button, label, action, needed = () => true) {
  let armed = null;
  let text = "";
  const disarm = () => {
    clearTimeout(armed);
    armed = null;
    button.textContent = text;
    button.classList.remove("armed");
  };
  button.onclick = () => {
    if (!needed()) return action();
    if (armed) {
      disarm();
      return action();
    }
    text = button.textContent;
    button.textContent = label;
    button.classList.add("armed");
    armed = setTimeout(disarm, 3000);
  };
}

function paintInto(canvas) {
  const context = canvas.getContext("2d");
  return ({ width, height, image }) => {
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
    context.drawImage(image, 0, 0);
    image.close();
  };
}

function kind(hardware) {
  return hardware.persist ? "Persistent" : "Ephemeral";
}

// Moves the screen between its tile and the machine page with a view
// transition: `from` and the element `to` returns once `update` has rendered
// share a name, so the browser morphs one into the other.
function morph(from, update, to) {
  from.style.viewTransitionName = "screen";
  const transition = document.startViewTransition(() => {
    from.style.viewTransitionName = "";
    update();
    to().style.viewTransitionName = "screen";
  });
  transition.finished.then(() => (to().style.viewTransitionName = ""));
}

// Opens a machine; a running one's screen grows out of its tile, starting from
// the tile's last frame while the page's own stream connects.
function openMachine(name, show = () => data.select(name)) {
  const vm = data.state.vms.find((vm) => vm.name === name);
  if (!vm?.running || !tiles.has(name)) return show();
  const source = tiles.get(name).canvas;
  morph(source, () => {
    show();
    const screen = $("screen");
    screen.width = source.width;
    screen.height = source.height;
    screen.getContext("2d").drawImage(source, 0, 0);
  }, () => $("screen"));
}

// Picks the column count that shows the 16:10 screens largest in the wall's area.
function layoutWall() {
  const count = Math.max(1, $("grid").children.length);
  const { width, height } = $("grid").getBoundingClientRect();
  const size = (columns) => Math.min(width / columns, (height / Math.ceil(count / columns)) * 1.6);
  let columns = 1;
  for (let candidate = 2; candidate <= count; candidate++) if (size(candidate) > size(columns)) columns = candidate;
  $("grid").style.setProperty("--columns", columns);
  $("grid").style.setProperty("--rows", Math.ceil(count / columns));
}

new ResizeObserver(layoutWall).observe($("grid"));

// Overview: a wall of live screens, one tile per machine kept across renders
// so its stream keeps running, stopped machines shelved below it, and a
// summary of the fleet beside it.

const tiles = new Map();

function tile(name) {
  const node = element("li", "tile");
  node.tabIndex = 0;
  const canvas = element("canvas");
  const label = element("div", "tile-label");
  const mute = element("button", "mute tile-mute");
  mute.append($("mute").firstElementChild.cloneNode(true));
  mute.onclick = (event) => {
    event.stopPropagation();
    data.setTileAudible(name, !data.state.audibleTiles.has(name));
  };
  mute.onkeydown = (event) => event.stopPropagation();
  label.append(element("span", "dot"), element("span", "tile-name", name), mute);
  node.append(canvas, element("span", "tile-empty", "Stopped"), label, element("span", "badge", "Persistent"));
  node.onclick = () => openMachine(name);
  node.onkeydown = (event) => event.key === "Enter" && openMachine(name);
  return { node, canvas, mute, stream: null, audible: false };
}

function renderOverview(state) {
  const showing = !state.selected;
  $("overview").hidden = !showing;
  $("summary").hidden = !showing;
  $("home").hidden = showing;
  const names = new Set(showing ? state.vms.filter((vm) => vm.running).map((vm) => vm.name) : []);
  for (const [name, entry] of tiles) {
    // A stream carries audio from its start, so muting or unmuting opens a new one.
    if ((names.has(name) && entry.audible === state.audibleTiles.has(name)) || !entry.stream) continue;
    entry.stream.close();
    entry.stream = null;
  }
  if (!showing) return;

  const nodes = state.starting.map((run) => {
    const node = element("li", "tile starting");
    const label = element("div", "tile-label");
    label.append(element("span", "dot"), element("span", "tile-name", run.name ?? "Starting…"));
    node.append(label);
    return node;
  });
  const shelved = [];
  for (const vm of state.vms) {
    if (!tiles.has(vm.name)) tiles.set(vm.name, tile(vm.name));
    const entry = tiles.get(vm.name);
    entry.node.classList.toggle("running", vm.running);
    entry.node.classList.toggle("persistent", vm.hardware.persist);
    const audible = state.audibleTiles.has(vm.name);
    entry.mute.dataset.muted = !audible;
    entry.mute.ariaLabel = audible ? "Mute" : "Unmute";
    if (vm.running && !entry.stream) {
      const stream = data.screenStream(vm.name, 10, paintInto(entry.canvas), audible);
      entry.stream = stream;
      entry.audible = audible;
      stream.done.catch(() => entry.stream === stream && (entry.stream = null));
    }
    (vm.running ? nodes : shelved).push(entry.node);
  }
  for (const name of tiles.keys()) if (!state.vms.some((vm) => vm.name === name)) tiles.delete(name);
  $("grid").replaceChildren(...nodes);
  $("shelf").replaceChildren(...shelved);
  $("grid").hidden = nodes.length === 0;
  $("shelf").hidden = shelved.length === 0;
  $("blank").hidden = nodes.length + shelved.length > 0;
  layoutWall();

  renderCharts(state);
}

// What the host uses as stacked areas, one per machine and one for the rest of
// the host, under what it has. Pointing at a layer names it.

const SVG = "http://www.w3.org/2000/svg";
const CHART_HEIGHT = 100;
const charts = { cpu: data.cpuHistory, memory: data.memoryHistory };
// Where the pointer is over each chart, as fractions of its size.
const pointers = { cpu: null, memory: null };

function svg(tag, attributes) {
  const node = document.createElementNS(SVG, tag);
  for (const [name, value] of Object.entries(attributes)) node.setAttribute(name, value);
  return node;
}

function legendEntry(className, name, value) {
  const entry = element("span", "entry");
  entry.append(element("span", `swatch ${className}`), element("span", "", name), element("span", "value", value));
  return entry;
}

function renderChart(id, history) {
  if (!history) return;
  const pointer = pointers[id];
  const count = history.layers[0].values.length;
  const offset = history.slots - count;
  const x = (index) => offset + index;
  const y = (value) => CHART_HEIGHT * (1 - value / history.capacity);
  const index = pointer && Math.min(count - 1, Math.max(0, Math.round(pointer.x * (history.slots - 1)) - offset));
  const hovered = pointer && data.layerAt(history, index, (1 - pointer.y) * history.capacity);

  const chart = $(`${id}-chart`);
  chart.setAttribute("viewBox", `0 0 ${history.slots - 1} ${CHART_HEIGHT}`);
  chart.replaceChildren(...history.layers.map((layer) => {
    const top = layer.to.map((value, index) => `${x(index)},${y(value)}`);
    const bottom = layer.from.map((value, index) => `${x(index)},${y(value)}`).reverse();
    const className = `${layer.name ? "machine-layer" : "host-layer"}${layer === hovered ? " hovered" : ""}`;
    return svg("path", { d: `M${top.join("L")}L${bottom.join("L")}Z`, class: className });
  }));
  if (pointer) chart.append(svg("line", { x1: x(index), x2: x(index), y1: 0, y2: CHART_HEIGHT, class: "crosshair" }));

  $(`${id}-summary`).textContent = history.summary;
  $(`${id}-legend`).replaceChildren(...(hovered
    ? [legendEntry(hovered.name ? "machine-layer" : "host-layer", hovered.name ?? "Rest of host", history.format(hovered.values[index]))]
    : [legendEntry("machine-layer", "Machines", history.format(history.machines)), legendEntry("host-layer", "Rest of host", history.format(history.rest))]));
}

function renderCharts(state) {
  if (!state.usage.length) return;
  for (const [id, history] of Object.entries(charts)) renderChart(id, history(state.usage));
}

for (const id of Object.keys(charts)) {
  const chart = $(`${id}-chart`);
  chart.addEventListener("mousemove", (event) => {
    const box = chart.getBoundingClientRect();
    pointers[id] = { x: (event.clientX - box.left) / box.width, y: (event.clientY - box.top) / box.height };
    renderChart(id, charts[id](data.state.usage));
  });
  chart.addEventListener("mouseleave", () => {
    pointers[id] = null;
    renderChart(id, charts[id](data.state.usage));
  });
}

// Machine page

function renderMachine(state) {
  const vm = selected();
  $("machine").hidden = !vm;
  $("machine-power").hidden = !vm;
  $("machine-delete").hidden = !vm?.hardware.persist;
  $("new").hidden = Boolean(vm);
  $("crumbs").replaceChildren();
  if (vm) {
    const status = element("span", "state");
    status.append(element("span", vm.running ? "dot on" : "dot"), vm.running ? "Running" : "Stopped");
    $("crumbs").append(vm.name, status);
    const facts = [
      ["Kind", kind(vm.hardware)],
      ["CPU", `${vm.hardware.cpus} cores`],
      ["Memory", data.specs(vm.hardware)[1]],
      ["Disk", `${vm.hardware.disk_gib} GiB`],
      ["Display", `${vm.hardware.width}×${vm.hardware.height}`],
    ];
    const network = state.network?.name === vm.name ? state.network.reply : null;
    if (network) {
      const [address] = network.addresses;
      const ports = network.ports.map(({ port, loopback, forwarded }) => {
        const item = element("div", "port");
        // A forward is on localhost, which browsers treat as a secure context.
        // Without one, loopback ports are not reachable from the host, and 22 is not HTTP.
        const host = forwarded ? "localhost" : address;
        const label = forwarded ? `localhost:${forwarded}` : host ? `${host}:${port}` : `${port}`;
        if (host && !(port === 22 && !forwarded) && (forwarded || !loopback)) {
          const link = element("a", "", label);
          link.href = `http://${label}`;
          link.target = "_blank";
          link.rel = "noopener";
          item.append(link);
        } else {
          item.append(element("span", "local", loopback ? `${port} (local)` : `${port}`));
        }
        const toggle = element("button", "btn quiet", forwarded ? "Stop" : "Forward");
        toggle.title = forwarded ? `Stop forwarding port ${port}` : `Forward port ${port} to localhost`;
        toggle.disabled = state.pending.has(`forward ${vm.name}:${port}`);
        toggle.addEventListener("click", () => (forwarded ? data.unforward : data.forward)(vm.name, port));
        item.append(toggle);
        return item;
      });
      facts.push(["IP", network.addresses.join(", ") || "none"], ["Ports", ports.length ? ports : "none"]);
    }
    $("machine-facts").replaceChildren(...facts.map(([term, value]) => {
      const row = element("div");
      const detail = element("dd", Array.isArray(value) ? "links" : "");
      detail.append(...[value].flat());
      row.append(element("dt", "", term), detail);
      return row;
    }));

    const power = $("machine-power");
    const busy = state.pending.has(`stop ${vm.name}`) || state.pending.has(`start ${vm.name}`);
    power.disabled = busy;
    if (!power.classList.contains("armed")) {
      power.textContent = busy ? (vm.running ? "Stopping…" : "Starting…") : vm.running ? (vm.hardware.persist ? "Stop" : "Stop and discard") : "Start";
    }
    const views = $("views");
    views.dataset.view = vm.running ? "running" : "stopped";
    views.dataset.expanded = state.screenExpanded;
    $("screen-toggle").textContent = state.screenExpanded ? "Collapse" : "Expand";
    const level = state.muted ? 0 : state.volume;
    $("mute").dataset.muted = level === 0;
    $("mute").ariaLabel = state.muted ? "Unmute" : "Mute";
    $("volume").value = level;
    $("volume").style.setProperty("--level", `${level * 100}%`);
    $("screen").tabIndex = state.screenExpanded ? 0 : -1;
    for (const button of document.querySelectorAll(".modifier-keys .key")) button.ariaPressed = state.modifiers.has(Number(button.dataset.code));
    if (!state.screenExpanded && document.activeElement === $("screen")) $("screen").blur();
  }
  if (session?.name !== (vm?.running ? vm.name : null)) {
    closeSession();
    if (vm?.running) openSession(vm.name);
  }
  session?.hint();
}

function keyButton({ label, name, code }) {
  const button = element("button", "key", label);
  button.dataset.code = code;
  if (name) button.dataset.name = name;
  return button;
}

// The key bars under the screen and the shell. Pressing a key keeps the focus
// where it is, so the screen or shell keeps the keyboard and the on-screen
// keyboard stays open.
$("shell-keys").querySelector(".keyboard").append($("keyboard").firstElementChild.cloneNode(true));
for (const bar of [$("keys"), $("shell-keys")]) {
  bar.querySelector(".modifier-keys").append(...data.MODIFIERS.map(keyButton));
  bar.querySelector(".named-keys").append(...data.KEYS.map(keyButton));
  bar.addEventListener("pointerdown", (event) => event.target !== $("typing") && event.preventDefault());
}

confirming(
  $("machine-power"),
  "Discard machine?",
  () => (selected().running ? data.stop(selected().name) : data.start(selected().name)),
  () => selected().running && !selected().hardware.persist,
);
confirming($("machine-delete"), "Delete machine?", () => data.remove(selected().name));
$("machine-start").onclick = () => data.start(selected().name);
// The display is a preview beside the shell until expanded into its place.
$("screen-toggle").onclick = () => data.setScreenExpanded(!data.state.screenExpanded);
$("shell-return").onclick = () => data.setScreenExpanded(false);
$("mute").onclick = () => data.setMuted(!data.state.muted);
$("volume").oninput = (event) => data.setVolume(Number(event.target.value));
document.addEventListener("pointerdown", data.resumeAudio);
document.addEventListener("keydown", data.resumeAudio);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && data.state.screenExpanded && !event.target.closest("canvas, .xterm, .typing, dialog")) {
    data.setScreenExpanded(false);
  }
});
// Returns to the overview, the screen shrinking back into its tile.
function closeMachine(show = () => data.select(null)) {
  const vm = selected();
  if (!vm?.running || !tiles.has(vm.name)) return show();
  morph($("screen"), show, () => tiles.get(vm.name).canvas);
}

$("home").onclick = () => closeMachine();

// Back and forward move between views the way clicks do.
window.addEventListener("popstate", () => {
  const target = data.route(location.pathname).selected;
  const current = data.state.selected;
  if (current && !target) closeMachine(data.followLocation);
  else if (!current && target) openMachine(target, data.followLocation);
  else data.followLocation();
});

// The selected machine's screen with input, and a shell sized to its view.

let session = null;

function caption(...parts) {
  $("caption").replaceChildren(...parts);
}

function openSession(name) {
  const canvas = $("screen");
  const frameBox = $("screen-frame");
  const events = new AbortController();
  const on = (target, type, listener) => target.addEventListener(type, listener, { signal: events.signal });
  const paint = paintInto(canvas);
  let frame = null;
  const idle = () => caption(data.state.screenExpanded ? "Click to control" : "Click to expand");

  caption("Connecting…");
  const screen = data.screenStream(name, 60, (next) => {
    if (!frame) idle();
    frame = next;
    $("views").style.setProperty("--aspect", frame.width / frame.height);
    paint(frame);
  }, true);
  screen.done.catch((error) => {
    frame = null;
    if (session?.screen === screen) caption(`Screen disconnected: ${error.message}`);
  });

  const input = data.inputStream(name);
  input.ready.then(() => {
    const held = new Set();
    const typing = $("typing");
    const pointer = (event) => {
      event.preventDefault();
      // The preview is too small to aim at, so a click there only expands it.
      if (!data.state.screenExpanded) {
        if (event.type === "pointerdown") data.setScreenExpanded(true);
        return;
      }
      // A touch keeps the on-screen keyboard open while it is in use.
      if (event.type === "pointerdown" && document.activeElement !== typing) canvas.focus();
      if (event.type === "pointerdown") canvas.setPointerCapture(event.pointerId);
      if (!frame) return;
      const box = canvas.getBoundingClientRect();
      const detail = event.type === "wheel" ? event.deltaY : event.button;
      data.pointerEvents(event.type, (event.clientX - box.left) / box.width, (event.clientY - box.top) / box.height, frame, detail)
        .forEach(input.send);
    };
    for (const type of ["pointermove", "pointerdown", "pointerup", "wheel"]) on(canvas, type, pointer);
    on(canvas, "contextmenu", (event) => event.preventDefault());
    const key = (event) => {
      event.preventDefault();
      const keyInput = data.keyEvent(event, held);
      if (keyInput) input.send(keyInput);
    };
    on(canvas, "keydown", key);
    on(canvas, "keyup", key);
    let flash = null;
    on(canvas, "focus", () => {
      frameBox.classList.add("captured", "flash");
      caption(element("span", "dot on"), "Keyboard and mouse go to the machine · click outside to release");
      clearTimeout(flash);
      flash = setTimeout(() => frameBox.classList.remove("flash"), 2000);
    });
    on(canvas, "blur", () => {
      frameBox.classList.remove("captured", "flash");
      idle();
      data.releaseKeys(held).forEach(input.send);
    });

    on($("keys"), "click", (event) => {
      const button = event.target.closest("[data-code]");
      if (!button) return;
      if (!button.dataset.name) return data.toggleModifier(Number(button.dataset.code));
      data.chord([Number(button.dataset.code)]).forEach(input.send);
    });

    // The on-screen keyboard types into a hidden field, whose changes become
    // key presses. It keeps one space, so Backspace has something to remove.
    const SPACE = " ";
    let typed = (typing.value = SPACE);
    const sync = (reset) => {
      try {
        data.typing(typed, typing.value).forEach(input.send);
      } catch (error) {
        caption(error.message);
      }
      typed = typing.value;
      if (reset) typed = typing.value = SPACE;
    };
    on(typing, "input", (event) => sync(!event.isComposing));
    on(typing, "compositionend", () => sync(true));
    on(typing, "keydown", (event) => {
      const code = data.namedKey(event.key);
      if (code === null) return;
      event.preventDefault();
      data.chord([code]).forEach(input.send);
    });
    on($("keyboard"), "click", () => (document.activeElement === typing ? typing.blur() : typing.focus()));
    on(typing, "focus", () => {
      $("keyboard").ariaPressed = true;
      frameBox.classList.add("captured");
      caption(element("span", "dot on"), "Typing goes to the machine");
    });
    on(typing, "blur", () => {
      $("keyboard").ariaPressed = false;
      frameBox.classList.remove("captured");
      idle();
    });
  }).catch((error) => caption(`Input unavailable: ${error.message}`));

  // Refreshes the hint when the display is expanded or collapsed.
  const hint = () => frame && ![canvas, $("typing")].includes(document.activeElement) && idle();
  session = { name, screen, input, events, shell: null, hint };
  openShell(name);
}

function terminalTheme() {
  const style = getComputedStyle(document.documentElement);
  const token = (name) => style.getPropertyValue(name).trim();
  const dark = matchMedia("(prefers-color-scheme: dark)").matches;
  return {
    background: token("--background"),
    foreground: token("--gray-1000"),
    cursor: token("--gray-1000"),
    cursorAccent: token("--background"),
    selectionBackground: dark ? "#333333" : "#d9d9d9",
    black: dark ? "#2e2e2e" : "#171717",
    brightBlack: token("--gray-700"),
    white: dark ? "#ededed" : "#8f8f8f",
    brightWhite: dark ? "#ffffff" : "#666666",
    red: token("--red"),
    brightRed: token("--red"),
    green: token("--green"),
    brightGreen: token("--green"),
    yellow: token("--amber"),
    brightYellow: token("--amber"),
    blue: dark ? "#52a8ff" : "#0068d6",
    brightBlue: dark ? "#52a8ff" : "#0068d6",
    magenta: dark ? "#c472fb" : "#7d00cc",
    brightMagenta: dark ? "#c472fb" : "#7d00cc",
    cyan: dark ? "#3bd4c9" : "#00838f",
    brightCyan: dark ? "#3bd4c9" : "#00838f",
  };
}

function openShell(name) {
  const host = $("shell");
  host.replaceChildren();
  $("shell-status").replaceChildren();
  const terminal = new Terminal({
    fontFamily: '"Geist Mono", ui-monospace, Menlo, monospace',
    fontSize: 13,
    cursorBlink: true,
    theme: terminalTheme(),
  });
  const fit = new FitAddon.FitAddon();
  terminal.loadAddon(fit);
  terminal.open(host);
  terminal.loadAddon(new CanvasAddon.CanvasAddon());
  fit.fit();
  const shell = data.shellSession(name, terminal.cols, terminal.rows, (bytes) => terminal.write(bytes));
  terminal.onData((text) => shell.write(data.terminalText(text)));
  const keys = new AbortController();
  $("shell-keys").addEventListener("click", (event) => {
    const button = event.target.closest(".key");
    if (!button) return;
    if (button.classList.contains("keyboard")) return terminal.textarea === document.activeElement ? terminal.blur() : terminal.focus();
    if (!button.dataset.name) return data.toggleModifier(Number(button.dataset.code));
    shell.write(data.terminalKey(button.dataset.name));
  }, { signal: keys.signal });
  terminal.onResize(({ cols, rows }) => shell.resize(cols, rows));
  // Captured before xterm, which only pastes text.
  terminal.element.addEventListener("paste", (event) => {
    const image = [...event.clipboardData.files].find((file) => file.type === "image/png");
    if (!image) return;
    event.preventDefault();
    event.stopPropagation();
    image.arrayBuffer().then((buffer) => shell.pasteImage(new Uint8Array(buffer))).catch((error) => terminal.write(`\r\n${error.message}\r\n`));
  }, true);
  const resizes = new ResizeObserver(() => host.clientWidth && host.clientHeight && fit.fit());
  resizes.observe(host);
  session.shell = { shell, terminal, resizes, keys };
  const ended = (text) => {
    if (session?.shell?.shell !== shell) return;
    const restart = element("button", "btn", "New shell");
    restart.onclick = () => {
      closeShell();
      openShell(name);
    };
    $("shell-status").replaceChildren(text, restart);
  };
  shell.exited.then((code) => ended(`Shell exited with ${code}.`), (error) => ended(`Shell disconnected: ${error.message}.`));
}

function closeShell() {
  session.shell.shell.close();
  session.shell.terminal.dispose();
  session.shell.resizes.disconnect();
  session.shell.keys.abort();
  session.shell = null;
}

function closeSession() {
  if (!session) return;
  session.screen.close();
  session.input.close();
  session.events.abort();
  closeShell();
  $("screen").blur();
  $("typing").blur();
  session = null;
}

// New machine

const sizes = element("div", "size-row");
sizes.append(...data.PRESETS.map((preset) => {
  const label = element("label", "size");
  const input = element("input");
  Object.assign(input, { type: "radio", name: "preset", value: preset.id, defaultChecked: preset.id === "standard" });
  const text = element("span");
  text.append(element("strong", "", preset.label), element("small", "", data.specs(preset).slice(0, 2).join(" · ")));
  label.append(input, text);
  return label;
}));
$("presets").append(sizes);

function renderSheet(state) {
  const sheet = $("sheet");
  if (state.creating && !sheet.open) {
    $("create").reset();
    sheet.showModal();
    $("create").elements.name.focus();
  }
  if (!state.creating && sheet.open) sheet.close();
}

$("sheet").addEventListener("close", () => {
  if ($("sheet").returnValue === "create") {
    const form = $("create").elements;
    data.create({
      preset: form.preset.value,
      name: form.name.value.trim(),
      persist: form.persist.checked,
      wait_network: form.wait_network.checked,
      base: form.base.value.trim(),
      disk_gib: form.disk_gib.value,
      size: form.size.value.trim(),
      env: form.env.value,
    });
  } else {
    data.setCreating(false);
  }
  $("sheet").returnValue = "";
});

for (const id of ["new", "blank-new"]) $(id).onclick = () => data.setCreating(true);

// Asks for the token until there is one, also after serve rejected it.
function renderLogin(state) {
  const login = $("login");
  if (!state.token && !login.open) {
    $("login-form").reset();
    login.showModal();
  }
  if (state.token && login.open) login.close();
}

$("login").addEventListener("cancel", (event) => event.preventDefault());
$("login").addEventListener("close", () => {
  const token = $("login-form").elements.token.value.trim();
  if (token) data.setToken(token);
  else renderLogin(data.state);
});

// Header and toasts

function renderHeader(state) {
  $("host").textContent = state.listError ? `Listing failed: ${state.listError}` : "Connected";
  $("host").title = state.listError ?? "";
  $("host").classList.toggle("down", Boolean(state.listError));
}

function renderToasts(state) {
  $("toasts").replaceChildren(...state.toasts.map((toast) => {
    const node = element("li", `toast ${toast.tone}`, toast.text);
    node.onclick = () => data.dismissToast(toast.id);
    return node;
  }));
}

// xterm measures its cell size once, so the font must be there before any terminal opens.
await document.fonts.load('13px "Geist Mono"');
for (const render of [renderLogin, renderHeader, renderOverview, renderMachine, renderSheet, renderToasts]) data.subscribe(render);
data.poll();
