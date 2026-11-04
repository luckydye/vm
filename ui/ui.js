// Renders state from data.js and forwards user events to it. Live
// connections (tile previews, the machine's screen and shells) are opened and
// closed as the rendered state asks for them.

import * as data from "./data.js";
import * as layout from "./layout.js";

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
  $("machine-info").hidden = !vm;
  if (!vm) $("facts-popover").hidePopover();
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
    $("workspace").hidden = !vm.running;
    $("stopped").hidden = vm.running;
    const level = state.muted ? 0 : state.volume;
    $("mute").dataset.muted = level === 0;
    $("mute").ariaLabel = state.muted ? "Unmute" : "Mute";
    $("volume").value = level;
    $("volume").style.setProperty("--level", `${level * 100}%`);
    for (const button of document.querySelectorAll(".modifier-keys .key")) button.ariaPressed = state.modifiers.has(Number(button.dataset.code));
  }
  const open = layout.windows(state.layout);
  $("window-menu").hidden = !vm?.running;
  $("open-display").hidden = open.includes(layout.DISPLAY);
  if (session?.name !== (vm?.running ? vm.name : null)) {
    closeSession();
    if (vm?.running) openSession(vm.name);
  }
  if (!session) return;
  // A terminal opens once its window is in place, so it measures its size.
  const shells = open.filter(layout.isShell);
  for (const id of session.shells.keys()) if (!shells.includes(id)) closeShell(id);
  const opening = shells.filter((id) => !session.shells.has(id)).map(shellWindow);
  renderWorkspace(state);
  opening.forEach(startShell);
}

// The windows tiled as the layout says, or the zoomed one alone. Windows are
// moved, never rebuilt, so their connections keep running; a resize only sets
// the sizes of the splits rendered for the same structure.

const STATIC_WINDOWS = { [layout.DISPLAY]: $("display-window") };
let structure = null;
let splits = [];

const windowElements = () => [...Object.values(STATIC_WINDOWS), ...[...session.shells.values()].map((entry) => entry.window)];

function decorate(window, id) {
  window.dataset.window = id;
  window.querySelector(".window-bar").append($("window-buttons").content.cloneNode(true));
}
for (const [id, window] of Object.entries(STATIC_WINDOWS)) decorate(window, id);

function build(node, path) {
  if (typeof node === "string") return STATIC_WINDOWS[node] ?? session.shells.get(node).window;
  const split = element("div", `split ${node.split}`);
  split.append(build(node.first, [...path, "first"]), element("div", "divider"), build(node.second, [...path, "second"]));
  splits.push({ split, path });
  return split;
}

function renderWorkspace(state) {
  const tiling = state.zoomed ?? state.layout;
  const next = JSON.stringify(tiling, (key, value) => (key === "ratio" ? undefined : value));
  if (next !== structure) {
    structure = next;
    splits = [];
    // A window keeps no share from the split it was in before.
    for (const window of windowElements()) window.style.flexGrow = "";
    $("parked").append(...windowElements());
    $("workspace").replaceChildren(tiling === null ? element("p", "workspace-empty", "No windows open. Open one from the bar above.") : build(tiling, []));
  }
  for (const { split, path } of splits) {
    const { ratio } = layout.at(tiling, path);
    split.firstElementChild.style.flexGrow = ratio;
    split.lastElementChild.style.flexGrow = 1 - ratio;
  }
  for (const window of windowElements()) window.querySelector("[data-action=zoom]").ariaPressed = state.zoomed === window.dataset.window;
}

const toggleZoom = (id) => data.setZoomed(data.state.zoomed === id ? null : id);

$("workspace").addEventListener("click", (event) => {
  const button = event.target.closest("[data-action]");
  if (!button) return;
  const id = button.closest(".window").dataset.window;
  if (button.dataset.action === "close") data.closeWindow(id);
  else toggleZoom(id);
});
$("workspace").addEventListener("dblclick", (event) => {
  const bar = event.target.closest(".window-bar");
  if (bar && !event.target.closest("button, input")) toggleZoom(bar.closest(".window").dataset.window);
});
$("workspace").addEventListener("pointerdown", (event) => {
  if (event.target.classList.contains("divider")) return resizeSplit(event);
  const bar = event.target.closest(".window-bar");
  if (bar && !event.target.closest("button, input") && !data.state.zoomed) dragWindow(event, bar);
});

// Dragging a divider sets the share of its split under the pointer.
function resizeSplit(event) {
  const divider = event.target;
  const split = divider.parentElement;
  const { path } = splits.find((entry) => entry.split === split);
  const row = split.classList.contains("row");
  const move = (event) => {
    const box = split.getBoundingClientRect();
    data.resizeSplit(path, row ? (event.clientX - box.left) / box.width : (event.clientY - box.top) / box.height);
  };
  divider.setPointerCapture(event.pointerId);
  divider.addEventListener("pointermove", move);
  divider.addEventListener("lostpointercapture", () => divider.removeEventListener("pointermove", move), { once: true });
}

// Dragging a window by its bar onto another shows where it would go: beside
// it on the edge nearest the pointer, or in its place in the middle.
const DROP_AREAS = { left: [0, 0, 0.5, 1], right: [0.5, 0, 0.5, 1], top: [0, 0, 1, 0.5], bottom: [0, 0.5, 1, 0.5], center: [0, 0, 1, 1] };

function dragWindow(event, bar) {
  const window = bar.closest(".window");
  let drop = null;
  const move = (event) => {
    const target = document.elementFromPoint(event.clientX, event.clientY)?.closest("#workspace .window");
    window.classList.add("dragging");
    $("drop").hidden = !target || target === window;
    if ($("drop").hidden) return (drop = null);
    const box = target.getBoundingClientRect();
    drop = { target: target.dataset.window, side: layout.dropSide((event.clientX - box.left) / box.width, (event.clientY - box.top) / box.height) };
    const [left, top, width, height] = DROP_AREAS[drop.side];
    Object.assign($("drop").style, {
      left: `${box.left + left * box.width}px`,
      top: `${box.top + top * box.height}px`,
      width: `${width * box.width}px`,
      height: `${height * box.height}px`,
    });
  };
  const up = () => drop && data.moveWindow(window.dataset.window, drop.target, drop.side);
  bar.setPointerCapture(event.pointerId);
  bar.addEventListener("pointermove", move);
  bar.addEventListener("pointerup", up);
  bar.addEventListener("lostpointercapture", () => {
    bar.removeEventListener("pointermove", move);
    bar.removeEventListener("pointerup", up);
    window.classList.remove("dragging");
    $("drop").hidden = true;
  }, { once: true });
}

$("open-display").onclick = () => data.openWindow(layout.DISPLAY);
$("open-shell").onclick = data.openShell;
$("facts-popover").addEventListener("toggle", (event) => ($("machine-info").ariaExpanded = event.newState === "open"));

function keyButton({ label, name, code }) {
  const button = element("button", "key", label);
  button.dataset.code = code;
  if (name) button.dataset.name = name;
  return button;
}

// The key bars under the screen and the shells. Pressing a key keeps the
// focus where it is, so the screen or shell keeps the keyboard and the
// on-screen keyboard stays open.
function keyBar(bar) {
  bar.querySelector(".modifier-keys").append(...data.MODIFIERS.map(keyButton));
  bar.querySelector(".named-keys").append(...data.KEYS.map(keyButton));
  bar.addEventListener("pointerdown", (event) => event.target !== $("typing") && event.preventDefault());
}
keyBar($("keys"));

confirming(
  $("machine-power"),
  "Discard machine?",
  () => (selected().running ? data.stop(selected().name) : data.start(selected().name)),
  () => selected().running && !selected().hardware.persist,
);
confirming($("machine-delete"), "Delete machine?", () => data.remove(selected().name));
$("machine-start").onclick = () => data.start(selected().name);
$("mute").onclick = () => data.setMuted(!data.state.muted);
$("volume").oninput = (event) => data.setVolume(Number(event.target.value));
document.addEventListener("pointerdown", data.resumeAudio);
document.addEventListener("keydown", data.resumeAudio);
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

// The selected machine's screen with input, and its shells sized to their windows.

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
  const idle = () => caption("Click to control");

  caption("Connecting…");
  const screen = data.screenStream(name, 60, (next) => {
    if (!frame) idle();
    frame = next;
    frameBox.style.setProperty("--aspect", frame.width / frame.height);
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

    // Types the clipboard's text, since the guest only has its own clipboard.
    on($("paste"), "click", () => {
      data.readClipboardText().then((text) => data.pasting(text).forEach(input.send)).catch((error) => caption(error.message));
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

  session = { name, screen, input, events, shells: new Map() };
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

function shellWindow(id) {
  const window = $("shell-window").content.firstElementChild.cloneNode(true);
  decorate(window, id);
  window.querySelector(".window-title").textContent = `Terminal ${id}`;
  keyBar(window.querySelector(".shell-keys"));
  const entry = { id, window, connection: null };
  session.shells.set(id, entry);
  return entry;
}

function startShell(entry) {
  const host = entry.window.querySelector(".shell");
  const status = entry.window.querySelector(".shell-status");
  host.replaceChildren();
  status.replaceChildren();
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
  const shell = data.shellSession(session.name, entry.id, terminal.cols, terminal.rows, (bytes) => terminal.write(bytes));
  terminal.onData((text) => shell.write(data.terminalText(text)));
  const keys = new AbortController();
  entry.window.querySelector(".shell-keys").addEventListener("click", (event) => {
    const button = event.target.closest(".key");
    if (!button) return;
    if (button.classList.contains("keyboard")) return terminal.textarea === document.activeElement ? terminal.blur() : terminal.focus();
    if (button.classList.contains("paste")) {
      return data.readClipboard()
        .then(({ png, text }) => (png ? shell.pasteImage(png) : terminal.paste(text)))
        .catch((error) => terminal.write(`\r\n${error.message}\r\n`));
    }
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
  entry.connection = { shell, terminal, resizes, keys };
  // A shell that exits closes its window; one that lost its connection offers a new one.
  const current = () => entry.connection?.shell === shell;
  shell.exited.then(() => current() && data.removeWindow(entry.id), (error) => {
    if (!current()) return;
    const restart = element("button", "btn", "New shell");
    restart.onclick = () => {
      stopShell(entry);
      startShell(entry);
    };
    status.replaceChildren(`Shell disconnected: ${error.message}.`, restart);
  });
}

function stopShell(entry) {
  const { shell, terminal, resizes, keys } = entry.connection;
  shell.close();
  terminal.dispose();
  resizes.disconnect();
  keys.abort();
  entry.connection = null;
}

function closeShell(id) {
  const entry = session.shells.get(id);
  stopShell(entry);
  entry.window.remove();
  session.shells.delete(id);
}

function closeSession() {
  if (!session) return;
  session.screen.close();
  session.input.close();
  session.events.abort();
  for (const id of [...session.shells.keys()]) closeShell(id);
  $("screen").blur();
  $("typing").blur();
  session = null;
  structure = null;
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
      stores: form.stores.value,
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
