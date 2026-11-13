// The machine page's windows, tiled as a binary tree. A leaf is a window id;
// a split holds two nodes side by side ("row") or stacked ("column") and the
// share of its space the first one takes. An empty page is null.

export const DISPLAY = "display";
export const isShell = (id) => id !== DISPLAY;

export const INITIAL = { split: "row", ratio: 0.5, first: "main", second: DISPLAY };

const isLeaf = (node) => typeof node === "string";

export function windows(node) {
  if (node === null) return [];
  return isLeaf(node) ? [node] : [...windows(node.first), ...windows(node.second)];
}

// `node` with each leaf passed through `leaf`, a leaf it maps to null taken
// out and its sibling taking the split's place.
function mapped(node, leaf) {
  if (isLeaf(node)) return leaf(node);
  const first = mapped(node.first, leaf);
  const second = mapped(node.second, leaf);
  if (first === null) return second;
  if (second === null) return first;
  return { ...node, first, second };
}

export const without = (node, id) => node && mapped(node, (leaf) => (leaf === id ? null : leaf));

// `id` beside `target` on the right, or the whole page when it is empty.
export const added = (node, id) => (node === null ? id : { split: "row", ratio: 0.6, first: node, second: id });

const SIDES = { left: ["row", true], right: ["row", false], top: ["column", true], bottom: ["column", false] };

// `id` moved into half of `target` on `side`, or for "center" swapped with it.
export function placed(node, id, target, side) {
  if (id === target) throw new Error(`window ${id} cannot be placed beside itself`);
  if (side === "center") return mapped(node, (leaf) => (leaf === id ? target : leaf === target ? id : leaf));
  const [split, before] = SIDES[side];
  const pair = { split, ratio: 0.5, first: before ? id : target, second: before ? target : id };
  return mapped(without(node, id), (leaf) => (leaf === target ? pair : leaf));
}

// `path` names the split by the "first" and "second" steps down to it.
export function resized(node, path, ratio) {
  if (path.length === 0) return { ...node, ratio: Math.min(0.9, Math.max(0.1, ratio)) };
  const [step, ...rest] = path;
  return { ...node, [step]: resized(node[step], rest, ratio) };
}

export const at = (node, path) => path.reduce((split, step) => split[step], node);

// The side of a window a point at fractions `x` and `y` of it drops onto: an
// edge near it, otherwise the center.
export function dropSide(x, y) {
  const [side, distance] = Object.entries({ left: x, right: 1 - x, top: y, bottom: 1 - y }).sort((a, b) => a[1] - b[1])[0];
  return distance < 0.3 ? side : "center";
}

// A shell id no window has; ids name the shell's tmux session.
export function newShell(ids) {
  const taken = new Set(ids);
  let number = 2;
  while (taken.has(`shell-${number}`)) number++;
  return `shell-${number}`;
}

// A floating window's box as fractions of the workspace. A new one steps down
// and right from the ones before it, so they cascade instead of stacking.
export function floatBox(count) {
  const offset = Math.min(count * 0.03, 0.18);
  return { x: 0.1 + offset, y: 0.1 + offset, width: 0.55, height: 0.62 };
}

const MIN_WIDTH = 0.25;
const MIN_HEIGHT = 0.2;
const clamp = (value, min, max) => Math.min(max, Math.max(min, value));

// `box` moved by `dx` and `dy`, kept where its bar stays reachable.
export const moved = (box, dx, dy) => ({
  ...box,
  x: clamp(box.x + dx, 0, 1 - box.width),
  y: clamp(box.y + dy, 0, 1 - box.height),
});

// `box` resized by `dx` and `dy`, no smaller than its controls need.
export const sized = (box, dx, dy) => ({
  ...box,
  width: clamp(box.width + dx, MIN_WIDTH, Math.max(MIN_WIDTH, 1 - box.x)),
  height: clamp(box.height + dy, MIN_HEIGHT, Math.max(MIN_HEIGHT, 1 - box.y)),
});
