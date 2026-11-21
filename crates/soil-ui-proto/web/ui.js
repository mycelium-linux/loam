// SPDX-License-Identifier: MIT OR Apache-2.0
// The entire host-side UI implementation for the prototype.
//
// Lisp emits a tree of {tag, attrs, children} and {text} records plus a CSS
// string. This file mounts the tree, patches it in place, injects the CSS, and
// sends named actions back to the server. It has no knowledge of the counter.

const root = document.getElementById("root");
const source = document.getElementById("source");
const status = document.getElementById("status");
const apply = document.getElementById("apply");

let mounted = null;
let currentState = null;
let styleElement = null;

function setStatus(text, isError = false) {
  status.textContent = text;
  status.classList.toggle("error", isError);
}

function mount(vnode) {
  if (vnode == null) return null;
  if (typeof vnode === "string") return document.createTextNode(vnode);
  if (Object.prototype.hasOwnProperty.call(vnode, "text")) {
    return document.createTextNode(vnode.text ?? "");
  }
  const element = document.createElement(vnode.tag || "div");
  element.__vnode = vnode;
  applyAttrs(element, vnode.attrs || {});
  for (const child of vnode.children || []) {
    const node = mount(child);
    if (node) element.appendChild(node);
  }
  return element;
}

function patch(dom, vnode) {
  if (!dom) return mount(vnode);
  if (vnode == null) {
    dom.remove();
    return null;
  }
  if (Object.prototype.hasOwnProperty.call(vnode, "text") || typeof vnode === "string") {
    const text = typeof vnode === "string" ? vnode : (vnode.text ?? "");
    if (dom.nodeType === Node.TEXT_NODE) {
      if (dom.data !== text) dom.data = text;
      return dom;
    }
    const next = document.createTextNode(text);
    dom.replaceWith(next);
    return next;
  }
  if (dom.nodeType !== Node.ELEMENT_NODE || !dom.__vnode || dom.__vnode.tag !== vnode.tag) {
    const next = mount(vnode);
    dom.replaceWith(next);
    return next;
  }
  applyAttrs(dom, vnode.attrs || {}, dom.__vnode.attrs || {});
  reconcileChildren(dom, vnode.children || []);
  dom.__vnode = vnode;
  return dom;
}

function applyAttrs(element, next, previous = {}) {
  for (const name of Object.keys(previous)) {
    if (!(name in next)) element.removeAttribute(name);
  }
  for (const [name, value] of Object.entries(next)) {
    if (value === false || value == null) {
      element.removeAttribute(name);
      continue;
    }
    const text = String(value);
    if (element.getAttribute(name) !== text) element.setAttribute(name, text);
  }
}

function reconcileChildren(parent, children) {
  const previous = Array.from(parent.childNodes);
  const byKey = new Map();
  previous.forEach((node, index) => {
    const key = node.nodeType === Node.ELEMENT_NODE && node.getAttribute("data-key");
    byKey.set(key || `i:${index}`, node);
  });

  const keep = new Set();
  children.forEach((vnode, index) => {
    const key = vnode && vnode.attrs && vnode.attrs["data-key"];
    const existing = byKey.get(key || `i:${index}`);
    const next = existing ? patch(existing, vnode) : mount(vnode);
    if (!next) return;
    keep.add(next);
    const target = parent.childNodes[index] || null;
    if (target !== next) parent.insertBefore(next, target);
  });

  for (const node of previous) {
    if (!keep.has(node) && node.parentNode === parent) node.remove();
  }
}

function renderTree(tree) {
  if (!mounted) {
    mounted = mount(tree);
    root.replaceChildren(mounted);
    return;
  }
  mounted = patch(mounted, tree);
  if (!mounted) root.replaceChildren();
  else if (mounted.parentNode !== root) root.replaceChildren(mounted);
}

function applyCss(css) {
  if (!styleElement) {
    styleElement = document.createElement("style");
    styleElement.id = "soil-css";
    document.head.appendChild(styleElement);
  }
  if (styleElement.textContent !== css) styleElement.textContent = css;
}

async function render(action = "") {
  const params = new URLSearchParams();
  if (currentState !== null) params.set("state", String(currentState));
  if (action) params.set("action", action);

  const response = await fetch(`/view?${params.toString()}`, { cache: "no-store" });
  const data = await response.json();
  if (!response.ok) {
    setStatus(data.error || "render failed", true);
    return;
  }
  currentState = data.state;
  applyCss(data.css);
  renderTree(data.tree);
  setStatus(`state ${currentState}`);
}

async function loadSource() {
  const response = await fetch("/source", { cache: "no-store" });
  if (!response.ok) {
    setStatus("could not load source", true);
    return;
  }
  source.value = await response.text();
}

async function applySource() {
  setStatus("applying…");
  const response = await fetch("/source", {
    method: "POST",
    body: source.value,
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
  if (!response.ok) {
    const data = await response.json().catch(() => null);
    setStatus((data && data.error) || "apply failed", true);
    return;
  }
  await render();
}

root.addEventListener("click", (event) => {
  const target = event.target instanceof Element ? event.target.closest("[data-action]") : null;
  if (!target) return;
  event.preventDefault();
  render(target.dataset.action).catch((error) => setStatus(String(error), true));
});

apply.addEventListener("click", () => {
  applySource().catch((error) => setStatus(String(error), true));
});

source.addEventListener("keydown", (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
    event.preventDefault();
    applySource().catch((error) => setStatus(String(error), true));
  }
});

(async () => {
  try {
    await loadSource();
    await render();
  } catch (error) {
    setStatus(String(error), true);
  }
})();
