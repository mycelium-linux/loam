(() => {
  if (window.__myceliumAgentSidebar) return;
  window.__myceliumAgentSidebar = true;

  const ASSET = 'https://agent.myc';
  const root = document.createElement('div');
  root.className = 'mycelium-agent-root';
  root.dataset.open = 'false';
  root.innerHTML = `
    <button class="mycelium-agent-rail" type="button" aria-label="Open agent"></button>
    <button class="mycelium-agent-teaser" type="button" aria-label="Open agent">
      <span class="mycelium-agent-teaser-mark" aria-hidden="true">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="m12 3 1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8z"/><path d="m18.8 16.4.6 1.8 1.8.6-1.8.6-.6 1.8-.6-1.8-1.8-.6 1.8-.6z"/></svg>
      </span>
      <span class="mycelium-agent-teaser-label">Agent</span>
    </button>
    <section class="mycelium-agent-panel" aria-label="Agent" hidden>
      <iframe title="Agent" src="${ASSET}/?embed=1"></iframe>
    </section>`;
  document.documentElement.append(root);

  const link = document.createElement('link');
  link.rel = 'stylesheet';
  link.href = `${ASSET}/embed.css`;
  document.head.append(link);

  const rail = root.querySelector('.mycelium-agent-rail');
  const teaser = root.querySelector('.mycelium-agent-teaser');
  const panel = root.querySelector('.mycelium-agent-panel');
  const frame = panel.querySelector('iframe');
  let pinned = false;
  let closeTimer = 0;
  let teaserTimer = 0;
  let loaded = false;

  const context = () => {
    const explicit = window.__MYCELIUM_AGENT_CONTEXT;
    const app = explicit?.app || document.querySelector('meta[name="mycelium-app"]')?.content || document.title.split('—')[0].trim() || location.hostname;
    const detail = explicit?.detail || location.pathname;
    return { app, detail, host: location.hostname, title: document.title };
  };

  function post(type, extra = {}) {
    if (!loaded) return;
    frame.contentWindow?.postMessage({ type, ...extra }, ASSET);
  }

  function sendContext() {
    post('mycelium:agent-context', { context: context() });
  }

  function showTeaser() {
    clearTimeout(teaserTimer);
    if (root.dataset.open === 'true') return;
    root.dataset.teaser = 'true';
  }

  function hideTeaser() {
    clearTimeout(teaserTimer);
    if (root.dataset.open === 'true') return;
    root.dataset.teaser = 'false';
  }

  function sendPinState() {
    post('mycelium:agent-pin-state', { pinned });
  }

  function open() {
    clearTimeout(closeTimer);
    clearTimeout(teaserTimer);
    if (root.dataset.open === 'true') return;
    root.dataset.teaser = 'false';
    panel.hidden = false;
    root.dataset.open = 'true';
    pinned = true;
    sendPinState();
    sendContext();
  }

  function toggle() {
    root.dataset.open === 'true' ? close(true) : open();
  }

  function close(force = false) {
    clearTimeout(closeTimer);
    if (pinned && !force) return;
    pinned = false;
    root.dataset.open = 'false';
    root.dataset.teaser = 'false';
    sendPinState();
    closeTimer = setTimeout(() => {
      if (root.dataset.open !== 'true') panel.hidden = true;
    }, 280);
  }

  rail.addEventListener('pointerenter', showTeaser);
  rail.addEventListener('pointerleave', () => {
    teaserTimer = window.setTimeout(() => {
      if (!teaser.matches(':hover') && !panel.matches(':hover')) hideTeaser();
    }, 140);
  });
  teaser.addEventListener('pointerenter', showTeaser);
  teaser.addEventListener('pointerleave', () => {
    teaserTimer = window.setTimeout(() => {
      if (!panel.matches(':hover')) hideTeaser();
    }, 140);
  });
  rail.addEventListener('click', toggle);
  teaser.addEventListener('click', toggle);
  panel.addEventListener('pointerenter', () => clearTimeout(closeTimer));
  panel.addEventListener('pointerleave', () => close());
  document.addEventListener('pointerdown', (event) => {
    if (root.dataset.open !== 'true') return;
    if (panel.contains(event.target) || rail.contains(event.target)) return;
    close(true);
  }, true);

  frame.addEventListener('load', () => {
    loaded = true;
    sendPinState();
    sendContext();
  });

  document.addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === '.') {
      event.preventDefault();
      toggle();
    }
    if (event.key === 'Escape' && root.dataset.open === 'true') close(true);
  });

  window.addEventListener('message', (event) => {
    if (event.origin !== ASSET) return;
    const data = event.data || {};
    if (data.type === 'mycelium:agent-ready') {
      loaded = true;
      sendContext();
    }
    if (data.type === 'mycelium:agent-pin') {
      pinned = Boolean(data.pinned);
      if (pinned) clearTimeout(closeTimer);
      sendPinState();
    }
    if (data.type === 'mycelium:agent-close') close(true);
    if (data.type === 'mycelium:agent-context-request') sendContext();
  });

  window.addEventListener('mycelium-agent-context', sendContext);
  setInterval(() => {
    if (root.dataset.open === 'true') sendContext();
  }, 1200);
})();
