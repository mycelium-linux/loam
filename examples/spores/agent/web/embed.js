(() => {
  if (window.__myceliumAgentSidebar) return;
  window.__myceliumAgentSidebar = true;

  const ASSET = 'https://agent.myc';
  const root = document.createElement('div');
  root.className = 'mycelium-agent-root';
  root.dataset.open = 'false';
  root.innerHTML = `
    <button class="mycelium-agent-rail" type="button" aria-label="Open agent"></button>
    <section class="mycelium-agent-panel" aria-label="Agent" hidden>
      <iframe title="Agent" src="${ASSET}/?embed=1"></iframe>
    </section>`;
  document.documentElement.append(root);

  const link = document.createElement('link');
  link.rel = 'stylesheet';
  link.href = `${ASSET}/embed.css`;
  document.head.append(link);

  const rail = root.querySelector('.mycelium-agent-rail');
  const panel = root.querySelector('.mycelium-agent-panel');
  const frame = panel.querySelector('iframe');
  let pinned = false;
  let closeTimer = 0;
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

  function open() {
    clearTimeout(closeTimer);
    if (root.dataset.open === 'true') return;
    panel.hidden = false;
    root.dataset.open = 'true';
    sendContext();
  }

  function close(force = false) {
    clearTimeout(closeTimer);
    if (pinned && !force) return;
    root.dataset.open = 'false';
    closeTimer = setTimeout(() => {
      if (root.dataset.open !== 'true') panel.hidden = true;
    }, 280);
  }

  rail.addEventListener('pointerenter', open);
  rail.addEventListener('click', open);
  rail.addEventListener('pointerleave', () => {
    window.setTimeout(() => {
      if (!panel.matches(':hover')) close();
    }, 120);
  });
  panel.addEventListener('pointerenter', () => clearTimeout(closeTimer));
  panel.addEventListener('pointerleave', () => close());
  document.addEventListener('pointerdown', (event) => {
    if (root.dataset.open !== 'true') return;
    if (panel.contains(event.target) || rail.contains(event.target)) return;
    close(true);
  }, true);

  frame.addEventListener('load', () => {
    loaded = true;
    sendContext();
  });

  document.addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === '.') {
      event.preventDefault();
      root.dataset.open === 'true' ? close(true) : open();
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
    }
    if (data.type === 'mycelium:agent-close') close(true);
    if (data.type === 'mycelium:agent-context-request') sendContext();
  });

  window.addEventListener('mycelium-agent-context', sendContext);
  setInterval(() => {
    if (root.dataset.open === 'true') sendContext();
  }, 1200);
})();
