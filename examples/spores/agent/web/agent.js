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
  const modelSwitch = document.getElementById('model-switch');
  const modelMenu = document.getElementById('model-menu');
  const modelName = document.getElementById('model-name');
  const modelLogo = document.getElementById('model-logo');
  const modelProviders = {
    fpl: { label: 'FPL', mark: 'F' },
    openai: { label: 'OpenAI', icon: 'https://cdn.jsdelivr.net/npm/@lobehub/icons-static-svg@latest/icons/openai.svg', light: true },
    anthropic: { label: 'Anthropic', icon: 'https://cdn.simpleicons.org/anthropic/e5e7eb' },
    openrouter: { label: 'OpenRouter', icon: 'https://cdn.simpleicons.org/openrouter/e5e7eb' },
  };
  let model = localStorage.getItem('mycelium.agent.model') || 'fpl';
  let sessionId = localStorage.getItem('mycelium.agent.session') || '';
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

  function setModel(id, announce = false) {
    const provider = modelProviders[id] || modelProviders.fpl;
    model = modelProviders[id] ? id : 'fpl';
    try { localStorage.setItem('mycelium.agent.model', model); } catch {}
    if (modelName) modelName.textContent = provider.label;
    if (modelLogo) {
      modelLogo.replaceChildren();
      modelLogo.className = `model-logo${model === 'fpl' ? ' model-logo--fpl' : ''}`;
      if (provider.icon) {
        const image = document.createElement('img');
        image.alt = '';
        image.src = provider.icon;
        if (provider.light) image.className = 'is-light';
        image.addEventListener('error', () => {
          image.remove();
          modelLogo.textContent = provider.label.slice(0, 1);
        });
        modelLogo.append(image);
      } else {
        modelLogo.textContent = provider.mark || provider.label.slice(0, 1);
      }
    }
    modelMenu?.querySelectorAll('[data-model]').forEach((button) => {
      const selected = button.dataset.model === model;
      button.classList.toggle('is-selected', selected);
      button.setAttribute('aria-checked', String(selected));
    });
    if (announce) {
      addMessage('system', `Model provider preference set to ${provider.label}.`);
    }
  }

  function openModelMenu() {
    if (!modelMenu || !modelSwitch) return;
    modelMenu.hidden = false;
    modelSwitch.setAttribute('aria-expanded', 'true');
    modelMenu.querySelector(`[data-model="${model}"]`)?.focus();
  }
  function closeModelMenu() {
    if (!modelMenu || modelMenu.hidden) return;
    modelMenu.hidden = true;
    modelSwitch?.setAttribute('aria-expanded', 'false');
  }

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
    if (data.type === 'mycelium:agent-seed') runSeed(data);
  });
  window.addEventListener('mycelium-agent-context', (event) => setContext(event.detail));


  function addThinking() {
    empty?.remove();
    const message = document.createElement('div');
    message.className = 'message agent thinking';
    const who = document.createElement('span');
    who.className = 'who';
    who.textContent = 'Agent';
    const bubble = document.createElement('div');
    bubble.className = 'bubble';
    const dots = document.createElement('span');
    dots.className = 'thinking-dots';
    dots.innerHTML = '<i></i><i></i><i></i>';
    bubble.append(dots);
    message.append(who, bubble);
    thread.append(message);
    thread.scrollTop = thread.scrollHeight;
    return message;
  }

  function addToolList(tools) {
    if (!tools?.length) return null;
    empty?.remove();
    const list = document.createElement('div');
    list.className = 'agent-tools';
    const heading = document.createElement('p');
    heading.className = 'agent-tools-head';
    heading.textContent = 'Getting tools ready';
    list.append(heading);
    tools.forEach((tool, index) => {
      const row = document.createElement('div');
      row.className = 'agent-tool';
      row.dataset.state = tool.state || 'preparing';
      const dot = document.createElement('span');
      dot.className = 'agent-tool-dot';
      const copy = document.createElement('span');
      copy.className = 'agent-tool-copy';
      const strong = document.createElement('strong');
      strong.textContent = tool.label;
      const small = document.createElement('small');
      small.textContent = tool.note || '';
      copy.append(strong, small);
      const state = document.createElement('span');
      state.className = 'agent-tool-state';
      state.textContent = tool.state === 'waiting' ? 'Waiting' : 'Preparing';
      row.append(dot, copy, state);
      list.append(row);
      window.setTimeout(() => {
        if (row.dataset.state === 'waiting') return;
        row.dataset.state = 'ready';
        state.textContent = 'Ready';
      }, 360 + index * 280);
    });
    thread.append(list);
    thread.scrollTop = thread.scrollHeight;
    return list;
  }

  function runSeed(payload = {}) {
    if (payload.context) setContext(payload.context);
    if (payload.prompt) {
      addMessage('user', payload.prompt);
      void ask(payload.prompt);
      return;
    }
    const thinking = addThinking();
    const tools = addToolList(payload.tools || []);
    window.setTimeout(() => {
      thinking?.remove();
      if (tools) {
        tools.dataset.done = 'true';
        tools.querySelector('.agent-tools-head').textContent = 'Tools ready';
      }
      if (payload.ready) addMessage('agent', payload.ready);
      addMessage('system', payload.note || 'Ready.');
      thread.scrollTop = thread.scrollHeight;
    }, 900);
  }

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

  async function ask(text) {
    const thinking = addThinking();
    try {
      const response = await fetch('/api/agent', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ text, sessionId }),
      });
      const data = await response.json();
      thinking?.remove();
      if (!response.ok || data.error) {
        addMessage('system', data.error || `Agent error (${response.status}).`);
        return;
      }
      if (data.sessionId) {
        sessionId = data.sessionId;
        try { localStorage.setItem('mycelium.agent.session', sessionId); } catch {}
      }
      addMessage('agent', data.reply || 'The turn finished without text.');
    } catch (error) {
      thinking?.remove();
      addMessage('system', `Agent unavailable: ${error.message}`);
    }
  }

  function submit() {
    const text = prompt.value.trim();
    if (!text) return;
    addMessage('user', text);
    prompt.value = '';
    resizePrompt();
    void ask(text);
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
  modelSwitch?.addEventListener('click', (event) => {
    event.stopPropagation();
    modelMenu?.hidden ? openModelMenu() : closeModelMenu();
  });
  modelMenu?.addEventListener('click', (event) => {
    const choice = event.target.closest('[data-model]');
    if (!choice) return;
    setModel(choice.dataset.model, true);
    closeModelMenu();
    modelSwitch?.focus();
  });
  document.addEventListener('pointerdown', (event) => {
    if (!modelMenu || modelMenu.hidden) return;
    if (modelMenu.contains(event.target) || modelSwitch?.contains(event.target)) return;
    closeModelMenu();
  }, true);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') closeModelMenu();
  });
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
  setModel(model);
  setContext({ app: 'This app', detail: '/' });
  sendParent('mycelium:agent-ready');
  resizePrompt();
  if (!embedded) empty?.querySelector('p')?.append(' Open this from an app to attach its context.');
})();
