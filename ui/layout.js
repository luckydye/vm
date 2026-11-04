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
export function newShell(node) {
  const taken = new Set(windows(node));
  let number = 2;
  while (taken.has(`shell-${number}`)) number++;
  return `shell-${number}`;
}
