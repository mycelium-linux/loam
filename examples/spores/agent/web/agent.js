(() => {
  const params = new URLSearchParams(location.search);
  const embedded = params.get('embed') === '1';
  const app = document.getElementById('context-app');
  const detail = document.getElementById('context-detail');
  const thread = document.getElementById('thread');
  const empty = document.getElementById('empty');
  const prompt = document.getElementById('prompt');
  const send = document.getElementById('send');
  const composer = document.getElementById('composer');
  const pin = document.getElementById('pin');
  const close = document.getElementById('close');
  const expand = document.getElementById('expand');
  let context = { app: 'This app', detail: '/' };
  let pinned = false;
  let fullscreen = false;

  const toParent = (type, extra = {}) => {
    if (embedded) parent.postMessage({ type, ...extra }, 'https://home.myc');
  };
  // The parent origin is the app host, not always home.myc. Use the
  // document.referrer origin when it is a .myc surface.
  const parentOrigin = (() => {
    try {
      const url = new URL(document.referrer);
      return url.hostname.endsWith('.myc') ? url.origin : null;
    } catch { return null; }
  })();
  const sendParent = (type, extra = {}) => {
    if (embedded) parent.postMessage({ type, ...extra }, parentOrigin || '*');
  };

  function setContext(next) {
    if (!next) return;
    context = { app: next.app || context.app, detail: next.detail || context.detail };
    app.textContent = context.app;
    detail.textContent = context.detail;
    const emptyTitle = document.getElementById('empty-title');
    if (emptyTitle) {
      emptyTitle.textContent = context.app === 'This app' ? 'Work inside the app.' : `You're in ${context.app}.`;
    }
  }

  window.addEventListener('message', (event) => {
    if (!embedded) return;
    const data = event.data || {};
    if (data.type === 'mycelium:agent-context') setContext(data.context);
    if (data.type === 'mycelium:agent-pin-state') {
      pinned = Boolean(data.pinned);
      pin.setAttribute('aria-pressed', String(pinned));
      document.getElementById('agent').dataset.pinned = String(pinned);
    }
    if (data.type === 'mycelium:agent-fullscreen-state') {
      fullscreen = Boolean(data.fullscreen);
      expand.setAttribute('aria-pressed', String(fullscreen));
      document.getElementById('agent').dataset.fullscreen = String(fullscreen);
    }
  });
  window.addEventListener('mycelium-agent-context', (event) => setContext(event.detail));

  function addMessage(role, text) {
    empty?.remove();
    const message = document.createElement('div');
    message.className = `message ${role}`;
    const who = document.createElement('span');
    who.className = 'who';
    who.textContent = role === 'user' ? 'You' : role === 'system' ? 'System' : 'Agent';
    const bubble = document.createElement('div');
    bubble.className = 'bubble';
    bubble.textContent = text;
    message.append(who, bubble);
    thread.append(message);
    thread.scrollTop = thread.scrollHeight;
  }

  function resizePrompt() {
    prompt.style.height = 'auto';
    prompt.style.height = `${Math.min(prompt.scrollHeight, 140)}px`;
    send.disabled = !prompt.value.trim();
  }

  function submit() {
    const text = prompt.value.trim();
    if (!text) return;
    addMessage('user', text);
    prompt.value = '';
    resizePrompt();
    window.setTimeout(() => {
      addMessage('system', 'The agent harness is not connected yet. This surface is ready for it: context is attached, the prompt is composed, and the app can receive an edit intent.');
    }, 320);
  }

  composer.addEventListener('submit', (event) => {
    event.preventDefault();
    submit();
  });
  prompt.addEventListener('input', resizePrompt);
  prompt.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      submit();
    }
  });
  for (const button of document.querySelectorAll('[data-prompt]')) {
    button.addEventListener('click', () => {
      prompt.value = button.dataset.prompt;
      resizePrompt();
      prompt.focus();
    });
  }
  pin.addEventListener('click', () => {
    pinned = !pinned;
    pin.setAttribute('aria-pressed', String(pinned));
    document.getElementById('agent').dataset.pinned = String(pinned);
    sendParent('mycelium:agent-pin', { pinned });
  });
  expand.addEventListener('click', () => sendParent('mycelium:agent-fullscreen-toggle'));
  close.addEventListener('click', () => sendParent('mycelium:agent-close'));
  document.getElementById('context-change').addEventListener('click', () => sendParent('mycelium:agent-context-request'));
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') sendParent('mycelium:agent-close');
  });
  setContext({ app: 'This app', detail: '/' });
  sendParent('mycelium:agent-ready');
  resizePrompt();
  if (!embedded) empty?.querySelector('p')?.append(' Open this from an app to attach its context.');
})();
